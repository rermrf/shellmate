//! Running one item: walking the selection, then transferring entry after entry.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesOrdered, FuturesUnordered, StreamExt as _};
use russh_sftp::client::error::Error as RawError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

use super::control::{Control, Interrupt};
use super::names::{download_temp_name, upload_temp_path};
use super::plan::{Plan, PlanEntry, Top, Walked, walk_local, walk_remote};
use super::queue::{Partial, Reporter};
use super::{Choice, Clash, Direction, PauseReason, TransferFailure, failure_summary};
use crate::entry::EntryKind;
use crate::error::SftpError;
use crate::path;
use crate::session::SftpSession;

/// What a task needs to know to run its item.
pub(super) struct RunInput {
    pub direction: Direction,
    /// The selection of an upload.
    pub local: Vec<PathBuf>,
    /// The selection of a download.
    pub remote: Vec<String>,
    /// The destination directory (remote for uploads, local for downloads).
    pub destination: String,
    pub choice: Option<Choice>,
    pub plan: Option<Arc<Plan>>,
    pub cursor: usize,
    pub partial: Option<Partial>,
}

/// How a task ended.
pub(super) enum RunOutcome {
    /// Every entry was dealt with (failures, if any, are in the item).
    Done,
    /// Stopped on request.
    Interrupted(Interrupt),
    /// The connection is gone.
    Disconnected,
    /// The item cannot go on at all.
    Failed(String),
    /// Names exist at the destination: the user has to say what to do.
    Clash(Clash),
}

/// How one entry ended.
enum End {
    Done,
    Interrupted(Interrupt),
}

/// How long in-flight writes get to finish when the user pauses an upload: half the time a request
/// may take (15 seconds by default), so that a session that does not answer is given up on before
/// its requests time out one by one.
fn drain_timeout(session: &SftpSession) -> Duration {
    (session.request_timeout() / 2).max(Duration::from_millis(250))
}

/// How long cleaning up after a stop (closing a handle, removing a temporary file) may take: on a
/// session that does not answer, it must not hold the stop up.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Runs `cleanup`, but not for longer than [`CLEANUP_TIMEOUT`], and ignores how it went.
async fn quietly<T>(cleanup: impl std::future::Future<Output = T>) {
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, cleanup).await;
}

/// Whether an error means the session is gone (or not answering), so that the item has to wait for
/// a new one instead of failing the file: the error says so, or the queue has already taken the
/// session away (whatever comes back from a session being torn down is not the file's fault).
fn session_gone(err: &SftpError, control: &Control) -> bool {
    err.is_session_lost() || control.interrupted() == Some(Interrupt::Pause(PauseReason::Disconnected))
}

// ---- the whole item ---------------------------------------------------------------------------

/// Runs the item: plans it if it has no plan yet, then transfers from the cursor on.
pub(super) async fn run(reporter: &Reporter, session: &SftpSession, control: &Control) -> RunOutcome {
    let Some(input) = reporter.input() else { return RunOutcome::Interrupted(Interrupt::Cancel) };
    let plan = match input.plan.clone() {
        Some(plan) => plan,
        None => match make_plan(reporter, session, &input).await {
            Ok(plan) => plan,
            Err(outcome) => return outcome,
        },
    };
    execute(reporter, session, control, &input, &plan).await
}

