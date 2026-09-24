use clap::Parser;
use fuser::{
    FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, LockOwner, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};

use std::ffi::OsStr;
use std::path::PathBuf;
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
const ROOT_CHILDREN: &[Child] = &[Child {
    name: "hello.txt",
    ino: HELLO,
}];
const NODES: &[Node] = &[
    Node {
        ino: INodeNo::ROOT,
        kind: FileType::Directory,
        perm: 0o755,
        contents: b"",
        children: ROOT_CHILDREN,
    },
    Node {
        ino: HELLO,
        kind: FileType::RegularFile,
        perm: 0o644,
        contents: b"hello world",
        children: &[],
    },
];

struct Child {
    name: &'static str,
    ino: INodeNo,
}

struct Node {
    ino: INodeNo,
    kind: FileType,
    perm: u16,
    contents: &'static [u8],
    children: &'static [Child],
}

struct NullFS;

impl NullFS {
    fn node(ino: INodeNo) -> Option<&'static Node> {
        NODES.iter().find(|node| node.ino == ino)
    }

    fn attr(node: &Node) -> FileAttr {
        FileAttr {
            ino: node.ino,
            size: node.contents.len() as u64,
            blocks: 0,
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            crtime: SystemTime::UNIX_EPOCH,
            kind: node.kind,
            perm: node.perm,
            nlink: if node.kind == FileType::Directory {
                2 + node
                    .children
                    .iter()
                    .filter(|child| {
                        Self::node(child.ino)
                            .is_some_and(|node| node.kind == FileType::Directory)
                    })
                    .count() as u32
            } else {
                1
            },
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

impl Filesystem for NullFS {
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(node) = Self::node(ino) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };

        reply.attr(&Duration::from_secs(1), &Self::attr(node));
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent) = Self::node(parent) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        let Some(child) = parent
            .children
            .iter()
            .find(|child| name == OsStr::new(child.name))
            .and_then(|child| Self::node(child.ino))
        else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };

        reply.entry(&Duration::from_secs(1), &Self::attr(child), Generation(0));
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = Self::node(ino).filter(|node| node.kind == FileType::Directory) else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        let entries = [(node.ino, node.kind, "."), (node.ino, node.kind, "..")]
            .into_iter()
            .chain(node.children.iter().filter_map(|child| {
                Self::node(child.ino).map(|node| (node.ino, node.kind, child.name))
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
        let Some(node) = Self::node(ino).filter(|node| node.kind == FileType::RegularFile) else {
            reply.error(fuser::Errno::ENOENT);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_contains_hello_world() {
        assert_eq!(NullFS::node(HELLO).unwrap().contents, b"hello world");
        assert_eq!(NullFS::node(INodeNo::ROOT).unwrap().children[0].ino, HELLO);
    }
}

fn main() {
    let args = Args::parse();
    let cfg = args.config();
    fuser::mount(NullFS, &args.mount_point, &cfg).unwrap();
}
