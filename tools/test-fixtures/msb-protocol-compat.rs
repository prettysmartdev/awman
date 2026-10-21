// Compiled against both published protocol versions by test-msb-protocol-compat.sh.
// Fixtures are produced by the actual typed serializers, never a schema imitation.
use microsandbox_protocol::{
    bulk::BULK_PROTOCOL_VERSION,
    codec::{decode_message_frame, encode_to_buf},
    core::{Ping, Ready},
    exec::{ExecExited, ExecRequest, ExecStderr, ExecStdin, ExecStdout},
    message::{Message, MessageType, PROTOCOL_VERSION},
};
use std::path::PathBuf;

fn frames() -> Vec<(&'static str, Message)> {
    vec![
        ("ready", Message::with_payload(MessageType::Ready, 0, &Ready {
            boot_time_ns: 11, init_time_ns: 22, ready_time_ns: 33,
            agent_version: "synthetic-fixture".into(), ..Default::default()
        }).unwrap()),
        ("exec", Message::with_payload(MessageType::ExecRequest, 42, &serde_json::from_value::<ExecRequest>(
            serde_json::json!({"cmd":"/bin/sh","args":["-c","exit 37"],"env":["SYNTHETIC=1"],"cwd":"/tmp","tty":false})
        ).unwrap()).unwrap()),
        ("exit", Message::with_payload(MessageType::ExecExited, 42, &ExecExited { code: 37 }).unwrap()),
        ("stdin", Message::with_payload(MessageType::ExecStdin, 42, &ExecStdin { data: vec![0, 255, 10, 128] }).unwrap()),
        ("stdout", Message::with_payload(MessageType::ExecStdout, 42, &ExecStdout { data: vec![0, 255, 10, 128] }).unwrap()),
        ("stderr", Message::with_payload(MessageType::ExecStderr, 42, &ExecStderr { data: b"synthetic error".to_vec() }).unwrap()),
        ("ping", Message::with_payload(MessageType::Ping, 7, &Ping {}).unwrap()),
    ]
}

fn directory() -> PathBuf {
    PathBuf::from(std::env::var_os("AWMAN_PROTOCOL_FIXTURES").unwrap())
}

#[test]
fn write_versioned_protocol_fixtures() {
    assert_eq!(PROTOCOL_VERSION, 9);
    assert_eq!(BULK_PROTOCOL_VERSION, 8);
    let directory = directory().join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&directory).unwrap();
    for (name, message) in frames() {
        let mut bytes = Vec::new();
        encode_to_buf(&message, &mut bytes).unwrap();
        std::fs::write(directory.join(name), bytes).unwrap();
    }
}

#[test]
fn read_other_version_protocol_fixtures() {
    let other = if env!("CARGO_PKG_VERSION") == "0.7.2" {
        "0.7.8"
    } else {
        "0.7.2"
    };
    for (name, expected) in frames() {
        let bytes = std::fs::read(directory().join(other).join(name)).unwrap();
        let decoded = decode_message_frame(&bytes).unwrap();
        assert_eq!(
            (decoded.v, decoded.id, decoded.flags, decoded.t),
            (expected.v, expected.id, expected.flags, expected.t),
            "{name}"
        );
        assert_eq!(decoded.p, expected.p, "{name} payload changed");
        let mut current = Vec::new();
        encode_to_buf(&expected, &mut current).unwrap();
        assert_eq!(bytes, current, "{name} framing changed");
        match name {
            "ready" => {
                assert_eq!(
                    decoded.payload::<Ready>().unwrap().agent_version,
                    "synthetic-fixture"
                );
            }
            "exec" => {
                assert_eq!(
                    decoded.payload::<ExecRequest>().unwrap().args,
                    ["-c", "exit 37"]
                );
            }
            "exit" => {
                assert_eq!(decoded.payload::<ExecExited>().unwrap().code, 37);
            }
            "stdin" => {
                assert_eq!(
                    decoded.payload::<ExecStdin>().unwrap().data,
                    [0, 255, 10, 128]
                );
            }
            "stdout" => {
                assert_eq!(
                    decoded.payload::<ExecStdout>().unwrap().data,
                    [0, 255, 10, 128]
                );
            }
            "stderr" => {
                assert_eq!(
                    decoded.payload::<ExecStderr>().unwrap().data,
                    b"synthetic error"
                );
            }
            "ping" => {
                decoded.payload::<Ping>().unwrap();
            }
            _ => unreachable!(),
        }
    }
}
