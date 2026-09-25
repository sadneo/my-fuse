use clap::Parser;
use fuser::{
    FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request,
};

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use fuser::Config;
use fuser::MountOption;
use fuser::SessionACL;

#[derive(Parser)]
#[command(version)]
pub struct Args {
    pub mount_point: PathBuf,

    /// Automatically unmount on process exit
    #[clap(long)]
    pub auto_unmount: bool,

    /// Allow root user to access filesystem
    #[clap(long)]
    pub allow_root: bool,

    /// Number of threads to use
    #[clap(long, default_value_t = 1)]
    pub n_threads: usize,

    /// Use `FUSE_DEV_IOC_CLONE` to give each worker thread its own fd.
    /// This enables more efficient request processing
    /// when multiple threads are used. Requires Linux 4.5+.
    #[clap(long)]
    pub clone_fd: bool,
}

impl Args {
    pub fn config(&self) -> Config {
        let mut config = Config::default();
        if self.auto_unmount {
            config.mount_options.push(MountOption::AutoUnmount);
        }
        if self.allow_root {
            config.acl = SessionACL::RootAndOwner;
        }
        if config.mount_options.contains(&MountOption::AutoUnmount)
            && config.acl != SessionACL::RootAndOwner
        {
            config.acl = SessionACL::All;
        }
        config.n_threads = Some(self.n_threads);
        config.clone_fd = self.clone_fd;
        config
    }
}

const HELLO: INodeNo = INodeNo(2);

struct Node {
    kind: FileType,
    perm: u16,
    uid: u32,
    gid: u32,
    contents: Vec<u8>,
    children: BTreeMap<OsString, INodeNo>,
    atime: SystemTime,
    mtime: SystemTime,
    ctime: SystemTime,
}

struct FsState {
    next_ino: u64,
    inodes: HashMap<INodeNo, Node>,
}

struct NullFS {
    state: RwLock<FsState>,
}

impl NullFS {
    fn new() -> Self {
        let now = SystemTime::now();
        let mut inodes = HashMap::new();
        inodes.insert(
            INodeNo::ROOT,
            Node {
                kind: FileType::Directory,
                perm: 0o755,
                uid: 0,
                gid: 0,
                contents: Vec::new(),
                children: BTreeMap::from([(OsString::from("hello.txt"), HELLO)]),
                atime: now,
                mtime: now,
                ctime: now,
            },
        );
        inodes.insert(
            HELLO,
            Node {
                kind: FileType::RegularFile,
                perm: 0o644,
                uid: 0,
                gid: 0,
                contents: b"hello world".to_vec(),
                children: BTreeMap::new(),
                atime: now,
                mtime: now,
                ctime: now,
            },
        );

        Self {
            state: RwLock::new(FsState {
                next_ino: HELLO.0 + 1,
                inodes,
            }),
        }
    }

