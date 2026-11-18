//! The state of an open file in the editor: what has been uploaded, what is only in the buffer,
//! what a save is doing. No gpui here: the view feeds it what happens (typing, answers from the
//! server, the user's choices) and draws what it says, so every transition is tested.
//!
//! ```text
//!   Clean ──edit──▶ Modified ──save──▶ Saving ──ok──▶ Saved ──edit──▶ Modified
//!                      ▲  ▲               │  │
//!                      │  └──cancel───────┘  └──refused──▶ Conflict ─(取消)─▶ Modified
//!                      │                  │                   │  └(载入远端版本)──▶ Clean
//!                      └──edit── Failed ◀─┘ (error)           └(覆盖远端)──▶ Saving
//! ```

use shellmate_sftp::{InPlaceReason, LineEnding, Revision, SaveFormat, SaveOutcome, SftpError, TextFile, WriteMode};

/// Where the file stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// What is in the buffer is what the server has.
    Clean,
    /// The buffer has changes that are not uploaded.
    Modified,
    /// An upload is on its way.
    Saving,
    /// The last upload worked (`at` is the time, `HH:MM:SS`); `in_place` says why the file had to be
    /// rewritten in place instead of replaced atomically, if it did.
    Saved { at: String, in_place: Option<InPlaceReason> },
    /// The server's copy changed since the file was opened: nothing was written ([`EditModel::conflict`]).
    Conflict,
    /// The upload failed (the reason is for the user); the buffer is kept and saving again works.
    Failed(String),
}

/// How the status text is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Muted,
    Warn,
    Accent,
    Ok,
    Err,
}

/// The server's copy of a file that changed under the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictInfo {
    /// The text now on the server (`\n` line ends); `None` if it can no longer be shown as text.
    pub remote_text: Option<String>,
    /// When it was changed (seconds since the epoch).
    pub remote_mtime: Option<i64>,
    /// Its size and time: what an overwrite expects to find.
    pub remote_revision: Revision,
}

/// An upload to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveRequest {
    /// What the server's file must still look like (`save_text`'s `expected`): the revision the
    /// editor last saw, or, when overwriting after a conflict, the one the conflict reported.
    pub expected: Revision,
    /// How to write the text.
    pub format: SaveFormat,
}

/// What a finished save means for the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveEnd {
    /// Uploaded. `close` if this save was "上传并关闭" and nothing was typed since.
    Saved { close: bool },
    /// The server's copy is not what was opened: ask the user ([`EditModel::conflict`]).
    Conflict,
    /// It did not work. A lost session has been reported by the file panel already.
    Failed { session_lost: bool },
    /// No save was running: nothing happened.
    Ignored,
}

/// An open file.
#[derive(Clone, Debug)]
pub struct EditModel {
    format: SaveFormat,
    /// The size and time the server's file had when it was last read or written.
    revision: Revision,
    phase: Phase,
    /// How many edits have been made (only ever grows, except when the buffer is replaced).
    edits: u64,
    /// The value of `edits` when the running save began: edits made while it ran are not in it.
    saving_since: u64,
    conflict: Option<ConflictInfo>,
    /// The running save is "上传并关闭".
    close_after: bool,
}

impl EditModel {
    /// A file just opened.
    pub fn new(file: &TextFile) -> Self {
        Self {
            format: SaveFormat::from(file),
            revision: file.revision,
            phase: Phase::Clean,
            edits: 0,
            saving_since: 0,
            conflict: None,
            close_after: false,
        }
    }

    #[cfg(test)]
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The revision the next save expects.
    #[cfg(test)]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    #[cfg(test)]
    pub fn format(&self) -> SaveFormat {
        self.format
    }

    pub fn is_saving(&self) -> bool {
        self.phase == Phase::Saving
    }

    /// The server's copy as it was reported, while the conflict is being decided.
    pub fn conflict(&self) -> Option<&ConflictInfo> {
        self.conflict.as_ref()
    }

