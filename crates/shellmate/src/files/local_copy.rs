use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use super::local::{Result, io_error, rename_with};
use shellmate_sftp::SftpError;
use shellmate_sftp::transfer::{Choice, Clash, candidates};

pub(super) struct CopyPlan {
    destination: PathBuf,
    items: Vec<(PathBuf, String, bool)>,
    pub clash: Clash,
}

impl CopyPlan {
    pub fn new(sources: Vec<PathBuf>, dir: String) -> Result<Self> {
        let destination = fs::canonicalize(&dir).map_err(|err| io_error(dir.as_ref(), err))?;
        let mut seen = HashSet::new();
        let mut clash = Clash {
            dir,
            count: 0,
            names: Vec::new(),
        };
        let mut items = Vec::new();
        for source in sources {
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| io_error(&source, io::ErrorKind::InvalidInput.into()))?
                .to_owned();
            let target = destination.join(&name);
            let taken = match fs::symlink_metadata(&target) {
                Ok(_) => true,
                Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                Err(err) => return Err(io_error(&target, err)),
            };
            let collision = !seen.insert(name.clone()) || taken;
            if collision {
                clash.count += 1;
                if clash.names.len() < 5 {
                    clash.names.push(name.clone());
                }
            }
            items.push((source, name, collision));
        }
        Ok(Self {
            destination,
            items,
            clash,
        })
    }

    pub fn execute(self, choice: Choice) -> Result<usize> {
        let mut copied = 0;
        for (source, name, collision) in self.items {
            if collision && choice == Choice::Skip {
                continue;
            }
            let dest = self.destination.join(&name);
            let metadata = fs::symlink_metadata(&source).map_err(|err| io_error(&source, err))?;
            if metadata.is_dir() && contains(&metadata, &self.destination).map_err(|err| io_error(&source, err))? {
                return Err(SftpError::Failed {
                    path: source.display().to_string(),
                    message: "不能将目录拷贝到自身或子目录".into(),
                });
            }
            if collision
                && choice == Choice::Overwrite
                && let Ok(target) = fs::symlink_metadata(&dest)
            {
                let same = target.dev() == metadata.dev() && target.ino() == metadata.ino();
                let ancestor = target.is_dir()
                    && source
                        .parent()
                        .is_some_and(|parent| contains(&target, parent).unwrap_or(true));
                if same || ancestor {
                    return Err(SftpError::Refused {
                        path: dest.display().to_string(),
                    });
                }
            }
            let staging = tempfile::tempdir_in(&self.destination).map_err(|err| io_error(&dest, err))?;
            let staged = staging.path().join("item");
            copy_tree(&source, &staged).map_err(|err| io_error(&source, err))?;
            let mut installed = false;
            for candidate in candidates(&name, !metadata.is_dir()) {
                let target = self.destination.join(candidate);
                match rename_with(&staged, &target, libc::RENAME_EXCL) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists && collision => match choice {
                        Choice::KeepBoth => continue,
                        Choice::Overwrite => {
                            rename_with(&staged, &target, libc::RENAME_SWAP).map_err(|err| io_error(&target, err))?
                        }
                        Choice::Skip => unreachable!(),
                    },
                    Err(err) => return Err(io_error(&target, err)),
                }
                installed = true;
                break;
            }
            if !installed {
                return Err(SftpError::AlreadyExists {
                    path: dest.display().to_string(),
                });
            }
            staging.close().map_err(|err| io_error(&dest, err))?;
            copied += 1;
        }
        Ok(copied)
    }
}

fn contains(directory: &fs::Metadata, path: &Path) -> io::Result<bool> {
    for ancestor in fs::canonicalize(path)?.ancestors() {
        let metadata = fs::metadata(ancestor)?;
        if (metadata.dev(), metadata.ino()) == (directory.dev(), directory.ino()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_symlink() {
        symlink(fs::read_link(source)?, target)
    } else if meta.is_file() {
        fs::copy(source, target).map(|_| ())
    } else if meta.is_dir() {
        fs::create_dir(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            copy_tree(&child.path(), &target.join(child.file_name()))?;
        }
        fs::set_permissions(target, meta.permissions())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "不能拷贝特殊文件"))
    }
}
