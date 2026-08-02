pub mod error;
pub(crate) mod index;
pub mod path;
pub(crate) mod reader;
pub(crate) mod store;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    path::Path,
};

use chrono::{DateTime, Utc};
use turso::Connection;

use crate::{error::Result, path::validate};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub use blake3::Hash;
pub use uuid::Uuid;

#[derive(Debug)]
pub struct AddResult {
    pub hash: Hash,
    pub uuid: Uuid,
    pub blob_already_existed: bool,
}

#[derive(Debug)]
pub struct PathEntry {
    pub path: String,
    pub hash: Hash,
    pub uuid: Uuid,
}

#[derive(Debug)]
pub enum CommonPrefixEntry {
    CommonPrefix { path: String },
    Path(PathEntry),
}

#[derive(Debug)]
pub struct VersionInfo {
    pub hash: Hash,
    pub uuid: Uuid,
}

#[derive(Debug, Clone, Copy)]
pub struct PathStat {
    pub hash: Hash,
    pub uuid: Uuid,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub refcount: u32,
}

// ---------------------------------------------------------------------------
// Bloxtree
// ---------------------------------------------------------------------------

pub struct Bloxtree {
    store: store::BlobStore,
    conn: Connection,
}

impl Bloxtree {
    pub async fn open<P: AsRef<Path>>(root: P) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.try_exists()? {
            return Err(crate::error::Error::Other(format!(
                "root directory does not exist: {}",
                root.display()
            )));
        }
        if !root.is_dir() {
            return Err(crate::error::Error::Other(format!(
                "root is not a directory: {}",
                root.display()
            )));
        }
        store::create_dir_if_not_exists(root.join("objects").as_path())?;
        let db_path = root.join("index.db");
        let db_path_str = db_path
            .to_str()
            .ok_or_else(|| crate::error::Error::Other("index path is not valid UTF-8".into()))?;
        let db = turso::Builder::new_local(db_path_str)
            .experimental_multiprocess_wal(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute_batch(index::SCHEMA).await?;
        let store = store::BlobStore::open(root.join("objects").as_path())?;
        Ok(Self { store, conn })
    }

    // ---------------------------------------------------------------------------
    // add
    // ---------------------------------------------------------------------------

    /// Single path convenience variant for add_bytes_to
    pub async fn add_bytes(&mut self, path: &str, data: &[u8]) -> Result<AddResult> {
        self.add_bytes_to(&[path], data).await
    }

    /// Add the given bytes to all paths in the bloxtree
    pub async fn add_bytes_to(&mut self, paths: &[&str], data: &[u8]) -> Result<AddResult> {
        let mut reader = io::Cursor::new(data);
        self.add_reader_to(paths, &mut reader).await
    }

    /// Single path convenience variant for add_reader_to
    pub async fn add_reader(
        &mut self,
        path: &str,
        reader: &mut impl io::Read,
    ) -> Result<AddResult> {
        self.add_reader_to(&[path], reader).await
    }

    ///Insert data into the bloxtree by consuming the given io::Read and registering at all paths
    pub async fn add_reader_to(
        &mut self,
        paths: &[&str],
        reader: &mut impl io::Read,
    ) -> Result<AddResult> {
        for path in paths {
            path::validate(path)?;
        }
        if paths.is_empty() {
            return Err(crate::error::Error::Other("no paths given".into()));
        }
        let (hash, already_existed) = self.store.add_reader(reader)?;
        self.add_paths_to_index(paths, hash, already_existed).await
    }

    async fn add_paths_to_index(
        &mut self,
        paths: &[&str],
        hash: Hash,
        already_existed: bool,
    ) -> Result<AddResult> {
        let uuid = Uuid::now_v7();
        index::insert_entries(&self.conn, paths, hash, uuid).await?;
        Ok(AddResult {
            hash,
            uuid,
            blob_already_existed: already_existed,
        })
    }

