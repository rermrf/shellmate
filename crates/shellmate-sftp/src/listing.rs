use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use crate::{Entry, SftpError};

/// First paint of a directory. Symlink descriptions can arrive later.
#[derive(Clone, Debug)]
pub struct Listed {
    /// Absolute directory, when known.
    pub dir: Option<String>,
    /// Sorted entries, or the reason the directory could not be listed.
    pub entries: Result<Vec<Entry>, SftpError>,
    /// The directory's own attributes, even when READDIR was refused.
    pub stat: Option<Entry>,
}

impl Listed {
    /// A listing whose directory could not be determined.
    pub fn failed(error: SftpError) -> Self {
        Self { dir: None, entries: Err(error), stat: None }
    }
}

#[derive(Default)]
pub(crate) struct Cache {
    rows: VecDeque<(Instant, Arc<Listed>)>,
    home: Option<String>,
    pub epoch: u64,
}

impl Cache {
    pub fn get(&mut self, path: &str) -> Option<Arc<Listed>> {
        let path = if path == "." { self.home.as_deref()? } else { path };
        let index = self.rows.iter().position(|(_, row)| row.dir.as_deref() == Some(path))?;
        let row = self.rows.remove(index)?;
        let found = row.1.clone();
        self.rows.push_front(row);
        Some(found)
    }

    pub fn put(&mut self, path: &str, epoch: u64, started: Instant, listing: Arc<Listed>) {
        if self.epoch != epoch || listing.entries.is_err() {
            return;
        }
        if path == "." {
            self.home.clone_from(&listing.dir);
        }
        if self.rows.iter().any(|(at, old)| old.dir == listing.dir && *at > started) {
            return;
        }
        self.rows.retain(|(_, old)| old.dir != listing.dir);
        self.rows.push_front((started, listing));
        self.rows.truncate(32);
    }

    pub fn invalidate(&mut self, dir: &str) {
        self.epoch += 1;
        self.rows.retain(|(_, row)| row.dir.as_deref() != Some(dir));
    }

    pub fn replace(&mut self, original: &Arc<Listed>, updated: Arc<Listed>) {
        if let Some((_, row)) = self.rows.iter_mut().find(|(_, row)| Arc::ptr_eq(original, row)) {
            *row = updated;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_shares_home_evicts_lru_and_rejects_stale_fills() {
        let mut cache = Cache::default();
        let at = Instant::now();
        let row = |dir: &str| Arc::new(Listed { dir: Some(dir.into()), entries: Ok(Vec::new()), stat: None });
        let home = row("/home/tester");
        cache.put(".", 0, at, home.clone());
        for index in 0..31 {
            let dir = format!("/{index}");
            cache.put(&dir, 0, at, row(&dir));
        }
        assert!(Arc::ptr_eq(&home, &cache.get(".").unwrap()));
        cache.put("/new", 0, at, row("/new"));
        assert!(cache.get("/0").is_none());
        assert!(Arc::ptr_eq(&home, &cache.get("/home/tester").unwrap()));
        cache.invalidate("/home/tester");
        cache.put(".", 0, at, home);
        assert!(cache.get(".").is_none());
        let newer = row("/new");
        cache.put("/new", 1, at + std::time::Duration::from_secs(1), newer.clone());
        cache.put("/new", 1, at, row("/new"));
        assert!(Arc::ptr_eq(&newer, &cache.get("/new").unwrap()));
    }
}
