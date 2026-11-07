// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// Writing files so that losing power leaves either the old contents or the
// new.
//
// Writing a file and renaming it over the original is not enough on its
// own: ext4 can commit the rename before the new file's data, so a power cut
// soon after can leave a file of the right length full of zeros.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Replaces the file at `path` with `contents`.
pub fn write_file_durably(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_file_durably_with(path, contents, |_| Ok(()))
}

/// As write_file_durably(), calling `prepare` on the new file (e.g. to set
/// its permissions or owner) before it replaces the original.
pub fn write_file_durably_with(
    path: &Path,
    contents: &[u8],
    prepare: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let tmp_path = temp_path(path);
    let result = write_and_replace(path, &tmp_path, contents, prepare);
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn write_and_replace(
    path: &Path,
    tmp_path: &Path,
    contents: &[u8],
    prepare: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = File::create(tmp_path)?;
    file.write_all(contents)?;
    // The data must be committed before the rename.
    file.sync_all()?;
    drop(file);
    prepare(tmp_path)?;
    fs::rename(tmp_path, path)?;
    sync_parent_dir(path)
}

/// Makes a completed rename or file creation in `path`'s directory durable.
pub fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

// A scratch file next to `path`, unique to this write: concurrent writes of
// the same file must not share one, or one could rename another's
// half-written file into place.
fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{}.tmp", std::process::id(), n));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The names of the files in `dir`.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.binpb");
        write_file_durably(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write_file_durably(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        // No scratch file is left behind.
        assert_eq!(names(dir.path()), ["state.binpb"]);
    }

    #[test]
    fn prepare_failure_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.binpb");
        write_file_durably(&path, b"original").unwrap();
        let result = write_file_durably_with(&path, b"new", |_| {
            Err(io::Error::other("refused"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(names(dir.path()), ["state.binpb"]);
    }

    #[test]
    fn concurrent_writes_each_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.binpb");
        let contents: Vec<Vec<u8>> =
            (0..8u8).map(|i| vec![i + 1; 64 * 1024]).collect();
        std::thread::scope(|s| {
            for c in &contents {
                let path = &path;
                s.spawn(move || {
                    for _ in 0..20 {
                        write_file_durably(path, c).unwrap();
                    }
                });
            }
        });
        // The result is one writer's complete contents, never a mixture.
        assert!(contents.contains(&fs::read(&path).unwrap()));
        assert_eq!(names(dir.path()), ["state.binpb"]);
    }

    #[test]
    fn scratch_names_are_unique_and_alongside() {
        let a = temp_path(Path::new("dir/prefs.binpb"));
        let b = temp_path(Path::new("dir/prefs.binpb"));
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(Path::new("dir")));
        let name = a.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("prefs.binpb.") && name.ends_with(".tmp"));
    }
}
