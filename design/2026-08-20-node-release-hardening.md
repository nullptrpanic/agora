# Node Release Hardening Boundary

**Status:** approved, intentionally lightweight

**Date:** 2026-08-20

**Scope:** `agora-node`

This note fixes the release boundary. The detailed shipped behavior remains canonical in `spec/`.

## Required before release

| Area | Shipped rule |
| --- | --- |
| Single process | One operating-system user may run only one Node against the fixed `~/.agora` state. The daemon holds `~/.agora/node.lock` for its lifetime; a second Node fails before opening SQLite. |
| Private local state | On Unix, `~/.agora`, its database directory, Store files, and lock use private permissions. Generated configuration is replaced atomically with mode `0600`. |
| Configuration | Reject unsupported filters, invalid limits, invalid or non-executable Agent paths, invalid Telegram token shape, empty credentials, and empty permission identifiers before channel startup. The generator requires one allowed user and hides credentials on an interactive terminal. |
| Persistent identity | Session and disabled-Agent rows include configured Agent name, backend type, canonical workspace, configured Channel name, provider type, and non-secret provider account identity where applicable. Changing those identities cannot accidentally reuse old state. |
| Session lifecycle | A fresh successful Codex run must report a nonempty thread ID. A resumed run may not replace the requested ID with a different one. Missing mapped sessions are conditionally removed and retried fresh once. |
| Session deletion | `/reset` deletes the backend session first and removes only the matching local row. Backend “not found” is success; other deletion errors keep the mapping. |
| Store upgrade | Schema v3 is migrated to v4 in one transaction. Legacy rows receive identities that cannot match an active v4 configuration. Unknown versions are rejected. |
| Network lifetime | Existing Lark reconnect and Telegram polling retry behavior remains provider-owned. Lark event capacity and tenant-token cache are shared across reconnects/card runs so reconnects cannot multiply admission capacity or token requests. |
| Memory bounds | Lark and Telegram retain bounded answer and process state. Truncation is UTF-8 safe and visible. Existing provider-specific rendering and Telegram multipart terminal delivery remain unchanged. |

## Runtime semantics kept unchanged

- The scheduler remains process-local and authoritative for queued/running work.
- Clicking `结束任务` targets exactly that run. If it is queued, backend execution never starts and
  the next FIFO item advances.
- One Agent subscribed to multiple Channels has independent Channel connection, delivery,
  permission, and rendering state. With `isolate: session`, provider account and channel session
  identity separate backend sessions. With `isolate: none`, sharing one backend session is explicit.
- Store operations remain short synchronous SQLite calls behind the existing mutex. No provider or
  Agent I/O occurs while a Store transaction is held.
- Lark frame jobs may be spawned, but one semaphore shared by all reconnects bounds them. They do
  not create a durable queue.

## Deliberately not implemented

- Configurable `state_dir`, per-Node instance identity, or multiple Nodes per user.
- Durable inbox, task queue, run journal, renderer snapshot, delivery outbox, or persisted polling
  cursor.
- Recovery or replay of queued/running work after `SIGKILL`, host crash, or power loss.
- A Store worker, actor system, workflow engine, generic retry framework, or new repository layer.
- Exactly-once backend execution or provider delivery across process crashes.

Transient network failures recover while the process remains alive. An abnormal process exit may
lose in-memory tasks and output; users may submit them again after restart.

## Release checks

- Node tests and Clippy pass without warnings.
- Workspace line coverage remains at least 80%.
- v3 migration, identity isolation, conditional removal, singleton locking, and bounded Unicode
  output have regression tests.
- The release build is verified for the current host architecture.
- `spec/` matches the implementation.
