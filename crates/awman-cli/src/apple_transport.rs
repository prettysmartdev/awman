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

#[cfg(all(test, target_os = "macos"))]
mod native_tests;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Echo;
    impl native::XpcTransport for Echo {
        fn send(
            &self,
            service: &str,
            request: &[(&str, native::XpcValue)],
            keys: &[(&str, native::XpcKind)],
            wait: &native::WaitControl<'_>,
        ) -> Result<native::XpcReply, native::XpcFailure> {
            assert_eq!(service, "test-service");
            assert_eq!(
                keys,
                &[
                    ("text", native::XpcKind::String),
                    ("bytes", native::XpcKind::Data)
                ]
            );
            assert!((wait.stop)());
            assert!(wait.deadline > Instant::now());
            Ok(request
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect())
        }
    }

    #[test]
    fn adapter_preserves_values_keys_service_and_wait_control() {
        let request = [
            ("text", XpcValue::String("value".into())),
            ("bytes", XpcValue::Data(vec![0, 255])),
        ];
        let keys = [("text", XpcKind::String), ("bytes", XpcKind::Data)];
        let reply = send(
            &Echo,
            "test-service",
            &request,
            &keys,
            &WaitControl {
                deadline: Instant::now() + Duration::from_secs(60),
                stop: &|| true,
            },
        )
        .unwrap();
        assert_eq!(
            reply,
            request
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect()
        );
    }

    struct Failure(native::XpcFailure);
    impl native::XpcTransport for Failure {
        fn send(
            &self,
            _: &str,
            _: &[(&str, native::XpcValue)],
            _: &[(&str, native::XpcKind)],
            _: &native::WaitControl<'_>,
        ) -> Result<native::XpcReply, native::XpcFailure> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn adapter_preserves_every_transport_failure() {
        for (native, engine) in [
            (native::XpcFailure::Unavailable, XpcFailure::Unavailable),
            (native::XpcFailure::Interrupted, XpcFailure::Interrupted),
            (native::XpcFailure::Stopped, XpcFailure::Stopped),
            (native::XpcFailure::TimedOut, XpcFailure::TimedOut),
            (
                native::XpcFailure::UnexpectedReply,
                XpcFailure::UnexpectedReply,
            ),
        ] {
            assert_eq!(
                send(
                    &Failure(native),
                    "s",
                    &[],
                    &[],
                    &WaitControl {
                        deadline: Instant::now(),
                        stop: &|| false,
                    }
                ),
                Err(engine)
            );
        }
    }
}
