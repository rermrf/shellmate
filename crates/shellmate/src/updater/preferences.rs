use crate::update::{check::Cache, release::Release};
use serde_json::{Value, json};
use shellmate_store::SharedStore;

const AUTO: &str = "update.auto_check";
const SKIP: &str = "update.skipped_version";
const CACHE: &str = "update.release_cache";

pub(super) struct Preferences {
    db: SharedStore,
    pub auto: bool,
    pub skipped: String,
    pub last: Option<i64>,
}

impl Preferences {
    pub fn load(db: SharedStore, api: Option<&str>) -> (Self, Cache) {
        let read = |key| {
            db.borrow().setting(key).unwrap_or_else(|e| {
                log::warn!("reading {key}: {e}");
                None
            })
        };
        let auto = read(AUTO).as_deref() != Some("0");
        let skipped = read(SKIP).unwrap_or_default();
        let stored = read(CACHE)
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|c| {
                api.is_some()
                    && c["api"].as_str() == api
                    && c["json"]
                        .as_str()
                        .is_some_and(|s| Release::parse(s).is_ok())
            });
        let (cache, last) = stored.map_or_else(
            || (Cache::default(), None),
            |v| {
                (
                    Cache {
                        json: v["json"].as_str().unwrap_or_default().into(),
                        etag: v["etag"].as_str().map(str::to_owned),
                    },
                    v["last_success"].as_i64(),
                )
            },
        );
        (
            Self {
                db,
                auto,
                skipped,
                last,
            },
            cache,
        )
    }

    pub fn due(&self, now: i64) -> bool {
        self.auto
            && !self
                .last
                .is_some_and(|last| (0..6 * 3600).contains(&now.saturating_sub(last)))
    }

    pub fn set_auto(&mut self, value: bool) -> anyhow::Result<()> {
        self.db
            .borrow_mut()
            .set_setting(AUTO, if value { "1" } else { "0" })?;
        self.auto = value;
        Ok(())
    }

    pub fn skip(&mut self, version: String) -> anyhow::Result<()> {
        self.db.borrow_mut().set_setting(SKIP, &version)?;
        self.skipped = version;
        Ok(())
    }

    pub fn save_check(&mut self, api: Option<&str>, cache: &Cache, now: i64) {
        let record = json!({"api":api,"json":cache.json,"etag":cache.etag,"last_success":now});
        if let Err(e) = self.db.borrow_mut().set_setting(CACHE, &record.to_string()) {
            log::warn!("persisting update check: {e}");
        }
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_store::HostStore;

    #[test]
    fn persisted_checks_are_scoped_to_api_and_throttled_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("updates.db");
        let open = || SharedStore::new(HostStore::open(&path).unwrap());
        let api = Some("http://127.0.0.1:8080");
        let (mut prefs, _) = Preferences::load(open(), api);
        assert!(prefs.auto && prefs.due(1000));
        let json=json!({"tag_name":"v0.1.9","name":"fixture","body":"","published_at":"2026-11-20T00:00:00Z","html_url":"http://127.0.0.1/page","assets":[]}).to_string();
        prefs.save_check(
            api,
            &Cache {
                etag: Some("test".into()),
                json: json.clone(),
            },
            1000,
        );
        prefs.skip("0.1.9".into()).unwrap();
        prefs.set_auto(false).unwrap();
        drop(prefs);
        let (mut prefs, cache) = Preferences::load(open(), api);
        assert_eq!(cache.json, json);
        assert_eq!(cache.etag.as_deref(), Some("test"));
        assert_eq!(prefs.skipped, "0.1.9");
        assert_eq!(prefs.last, Some(1000));
        assert!(!prefs.due(1000 + 6 * 3600));
        prefs.set_auto(true).unwrap();
        assert!(!prefs.due(1000 + 6 * 3600 - 1));
        assert!(prefs.due(1000 + 6 * 3600));
        let (prefs, cache) = Preferences::load(open(), Some("http://127.0.0.1:8081"));
        assert!(prefs.last.is_none() && cache.etag.is_none() && cache.json.is_empty());
    }
}
