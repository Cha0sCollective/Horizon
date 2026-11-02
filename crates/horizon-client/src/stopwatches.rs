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

/// Opaque native timer state for the dormant retained metadata guard. Elapsed
/// milliseconds are sampled once; worker attempts never consult the clock.
#[cfg(feature = "paired-packages")]
pub struct RetainedStopwatchExpected {
    state: RetainedStopwatchState,
    sampled_at: Option<i64>,
}
#[cfg(feature = "paired-packages")]
#[derive(Clone, PartialEq)]
struct RetainedStopwatchState {
    watches: BTreeMap<String, Stopwatch>,
    dirty: bool,
}
#[cfg(feature = "paired-packages")]
#[derive(Clone)]
pub struct RetainedStopwatchInput {
    state: RetainedStopwatchState,
    sampled_at: i64,
    data: Tag,
    skipped: bool,
}
#[cfg(feature = "paired-packages")]
pub struct RetainedStopwatchPost {
    state: RetainedStopwatchState,
    sampled_at: i64,
    wrote: bool,
}
#[cfg(feature = "paired-packages")]
impl RetainedStopwatchPost {
    pub fn did_write(&self) -> bool {
        self.wrote
    }
}
#[cfg(feature = "paired-packages")]
impl RetainedStopwatchInput {
    pub fn skipped(&self) -> bool {
        self.skipped
    }
    pub fn write(&self, root: &Path) -> Result<RetainedStopwatchPost, String> {
        if !self.skipped {
            save::write_dat(&file(root), self.data.clone()).map_err(|error| error.to_string())?;
        }
        Ok(RetainedStopwatchPost {
            state: self.state.clone(),
            sampled_at: self.sampled_at,
            wrote: !self.skipped,
        })
    }
}
#[cfg(feature = "paired-packages")]
impl Stopwatches {
    pub fn retained_state_snapshot(&self) -> RetainedStopwatchExpected {
        RetainedStopwatchExpected {
            state: RetainedStopwatchState {
                watches: self.watches.clone(),
                dirty: self.dirty,
            },
            sampled_at: None,
        }
    }
    pub fn matches_retained_state(&self, expected: &RetainedStopwatchExpected) -> bool {
        self.watches == expected.state.watches && self.dirty == expected.state.dirty
    }
    pub fn prepare_retained_save(
        &self,
        now: i64,
    ) -> Result<(RetainedStopwatchExpected, RetainedStopwatchInput), String> {
        let state = RetainedStopwatchState {
            watches: self.watches.clone(),
            dirty: self.dirty,
        };
        let mut map = Compound::new();
        for (id, watch) in &self.watches {
            let elapsed = i64::try_from(
                i128::from(watch.accumulated) + i128::from(now) - i128::from(watch.creation_time),
            )
            .map_err(|_| format!("stopwatch elapsed milliseconds overflow: {id}"))?;
            map.insert(id.as_str().into(), Tag::Long(elapsed));
        }
        let data = Tag::compound([
            ("data", Tag::compound([("stopwatches", Tag::Compound(map))])),
            ("DataVersion", Tag::Int(horizon_world::DATA_VERSION)),
        ]);
        let input = RetainedStopwatchInput {
            state: state.clone(),
            sampled_at: now,
            data,
            skipped: !self.dirty && self.watches.is_empty(),
        };
        Ok((
            RetainedStopwatchExpected {
                state,
                sampled_at: Some(now),
            },
            input,
        ))
    }
    pub fn preflight_retained_save(
        &self,
        expected: &RetainedStopwatchExpected,
        post: &RetainedStopwatchPost,
    ) -> Result<(), String> {
        if !self.matches_retained_state(expected)
            || post.state != expected.state
            || Some(post.sampled_at) != expected.sampled_at
        {
            return Err("retained stopwatch original state or attempt changed".into());
        }
        Ok(())
    }
    /// The batch owner preflights every family before any commit. This only
    /// acknowledges the original dirty marker; timers keep running unchanged.
    pub fn commit_retained_save(&mut self, post: RetainedStopwatchPost) {
        if post.wrote {
            self.dirty = false;
        }
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

    #[cfg(feature = "paired-packages")]
    #[test]
    fn retained_stopwatch_fixed_i64_native_retry_and_state_preflight() {
        let root = scratch("retained-exact");
        let mut watches = Stopwatches::default();
        watches.watches.insert(
            "test:exact".into(),
            Stopwatch {
                creation_time: 100,
                accumulated: 9_007_199_254_740_993,
            },
        );
        watches.dirty = true;
        let before = watches.watches.clone();
        let (expected, input) = watches.prepare_retained_save(160).unwrap();
        let blocked = file(&root).with_file_name("stopwatches.dat_new");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(input.write(&root).is_err());
        assert!(watches.dirty);
        assert_eq!(watches.watches, before);
        std::fs::remove_dir(blocked).unwrap();
        let post = input.clone().write(&root).unwrap();
        let mut changed = watches.clone();
        changed.watches.get_mut("test:exact").unwrap().accumulated += 1;
        assert!(changed.preflight_retained_save(&expected, &post).is_err());
        assert!(changed.dirty && watches.dirty);
        watches.preflight_retained_save(&expected, &post).unwrap();
        watches.commit_retained_save(post);
        assert!(!watches.dirty);
        assert_eq!(watches.watches, before);
        assert_eq!(
            saved_watches(&root).get("test:exact"),
            Some(&Tag::Long(9_007_199_254_741_053))
        );
        let mut overflow = Stopwatches::default();
        overflow.watches.insert(
            "test:overflow".into(),
            Stopwatch {
                creation_time: -1,
                accumulated: i64::MAX,
            },
        );
        overflow.dirty = true;
        assert!(overflow.prepare_retained_save(1).is_err());
        assert!(overflow.dirty);
        std::fs::remove_dir_all(root).unwrap();
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