    /// The buffer changed.
    pub fn edited(&mut self) {
        self.edits += 1;
        match self.phase {
            // A save is running: it carries what was typed before it began; the file is modified again when it ends.
            Phase::Saving | Phase::Modified => {}
            Phase::Clean | Phase::Saved { .. } | Phase::Failed(_) | Phase::Conflict => {
                self.phase = Phase::Modified;
                self.conflict = None;
            }
        }
    }

    /// ⌘S / 保存并上传: the upload to start, if there is anything to upload. A file that is up to date
    /// is not uploaded again; one that failed is (its buffer was kept); one waiting for the answer
    /// to a conflict is decided by the dialog's buttons, not here.
    pub fn begin_save(&mut self) -> Option<SaveRequest> {
        if !matches!(self.phase, Phase::Modified | Phase::Failed(_)) {
            return None;
        }
        self.close_after = false;
        Some(self.start(self.revision))
    }

    /// 上传并关闭: like [`begin_save`](Self::begin_save); when it works the view closes the editor.
    pub fn begin_save_and_close(&mut self) -> Option<SaveRequest> {
        let request = self.begin_save()?;
        self.close_after = true;
        Some(request)
    }

    fn start(&mut self, expected: Revision) -> SaveRequest {
        self.phase = Phase::Saving;
        self.saving_since = self.edits;
        SaveRequest { expected, format: self.format }
    }

    /// The outcome of the running save. `now` is the time to show if it worked (`HH:MM:SS`).
    pub fn save_done(&mut self, result: Result<SaveOutcome, SftpError>, now: &str) -> SaveEnd {
        if self.phase != Phase::Saving {
            return SaveEnd::Ignored;
        }
        let close = std::mem::take(&mut self.close_after);
        match result {
            Ok(SaveOutcome::Saved { revision, mode }) => {
                self.revision = revision;
                let typed_meanwhile = self.edits > self.saving_since;
                self.phase = if typed_meanwhile {
                    // What was typed while the upload ran is not on the server.
                    Phase::Modified
                } else {
                    Phase::Saved {
                        at: now.to_string(),
                        in_place: match mode {
                            WriteMode::Atomic => None,
                            WriteMode::InPlace(reason) => Some(reason),
                        },
                    }
                };
                SaveEnd::Saved { close: close && !typed_meanwhile }
            }
            Ok(SaveOutcome::Conflict { remote_text, remote_mtime, remote_revision }) => {
                self.conflict = Some(ConflictInfo { remote_text, remote_mtime, remote_revision });
                self.phase = Phase::Conflict;
                SaveEnd::Conflict
            }
            Err(err) => {
                self.phase = Phase::Failed(err.to_string());
                SaveEnd::Failed { session_lost: err.is_session_lost() }
            }
        }
    }

    /// 取消 in the conflict dialog: back to a modified buffer.
    pub fn cancel_conflict(&mut self) {
        if self.phase == Phase::Conflict {
            self.phase = Phase::Modified;
            self.conflict = None;
        }
    }

    /// 载入远端版本: the text to put in the buffer (the view does that), with the remote revision as
    /// the one to expect from now on; the editor is clean. `None` if the remote text cannot be shown.
    pub fn load_remote(&mut self) -> Option<String> {
        if self.phase != Phase::Conflict {
            return None;
        }
        let info = self.conflict.as_ref()?;
        let text = info.remote_text.clone()?;
        self.revision = info.remote_revision;
        self.conflict = None;
        self.phase = Phase::Clean;
        self.edits = 0;
        self.saving_since = 0;
        Some(text)
    }

    /// 覆盖远端: saves again, expecting the revision the conflict reported (not "anything"), so that
    /// a third change made after the dialog came up is another conflict instead of being overwritten.
    pub fn overwrite(&mut self) -> Option<SaveRequest> {
        if self.phase != Phase::Conflict {
            return None;
        }
        let expected = self.conflict.take()?.remote_revision;
        self.close_after = false;
        Some(self.start(expected))
    }

    /// Whether closing the editor loses something (or something that may be lost): the buffer is
    /// not known to be on the server.
    pub fn close_needs_confirmation(&self) -> bool {
        !matches!(self.phase, Phase::Clean | Phase::Saved { .. })
    }

