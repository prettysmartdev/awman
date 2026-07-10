//! Display-safe formatting of container CLI arguments.
//!
//! Environment variable values are masked with `***` so the resulting
//! string is safe to log, print to TUI status bars, etc.

/// Take a set of CLI args and return a display-safe version where `-e VAR=val`
/// pairs have the value replaced with `***`.
///
/// The name-only form `-e VAR` (no `=`) carries no value in argv — the value
/// is inherited from the CLI child's environment, so there is nothing to mask.
/// It is rendered unchanged (`-e VAR`), matching the actual invocation. This is
/// how agent credentials are transported (see `docker::build_run_argv`).
pub fn mask_env_in_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for arg in args {
        if mask_next {
            if let Some(eq) = arg.find('=') {
                out.push(format!("{}=***", &arg[..eq]));
            } else {
                // Name-only `-e VAR`: no value present in argv, nothing to mask.
                out.push(arg.clone());
            }
            mask_next = false;
        } else if arg == "-e" {
            out.push(arg.clone());
            mask_next = true;
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Format masked args as a single shell-like string for display.
pub fn display_command(binary: &str, args: &[String]) -> String {
    let masked = mask_env_in_args(args);
    let mut parts = vec![binary.to_string()];
    parts.extend(masked);
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_env_replaces_values() {
        let args: Vec<String> = vec![
            "run",
            "--rm",
            "-e",
            "SECRET=hunter2",
            "-e",
            "PATH=/usr/bin",
            "image",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let masked = mask_env_in_args(&args);
        assert_eq!(masked[3], "SECRET=***");
        assert_eq!(masked[5], "PATH=***");
        assert_eq!(masked[6], "image");
    }

    #[test]
    fn mask_env_no_env_args_unchanged() {
        let args: Vec<String> = vec!["run", "--rm", "image"]
            .into_iter()
            .map(String::from)
            .collect();
        let masked = mask_env_in_args(&args);
        assert_eq!(masked, args);
    }

    #[test]
    fn display_command_includes_binary() {
        let args: Vec<String> = vec!["run", "--rm", "-e", "X=1", "img"]
            .into_iter()
            .map(String::from)
            .collect();
        let s = display_command("docker", &args);
        assert!(s.starts_with("docker "));
        assert!(s.contains("X=***"));
        assert!(!s.contains("X=1"));
    }

    // ── WI-0098 Finding A: name-only `-e KEY` credential form ─────────────────

    #[test]
    fn mask_env_name_only_form_rendered_unchanged() {
        // The name-only `-e KEY` form (how agent credentials are transported)
        // carries no value in argv, so there is nothing to mask — it renders
        // verbatim. A `KEY=VALUE` pair in the same args is still masked.
        let args: Vec<String> = vec![
            "run",
            "-e",
            "ANTHROPIC_API_KEY", // credential: name only, no value present
            "-e",
            "LITERAL=shown-then-masked",
            "img",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let masked = mask_env_in_args(&args);
        assert_eq!(masked[2], "ANTHROPIC_API_KEY", "name-only form unchanged");
        assert_eq!(masked[4], "LITERAL=***", "value form still masked");
        assert_eq!(masked[5], "img");
    }

    #[test]
    fn display_command_credential_name_only_shows_key_without_value() {
        let args: Vec<String> = vec!["run", "-e", "SECRET_KEY", "img"]
            .into_iter()
            .map(String::from)
            .collect();
        let s = display_command("docker", &args);
        // The displayed command matches the real invocation: `-e SECRET_KEY`
        // with no `=` and no value anywhere.
        assert!(s.contains("-e SECRET_KEY"), "display: {s}");
        assert!(
            !s.contains("SECRET_KEY="),
            "no value should be rendered: {s}"
        );
    }
}
