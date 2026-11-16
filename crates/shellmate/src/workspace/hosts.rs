//! Host management: the editor, deletion and the ssh config importer.

use gpui::component::WindowExt as _;
use gpui::component::notification::Notification;
use gpui::{Context, Window};
use shellmate_core::{Host, HostId, ImportCandidate, candidates, parse_ssh_config};

use super::Workspace;
use crate::dialogs::confirm::ConfirmDialog;
use crate::dialogs::host_editor::{EditorMode, HostEditor};
use crate::dialogs::import::{ImportDialog, ImportNotes};
use crate::library::{self, HostDraft};

impl Workspace {
    /// Opens the editor for a new host.
    pub(super) fn new_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        HostEditor::open(cx.entity().downgrade(), EditorMode::New, &self.book, window, cx);
    }

    /// Opens the editor for a saved host, or a new host prefilled from it.
    pub(super) fn edit_host(&mut self, id: &HostId, duplicate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.book.host(id).cloned() else { return };
        let mode = if duplicate { EditorMode::Duplicate(host) } else { EditorMode::Edit(host) };
        HostEditor::open(cx.entity().downgrade(), mode, &self.book, window, cx);
    }

    /// Asks before deleting a host.
    pub(super) fn confirm_delete_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        if shellmate_store::SecretStore::is_locked(self.secrets.as_ref()) {let id=id.clone();let this=cx.entity().downgrade();crate::security::ensure_unlocked(self.secrets.clone(),"删除主机及其凭据",window,cx,move|ok,w,cx|{if ok{this.update(cx,|s,cx|s.confirm_delete_host(&id,w,cx)).ok();}});return;}
        let Some(host) = self.book.host(id) else { return };
        let (name, id) = (host.name.clone(), id.clone());
        let workspace = cx.entity().downgrade();
        ConfirmDialog::open(
            format!("删除主机 {name}？"),
            "已打开的会话不受影响。保存在凭据存储中的此主机的密码会一并移除。",
            "删除主机",
            true,
            move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| workspace.delete_host_now(&id, window, cx))
                    .map_err(|err| err.to_string())?
            },
            window,
            cx,
        );
    }

    fn delete_host_now(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let name = self.book.host(id).map(|h| h.name.clone()).unwrap_or_default();
        self.store.borrow_mut().delete_host(id).map_err(|e|e.to_string())?;
        let warning = shellmate_store::SecretStore::delete(self.secrets.as_ref(), &shellmate_store::SecretKey::HostPassword(id.clone())).err().map(|e|e.to_string());
        self.reload_book(window, cx);
        window.push_notification(Notification::success(format!("已删除 {name}")), cx);
        if let Some(warning) = warning {
            window.push_notification(
                Notification::warning(format!("主机已删除，但凭据存储中的密码没有清除：{warning}")),
                cx,
            );
        }
        Ok(())
    }

    /// Saves what the editor produced. The error is shown inside the editor.
    pub(crate) fn save_host_draft(
        &mut self,
        draft: HostDraft,
        credential: Option<crate::connect::FormCredential>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let name = draft.host.name.clone();
        let id = draft.host.id.clone();
        let endpoint = crate::connect::Endpoint::of(&draft.host, &crate::connect::default_user());
        let host=draft.host.clone();
        let outcome = library::save_host(&mut self.store.borrow_mut(), self.secrets.as_ref(), draft)?;
        if let Some(credential) = credential {
            if let Err(e)=library::save_telnet_password(self.secrets.as_ref(),&host,&credential.answer.secret,credential.answer.save) {
                window.push_notification(Notification::warning(format!("主机已保存，但密码没有保存到凭据存储：{e}")),cx);
            }
            self.pending_forms.insert(id, (endpoint, credential));
        }
        else { self.pending_forms.remove(&id); }
        self.reload_book(window, cx);
        window.push_notification(Notification::success(format!("已保存 {name}")), cx);
        if let Some(warning) = outcome.keychain_warning {
            window.push_notification(Notification::warning(format!("主机已保存，但密码没有保存到凭据存储：{warning}")), cx);
        }
        Ok(())
    }

    /// Resolve an unsaved form through a scratch library. The saved library is untouched.
    pub(crate) fn editor_route(&self, host: &Host) -> Result<Vec<Host>, String> {
        library::editor_route(&self.book,host)
    }

    pub(crate) fn editor_secrets(&self) -> std::sync::Arc<dyn shellmate_store::SecretStore> {
        self.secrets.clone()
    }

    /// Opens the importer with the hosts of `~/.ssh/config`.
    pub(crate) fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let home = std::env::home_dir();
        let workspace = cx.entity().downgrade();
        let groups = self.book.groups.iter().map(|g| g.name.clone()).collect();
        let path = home.as_ref().map(|h| h.join(".ssh").join("config"));
        let text = path.as_ref().map(std::fs::read_to_string);
        let (rows, notes, unavailable): (Vec<ImportCandidate>, ImportNotes, Option<String>) = match text {
            Some(Ok(text)) => {
                let parsed = parse_ssh_config(&text, home.as_deref());
                let rows = candidates(&parsed, &self.book.hosts);
                let importable = rows.iter().filter(|row| row.existing.is_none());
                let notes = ImportNotes {
                    includes: parsed.includes,
                    skipped_patterns: parsed.skipped_patterns,
                    match_blocks: parsed.match_blocks,
                    forwards: importable.clone().map(|row| row.forwards.len()).sum(),
                    skipped_forwards: importable.map(|row| row.skipped_forwards).sum(),
                };
                let unavailable =
                    rows.is_empty().then(|| "~/.ssh/config 里没有可导入的具体主机（只有通配符条目）。".to_string());
                (rows, notes, unavailable)
            }
            Some(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                (Vec::new(), ImportNotes::default(), Some("没有找到 ~/.ssh/config。".into()))
            }
            Some(Err(err)) => (Vec::new(), ImportNotes::default(), Some(format!("无法读取 ~/.ssh/config：{err}"))),
            None => (Vec::new(), ImportNotes::default(), Some("找不到用户目录。".into())),
        };
        ImportDialog::open(workspace, rows, notes, unavailable, groups, window, cx);
    }

    /// Imports the rows chosen in the importer. The error is shown inside the dialog.
    pub(crate) fn import_rows(
        &mut self,
        selected: &[ImportCandidate],
        all: &[ImportCandidate],
        group: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let summary = library::import_hosts(&mut self.store.borrow_mut(), selected, all, group)?;
        self.reload_book(window, cx);
        window.push_notification(Notification::success(summary.message()).title("导入完成"), cx);
        Ok(())
    }
}
