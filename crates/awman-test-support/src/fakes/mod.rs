//! Verified fakes. Each fake has a conformance pair in the gate manifest
//! (`[fakes.<id>] fake_test = …, real_test = …`): the same
//! [`crate::conformance`] checks run against the fake in `component` and
//! against the real thing in `boundary`, so hermetic tests cannot drift from
//! reality.

mod cli;
mod git_http;
mod registry;
mod work_server;

pub use cli::{Call, FakeCli, Rule, FAKE_PROTOCOL};
pub use git_http::FakeGitHttp;
pub use registry::FakeRegistry;
pub use work_server::FakeWorkServer;

/// Install the rules a fake `docker` needs to pass
/// [`crate::conformance::docker_cli`]. Engine tests add their own rules on
/// top (a longer argument prefix wins).
pub fn docker_cli_baseline(docker: &FakeCli) {
    docker
        .on(&["version", "--format", "{{.Client.Version}}"])
        .stdout("27.5.1\n");
    docker
        .on(&["info", "--format", "{{.ServerVersion}}"])
        .stdout("27.5.1\n");
    docker
        .on(&["image", "inspect"])
        .stderr("Error response from daemon: No such image: awman-conformance-missing:none\n")
        .exit(1);
}
