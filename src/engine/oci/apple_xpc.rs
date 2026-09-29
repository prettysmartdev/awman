//! The native libxpc transport for the Apple Containers image-store bridge.
//!
//! Compiled into the `awman` binary only (`main.rs` includes this file by
//! path on macOS), never into the library, which stays
//! `#![forbid(unsafe_code)]`. It implements the library's safe
//! [`XpcTransport`] trait and nothing else: every route, key, version check,
//! selection rule and output check lives in `engine::oci::apple_store`.
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

use awman::engine::oci::apple_store::{
    WaitControl, XpcFailure, XpcKind, XpcReply, XpcTransport, XpcValue,
};

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
unsafe extern "C" fn ignore_event(_block: *const BlockLiteral, _event: XpcObject) {}

static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<BlockLiteral>(),
};

struct SharedBlock(BlockLiteral);
// SAFETY: the block is immutable after construction and has no captures;
// the blocks runtime treats global blocks as shared constants.
unsafe impl Send for SharedBlock {}
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
                    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
                    out.insert(key.clone(), XpcValue::String(s));
                }
            }
            XpcKind::Data => {
                let mut len = 0usize;
                // SAFETY: as above; `len` receives the byte count.
                let p = unsafe { xpc_dictionary_get_data(reply.0, ckey.as_ptr(), &mut len) };
                if !p.is_null() {
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
    use awman::engine::oci::apple_store::{check_release, CONTRACT};

    fn native_enabled() -> bool {
        std::env::var("AWMAN_TEST_APPLE_CONTAINER").is_ok_and(|v| v == "1")
    }

    fn wait(secs: u64) -> (Instant, fn() -> bool) {
        (Instant::now() + Duration::from_secs(secs), || false)
    }

    #[test]
    fn the_event_handler_is_one_global_block() {
        let a = event_handler();
        assert_eq!(a, event_handler());
        let block = unsafe { &*(a as *const BlockLiteral) };
        assert_eq!(block.flags, BLOCK_IS_GLOBAL);
    }

    #[test]
    fn an_unregistered_service_is_unavailable() {
        let (deadline, stop) = wait(10);
        let err = NativeXpc
            .send(
                "com.example.awman.no-such-service",
                &[(CONTRACT.route_key, XpcValue::String("ping".into()))],
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

    /// Requires the real, running Apple Containers service
    /// (`AWMAN_TEST_APPLE_CONTAINER=1`); fails closed when it is absent.
    #[test]
    fn apple_native_ping_reports_a_supported_release() {
        if !native_enabled() {
            return;
        }
        let (deadline, stop) = wait(60);
        let reply = NativeXpc
            .send(
                CONTRACT.api_service,
                &[(
                    CONTRACT.route_key,
                    XpcValue::String(CONTRACT.ping_route.into()),
                )],
                &[
                    (CONTRACT.version_key, XpcKind::String),
                    (CONTRACT.commit_key, XpcKind::String),
                    (CONTRACT.build_key, XpcKind::String),
                ],
                &WaitControl {
                    deadline,
                    stop: &stop,
                },
            )
            .expect("the Apple Containers API server must answer");
        let release = check_release(&reply).expect("installed release must be supported");
        eprintln!("apple container release {release:?}");
    }

    #[test]
    fn apple_native_list_returns_image_descriptions() {
        if !native_enabled() {
            return;
        }
        let (deadline, stop) = wait(60);
        let reply = NativeXpc
            .send(
                CONTRACT.images_service,
                &[(
                    CONTRACT.route_key,
                    XpcValue::String(CONTRACT.list_route.into()),
                )],
                &[(CONTRACT.descriptions_key, XpcKind::Data)],
                &WaitControl {
                    deadline,
                    stop: &stop,
                },
            )
            .expect("the image helper must answer");
        let Some(XpcValue::Data(listing)) = reply.get(CONTRACT.descriptions_key) else {
            panic!("no descriptions in {reply:?}");
        };
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(listing).unwrap();
        eprintln!("apple store lists {} images", parsed.len());
    }

    #[test]
    fn apple_native_stop_cancels_an_outstanding_request() {
        if !native_enabled() {
            return;
        }
        let stop = || true;
        let err = NativeXpc.send(
            CONTRACT.images_service,
            &[(
                CONTRACT.route_key,
                XpcValue::String(CONTRACT.list_route.into()),
            )],
            &[],
            &WaitControl {
                deadline: Instant::now() + Duration::from_secs(60),
                stop: &stop,
            },
        );
        // Either the reply beat the first poll or the stop was honoured.
        assert!(matches!(err, Ok(_) | Err(XpcFailure::Stopped)), "{err:?}");
    }

    /// Exports a real store image through the production acquirer and
    /// validates it. `AWMAN_TEST_APPLE_IMAGE` names the image (default
    /// `awman-amux:latest`).
    #[test]
    fn apple_native_export_round_trip() {
        if !native_enabled() {
            return;
        }
        use awman::data::config::env::EnvSnapshot;
        use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
        use awman::data::oci_identity::OciPlatform;
        use awman::engine::oci::{
            AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace,
            ImageAcquirer, RetryPolicy,
        };
        struct Unknown;
        impl DiskSpace for Unknown {
            fn available(&self, _: &std::path::Path) -> Option<u64> {
                None
            }
        }
        let image =
            std::env::var("AWMAN_TEST_APPLE_IMAGE").unwrap_or_else(|_| "awman-amux:latest".into());
        let state = tempfile::tempdir().unwrap();
        let acq = CachingAcquirer::new(
            state.path(),
            AcquireLimits::DEFAULT,
            EnvSnapshot::empty(),
            std::sync::Arc::new(Unknown),
        )
        .with_retry(RetryPolicy::NONE)
        .with_apple_transport(Some(std::sync::Arc::new(NativeXpc)));
        let request = AcquireRequest {
            tag: image.clone(),
            source: ImageSourceSpec::AppleStore { reference: None },
            platform: OciPlatform::host_linux(),
            policy: AcquirePolicy::IfMissing,
            registries: Default::default(),
        };
        let started = Instant::now();
        let got = acq.acquire(&request, &mut |_| {}).expect("native export");
        assert_eq!(got.identity.source, ImageSourceKind::AppleStore);
        // Keep the validated archive for guest tests that follow.
        if let Some(keep) = std::env::var_os("AWMAN_TEST_APPLE_KEEP_ARCHIVE") {
            std::fs::copy(&got.archive, keep).expect("keep the exported archive");
        }
        eprintln!(
            "exported {image} in {:?}: manifest {} config {} platform {} ({} bytes)",
            started.elapsed(),
            got.identity.manifest_digest,
            got.identity.config_digest,
            got.identity.platform,
            got.bytes
        );
    }

    fn native_acquirer(state: &std::path::Path) -> awman::engine::oci::CachingAcquirer {
        use awman::data::config::env::EnvSnapshot;
        use awman::engine::oci::{AcquireLimits, CachingAcquirer, DiskSpace, RetryPolicy};
        struct Unknown;
        impl DiskSpace for Unknown {
            fn available(&self, _: &std::path::Path) -> Option<u64> {
                None
            }
        }
        CachingAcquirer::new(
            state,
            AcquireLimits::DEFAULT,
            EnvSnapshot::empty(),
            std::sync::Arc::new(Unknown),
        )
        .with_retry(RetryPolicy::NONE)
        .with_apple_transport(Some(std::sync::Arc::new(NativeXpc)))
    }

    fn native_request(
        policy: awman::engine::oci::AcquirePolicy,
    ) -> awman::engine::oci::AcquireRequest {
        use awman::data::config::image_source::ImageSourceSpec;
        use awman::data::oci_identity::OciPlatform;
        awman::engine::oci::AcquireRequest {
            tag: std::env::var("AWMAN_TEST_APPLE_IMAGE")
                .unwrap_or_else(|_| "awman-amux:latest".into()),
            source: ImageSourceSpec::AppleStore { reference: None },
            platform: OciPlatform::host_linux(),
            policy,
            registries: Default::default(),
        }
    }

    fn staging_entries(state: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(state.join("oci-cache").join("tmp"))
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    }

    /// Cancels a live export mid-flight; the helper's late write must not
    /// leave anything in awman's staging area or cache.
    #[test]
    fn apple_native_cancelled_export_leaves_nothing() {
        if !native_enabled() {
            return;
        }
        use awman::engine::oci::{AcquirePolicy, ImageAcquirer};
        let state = tempfile::tempdir().unwrap();
        let acq = native_acquirer(state.path());
        let token = acq.cancel_token();
        // Cancel while `imageSave` is outstanding: as soon as the bridge's
        // private export directory exists.
        let watch = state.path().to_path_buf();
        let canceller = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                if staging_entries(&watch)
                    .iter()
                    .any(|d| d.join("apple-export").exists())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            token.cancel();
        });
        let started = Instant::now();
        let err = acq
            .acquire(&native_request(AcquirePolicy::IfMissing), &mut |_| {})
            .expect_err("the export must be cancelled");
        canceller.join().unwrap();
        assert!(awman::engine::oci::retry::is_cancelled(&err), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        // Give the helper time to finish (or fail) its abandoned write.
        std::thread::sleep(Duration::from_secs(20));
        assert!(
            staging_entries(state.path()).is_empty(),
            "{:?}",
            staging_entries(state.path())
        );
        let images = std::fs::read_dir(state.path().join("oci-cache").join("images"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(images, 0);
    }

    /// Two consumers importing the same image at once both get the one
    /// committed image.
    #[test]
    fn apple_native_concurrent_imports_share_one_commit() {
        if !native_enabled() {
            return;
        }
        use awman::engine::oci::{AcquirePolicy, ImageAcquirer};
        let state = tempfile::tempdir().unwrap();
        let path = state.path().to_path_buf();
        let run = |p: std::path::PathBuf| {
            std::thread::spawn(move || {
                native_acquirer(&p)
                    .acquire(&native_request(AcquirePolicy::IfMissing), &mut |_| {})
                    .map(|i| i.identity.clone())
            })
        };
        let (a, b) = (run(path.clone()), run(path.clone()));
        let (a, b) = (a.join().unwrap().unwrap(), b.join().unwrap().unwrap());
        assert_eq!(a, b);
        assert!(staging_entries(&path).is_empty());
    }

    /// Kills the image helper while `imageSave` is outstanding. The bridge
    /// must report a retryable interruption, and the next attempt must
    /// succeed once launchd restarts the helper on demand. Opt-in with
    /// `AWMAN_TEST_APPLE_KILL_HELPER=1`: it disrupts the running service.
    /// The helper's save takes only a second or two, so name a large image
    /// with `AWMAN_TEST_APPLE_IMAGE` for the kill to land mid-save.
    #[test]
    fn apple_native_helper_crash_is_retried() {
        if !native_enabled() || std::env::var("AWMAN_TEST_APPLE_KILL_HELPER").as_deref() != Ok("1")
        {
            return;
        }
        use awman::engine::oci::{AcquirePolicy, ImageAcquirer, RetryPolicy};
        use std::sync::{Arc, Mutex};
        /// Records every native outcome as `(route, failure)`.
        struct Recording(Mutex<Vec<(String, Option<XpcFailure>)>>);
        impl XpcTransport for Recording {
            fn send(
                &self,
                service: &str,
                request: &[(&str, XpcValue)],
                keys: &[(&str, XpcKind)],
                wait: &WaitControl<'_>,
            ) -> Result<XpcReply, XpcFailure> {
                let route = request
                    .iter()
                    .find_map(|(_, v)| match v {
                        XpcValue::String(r) if r.starts_with("image") || r == "ping" => {
                            Some(r.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                let result = NativeXpc.send(service, request, keys, wait);
                self.0
                    .lock()
                    .unwrap()
                    .push((route, result.as_ref().err().cloned()));
                result
            }
        }
        let state = tempfile::tempdir().unwrap();
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let acq = native_acquirer(state.path())
            .with_apple_transport(Some(recording.clone()))
            .with_retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_secs(2),
                ..RetryPolicy::DEFAULT
            });
        let watch = state.path().to_path_buf();
        let killer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                if staging_entries(&watch)
                    .iter()
                    .any(|d| d.join("apple-export").exists())
                {
                    std::thread::sleep(Duration::from_millis(100));
                    return std::process::Command::new("/usr/bin/pkill")
                        .args([
                            "-9",
                            "-f",
                            "^/.*/container-core-images/bin/container-core-images start",
                        ])
                        .status()
                        .is_ok_and(|s| s.success());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            false
        });
        let got = acq
            .acquire(&native_request(AcquirePolicy::IfMissing), &mut |_| {})
            .expect("the export is retried after the helper restarts");
        assert!(killer.join().unwrap(), "the helper was not killed");
        let calls = recording.0.lock().unwrap().clone();
        eprintln!("helper-crash calls: {calls:?}");
        assert!(
            calls
                .iter()
                .any(|(r, f)| r == "imageSave" && *f == Some(XpcFailure::Interrupted)),
            "{calls:?}"
        );
        assert!(
            matches!(calls.last(), Some((r, None)) if r == "imageSave"),
            "{calls:?}"
        );
        assert!(staging_entries(state.path()).is_empty());
        eprintln!("helper crash retried: {}", got.identity.manifest_digest);
    }
}
