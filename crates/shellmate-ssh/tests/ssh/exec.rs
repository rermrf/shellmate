//! Exec channels: one-off commands with their output and status, and line streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use shellmate_core::AuthMethod;
use shellmate_ssh::{Connection, ExecOptions, ExecStatus, Route, Secret, SshError, SshHost, StreamOptions};

use crate::support::{Env, ExecChannel, ServerOptions, TestServer, eventually, exec_runner, lock};

async fn connect(server: &TestServer, env: &Env) -> Connection {
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    Connection::connect(Route::direct(host), env.trusting()).await.unwrap()
}

/// A server whose commands are answered by `run`.
async fn serving<F, Fut>(run: F) -> TestServer
where
    F: Fn(String, ExecChannel) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    TestServer::start(ServerOptions { exec: Some(exec_runner(run)), ..Default::default() }).await
}

fn quick(timeout_ms: u64, max_output: usize) -> ExecOptions {
    ExecOptions { timeout: Duration::from_millis(timeout_ms), max_output }
}

#[tokio::test]
async fn exec_returns_stdout_stderr_and_the_exit_status_apart() {
    let server = serving(|command, channel| async move {
        assert_eq!(command, "make-noise --loud");
        channel.stdout("hello ").await;
        channel.stderr("warning\n").await;
        channel.stdout("world\n").await;
        channel.exit(3).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;

    let output = connection.exec("make-noise --loud").await.unwrap();
    assert_eq!(output.stdout_text(), "hello world\n");
    assert_eq!(output.stderr_text(), "warning\n");
    assert_eq!(output.status, ExecStatus::Code(3));
    assert!(!output.status.success() && !output.truncated);
    assert_eq!(*lock(&server.state.execs), ["make-noise --loud"]);
    assert!(lock(&server.state.ptys).is_empty(), "an exec channel has no PTY");
}

#[tokio::test]
async fn a_successful_command_reports_success() {
    let server = serving(|_, channel| async move {
        channel.stdout("ok").await;
        channel.exit(0).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let output = connection.exec("true").await.unwrap();
    assert!(output.status.success());
    assert_eq!(output.status.code(), Some(0));
}

#[tokio::test]
async fn bytes_that_are_not_utf8_are_kept_and_lossily_readable() {
    let server = serving(|_, channel| async move {
        channel.stdout(b"caf\xc3\xa9 \xff\n".to_vec()).await;
        channel.exit(0).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let output = connection.exec("x").await.unwrap();
    assert_eq!(output.stdout, b"caf\xc3\xa9 \xff\n");
    assert_eq!(output.stdout_text(), "café \u{fffd}\n");
}

#[tokio::test]
async fn output_over_the_cap_is_cut_and_the_command_is_told_to_go_away() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sent = Arc::new(AtomicUsize::new(0));
    let server = serving({
        let (stopped, sent) = (stopped.clone(), sent.clone());
        move |_, channel| {
            let (stopped, sent) = (stopped.clone(), sent.clone());
            async move {
                // A command that never stops writing: 64 MiB are on offer.
                for _ in 0..1024 {
                    if !channel.stdout(vec![b'x'; 64 * 1024]).await {
                        stopped.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    sent.fetch_add(64 * 1024, Ordering::SeqCst);
                }
                channel.exit(0).await;
            }
        }
    })
    .await;
    let connection = connect(&server, &Env::new()).await;

    let output = connection.exec_with("yes", quick(10_000, 100_000)).await.unwrap();
    assert!(output.truncated);
    assert_eq!(output.stdout.len(), 100_000, "the start is kept, nothing beyond the cap");
    assert_eq!(output.status, ExecStatus::Unknown, "it was not waited for");
    eventually("the command to notice that the channel closed", || (stopped.load(Ordering::SeqCst) == 1).then_some(()))
        .await;
    assert!(sent.load(Ordering::SeqCst) < 32 * 1024 * 1024, "the writer was stopped long before it ran out");
}

#[tokio::test]
async fn stderr_has_its_own_cap_and_marks_the_result_truncated() {
    let server = serving(|_, channel| async move {
        channel.stderr(vec![b'e'; 5000]).await;
        channel.exit(1).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let output = connection.exec_with("x", quick(10_000, 1000)).await.unwrap();
    assert!(output.truncated);
    assert_eq!(output.stderr.len(), 1000);
    assert!(output.stdout.is_empty());
}

#[tokio::test]
async fn a_command_that_takes_too_long_times_out_and_its_channel_is_closed() {
    let closed = Arc::new(AtomicUsize::new(0));
    let server = serving({
        let closed = closed.clone();
        move |_, channel| {
            let closed = closed.clone();
            async move {
                channel.stdout("started\n").await;
                channel.wait_closed().await;
                closed.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
    .await;
    let connection = connect(&server, &Env::new()).await;

    let started = std::time::Instant::now();
    let err = connection.exec_with("sleep 1000", quick(300, 1000)).await.unwrap_err();
    assert!(matches!(err, SshError::ExecTimeout { limit } if limit == Duration::from_millis(300)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(err.to_string().contains("命令执行超时"), "{err}");
    eventually("the channel to be closed", || (closed.load(Ordering::SeqCst) == 1).then_some(())).await;
}

#[tokio::test]
async fn the_default_time_limit_is_the_budget_of_the_connection() {
    let server = serving(|_, channel| async move {
        channel.wait_closed().await;
    })
    .await;
    let env = Env::new();
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    let options = env.trusting().with_connect_watchdog(Duration::from_millis(400));
    let connection = Connection::connect(Route::direct(host), options).await.unwrap();
    let err = connection.exec("sleep 1000").await.unwrap_err();
    assert!(matches!(err, SshError::ExecTimeout { limit } if limit == Duration::from_millis(400)), "{err:?}");
}

#[tokio::test]
async fn a_server_that_refuses_exec_gives_a_clear_error() {
    let server = TestServer::start(ServerOptions::default()).await;
    let connection = connect(&server, &Env::new()).await;
    let err = connection.exec("true").await.unwrap_err();
    assert!(matches!(&err, SshError::Channel(text) if text.contains("执行命令")), "{err:?}");
    assert!(connection.exec_stream("true").await.is_err());
}

#[tokio::test]
async fn a_connection_that_drops_during_a_command_is_reported() {
    let server = serving(|_, channel| async move {
        channel.stdout("partial").await;
        channel.wait_closed().await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let killer = {
        let state = server.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            state.kill_connections();
        })
    };
    let err = connection.exec_with("x", quick(10_000, 1000)).await.unwrap_err();
    killer.await.unwrap();
    assert!(matches!(err, SshError::Disconnected(_)), "{err:?}");
}

#[tokio::test]
async fn several_commands_run_at_the_same_time_on_one_connection() {
    let server = serving(|command, channel| async move {
        // The command says how long to take: the second one finishes first.
        let wait: u64 = command.parse().unwrap();
        tokio::time::sleep(Duration::from_millis(wait)).await;
        channel.stdout(command).await;
        channel.exit(0).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let (slow, fast) = tokio::join!(connection.exec("300"), connection.exec("20"));
    assert_eq!(slow.unwrap().stdout_text(), "300");
    assert_eq!(fast.unwrap().stdout_text(), "20");
}

// ---- streams ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_yields_lines_however_the_packets_cut_them() {
    let server = serving(|command, channel| async move {
        assert_eq!(command, "follow");
        channel.stdout("one\ntw").await;
        channel.stdout("o\n\nthree\r\nfo").await;
        channel.stdout("ur").await;
        channel.exit(7).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream("follow").await.unwrap();
    let mut lines = Vec::new();
    while let Some(line) = stream.next_line().await {
        lines.push(line);
    }
    assert_eq!(lines, ["one", "two", "", "three\r", "four"]);
    assert_eq!(stream.status(), Some(ExecStatus::Code(7)));
    assert!(lock(&server.state.ptys).is_empty(), "no PTY for a stream either");
}

#[tokio::test]
async fn a_stream_survives_bytes_that_are_not_utf8() {
    let server = serving(|_, channel| async move {
        channel.stdout(b"ok \xff\xfe\n\xe4\xb8".to_vec()).await;
        channel.stdout(b"\xad done\n".to_vec()).await;
        channel.exit(0).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream("x").await.unwrap();
    assert_eq!(stream.next_line().await.unwrap(), "ok \u{fffd}\u{fffd}");
    assert_eq!(stream.next_line().await.unwrap(), "中 done");
    assert_eq!(stream.next_line().await, None);
}

#[tokio::test]
async fn a_line_that_is_too_long_is_cut_not_buffered() {
    let server = serving(|_, channel| async move {
        for _ in 0..100 {
            channel.stdout(vec![b'x'; 10_000]).await;
        }
        channel.stdout("\nshort\n").await;
        channel.exit(0).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream_with("x", StreamOptions { max_line: 100 }).await.unwrap();
    assert_eq!(stream.next_line().await.unwrap().len(), 100);
    assert_eq!(stream.next_line().await.unwrap(), "short");
}

#[tokio::test]
async fn the_end_of_stderr_is_kept_for_diagnostics() {
    let server = serving(|_, channel| async move {
        channel.stderr(vec![b'a'; 10_000]).await;
        channel.stderr("sh: broken at the end\n").await;
        channel.stdout("line\n").await;
        channel.exit(2).await;
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream("x").await.unwrap();
    assert_eq!(stream.next_line().await.unwrap(), "line", "stderr is not part of the lines");
    assert_eq!(stream.next_line().await, None);
    let tail = stream.stderr_tail();
    assert_eq!(tail.len(), 4096);
    assert!(tail.ends_with("sh: broken at the end\n"), "{tail:?}");
}

#[tokio::test]
async fn dropping_a_stream_closes_its_channel() {
    let closed = Arc::new(AtomicUsize::new(0));
    let server = serving({
        let closed = closed.clone();
        move |_, channel| {
            let closed = closed.clone();
            async move {
                // A collector loop: a line per tick until nobody listens.
                while channel.stdout("tick\n").await {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                closed.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream("loop").await.unwrap();
    assert_eq!(stream.next_line().await.unwrap(), "tick");
    assert_eq!(closed.load(Ordering::SeqCst), 0, "still going while somebody listens");
    drop(stream);
    eventually("the writer to see the closed channel", || (closed.load(Ordering::SeqCst) == 1).then_some(())).await;
    assert!(!connection.is_closed(), "only the channel went, not the connection");
}

#[tokio::test]
async fn a_stream_holds_the_connection_open_and_dropping_everything_closes_it() {
    let server = serving(|_, channel| async move {
        while channel.stdout("tick\n").await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream("loop").await.unwrap();
    assert_eq!(stream.next_line().await.unwrap(), "tick");

    // The caller lets go of the connection; the stream keeps it up.
    drop(connection);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*lock(&server.state.open_connections), 1, "the stream holds the connection");
    assert_eq!(stream.next_line().await.unwrap(), "tick", "and it still works");

    drop(stream);
    eventually("the connection to close", || (*lock(&server.state.open_connections) == 0).then_some(())).await;
}

#[tokio::test]
async fn a_slow_reader_does_not_make_the_stream_buffer_without_end() {
    // The writer is far ahead of the reader. The stream stops reading the channel when its queue
    // is full, so the writer has to wait (flow control) instead of the memory growing.
    let written = Arc::new(AtomicUsize::new(0));
    let server = serving({
        let written = written.clone();
        move |_, channel| {
            let written = written.clone();
            async move {
                for _ in 0..200_000 {
                    if !channel.stdout([vec![b'x'; 1000], b"\n".to_vec()].concat()).await {
                        return;
                    }
                    written.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    })
    .await;
    let connection = connect(&server, &Env::new()).await;
    let mut stream = connection.exec_stream("flood").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = written.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = written.load(Ordering::SeqCst);
    // The queue (256 lines), the channel buffers and the SSH window hold a few MB at the most.
    assert!(after < 20_000, "the writer ran on without a reader: {after} lines");
    assert!(after - before < 100, "it is stuck waiting, not creeping on: {before} -> {after}");
    assert_eq!(stream.next_line().await.unwrap().len(), 1000, "and everything is still there to read");
}