    // ---------------------------------------------------------------------------
    // get
    // ---------------------------------------------------------------------------
    #[allow(clippy::bind_instead_of_map)]
    pub async fn get_bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.get_reader(path)
            .await?
            .and_then(|mut reader| {
                let mut buf = Vec::new();
                Some(reader.read_to_end(&mut buf).map(|_| buf))
            })
            .transpose()
            .map_err(Into::into)
    }

    pub async fn get_reader(&self, path: &str) -> Result<Option<impl io::Read + use<>>> {
        path::validate(path)?;
        let result = index::get_latest(&self.conn, path).await?;
        match result {
            Some((hash, _)) => self.get_reader_by_hash(hash).await,
            None => Ok(None),
        }
    }

    pub async fn get_reader_by_hash(&self, hash: Hash) -> Result<Option<impl io::Read + use<>>> {
        self.store.get_reader(hash)
    }

    pub async fn remove_path(&mut self, path: &str, hash: Option<Hash>) -> Result<()> {
        path::validate(path)?;
        let deleted = match hash {
            Some(h) => {
                if index::delete_version(&self.conn, path, h).await? {
                    vec![h]
                } else {
                    vec![]
                }
            }
            None => index::delete_all_versions(&self.conn, path).await?,
        };
        for h in deleted {
            if index::ref_count(&self.conn, h).await? == 0 {
                self.store.delete(h)?;
            }
        }
        Ok(())
    }

    ///Removes all entries with the (folder_name + "/") prefix
    pub async fn remove_folder(&mut self, folder_name: &str) -> Result<()> {
        validate(folder_name)?;
        let entries =
            index::list_prefix_entries(&self.conn, (folder_name.to_owned() + "/").as_str()).await?;
        if entries.is_empty() {
            return Ok(());
        }
        for entry in entries {
            self.remove_path(&entry.path, None).await?;
        }
        Ok(())
    }

    pub async fn trim_path(&mut self, path: &str, max_versions: u8) -> Result<()> {
        path::validate(path)?;
        if max_versions == 0 {
            return self.remove_path(path, None).await;
        }
        let versions = index::list_versions(&self.conn, path).await?;
        if versions.len() <= max_versions as usize {
            return Ok(());
        }
        let mut deleted = Vec::new();
        for (hash, _) in &versions[max_versions as usize..] {
            if index::delete_version(&self.conn, path, *hash).await? {
                deleted.push(*hash);
            }
        }
        for h in deleted {
            if index::ref_count(&self.conn, h).await? == 0 {
                self.store.delete(h)?;
            }
        }
        Ok(())
    }
    ///List all paths that have the folder_name + "/" prefix
    pub async fn list_folder(&self, folder_name: &str) -> Result<Vec<CommonPrefixEntry>> {
        let mut prefix: String = folder_name.to_string();
        if !prefix.is_empty() {
            path::validate(prefix.as_str())?;
            prefix.push('/');
        };
        let entries = index::list_prefix_entries(&self.conn, prefix.as_str()).await?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut leaves: BTreeMap<String, (Hash, Uuid)> = BTreeMap::new();
        for entry in &entries {
            let rest = &entry.path[folder_name.len()..];
            let rest = rest.trim_start_matches('/');
            let comp = match rest.find('/') {
                Some(p) => &rest[..p],
                None => rest,
            };
            let full_comp = if folder_name.is_empty() {
                comp.to_string()
            } else if folder_name.ends_with('/') {
                format!("{folder_name}{comp}")
            } else {
                format!("{folder_name}/{comp}")
            };
            if rest == comp {
                let u = entry.uuid;
                leaves
                    .entry(full_comp.clone())
                    .and_modify(|(h, c)| {
                        if u > *c {
                            *h = entry.hash;
                            *c = u;
                        }
                    })
                    .or_insert((entry.hash, u));
            }
            *counts.entry(full_comp.clone()).or_insert(0) += 1;
        }
        let mut all: BTreeSet<String> = counts.keys().cloned().collect();
        all.extend(leaves.keys().cloned());
        Ok(all
            .into_iter()
            .map(|comp| {
                let cnt = counts.get(&comp).copied().unwrap_or(0);
                let leaf = leaves.contains_key(&comp);
                if cnt > 1 {
                    CommonPrefixEntry::CommonPrefix { path: comp }
                } else if leaf {
                    let (hash, uuid) = leaves[&comp];
                    CommonPrefixEntry::Path(PathEntry {
                        path: comp,
                        hash,
                        uuid,
                    })
                } else {
                    // Shouldn't happen but handle gracefully.
                    CommonPrefixEntry::CommonPrefix { path: comp }
                }
            })
            .collect())
    }

    pub async fn list_paths(&self) -> Result<Vec<PathEntry>> {
        let entries = index::all_entries(&self.conn).await?;
        let mut latest: BTreeMap<String, (Hash, Uuid)> = BTreeMap::new();
        for entry in &entries {
            latest
                .entry(entry.path.clone())
                .and_modify(|(h, c)| {
                    if entry.uuid > *c {
                        *h = entry.hash;
                        *c = entry.uuid;
                    }
                })
                .or_insert((entry.hash, entry.uuid));
        }
        Ok(latest
            .into_iter()
            .map(|(path, (hash, uuid))| PathEntry { path, hash, uuid })
            .collect())
    }

    pub async fn versions(&self, path: &str) -> Result<Vec<VersionInfo>> {
        path::validate(path)?;
        let versions = index::list_versions(&self.conn, path).await?;
        Ok(versions
            .into_iter()
            .map(|(hash, uuid)| VersionInfo { hash, uuid })
            .collect())
    }

    pub async fn shutdown(self) -> Result<()> {
        Ok(())
    }

    pub async fn stat_path(&self, path: &str) -> Result<Option<PathStat>> {
        path::validate(path)?;
        let Some((hash, uuid)) = index::get_latest(&self.conn, path).await? else {
            return Ok(None);
        };
        let blob = self.store.stat(hash)?;
        let refcount = index::ref_count(&self.conn, hash).await?;
        Ok(Some(PathStat {
            hash,
            uuid,
            size: blob.size,
            created_at: blob.created_at,
            refcount,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_bloxtree() -> (tempfile::TempDir, Bloxtree) {
        let dir = tempfile::TempDir::new().unwrap();
        let bt = Bloxtree::open(dir.path()).await.unwrap();
        (dir, bt)
    }

    #[tokio::test]
    async fn open_fails_on_nonexistent_root() {
        assert!(
            Bloxtree::open("/tmp/__bloxtree_nonexistent_dir_xyzzy__")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn add_and_get_bytes() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let r = bt
            .add_bytes("test/greeting.txt", b"hello world")
            .await
            .unwrap();
        assert!(!r.blob_already_existed);
        assert_eq!(
            bt.get_bytes("test/greeting.txt").await.unwrap().unwrap(),
            b"hello world"
        );
    }

    #[tokio::test]
    async fn get_bytes_missing() {
        let (_dir, bt) = temp_bloxtree().await;
        assert!(bt.get_bytes("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn add_duplicate_content() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let r1 = bt.add_bytes("path/a", b"hello").await.unwrap();
        assert!(!r1.blob_already_existed);
        let r2 = bt.add_bytes("path/b", b"hello").await.unwrap();
        assert!(r2.blob_already_existed);
        assert_eq!(r1.hash, r2.hash);
    }

    #[tokio::test]
    async fn remove_path_all_versions() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let r = bt.add_bytes("rm/me.txt", b"data").await.unwrap();
        bt.remove_path("rm/me.txt", None).await.unwrap();
        assert!(bt.get_bytes("rm/me.txt").await.unwrap().is_none());
        assert!(bt.store.stat(r.hash).is_err());
    }

    #[tokio::test]
    async fn remove_path_specific_version() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let v1 = bt.add_bytes("multi/ver.txt", b"v1").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        bt.add_bytes("multi/ver.txt", b"v2").await.unwrap();
        bt.remove_path("multi/ver.txt", Some(v1.hash))
            .await
            .unwrap();
        assert_eq!(bt.get_bytes("multi/ver.txt").await.unwrap().unwrap(), b"v2");
        assert!(bt.store.stat(v1.hash).is_err());
    }

    #[tokio::test]
    async fn trim_path_keeps_latest() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let r1 = bt.add_bytes("trim/test.txt", b"oldest").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let r2 = bt.add_bytes("trim/test.txt", b"middle").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        bt.add_bytes("trim/test.txt", b"newest").await.unwrap();
        bt.trim_path("trim/test.txt", 2).await.unwrap();
        assert_eq!(bt.versions("trim/test.txt").await.unwrap().len(), 2);
        assert_eq!(
            bt.get_bytes("trim/test.txt").await.unwrap().unwrap(),
            b"newest"
        );
        assert!(bt.store.stat(r1.hash).is_err());
        assert!(bt.store.stat(r2.hash).is_ok());
    }

    #[tokio::test]
    async fn trim_path_zero_removes_all() {
        let (_dir, mut bt) = temp_bloxtree().await;
        bt.add_bytes("trim/zero.txt", b"data").await.unwrap();
        bt.trim_path("trim/zero.txt", 0).await.unwrap();
        assert!(bt.get_bytes("trim/zero.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn trim_path_missing_is_noop() {
        let (_dir, mut bt) = temp_bloxtree().await;
        assert!(bt.trim_path("nonexistent", 5).await.is_ok());
    }

    #[tokio::test]
    async fn versions_newest_first() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let r1 = bt.add_bytes("ver/order.txt", b"first").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let r2 = bt.add_bytes("ver/order.txt", b"second").await.unwrap();
        let v = bt.versions("ver/order.txt").await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].hash, r2.hash);
        assert_eq!(v[1].hash, r1.hash);
    }

    #[tokio::test]
    async fn versions_missing_is_empty() {
        let (_dir, bt) = temp_bloxtree().await;
        assert!(bt.versions("nonexistent").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_content_adds_do_not_collide() {
        let (_dir, mut bt) = temp_bloxtree().await;
        let r1 = bt.add_bytes("collide/x.txt", b"same").await.unwrap();
        let r2 = bt.add_bytes("collide/x.txt", b"same").await.unwrap();
        assert_ne!(r1.uuid, r2.uuid);
        let v = bt.versions("collide/x.txt").await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].hash, v[1].hash);
    }

    #[tokio::test]
    async fn list_prefix_root() {
        let (_dir, mut bt) = temp_bloxtree().await;
        bt.add_bytes("a/b/x.txt", b"x").await.unwrap();
        bt.add_bytes("a/b/y.txt", b"y").await.unwrap();
        bt.add_bytes("a/c.txt", b"c").await.unwrap();
        bt.add_bytes("d.txt", b"d").await.unwrap();
        let entries = bt.list_folder("").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], CommonPrefixEntry::CommonPrefix { path } if path == "a"));
        assert!(matches!(&entries[1], CommonPrefixEntry::Path(_)));
    }

    #[tokio::test]
    async fn list_prefix_subdir() {
        let (_dir, mut bt) = temp_bloxtree().await;
        bt.add_bytes("a/b/x.txt", b"x").await.unwrap();
        bt.add_bytes("a/b/y.txt", b"y").await.unwrap();
        let entries = bt.list_folder("a/b").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], CommonPrefixEntry::Path(_)));
        assert!(matches!(entries[1], CommonPrefixEntry::Path(_)));
    }

    #[tokio::test]
    async fn list_paths_unique() {
        let (_dir, mut bt) = temp_bloxtree().await;
        bt.add_bytes("one.txt", b"1").await.unwrap();
        bt.add_bytes("two.txt", b"2").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        bt.add_bytes("one.txt", b"1v2").await.unwrap();
        let paths = bt.list_paths().await.unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn invalid_paths_rejected() {
        let (_dir, mut bt) = temp_bloxtree().await;
        assert!(bt.add_bytes("", b"data").await.is_err());
        assert!(bt.add_bytes(" x", b"data").await.is_err());
        assert!(bt.add_bytes("x ", b"data").await.is_err());
    }

    #[tokio::test]
    async fn concurrent_open_and_write_same_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut bt1 = Bloxtree::open(dir.path()).await.unwrap();
        let mut bt2 = Bloxtree::open(dir.path()).await.unwrap();

        bt1.add_bytes("a.txt", b"from one").await.unwrap();
        bt2.add_bytes("b.txt", b"from two").await.unwrap();
        assert_eq!(bt1.get_bytes("a.txt").await.unwrap().unwrap(), b"from one");
        assert_eq!(bt2.get_bytes("a.txt").await.unwrap().unwrap(), b"from one");
        assert_eq!(bt1.get_bytes("b.txt").await.unwrap().unwrap(), b"from two");
        assert_eq!(bt1.list_paths().await.unwrap().len(), 2);
    }
}
