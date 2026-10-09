use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::env;
use std::error::Error;
use std::io;
use std::thread;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use fuser::{FileType, INodeNo};
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, multipart};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{FsState, Node};

const API_BASE: &str = "https://discord.com/api/v10";

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub struct DiscordClient {
    client: Client,
    download_client: Client,
    #[allow(dead_code)]
    guild_id: String,
    channel_id: String,
}

#[derive(Serialize)]
struct Metadata<'a> {
    version: u8,
    kind: &'static str,
    name: &'a str,
    parent_id: Option<&'a str>,
    mode: u16,
    uid: u32,
    gid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    atime: String,
    mtime: String,
    ctime: String,
}

#[derive(Serialize)]
struct MessagePayload<'a> {
    content: &'a str,
    allowed_mentions: AllowedMentions,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<NewAttachment>>,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: [String; 0],
}

#[derive(Serialize)]
struct NewAttachment {
    id: u8,
    filename: &'static str,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    content: String,
    attachments: Vec<Attachment>,
}

#[derive(Deserialize)]
struct Attachment {
    url: String,
    size: usize,
}

#[derive(Deserialize)]
struct DownloadedMetadata {
    version: u8,
    kind: String,
    name: String,
    parent_id: Option<String>,
    mode: u16,
    uid: u32,
    gid: u32,
    size: Option<usize>,
    sha256: Option<String>,
    atime: String,
    mtime: String,
    ctime: String,
}

#[derive(Deserialize)]
struct RateLimit {
    retry_after: f64,
}

impl DiscordClient {
    pub fn new() -> Result<Self> {
        let token = env::var("DISCORD_TOKEN")?;
        let guild_id = env::var("DISCORD_GUILD_ID")?;
        let channel_id = env::var("DISCORD_CHANNEL_ID")?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bot {token}"))?,
        );
        let client = Client::builder()
            .default_headers(headers)
            .user_agent(concat!(
                "DiscordBot (https://github.com/aidan/discordfs, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()?;
        let download_client = Client::builder()
            .user_agent(concat!(
                "DiscordBot (https://github.com/aidan/discordfs, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()?;

        Ok(Self {
            client,
            download_client,
            guild_id,
            channel_id,
        })
    }

    pub fn upload_all(&self, state: &FsState) -> Result<HashMap<INodeNo, String>> {
        validate_tree(state)?;

        let mut ids = HashMap::new();
        let mut queue = VecDeque::from([(INodeNo::ROOT, "/".to_owned(), None)]);
        while let Some((ino, name, parent_id)) = queue.pop_front() {
            let node = &state.inodes[&ino];
            let message_id =
                self.upload_inode(node, &name, parent_id.as_deref(), ino == INodeNo::ROOT)?;
            ids.insert(ino, message_id.clone());
            queue.extend(node.children.iter().map(|(name, ino)| {
                (
                    *ino,
                    name.to_str().unwrap().to_owned(),
                    Some(message_id.clone()),
                )
            }));
        }
        Ok(ids)
    }

    pub fn clear_all(&self) -> Result<usize> {
        let messages_url = format!("{API_BASE}/channels/{}/messages", self.channel_id);
        let list_url = format!("{messages_url}?limit=100");
        let mut deleted = 0;
        loop {
            let messages: Vec<Message> = self.send(|| self.client.get(&list_url))?;
            if messages.is_empty() {
                return Ok(deleted);
            }
            for message in messages {
                let url = format!("{messages_url}/{}", message.id);
                self.send_empty(|| self.client.delete(&url))?;
                deleted += 1;
            }
        }
    }

    pub fn download_all(&self) -> Result<FsState> {
        let messages_url = format!("{API_BASE}/channels/{}/messages", self.channel_id);
        let mut messages = Vec::new();
        let mut before = None;
        loop {
            let url = match &before {
                Some(id) => format!("{messages_url}?limit=100&before={id}"),
                None => format!("{messages_url}?limit=100"),
            };
            let batch: Vec<Message> = self.send(|| self.client.get(&url))?;
            if batch.is_empty() {
                break;
            }
            before = batch.last().map(|message| message.id.clone());
            messages.extend(batch);
        }

        reconstruct(messages, |attachment| self.download(attachment))
    }

    fn download(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        let data = self
            .download_client
            .get(&attachment.url)
            .send()?
            .error_for_status()?
            .bytes()?
            .to_vec();
        if data.len() != attachment.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attachment size does not match Discord metadata",
            )
            .into());
        }
        Ok(data)
    }

