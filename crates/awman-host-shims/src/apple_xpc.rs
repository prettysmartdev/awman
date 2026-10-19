//! The native libxpc transport for the Apple Containers image-store bridge.
//!
//! Owned by the host-shims crate. The binary adapts its safe transport trait
//! to the image-store engine; route and release policy stay with that engine.
//!
//! Boundary (the whole unsafe surface, reviewed for WI 0122):
//!
//! - public libxpc C functions from libSystem (`xpc_connection_*`,
//!   `xpc_dictionary_*`, `xpc_get_type`, `xpc_retain`/`xpc_release`) and the
//!   exported type/error singletons they are compared against;
//! - one process-lifetime global block literal (no captures) used as the
//!   connection's required event handler, whose isa is libSystem's
//!   `_NSConcreteGlobalBlock`.
//!
//! No executable, library or firmware is extracted, loaded or spawned, and
//! no file is touched here.
//!
//! Waiting: the synchronous send runs on its own thread holding its own
//! connection reference. The calling thread polls `stop` and the deadline;
//! either one cancels the connection, which completes the send with
//! `XPC_ERROR_CONNECTION_INVALID`. If the send still has not returned
//! shortly afterwards the thread is left to finish on its own reference.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::xpc::{WaitControl, XpcFailure, XpcKind, XpcReply, XpcTransport, XpcValue};

type XpcObject = *mut c_void;

/// Opaque storage of an exported libxpc singleton; only its address is used.
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[link(name = "System", kind = "dylib")]
extern "C" {
    static _xpc_type_dictionary: Opaque;
    static _xpc_type_error: Opaque;
    static _xpc_error_connection_invalid: Opaque;
    static _xpc_error_connection_interrupted: Opaque;
    static _NSConcreteGlobalBlock: Opaque;

    fn xpc_connection_create_mach_service(
        name: *const c_char,
        targetq: *mut c_void,
        flags: u64,
    ) -> XpcObject;
    fn xpc_connection_set_event_handler(connection: XpcObject, handler: *const c_void);
    fn xpc_connection_resume(connection: XpcObject);
    fn xpc_connection_cancel(connection: XpcObject);
    fn xpc_connection_send_message_with_reply_sync(
        connection: XpcObject,
        message: XpcObject,
    ) -> XpcObject;
    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const XpcObject,
        count: usize,
    ) -> XpcObject;
    fn xpc_dictionary_set_string(dict: XpcObject, key: *const c_char, value: *const c_char);
    fn xpc_dictionary_set_data(
        dict: XpcObject,
        key: *const c_char,
        bytes: *const c_void,
        len: usize,
    );
    fn xpc_dictionary_get_string(dict: XpcObject, key: *const c_char) -> *const c_char;
    fn xpc_dictionary_get_data(
        dict: XpcObject,
        key: *const c_char,
        len: *mut usize,
    ) -> *const c_void;
    fn xpc_get_type(object: XpcObject) -> *const c_void;
    fn xpc_retain(object: XpcObject) -> XpcObject;
    fn xpc_release(object: XpcObject);
}

// ── Event handler block ────────────────────────────────────────────────────

/// Clang's block ABI: a global block needs no copy/dispose helpers.
#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
}

#[repr(C)]
struct BlockLiteral {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*const BlockLiteral, XpcObject),
    descriptor: *const BlockDescriptor,
}

const BLOCK_IS_GLOBAL: i32 = 1 << 28;

/// Connection events (errors, peer messages) need no action: every request
/// reads its outcome from its own reply.
// SAFETY: the callback uses the block ABI and never dereferences either opaque argument.
unsafe extern "C" fn ignore_event(_block: *const BlockLiteral, _event: XpcObject) {}

static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<BlockLiteral>(),
};

struct SharedBlock(BlockLiteral);
// SAFETY: the block is immutable after construction and has no captures;
// the blocks runtime treats global blocks as shared constants.
unsafe impl Send for SharedBlock {}
// SAFETY: the immutable global block has no captures or mutable state.
unsafe impl Sync for SharedBlock {}

