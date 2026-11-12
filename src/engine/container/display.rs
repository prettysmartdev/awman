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