async fn execute(
    reporter: &Reporter,
    session: &SftpSession,
    control: &Control,
    input: &RunInput,
    plan: &Plan,
) -> RunOutcome {
    // A directory that could not be created: what is below it is not attempted.
    let mut broken: Option<(usize, Vec<String>)> = None;
    for index in input.cursor..plan.entries.len() {
        if let Some(interrupt) = control.interrupted() {
            return RunOutcome::Interrupted(interrupt);
        }
        let entry = &plan.entries[index];
        if let Some((top, prefix)) = &broken
            && entry.top == *top
            && entry.rel.starts_with(prefix)
        {
            reporter.entry_skipped(index, entry.size);
            continue;
        }
        reporter.begin_entry(index, plan.display_name(entry));
        let result = match (input.direction, entry.dir) {
            (Direction::Upload, true) => make_remote_dir(session, input, plan, entry).await,
            (Direction::Upload, false) => upload_file(reporter, session, control, input, plan, index, entry).await,
            (Direction::Download, true) => make_local_dir(plan, entry).await,
            (Direction::Download, false) => download_file(reporter, session, control, input, plan, index, entry).await,
        };
        match result {
            Ok(End::Done) => {
                reporter.entry_done(index, entry.size, !entry.dir);
                // A downloaded directory ends up where it was planned; a file reports its own path
                // (the name may have changed at the last moment).
                if input.direction == Direction::Download
                    && entry.dir
                    && entry.rel.is_empty()
                    && let Some(dest) = &plan.tops[entry.top].dest
                {
                    reporter.set_result(dest.clone());
                }
            }
            Ok(End::Interrupted(interrupt)) => return RunOutcome::Interrupted(interrupt),
            // The session is gone: wait for a new one, the file did nothing wrong.
            Err(err) if session_gone(&err, control) => return RunOutcome::Disconnected,
            Err(err) => {
                let top = &plan.tops[entry.top];
                let shown = source_path(input.direction, top, entry);
                reporter.entry_failed(index, TransferFailure { path: shown, reason: err.to_string() }, entry.size);
                if entry.dir {
                    broken = Some((entry.top, entry.rel.clone()));
                }
            }
        }
    }
    // The item is over; the queue turns failures, if there were any, into its final state.
    RunOutcome::Done
}

/// The path of an entry's source as the user knows it.
fn source_path(direction: Direction, top: &Top, entry: &PlanEntry) -> String {
    match direction {
        Direction::Upload => {
            entry.local.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| top.name.clone())
        }
        Direction::Download => remote_source(top, entry),
    }
}

fn remote_source(top: &Top, entry: &PlanEntry) -> String {
    let base = top.remote.clone().unwrap_or_default();
    if entry.rel.is_empty() { base } else { path::join(&base, &entry.rel_path()) }
}

// ---- planning -----------------------------------------------------------------------------------

async fn make_plan(reporter: &Reporter, session: &SftpSession, input: &RunInput) -> Result<Arc<Plan>, RunOutcome> {
    reporter.set_scanning(true);
    let (plan, skipped, failures) = match input.direction {
        Direction::Upload => {
            let selection = input.local.clone();
            let walked = tokio::task::spawn_blocking(move || walk_local(&selection))
                .await
                .map_err(|err| RunOutcome::Failed(format!("读取本地文件失败：{err}")))?;
            plan_upload(session, input, walked).await?
        }
        Direction::Download => {
            let walked = walk_remote(session, &input.remote).await;
            if walked.disconnected {
                return Err(RunOutcome::Disconnected);
            }
            plan_download(reporter, input, walked)?
        }
    };
    if plan.entries.is_empty() && !failures.is_empty() {
        reporter.set_plan(Arc::new(plan), skipped, failures.clone());
        return Err(RunOutcome::Failed(failure_summary(&failures, failures.len())));
    }
    let plan = Arc::new(plan);
    reporter.set_plan(plan.clone(), skipped, failures);
    Ok(plan)
}

