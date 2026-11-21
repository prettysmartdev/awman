//! A tolerant JSON-with-comments reader — Layer 0.
//!
//! OpenCode accepts `opencode.json` and `opencode.jsonc` with `//` and
//! `/* */` comments and trailing commas. awman reads those files only to
//! verify configuration (the `--plan` guard), so a small pre-pass that
//! removes comments and trailing commas outside strings, followed by
//! `serde_json`, is enough. No new dependency.

/// Parse `text` as JSON after removing comments and trailing commas that are
/// outside string literals. Returns the parse error text on failure.
pub fn parse_tolerant(text: &str) -> Result<serde_json::Value, String> {
    let stripped = strip(text)?;
    serde_json::from_str(&stripped).map_err(|e| e.to_string())
}

fn strip(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' => {
                // Copy the whole string literal, honouring escapes.
                out.push(b);
                i += 1;
                loop {
                    let Some(&c) = bytes.get(i) else {
                        return Err("unterminated string".into());
                    };
                    out.push(c);
                    i += 1;
                    if c == b'\\' {
                        if let Some(&next) = bytes.get(i) {
                            out.push(next);
                            i += 1;
                        }
                    } else if c == b'"' {
                        break;
                    }
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                loop {
                    if i + 1 >= bytes.len() {
                        return Err("unterminated comment".into());
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                // Keep tokens on either side of the comment apart.
                out.push(b' ');
            }
            b']' | b'}' => {
                // Drop a trailing comma before the closer (whitespace allowed).
                let mut end = out.len();
                while end > 0 && out[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                if end > 0 && out[end - 1] == b',' {
                    out.remove(end - 1);
                }
                out.push(b);
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_tolerant;
    use serde_json::json;

    #[test]
    fn comments_and_trailing_commas_outside_strings_are_ignored() {
        let text = r#"{
            // line comment
            "a": 1, /* block */ "b": [1, 2,],
            "c": {"d": true,},
        }"#;
        assert_eq!(
            parse_tolerant(text).unwrap(),
            json!({"a": 1, "b": [1, 2], "c": {"d": true}})
        );
    }

    #[test]
    fn strings_containing_comment_markers_and_commas_are_kept() {
        let text = r#"{"url": "http://x//y", "s": "/* no */", "t": "a,}", "q": "say \"//\""}"#;
        assert_eq!(
            parse_tolerant(text).unwrap(),
            json!({"url": "http://x//y", "s": "/* no */", "t": "a,}", "q": "say \"//\""})
        );
    }

    #[test]
    fn malformed_input_is_an_error() {
        assert!(parse_tolerant("{\"a\": 1").is_err());
        assert!(parse_tolerant("{\"a\": \"x}").is_err());
        assert!(parse_tolerant("{ /* never closed }").is_err());
        assert!(parse_tolerant("plain text").is_err());
    }
}
