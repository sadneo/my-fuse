# Discord Storage Plan

DiscordFS uses one private Discord channel for both filesystem metadata and file blobs.

## Configuration

Configuration comes from `.env`:

```dotenv
DISCORD_TOKEN=bot-token
DISCORD_GUILD_ID=guild-id
DISCORD_CHANNEL_ID=storage-channel-id
```

The bot needs permission to view the configured channel and to create, read, edit, and delete messages and attachments there. `.env` is ignored by Git; `.env.example` documents the required variables.

## Storage Model

Every file and directory is one Discord message. The message body contains exactly one fenced JSON object. A file message also has one attachment containing its bytes; a directory message has no attachment.

Discord's message ID is the object's persistent ID. Parent references use message IDs, so names and paths can change without changing object identity. The root directory has a null parent.

Directory message:

````text
```json
{
  "version": 1,
  "kind": "directory",
  "name": "documents",
  "parent_id": "123456789012345678",
  "mode": 493,
  "uid": 1000,
  "gid": 1000,
  "atime": "2026-09-26T12:00:00Z",
  "mtime": "2026-09-26T12:00:00Z",
  "ctime": "2026-09-26T12:00:00Z"
}
```
````

File message, with the file bytes uploaded as an attachment:

````text
```json
{
  "version": 1,
  "kind": "file",
  "name": "notes.txt",
  "parent_id": "123456789012345678",
  "mode": 420,
  "uid": 1000,
  "gid": 1000,
  "size": 12,
  "sha256": "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447",
  "atime": "2026-09-26T12:00:00Z",
  "mtime": "2026-09-26T12:00:00Z",
  "ctime": "2026-09-26T12:00:00Z"
}
```
````

Modes are stored as decimal JSON numbers (`0755` is `493`, `0644` is `420`). Timestamps use UTC RFC 3339. The hash detects incomplete or corrupted downloads.

## Operations

- Create: post the JSON message, adding one attachment for a file.
- Read directory: scan metadata messages and select objects whose `parent_id` matches the directory message ID.
- Read file: fetch its message, verify the attachment size and SHA-256 hash, then return its bytes.
- Rename or change metadata: edit the message JSON.
- Replace file contents: replace the file message and update references to its new message ID.
- Delete file: delete its message.
- Delete directory: reject non-empty directories, otherwise delete its message.
- Mount or recovery: scan channel history, parse valid `json` code blocks, and rebuild the in-memory inode index.

## Initial Limits

- Files must fit in one Discord attachment.
- Metadata must fit in one Discord message.
- The channel is the source of truth; CDN URLs are not persisted as identifiers.
- Discord writes are remote and non-transactional. Validate the completed message before exposing it through FUSE.
- Chunking, deduplication, multiple channels, and a separate index are deferred until measurements require them.
