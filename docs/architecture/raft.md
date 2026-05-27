# Raft Log Schema and Dialog Delta Format

This document describes the Raft consensus layer introduced in slice 6.3a
(`smiths-raft`) and completed in 6.3b with snapshot support and multi-node
network transport.

## Overview

`smiths-raft` wraps [`openraft`](https://crates.io/crates/openraft) to provide
a linearizable, replicated log over the engine's `DashMap<DialogKey, DialogRecord>`
dialog table.

```
                   ┌─────────────────────┐
  SIP Traffic ───► │   UasServer         │
                   │  (emit DialogDelta) │
                   └────────┬────────────┘
                            │ client_write(delta)
                   ┌────────▼────────────┐
                   │   openraft::Raft    │  ◄── SQLite log (SqliteLogStore)
                   │   (consensus layer) │
                   └────────┬────────────┘
                            │ apply(committed entries)
                   ┌────────▼────────────┐
                   │ DialogStateMachine  │
                   │   (DashMap update)  │
                   └─────────────────────┘
```

## SQLite Schema

The log store (`crates/smiths-raft/src/log_store.rs`) creates two tables:

### `raft_state`

Stores scalar Raft node state.

| Column | Type | Description |
|--------|------|-------------|
| `key`  | `TEXT PRIMARY KEY` | Logical key, currently `"vote"` |
| `value` | `BLOB` | JSON-serialized value |

The `"vote"` record stores the current `openraft::Vote<NodeId>` (term + voted-for node ID)
so the node can uphold the Raft durability guarantee across restarts.

### `raft_logs`

Stores committed and uncommitted Raft log entries.

| Column | Type | Description |
|--------|------|-------------|
| `log_index` | `INTEGER PRIMARY KEY` | Monotonically increasing log index |
| `term`      | `INTEGER` | Term in which this entry was proposed |
| `payload`   | `BLOB` | `serde_json`-encoded `openraft::Entry<SmithsTypeConfig>` |

Each `payload` is a JSON-encoded `Entry` whose `EntryPayload::Normal` variant carries
a `DialogDelta`.

## `DialogDelta` Format

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DialogDelta {
    Upsert(Box<DialogRecord>),
    Delete(DialogKey),
}
```

`DialogKey = (call_id: String, local_tag: String, remote_tag: String)`

## Snapshot Format (6.3b)

Snapshots are JSON-encoded `SnapshotPayload` structs stored in a `Cursor<Vec<u8>>`:

```rust
struct SnapshotPayload {
    dialogs: Vec<DialogRecord>,
    last_applied_term: Option<u64>,
    last_applied_node: Option<u64>,
    last_applied_index: Option<u64>,
}
```

Snapshot lifecycle:
1. **Build**: `DialogSnapshotBuilder::build_snapshot()` iterates the `DashMap`, serializes all records + last-applied metadata.
2. **Install**: `install_snapshot()` clears the `DashMap`, deserializes the payload, and repopulates. Updates `last_applied` and `last_membership`.
3. **Cache**: The most recent snapshot is cached in-memory for `get_current_snapshot()`.

## Network Transport (6.3b)

Inter-node Raft RPCs use TCP + JSON-lines:

```
┌──────────┐     TCP + JSON-lines      ┌──────────┐
│  Node 1  │ ◄────────────────────────► │  Node 2  │
│ (leader) │   AppendEntries/Vote/      │(follower)│
│          │   InstallSnapshot          │          │
└──────────┘                            └──────────┘
```

### Protocol

Each RPC is a single TCP connection carrying one JSON-lines request and response:

- **Request envelope** (`RaftRpc`): tagged union with `"rpc": "append_entries" | "install_snapshot" | "vote"`.
- **Response envelope** (`RaftRpcResponse`): tagged union with matching variant or `"error"`.

The server (`run_raft_server`) listens on `cluster.raft_addr` and dispatches to the local `Raft` instance.

## Boot-Time Log Replay

On startup the `openraft::Raft` node:

1. Opens the `SqliteLogStore` at the configured `cluster.raft_dir`.
2. Loads the persisted `Vote` (voted-for state).
3. Reads the log from `last_purged_log_id` up to `last_log_id`.
4. Applies committed entries to `DialogStateMachine` via the normal `apply()` path.
5. Only after the state machine is caught up does the node accept SIP traffic.

## Configuration

```toml
[cluster]
mode           = "primary"
node_id        = 1
raft_dir       = "/var/lib/smiths/raft"
raft_addr      = "10.42.0.1:9000"
initial_peers  = ["2@10.42.0.2:9000", "3@10.42.0.3:9000"]
peer_addr      = "10.42.0.10:8000"   # legacy 6.2 TCP replication
```

| Field | Type | Description |
|-------|------|-------------|
| `node_id` | `u64` | Unique Raft node identifier |
| `raft_dir` | `PathBuf` | Directory for SQLite Raft log |
| `raft_addr` | `SocketAddr` | Bind address for Raft RPC server |
| `initial_peers` | `Vec<String>` | `"node_id@host:port"` for cluster bootstrap |
