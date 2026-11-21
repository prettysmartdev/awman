//! Agent credential files with fake secrets (every secret starts with
//! `test-secret-`, so a leak scan can find it). Each secret is registered
//! for redaction in the sandbox's diagnostics and evidence.

use crate::TestSandbox;
use std::path::PathBuf;

/// One written credential file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenCredential {
    /// The file.
    pub path: PathBuf,
    /// The fake secret it contains.
    pub secret: String,
}

/// Writes fake agent credentials into the sandbox HOME.
pub struct CredentialFiles<'a> {
    sandbox: &'a TestSandbox,
}

impl<'a> CredentialFiles<'a> {
    /// Credentials for `sandbox`.
    pub fn new(sandbox: &'a TestSandbox) -> CredentialFiles<'a> {
        CredentialFiles { sandbox }
    }

    fn secret(&self, name: &str) -> String {
        let secret = format!("test-secret-{name}-0123456789abcdef");
        self.sandbox.redact(&secret);
        secret
    }

    /// Any file under HOME; `{secret}` in `template` is replaced by the secret.
    pub fn file(&self, relative: &str, name: &str, template: &str) -> WrittenCredential {
        let secret = self.secret(name);
        let path = self.sandbox.home().join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create credential dir");
        std::fs::write(&path, template.replace("{secret}", &secret)).expect("write credentials");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod credentials");
        }
        WrittenCredential { path, secret }
    }

    /// `~/.claude/.credentials.json`.
    pub fn claude(&self) -> WrittenCredential {
        self.file(
            ".claude/.credentials.json",
            "claude",
            r#"{"claudeAiOauth":{"accessToken":"{secret}","refreshToken":"{secret}-refresh","expiresAt":4102444800000}}"#,
        )
    }

    /// `~/.codex/auth.json`.
    pub fn codex(&self) -> WrittenCredential {
        self.file(
            ".codex/auth.json",
            "codex",
            r#"{"OPENAI_API_KEY":"{secret}"}"#,
        )
    }

    /// `~/.config/gh/hosts.yml`.
    pub fn gh(&self) -> WrittenCredential {
        self.file(
            ".config/gh/hosts.yml",
            "gh",
            "github.com:\n    oauth_token: {secret}\n    user: awman-test\n",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_secrets_inside_home_and_redacts_them() {
        let sandbox = TestSandbox::new();
        let creds = CredentialFiles::new(&sandbox);
        let claude = creds.claude();
        assert!(claude.path.starts_with(sandbox.home()));
        assert!(claude.secret.starts_with("test-secret-"));
        let text = std::fs::read_to_string(&claude.path).unwrap();
        assert!(text.contains(&claude.secret));
        assert!(creds.codex().path.ends_with(".codex/auth.json"));
        assert!(creds.gh().path.ends_with(".config/gh/hosts.yml"));
    }
}
