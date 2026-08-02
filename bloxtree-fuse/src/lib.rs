use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsStr,
    hash::Hasher,
    io::{self, Read},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bloxtree_core::Bloxtree;
use chrono::{DateTime, Utc};
use fuser::{
    BsdFileFlags, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner,
    MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Open the store at `store` and mount it at `mountpoint`, blocking until the
/// filesystem is unmounted.
///
/// A dedicated tokio runtime is created internally: the async
/// `bloxtree-core` API is driven from fuser's synchronous callbacks via
/// `Runtime::block_on` on the (single) FUSE session thread.
pub fn mount<P: AsRef<Path>>(store: &Path, mountpoint: P) -> io::Result<()> {
    let rt = Arc::new(Runtime::new().map_err(|e| io::Error::other(e.to_string()))?);
    let bt = rt
        .block_on(Bloxtree::open(store))
        .map_err(|e| io::Error::other(e.to_string()))?;
    let fs = BloxtreeFs::new(bt, rt);
    let mut options = fuser::Config::default();
    options.mount_options = vec![MountOption::FSName("bloxtree".into())];
    fuser::mount(fs, mountpoint, &options)
}

// ---------------------------------------------------------------------------
// Inode hashing
// ---------------------------------------------------------------------------

fn make_ino(path: &str, kind: FileType) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(path, &mut h);
    std::hash::Hash::hash(&(kind as u8), &mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Inode metadata
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct InodeInfo {
    path: String,
    kind: FileType,
}

// ---------------------------------------------------------------------------
// Filesystem state
// ---------------------------------------------------------------------------

struct FsState {
    bt: Bloxtree,
    ino_info: HashMap<u64, InodeInfo>,
    ephemeral_dirs: BTreeSet<String>,
    write_bufs: HashMap<u64, (u64, Vec<u8>)>,
    next_fh: u64,
}

impl FsState {
    fn register_ino(&mut self, path: &str, kind: FileType) -> u64 {
        let ino = if path.is_empty() && kind == FileType::Directory {
            1
        } else {
            make_ino(path, kind)
        };
        self.ino_info.entry(ino).or_insert_with(|| InodeInfo {
            path: path.to_string(),
            kind,
        });
        ino
    }

    fn remove_ino(&mut self, path: &str, kind: FileType) {
        self.ino_info.remove(&make_ino(path, kind));
    }

    fn alloc_fh(&mut self, ino: u64) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.write_bufs.insert(fh, (ino, Vec::new()));
        fh
    }

    fn child_path(parent: &str, name: &str) -> String {
        match parent {
            "" => name.to_string(),
            p => format!("{p}/{name}"),
        }
    }
}

// ---------------------------------------------------------------------------
// BloxtreeFs
// ---------------------------------------------------------------------------

struct BloxtreeFs {
    state: Arc<Mutex<FsState>>,
    rt: Arc<Runtime>,
}

impl BloxtreeFs {
    fn new(bt: Bloxtree, rt: Arc<Runtime>) -> Self {
        let mut state = FsState {
            bt,
            ino_info: HashMap::new(),
            ephemeral_dirs: BTreeSet::new(),
            write_bufs: HashMap::new(),
            next_fh: 1,
        };
        state.register_ino("", FileType::Directory);
        Self {
            state: Arc::new(Mutex::new(state)),
            rt,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FsState> {
        self.state.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Attribute helpers
// ---------------------------------------------------------------------------

fn dir_attr(ino: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: 4096,
        blocks: 8,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn file_attr(ino: u64, size: u64, ts: SystemTime) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
        atime: ts,
        mtime: ts,
        ctime: ts,
        crtime: ts,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn system_time_from_datetime(dt: DateTime<Utc>) -> SystemTime {
    let nanos = dt.timestamp_nanos_opt().unwrap_or(0);
    if nanos >= 0 {
        UNIX_EPOCH + Duration::from_nanos(nanos as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos((-nanos) as u64)
    }
}

fn file_attr_for_path(rt: &Runtime, state: &FsState, ino: u64, path: &str) -> io::Result<FileAttr> {
    let st = rt
        .block_on(async { state.bt.stat_path(path).await })
        .map_err(|e| io::Error::other(e.to_string()))?
        .ok_or(io::ErrorKind::NotFound)?;
    let ts = system_time_from_datetime(st.created_at);
    Ok(file_attr(ino, st.size, ts))
}

// ---------------------------------------------------------------------------
// FUSE trait impl
// ---------------------------------------------------------------------------

impl Filesystem for BloxtreeFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut state = self.lock();
        let name = name.to_str().unwrap_or("");

        let parent_path = match parent.0 {
            1 => String::new(),
            _ => match state.ino_info.get(&parent.0) {
                Some(info) => info.path.clone(),
                None => {
                    reply.error(fuser::Errno::ENOENT);
                    return;
                }
            },
        };

        let (real_path, kind) = match resolve_child(&self.rt, &state, &parent_path, name) {
            Some(r) => r,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };

        let ino = state.register_ino(&real_path, kind);

        let attr = match kind {
            FileType::Directory => dir_attr(ino),
            FileType::RegularFile => file_attr_for_path(&self.rt, &state, ino, &real_path)
                .unwrap_or(file_attr(ino, 0, UNIX_EPOCH)),
            _ => dir_attr(ino),
        };

        reply.entry(&Duration::ZERO, &attr, fuser::Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let state = self.lock();
        let info = match state.ino_info.get(&ino.0) {
            Some(i) => i.clone(),
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };

        let attr = match info.kind {
            FileType::Directory => dir_attr(ino.0),
            FileType::RegularFile => file_attr_for_path(&self.rt, &state, ino.0, &info.path)
                .unwrap_or(file_attr(ino.0, 0, UNIX_EPOCH)),
            _ => dir_attr(ino.0),
        };

        reply.attr(&Duration::ZERO, &attr);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut state = self.lock();

        let parent_path = match ino.0 {
            1 => String::new(),
            _ => match state.ino_info.get(&ino.0) {
                Some(i) => i.path.clone(),
                None => {
                    reply.error(fuser::Errno::ENOENT);
                    return;
                }
            },
        };

        let prefix = if parent_path.is_empty() {
            String::new()
        } else {
            parent_path.clone()
        };

        let entries = match self
            .rt
            .block_on(async { state.bt.list_folder(&prefix).await })
        {
            Ok(e) => e,
            Err(_) => {
                reply.error(fuser::Errno::EIO);
                return;
            }
        };

        struct DirEntry {
            name: String,
            path: String,
            kind: FileType,
        }
        let mut dir_entries: Vec<DirEntry> = Vec::new();

        for entry in &entries {
            match entry {
                bloxtree_core::CommonPrefixEntry::CommonPrefix { path: full } => {
                    let name = full.rsplit('/').next().unwrap_or(full).to_string();
                    dir_entries.push(DirEntry {
                        name,
                        path: full.clone(),
                        kind: FileType::Directory,
                    });

                    let has_leaf = self
                        .rt
                        .block_on(async { state.bt.versions(full).await })
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                    if has_leaf {
                        let underscore_name = format!("_{}", dir_entries.last().unwrap().name);
                        dir_entries.push(DirEntry {
                            name: underscore_name,
                            path: full.clone(),
                            kind: FileType::RegularFile,
                        });
                    }
                }
                bloxtree_core::CommonPrefixEntry::Path(pe) => {
                    let name = pe.path.rsplit('/').next().unwrap_or(&pe.path).to_string();
                    dir_entries.push(DirEntry {
                        name,
                        path: pe.path.clone(),
                        kind: FileType::RegularFile,
                    });
                }
            }
        }

        {
            let mut seen: BTreeSet<String> = dir_entries.iter().map(|d| d.name.clone()).collect();
            for child in &state.ephemeral_dirs {
                if !is_direct_child(child, &parent_path) {
                    continue;
                }
                let name = child.rsplit('/').next().unwrap_or(child).to_string();
                if seen.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());
                dir_entries.push(DirEntry {
                    name: name.clone(),
                    path: child.clone(),
                    kind: FileType::Directory,
                });

                let underscore_name = format!("_{name}");
                if !seen.contains(&underscore_name) {
                    let has_leaf = self
                        .rt
                        .block_on(async { state.bt.versions(child).await })
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                    if has_leaf {
                        seen.insert(underscore_name.clone());
                        dir_entries.push(DirEntry {
                            name: underscore_name,
                            path: child.clone(),
                            kind: FileType::RegularFile,
                        });
                    }
                }
            }
        }

        dir_entries.insert(
            0,
            DirEntry {
                name: ".".into(),
                path: parent_path.clone(),
                kind: FileType::Directory,
            },
        );
        dir_entries.insert(
            1,
            DirEntry {
                name: "..".into(),
                path: String::new(),
                kind: FileType::Directory,
            },
        );

        for (i, de) in dir_entries.iter().enumerate().skip(offset as usize) {
            let entry_ino = state.register_ino(&de.path, de.kind);
            if reply.add(
                INodeNo(entry_ino),
                (i + 1) as u64,
                de.kind,
                de.name.as_str(),
            ) {
                break;
            }
        }

        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let state = self.lock();
        let info = match state.ino_info.get(&ino.0) {
            Some(i) => i.clone(),
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };

        let data = match self
            .rt
            .block_on(async { state.bt.get_bytes(&info.path).await })
        {
            Ok(Some(d)) => d,
            _ => {
                reply.error(fuser::Errno::EIO);
                return;
            }
        };

        let start = offset as usize;
        if start >= data.len() {
            reply.data(&[]);
        } else {
            let end = (start + size as usize).min(data.len());
            reply.data(&data[start..end]);
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut state = self.lock();
        let buf = state
            .write_bufs
            .entry(fh.0)
            .or_insert_with(|| (ino.0, Vec::new()));
        let off = offset as usize;
        if off + data.len() > buf.1.len() {
            buf.1.resize(off + data.len(), 0);
        }
        buf.1[off..off + data.len()].copy_from_slice(data);
        reply.written(data.len() as u32);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let mut state = self.lock();
        let name = name.to_str().unwrap_or("");

        let parent_path = match parent.0 {
            1 => String::new(),
            _ => match state.ino_info.get(&parent.0) {
                Some(info) => info.path.clone(),
                None => {
                    reply.error(fuser::Errno::ENOENT);
                    return;
                }
            },
        };

        let real_path = if name.starts_with('_') {
            let stripped = name.trim_start_matches('_');
            FsState::child_path(&parent_path, stripped)
        } else {
            FsState::child_path(&parent_path, name)
        };

        let ino = state.register_ino(&real_path, FileType::RegularFile);
        let fh = state.alloc_fh(ino);

        reply.created(
            &Duration::ZERO,
            &file_attr(ino, 0, UNIX_EPOCH),
            fuser::Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut state = self.lock();

        if let Some((_buf_ino, buf)) = state.write_bufs.remove(&fh.0) {
            let path = state.ino_info.get(&ino.0).map(|i| i.path.clone());
            if let Some(path) = path {
                let _ = self
                    .rt
                    .block_on(async { state.bt.add_bytes(&path, &buf).await });
            }
        }

        reply.ok();
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut state = self.lock();
        let name = name.to_str().unwrap_or("");

        let parent_path = match parent.0 {
            1 => String::new(),
            _ => match state.ino_info.get(&parent.0) {
                Some(info) => info.path.clone(),
                None => {
                    reply.error(fuser::Errno::ENOENT);
                    return;
                }
            },
        };

        let real_path = if name.starts_with('_') {
            let stripped = name.trim_start_matches('_');
            FsState::child_path(&parent_path, stripped)
        } else {
            FsState::child_path(&parent_path, name)
        };

        match self
            .rt
            .block_on(async { state.bt.remove_path(&real_path, None).await })
        {
            Ok(()) => {
                state.remove_ino(&real_path, FileType::RegularFile);
                reply.ok();
            }
            Err(_) => reply.error(fuser::Errno::EIO),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mut state = self.lock();
        let name = name.to_str().unwrap_or("");
        let newname = newname.to_str().unwrap_or("");

        let get_parent_path = |p: u64, state: &FsState| -> Option<String> {
            match p {
                1 => Some(String::new()),
                _ => state.ino_info.get(&p).map(|i| i.path.clone()),
            }
        };

        let parent_path = match get_parent_path(parent.0, &state) {
            Some(p) => p,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        let new_parent_path = match get_parent_path(newparent.0, &state) {
            Some(p) => p,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };

        let old_path = if name.starts_with('_') {
            FsState::child_path(&parent_path, name.trim_start_matches('_'))
        } else {
            FsState::child_path(&parent_path, name)
        };

        let new_path = if newname.starts_with('_') {
            FsState::child_path(&new_parent_path, newname.trim_start_matches('_'))
        } else {
            FsState::child_path(&new_parent_path, newname)
        };

        let mut reader = match self
            .rt
            .block_on(async { state.bt.get_reader(&old_path).await })
        {
            Ok(Some(r)) => r,
            _ => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };

        if self
            .rt
            .block_on(async { state.bt.add_reader(&new_path, &mut reader).await })
            .is_err()
        {
            reply.error(fuser::Errno::EIO);
            return;
        }
        drop(reader);

        let _ = self
            .rt
            .block_on(async { state.bt.remove_path(&old_path, None).await });
        state.remove_ino(&old_path, FileType::RegularFile);

        reply.ok();
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut state = self.lock();

        let info = match state.ino_info.get(&ino.0) {
            Some(i) => i.clone(),
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };

        if let Some(new_size) = size {
            let Some(reader) = self
                .rt
                .block_on(async { state.bt.get_reader(&info.path).await })
                .ok()
                .flatten()
            else {
                reply.error(fuser::Errno::EIO);
                return;
            };
            let _ = self.rt.block_on(async {
                state
                    .bt
                    .add_reader(&info.path, &mut reader.take(new_size))
                    .await
            });
        }

        // Ignore mode changes (virtual fs)
        let _ = mode;

        let attr = match info.kind {
            FileType::Directory => dir_attr(ino.0),
            FileType::RegularFile => file_attr_for_path(&self.rt, &state, ino.0, &info.path)
                .unwrap_or(file_attr(ino.0, 0, UNIX_EPOCH)),
            _ => dir_attr(ino.0),
        };

        reply.attr(&Duration::ZERO, &attr);
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let mut state = self.lock();
        let name = name.to_str().unwrap_or("");

        let parent_path = match parent.0 {
            1 => String::new(),
            _ => match state.ino_info.get(&parent.0) {
                Some(info) => info.path.clone(),
                None => {
                    reply.error(fuser::Errno::ENOENT);
                    return;
                }
            },
        };

        let real_path = FsState::child_path(&parent_path, name);

        // POSIX: EEXIST if the path already exists as a dir or file.
        if resolve_child(&self.rt, &state, &parent_path, name).is_some() {
            reply.error(fuser::Errno::EEXIST);
            return;
        }

        state.ephemeral_dirs.insert(real_path.clone());
        let ino = state.register_ino(&real_path, FileType::Directory);
        reply.entry(&Duration::ZERO, &dir_attr(ino), fuser::Generation(0));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut state = self.lock();
        let name = name.to_str().unwrap_or("");

        let parent_path = match parent.0 {
            1 => String::new(),
            _ => match state.ino_info.get(&parent.0) {
                Some(info) => info.path.clone(),
                None => {
                    reply.error(fuser::Errno::ENOENT);
                    return;
                }
            },
        };

        let real_path = FsState::child_path(&parent_path, name);

        // Only explicitly mkdir'd dirs are removable; virtual/derived dirs
        // are not owned by the FUSE layer.
        if !state.ephemeral_dirs.contains(&real_path) {
            reply.error(fuser::Errno::ENOENT);
            return;
        }

        // ENOTEMPTY if any ephemeral child or any core child remains.
        let has_ephemeral_child = state
            .ephemeral_dirs
            .range(real_path.clone() + "/"..)
            .next()
            .is_some_and(|next| next.starts_with(&(real_path.as_str().to_owned() + "/")));
        if has_ephemeral_child {
            reply.error(fuser::Errno::ENOTEMPTY);
            return;
        }
        let has_core_child = self
            .rt
            .block_on(async { state.bt.list_folder(&real_path).await })
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if has_core_child {
            reply.error(fuser::Errno::ENOTEMPTY);
            return;
        }

        state.ephemeral_dirs.remove(&real_path);
        state.remove_ino(&real_path, FileType::Directory);
        reply.ok();
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

fn resolve_child(
    rt: &Runtime,
    state: &FsState,
    parent_path: &str,
    name: &str,
) -> Option<(String, FileType)> {
    let (candidate, is_underscore) = if let Some(stripped) = name.strip_prefix('_') {
        if let Some(stripped2) = stripped.strip_prefix('_') {
            (FsState::child_path(parent_path, stripped2), true)
        } else {
            (FsState::child_path(parent_path, stripped), true)
        }
    } else {
        (FsState::child_path(parent_path, name), false)
    };

    let has_leaf = rt
        .block_on(async { state.bt.versions(&candidate).await })
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let core_has_children = rt
        .block_on(async { state.bt.list_folder(&candidate).await })
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let is_ephemeral = state.ephemeral_dirs.contains(&candidate);
    let has_children = core_has_children || is_ephemeral;

    if !has_leaf && !has_children {
        return None;
    }

    if is_underscore {
        if has_leaf && has_children {
            return Some((candidate, FileType::RegularFile));
        }
        return None;
    }

    if has_children {
        Some((candidate, FileType::Directory))
    } else {
        Some((candidate, FileType::RegularFile))
    }
}

fn is_direct_child(candidate: &str, parent: &str) -> bool {
    if parent.is_empty() {
        return !candidate.is_empty() && !candidate.contains('/');
    }
    candidate
        .strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_child_root() {
        assert!(is_direct_child("a", ""));
        assert!(is_direct_child("ab", ""));
        assert!(!is_direct_child("a/b", ""));
        assert!(!is_direct_child("", ""));
    }

    #[test]
    fn direct_child_subdir() {
        assert!(is_direct_child("a/b", "a"));
        assert!(!is_direct_child("a/b", ""));
        assert!(!is_direct_child("a/b/c", "a"));
        assert!(is_direct_child("a/b/c", "a/b"));
        assert!(!is_direct_child("ab", "a"));
        assert!(is_direct_child("a/bc", "a"));
    }

    #[test]
    fn direct_child_no_match() {
        assert!(!is_direct_child("x/y", "a"));
        assert!(!is_direct_child("a", "a"));
    }
}