/// Settles what to do about names that exist at the destination of an upload.
async fn plan_upload(
    session: &SftpSession,
    input: &RunInput,
    walked: Walked,
) -> Result<(Plan, super::Skipped, Vec<TransferFailure>), RunOutcome> {
    let Walked { tops, entries, mut skipped, failures, .. } = walked;
    let mut plan = Plan::new(tops, entries);
    let dir = &input.destination;
    let mut exists = Vec::with_capacity(plan.tops.len());
    for top in &plan.tops {
        exists.push(remote_exists(session, &path::join(dir, &top.name)).await.map_err(outcome_of)?);
    }
    let names: Vec<String> = plan.tops.iter().zip(&exists).filter(|(_, e)| **e).map(|(t, _)| t.name.clone()).collect();
    match input.choice {
        None if !names.is_empty() => {
            return Err(RunOutcome::Clash(Clash {
                dir: dir.clone(),
                count: names.len(),
                names: names.into_iter().take(5).collect(),
            }));
        }
        Some(Choice::Skip) => {
            skipped.existing += names.len() as u32;
            let mut index = 0;
            plan.retain_tops(|_, _| {
                index += 1;
                !exists[index - 1]
            });
        }
        Some(Choice::KeepBoth) => {
            let mut used: std::collections::HashSet<String> =
                plan.tops.iter().zip(&exists).filter(|(_, e)| !**e).map(|(t, _)| t.name.clone()).collect();
            for (top, existing) in plan.tops.iter_mut().zip(&exists) {
                if !*existing {
                    continue;
                }
                let mut chosen = None;
                for candidate in super::candidates(&top.name, !top.dir) {
                    if used.contains(&candidate)
                        || remote_exists(session, &path::join(dir, &candidate)).await.map_err(outcome_of)?
                    {
                        continue;
                    }
                    chosen = Some(candidate);
                    break;
                }
                let Some(name) = chosen else {
                    return Err(RunOutcome::Failed(format!("找不到可用的名称：{}", top.name)));
                };
                used.insert(name.clone());
                top.name = name;
            }
        }
        Some(Choice::Overwrite) | None => {}
    }
    Ok((plan, skipped, failures))
}

async fn remote_exists(session: &SftpSession, path: &str) -> Result<bool, SftpError> {
    match session.lstat_plain(path).await {
        Ok(_) => Ok(true),
        Err(SftpError::NotFound { .. }) => Ok(false),
        Err(err) => Err(err),
    }
}

fn outcome_of(err: SftpError) -> RunOutcome {
    if err.is_disconnected() { RunOutcome::Disconnected } else { RunOutcome::Failed(err.to_string()) }
}

/// Which of the top-level `names` exist in `dir` on the server.
pub(super) async fn existing_names(
    session: &SftpSession,
    dir: &str,
    names: &[String],
) -> Result<Vec<String>, SftpError> {
    let mut found = Vec::new();
    for name in names {
        if remote_exists(session, &path::join(dir, name)).await? {
            found.push(name.clone());
        }
    }
    Ok(found)
}

/// Picks the local names a download will create.
fn plan_download(
    reporter: &Reporter,
    input: &RunInput,
    walked: Walked,
) -> Result<(Plan, super::Skipped, Vec<TransferFailure>), RunOutcome> {
    let Walked { tops, entries, skipped, failures, .. } = walked;
    let mut plan = Plan::new(tops, entries);
    let dir = Path::new(&input.destination);
    for top in &mut plan.tops {
        let dest = reporter
            .reserve_local(dir, &top.name, !top.dir)
            .ok_or_else(|| RunOutcome::Failed(format!("找不到可用的本地名称：{}", top.name)))?;
        top.dest = Some(dest);
    }
    Ok((plan, skipped, failures))
}

// ---- directories ----------------------------------------------------------------------------------

/// The path of an entry on the server for an upload.
fn remote_target(input: &RunInput, top: &Top, entry: &PlanEntry) -> String {
    let root = path::join(&input.destination, &top.name);
    if entry.rel.is_empty() { root } else { path::join(&root, &entry.rel_path()) }
}

async fn make_remote_dir(
    session: &SftpSession,
    input: &RunInput,
    plan: &Plan,
    entry: &PlanEntry,
) -> Result<End, SftpError> {
    let target = remote_target(input, &plan.tops[entry.top], entry);
    match session.lstat_plain(&target).await {
        Ok(existing) if existing.kind == EntryKind::Dir => {}
        Ok(existing)
            if existing.kind == EntryKind::Symlink && session.stat(&target).await.is_ok_and(|e| e.is_dir()) => {}
        Ok(_) => {
            return Err(SftpError::Failed {
                path: target, message: "远端已有同名文件，无法创建目录".into()
            });
        }
        Err(SftpError::NotFound { .. }) => session.mkdir_with_mode(&target, entry.mode).await?,
        Err(err) => return Err(err),
    }
    Ok(End::Done)
}

/// The path of an entry on this machine for a download.
fn local_target(plan: &Plan, entry: &PlanEntry) -> PathBuf {
    let mut target = plan.tops[entry.top].dest.clone().unwrap_or_default();
    for name in &entry.rel {
        target.push(name);
    }
    target
}

