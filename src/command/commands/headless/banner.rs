//! Legacy box-drawing banner emitted when a fresh API key is generated.
//! Format is byte-identical to `oldsrc/commands/headless/auth.rs::print_key_banner`.

/// Render the legacy banner around a 64-char hex API key.
pub fn render_api_key_banner(key: &str) -> String {
    // Inner width chosen to fit the title verbatim; matches oldsrc.
    let inner_width: usize = 67;
    let key_line = format!("  {key}  ");
    let key_padded = format!("{:<width$}", key_line, width = inner_width);
    let title_line = "  amux headless API key (store this — it will not be shown again)  ";
    let bar = "═".repeat(inner_width);
    format!("╔{bar}╗\n║{title_line}║\n║{key_padded}║\n╚{bar}╝")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_uses_box_drawing_characters() {
        let out = render_api_key_banner(&"a".repeat(64));
        assert!(out.starts_with("╔"), "banner must open with ╔");
        assert!(out.ends_with("╝"), "banner must close with ╝");
        assert!(
            out.contains("amux headless API key (store this"),
            "banner must include legacy title"
        );
    }

    #[test]
    fn banner_includes_key_inline() {
        let key = "deadbeef".repeat(8);
        let out = render_api_key_banner(&key);
        assert!(out.contains(&key), "banner must include the key inline");
    }
}
