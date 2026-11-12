//! Library side of FakeCli protocol v1 (format documented in
//! `fake-cli/main.rs`).

use crate::TestSandbox;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The protocol version this library writes.
pub const FAKE_PROTOCOL: u32 = 1;
const HEADER: &str = "awman-fake-cli rules v1";

fn escape(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
        .replace('\t', "%09")
}

fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Default)]
struct RuleSpec {
    args: Vec<String>,
    stdout: String,
    stderr: String,
    exit: i32,
    read_stdin: bool,
    hold: bool,
    touch: Vec<String>,
    spawn_child: bool,
}

impl RuleSpec {
    fn render(&self) -> String {
        let mut out = String::from("rule\n");
        for a in &self.args {
            out.push_str(&format!("args {}\n", escape(a)));
        }
        if !self.stdout.is_empty() {
            out.push_str(&format!("stdout {}\n", escape(&self.stdout)));
        }
        if !self.stderr.is_empty() {
            out.push_str(&format!("stderr {}\n", escape(&self.stderr)));
        }
        out.push_str(&format!("exit {}\n", self.exit));
        if self.read_stdin {
            out.push_str("read-stdin\n");
        }
        if self.hold {
            out.push_str("hold\n");
        }
        for t in &self.touch {
            out.push_str(&format!("touch {}\n", escape(t)));
        }
        if self.spawn_child {
            out.push_str("spawn-child\n");
        }
        out.push_str("end\n");
        out
    }
}

/// One installed fake executable and its scenario.
///
/// ```no_run
/// # use awman_test_support::{TestSandbox, TestCase};
/// # let sandbox = TestSandbox::for_test(TestCase::component("doc"));
/// # let fake_cli_exe = std::path::Path::new("x");
/// let docker = sandbox.install_fake(fake_cli_exe, "docker");
/// docker.on(&["version"]).stdout("27.0.0\n");
/// docker.on(&["run"]).stderr("no space left\n").exit(125);
/// ```
#[derive(Clone)]
pub struct FakeCli {
    persona: String,
    exe: PathBuf,
    dir: PathBuf,
    rules: Arc<Mutex<Vec<RuleSpec>>>,
}

impl std::fmt::Debug for FakeCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeCli")
            .field("persona", &self.persona)
            .field("exe", &self.exe)
            .finish()
    }
}

impl FakeCli {
    pub(crate) fn install(sandbox: &TestSandbox, fake_cli_exe: &Path, name: &str) -> FakeCli {
        check_protocol(sandbox, fake_cli_exe);
        let exe = crate::exe::install_as(fake_cli_exe, &sandbox.bin(), name);
        let persona = exe
            .file_stem()
            .expect("installed name")
            .to_string_lossy()
            .into_owned();
        let dir = sandbox.bin().join(".fake").join(&persona);
        std::fs::create_dir_all(dir.join("calls")).expect("create fake dir");
        let fake = FakeCli {
            persona,
            exe,
            dir,
            rules: Arc::new(Mutex::new(Vec::new())),
        };
        fake.write_rules();
        fake
    }

    /// The installed executable (`<bin>/<name>`).
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// The scenario directory (`<bin>/.fake/<name>`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A rule for invocations whose arguments start with `args` (empty =
    /// every invocation). It takes effect when the returned [`Rule`] is dropped
    /// (end of the statement).
    pub fn on(&self, args: &[&str]) -> Rule {
        Rule {
            fake: self.clone(),
            spec: RuleSpec {
                args: args.iter().map(|s| s.to_string()).collect(),
                ..RuleSpec::default()
            },
        }
    }

    fn add(&self, spec: RuleSpec) {
        self.rules.lock().unwrap().push(spec);
        self.write_rules();
    }

    /// Rewrite the rules file atomically (temp + rename), so a running fake
    /// never reads half a file.
    fn write_rules(&self) {
        let mut text = format!("{HEADER}\n");
        for rule in self.rules.lock().unwrap().iter() {
            text.push_str(&rule.render());
        }
        let mut temp = tempfile::NamedTempFile::new_in(&self.dir).expect("rules temp file");
        temp.write_all(text.as_bytes()).expect("write rules");
        temp.persist(self.dir.join("rules")).expect("install rules");
    }

    /// Recorded invocations, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(self.dir.join("calls"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        dirs.sort();
        dirs.iter().map(|d| Call::read(d)).collect()
    }

    /// Wait until at least `n` invocations finished; returns them.
    pub fn wait_for_calls(&self, n: usize, limit: Duration) -> Vec<Call> {
        crate::wait::wait_for_with(
            &format!("{n} finished call(s) of fake {}", self.persona),
            limit,
            || {
                let calls = self.calls();
                (calls.iter().filter(|c| c.exit.is_some()).count() >= n).then_some(calls)
            },
            || format!("{:#?}", self.calls()),
        )
    }

    /// Wait until an invocation is blocked in a `hold` rule.
    pub fn wait_until_held(&self, limit: Duration) -> Call {
        crate::wait::wait_for(&format!("fake {} to hold", self.persona), limit, || {
            self.calls().into_iter().find(|c| c.held)
        })
    }

    /// Let every `hold` invocation continue.
    pub fn release(&self) {
        std::fs::write(self.dir.join("release"), b"").expect("release");
    }
}

