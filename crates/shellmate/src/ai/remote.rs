//! Internal read-only discovery, never a model tool or a registered background task.
use super::{exec::Binding, host::Ui};
use shellmate_core::remote_instructions::{self as remote, Discovery};

pub async fn discover(
    binding: &Binding,
    ui: &async_channel::Sender<Ui>,
    cwd: Option<String>,
    refresh: bool,
) -> Option<Discovery> {
    if !binding.capabilities.background || (!binding.local && binding.connection.is_none()) {
        return None;
    }
    let result = async {
        let cwd = if refresh {
            let (reply, receive) = tokio::sync::oneshot::channel();
            ui.send(Ui::LocalCwd {
                session: binding.session,
                reply,
            })
            .await
            .map_err(|_| "context bridge closed".to_string())?;
            receive
                .await
                .map_err(|_| "context request cancelled".to_string())?
        } else {
            cwd
        };
        let nonce = remote::nonce();
        let script = remote::script(cwd.as_deref(), &nonce)?;
        let output = if binding.local {
            super::exec::discovery_local(&script, None).await?
        } else {
            let command = format!("sh -c {}", remote::quote(&script));
            let output = binding
                .connection
                .as_ref()
                .unwrap()
                .exec_with(
                    &command,
                    shellmate_ssh::ExecOptions {
                        timeout: remote::TIMEOUT,
                        max_output: remote::WIRE_BYTES,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            if !output.status.success() || output.truncated {
                return Err("remote instruction exec failed or exceeded output limit".into());
            }
            output.stdout
        };
        remote::parse(&output, &nonce)
    };
    let result = tokio::select! {
        biased;
        () = super::exec::disconnected(&binding.alive) => Err("session disconnected".to_string()),
        result = tokio::time::timeout(remote::TIMEOUT, result) => result.unwrap_or_else(|_| Err("remote instruction discovery timed out".into())),
    };
    match result {
        Ok(discovery) => Some(discovery),
        Err(error) => {
            log::debug!("project instruction discovery skipped: {error}");
            None
        }
    }
}

/// A batch keeps known decisions fixed; cancellation never returns new decisions to persist.
pub struct TrustBatch {
    pub discovery: Discovery,
    pub choices: Vec<Option<bool>>,
    pub unknown: Vec<usize>,
}
impl TrustBatch {
    pub fn new(discovery: Discovery, mut lookup: impl FnMut(&str) -> Option<bool>) -> Self {
        let choices: Vec<_> = discovery.files.iter().map(|f| lookup(&f.path)).collect();
        let unknown = choices
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.is_none().then_some(i))
            .collect();
        Self {
            discovery,
            choices,
            unknown,
        }
    }
    pub fn decisions(&self) -> Vec<(String, bool)> {
        self.unknown
            .iter()
            .filter_map(|i| {
                self.choices[*i].map(|choice| (self.discovery.files[*i].path.clone(), choice))
            })
            .collect()
    }
    pub fn finish(mut self, cancel: bool) -> Discovery {
        self.discovery.cancelled = cancel && !self.unknown.is_empty();
        let mut i = 0;
        self.discovery.files.retain(|_| {
            let keep = self.choices[i] == Some(true) && !(cancel && self.unknown.contains(&i));
            i += 1;
            keep
        });
        self.discovery
    }
}

#[derive(Default)]
pub struct Decisions(std::collections::HashMap<(String, String), bool>);
impl Decisions {
    pub fn prepare(
        &self,
        db: &shellmate_store::HostStore,
        key: &str,
        persist: bool,
        discovery: Discovery,
    ) -> TrustBatch {
        TrustBatch::new(discovery, |path| {
            if persist {
                db.remote_doc_trust(key, path).ok().flatten()
            } else {
                self.0.get(&(key.to_owned(), path.to_owned())).copied()
            }
        })
    }
    pub fn save(
        &mut self,
        db: &mut shellmate_store::HostStore,
        key: &str,
        persist: bool,
        batch: &TrustBatch,
    ) -> shellmate_store::Result<()> {
        let decisions = batch.decisions();
        if persist {
            db.save_remote_doc_trust(key, &decisions)?;
        } else {
            for (path, choice) in decisions {
                self.0.insert((key.to_owned(), path), choice);
            }
        }
        Ok(())
    }
}
#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
