//! Adapt the native dictionary transport without exposing engine policy to it.
use awman::engine::oci::apple_store::{WaitControl, XpcFailure, XpcKind, XpcReply, XpcValue};
use awman_host_shims::xpc as native;

#[cfg(target_os = "macos")]
pub struct NativeXpc;

#[cfg(target_os = "macos")]
impl awman::engine::oci::apple_store::XpcTransport for NativeXpc {
    fn send(
        &self,
        service: &str,
        request: &[(&str, XpcValue)],
        reply_keys: &[(&str, XpcKind)],
        wait: &WaitControl<'_>,
    ) -> Result<XpcReply, XpcFailure> {
        send(
            &awman_host_shims::NativeXpc,
            service,
            request,
            reply_keys,
            wait,
        )
    }
}

fn send(
    transport: &dyn native::XpcTransport,
    service: &str,
    request: &[(&str, XpcValue)],
    reply_keys: &[(&str, XpcKind)],
    wait: &WaitControl<'_>,
) -> Result<XpcReply, XpcFailure> {
    let request: Vec<_> = request
        .iter()
        .map(|(key, value)| {
            (
                *key,
                match value {
                    XpcValue::String(value) => native::XpcValue::String(value.clone()),
                    XpcValue::Data(value) => native::XpcValue::Data(value.clone()),
                },
            )
        })
        .collect();
    let keys: Vec<_> = reply_keys
        .iter()
        .map(|(key, kind)| {
            (
                *key,
                match kind {
                    XpcKind::String => native::XpcKind::String,
                    XpcKind::Data => native::XpcKind::Data,
                },
            )
        })
        .collect();
    transport
        .send(
            service,
            &request,
            &keys,
            &native::WaitControl {
                deadline: wait.deadline,
                stop: wait.stop,
            },
        )
        .map(|reply| {
            reply
                .into_iter()
                .map(|(key, value)| {
                    (
                        key,
                        match value {
                            native::XpcValue::String(value) => XpcValue::String(value),
                            native::XpcValue::Data(value) => XpcValue::Data(value),
                        },
                    )
                })
                .collect()
        })
        .map_err(|error| match error {
            native::XpcFailure::Unavailable => XpcFailure::Unavailable,
            native::XpcFailure::Interrupted => XpcFailure::Interrupted,
            native::XpcFailure::Stopped => XpcFailure::Stopped,
            native::XpcFailure::TimedOut => XpcFailure::TimedOut,
            native::XpcFailure::UnexpectedReply => XpcFailure::UnexpectedReply,
        })
}