fn event_handler() -> *const c_void {
    static BLOCK: OnceLock<SharedBlock> = OnceLock::new();
    let block = BLOCK.get_or_init(|| {
        SharedBlock(BlockLiteral {
            // Taking the address of an exported symbol reads nothing.
            isa: std::ptr::addr_of!(_NSConcreteGlobalBlock).cast(),
            flags: BLOCK_IS_GLOBAL,
            reserved: 0,
            invoke: ignore_event,
            descriptor: &DESCRIPTOR,
        })
    });
    (&block.0 as *const BlockLiteral).cast()
}

// ── Owned handles ──────────────────────────────────────────────────────────

/// One owned libxpc reference, released on drop.
struct Owned(XpcObject);
// SAFETY: libxpc objects are reference counted and thread-safe; each
// `Owned` holds its own reference.
unsafe impl Send for Owned {}

impl Owned {
    fn new(object: XpcObject) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }
    fn retained(&self) -> Self {
        // SAFETY: self.0 is a live object; retain returns the same object.
        Self(unsafe { xpc_retain(self.0) })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: we own exactly this reference.
        unsafe { xpc_release(self.0) }
    }
}

fn is(object: XpcObject, singleton: *const Opaque) -> bool {
    std::ptr::eq(object as *const Opaque, singleton)
}

/// Build the request dictionary. Keys or strings with NUL are refused.
fn request_dictionary(fields: &[(String, XpcValue)]) -> Option<Owned> {
    // SAFETY: an empty dictionary from null arrays and a zero count.
    let dict = Owned::new(unsafe { xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0) })?;
    for (key, value) in fields {
        let key = CString::new(key.as_str()).ok()?;
        match value {
            XpcValue::String(s) => {
                let s = CString::new(s.as_str()).ok()?;
                // SAFETY: both C strings outlive the call; libxpc copies them.
                unsafe { xpc_dictionary_set_string(dict.0, key.as_ptr(), s.as_ptr()) }
            }
            XpcValue::Data(d) => {
                // SAFETY: `d` is valid for `d.len()` bytes; libxpc copies them.
                unsafe { xpc_dictionary_set_data(dict.0, key.as_ptr(), d.as_ptr().cast(), d.len()) }
            }
        }
    }
    Some(dict)
}

/// Copy the wanted keys out of a reply into owned values.
fn read_reply(reply: &Owned, keys: &[(String, XpcKind)]) -> Result<XpcReply, XpcFailure> {
    // SAFETY: reply.0 is a live object. The singletons below are only
    // compared by address.
    let ty = unsafe { xpc_get_type(reply.0) };
    if std::ptr::eq(ty.cast(), std::ptr::addr_of!(_xpc_type_error)) {
        return Err(
            if is(reply.0, std::ptr::addr_of!(_xpc_error_connection_invalid)) {
                XpcFailure::Unavailable
            } else if is(
                reply.0,
                std::ptr::addr_of!(_xpc_error_connection_interrupted),
            ) {
                XpcFailure::Interrupted
            } else {
                XpcFailure::UnexpectedReply
            },
        );
    }
    if !std::ptr::eq(ty.cast(), std::ptr::addr_of!(_xpc_type_dictionary)) {
        return Err(XpcFailure::UnexpectedReply);
    }
    let mut out = XpcReply::new();
    for (key, kind) in keys {
        let Ok(ckey) = CString::new(key.as_str()) else {
            continue;
        };
        match kind {
            XpcKind::String => {
                // SAFETY: the returned pointer is owned by the dictionary,
                // valid until it is released; copied immediately.
                let p = unsafe { xpc_dictionary_get_string(reply.0, ckey.as_ptr()) };
                if !p.is_null() {
                    // SAFETY: libxpc returns a NUL-terminated string valid while reply is alive.
                    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
                    out.insert(key.clone(), XpcValue::String(s));
                }
            }
            XpcKind::Data => {
                let mut len = 0usize;
                // SAFETY: as above; `len` receives the byte count.
                let p = unsafe { xpc_dictionary_get_data(reply.0, ckey.as_ptr(), &mut len) };
                if !p.is_null() {
                    // SAFETY: libxpc supplies len initialized bytes owned by the live reply.
                    let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
                    out.insert(key.clone(), XpcValue::Data(bytes.to_vec()));
                }
            }
        }
    }
    Ok(out)
}