    /// The text at the top right, and its colour.
    pub fn status(&self) -> (String, Tone) {
        match &self.phase {
            Phase::Clean => ("已与服务器同步".into(), Tone::Muted),
            Phase::Modified => ("● 已修改，未上传".into(), Tone::Warn),
            Phase::Saving => ("正在通过 SFTP 上传…".into(), Tone::Accent),
            Phase::Saved { at, in_place: None } => (format!("✓ 已上传 · {at}"), Tone::Ok),
            Phase::Saved { at, in_place: Some(reason) } => {
                (format!("✓ 已上传 · {at}（已直接写回：{}）", reason.label()), Tone::Ok)
            }
            Phase::Conflict => ("远端文件已被修改".into(), Tone::Warn),
            Phase::Failed(reason) => (format!("上传失败：{reason}"), Tone::Err),
        }
    }

    pub fn local_status(&self) -> (String, Tone) {
        match &self.phase {
            Phase::Clean => ("已与磁盘同步".into(), Tone::Muted),
            Phase::Modified => ("● 已修改，未保存".into(), Tone::Warn),
            Phase::Saving => ("正在保存…".into(), Tone::Accent),
            Phase::Saved { at, .. } => (format!("✓ 已保存 · {at}"), Tone::Ok),
            Phase::Conflict => ("磁盘文件已被修改".into(), Tone::Warn),
            Phase::Failed(reason) => (format!("保存失败：{reason}"), Tone::Err),
        }
    }