/// A rule being built; written when dropped.
pub struct Rule {
    fake: FakeCli,
    spec: RuleSpec,
}

impl Rule {
    /// Print this to stdout.
    pub fn stdout(mut self, text: &str) -> Self {
        self.spec.stdout.push_str(text);
        self
    }

    /// Print this to stderr.
    pub fn stderr(mut self, text: &str) -> Self {
        self.spec.stderr.push_str(text);
        self
    }

    /// Exit with this code (default 0).
    pub fn exit(mut self, code: i32) -> Self {
        self.spec.exit = code;
        self
    }

    /// Block until [`FakeCli::release`] (event-based cancellation tests).
    pub fn hold_until_released(mut self) -> Self {
        self.spec.hold = true;
        self
    }

    /// Create this file when invoked.
    pub fn touch(mut self, path: &Path) -> Self {
        self.spec.touch.push(path.to_string_lossy().into_owned());
        self
    }

    /// Read and record stdin.
    pub fn read_stdin(mut self) -> Self {
        self.spec.read_stdin = true;
        self
    }

    /// Start a grandchild that never exits (process-tree tests).
    pub fn spawn_child(mut self) -> Self {
        self.spec.spawn_child = true;
        self
    }
}

impl Drop for Rule {
    fn drop(&mut self) {
        self.fake.add(std::mem::take(&mut self.spec));
    }
}

/// One recorded invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Call {
    /// Arguments (without the program name).
    pub argv: Vec<String>,
    /// The environment the fake saw.
    pub env: BTreeMap<String, String>,
    /// Its working directory.
    pub cwd: PathBuf,
    /// stdin, when the rule said `read_stdin`.
    pub stdin: Vec<u8>,
    /// Whether it is (or was) held.
    pub held: bool,
    /// pid of the grandchild started by `spawn_child`.
    pub child_pid: Option<u32>,
    /// Exit code once finished.
    pub exit: Option<i32>,
}

impl Call {
    fn read(dir: &Path) -> Call {
        let lines = |name: &str| -> Vec<String> {
            std::fs::read_to_string(dir.join(name))
                .map(|t| t.lines().map(unescape).collect())
                .unwrap_or_default()
        };
        Call {
            argv: lines("argv"),
            env: lines("env")
                .into_iter()
                .filter_map(|l| {
                    l.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect(),
            cwd: lines("cwd")
                .into_iter()
                .next()
                .map(PathBuf::from)
                .unwrap_or_default(),
            stdin: std::fs::read(dir.join("stdin")).unwrap_or_default(),
            held: dir.join("held").exists(),
            child_pid: lines("child-pid").first().and_then(|p| p.parse().ok()),
            exit: lines("done").first().and_then(|c| c.parse().ok()),
        }
    }
}

/// Check `<exe> --fake-protocol` once per process per executable.
fn check_protocol(sandbox: &TestSandbox, exe: &Path) {
    static CHECKED: OnceLock<Mutex<BTreeMap<PathBuf, ()>>> = OnceLock::new();
    let checked = CHECKED.get_or_init(|| Mutex::new(BTreeMap::new()));
    if checked.lock().unwrap().contains_key(exe) {
        return;
    }
    assert!(
        exe.is_file(),
        "BLOCKED[fake-cli]: {} does not exist",
        exe.display()
    );
    sandbox.allow_exe_path(exe);
    let out = sandbox
        .command(exe)
        .arg("--fake-protocol")
        .label("fake-cli --fake-protocol")
        .run();
    let version = out.stdout_text().trim().to_string();
    assert_eq!(
        version,
        FAKE_PROTOCOL.to_string(),
        "{} speaks FakeCli protocol {version:?}; this library speaks {FAKE_PROTOCOL}",
        exe.display()
    );
    checked.lock().unwrap().insert(exe.to_path_buf(), ());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_round_trips() {
        for text in ["plain", "a\nb\tc\r%25", "", "100%"] {
            assert_eq!(unescape(&escape(text)), text);
        }
    }

    #[test]
    fn rule_render_is_line_oriented() {
        let spec = RuleSpec {
            args: vec!["image".into(), "ls".into()],
            stdout: "a\nb\n".into(),
            exit: 3,
            hold: true,
            ..RuleSpec::default()
        };
        assert_eq!(
            spec.render(),
            "rule\nargs image\nargs ls\nstdout a%0Ab%0A\nexit 3\nhold\nend\n"
        );
    }
}
