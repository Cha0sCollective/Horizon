//! Directory-handle capture. Files are not reread by path after capture.
use crate::content::{MAX_FILE, MAX_FILES, MAX_TOTAL, check_path};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, DirEntry, OpenOptions},
};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::Path,
};

pub(super) struct Folder(Dir);
impl Folder {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let path = std::path::absolute(path).map_err(|e| e.to_string())?;
        let parent = path.parent().ok_or("package folder needs a parent")?;
        let name = path.file_name().ok_or("package folder needs a name")?;
        let parent =
            Dir::open_ambient_dir(parent, ambient_authority()).map_err(|e| e.to_string())?;
        parent
            .open_dir_nofollow(name)
            .map(Self)
            .map_err(|e| e.to_string())
    }
    pub(super) fn file(&self, name: &str, max: u64) -> Result<Vec<u8>, String> {
        // Only called with the three fixed root file names. DirEntry opens do
        // not follow the final symlink; their directory handle pins the parent.
        for (count, entry) in self.0.entries().map_err(|e| e.to_string())?.enumerate() {
            if count >= MAX_FILES {
                return Err("too many package-root entries".into());
            }
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_name() == name {
                let bytes = read_file(&entry, max)?;
                return if bytes.is_empty() {
                    Err(format!("{name} is empty"))
                } else {
                    Ok(bytes)
                };
            }
        }
        Err(format!("missing {name}"))
    }
    pub(super) fn content(&self) -> Result<BTreeMap<String, Vec<u8>>, String> {
        let mut files = BTreeMap::new();
        let mut entries = 0;
        let mut total = 0;
        for top in ["assets", "data"] {
            let dir = match self.0.open_dir_nofollow(top) {
                Ok(dir) => dir,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{top}: {e}")),
            };
            walk(&dir, top, 0, &mut entries, &mut total, &mut files)?;
        }
        Ok(files)
    }
}

fn read_file(entry: &DirEntry, max: u64) -> Result<Vec<u8>, String> {
    if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
        return Err("package entries must be regular files, not symlinks or special files".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).nonblock(true).follow(FollowSymlinks::No);
    let file = entry.open_with(&options).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > max {
        return Err(format!(
            "package file exceeds its {max}-byte limit or is not regular"
        ));
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max {
        return Err(format!("package file exceeds its {max}-byte limit"));
    }
    Ok(bytes)
}

fn open_directory(parent: &Dir, entry: &DirEntry) -> Result<Dir, String> {
    parent
        .open_dir_nofollow(entry.file_name())
        .map_err(|e| e.to_string())
}

fn walk(
    dir: &Dir,
    prefix: &str,
    depth: usize,
    entries: &mut usize,
    total: &mut u64,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    if depth >= 64 {
        return Err("paired content exceeds 64 directory levels".into());
    }
    for entry in dir.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        *entries += 1;
        if *entries > MAX_FILES {
            return Err(format!("more than {MAX_FILES} paired content entries"));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "content name is not UTF-8")?;
        let name = format!("{prefix}/{name}");
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            check_path(&format!("{name}/_"))?;
            walk(
                &open_directory(dir, &entry)?,
                &name,
                depth + 1,
                entries,
                total,
                files,
            )?;
        } else {
            check_path(&name)?;
            let bytes = read_file(&entry, MAX_FILE.min(MAX_TOTAL - *total))?;
            *total += bytes.len() as u64;
            if files.insert(name, bytes).is_some() {
                return Err("duplicate captured content path".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn directory_replaced_after_type_check_is_not_followed() {
        let path =
            std::env::temp_dir().join(format!("paired-directory-swap-{}", std::process::id()));
        std::fs::create_dir_all(path.join("child")).unwrap();
        std::fs::create_dir(path.join("target")).unwrap();
        std::fs::write(path.join("target/payload"), b"not from the observed child").unwrap();
        let parent = Dir::open_ambient_dir(&path, ambient_authority()).unwrap();
        let entry = parent
            .entries()
            .unwrap()
            .map(Result::unwrap)
            .find(|e| e.file_name() == "child")
            .unwrap();
        assert!(entry.file_type().unwrap().is_dir());
        std::fs::remove_dir(path.join("child")).unwrap();
        std::os::unix::fs::symlink("target", path.join("child")).unwrap();
        assert!(open_directory(&parent, &entry).is_err());
        drop((entry, parent));
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn traversal_rejects_exhausted_entry_byte_and_depth_budgets() {
        let path = std::env::temp_dir().join(format!("paired-budget-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("payload"), b"abc").unwrap();
        let dir = Dir::open_ambient_dir(&path, ambient_authority()).unwrap();
        for (mut entries, mut total, depth, expected) in [
            (MAX_FILES, 0, 0, "entries"),
            (0, MAX_TOTAL - 2, 0, "byte"),
            (0, 0, 64, "directory levels"),
        ] {
            let mut files = BTreeMap::new();
            let error = walk(
                &dir,
                "assets/demo",
                depth,
                &mut entries,
                &mut total,
                &mut files,
            )
            .unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(files.is_empty());
        }
        drop(dir);
        std::fs::remove_dir_all(path).unwrap();
    }
}