    fn attr(ino: INodeNo, node: &Node) -> FileAttr {
        FileAttr {
            ino,
            size: node.contents.len() as u64,
            blocks: 0,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
            crtime: SystemTime::UNIX_EPOCH,
            kind: node.kind,
            perm: node.perm,
            nlink: if node.kind == FileType::Directory {
                2
            } else {
                1
            },
            uid: node.uid,
            gid: node.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn create_node(
        &self,
        parent: INodeNo,
        name: &OsStr,
        kind: FileType,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<FileAttr, fuser::Errno> {
        if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
            return Err(fuser::Errno::EINVAL);
        }

        let mut state = self.state.write().unwrap();
        let parent_node = state.inodes.get(&parent).ok_or(fuser::Errno::ENOENT)?;
        if parent_node.kind != FileType::Directory {
            return Err(fuser::Errno::ENOTDIR);
        }
        if parent_node.children.contains_key(name) {
            return Err(fuser::Errno::EEXIST);
        }

        let ino = INodeNo(state.next_ino);
        state.next_ino += 1;
        let now = SystemTime::now();
        let node = Node {
            kind,
            perm: (mode as u16 & 0o7777) & !(umask as u16),
            uid,
            gid,
            contents: Vec::new(),
            children: BTreeMap::new(),
            atime: now,
            mtime: now,
            ctime: now,
        };
        let attr = Self::attr(ino, &node);
        state.inodes.insert(ino, node);
        let parent_node = state.inodes.get_mut(&parent).unwrap();
        parent_node.children.insert(name.to_os_string(), ino);
        parent_node.mtime = now;
        parent_node.ctime = now;
        Ok(attr)
    }

    fn unlink_node(&self, parent: INodeNo, name: &OsStr) -> Result<(), fuser::Errno> {
        let mut state = self.state.write().unwrap();
        let parent_node = state.inodes.get(&parent).ok_or(fuser::Errno::ENOENT)?;
        if parent_node.kind != FileType::Directory {
            return Err(fuser::Errno::ENOTDIR);
        }
        let ino = *parent_node
            .children
            .get(name)
            .ok_or(fuser::Errno::ENOENT)?;
        let node = state.inodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
        if node.kind == FileType::Directory {
            return Err(fuser::Errno::EISDIR);
        }

        let now = SystemTime::now();
        let parent_node = state.inodes.get_mut(&parent).unwrap();
        parent_node.children.remove(name);
        parent_node.mtime = now;
        parent_node.ctime = now;
        state.inodes.remove(&ino);
        Ok(())
    }

    fn setattr_node(
        &self,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<SystemTime>,
    ) -> Result<FileAttr, fuser::Errno> {
        let size = size
            .map(|size| usize::try_from(size).map_err(|_| fuser::Errno::EFBIG))
            .transpose()?;
        let mut state = self.state.write().unwrap();
        let node = state.inodes.get_mut(&ino).ok_or(fuser::Errno::ENOENT)?;
        if size.is_some() && node.kind != FileType::RegularFile {
            return Err(fuser::Errno::EISDIR);
        }

        let now = SystemTime::now();
        let changed = mode.is_some()
            || uid.is_some()
            || gid.is_some()
            || size.is_some()
            || atime.is_some()
            || mtime.is_some();
        if let Some(mode) = mode {
            node.perm = mode as u16 & 0o7777;
        }
        if let Some(uid) = uid {
            node.uid = uid;
        }
        if let Some(gid) = gid {
            node.gid = gid;
        }
        if let Some(size) = size {
            node.contents.resize(size, 0);
        }
        if let Some(atime) = atime {
            node.atime = match atime {
                fuser::TimeOrNow::SpecificTime(time) => time,
                fuser::TimeOrNow::Now => now,
            };
        }
        if let Some(mtime) = mtime {
            node.mtime = match mtime {
                fuser::TimeOrNow::SpecificTime(time) => time,
                fuser::TimeOrNow::Now => now,
            };
        } else if size.is_some() {
            node.mtime = now;
        }
        if let Some(ctime) = ctime {
            node.ctime = ctime;
        } else if changed {
            node.ctime = now;
        }
        Ok(Self::attr(ino, node))
    }

    fn rmdir_node(&self, parent: INodeNo, name: &OsStr) -> Result<(), fuser::Errno> {
        let mut state = self.state.write().unwrap();
        let parent_node = state.inodes.get(&parent).ok_or(fuser::Errno::ENOENT)?;
        if parent_node.kind != FileType::Directory {
            return Err(fuser::Errno::ENOTDIR);
        }
        let ino = *parent_node
            .children
            .get(name)
            .ok_or(fuser::Errno::ENOENT)?;
        let node = state.inodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
        if node.kind != FileType::Directory {
            return Err(fuser::Errno::ENOTDIR);
        }
        if !node.children.is_empty() {
            return Err(fuser::Errno::ENOTEMPTY);
        }

        let now = SystemTime::now();
        let parent_node = state.inodes.get_mut(&parent).unwrap();
        parent_node.children.remove(name);
        parent_node.mtime = now;
        parent_node.ctime = now;
        state.inodes.remove(&ino);
        Ok(())
    }

    fn write_node(&self, ino: INodeNo, offset: u64, data: &[u8]) -> Result<(), fuser::Errno> {
        let start = usize::try_from(offset).map_err(|_| fuser::Errno::EFBIG)?;
        let end = start.checked_add(data.len()).ok_or(fuser::Errno::EFBIG)?;
        let mut state = self.state.write().unwrap();
        let node = state.inodes.get_mut(&ino).ok_or(fuser::Errno::ENOENT)?;
        if node.kind != FileType::RegularFile {
            return Err(fuser::Errno::EISDIR);
        }
        node.contents.resize(end, 0);
        node.contents[start..end].copy_from_slice(data);
        node.mtime = SystemTime::now();
        node.ctime = node.mtime;
        Ok(())
    }
}

impl Filesystem for NullFS {
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let state = self.state.read().unwrap();
        let Some(node) = state.inodes.get(&ino) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };

        reply.attr(&Duration::from_secs(1), &Self::attr(ino, node));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        match self.setattr_node(ino, mode, uid, gid, size, atime, mtime, ctime) {
            Ok(attr) => reply.attr(&Duration::from_secs(1), &attr),
            Err(err) => reply.error(err),
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let state = self.state.read().unwrap();
        let Some(parent_node) = state.inodes.get(&parent) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        if parent_node.kind != FileType::Directory {
            reply.error(fuser::Errno::ENOTDIR);
            return;
        }
        let Some((ino, child)) = parent_node
            .children
            .get(name)
            .and_then(|ino| state.inodes.get(ino).map(|node| (*ino, node)))
        else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };

        reply.entry(
            &Duration::from_secs(1),
            &Self::attr(ino, child),
            Generation(0),
        );
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let state = self.state.read().unwrap();
        let Some(node) = state.inodes.get(&ino) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        if node.kind != FileType::Directory {
            reply.error(fuser::Errno::ENOTDIR);
            return;
        }
        let entries = std::iter::once((ino, FileType::Directory, OsString::from(".")))
            .chain(std::iter::once((ino, FileType::Directory, OsString::from(".."))))
            .chain(node.children.iter().filter_map(|(name, child_ino)| {
                state
                    .inodes
                    .get(child_ino)
                    .map(|child| (*child_ino, child.kind, name.clone()))
            }));

        for (index, (ino, kind, name)) in entries
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(ino, (index + 1) as u64, kind, name) {
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
        let state = self.state.read().unwrap();
        let Some(node) = state.inodes.get(&ino) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        if node.kind != FileType::RegularFile {
            reply.error(fuser::Errno::EISDIR);
            return;
        };
        let data = usize::try_from(offset)
            .ok()
            .and_then(|start| {
                node.contents
                    .get(start..start.saturating_add(size as usize).min(node.contents.len()))
            })
            .unwrap_or_default();

        reply.data(data);
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.create_node(
            parent,
            name,
            FileType::RegularFile,
            mode,
            umask,
            req.uid(),
            req.gid(),
        ) {
            Ok(attr) => reply.created(
                &Duration::from_secs(1),
                &attr,
                Generation(0),
                FileHandle(0),
                FopenFlags::empty(),
            ),
            Err(err) => reply.error(err),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self.create_node(
            parent,
            name,
            FileType::Directory,
            mode,
            umask,
            req.uid(),
            req.gid(),
        ) {
            Ok(attr) => reply.entry(&Duration::from_secs(1), &attr, Generation(0)),
            Err(err) => reply.error(err),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.unlink_node(parent, name) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.rmdir_node(parent, name) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let state = self.state.read().unwrap();
        match state.inodes.get(&ino) {
            Some(node) if node.kind == FileType::RegularFile => {
                reply.opened(FileHandle(0), FopenFlags::empty())
            }
            Some(_) => reply.error(fuser::Errno::EISDIR),
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.write_node(ino, offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(err) => reply.error(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_contains_hello_world() {
        let fs = NullFS::new();
        let state = fs.state.read().unwrap();
        assert_eq!(state.inodes[&HELLO].contents, b"hello world");
        assert_eq!(
            state.inodes[&INodeNo::ROOT].children[OsStr::new("hello.txt")],
            HELLO
        );
    }

    #[test]
    fn create_adds_a_file_with_masked_permissions() {
        let fs = NullFS::new();
        let attr = fs
            .create_node(
                INodeNo::ROOT,
                OsStr::new("new.txt"),
                FileType::RegularFile,
                0o666,
                0o022,
                1000,
                1000,
            )
            .unwrap();

        let state = fs.state.read().unwrap();
        assert_eq!(attr.perm, 0o644);
        assert_eq!(state.inodes[&attr.ino].contents, b"");
        assert_eq!(
            state.inodes[&INodeNo::ROOT].children[OsStr::new("new.txt")],
            attr.ino
        );
    }

    #[test]
    fn write_extends_files_with_zeroes() {
        let fs = NullFS::new();
        fs.write_node(HELLO, 13, b"!").unwrap();

        let state = fs.state.read().unwrap();
        assert_eq!(state.inodes[&HELLO].contents, b"hello world\0\0!");
    }

    #[test]
    fn setattr_updates_metadata_and_truncates_files() {
        let fs = NullFS::new();
        let atime = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
        let attr = fs
            .setattr_node(
                HELLO,
                Some(0o600),
                Some(1000),
                Some(1001),
                Some(5),
                Some(fuser::TimeOrNow::SpecificTime(atime)),
                Some(fuser::TimeOrNow::SpecificTime(mtime)),
                None,
            )
            .unwrap();

        assert_eq!(attr.size, 5);
        assert_eq!(attr.perm, 0o600);
        assert_eq!(attr.uid, 1000);
        assert_eq!(attr.gid, 1001);
        assert_eq!(attr.atime, atime);
        assert_eq!(attr.mtime, mtime);
        assert_eq!(fs.state.read().unwrap().inodes[&HELLO].contents, b"hello");
    }

    #[test]
    fn setattr_cannot_truncate_a_directory() {
        let fs = NullFS::new();

        assert_eq!(
            fs.setattr_node(INodeNo::ROOT, None, None, None, Some(0), None, None, None),
            Err(fuser::Errno::EISDIR)
        );
    }

    #[test]
    fn mkdir_adds_an_empty_directory_with_masked_permissions() {
        let fs = NullFS::new();
        let attr = fs
            .create_node(
                INodeNo::ROOT,
                OsStr::new("docs"),
                FileType::Directory,
                0o777,
                0o022,
                1000,
                1000,
            )
            .unwrap();

        let state = fs.state.read().unwrap();
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.perm, 0o755);
        assert!(state.inodes[&attr.ino].children.is_empty());
        assert_eq!(state.inodes[&INodeNo::ROOT].children[OsStr::new("docs")], attr.ino);
    }

    #[test]
    fn unlink_removes_a_file_but_not_a_directory() {
        let fs = NullFS::new();
        fs.create_node(
            INodeNo::ROOT,
            OsStr::new("docs"),
            FileType::Directory,
            0o755,
            0,
            1000,
            1000,
        )
        .unwrap();
        fs.unlink_node(INodeNo::ROOT, OsStr::new("hello.txt"))
            .unwrap();

        let state = fs.state.read().unwrap();
        assert!(!state.inodes.contains_key(&HELLO));
        assert!(!state.inodes[&INodeNo::ROOT]
            .children
            .contains_key(OsStr::new("hello.txt")));
        drop(state);

        assert_eq!(
            fs.unlink_node(INodeNo::ROOT, OsStr::new("missing.txt")),
            Err(fuser::Errno::ENOENT)
        );
        assert_eq!(
            fs.unlink_node(INodeNo::ROOT, OsStr::new("docs")),
            Err(fuser::Errno::EISDIR)
        );
    }

    #[test]
    fn rmdir_removes_empty_directories_only() {
        let fs = NullFS::new();
        let empty = fs
            .create_node(
                INodeNo::ROOT,
                OsStr::new("empty"),
                FileType::Directory,
                0o755,
                0,
                1000,
                1000,
            )
            .unwrap()
            .ino;
        fs.rmdir_node(INodeNo::ROOT, OsStr::new("empty"))
            .unwrap();
        assert!(!fs.state.read().unwrap().inodes.contains_key(&empty));

        let full = fs
            .create_node(
                INodeNo::ROOT,
                OsStr::new("full"),
                FileType::Directory,
                0o755,
                0,
                1000,
                1000,
            )
            .unwrap()
            .ino;
        fs.create_node(
            full,
            OsStr::new("child"),
            FileType::RegularFile,
            0o644,
            0,
            1000,
            1000,
        )
        .unwrap();

        assert_eq!(
            fs.rmdir_node(INodeNo::ROOT, OsStr::new("full")),
            Err(fuser::Errno::ENOTEMPTY)
        );
        assert_eq!(
            fs.rmdir_node(INodeNo::ROOT, OsStr::new("hello.txt")),
            Err(fuser::Errno::ENOTDIR)
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let cfg = args.config();
    let (interrupt_tx, interrupt_rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = interrupt_tx.send(());
    })?;

    let session = fuser::spawn_mount(NullFS::new(), &args.mount_point, &cfg)?;
    interrupt_rx.recv()?;
    session.umount_and_join()?;
    Ok(())
}
