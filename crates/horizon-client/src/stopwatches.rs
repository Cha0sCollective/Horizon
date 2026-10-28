//! `Stopwatches`: the named wall-clock timers of `/stopwatch`, saved in
//! `data/minecraft/stopwatches.dat` as the elapsed milliseconds of each.
use crate::save;
use horizon_world::nbt::{Compound, JavaString, Tag};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Instant,
};

/// `Util.getMillis`, measured from the first call.
pub fn current_time() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as i64
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stopwatch {
    creation_time: i64,
    accumulated: i64,
}
impl Stopwatch {
    fn elapsed_ms(self, now: i64) -> i64 {
        self.accumulated + (now - self.creation_time)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stopwatches {
    watches: BTreeMap<String, Stopwatch>,
    dirty: bool,
}
fn file(root: &Path) -> PathBuf {
    root.join("data/minecraft/stopwatches.dat")
}
impl Stopwatches {
    pub fn load(root: &Path) -> Self {
        let mut s = Self::default();
        let now = current_time();
        if let Some(Tag::Compound(data)) =
            save::read_dat(&file(root)).and_then(|t| t.get("data").cloned())
            && let Some(Tag::Compound(map)) = data.get(&JavaString::from("stopwatches"))
        {
            for (k, v) in map.iter() {
                if let Tag::Long(ms) = v {
                    s.watches.insert(
                        k.to_string_lossy(),
                        Stopwatch {
                            creation_time: now,
                            accumulated: *ms,
                        },
                    );
                }
            }
        }
        s
    }
    pub fn save(&mut self, root: &Path) -> crate::Result<()> {
        if !self.dirty && self.watches.is_empty() {
            return Ok(());
        }
        self.save_at(root, current_time())
    }
    fn save_at(&mut self, root: &Path, now: i64) -> crate::Result<()> {
        let map: Compound = self
            .watches
            .iter()
            .map(|(k, w)| (k.as_str().into(), Tag::Long(w.elapsed_ms(now))))
            .collect();
        save::write_dat(
            &file(root),
            Tag::compound([
                ("data", Tag::compound([("stopwatches", Tag::Compound(map))])),
                ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
            ]),
        )?;
        self.dirty = false;
        Ok(())
    }
    pub fn ids(&self) -> Vec<String> {
        self.watches.keys().cloned().collect()
    }
    /// `add`: false when the id exists.
    pub fn create(&mut self, id: &str) -> bool {
        if self.watches.contains_key(id) {
            return false;
        }
        self.watches.insert(
            id.to_owned(),
            Stopwatch {
                creation_time: current_time(),
                accumulated: 0,
            },
        );
        self.dirty = true;
        true
    }
    /// `update` with a fresh stopwatch.
    pub fn restart(&mut self, id: &str) -> bool {
        let Some(w) = self.watches.get_mut(id) else {
            return false;
        };
        *w = Stopwatch {
            creation_time: current_time(),
            accumulated: 0,
        };
        self.dirty = true;
        true
    }
    pub fn remove(&mut self, id: &str) -> bool {
        let removed = self.watches.remove(id).is_some();
        self.dirty |= removed;
        removed
    }
    /// `Stopwatch.elapsedSeconds`.
    pub fn elapsed_seconds(&self, id: &str) -> Option<f64> {
        self.watches
            .get(id)
            .map(|w| w.elapsed_ms(current_time()) as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-stopwatch-retry-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn saved_watches(root: &Path) -> Tag {
        let tag = save::read_dat(&file(root)).unwrap();
        assert_eq!(
            tag.get("DataVersion"),
            Some(&Tag::Int(horizon_world::DATA_VERSION))
        );
        tag.get("data").unwrap().get("stopwatches").unwrap().clone()
    }

    #[test]
    fn failed_stopwatch_write_retries_elapsed_snapshot_without_mutating_live_watches() {
        let root = scratch("elapsed");
        let mut watches = Stopwatches::default();
        watches.watches.insert(
            "test:clock".into(),
            Stopwatch {
                creation_time: 100,
                accumulated: 40,
            },
        );
        watches.dirty = true;
        let before = watches.watches.clone();
        let blocked = file(&root).with_file_name("stopwatches.dat_new");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(watches.save_at(&root, 150).is_err());
        assert!(watches.dirty);
        assert_eq!(watches.watches, before);
        std::fs::remove_dir(blocked).unwrap();
        watches.save_at(&root, 160).unwrap();
        assert!(!watches.dirty);
        assert_eq!(watches.watches, before);
        assert_eq!(
            saved_watches(&root),
            Tag::compound([("test:clock", Tag::Long(100))])
        );
        assert_eq!(
            Stopwatches::load(&root).watches["test:clock"].accumulated,
            100
        );
        // Clean nonempty watches keep saving current elapsed time, without resetting their origin.
        watches.save_at(&root, 170).unwrap();
        assert_eq!(
            saved_watches(&root),
            Tag::compound([("test:clock", Tag::Long(110))])
        );
        assert_eq!(watches.watches, before);
        let fresh = root.join("clean-public-save");
        watches.save(&fresh).unwrap();
        assert!(saved_watches(&fresh).get("test:clock").is_some());
        assert_eq!(watches.watches, before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_last_stopwatch_removal_keeps_dirty_for_empty_retry() {
        let root = scratch("removal");
        let mut watches = Stopwatches::default();
        assert!(watches.create("test:last"));
        watches.save(&root).unwrap();
        let path = file(&root);
        let old = std::fs::read(&path).unwrap();
        let blocked = path.with_file_name("stopwatches.dat_new");
        std::fs::create_dir(&blocked).unwrap();
        assert!(watches.remove("test:last"));
        assert!(watches.save(&root).is_err());
        assert!(watches.dirty && watches.watches.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), old);
        std::fs::remove_dir(blocked).unwrap();
        watches.save(&root).unwrap();
        assert!(!watches.dirty);
        assert_eq!(saved_watches(&root), Tag::Compound(Compound::new()));
        let mut reloaded = Stopwatches::load(&root);
        assert!(reloaded.ids().is_empty());
        let sparse = root.join("clean-empty");
        reloaded.save(&sparse).unwrap();
        assert!(!sparse.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