async fn make_local_dir(plan: &Plan, entry: &PlanEntry) -> Result<End, SftpError> {
    let target = local_target(plan, entry);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(existing) if existing.is_dir() => {}
        Ok(_) => return Err(SftpError::AlreadyExists { path: target.display().to_string() }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Like sftp's `get -r`: the source's permissions, but always usable by us.
            tokio::fs::DirBuilder::new()
                .mode(entry.mode | 0o700)
                .create(&target)
                .await
                .map_err(|err| local_error(&target, err))?;
        }
        Err(err) => return Err(local_error(&target, err)),
    }
    Ok(End::Done)
}

/// An I/O error on this machine, in the words of the SFTP errors.
fn local_error(path: &Path, err: io::Error) -> SftpError {
    let shown = path.display().to_string();
    match err.kind() {
        io::ErrorKind::PermissionDenied => SftpError::PermissionDenied { path: shown },
        io::ErrorKind::NotFound => SftpError::NotFound { path: shown },
        io::ErrorKind::AlreadyExists => SftpError::AlreadyExists { path: shown },
        _ => SftpError::Failed { path: shown, message: err.to_string() },
    }
}

// ---- uploading a file -----------------------------------------------------------------------------

/// A time for `setstat`: seconds, clamped into 32 bits.
fn sftp_time(time: Option<i64>) -> Option<u32> {
    time.map(|secs| secs.clamp(0, i64::from(u32::MAX)) as u32)
}

async fn upload_file(
    reporter: &Reporter,
    session: &SftpSession,
    control: &Control,
    input: &RunInput,
    plan: &Plan,
    index: usize,
    entry: &PlanEntry,
) -> Result<End, SftpError> {
    let top = &plan.tops[entry.top];
    let dest = remote_target(input, top, entry);
    let local = entry.local.clone().unwrap_or_default();

    // The source as it is now.
    let metadata = tokio::fs::metadata(&local).await.map_err(|err| local_error(&local, err))?;
    if !metadata.is_file() {
        return Err(SftpError::NotAFile { path: local.display().to_string() });
    }
    let (size, mtime) = (metadata.len(), Some(metadata.mtime()));
    if size != entry.size {
        reporter.size_found(index, entry.size, size);
    }

    // What is at the destination: a link is written through (a dangling one at the place it
    // points to), a file keeps its permissions, a directory is in the way.
    let (target, existing) = session.resolve_for_write(&dest).await?;
    let mode = match &existing {
        Some(found) => match found.kind {
            EntryKind::Dir => return Err(SftpError::IsADirectory { path: dest }),
            EntryKind::File => {
                // Replacing a file by renaming needs no permission on the file itself: ask.
                session.ensure_writable(&target).await?;
                found.mode.unwrap_or(entry.mode)
            }
            _ => return Err(SftpError::NotAFile { path: dest }),
        },
        None => entry.mode & 0o777,
    };

    let temp = upload_temp_path(&target);
    let resumed =
        resume_upload(session, input.partial.as_ref().filter(|p| p.entry == index), &temp, size, mtime).await?;
    // Where the data goes: a temporary file next to the target (resumable), or, when the directory
    // does not allow one, the existing file itself (not atomic, and not resumable: a stop leaves it
    // partly written, and a resume starts over).
    let (handle, start, in_place) = match resumed {
        Some((handle, from)) => (handle, from, false),
        None => {
            let attrs = FileAttributes { permissions: Some(mode & 0o7777), ..FileAttributes::empty() };
            match session.create_exclusive(&temp, attrs).await {
                Ok(handle) => (handle, 0, false),
                Err(SftpError::PermissionDenied { .. }) if existing.is_some() => {
                    let flags = OpenFlags::WRITE | OpenFlags::TRUNCATE;
                    (session.open_handle(&target, flags, FileAttributes::empty()).await?, 0, true)
                }
                Err(err) => return Err(err),
            }
        }
    };
    let written_to = if in_place { target.clone() } else { temp.clone() };
    if in_place {
        reporter.note_in_place();
    }
    reporter.set_partial((!in_place).then(|| Partial {
        entry: index,
        temp_remote: Some(temp.clone()),
        temp_local: None,
        src_size: size,
        src_mtime: mtime,
        highwater: start,
    }));
    reporter.set_offset(start);

    match push(session, &handle, &local, start, size, control, &written_to, reporter).await {
        Ok(End::Done) => {}
        Ok(End::Interrupted(interrupt)) => {
            quietly(session.close_handle(&handle, &written_to)).await;
            if interrupt == Interrupt::Cancel && !in_place {
                quietly(session.remove_file(&temp)).await;
                reporter.set_partial(None);
            }
            return Ok(End::Interrupted(interrupt));
        }
        Err(err) => {
            quietly(session.close_handle(&handle, &written_to)).await;
            // A lost session keeps the temporary file for the resume; any other failure removes it.
            if !session_gone(&err, control) && !in_place {
                quietly(session.remove_file(&temp)).await;
                reporter.set_partial(None);
            }
            return Err(err);
        }
    }

    let finished = async {
        // Best effort: only root can give a file to somebody else. (The editor falls back to
        // writing in place when this fails; an upload does not, so that it stays atomic and resumable.)
        session.try_set_handle_owner(&handle, existing.as_ref()).await;
        let time = sftp_time(mtime);
        let attrs =
            FileAttributes { permissions: Some(mode & 0o7777), atime: time, mtime: time, ..FileAttributes::empty() };
        session.set_handle_attrs(&handle, attrs, &written_to).await?;
        // No fsync: it costs a disk flush per file (twice the time for a directory of small files)
        // and, like scp and sftp, an upload that the server loses in a crash is simply repeated.
        session.close_handle(&handle, &written_to).await
    }
    .await;
    let replaced = match finished {
        Ok(()) if in_place => Ok(()),
        Ok(()) => session.replace_file(&temp, &target).await.map_err(|err| err.at(&dest)),
        Err(err) => {
            quietly(session.close_handle(&handle, &written_to)).await;
            Err(err)
        }
    };
    if let Err(err) = replaced {
        if !session_gone(&err, control) && !in_place {
            quietly(session.remove_file(&temp)).await;
            reporter.set_partial(None);
        }
        return Err(err);
    }
    Ok(End::Done)
}

