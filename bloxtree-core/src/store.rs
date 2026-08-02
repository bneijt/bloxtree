use std::{
    fs, io,
    path::{Path, PathBuf},
};

use blake3::{Hash, Hasher};
use chrono::{DateTime, Utc};

use crate::{error::Result, reader};

/// Create a single directory if it does not already exist.
///
/// Unlike `fs::create_dir_all`, this errors if the parent directory is missing
/// rather than silently creating the whole path. `AlreadyExists` is treated as
/// success.
pub fn create_dir_if_not_exists(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Content-addressed blob store.
///
/// Blobs are stored as flat files under `objects/ab/cdef...`
/// (two-level hex prefix, like Git loose objects).
pub(crate) struct BlobStore {
    root: PathBuf,
}

/// Lightweight metadata about a stored blob.
///
/// Deliberately small so that alternative backing storage can implement
/// `stat` without exposing filesystem-specific types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlobStat {
    pub(crate) size: u64,
    pub(crate) created_at: DateTime<Utc>,
}

impl BlobStore {
    /// Open an object store rooted at an existing `objects/` directory.
    ///
    /// The caller (`Bloxtree::open`) is responsible for creating `objects/`.
    /// This creates `objects/tmp/` if missing.
    pub(crate) fn open(root: &Path) -> Result<Self> {
        create_dir_if_not_exists(&root.join("tmp"))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Stream from a reader into a temp file, hash incrementally, and
    /// atomically rename into place. Returns `(hash, already_existed)`.
    pub(crate) fn add_reader(&self, reader: &mut impl io::Read) -> Result<(Hash, bool)> {
        // `objects/tmp/` is created once at `ObjectStore::open`; reuse it.
        let tmp_dir = self.root.join("tmp");
        let mut tmp_file = tempfile::NamedTempFile::new_in(&tmp_dir)?;

        let mut hasher = Hasher::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            io::Write::write_all(&mut tmp_file, &buf[..n])?;
        }

        let hash = hasher.finalize();
        let path = self.blob_path(hash);

        if path.exists() {
            // Already exists — temp file will be cleaned up on drop.
            return Ok((hash, true));
        }

        // Ensure prefix directory and atomically persist.
        if let Some(parent) = path.parent() {
            create_dir_if_not_exists(parent)?;
        }
        tmp_file.as_file().sync_all()?;
        match tmp_file.persist(&path) {
            Ok(_) => Ok((hash, false)),
            // Another process won the race and persisted the same blob.
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok((hash, true)),
            Err(e) => Err(e.error.into()),
        }
    }

    /// Open a blob for reading
    pub(crate) fn get_reader(&self, hash: Hash) -> Result<Option<impl io::Read + use<>>> {
        let blob_path = self.blob_path(hash);
        Ok(Some(reader::BloxtreeReader::new(blob_path, hash)?))
    }

    /// Delete a blob by hash. Silently succeeds if the blob doesn't exist.
    pub(crate) fn delete(&self, hash: Hash) -> Result<()> {
        let path = self.blob_path(hash);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Return metadata for the blob identified by `hash`.
    pub(crate) fn stat(&self, hash: Hash) -> Result<BlobStat> {
        let meta = fs::metadata(self.blob_path(hash))?;
        Ok(BlobStat {
            size: meta.len(),
            created_at: DateTime::<Utc>::from(meta.created()?),
        })
    }

    /// Return the filesystem path for a given hash.
    ///
    /// Backing-storage specific; private so the engine can't depend on the
    /// on-disk layout. Callers wanting blob facts use `stat` instead.
    fn blob_path(&self, hash: Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn write_and_read() {
        let dir = temp_dir();
        let store = BlobStore::open(dir.path()).unwrap();

        let data = b"hello world";
        let (hash, existed) = store.add_reader(&mut &data[..]).unwrap();
        assert!(!existed);

        let mut read_back = store.get_reader(hash).unwrap().unwrap();
        let mut buf = Vec::new();
        read_back.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn write_duplicate() {
        let dir = temp_dir();
        let store = BlobStore::open(dir.path()).unwrap();

        let data = b"hello world";
        let (hash1, existed1) = store.add_reader(&mut &data[..]).unwrap();
        assert!(!existed1);

        let (hash2, existed2) = store.add_reader(&mut &data[..]).unwrap();
        assert!(existed2);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn write_stream_and_read() {
        let dir = temp_dir();
        let store = BlobStore::open(dir.path()).unwrap();

        let data = b"streaming test data that is a bit longer";
        let (hash, existed) = store.add_reader(&mut &data[..]).unwrap();
        assert!(!existed);

        let mut read_back = store.get_reader(hash).unwrap().unwrap();
        let mut buf = Vec::new();
        read_back.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn delete_and_exists() {
        let dir = temp_dir();
        let store = BlobStore::open(dir.path()).unwrap();

        let data = b"to be deleted";
        let (hash, _) = store.add_reader(&mut &data[..]).unwrap();
        assert!(store.stat(hash).is_ok());

        store.delete(hash).unwrap();
        assert!(store.stat(hash).is_err());

        // Deleting again is a no-op.
        store.delete(hash).unwrap();
    }

    #[test]
    fn stat_reports_size() {
        let dir = temp_dir();
        let store = BlobStore::open(dir.path()).unwrap();

        let data = b"hello world";
        let (hash, _) = store.add_reader(&mut &data[..]).unwrap();

        let stat = store.stat(hash).unwrap();
        assert_eq!(stat.size, data.len() as u64);
        assert!(stat.created_at > DateTime::<Utc>::from(std::time::UNIX_EPOCH));
    }

    #[test]
    fn stat_missing_blob_errors() {
        let dir = temp_dir();
        let store = BlobStore::open(dir.path()).unwrap();

        let missing = blake3::hash(b"never written");
        assert!(store.stat(missing).is_err());
    }
}
