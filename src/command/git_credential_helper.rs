//! Private host Git credential protocol entry point. Runs before startup,
//! tracing, clap, API relay or engine detection, and never prints diagnostics.
use crate::data::config::env::EnvSnapshot;
use crate::engine::git::lease_transport::{
    answer_credential_request, CREDENTIAL_HELPER_SUBCOMMAND,
};
use std::io::{BufRead, Write};

pub fn dispatch(
    env: &EnvSnapshot,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Option<bool> {
    let args = env.argv();
    if args.get(1).and_then(|s| s.to_str()) != Some(CREDENTIAL_HELPER_SUBCOMMAND) {
        return None;
    }
    Some(
        (|| {
            if args.len() != 4 {
                return None;
            }
            let nonce = args[2].to_str()?;
            let action = args[3].to_str()?;
            if nonce.len() != 32
                || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
                || !matches!(action, "get" | "store" | "erase")
            {
                return None;
            }
            let file = env.lease_credential_file()?;
            answer_credential_request(action, nonce, &file, input, output).ok()
        })()
        .is_some(),
    )
}
