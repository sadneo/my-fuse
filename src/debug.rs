use fuser::{FileType, INodeNo};

use crate::{DiscordFS, FsState, discord::DiscordClient, repl::Repl};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Empty,
    Help,
    State,
    Tree,
    Stat(INodeNo),
    Cat(INodeNo),
    UploadAll,
    ClearAll,
    ClearLocal,
    Exit,
    Invalid(String),
    Unknown(String),
}

pub fn run(fs: &DiscordFS) -> rustyline::Result<()> {
    let mut repl = Repl::new()?;
    loop {
        match repl.read_command()? {
            Command::Empty => {}
            Command::Help => {
                println!(
                    "Commands: help, state, tree, stat <inode>, cat <inode>, upload-all, clear-all, clear-local, exit, quit"
                )
            }
            Command::State => print_state(fs),
            Command::Tree => print_tree(fs),
            Command::Stat(ino) => print_stat(fs, ino),
            Command::Cat(ino) => print_cat(fs, ino),
            Command::UploadAll => upload_all(fs),
            Command::ClearAll => clear_all(),
            Command::ClearLocal => {
                fs.clear_local();
                println!("cleared local filesystem");
            }
            Command::Exit => break,
            Command::Invalid(message) => eprintln!("{message}"),
            Command::Unknown(command) => eprintln!("unknown command: {command}"),
        }
    }
    Ok(())
}

fn clear_all() {
    match DiscordClient::new().and_then(|client| client.clear_all()) {
        Ok(count) => println!("deleted {count} messages"),
        Err(error) => eprintln!("clear failed: {error}"),
    }
}

fn upload_all(fs: &DiscordFS) {
    let state = fs.state.read().unwrap().clone();
    match DiscordClient::new().and_then(|client| client.upload_all(&state)) {
        Ok(ids) => {
            println!("uploaded {} inodes", ids.len());
            let mut ids: Vec<_> = ids.into_iter().collect();
            ids.sort_unstable_by_key(|(ino, _)| ino.0);
            for (ino, message_id) in ids {
                println!("{} -> {message_id}", ino.0);
            }
        }
        Err(error) => eprintln!("upload failed: {error}"),
    }
}

fn print_state(fs: &DiscordFS) {
    let state = fs.state.read().unwrap();
    println!("inodes: {}\nnext inode: {}", state.inodes.len(), state.next_ino);
}

fn print_tree(fs: &DiscordFS) {
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

fn print_stat(fs: &DiscordFS, ino: INodeNo) {
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

fn print_cat(fs: &DiscordFS, ino: INodeNo) {
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