/// Opens the temporary file of an interrupted upload to continue it, if it is safe to: the
/// source is as it was, and the temporary file is still a regular file no longer than the source.
/// Returns the handle and where to go on from: the smaller of the temporary file's length and the
/// part known to be written without gaps (the server may have applied requests out of order, so
/// the length alone can have holes below it). What is above is simply written again.
async fn resume_upload(
    session: &SftpSession,
    partial: Option<&Partial>,
    temp: &str,
    size: u64,
    mtime: Option<i64>,
) -> Result<Option<(String, u64)>, SftpError> {
    let Some(partial) = partial else { return Ok(None) };
    if partial.temp_remote.as_deref() != Some(temp) || partial.src_size != size || partial.src_mtime != mtime {
        return Ok(None);
    }
    let found = match session.lstat_plain(temp).await {
        Ok(found) => found,
        Err(SftpError::NotFound { .. }) => return Ok(None),
        Err(err) => return Err(err),
    };
    if found.kind != EntryKind::File || found.size > size {
        return Ok(None);
    }
    let handle = session.open_handle(temp, OpenFlags::WRITE, FileAttributes::empty()).await?;
    Ok(Some((handle, found.size.min(partial.highwater))))
}

/// Writes the local file from `start` on to the open remote `handle`, with many requests in flight,
/// and tells `reporter` how far the file is written without gaps (requests may be answered, or even
/// applied, in any order).
///
/// Stops at a chunk boundary when told to. A user's pause waits (for a while) for the requests
/// already sent; a cancel or a lost session does not wait for anything.
#[allow(clippy::too_many_arguments)]
async fn push(
    session: &SftpSession,
    handle: &str,
    local: &Path,
    start: u64,
    size: u64,
    control: &Control,
    shown: &str,
    reporter: &Reporter,
) -> Result<End, SftpError> {
    let tuning = session.info().await.tuning();
    let (chunk, window) = (tuning.write_chunk, tuning.write_window);
    let drain = drain_timeout(session);
    let mut file = tokio::fs::File::open(local).await.map_err(|err| local_error(local, err))?;
    file.seek(io::SeekFrom::Start(start)).await.map_err(|err| local_error(local, err))?;

    let mut inflight = FuturesUnordered::new();
    let mut offset = start;
    // Writes that were answered but are not yet part of the gapless run from `start`.
    let mut answered: BTreeMap<u64, u64> = BTreeMap::new();
    let mut highwater = start;
    let mut pausing = false;
    loop {
        while !pausing && inflight.len() < window && offset < size {
            match control.interrupted() {
                Some(stop @ (Interrupt::Cancel | Interrupt::Pause(PauseReason::Disconnected))) => {
                    return Ok(End::Interrupted(stop));
                }
                Some(Interrupt::Pause(PauseReason::User)) => {
                    pausing = true;
                    break;
                }
                None => {}
            }
            let len = chunk.min((size - offset) as usize);
            let mut buffer = vec![0u8; len];
            file.read_exact(&mut buffer).await.map_err(|err| local_error(local, err))?;
            let (at, handle) = (offset, handle.to_string());
            offset += len as u64;
            let raw = &session.raw;
            inflight.push(async move {
                raw.read()
                    .await
                    .write(handle, at, buffer)
                    .await
                    .map(|_| (at, len as u64))
                    .map_err(|err| SftpError::from_raw(err, shown))
            });
        }
        if inflight.is_empty() {
            break;
        }
        let next = if pausing {
            // Let what is on its way land, so that the part known to be written is as long as it can be.
            tokio::select! {
                biased;
                stop = control.hard_stop() => return Ok(End::Interrupted(stop)),
                waited = tokio::time::timeout(drain, inflight.next()) => match waited {
                    Ok(next) => next,
                    // Not answering: the pause is no less of a pause for that.
                    Err(_) => return Ok(End::Interrupted(Interrupt::Pause(PauseReason::User))),
                },
            }
        } else {
            tokio::select! {
                biased;
                stop = control.interrupt() => match stop {
                    Interrupt::Pause(PauseReason::User) => {
                        // Stop sending, and wait (for a while) for what is on its way.
                        pausing = true;
                        continue;
                    }
                    hard => return Ok(End::Interrupted(hard)),
                },
                next = inflight.next() => next,
            }
        };
        match next {
            Some(Ok((at, len))) => {
                reporter.progress(len);
                answered.insert(at, len);
                let before = highwater;
                while let Some(len) = answered.remove(&highwater) {
                    highwater += len;
                }
                if highwater != before {
                    reporter.set_highwater(highwater);
                }
            }
            Some(Err(err)) => return Err(err),
            None => break,
        }
        if !pausing {
            match control.interrupted() {
                Some(stop @ (Interrupt::Cancel | Interrupt::Pause(PauseReason::Disconnected))) => {
                    return Ok(End::Interrupted(stop));
                }
                Some(Interrupt::Pause(PauseReason::User)) => pausing = true,
                None => {}
            }
        }
    }
    // A pause that came when everything had already been sent and answered changes nothing.
    Ok(if pausing && offset < size { End::Interrupted(Interrupt::Pause(PauseReason::User)) } else { End::Done })
}

