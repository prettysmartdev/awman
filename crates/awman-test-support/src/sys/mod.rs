//! Operating-system specifics of the supervisor: process groups / job objects
//! and inherited-descriptor hygiene. Everything else is portable.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
