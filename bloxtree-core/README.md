# bloxtree-core — Implementation Plan

## Overview

`bloxtree-core` is the engine: a content-addressable store that maps virtual
paths to versioned blobs.

We use [`turso`](https://crates.io/crates/turso) (Turso's Rust rewrite of
SQLite) to store path → blob hash references, plus a hash index for reference
counting. The database is opened with `experimental_multiprocess_wal`, so
**multiple processes can open and write the same store concurrently** (e.g.
`bloxtree-cli add` while `bloxtree-fuse` has the store mounted).

Each write to a path creates a new version. Versions are ordered by a `uuid7`
(chronologically sortable) id. Versions are **not numbered explicitly** — they
are multiple rows per path, each with a different `uuid`.

The whole API is **asynchronous** (tokio-based).

For hashing we use BLAKE3 and simply re-export the `blake3` hash.

## API Surface

```rust
pub struct Bloxtree { … }

impl Bloxtree {
    /// Open (or create) a bloxtree store at `root`.
    /// Creates `root/objects` (blob store), `root/objects/tmp`, and
    /// `root/index.db` (turso database, multiprocess WAL enabled).
    /// Fails if `root` itself does not yet exist.
    pub async fn open<P: AsRef<Path>>(root: P) -> Result<Self>;

    /// Add from memory into a blob
    pub async fn add_bytes(&mut self, path: &str, data: &[u8]) -> Result<AddResult>;

    /// Read bytes into memory, return None if not found
    pub async fn get_bytes(&self, path: &str) -> Result<Option<Vec<u8>>>;

    /// Store from a reader, will store results in temporary storage
    /// and insert after full reader close.
    pub async fn add_reader(&mut self, path: &str, reader: &mut impl std::io::Read) -> Result<AddResult>;

    /// Retrieve the latest blob for a path.
    /// Returns None if the blob does not exist.
    pub async fn get_reader(&self, path: &str) -> Result<Option<impl std::io::Read>>;

    /// Remove a path or version of a path
    /// Hash is the specific hash link to remove, if None is given then all versions are removed
    /// A blob is deleted from storage when no path references it anymore.
    pub async fn remove_path(&mut self, path: &str, hash: Option<Hash>) -> Result<()>;

    /// Remove any version above max_versions
    /// if `max_versions` is 0, this will call remove.
    /// if `max_versions` is 1, only the latest version is kept.
    /// This method has a silent no-op on missing paths
    pub async fn trim_path(&mut self, path: &str, max_versions: u8) -> Result<()>;

    /// List all paths that start with, possibly empty, `prefix`.
    /// Returns a simulated directory listing for `/` based prefixes.
    /// It only returns a single level deep.
    pub async fn list_folder(&self, folder_name: &str) -> Result<Vec<CommonPrefixEntry>>;

    /// List all paths with their latest version
    pub async fn list_paths(&self) -> Result<Vec<PathEntry>>;

    /// Get a list of versions from new to old for a given path
    /// Returns an empty list if the path is not found
    pub async fn versions(&self, path: &str) -> Result<Vec<VersionInfo>>;

    /// Flush everything to disk and drop the connection.
    pub async fn shutdown(self) -> Result<()>;

    /// Stat the latest version of a path: size, creation time and refcount.
    pub async fn stat_path(&self, path: &str) -> Result<Option<PathStat>>;
}

pub struct AddResult {
    pub hash: Hash,                    // blake3 Hash
    pub uuid: Uuid,                    // uuid7 id of the new version
    pub blob_already_existed: bool,    // True if the blob already existed in storage
}

pub struct PathEntry {
    pub path: String,
    pub hash: Hash,
    pub uuid: Uuid,
}

pub enum CommonPrefixEntry {
    CommonPrefix { path: String },
    Path(PathEntry),
}

pub struct VersionInfo {
    pub hash: Hash,
    pub uuid: Uuid,
}

pub struct PathStat {
    pub hash: Hash,
    pub uuid: Uuid,
    pub size: u64,
    pub created_at: chrono::DateTime<Utc>,
    pub refcount: u32,
}
```

## On-Disk Layout

```
<root>/
├── objects/                  # Content-addressed blob store
│   ├── ab/
│   │   └── abcdef01...       # blob file named by full hex hash
│   └── cd/
│   │   ├── cdef0203...
│   │   └── cdef0405...
│   └── tmp/                  # Temp location for streaming writes
├── index.db                  # turso index database
├── index.db-wal              # write-ahead log (multiprocess WAL)
└── index.db-tshm             # multiprocess WAL shared-memory coordinator
```

- Objects are stored two-level: first 2 hex chars as directory, remainder as filename.
  This avoids too many entries in a single directory.
- Streaming writes go to a temp file first; on completion and hash verification,
  the file is atomically renamed into the correct `objects/` prefix. If a
  concurrent process persists the same blob first, the `AlreadyExists` race is
  treated as `blob_already_existed`.

## Index schema

The turso database contains a single table:

```sql
CREATE TABLE paths (
    path BLOB NOT NULL,
    uuid BLOB NOT NULL,     -- 16-byte uuid7, bytewise-ordered (chronological)
    hash BLOB NOT NULL,     -- 32-byte blake3 hash
    PRIMARY KEY (path, uuid)
);
CREATE INDEX idx_paths_hash ON paths(hash);
```

- **`paths`**: one row per (path, version). `path` is stored as a BLOB so
  comparisons are bytewise — the same ordering the old redb keys used.
- **Reference counts are computed**, not stored: the number of references to a
  blob is `SELECT COUNT(*) FROM paths WHERE hash = ?`, backed by
  `idx_paths_hash`. When a removal leaves a hash with zero references, the blob
  file is deleted from `objects/`.
- Prefix listing uses a byte-exact half-open range `path >= prefix AND path <
  prefix+1` on the BLOB column, preserving the old prefix-scan semantics.
- Versions for a path are ordered `uuid DESC` (uuid7 leading bytes are a
  millisecond timestamp), so the newest version sorts first.

## Path Rules

- Paths are logical identifiers, **not** filesystem paths. They follow `/`-separated
  segments like `"documents/report.txt"`.
- Paths must not be empty and must not contain `\x00`.
- Leading/trailing whitespace is rejected.
- Paths are case-sensitive, no normalization.

## Concurrency

`Bloxtree::open` always enables `experimental_multiprocess_wal(true)`. Every
opener must use the same mode; a process that opens the database without it
will be rejected. Writers are serialized across processes (SQLite takes a write
lock), readers never block writers, and checkpoints are coordinated through the
`-tshm` shared-memory file. The on-disk format is experimental (Turso).
