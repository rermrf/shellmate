//! `-L`: a local port that leads to a target the server connects to.

use std::time::{Duration, Instant};

use shellmate_forward::{ForwardEvent, ForwardKind, RuleState, StatsSnapshot};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::rig::*;
use crate::support::{ServerOptions, eventually, lock};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_forward_carries_a_round_trip_and_counts_it() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    let started = rig.forwarder.start(rule.clone());
    let RuleState::Running { port } = started else { panic!("{started:?}") };
    assert_eq!(port, rule.listen.port);

    let message = b"hello through the tunnel";
    assert_eq!(echo_through(port, message).await, message);
    assert_eq!(rig.echo.accepted(), 1);
    assert_eq!(
        lock(&rig.server.state.forwards).as_slice(),
        [("127.0.0.1".to_string(), u32::from(rig.echo.port))],
        "the server was asked to connect to the target"
    );
    eventually("the connection to be done", || (rig.forwarder.stats(&rule.id).active == 0).then_some(())).await;
    let stats = rig.forwarder.stats(&rule.id);
    assert_eq!(stats, StatsSnapshot { active: 0, down: message.len() as u64, up: message.len() as u64 });
}

/// Both directions of many connections at once. Two russh sessions talking to each other (the
/// client and the in-process server) can lock each other up when more than a few hundred KB are
/// in flight in both directions, which is why the payloads here are small; the bulk tests below go
/// one way at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_connections_at_once_do_not_mix() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    let port = rule.listen.port;
    assert!(rig.forwarder.start(rule.clone()).is_running());

    let clients: Vec<_> = (0..24u64)
        .map(|n| {
            tokio::spawn(async move {
                let data = pattern(20_000 + n as usize * 100, n + 1);
                let back = echo_through(port, &data).await;
                (data.len(), checksum(&data), back.len(), checksum(&back))
            })
        })
        .collect();
    let mut total = 0u64;
    for client in clients {
        let (sent, sent_sum, got, got_sum) = client.await.unwrap();
        assert_eq!((got, got_sum), (sent, sent_sum));
        total += sent as u64;
    }
    assert_eq!(rig.echo.accepted(), 24);
    eventually("all connections to finish", || (rig.forwarder.stats(&rule.id).active == 0).then_some(())).await;
    let stats = rig.forwarder.stats(&rule.id);
    assert_eq!((stats.up, stats.down), (total, total));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_large_uploads_at_once_keep_their_own_checksums() {
    let rig = Rig::start().await;
    let digest = Digest::start().await;
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{}", digest.port));
    let port = rule.listen.port;
    assert!(rig.forwarder.start(rule.clone()).is_running());

    const SIZE: usize = 2 * 1024 * 1024;
    let uploads: Vec<_> = (0..12u64).map(|n| tokio::spawn(upload(port, SIZE + n as usize * 777, 900 + n))).collect();
    for (n, upload) in uploads.into_iter().enumerate() {
        let ((length, received_sum), sent_sum) = upload.await.unwrap();
        assert_eq!(length, (SIZE + n * 777) as u64);
        assert_eq!(received_sum, sent_sum, "upload {n}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_upload_arrives_intact() {
    let rig = Rig::start().await;
    let digest = Digest::start().await;
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{}", digest.port));
    assert!(rig.forwarder.start(rule.clone()).is_running());

    const SIZE: usize = 32 * 1024 * 1024;
    let ((length, received_sum), sent_sum) = upload(rule.listen.port, SIZE, 0x5eed).await;
    assert_eq!(length, SIZE as u64);
    assert_eq!(received_sum, sent_sum, "every byte arrived, in order");
    eventually("the connection to be done", || (rig.forwarder.stats(&rule.id).active == 0).then_some(())).await;
    let stats = rig.forwarder.stats(&rule.id);
    assert_eq!(stats.up, SIZE as u64);
    assert_eq!(stats.down, 16);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_download_arrives_intact() {
    let rig = Rig::start().await;
    const SIZE: usize = 32 * 1024 * 1024;
    let source = Source::start(SIZE, 77).await;
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &format!("127.0.0.1:{}", source.port));
    assert!(rig.forwarder.start(rule.clone()).is_running());

    let (length, sum) = download(rule.listen.port).await;
    assert_eq!(length, SIZE as u64);
    assert_eq!(sum, Source::expected(SIZE, 77));
    eventually("the connection to be done", || (rig.forwarder.stats(&rule.id).active == 0).then_some(())).await;
    assert_eq!(rig.forwarder.stats(&rule.id).down, SIZE as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_the_server_cannot_reach_closes_the_connection_and_the_rule_carries_on() {
    let rig = Rig::start().await;
    let dead = free_port();
    let broken = rule_to(&rig, &format!("127.0.0.1:{dead}"));
    let good = rig.local_rule();
    assert!(rig.forwarder.start(broken.clone()).is_running());
    assert!(rig.forwarder.start(good.clone()).is_running());

    let mut client = dial(broken.listen.port).await;
    expect_closed(&mut client).await;
    expect_closed(&mut dial(broken.listen.port).await).await;
    assert!(rig.forwarder.state(&broken.id).is_running(), "a failed connection does not stop the rule");
    assert_eq!(echo_through(good.listen.port, b"unaffected").await, b"unaffected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_forbids_forwarding_closes_the_connections() {
    let rig = Rig::with(ServerOptions { forwarding: false, ..Default::default() }).await;
    let rule = rig.local_rule();
    assert!(rig.forwarder.start(rule.clone()).is_running());
    expect_closed(&mut dial(rule.listen.port).await).await;
    assert_eq!(rig.echo.accepted(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_that_never_answers_is_given_up_on() {
    let rig =
        Rig::with_watchdog(ServerOptions { stall_direct_tcpip: true, ..forwarding() }, Duration::from_millis(400))
            .await;
    let rule = rig.local_rule();
    assert!(rig.forwarder.start(rule.clone()).is_running());
    let started = Instant::now();
    expect_closed(&mut dial(rule.listen.port).await).await;
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_rule_closes_its_port_and_ends_its_connections() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    assert!(rig.forwarder.start(rule.clone()).is_running());
    let mut open = dial(rule.listen.port).await;
    open.write_all(b"x").await.unwrap();
    let mut one = [0u8; 1];
    open.read_exact(&mut one).await.unwrap();
    assert_eq!(rig.forwarder.stats(&rule.id).active, 1);

    assert!(rig.forwarder.stop(&rule.id));
    assert!(!rig.forwarder.stop(&rule.id), "already stopped");
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::Stopped);
    expect_closed(&mut open).await;
    eventually("the port to be released", || {
        std::net::TcpStream::connect(("127.0.0.1", rule.listen.port)).is_err().then_some(())
    })
    .await;
    assert!(matches!(rig.events.try_recv(), Ok(ForwardEvent::State { state: RuleState::Running { .. }, .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarting_a_rule_on_the_same_port_works_right_away() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    for _ in 0..5 {
        assert!(rig.forwarder.start(rule.clone()).is_running(), "start replaces the running rule");
        assert_eq!(echo_through(rule.listen.port, b"again").await, b"again");
        assert!(rig.forwarder.stop(&rule.id));
        assert!(rig.forwarder.start(rule.clone()).is_running(), "stop then start");
    }
    assert_eq!(rig.forwarder.views().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statistics_are_reported_at_most_once_a_second() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    assert!(rig.forwarder.start(rule.clone()).is_running());
    let _ = drain(&rig.events);

    let started = Instant::now();
    let mut client = dial(rule.listen.port).await;
    let mut buf = [0u8; 1024];
    let mut updates = 0usize;
    let mut last = StatsSnapshot::default();
    while started.elapsed() < Duration::from_millis(2_600) {
        client.write_all(&[7u8; 1024]).await.unwrap();
        client.read_exact(&mut buf).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        for event in drain(&rig.events) {
            if let ForwardEvent::Stats { stats, .. } = event {
                updates += 1;
                assert!(stats.down >= last.down && stats.up >= last.up);
                last = stats;
            }
        }
    }
    assert!(updates >= 1, "numbers are reported");
    assert!(updates <= 3, "{updates} updates in 2.6 s");
    assert_eq!(last.active, 1);
}

fn rule_to(rig: &Rig, target: &str) -> shellmate_forward::ForwardRule {
    let _ = rig;
    rule(shellmate_forward::ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), target)
}
