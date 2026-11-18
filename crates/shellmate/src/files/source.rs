use std::sync::Arc;

use super::local::{LocalFs, Result};
use shellmate_sftp::{Listed, Revision, SaveFormat, SaveOutcome, SftpError, SftpSession, TextFile};

#[derive(Clone)]
pub(super) enum FileSource {
    Sftp(Arc<SftpSession>),
    Local(Arc<LocalFs>),
}

pub(super) async fn blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|err| SftpError::Failed {
            path: "本机文件".into(),
            message: err.to_string(),
        })?
}

impl FileSource {
    pub fn cached(&self, dir: &str) -> Option<Arc<Listed>> {
        match self {
            Self::Local(_) => None,
            Self::Sftp(session) => session.cached(dir),
        }
    }

    pub async fn listing(self, dir: String) -> Arc<Listed> {
        match self {
            Self::Sftp(session) => session.listing(&dir).await,
            Self::Local(fs) => Arc::new(
                blocking(move || Ok(fs.listing(&dir)))
                    .await
                    .unwrap_or_else(Listed::failed),
            ),
        }
    }

    pub async fn rename(self, from: String, to: String) -> Result<()> {
        match self {
            Self::Sftp(session) => session.rename(&from, &to).await,
            Self::Local(fs) => blocking(move || fs.rename(&from, &to)).await,
        }
    }

    pub async fn create_dir_unique(self, dir: String, base: &'static str) -> Result<String> {
        match self {
            Self::Sftp(session) => session.create_dir_unique(&dir, base).await,
            Self::Local(fs) => blocking(move || fs.create_dir_unique(&dir, base)).await,
        }
    }

    pub async fn remove(self, paths: Vec<String>) -> Result<()> {
        match self {
            Self::Sftp(session) => {
                for path in paths {
                    session.remove(&path).await?;
                }
                Ok(())
            }
            Self::Local(fs) => blocking(move || fs.trash_selected(&paths)).await,
        }
    }

    pub async fn chmod(self, path: String, mode: u32) -> Result<()> {
        match self {
            Self::Sftp(session) => session.chmod(&path, mode).await,
            Self::Local(fs) => blocking(move || fs.chmod(&path, mode)).await,
        }
    }

    pub async fn open_text(self, path: String) -> Result<TextFile> {
        match self {
            Self::Sftp(session) => session.open_text(&path).await,
            Self::Local(fs) => blocking(move || fs.open_text(&path)).await,
        }
    }

    pub async fn save_text(
        self,
        path: String,
        text: String,
        format: SaveFormat,
        expected: Revision,
    ) -> Result<SaveOutcome> {
        match self {
            Self::Sftp(session) => session.save_text(&path, &text, format, Some(expected)).await,
            Self::Local(fs) => blocking(move || fs.save_text(&path, &text, format, expected)).await,
        }
    }
}
