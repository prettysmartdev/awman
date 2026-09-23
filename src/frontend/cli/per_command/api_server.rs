//! `ApiServerCommandFrontend` impl for the CLI.

use async_trait::async_trait;

use crate::command::commands::api_server::{
    ApiKeyDisclosure, ApiServerCommandFrontend, ApiServerRuntime,
};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::frontend::cli::command_frontend::CliFrontend;

#[async_trait]
impl ApiServerCommandFrontend for CliFrontend {
    async fn serve_until_shutdown(
        &mut self,
        runtime: ApiServerRuntime,
    ) -> Result<(), CommandError> {
        crate::frontend::api::serve(runtime).await
    }

    fn show_api_key(&mut self, disclosure: &ApiKeyDisclosure) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: render_api_key_banner(disclosure.key()),
        });
    }
}

/// Draw the one-time API-key disclosure for a terminal.
///
/// Moved here from `src/command/commands/api_server/banner.rs`: the box is
/// presentation, and a command layer that composed it handed every frontend
/// terminal art it could not restyle — the TUI draws its own frames, and the
/// API serialised the `═` runs into JSON. Layer 2 now supplies the key; this
/// function is the CLI's way of saying it. Byte-identical to what the command
/// layer used to emit.
fn render_api_key_banner(key: &str) -> String {
    // Inner width chosen to fit the title verbatim; matches oldsrc.
    let inner_width: usize = 67;
    let key_line = format!("  {key}  ");
    let key_padded = format!("{:<width$}", key_line, width = inner_width);
    let title_line = "  awman API key (store this — it will not be shown again)         ";
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
            out.contains("awman API key (store this"),
            "banner must include title"
        );
    }

    #[test]
    fn banner_includes_key_inline() {
        let key = "deadbeef".repeat(8);
        let out = render_api_key_banner(&key);
        assert!(out.contains(&key), "banner must include the key inline");
    }

    /// The key banner presented on key generation must use "awman" branding,
    /// not the pre-rename "amux"/"headless". Moved here with the renderer
    /// from `tests/api_parity/rename_0077.rs`.
    #[test]
    fn banner_uses_awman_branding() {
        let banner = render_api_key_banner(&"a".repeat(64));
        let lower = banner.to_lowercase();
        assert!(
            lower.contains("awman"),
            "API key banner must mention 'awman'; got:\n{banner}"
        );
        assert!(
            !lower.contains("amux"),
            "API key banner must not mention 'amux'; got:\n{banner}"
        );
        assert!(
            !lower.contains("headless"),
            "API key banner must not mention 'headless'; got:\n{banner}"
        );
    }
}
