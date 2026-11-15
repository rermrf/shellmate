//! Per-channel flow control: one forwarded connection whose local peer stops reading must only
//! throttle its own channel. Everything else on the SSH connection — other forwards, terminals —
//! keeps moving, and the stalled peer gets every byte once it reads again.

use std::time::{Duration, Instant};

use shellmate_core::{PtyDims, TerminalBackend};
use shellmate_forward::ForwardKind;
use shellmate_ssh::Connection;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

use crate::rig::*;
use crate::support::{Recorder, ServerOptions};

const DIMS: PtyDims = PtyDims { cols: 100, rows: 30, pixel_width: 0, pixel_height: 0 };

/// Opens a shell on `connection` and checks that it comes up and echoes a line within `within`.
/// Fails (rather than hangs) when the connection does not answer.
pub async fn shell_answers(connection: &Connection, within: Duration) -> Duration {
    let (recorder, sink) = Recorder::sink();
    let pending = timeout(Duration::from_secs(5), connection.open_shell(DIMS))
        .await
        .expect("the connection is frozen: a shell could not be opened")
        .expect("open shell");
    let mut shell = pending.start(sink, DIMS);
    timeout(Duration::from_secs(5), recorder.wait_for_text("fixture ready"))
        .await
        .expect("the connection is frozen: the shell said nothing");
    let started = Instant::now();
    shell.write(b"ping-while-stalled").unwrap();
    timeout(within, recorder.wait_for_text("ping-while-stalled"))
        .await
        .unwrap_or_else(|_| panic!("the connection is frozen: the shell did not answer within {within:?}"));
    let took = started.elapsed();
    shell.close();
    took
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_stops_reading_throttles_only_its_own_channel() {
    const SIZE: usize = 64 * 1024 * 1024;
    const SEED: u64 = 5;
    const STALL: Duration = Duration::from_secs(5);

    let rig = Rig::start().await;
    // The old guard that cut such a peer off after a while is not what is being tested.
    rig.forwarder.set_stall_limit(Duration::from_secs(3600));
    let source = Source::start(SIZE, SEED).await;
    let digest = Digest::start().await;
    let firehose =
        rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{}", source.port));
    let other = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{}", digest.port));
    assert!(rig.forwarder.start(firehose.clone()).is_running());
    assert!(rig.forwarder.start(other.clone()).is_running());

    // The peer takes the first bytes and then stops reading.
    let mut stalled = dial(firehose.listen.port).await;
    let mut head = [0u8; 16];
    stalled.read_exact(&mut head).await.unwrap();
    let stall_began = Instant::now();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // (1a) Another forward on the same connection still moves several MB.
    let upload = timeout(Duration::from_secs(10), upload(other.listen.port, 6 * 1024 * 1024, 77))
        .await
        .expect("the connection is frozen: the second forward moved no data while the first was stalled");
    let ((length, received_sum), sent_sum) = upload;
    assert_eq!((length, received_sum), (6 * 1024 * 1024, sent_sum));

    // (1b) A terminal on the same connection answers within a second.
    let took = shell_answers(&rig.connection, Duration::from_secs(1)).await;
    eprintln!("a shell answered {took:?} into a stall of the first forward");

    // The stall lasts at least five seconds in all.
    tokio::time::sleep(STALL.saturating_sub(stall_began.elapsed())).await;

    // (3) The peer is throttled, not buffered without limit: what the firehose got rid of in the
    // meantime is bounded by the windows and queues in between, far below the 64 MiB on offer.
    let written = source.written();
    eprintln!("the firehose handed {} KiB to its socket in {:?} of stall", written / 1024, stall_began.elapsed());
    assert!(written < 24 * 1024 * 1024, "{written} bytes were taken from a source whose reader does not read");
    let settled = source.written();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(source.written(), settled, "nothing more flows while the peer does not read");

    // (2) The peer reads again and gets every byte.
    let mut sum = Checksum::new();
    sum.update(&head);
    let mut length = head.len();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = timeout(Duration::from_secs(30), stalled.read(&mut buf)).await.expect("progress").unwrap();
        if n == 0 {
            break;
        }
        sum.update(&buf[..n]);
        length += n;
    }
    assert_eq!(length, SIZE);
    assert_eq!(sum.finish(), Source::expected(SIZE, SEED), "every byte arrived intact and in order");
    // The connection ends when the peer is done as well.
    drop(stalled);
    eventually_idle(&rig, &firehose.id).await;
}

async fn eventually_idle(rig: &Rig, id: &shellmate_forward::RuleId) {
    crate::support::eventually("the connection to be done", || (rig.forwarder.stats(id).active == 0).then_some(()))
        .await;
}

/// Two russh sessions that talk to each other (the client and the in-process server) used to lock
/// each other up now and then when a lot of data went both ways at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_way_echo_stress_does_not_lock_up_the_connection() {
    for round in 0..50 {
        let rig = Rig::with(ServerOptions { forwarding: true, ..Default::default() }).await;
        let rule = rig.local_rule();
        let port = rule.listen.port;
        assert!(rig.forwarder.start(rule.clone()).is_running());
        let clients: Vec<_> = (0..24u64)
            .map(|n| {
                tokio::spawn(async move {
                    let data = pattern(150_000 + n as usize * 1_000, n + 1);
                    let back = echo_through(port, &data).await;
                    (checksum(&data), checksum(&back), data.len(), back.len())
                })
            })
            .collect();
        for client in clients {
            let (sent, got, sent_len, got_len) = timeout(Duration::from_secs(15), client)
                .await
                .unwrap_or_else(|_| panic!("round {round}: the connection locked up"))
                .unwrap();
            assert_eq!((sent_len, sent), (got_len, got), "round {round}");
        }
    }
}
