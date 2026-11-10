//! Interactive input for linking. No flag or pipe can approve a browser.
use crate::command::error::CommandError;
use std::io::{IsTerminal, Read};
pub fn confirm() -> Result<bool, CommandError> {
    if !std::io::stdin().is_terminal() {
        return Err(crate::command::commands::link::invalid(
            "run this command in an interactive terminal",
        ));
    }
    Ok(confirm_from_reader(&mut std::io::stdin().lock()))
}

fn confirm_from_reader(reader: &mut impl std::io::BufRead) -> bool {
    let mut line = String::new();
    // EOF and failed reads deny, exactly like every response except y/yes.
    if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn read_token() -> Result<String, CommandError> {
    let mut token = String::new();
    std::io::stdin()
        .read_to_string(&mut token)
        .map_err(|e| CommandError::Other(e.to_string()))?;
    Ok(token.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn browser_confirmation_denies_failed_partial_reads() {
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::Other.into())
            }
        }
        for prefix in ["y", "yes"] {
            let mut input = BufReader::new(Cursor::new(prefix).chain(FailedRead));
            assert!(
                !confirm_from_reader(&mut input),
                "failed read after {prefix}"
            );
        }
        for answer in ["", "n\n", "yes please\n"] {
            assert!(!confirm_from_reader(&mut Cursor::new(answer)));
        }
        for answer in ["y\n", "YES\n"] {
            assert!(confirm_from_reader(&mut Cursor::new(answer)));
        }
    }
}