    fn upload_inode(
        &self,
        node: &Node,
        name: &str,
        parent_id: Option<&str>,
        root: bool,
    ) -> Result<String> {
        let is_file = node.kind == FileType::RegularFile;
        let metadata = Metadata {
            version: 1,
            kind: if is_file { "file" } else { "directory" },
            name,
            parent_id,
            mode: node.perm,
            uid: node.uid,
            gid: node.gid,
            size: is_file.then_some(node.contents.len()),
            sha256: is_file.then(|| format!("{:x}", Sha256::digest(&node.contents))),
            atime: timestamp(node.atime),
            mtime: timestamp(node.mtime),
            ctime: timestamp(node.ctime),
        };
        let json = serde_json::to_string_pretty(&metadata)?;
        let content = if root {
            format!("### ROOT\n```json\n{json}\n```")
        } else {
            format!("```json\n{json}\n```")
        };
        if content.chars().count() > 2000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "metadata exceeds Discord's 2000-character message limit",
            )
            .into());
        }

        let url = format!("{API_BASE}/channels/{}/messages", self.channel_id);
        let message: Message = if is_file {
            self.send(|| {
                let payload = MessagePayload {
                    content: &content,
                    allowed_mentions: AllowedMentions { parse: [] },
                    attachments: Some(vec![NewAttachment {
                        id: 0,
                        filename: "blob",
                    }]),
                };
                let form = multipart::Form::new()
                    .text("payload_json", serde_json::to_string(&payload).unwrap())
                    .part(
                        "files[0]",
                        multipart::Part::bytes(node.contents.clone())
                            .file_name("blob")
                            .mime_str("application/octet-stream")
                            .unwrap(),
                    );
                self.client.post(&url).multipart(form)
            })?
        } else {
            self.send(|| {
                self.client.post(&url).json(&MessagePayload {
                    content: &content,
                    allowed_mentions: AllowedMentions { parse: [] },
                    attachments: None,
                })
            })?
        };
        Ok(message.id)
    }

    fn send<T: for<'de> Deserialize<'de>>(
        &self,
        mut request: impl FnMut() -> RequestBuilder,
    ) -> Result<T> {
        loop {
            let response = request().send()?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let limit: RateLimit = response.json()?;
                thread::sleep(Duration::from_secs_f64(limit.retry_after));
                continue;
            }
            return Ok(response.error_for_status()?.json()?);
        }
    }

    fn send_empty(&self, mut request: impl FnMut() -> RequestBuilder) -> Result<()> {
        loop {
            let response = request().send()?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let limit: RateLimit = response.json()?;
                thread::sleep(Duration::from_secs_f64(limit.retry_after));
                continue;
            }
            response.error_for_status()?;
            return Ok(());
        }
    }
}

fn reconstruct(
    messages: Vec<Message>,
    mut download: impl FnMut(&Attachment) -> Result<Vec<u8>>,
) -> Result<FsState> {
    let mut records = Vec::new();
    let mut ids = HashMap::new();
    let mut root_found = false;

    for message in messages {
        let Some((metadata, root)) = parse_metadata(&message.content)? else {
            continue;
        };
        if metadata.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported metadata version {}", metadata.version),
            )
            .into());
        }
        let ino = if root {
            if root_found {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "multiple roots").into());
            }
            root_found = true;
            INodeNo::ROOT
        } else {
            INodeNo(message.id.parse()?)
        };
        if ids.insert(message.id.clone(), ino).is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate message ID").into());
        }
        records.push((ino, message, metadata, root));
    }

    if !root_found {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "root message is missing").into());
    }

    let mut inodes = HashMap::new();
    let mut parents = Vec::new();
    for (ino, message, metadata, root) in records {
        let kind = match metadata.kind.as_str() {
            "directory" => FileType::Directory,
            "file" => FileType::RegularFile,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("inode {} has invalid type", ino.0),
                )
                .into());
            }
        };
        if root != metadata.parent_id.is_none() || (root && kind != FileType::Directory) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid root metadata").into());
        }
        let contents = if kind == FileType::RegularFile {
            let [attachment] = message.attachments.as_slice() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("file inode {} must have one attachment", ino.0),
                )
                .into());
            };
            let contents = download(attachment)?;
            if metadata.size != Some(contents.len())
                || metadata.sha256.as_deref()
                    != Some(format!("{:x}", Sha256::digest(&contents)).as_str())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("file inode {} failed integrity checks", ino.0),
                )
                .into());
            }
            contents
        } else {
            if !message.attachments.is_empty()
                || metadata.size.is_some()
                || metadata.sha256.is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("directory inode {} has file data", ino.0),
                )
                .into());
            }
            Vec::new()
        };
        let node = Node {
            kind,
            perm: metadata.mode,
            uid: metadata.uid,
            gid: metadata.gid,
            contents,
            children: BTreeMap::new(),
            atime: parse_timestamp(&metadata.atime)?,
            mtime: parse_timestamp(&metadata.mtime)?,
            ctime: parse_timestamp(&metadata.ctime)?,
        };
        if inodes.insert(ino, node).is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate inode").into());
        }
        parents.push((ino, OsString::from(metadata.name), metadata.parent_id));
    }

    for (ino, name, parent_id) in parents {
        let Some(parent_id) = parent_id else {
            continue;
        };
        let parent = *ids.get(&parent_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inode {} has a missing parent", ino.0),
            )
        })?;
        let parent = inodes.get_mut(&parent).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "parent inode is missing")
        })?;
        if parent.children.insert(name, ino).is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate filename").into());
        }
    }

    let next_ino = inodes
        .keys()
        .map(|ino| ino.0)
        .max()
        .unwrap_or(INodeNo::ROOT.0)
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "inode range exhausted"))?;
    let state = FsState { next_ino, inodes };
    validate_tree(&state)?;
    Ok(state)
}