    /// The footer's first part: `19 行 · UTF-8 · LF`, with `UTF-8 BOM` or `CRLF` where those apply.
    pub fn footer(&self, lines: usize) -> String {
        let encoding = if self.format.bom { "UTF-8 BOM" } else { "UTF-8" };
        let ending = match self.format.line_ending {
            LineEnding::Lf => LineEnding::Lf.label(),
            LineEnding::CrLf => LineEnding::CrLf.label(),
        };
        format!("{lines} 行 · {encoding} · {ending}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> TextFile {
        TextFile {
            path: "/srv/app/nginx.conf".into(),
            target: "/srv/app/nginx.conf".into(),
            text: text.into(),
            revision: Revision { size: text.len() as u64, mtime: Some(1_000) },
            line_ending: LineEnding::Lf,
            bom: false,
            mode: Some(0o644),
        }
    }

    fn model() -> EditModel {
        EditModel::new(&file("server {}\n"))
    }

    fn rev(size: u64, mtime: i64) -> Revision {
        Revision { size, mtime: Some(mtime) }
    }

    fn saved(revision: Revision) -> Result<SaveOutcome, SftpError> {
        Ok(SaveOutcome::Saved { revision, mode: WriteMode::Atomic })
    }

    fn conflict(remote: Option<&str>) -> Result<SaveOutcome, SftpError> {
        Ok(SaveOutcome::Conflict {
            remote_text: remote.map(str::to_string),
            remote_mtime: Some(2_000),
            remote_revision: rev(77, 2_000),
        })
    }

    /// A model that is modified and saving.
    fn saving() -> EditModel {
        let mut model = model();
        model.edited();
        model.begin_save().expect("a save");
        model
    }

    // ---- the happy path ----

    #[test]
    fn a_file_just_opened_is_clean() {
        let model = model();
        assert_eq!(model.phase(), &Phase::Clean);
        assert_eq!(model.status(), ("已与服务器同步".to_string(), Tone::Muted));
        assert!(!model.close_needs_confirmation());
        assert_eq!(model.revision(), rev(10, 1_000));
    }

    #[test]
    fn typing_makes_it_modified_and_saving_uploads_it() {
        let mut model = model();
        model.edited();
        assert_eq!(model.phase(), &Phase::Modified);
        assert_eq!(model.status(), ("● 已修改，未上传".to_string(), Tone::Warn));
        let request = model.begin_save().expect("something to upload");
        assert_eq!(request, SaveRequest { expected: rev(10, 1_000), format: model.format() });
        assert_eq!(model.phase(), &Phase::Saving);
        assert_eq!(model.status(), ("正在通过 SFTP 上传…".to_string(), Tone::Accent));
        assert!(model.is_saving());

        assert_eq!(model.save_done(saved(rev(12, 1_500)), "09:12:03"), SaveEnd::Saved { close: false });
        assert_eq!(model.phase(), &Phase::Saved { at: "09:12:03".into(), in_place: None });
        assert_eq!(model.status(), ("✓ 已上传 · 09:12:03".to_string(), Tone::Ok));
        assert_eq!(model.revision(), rev(12, 1_500), "the next save expects what this one wrote");
        assert!(!model.close_needs_confirmation());
    }

    #[test]
    fn every_save_expects_the_revision_of_the_previous_one() {
        let mut model = model();
        model.edited();
        assert_eq!(model.begin_save().unwrap().expected, rev(10, 1_000), "the one from opening");
        model.save_done(saved(rev(11, 1_100)), "09:00:00");
        model.edited();
        assert_eq!(model.begin_save().unwrap().expected, rev(11, 1_100), "the one from the first save");
        model.save_done(saved(rev(12, 1_200)), "09:00:01");
        model.edited();
        assert_eq!(model.begin_save().unwrap().expected, rev(12, 1_200));
    }

    #[test]
    fn a_save_written_in_place_says_why() {
        for (reason, text) in [
            (InPlaceReason::DirNotWritable, "✓ 已上传 · 09:12:03（已直接写回：目录不可写）"),
            (InPlaceReason::KeepOwner, "✓ 已上传 · 09:12:03（已直接写回：保留属主）"),
        ] {
            let mut model = saving();
            let outcome = Ok(SaveOutcome::Saved { revision: rev(12, 1_500), mode: WriteMode::InPlace(reason) });
            model.save_done(outcome, "09:12:03");
            assert_eq!(model.status(), (text.to_string(), Tone::Ok));
        }
    }

    #[test]
    fn saving_a_file_with_nothing_to_upload_does_nothing() {
        let mut model = model();
        assert_eq!(model.begin_save(), None, "clean");
        let mut model = saving();
        assert_eq!(model.begin_save(), None, "already saving: not twice");
        model.save_done(saved(rev(12, 1_500)), "09:12:03");
        assert_eq!(model.begin_save(), None, "saved and not changed since");
        assert_eq!(model.begin_save_and_close(), None);
    }

    // ---- typing while saving ----

    #[test]
    fn what_is_typed_during_a_save_leaves_the_file_modified_afterwards() {
        let mut model = saving();
        model.edited();
        assert_eq!(model.phase(), &Phase::Saving, "the status keeps saying so while it runs");
        assert_eq!(model.save_done(saved(rev(12, 1_500)), "09:12:03"), SaveEnd::Saved { close: false });
        assert_eq!(model.phase(), &Phase::Modified, "not clean: the new text is not on the server");
        assert_eq!(model.revision(), rev(12, 1_500), "but what is there is known");
        assert!(model.close_needs_confirmation());
        // And it can be saved again.
        assert_eq!(model.begin_save().unwrap().expected, rev(12, 1_500));
    }

    #[test]
    fn a_save_that_fails_after_typing_stays_failed() {
        let mut model = saving();
        model.edited();
        model.save_done(Err(SftpError::Timeout), "09:12:03");
        assert!(matches!(model.phase(), Phase::Failed(_)));
    }

    // ---- failures ----

    #[test]
    fn a_failed_save_keeps_the_buffer_and_saving_again_works() {
        let mut model = saving();
        let end = model.save_done(Err(SftpError::PermissionDenied { path: "/srv/app/nginx.conf".into() }), "09:12:03");
        assert_eq!(end, SaveEnd::Failed { session_lost: false });
        assert_eq!(model.status(), ("上传失败：权限不足：/srv/app/nginx.conf".to_string(), Tone::Err));
        assert!(model.close_needs_confirmation(), "the edits are not on the server");
        assert_eq!(model.revision(), rev(10, 1_000), "nothing was written: still the opened revision");

        let request = model.begin_save().expect("save again");
        assert_eq!(request.expected, rev(10, 1_000));
        assert_eq!(model.save_done(saved(rev(12, 1_500)), "09:13:00"), SaveEnd::Saved { close: false });
        assert_eq!(model.phase(), &Phase::Saved { at: "09:13:00".into(), in_place: None });
    }

    #[test]
    fn a_save_while_disconnected_fails_with_the_content_kept_and_reports_the_lost_session() {
        let mut model = saving();
        let end = model.save_done(Err(SftpError::Disconnected(String::new())), "09:12:03");
        assert_eq!(end, SaveEnd::Failed { session_lost: true }, "the view makes the panel reopen the session");
        assert_eq!(model.status().1, Tone::Err);
        assert!(model.status().0.starts_with("上传失败：连接已断开"));
        // After the reopen the user just presses ⌘S again.
        assert!(model.begin_save().is_some());
        let mut model = saving();
        assert_eq!(model.save_done(Err(SftpError::Timeout), "x"), SaveEnd::Failed { session_lost: true });
    }

    #[test]
    fn typing_after_a_failure_makes_it_plainly_modified_again() {
        let mut model = saving();
        model.save_done(Err(SftpError::Timeout), "09:12:03");
        model.edited();
        assert_eq!(model.phase(), &Phase::Modified);
    }

    // ---- conflicts ----

    #[test]
    fn a_save_against_a_changed_remote_file_is_a_conflict_with_the_remote_state() {
        let mut model = saving();
        assert_eq!(model.save_done(conflict(Some("server { new }\n")), "09:12:03"), SaveEnd::Conflict);
        assert_eq!(model.phase(), &Phase::Conflict);
        assert_eq!(model.status(), ("远端文件已被修改".to_string(), Tone::Warn));
        let info = model.conflict().expect("the dialog's data");
        assert_eq!(info.remote_text.as_deref(), Some("server { new }\n"));
        assert_eq!((info.remote_mtime, info.remote_revision), (Some(2_000), rev(77, 2_000)));
        assert!(model.close_needs_confirmation());
        assert_eq!(model.begin_save(), None, "the dialog decides, not ⌘S");
    }

    #[test]
    fn cancelling_a_conflict_leaves_the_buffer_modified() {
        let mut model = saving();
        model.save_done(conflict(Some("x")), "t");
        model.cancel_conflict();
        assert_eq!(model.phase(), &Phase::Modified);
        assert_eq!(model.conflict(), None);
        assert_eq!(
            model.revision(),
            rev(10, 1_000),
            "still the revision it was opened at: the next save conflicts again"
        );
        // A cancel when nothing is in conflict changes nothing.
        model.cancel_conflict();
        assert_eq!(model.phase(), &Phase::Modified);
    }

    #[test]
    fn loading_the_remote_version_replaces_the_buffer_and_makes_it_clean() {
        let mut model = saving();
        model.edited();
        model.save_done(conflict(Some("remote text\n")), "t");
        assert_eq!(model.load_remote().as_deref(), Some("remote text\n"));
        assert_eq!(model.phase(), &Phase::Clean);
        assert_eq!(model.revision(), rev(77, 2_000), "the editor now has that revision");
        assert!(!model.close_needs_confirmation());
        // The next edit and save expect that revision.
        model.edited();
        assert_eq!(model.begin_save().unwrap().expected, rev(77, 2_000));
    }

    #[test]
    fn a_remote_that_is_not_text_cannot_be_loaded() {
        let mut model = saving();
        model.save_done(conflict(None), "t");
        assert_eq!(model.load_remote(), None);
        assert_eq!(model.phase(), &Phase::Conflict, "still deciding");
        // Overwrite and cancel still work.
        assert!(model.overwrite().is_some());
    }

    #[test]
    fn overwriting_expects_the_revision_the_conflict_reported_not_anything() {
        let mut model = saving();
        model.save_done(conflict(Some("x")), "t");
        let request = model.overwrite().expect("an upload");
        assert_eq!(request.expected, rev(77, 2_000), "Some(conflict.remote_revision), never None");
        assert_eq!(model.phase(), &Phase::Saving);
        assert_eq!(model.conflict(), None);
        assert_eq!(model.save_done(saved(rev(14, 2_100)), "09:20:00"), SaveEnd::Saved { close: false });
        assert_eq!(model.revision(), rev(14, 2_100));
    }

    #[test]
    fn a_third_change_after_the_dialog_is_another_conflict() {
        let mut model = saving();
        model.save_done(conflict(Some("second version\n")), "t");
        model.overwrite().unwrap();
        // The server says the file is not what the conflict reported any more.
        let third = Ok(SaveOutcome::Conflict {
            remote_text: Some("third version\n".into()),
            remote_mtime: Some(3_000),
            remote_revision: rev(90, 3_000),
        });
        assert_eq!(model.save_done(third, "t"), SaveEnd::Conflict);
        assert_eq!(model.conflict().map(|c| c.remote_revision), Some(rev(90, 3_000)));
        assert_eq!(model.overwrite().unwrap().expected, rev(90, 3_000), "the new state this time");
    }

    #[test]
    fn conflict_choices_only_apply_to_a_conflict() {
        let mut model = model();
        assert_eq!((model.load_remote(), model.overwrite()), (None, None));
        let mut model = saving();
        assert_eq!((model.load_remote(), model.overwrite()), (None, None));
    }

    // ---- closing ----

    #[test]
    fn closing_asks_unless_the_file_is_known_to_be_on_the_server() {
        let mut model = model();
        assert!(!model.close_needs_confirmation(), "clean");
        model.edited();
        assert!(model.close_needs_confirmation(), "modified");
        model.begin_save().unwrap();
        assert!(model.close_needs_confirmation(), "a save is running: its outcome is not known");
        model.save_done(saved(rev(12, 1_500)), "t");
        assert!(!model.close_needs_confirmation(), "saved");
        model.edited();
        model.begin_save().unwrap();
        model.save_done(Err(SftpError::Timeout), "t");
        assert!(model.close_needs_confirmation(), "failed");
    }

    #[test]
    fn upload_and_close_closes_only_when_the_save_worked() {
        let mut model = model();
        model.edited();
        let request = model.begin_save_and_close().expect("an upload");
        assert_eq!(request.expected, rev(10, 1_000));
        assert_eq!(model.save_done(saved(rev(12, 1_500)), "t"), SaveEnd::Saved { close: true });

        // A conflict keeps the editor open, and so does what is decided afterwards: the user chose to look.
        let mut model = model_modified();
        model.begin_save_and_close().unwrap();
        assert_eq!(model.save_done(conflict(Some("x")), "t"), SaveEnd::Conflict);
        model.overwrite().unwrap();
        assert_eq!(model.save_done(saved(rev(12, 1_500)), "t"), SaveEnd::Saved { close: false });

        // A failure keeps it open too, and a plain save afterwards does not close it.
        let mut model = model_modified();
        model.begin_save_and_close().unwrap();
        assert_eq!(model.save_done(Err(SftpError::Timeout), "t"), SaveEnd::Failed { session_lost: true });
        model.begin_save().unwrap();
        assert_eq!(model.save_done(saved(rev(12, 1_500)), "t"), SaveEnd::Saved { close: false });
    }

    fn model_modified() -> EditModel {
        let mut model = model();
        model.edited();
        model
    }

    #[test]
    fn a_result_nobody_waits_for_is_ignored() {
        let mut model = model();
        assert_eq!(model.save_done(saved(rev(1, 1)), "t"), SaveEnd::Ignored);
        assert_eq!(model.phase(), &Phase::Clean);
        assert_eq!(model.revision(), rev(10, 1_000), "not touched");
    }

    // ---- the footer ----

    #[test]
    fn the_footer_names_lines_encoding_and_line_ending() {
        let mut opened = file("a\r\nb\r\n");
        let model = EditModel::new(&opened);
        assert_eq!(model.footer(19), "19 行 · UTF-8 · LF");
        opened.line_ending = LineEnding::CrLf;
        assert_eq!(EditModel::new(&opened).footer(2), "2 行 · UTF-8 · CRLF");
        opened.bom = true;
        assert_eq!(EditModel::new(&opened).footer(2), "2 行 · UTF-8 BOM · CRLF");
        opened.line_ending = LineEnding::Lf;
        assert_eq!(EditModel::new(&opened).footer(0), "0 行 · UTF-8 BOM · LF");
    }
}