/// After cancelling, how long to wait for the send thread to return.
const CANCEL_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// The libxpc-backed transport.
pub struct NativeXpc;

impl XpcTransport for NativeXpc {
    fn send(
        &self,
        service: &str,
        request: &[(&str, XpcValue)],
        reply_keys: &[(&str, XpcKind)],
        wait: &WaitControl<'_>,
    ) -> Result<XpcReply, XpcFailure> {
        let name = CString::new(service).map_err(|_| XpcFailure::Unavailable)?;
        // SAFETY: a new connection to a named mach service; null queue
        // selects libxpc's default target queue.
        let conn = Owned::new(unsafe {
            xpc_connection_create_mach_service(name.as_ptr(), std::ptr::null_mut(), 0)
        })
        .ok_or(XpcFailure::Unavailable)?;
        // SAFETY: the handler is a process-lifetime global block, set before
        // the connection is resumed as libxpc requires.
        unsafe {
            xpc_connection_set_event_handler(conn.0, event_handler());
            xpc_connection_resume(conn.0);
        }

        let fields: Vec<(String, XpcValue)> = request
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let keys: Vec<(String, XpcKind)> = reply_keys
            .iter()
            .map(|(k, t)| (k.to_string(), *t))
            .collect();
        let worker_conn = conn.retained();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("awman-apple-xpc".into())
            .spawn(move || {
                let result = match request_dictionary(&fields) {
                    None => Err(XpcFailure::UnexpectedReply),
                    Some(message) => {
                        // SAFETY: both objects are live references owned here.
                        let reply = unsafe {
                            xpc_connection_send_message_with_reply_sync(worker_conn.0, message.0)
                        };
                        match Owned::new(reply) {
                            Some(reply) => read_reply(&reply, &keys),
                            None => Err(XpcFailure::UnexpectedReply),
                        }
                    }
                };
                drop(worker_conn);
                let _ = tx.send(result);
            })
            .map_err(|_| XpcFailure::Interrupted)?;

        let abandon = |why: XpcFailure| {
            // SAFETY: cancelling a live connection is thread-safe and makes
            // the outstanding synchronous send return.
            unsafe { xpc_connection_cancel(conn.0) };
            let _ = rx.recv_timeout(CANCEL_GRACE);
            Err(why)
        };
        loop {
            match rx.recv_timeout(POLL) {
                Ok(result) => {
                    // SAFETY: as above; the request is complete.
                    unsafe { xpc_connection_cancel(conn.0) };
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(XpcFailure::Interrupted),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if (wait.stop)() {
                return abandon(XpcFailure::Stopped);
            }
            if Instant::now() >= wait.deadline {
                return abandon(XpcFailure::TimedOut);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(secs: u64) -> (Instant, fn() -> bool) {
        (Instant::now() + Duration::from_secs(secs), || false)
    }

    #[test]
    fn the_event_handler_is_one_global_block() {
        let a = event_handler();
        assert_eq!(a, event_handler());
        // SAFETY: event_handler returns the process-lifetime BlockLiteral above.
        let block = unsafe { &*(a as *const BlockLiteral) };
        assert_eq!(block.flags, BLOCK_IS_GLOBAL);
    }

    #[test]
    fn an_unregistered_service_is_unavailable() {
        let (deadline, stop) = wait(10);
        let err = NativeXpc
            .send(
                "com.example.awman.no-such-service",
                &[(
                    "com.apple.container.xpc.route",
                    XpcValue::String("ping".into()),
                )],
                &[],
                &WaitControl {
                    deadline,
                    stop: &stop,
                },
            )
            .unwrap_err();
        assert_eq!(err, XpcFailure::Unavailable);
    }

    #[test]
    fn nul_bytes_never_reach_libxpc() {
        let (deadline, stop) = wait(10);
        assert!(NativeXpc
            .send(
                "com.example.awman.no-such-service",
                &[("k\0", XpcValue::String("v".into()))],
                &[],
                &WaitControl {
                    deadline,
                    stop: &stop
                },
            )
            .is_err());
    }
}
