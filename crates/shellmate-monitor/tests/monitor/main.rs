//! The monitor against an in-process SSH server that plays the part of a Linux machine (the script is
//! not run: the server answers the probe with a prepared text and writes frames), and `#[ignore]`d tests
//! against the real OpenSSH server of the `shellmate-e2e` container.

#[path = "../../../shellmate-ssh/tests/ssh/support.rs"]
mod support;

mod actions;
mod e2e;
mod fake;
mod lifecycle;
mod pages;
