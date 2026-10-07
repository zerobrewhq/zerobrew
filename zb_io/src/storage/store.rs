use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::extraction::extract::extract_archive;
use zb_core::Error;

pub struct Store {
    store_dir: PathBuf,
    locks_dir: PathBuf,
}

impl Store {
    pub fn new(root: &Path) -> io::Result<Self> {
        let store_dir = root.join("store");
        let locks_dir = root.join("locks");

        fs::create_dir_all(&store_dir)?;
        fs::create_dir_all(&locks_dir)?;

        Ok(Self {
            store_dir,
            locks_dir,
        })
    }

    pub fn entry_path(&self, store_key: &str) -> PathBuf {
        self.store_dir.join(store_key)
    }

    pub fn has_entry(&self, store_key: &str) -> bool {
        self.entry_path(store_key).exists()
    }

    pub fn list_entries(&self) -> Result<Vec<String>, Error> {
        let mut entries = Vec::new();
        for entry in
            fs::read_dir(&self.store_dir).map_err(Error::store("failed to read store directory"))?
        {
            let entry = entry.map_err(Error::store("failed to read store entry"))?;
            let file_type = entry
                .file_type()
                .map_err(Error::store("failed to get store entry type"))?;
            if !file_type.is_dir() {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                entries.push(name);
            }
        }
        Ok(entries)
    }

    /// The marker next to an entry recording what it was prepared for.
    fn ready_path(&self, store_key: &str) -> PathBuf {
        self.store_dir.join(format!("{store_key}.ready"))
    }

    /// Whether the entry exists and was prepared for `fingerprint`.
    fn is_ready(&self, store_key: &str, fingerprint: &str) -> bool {
        self.entry_path(store_key).is_dir()
            && fs::read_to_string(self.ready_path(store_key))
                .is_ok_and(|recorded| recorded == fingerprint)
    }

    /// Unpack `blob_path` into the store as `store_key`, run `prepare` on
    /// the unpacked tree, and publish it. The entry is final when it
    /// appears: `prepare` runs in a temp dir inside the store, and the
    /// rename happens only once it succeeded.
    ///
    /// `fingerprint` names what `prepare` did, such as the prefix a bottle
    /// was relocated for. It is recorded next to the entry, and an entry
    /// recorded for something else, or for nothing (an entry from a release
    /// that unpacked verbatim), is rebuilt from the blob.
    pub fn ensure_entry(
        &self,
        store_key: &str,
        blob_path: &Path,
        fingerprint: &str,
        prepare: impl FnOnce(&Path) -> Result<(), Error>,
    ) -> Result<PathBuf, Error> {
        let entry_path = self.entry_path(store_key);

        // Fast path: already prepared for this fingerprint
        if self.is_ready(store_key, fingerprint) {
            return Ok(entry_path);
        }

        // Acquire exclusive lock for this store_key
        let lock_path = self.locks_dir.join(format!("{store_key}.lock"));
        let lock_file =
            File::create(&lock_path).map_err(Error::store("failed to create lock file"))?;

        lock_file
            .lock()
            .map_err(Error::store("failed to acquire lock"))?;

        // Double-check after acquiring lock (another process may have created it)
        if self.is_ready(store_key, fingerprint) {
            return Ok(entry_path);
        }

        let tmp_dir = tempfile::tempdir_in(&self.store_dir)
            .map_err(Error::store("failed to create temp directory"))?;

        extract_archive(blob_path, tmp_dir.path())?;
        prepare(tmp_dir.path())?;

        // An entry prepared for something else. Kegs cloned from it are
        // independent copies, so replacing it is safe.
        if entry_path.exists() {
            fs::remove_dir_all(&entry_path)
                .map_err(Error::store("failed to remove outdated store entry"))?;
        }
        let _ = fs::remove_file(self.ready_path(store_key));

        // Persist the temp dir by converting it into a permanent path.
        // into_path() prevents auto-cleanup so rename failure still needs manual handling.
        let tmp_path = tmp_dir.keep();
        if let Err(e) = fs::rename(&tmp_path, &entry_path) {
            let _ = fs::remove_dir_all(&tmp_path);
            return Err(Error::StoreCorruption {
                message: format!("failed to rename store entry: {e}"),
            });
        }
        fs::write(self.ready_path(store_key), fingerprint)
            .map_err(Error::store("failed to mark store entry ready"))?;

        // Lock will be released when lock_file is dropped
        Ok(entry_path)
    }

