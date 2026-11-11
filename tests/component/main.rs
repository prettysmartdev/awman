//! `component` tier (WI 0144 design 8): loopback sockets, owned subprocesses
//! (the debug `awman` binary, the fake CLI), local `git` on temporary
//! repositories, in-process daemon. Budget: 20 s per test.
//!
//! The module list is final; slices add files only inside their domain
//! directory (ownership.md). Every test builds its sandbox with
//! `support::sandbox("<module path>")`, which declares the component tier.

mod support;

mod command;
mod daemon;
mod data;
mod engine;
mod frontend;
mod parity;
mod security;
mod squad;
mod tools;
mod worker;
