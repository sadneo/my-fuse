use std::collections::{HashMap, HashSet, VecDeque};
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

        Ok(Self {
            client,
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
        validate_tree(&crate::NullFS::new().state.read().unwrap()).unwrap();
    }

    #[test]
    fn root_metadata_has_the_root_marker() {
        let state = crate::NullFS::new();
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
}
