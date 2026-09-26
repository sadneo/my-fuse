# Discord Client Plan

## Scope

The first implementation adds:

- `src/discord.rs` with `DiscordClient` and Discord storage types.
- `DiscordClient::new()` for configuration and HTTP client setup.
- `DiscordClient::upload_all()` for uploading a complete `FsState`, including file blobs.
- An `upload-all` debug command.

Downloading, loading metadata, and editing messages use the same types and request helpers later, but are not part of the first change.

## Configuration

`DiscordClient::new() -> Result<Self, DiscordError>` reads:

- `DISCORD_TOKEN`
- `DISCORD_GUILD_ID`
- `DISCORD_CHANNEL_ID`

It builds one reusable `reqwest::blocking::Client` with:

- `Authorization: Bot <token>`
- `User-Agent: DiscordBot (<project URL>, <version>)`
- A request timeout

The client reuses reqwest's connection pool. The guild ID is retained in configuration but is not needed by the message endpoints.

Use blocking reqwest because the current FUSE and debug interfaces are synchronous. Do not add an async runtime.

## Storage Types

Keep Discord metadata separate from the in-memory `Node`. A `Node` does not contain its name or parent; those are represented by its parent's `children` map.

```rust
struct InodeMetadata {
    version: u8,
    kind: InodeKind,
    name: String,
    parent_id: Option<MessageId>,
    mode: u16,
    uid: u32,
    gid: u32,
    size: Option<u64>,
    sha256: Option<String>,
    atime: String,
    mtime: String,
    ctime: String,
}
```

Directories omit `size` and `sha256`. Files include both. Timestamps are UTC RFC 3339 strings. Initially reject non-UTF-8 names because Discord message content is UTF-8.

Use a `MessageId` newtype around `u64` and serialize it as a string. Discord snowflakes can exceed JavaScript's safe integer range.

## Message Format

Normal inode:

````text
```json
<metadata>
```
````

Root inode:

````text
### ROOT
```json
<metadata>
```
````

The root still has `parent_id: null`. `### ROOT` distinguishes it from unlinked objects awaiting garbage collection. Parsing should require the exact marker and fenced JSON shape.

Files have exactly one attachment named `blob`; the attachment filename never follows the filesystem name. Directories have no attachments.

## Discord Requests

Create a directory with one JSON request:

```text
POST /api/v10/channels/{channel_id}/messages
```

Create a file with one multipart request to the same endpoint:

- `payload_json`: message content, empty allowed mentions, and attachment metadata.
- `files[0]`: the complete blob named `blob`.

The returned message object's `id` becomes the persistent inode ID. Check response statuses and honor Discord `429 retry_after` responses. Reject metadata exceeding Discord's 2,000-character message-content limit before sending.

## `upload_inode`

`upload_inode` is an internal primitive used by `upload_all`:

```rust
fn upload_inode(
    &self,
    metadata: &InodeMetadata,
    contents: Option<&[u8]>,
    root: bool,
) -> Result<MessageId, DiscordError>
```

Validate before sending:

- Files require contents, including empty files.
- Directories reject contents.
- File `size` equals the blob length.
- File `sha256` equals the blob digest.
- Root is a directory with no parent.

`upload_inode(&Node)` is not viable by itself because `Node` lacks its name and parent message ID. `upload_all` derives those values from the filesystem tree and builds `InodeMetadata`.

## `upload_all`

```rust
fn upload_all(
    &self,
    state: &FsState,
) -> Result<HashMap<INodeNo, MessageId>, DiscordError>
```

Algorithm:

1. Validate the tree before making requests: root exists and is a directory, every child exists, every non-root inode has exactly one parent, names are valid, and the graph has no cycles or unreachable inodes.
2. Upload `INodeNo::ROOT` with the `### ROOT` marker.
3. Traverse directories breadth-first from root.
4. For each child, derive its name from the parent's `children` entry and use the already-uploaded parent's Discord message ID as `parent_id`.
5. Upload directories as metadata-only messages and files with their complete contents.
6. Record and return `HashMap<INodeNo, MessageId>`.

Parent-first traversal is required because child metadata stores the parent's Discord message ID, not its local FUSE inode number.

The operation cannot be transactional. If an upload fails, return the error immediately; messages already created remain unreachable or form an incomplete tree and can be removed by the future mark-and-sweep collector. Do not automatically retry the entire upload because that would duplicate successful messages. Individual HTTP requests may retry only rate limits and safe pre-response transport failures.

The debug command should upload a consistent snapshot without holding the filesystem lock during network requests. Derive `Clone` for the state types, clone under the read lock, release the lock, then call `upload_all`. This temporarily duplicates file contents in memory, which is acceptable for the initial debug-only operation.

## Debug Command

Add `Command::UploadAll` and parse the exact command `upload-all`.

Execution:

1. Clone the current `FsState` under its read lock.
2. Construct `DiscordClient::new()`.
3. Call `upload_all(&snapshot)`.
4. Print each local inode to Discord message ID mapping.
5. Print configuration, validation, HTTP, and Discord errors without exiting the REPL.

Add `upload-all` to help text and syntax highlighting. Extend the existing REPL parser test with `upload-all`.

## Dependencies

Add only the required crates:

- `reqwest` with `blocking`, `json`, `multipart`, and Rustls TLS features.
- `serde` with `derive`.
- `serde_json`.
- `sha2`.
- `time` with formatting support for RFC 3339 timestamps.

Environment variables are already loaded by `.envrc`; do not add a dotenv crate.

## Verification

- Unit-test metadata serialization for root, directory, and file messages.
- Unit-test tree validation and parent-first upload ordering without sending HTTP requests.
- Unit-test the `upload-all` REPL command.
- Run `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features`.

Manual verification uses a dedicated empty Discord channel. Run `upload-all`, confirm one message per inode, confirm only files have attachments, and confirm every child `parent_id` matches its uploaded parent message ID.

## Later Client Operations

- `get_all_metadata()`: paginate channel history, locate `### ROOT`, and reconstruct reachable metadata without downloading blobs.
- `download_inode(message_id)`: fetch the message and attachment, then verify size and SHA-256.
- `edit_metadata(message_id, changes)`: merge minimal changes into the existing metadata and replace message content while preserving attachments.
- `edit_contents(message_id, contents, mtime, ctime)`: multipart PATCH the same message with a replacement attachment and updated size, hash, and timestamps. Discord preserves the message ID, so references do not change.
- Mark-and-sweep: start at `### ROOT`, mark through `parent_id` relationships, and delete old unreachable messages outside active mount/write windows.