    /// Remove a store entry. This should only be called when the refcount is 0.
    pub fn remove_entry(&self, store_key: &str) -> Result<(), Error> {
        let entry_path = self.entry_path(store_key);

        if !entry_path.exists() {
            return Ok(());
        }

        // Acquire exclusive lock for this store_key
        let lock_path = self.locks_dir.join(format!("{store_key}.lock"));
        let lock_file =
            File::create(&lock_path).map_err(Error::store("failed to create lock file"))?;

        lock_file
            .lock()
            .map_err(Error::store("failed to acquire lock"))?;

        let _ = fs::remove_file(self.ready_path(store_key));
        if entry_path.exists() {
            fs::remove_dir_all(&entry_path)
                .map_err(Error::store("failed to remove store entry"))?;
        }

        // Clean up the lock file
        let _ = fs::remove_file(&lock_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use tar::Builder;
    use tempfile::TempDir;

    fn create_test_tarball(content: &[u8]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header.set_path("test.txt").unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, content).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    /// A store and a blob holding `content` as `test.txt`.
    fn store_with_blob(content: &[u8]) -> (TempDir, Store, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let store = Store::new(tmp.path()).unwrap();
        let blob_path = tmp.path().join("test.tar.gz");
        fs::write(&blob_path, create_test_tarball(content)).unwrap();
        (tmp, store, blob_path)
    }

    fn nothing(_: &Path) -> Result<(), Error> {
        Ok(())
    }

    #[test]
    fn second_call_is_noop() {
        let (_tmp, store, blob_path) = store_with_blob(b"hello world");
        let store_key = "abc123";

        // First call extracts
        let path1 = store
            .ensure_entry(store_key, &blob_path, "fp", nothing)
            .unwrap();
        assert!(path1.exists());
        assert!(path1.join("test.txt").exists());

        // Modify the file to detect if it gets overwritten
        fs::write(path1.join("marker.txt"), "original").unwrap();

        // Second call should be a no-op
        let path2 = store
            .ensure_entry(store_key, &blob_path, "fp", |_| {
                panic!("a ready entry must not be prepared again")
            })
            .unwrap();
        assert_eq!(path1, path2);

        // Marker file should still exist (wasn't re-extracted)
        assert!(path2.join("marker.txt").exists());
    }

    #[test]
    fn prepare_runs_on_the_unpacked_tree_before_it_is_published() {
        let (tmp, store, blob_path) = store_with_blob(b"hello");
        let entry = store.entry_path("prep");

        let path = store
            .ensure_entry("prep", &blob_path, "fp", |root| {
                assert!(root.starts_with(tmp.path().join("store")));
                assert_ne!(root, entry, "prepare must see the temp dir, not the entry");
                assert!(!entry.exists(), "the entry must not be visible yet");
                fs::write(root.join("test.txt"), "prepared").unwrap();
                Ok(())
            })
            .unwrap();

        assert_eq!(path, entry);
        assert_eq!(
            fs::read_to_string(entry.join("test.txt")).unwrap(),
            "prepared"
        );
        assert_eq!(fs::read_to_string(store.ready_path("prep")).unwrap(), "fp");
    }

    #[test]
    fn a_failed_prepare_leaves_no_entry() {
        let (tmp, store, blob_path) = store_with_blob(b"hello");

        let err = store
            .ensure_entry("bad", &blob_path, "fp", |_| {
                Err(Error::StoreCorruption {
                    message: "relocation failed".into(),
                })
            })
            .unwrap_err();

        assert!(err.to_string().contains("relocation failed"));
        assert!(!store.has_entry("bad"));
        assert!(!store.ready_path("bad").exists());
        let leftovers: Vec<_> = fs::read_dir(tmp.path().join("store"))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp dir not cleaned up: {leftovers:?}"
        );

        // The next attempt starts from scratch and can succeed.
        store
            .ensure_entry("bad", &blob_path, "fp", nothing)
            .unwrap();
        assert!(store.has_entry("bad"));
    }

    #[test]
    fn an_entry_prepared_for_another_fingerprint_is_rebuilt() {
        let (_tmp, store, blob_path) = store_with_blob(b"hello");
        let entry = store
            .ensure_entry("fp", &blob_path, "prefix=/a", |root| {
                fs::write(root.join("test.txt"), "for /a").unwrap();
                Ok(())
            })
            .unwrap();
        fs::write(entry.join("stale.txt"), "from the old entry").unwrap();

        let rebuilt = store
            .ensure_entry("fp", &blob_path, "prefix=/b", |root| {
                fs::write(root.join("test.txt"), "for /b").unwrap();
                Ok(())
            })
            .unwrap();

        assert_eq!(rebuilt, entry);
        assert_eq!(
            fs::read_to_string(entry.join("test.txt")).unwrap(),
            "for /b"
        );
        assert!(
            !entry.join("stale.txt").exists(),
            "old contents must be gone"
        );
        assert_eq!(
            fs::read_to_string(store.ready_path("fp")).unwrap(),
            "prefix=/b"
        );
    }

    #[test]
    fn an_entry_without_a_marker_is_rebuilt() {
        // An entry unpacked verbatim by an older release.
        let (_tmp, store, blob_path) = store_with_blob(b"hello");
        let entry = store.entry_path("legacy");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("test.txt"), "verbatim").unwrap();
        assert!(store.has_entry("legacy"));

        let prepared = AtomicUsize::new(0);
        store
            .ensure_entry("legacy", &blob_path, "fp", |root| {
                prepared.fetch_add(1, Ordering::SeqCst);
                fs::write(root.join("test.txt"), "prepared").unwrap();
                Ok(())
            })
            .unwrap();

        assert_eq!(prepared.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(entry.join("test.txt")).unwrap(),
            "prepared"
        );
    }

