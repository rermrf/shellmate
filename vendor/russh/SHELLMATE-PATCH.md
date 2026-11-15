# russh 0.65.1 with per-channel flow control (Shellmate patch)

This directory is [russh](https://github.com/Eugeny/russh) **0.65.1** (crates.io release, git
`b33092aad1e51c66258a12e8b83fc473a68d9d34`, path `russh/`), licensed under Apache-2.0 (see `LICENSE`),
patched by Shellmate. The workspace uses it through `[patch.crates-io] russh = { path = "vendor/russh" }`
and excludes it from the workspace (`exclude = ["vendor/russh"]`), so our clippy and tests do not
lint or run it. Every changed file starts with a "Modified for Shellmate" line.

The pristine 0.65.1 is commit `bb32c67` of this repository (`git diff bb32c67 HEAD -- vendor/russh`
shows exactly what we changed).

## Why

russh's session loop handles every incoming packet one after the other. For channel data it

1. re-opens the channel's receive window **when the data arrives** (`adjust_window_size`), and
2. then awaits room in the channel's bounded queue (`chan.send(..).await`).

So the peer is never throttled, and as soon as one consumer stops reading a channel (a browser tab
behind a SOCKS proxy that paused a download, a local program that does not read its socket, a
`Channel` that nobody polls) the queue fills up and the **whole SSH connection** stops: every other
channel, terminals and file transfers included, and even the keepalive replies. OpenSSH does not
behave like this, it only re-opens a channel's window when the data has been taken.

The patch is a port of upstream pull request
[Eugeny/russh#730](https://github.com/Eugeny/russh/pull/730) ("gate WINDOW_ADJUST on consumption to
fix per-channel HOL blocking"), which the maintainer closed unmerged ("the application must always
drain"). For a port forwarder, always draining means unbounded memory, so we carry the patch.

What the patch does, client and server side alike:

- Messages for a channel are delivered with `try_send`; the session loop never awaits a channel.
  What does not fit is kept in a per-channel backlog (`ChannelBacklog`, `src/channels/mod.rs`),
  in order, so later messages (`Eof`, `Close`, exit status, requests...) cannot overtake data.
- Receiving data only *debits* the receive window (`Encrypted::record_received_data`). The window
  is re-opened (`Encrypted::replenish_receive_window`) only when the channel has no backlog, i.e.
  once the application has taken the data. The peer therefore runs out of window and stops sending
  on this channel, while the loop stays free for everything else.
- `ChannelReadHalf` tells the session (`drain_notify`) whenever it takes a message out of the
  queue; the loop then moves the backlog into the freed room and re-opens the window.
  Dropping the read half closes the queue, so consumers that only use the `Handler::data`
  callbacks (and drop their `Channel`) keep their window open.
- `Close` / `OpenFailure` leave the channel registered until its backlog has been delivered.
- `ChannelRx` (the `AsyncRead` of a channel) remembers the end of the data instead of closing the
  receiver, so what is queued behind `Eof` (exit status, `Close`) can still be read.

Differences from the upstream PR: the Data and ExtendedData arms of the client are kept apart
(smaller diff against 0.65.1), `record_received_data` keeps 0.65.1's "ignore data beyond the
window" rule, and `WindowAdjusted` notifications keep their direct `try_send` (they are
informational only).

Behaviour to know about: a server `Handler` that consumes channel data **only** through
`Handler::data` must drop the `Channel` it is handed in `channel_open_*` (documented on
`server::Handler::data`); a retained but unread `Channel` throttles the peer once its queue is full.

## Related upstream issues

- [#730](https://github.com/Eugeny/russh/pull/730): the port above.
- [#790](https://github.com/Eugeny/russh/pull/790) (window accounting): already contained in 0.65.1,
  nothing to do.
- [#806](https://github.com/Eugeny/russh/issues/806) (the session stops sending when a peer closes a
  channel with data still pending): **not patched**. It concerns `Handle::data`, which Shellmate
  never uses (it writes through `Channel` / `ChannelStream`).

## What changed

- `src/channels/mod.rs`: `ChannelBacklog`; `ChannelReadHalf` gets `drain_notify`, `new`,
  `notify_drained`, a `Drop`, and `wait` notifies; `Channel::new` takes the `drain_notify`;
  unit tests of the backlog.
- `src/channels/io/rx.rs`: notify after each message taken, EOF latch.
- `src/session.rs`: `adjust_window_size` is split into `record_received_data` (debit on receipt)
  and `replenish_receive_window`.
- `src/client/mod.rs`, `src/client/encrypted.rs`: the backlog and `drain_notify` in `Session` /
  `Handle`, the drain arm in the session loop, all `chan.send(..).await` replaced by
  `forward_channel_msg` / `close_channel`, window re-opened after delivery (`reopen_window`).
- `src/server/mod.rs`, `src/server/session.rs`, `src/server/encrypted.rs`: the same for the server
  side, and the doc note on `Handler::data`.
- `Cargo.toml`: one `[[test]]` entry; `tests/test_per_channel_flow_control.rs`: new integration
  tests (a parked consumer on either side must not block a sibling channel; the flood is bounded;
  everything arrives once the consumer reads again). They fail on the pristine 0.65.1.
- `LICENSE` (Apache-2.0 text from the upstream repository) and this file.

## Memory bound per channel

Unread channel data is bounded by roughly

    window_size + channel_buffer_size * maximum_packet_size

at the defaults 2 MiB + 100 * 32 KiB = about 5.2 MiB per channel and direction: the queue holds up
to `channel_buffer_size` messages of at most `maximum_packet_size` bytes, and the peer may send one
more window (`window_size`) before it has to wait for the backlog to be taken. A peer that ignores
flow control can grow the backlog without limit, like it can consume any other resource of a
session it is authenticated for.

## Re-porting on a russh upgrade

1. First check whether upstream fixed the problem (look at #730 and the `ChannelBacklog`-like
   handling in `client/encrypted.rs` of the new version). If it did, delete `vendor/russh`, the
   `[patch.crates-io]` entry and the `exclude`.
2. Otherwise: copy the new release over `vendor/russh` in one commit (pristine, like `bb32c67`),
   then re-apply our diff: `git diff bb32c67 <the commit before the upgrade> -- vendor/russh/src
   vendor/russh/tests vendor/russh/Cargo.toml` and resolve the conflicts. The places to look at are
   the `chan.send(..).await` / `channels.remove` / `adjust_window_size` call sites in
   `client/encrypted.rs`, `server/encrypted.rs` and the `Session` constructors.
3. Check: `cargo test --manifest-path vendor/russh/Cargo.toml --target-dir <the workspace target>
   --lib --tests` (all tests, including `test_per_channel_flow_control`), then in the workspace
   `cargo test -p shellmate-forward --test forward flow` and the `#[ignore]` e2e test
   `e2e_a_reader_that_stalls_for_20_seconds_holds_up_nothing_else`.