// ---- downloading a file -------------------------------------------------------------------------

async fn download_file(
    reporter: &Reporter,
    session: &SftpSession,
    control: &Control,
    input: &RunInput,
    plan: &Plan,
    index: usize,
    entry: &PlanEntry,
) -> Result<End, SftpError> {
    let top = &plan.tops[entry.top];
    let remote = remote_source(top, entry);
    let final_path = local_target(plan, entry);

    // The source as it is now.
    let source = session.stat(&remote).await?;
    match source.kind {
        EntryKind::File => {}
        EntryKind::Dir => return Err(SftpError::IsADirectory { path: remote }),
        _ => return Err(SftpError::NotAFile { path: remote }),
    }
    let (size, mtime) = (source.size, source.modified);
    if size != entry.size {
        reporter.size_found(index, entry.size, size);
    }

    let name = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = final_path.with_file_name(download_temp_name(&name));
    let mode = (source.mode.unwrap_or(entry.mode) & 0o777) | 0o200;

    let resumed = resume_download(input.partial.as_ref().filter(|p| p.entry == index), &temp, size, mtime).await?;
    let (mut file, start) = match resumed {
        Some(found) => found,
        None => (create_local_temp(&temp, mode).await?, 0),
    };
    reporter.set_partial(Some(Partial {
        entry: index,
        temp_remote: None,
        temp_local: Some(temp.clone()),
        src_size: size,
        src_mtime: mtime,
        highwater: start,
    }));
    reporter.set_offset(start);

    // A file is never dropped with a write still pending: a resume that opens it would race that write.
    let handle = match session.open_handle(&remote, OpenFlags::READ, FileAttributes::empty()).await {
        Ok(handle) => handle,
        Err(err) => {
            let _ = file.flush().await;
            drop(file);
            if !session_gone(&err, control) {
                let _ = tokio::fs::remove_file(&temp).await;
                reporter.set_partial(None);
            }
            return Err(err);
        }
    };
    let pulled =
        pull(session, &handle, &mut file, start, size, control, &remote, |bytes| reporter.progress(bytes)).await;
    quietly(session.close_handle(&handle, &remote)).await;
    match pulled {
        Ok(End::Done) => {}
        Ok(End::Interrupted(interrupt)) => {
            let _ = file.flush().await;
            drop(file);
            if interrupt == Interrupt::Cancel {
                let _ = tokio::fs::remove_file(&temp).await;
                reporter.set_partial(None);
            }
            return Ok(End::Interrupted(interrupt));
        }
        Err(err) => {
            let _ = file.flush().await;
            drop(file);
            // A lost session keeps the temporary file for the resume; any other failure removes it.
            if !session_gone(&err, control) {
                let _ = tokio::fs::remove_file(&temp).await;
                reporter.set_partial(None);
            }
            return Err(err);
        }
    }

    // The file is complete: give it the remote time and move it to its name.
    let finished = async {
        file.flush().await.map_err(|err| local_error(&temp, err))?;
        let std_file = file.into_std().await;
        if let Some(secs) = mtime {
            let time = std::time::UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64);
            std_file.set_modified(time).map_err(|err| local_error(&temp, err))?;
        }
        drop(std_file);
        // Never over something that appeared in the meantime.
        let destination = free_local_name(&final_path).await;
        tokio::fs::rename(&temp, &destination).await.map_err(|err| local_error(&destination, err))?;
        Ok(destination)
    }
    .await;
    match finished {
        Ok(destination) => {
            // Where it really ended up (the name may have been taken since it was planned).
            if entry.rel.is_empty() {
                reporter.set_result(destination);
            }
            Ok(End::Done)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp).await;
            reporter.set_partial(None);
            Err(err)
        }
    }
}

