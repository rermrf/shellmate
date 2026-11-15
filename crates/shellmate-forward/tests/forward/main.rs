//! Forwarding tests: an in-process SSH server on 127.0.0.1 (the fixtures of the SSH crate are
//! shared) with echo services behind it, and `#[ignore]`d tests against a real OpenSSH server.
//! The tests only listen on loopback ports chosen by the system.

#[path = "../../../shellmate-ssh/tests/ssh/support.rs"]
mod support;

mod e2e;
mod flow;
mod lifecycle;
mod local;
mod remote;
mod rig;
mod socks5;
