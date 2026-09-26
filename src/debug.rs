use fuser::{FileType, INodeNo};

use crate::{FsState, NullFS, repl::Repl};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Empty,
    Help,
    State,
    Tree,
    Stat(INodeNo),
    Cat(INodeNo),
    Exit,
    Invalid(String),
    Unknown(String),
}

pub fn run(fs: &NullFS) -> rustyline::Result<()> {
    let mut repl = Repl::new()?;
    loop {
        match repl.read_command()? {
            Command::Empty => {}
            Command::Help => {
                println!("Commands: help, state, tree, stat <inode>, cat <inode>, exit, quit")
            }
            Command::State => print_state(fs),
            Command::Tree => print_tree(fs),
            Command::Stat(ino) => print_stat(fs, ino),
            Command::Cat(ino) => print_cat(fs, ino),
            Command::Exit => break,
            Command::Invalid(message) => eprintln!("{message}"),
            Command::Unknown(command) => eprintln!("unknown command: {command}"),
        }
    }
    Ok(())
}

fn print_state(fs: &NullFS) {
    let state = fs.state.read().unwrap();
    println!("inodes: {}\nnext inode: {}", state.inodes.len(), state.next_ino);
}

fn print_tree(fs: &NullFS) {
    fn visit(state: &FsState, ino: INodeNo, name: &str, depth: usize) {
        let Some(node) = state.inodes.get(&ino) else {
            println!("{}{} ({}) [missing]", "  ".repeat(depth), name, ino.0);
            return;
        };
        println!(
            "{}{} ({}) {} {:04o} {} bytes",
            "  ".repeat(depth),
            name,
            ino.0,
            kind(node.kind),
            node.perm,
            node.contents.len()
        );
        for (name, child) in &node.children {
            visit(state, *child, &name.to_string_lossy(), depth + 1);
        }
    }

    visit(&fs.state.read().unwrap(), INodeNo::ROOT, "/", 0);
}

fn print_stat(fs: &NullFS, ino: INodeNo) {
    let state = fs.state.read().unwrap();
    let Some(node) = state.inodes.get(&ino) else {
        eprintln!("inode {} not found", ino.0);
        return;
    };
    println!(
        "inode: {}\ntype: {}\npermissions: {:04o}\nuid: {}\ngid: {}\nsize: {}\nchildren: {}\natime: {:?}\nmtime: {:?}\nctime: {:?}",
        ino.0,
        kind(node.kind),
        node.perm,
        node.uid,
        node.gid,
        node.contents.len(),
        node.children.len(),
        node.atime,
        node.mtime,
        node.ctime
    );
}

fn print_cat(fs: &NullFS, ino: INodeNo) {
    let state = fs.state.read().unwrap();
    let Some(node) = state.inodes.get(&ino) else {
        eprintln!("inode {} not found", ino.0);
        return;
    };
    if node.kind != FileType::RegularFile {
        eprintln!("inode {} is not a file", ino.0);
        return;
    }
    println!("{}", node.contents.escape_ascii());
}

fn kind(kind: FileType) -> &'static str {
    match kind {
        FileType::Directory => "directory",
        FileType::RegularFile => "file",
        FileType::Symlink => "symlink",
        FileType::BlockDevice => "block device",
        FileType::CharDevice => "character device",
        FileType::NamedPipe => "named pipe",
        FileType::Socket => "socket",
    }
}