    #[test]
    fn remove_entry_removes_the_marker_too() {
        let (_tmp, store, blob_path) = store_with_blob(b"hello");
        store
            .ensure_entry("gone", &blob_path, "fp", nothing)
            .unwrap();
        assert!(store.ready_path("gone").exists());

        store.remove_entry("gone").unwrap();

        assert!(!store.has_entry("gone"));
        assert!(!store.ready_path("gone").exists());
        assert!(store.list_entries().unwrap().is_empty());
    }

    #[test]
    fn list_entries_ignores_markers() {
        let (_tmp, store, blob_path) = store_with_blob(b"hello");
        store
            .ensure_entry("listed", &blob_path, "fp", nothing)
            .unwrap();

        assert_eq!(store.list_entries().unwrap(), vec!["listed".to_string()]);
    }

    #[test]
    fn concurrent_calls_unpack_once() {
        let (_tmp, store, blob_path) = store_with_blob(b"concurrent test");
        let store = Arc::new(store);

        let store_key = "concurrent123";
        let prepared = Arc::new(AtomicUsize::new(0));

        // Spawn multiple threads that all try to ensure the same entry
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let store = store.clone();
                let blob = blob_path.clone();
                let prepared = prepared.clone();
                let key = store_key.to_string();

                thread::spawn(move || {
                    store.ensure_entry(&key, &blob, "fp", |_| {
                        prepared.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
            })
            .collect();

        // All threads should succeed
        for handle in handles {
            let result = handle.join().unwrap();
            assert!(result.is_ok());
        }

        assert_eq!(
            prepared.load(Ordering::SeqCst),
            1,
            "prepared more than once"
        );

        // Entry should exist
        assert!(store.has_entry(store_key));

        // Content should be correct
        let entry_path = store.entry_path(store_key);
        let content = fs::read_to_string(entry_path.join("test.txt")).unwrap();
        assert_eq!(content, "concurrent test");
    }

    #[test]
    fn has_entry_returns_correct_state() {
        let (_tmp, store, blob_path) = store_with_blob(b"exists");
        let store_key = "checkme";

        assert!(!store.has_entry(store_key));

        store
            .ensure_entry(store_key, &blob_path, "fp", nothing)
            .unwrap();

        assert!(store.has_entry(store_key));
    }
}