fn parse_metadata(content: &str) -> Result<Option<(DownloadedMetadata, bool)>> {
    let (content, root) = match content.strip_prefix("### ROOT\n") {
        Some(content) => (content, true),
        None => (content, false),
    };
    let Some(json) = content
        .strip_prefix("```json\n")
        .and_then(|content| content.strip_suffix("\n```"))
    else {
        return Ok(None);
    };
    Ok(Some((serde_json::from_str(json)?, root)))
}

fn parse_timestamp(value: &str) -> Result<SystemTime> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc).into())
}

fn validate_tree(state: &FsState) -> Result<()> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([INodeNo::ROOT]);
    while let Some(ino) = queue.pop_front() {
        if !seen.insert(ino) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inode {} has multiple parents or forms a cycle", ino.0),
            )
            .into());
        }
        let node = state.inodes.get(&ino).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inode {} is missing", ino.0),
            )
        })?;
        if !matches!(node.kind, FileType::Directory | FileType::RegularFile) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inode {} has unsupported type", ino.0),
            )
            .into());
        }
        if node.kind == FileType::RegularFile && !node.children.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file inode {} has children", ino.0),
            )
            .into());
        }
        for (name, child) in &node.children {
            if name.to_str().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Discord metadata cannot store a non-UTF-8 filename",
                )
                .into());
            }
            queue.push_back(*child);
        }
    }
    if seen.len() != state.inodes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem contains unreachable inodes",
        )
        .into());
    }
    Ok(())
}

fn timestamp(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_filesystem_is_a_valid_tree() {
        validate_tree(&crate::DiscordFS::new().state.read().unwrap()).unwrap();
    }

    #[test]
    fn root_metadata_has_the_root_marker() {
        let state = crate::DiscordFS::new();
        let state = state.state.read().unwrap();
        let node = &state.inodes[&INodeNo::ROOT];
        let metadata = Metadata {
            version: 1,
            kind: "directory",
            name: "/",
            parent_id: None,
            mode: node.perm,
            uid: node.uid,
            gid: node.gid,
            size: None,
            sha256: None,
            atime: timestamp(node.atime),
            mtime: timestamp(node.mtime),
            ctime: timestamp(node.ctime),
        };
        let content = format!(
            "### ROOT\n```json\n{}\n```",
            serde_json::to_string(&metadata).unwrap()
        );

        assert!(content.starts_with("### ROOT\n```json\n"));
        assert!(content.ends_with("\n```"));
    }

    #[test]
    fn reconstructs_filesystem_from_messages() {
        let timestamp = "1970-01-01T00:00:00Z";
        let metadata = |kind: &str, name: &str, parent_id: Option<&str>, file: bool| {
            serde_json::json!({
                "version": 1,
                "kind": kind,
                "name": name,
                "parent_id": parent_id,
                "mode": if file { 420 } else { 493 },
                "uid": 1000,
                "gid": 1000,
                "size": file.then_some(3),
                "sha256": file.then_some(format!("{:x}", Sha256::digest(b"abc"))),
                "atime": timestamp,
                "mtime": timestamp,
                "ctime": timestamp
            })
        };
        let messages = vec![
            Message {
                id: "100".into(),
                content: format!("### ROOT\n```json\n{}\n```", metadata("directory", "/", None, false)),
                attachments: Vec::new(),
            },
            Message {
                id: "200".into(),
                content: format!("```json\n{}\n```", metadata("file", "file", Some("100"), true)),
                attachments: vec![Attachment {
                    url: "unused".into(),
                    size: 3,
                }],
            },
        ];

        let state = reconstruct(messages, |_| Ok(b"abc".to_vec())).unwrap();

        assert_eq!(state.next_ino, 201);
        assert_eq!(state.inodes[&INodeNo(200)].contents, b"abc");
        assert_eq!(
            state.inodes[&INodeNo::ROOT].children[std::ffi::OsStr::new("file")],
            INodeNo(200)
        );
    }
}