/// `path`, or the first numbered variant that does not exist.
async fn free_local_name(path: &Path) -> PathBuf {
    if tokio::fs::symlink_metadata(path).await.is_err() {
        return path.to_path_buf();
    }
    let (dir, name) =
        (path.parent().unwrap_or(Path::new(".")), path.file_name().map(|n| n.to_string_lossy().into_owned()));
    let name = name.unwrap_or_default();
    for candidate in super::candidates(&name, true).skip(1) {
        let candidate = dir.join(candidate);
        if tokio::fs::symlink_metadata(&candidate).await.is_err() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// Creates the temporary file of a download, exclusively: whatever is at that name is unlinked
/// first (never followed). The mode is the source's plus owner-write, under the umask, like
/// OpenSSH's `sftp get`.
async fn create_local_temp(temp: &Path, mode: u32) -> Result<tokio::fs::File, SftpError> {
    let create = || {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(mode);
        options
    };
    match create().open(temp).await {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            tokio::fs::remove_file(temp).await.map_err(|err| local_error(temp, err))?;
            create().open(temp).await.map_err(|err| local_error(temp, err))
        }
        Err(err) => Err(local_error(temp, err)),
    }
}

/// Opens the temporary file of an interrupted download to continue it, if it is safe to: the
/// source is as it was, and the temporary file is still a regular file no longer than the source.
async fn resume_download(
    partial: Option<&Partial>,
    temp: &Path,
    size: u64,
    mtime: Option<i64>,
) -> Result<Option<(tokio::fs::File, u64)>, SftpError> {
    let Some(partial) = partial else { return Ok(None) };
    if partial.temp_local.as_deref() != Some(temp) || partial.src_size != size || partial.src_mtime != mtime {
        return Ok(None);
    }
    let found = match tokio::fs::symlink_metadata(temp).await {
        Ok(found) => found,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(local_error(temp, err)),
    };
    if !found.is_file() || found.len() > size {
        return Ok(None);
    }
    let mut file = tokio::fs::OpenOptions::new().write(true).open(temp).await.map_err(|err| local_error(temp, err))?;
    file.seek(io::SeekFrom::Start(found.len())).await.map_err(|err| local_error(temp, err))?;
    Ok(Some((file, found.len())))
}

/// Reads the remote file from `start` on and appends it to `file`, with many requests in flight.
/// The file always holds a contiguous prefix of the source, so its length is the way to resume.
#[allow(clippy::too_many_arguments)]
async fn pull(
    session: &SftpSession,
    handle: &str,
    file: &mut tokio::fs::File,
    start: u64,
    size: u64,
    control: &Control,
    shown: &str,
    mut progress: impl FnMut(u64),
) -> Result<End, SftpError> {
    let tuning = session.info().await.tuning();
    let (chunk, window) = (tuning.read_chunk, tuning.read_window);
    let raw = &session.raw;
    let request = |offset: u64, len: u32| async move {
        let result = raw.read().await.read(handle.to_string(), offset, len).await;
        (offset, len, result)
    };

    let mut inflight = FuturesOrdered::new();
    let (mut next, mut written) = (start, start);
    loop {
        if let Some(interrupt) = control.interrupted() {
            return Ok(End::Interrupted(interrupt));
        }
        while inflight.len() < window && next < size {
            let len = chunk.min((size - next) as usize) as u32;
            inflight.push_back(request(next, len));
            next += u64::from(len);
        }
        if inflight.is_empty() {
            break;
        }
        // Nothing to wait for when stopping: what has not arrived is simply read again.
        let answer = tokio::select! {
            biased;
            stop = control.interrupt() => return Ok(End::Interrupted(stop)),
            answer = inflight.next() => answer,
        };
        let Some((offset, asked, result)) = answer else { break };
        match result {
            Ok(data) => {
                let data = data.data;
                if data.is_empty() || data.len() > asked as usize || offset != written {
                    return Err(SftpError::Protocol("服务器返回了意外的数据".into()));
                }
                file.write_all(&data).await.map_err(|err| SftpError::Failed {
                    path: shown.to_string(),
                    message: format!("写入本地文件失败：{err}"),
                })?;
                written += data.len() as u64;
                progress(data.len() as u64);
                if data.len() < asked as usize {
                    // A short answer (allowed by the protocol) breaks the run of offsets the other
                    // requests were made for: ask again from where the file ends.
                    inflight = FuturesOrdered::new();
                    next = written;
                }
            }
            Err(RawError::Status(status)) if status.status_code == StatusCode::Eof => {
                return Err(SftpError::Failed {
                    path: shown.to_string(), message: "文件在传输过程中变短".into()
                });
            }
            Err(err) => return Err(SftpError::from_raw(err, shown)),
        }
    }
    file.flush().await.map_err(|err| SftpError::Failed { path: shown.to_string(), message: err.to_string() })?;
    Ok(End::Done)
}

/// Removes the temporary file a stopped transfer left (for an item cancelled while not running).
pub(super) async fn remove_partial(partial: &Partial, session: Option<&SftpSession>) {
    if let Some(local) = &partial.temp_local {
        let _ = tokio::fs::remove_file(local).await;
    }
    if let (Some(remote), Some(session)) = (&partial.temp_remote, session) {
        let _ = session.remove_file(remote).await;
    }
}
