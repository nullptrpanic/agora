# Local Store

`agora-node` persists backend session mappings and conversation-scoped Agent controls in SQLite at
`~/.agora/db/store.db`. The Store is node-local and neutral: it does not import Agent or Channel
implementations, parse backend output, or perform network or Agent I/O.

The daemon acquires `~/.agora/node.lock` before opening SQLite and holds it for its lifetime. A
second Node for the same operating-system user fails immediately. There is no configurable
`state_dir` or multi-Node namespace. On Unix, `~/.agora` and its database directory use mode `0700`;
the lock, database, WAL, and SHM files use mode `0600`.

## Persistent identity

Configured names alone are not sufficient because reusing a name for a different backend,
workspace, provider, or account must not resume or disable unrelated state. Schema v4 therefore
uses these non-secret identities:

```text
AgentIdentity = (configured_agent_name, backend_type, canonical_workspace)
ChannelIdentity = (configured_channel_name, provider_type, provider_account_identity)

shared session:
  (AgentIdentity, shared) -> agent_session_id

isolated session:
  (AgentIdentity, session, ChannelIdentity, channel_session_id) -> agent_session_id

disabled Agent:
  (ChannelIdentity, channel_session_id, AgentIdentity)
```

The workspace identity canonicalizes the nearest existing ancestor and appends a missing suffix
without creating the workspace. Lark uses its configured app ID as account identity. Telegram uses
the numeric bot ID before the colon in its token. Provider secrets and tokens are never persisted.

With `isolate: none`, one `AgentIdentity` intentionally shares a backend session across Channels and
conversations. With `isolate: session`, the complete Channel identity and normalized conversation
ID select an independent mapping. A backend session ID is unique per `AgentIdentity`, preventing
one backend conversation from being attached to two scopes of that Agent.

## Schema and migration

Schema version 4 contains only two tables:

- `agent_sessions` stores the identity-qualified opaque backend mapping and timestamps.
- `channel_session_agent_blocks` is a sparse denylist; absence means enabled.

The canonical v4 schema is embedded from `crates/agora-node/src/store/schema.sql`. Store operations
use the existing mutex-protected SQLite connection, WAL mode, foreign-key enforcement, a five-second
busy timeout, and short transactions. No transaction spans an `.await` or external I/O.

Version 3 is the only supported upgrade source. Migration runs in one exclusive transaction,
creates the v4 tables, and copies v3 rows under `legacy_v3` backend/provider/account/workspace
identities. Those rows remain inspectable but cannot match active v4 configuration. Any migration
failure rolls back the table changes and `user_version`. Unknown versions are rejected rather than
rewritten.

## Session operations

The daemon and Store apply compare-and-set semantics:

1. Read the optional mapping after the run reaches the front of its process-local FIFO.
2. Invoke the Agent fresh or with that exact opaque ID without holding the Store lock.
3. Observe a newly reported ID immediately through the synchronous Agent output callback, using
   the expected mapping as the compare-and-set guard; completion may repeat this observation.
4. Treat repeated equal observations as idempotent and reject a conflicting ID without overwriting
   the row.

Codex reports the session fact when `thread.started` is parsed, before awaiting channel output.
Timeout, cancellation, or a non-zero process exit therefore retain an already observed session
for the next resume or reset. Empty or changing IDs are rejected; session observations are not
UI output events and the Channel never owns their persistence.

A successful fresh Codex run must report a nonempty thread ID. A resumed run may omit the ID, but
if it reports one, it must equal the attempted ID. If the Agent classifies the attempted backend
session as missing before task execution, the daemon conditionally removes only that mapping and
retries the same task once without a session. The Agent must not request this destructive fallback
after observing execution or on ambiguous diagnostics. Codex requires a non-zero resumed command,
no observed stdout, untruncated stderr, and its explicit pre-execution rollout-not-found error; successful commands
or unrelated missing-session text retain their original mapping and are never automatically replayed.

`/reset` uses the scheduler barrier before reading the mapping. It asks the Agent to delete the
opaque backend session and then removes only the row that still contains the attempted ID. Backend
“not found” is idempotent success. Any other supported-backend deletion error preserves the row.
An Agent that does not support native deletion still permits removal of its local mapping.

Conversation-scoped `/agent enable`, `/agent disable`, list, and status operations use the complete
Channel and Agent identities. Changing provider account, backend type, or canonical workspace
therefore leaves old rows unmatched instead of applying them to the new configuration.

## Persistence boundary

The Store persists only session mappings and disabled-Agent rows. Task payloads, attachments,
polling offsets, queued/running work, renderer state, and delivery attempts remain in memory. A
normal restart keeps backend session mappings. An abnormal exit may lose in-flight work; the Node
does not replay it.
