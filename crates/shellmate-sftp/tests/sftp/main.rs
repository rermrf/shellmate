//! SFTP tests: an in-process SSH server serving the `sftp` subsystem over a temporary
//! directory (the fixtures of the SSH crate are shared), and `#[ignore]`d tests against a real
//! OpenSSH server.

#[path = "../../../shellmate-ssh/tests/ssh/support.rs"]
mod support;

mod fixture;
mod browse;
mod e2e;
mod lifecycle;
mod ops;
mod text;
mod transfer;
