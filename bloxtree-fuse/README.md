# bloxtree-fuse

Mount a [bloxtree](../bloxtree-core) content-addressable store as a local
filesystem via [FUSE](https://en.wikipedia.org/wiki/Filesystem_in_Userspace).

## Usage

```
bloxtree-fuse --store <STORE> <MOUNTPOINT>
```

Mounts the bloxtree at `--store` (or `$BLOXTREE_STORE`, or
`$XDG_DATA_HOME/bloxtree`) onto `<MOUNTPOINT>`.  `<MOUNTPOINT>` must be an
empty directory.  Unmount with `fusermount3 -u <MOUNTPOINT>`.

```
bloxtree-fuse [OPTIONS] <MOUNTPOINT>

Options:
  --store <PATH>   Path to the bloxtree store root [env: BLOXTREE_STORE]
  -h, --help       Print help
```

Once mounted, the tree mirrors the logical paths stored in bloxtree:

```
$ ls /mnt/blox/
documents/  readme.txt

$ cat /mnt/blox/readme.txt
hello world

$ ls /mnt/blox/documents/
report.txt  notes.txt
```

## Filesystem mapping

Every path in the store is a regular file whose contents are the latest version
of that blob.  Directories are implied by `/`-separated path prefixes — there
is no separate "create directory" operation.

| Bloxtree path | Mounted entry |
|---|---|
| `readme.txt` | `/mnt/blox/readme.txt` — regular file |
| `documents/report.txt` | `/mnt/blox/documents/report.txt` — regular file; `documents/` is a directory |
| `documents/notes.txt` | `/mnt/blox/documents/notes.txt` — regular file inside the same `documents/` directory |

### Files

- **Contents**: the blob data of the **latest version** of the path.
- **Size**: the byte length of that blob.
- **Timestamp**: the blob's creation time, surfaced via
  `stat_path` → `PathStat.created_at` (the file's birth time).  This is used
  for atime, mtime, ctime and crtime alike.
- **Permissions**: `0644` (read/write for owner, read for group/other).
- **Hard links**: always 1.

### Directories

- Populated from `list_folder` — one level of immediate children.
- Entries are `.`, `..`, plus:
  - directory entries for common prefixes (trailing `/`)
  - file entries for leaf paths
- **Timestamp**: always the Unix epoch (directories are virtual; there is no
  per-directory metadata).
- **Permissions**: `0755`.
- **Read**: reading a directory (e.g. `ls`) lists its children.  Directories
  are not regular files and have no readable contents.
- Directories are dynamic — they reflect whatever paths are currently in the
  store.  There is no need to "mkdir" or "rmdir".

### Conflicting paths

A path can be both a leaf blob **and** a common prefix for other paths
(e.g. `a.jpg` has its own blob, and `a.jpg/1.jpg` also exists).  In a POSIX
filesystem an inode cannot be both a regular file and a directory
simultaneously.  When this conflict occurs **the directory takes precedence**
and the leaf blob is exposed with an **underscore prefix**: `a.jpg` appears as
a directory, and the blob that was stored at `a.jpg` is accessible as
`_a.jpg`.

```
# Store contains:
#   photos/a.jpg          (a blob — a single photo)
#   photos/a.jpg/1.jpg    (a sub-entry — the directory has children)

$ ls /mnt/blox/photos/
a.jpg/              # the directory (conflict: directory wins)
_a.jpg              # the leaf blob (underscore-prefixed)
```

`lookup` also accepts a double-underscore form (`__a.jpg`) as an alias for the
shadowed leaf, for the rare case that `_a.jpg` is itself a real stored path.

## Write support

The filesystem is **read-write**.  Writes translate to `bloxtree-core`
operations:

| Operation | Behaviour |
|---|---|
| `creat` + `write` + `release` | Creates a new version of the path.  The kernel may issue multiple `write` calls before `release`; the driver buffers them in a per-handle `Vec`.  On `release` the accumulated data is committed via `add_bytes`. |
| `open` + `write` + `release` | The same buffered-write path for existing files; the buffer is flushed to a new version on `release`. |
| `unlink` | Calls `remove_path(path, None)` — deletes **all versions** of the path.  If the path is also a prefix for other entries, the directory remains with its remaining children. |
| `rename` | Adds the blob at the new path and removes the old path.  If the source blob has multiple versions, the latest is copied; the old path is fully removed. |
| `truncate` | Truncates the latest version to the requested size (the first `size` bytes are kept, via `reader.take(size)`). |
| `mkdir` | Creates an ephemeral directory in the FUSE process — never written to the core.  `mkdir -p a/b` allows organizing before writing files.  POSIX: returns `EEXIST` if the path already exists as a file or directory. |
| `rmdir` | Removes only ephemeral (mkdir'd) directories.  Refuses with `ENOTEMPTY` if the directory has children (ephemeral or core).  Virtual directories that exist only as common prefixes of core paths cannot be removed — they disappear when their last child is removed. |

- **No atomic cross-path moves.**  If a path has multiple versions, rename only
  copies the latest blob to the new name; historical versions are lost at the
  source.
- **Write conflicts.**  `release` (and therefore `add_bytes`) happens under the
  FUSE session's single-threaded dispatch, so there are no internal races.

## Architecture

`bloxtree-fuse` is a Rust library that provides a `mount` function:

```rust
pub fn mount<P: AsRef<Path>>(store: &Path, mountpoint: P) -> io::Result<()>;
```

`mount` opens the store at `store` and blocks until the filesystem is unmounted.
It creates a dedicated tokio runtime internally: `bloxtree-core` is fully async,
but the fuser `Filesystem` trait is synchronous, so each handler drives its
async call through `Runtime::block_on` on the (single) FUSE session thread. The
library spawns a FUSE session via the [`fuser`](https://crates.io/crates/fuser)
crate (Linux-only), implementing the required VFS operations (`lookup`,
`getattr`, `readdir`, `read`, `write`, `create`, `open`, `release`, `unlink`,
`rename`, `setattr`).

The FUSE trait dispatches all calls through `&self`, so the driver wraps a
`FsState` struct — holding the `Bloxtree`, the inode map (`ino → path`), and the
open-handle write buffers — in an `Arc<Mutex<FsState>>`, alongside the tokio
runtime handle. Because the store is opened with turso's multiprocess WAL, other
processes (`bloxtree-cli`) can read and write the same store while this
filesystem is mounted.

Buffered writes: `write` calls append to a per-open-handle `Vec<u8>`.  On
`release` the buffer is flushed via `add_bytes`.

Read-ahead, caching, and writeback are left to the kernel's VFS layer.  The
driver does not implement its own cache — `read` delegates to
`bloxtree-core`'s `get_bytes` and buffers the returned range for `reply.data`.

## Unmounting

```
fusermount3 -u <MOUNTPOINT>
```

There is no `auto_unmount`: if the process exits without unmounting, the
mountpoint goes stale and must be released with `fusermount3 -u`.

## Limitations

- **Linux-only** (`fuser` crate).  Other `libfuse` platforms (macOS, FreeBSD)
  may follow.
- **Single-threaded dispatch.**  One FUSE operation at a time.  Sufficient for
  lightweight interactive use; not designed for high-throughput parallel
  workloads.
- **No partial version removal via mount.**  `unlink` removes all versions.
  Use `bloxtree-cli remove --hash` for surgical version deletion.
- **Only `setattr` truncation is honored.**  `chmod` / `chown` and explicit
  timestamp updates are ignored — metadata is virtual.
- **Rename may lose version history.**  Only the latest version moves to the
  new name.
- **`read` buffers whole blobs.**  Each read fetches the full blob through
  `get_bytes` and slices the requested range; large files are held in memory
  briefly.
