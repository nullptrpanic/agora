# Agent Channel

Agent Channel is the first concrete subsystem in Agora.

A channel is a task input and run-event output adapter. It is not responsible for agent execution, planning, sandboxing, or artifact storage.

## Responsibilities

A channel implementation should:

- Receive or poll tasks from an external source.
- Normalize external messages into internal task envelopes.
- Claim, acknowledge, or checkpoint task delivery where the channel supports it.
- Publish run events back to the origin.
- Send channel-native replies for daemon commands without forwarding those commands to an agent.
- Apply channel-specific chunking, retry, rate limit, and formatting rules.

A channel implementation should not:

- Spawn agent commands directly.
- Interpret private agent reasoning.
- Decide global task plans.
- Own sandbox policy.
- Store long-term artifact lineage.

The concrete receive drivers (`ConfiguredChannel`, `LarkChannel`, and `TelegramChannel`) are not `Clone`. Their clonable senders share the API client and non-secret identity. Feishu senders also share staged replies until source admission is confirmed. Telegram senders share their native interrupt registry. Receiver tasks, ACK/offset state, fragment/reconnect state, permissions, and receive caches remain exclusively on the receive side. The daemon routes through `ChannelSender`; receiver send conveniences delegate to that same implementation instead of constructing half-initialized channel copies.

## Initial Channels

Planned order:

1. Lark channel for the first end-to-end MVP.
2. Telegram channel as the second active adapter and boundary validation.
3. HTTP polling channel for generic integration.
4. Local channel when a dedicated development adapter becomes useful.

## Lark Channel Shape

The Lark channel receives IM messages through a local long-lived event subscription. Each normally completed agent run produces two native messages quoting the source message in its conversation: a collapsible `message_cot` process and a separate ordinary `post` answer. Agent-run and permission replies do not use interactive cards.

The receive side must be implemented inside `agora-node` as a native WebSocket long connection based on the official Lark/Feishu long-connection documentation and official SDK protocol behavior. It must not depend on local developer CLIs, shell commands, or third-party Lark channel wrapper crates. The channel adapter should request the Lark WebSocket endpoint with the configured app credentials and consume `im.message.receive_v1` events through a capacity-one delivery boundary. A supported message must contain a non-empty sender identity before it can reach permission checks. WebSocket frame reading, Ping/Pong, and ACK writes remain independent from task admission: at most 64 ordinary admission jobs and eight reserved jobs for legacy interrupt actions and text stop candidates may be in flight across all reconnects of that configured Channel, and overload receives code `500` without creating an unbounded task queue. The receiver enqueues frames in wire order. It checks permissions and normalizes one event at a time per conversation. One attachment-bearing message downloads at a time per channel. Other conversations can proceed while it downloads. The original sender and actual bot mentions still determine permission and command routing. One admission has a 60-second end-to-end deadline covering local queueing, permission checks, normalization, attachment downloads, daemon capacity reservation, and initial visible publication. Deliverable events are deduplicated by their stable event id; an in-progress duplicate shares the original result, while a successful code `200` result is replayed for ten minutes from a 4096-entry cache. Transient code `500` results are not cached and remain eligible for redelivery. A normalized task crosses the channel boundary as a neutral deferred delivery. Lark returns code `200` only after the daemon has bounded the task, atomically admitted its complete run fan-out, and confirmed every initial visible queued reply, or has successfully sent a command or overload reply. Dropping or rejecting that delivery returns code `500`. A permanent attachment failure such as a non-retryable HTTP 4xx response or the task size/count limit is logged and acknowledged with code `200` so it cannot redeliver forever; transient transport, HTTP 408/429/5xx, body-read, queue-overload, and admission-timeout failures use code `500` for redelivery. Malformed or unsupported event payloads are logged when appropriate and acknowledged with code `200` because replay cannot make those provider payloads valid.

The Lark implementation lives under `channel/lark/`. `lark_api.rs` owns the concrete Lark transport client, app credentials, shared HTTP client, WebSocket protocol, tenant token acquisition, image and management-card transport. Its private `lark_api/native.rs` implements COT creation, event append, completion, and native post delivery. `channel.rs` owns event parsing, task normalization, receive-loop composition, and the generic `Channel` implementation. `reply.rs` owns native run publication, coalescing, retries, and image delivery; `reply/content.rs` projects neutral output into bounded chronological COT events and a separate answer. `card.rs` renders only management replies with their existing buttons. `mod.rs` publicly re-exports only the `LarkChannel`, `LarkSender`, `LarkTask`, and `LarkRun` boundary types required by the public channel traits. Lark configuration belongs to the public `config` module. The API client, protocol frames, event payloads, reply targets, receiver, and reply/card implementations are visible only inside `channel::lark`, not to other crate modules.

The Lark receive loop is long-lived. It should log channel startup, connection attempts, successful connections, normal disconnects, and startup or post-connection failures without logging credentials or WebSocket endpoint URLs. If the WebSocket is closed, endpoint bootstrap fails, the local network is temporarily unavailable, or an established connection stops receiving frames, the channel should log the failure and reconnect indefinitely with bounded backoff. The complete direct or proxied WebSocket connection and TLS handshake has a 10-second deadline, every WebSocket write has a 10-second deadline, and an established connection fails when no inbound frame arrives within `max(2 * effective PingInterval, 30 seconds)`. Any inbound data, Ping, Pong, or Close frame refreshes the idle deadline. Establishing a WebSocket resets reconnect backoff even when the established connection later ends with an error, so an earlier startup outage cannot delay recovery after a healthy connection. A bad or unsupported event payload must not terminate the daemon process. If the receiver worker returns, fails, or panics, `Channel::recv` clears the closed receiver and its next supervisor attempt starts a new worker. Dropping the receiver aborts its owned worker; cancellation while joining a worker must retain that ownership.

The transport should classify frames by `header.event_type` before parsing an agent task. `im.message.receive_v1` becomes a message task. Other well-formed event types, including `im.message.message_read_v1`, produce an explicit ignore result that retains the event type, are acknowledged with code `200`, and are neither queued nor logged as errors. Malformed JSON and events without `header.event_type` are permanent parsing errors: they are logged, acknowledged with code `200`, and never delivered to the daemon.

For supported incoming messages, Lark and Telegram log operational identifiers, normalized text, its original UTF-8 byte length, and the attachment count before handing the task to the daemon. The logged text is escaped onto one line and bounded to 2048 UTF-8 bytes including a `[truncated]` marker; attachment bytes are never logged.

Lark HTTP calls, including WebSocket endpoint bootstrap and message API calls, should allow at most 10 idle connections per host, close idle connections after 300 seconds, use a 10-second connect timeout, and use a 60-second default total request timeout. Native COT and post publications use a shorter 10-second per-request timeout.

For the first Rust implementation, endpoint bootstrap should call the official long-connection endpoint path `/callback/ws/endpoint` with `AppID` and `AppSecret`, then connect to the returned `wss://...` URL. WebSocket event frames should be decoded from the protobuf frame shape used by the official SDK: headers carry values such as `type`, `message_id`, `trace_id`, `sum`, and `seq`, while the event JSON sits in the frame payload. The ACK should reuse the received event frame headers, add `biz_rt`, and set the payload to a JSON response with HTTP-style `code`.

Multi-part events are reassembled by `message_id` and zero-based `seq` before JSON parsing or admission. Each connection owns its own fragment cache: at most 64 messages, 64 parts per message, 1 MiB of aggregate payload, and five seconds from the first fragment. Expired entries are purged on the next received event; disconnect drops the cache. Identical duplicates do not advance completion, conflicting fragments are rejected, and invalid metadata or capacity failures receive code `500`. Incomplete valid messages receive no ACK; completion reuses the completing frame's headers. Admission permits are acquired only for complete messages, so fragments never occupy the task queue or interfere with Ping/Pong.

Expected shape:

```text
Lark WebSocket message event
  -> channel normalizes text and image references
  -> channel downloads referenced message images into neutral task attachments
daemon finds agents subscribed to the channel
  -> daemon starts one run per subscribed agent
first run event
  -> channel creates one native COT process linked to the source message
output from one agent
  -> channel appends process events to that COT and accumulates answer text
each agent run completes, fails, or is stopped
  -> channel closes that COT and sends a quoted native post answer unless the user stopped the run
```

For Lark group messages with mentions, normalization resolves the current bot identity and removes leading mention placeholders only when they address that bot and precede a slash command. Each exact provider `key` must have a whitespace/end boundary. Ordinary prompts, standalone mentions, mentions of other users, embedded mentions, and the remaining command/prompt text are preserved; permission checks still use the original mention identities. Messages whose trimmed text starts with `/` are node commands, not ordinary agent input. `CommandRuntime` resolves them through an immutable recursive command registry before the daemon opens agent runs. Each registered node may have a default handler, child commands, or both; an exact child match takes precedence over the current node's default handler. `/stop` cancels every active or queued run in the current `(channel_name, session_id)` conversation; `/stop {agent_name}` limits cancellation to that configured agent in the same conversation. `/reset` resets every agent subscribed to the current channel for the command's conversation and does not accept an agent argument. `/ask {agent_name} {prompt...}` routes the prompt only to the named agent subscribed to the current channel. This one invocation bypasses the current frontend conversation's disabled-agent filter without changing its persisted enablement state. `/agent disable {agent_name}` and `/agent enable {agent_name}` control whether one subscribed agent receives subsequent ordinary messages in the current frontend conversation. `/agent list` reports every current subscription and `/agent status {agent_name}` reports one. `/help` lists top-level command entries only. `/help {command path}` generates help for the selected node, including `/help stop`, `/help reset`, `/help ask`, and `/help agent enable`. Argument-taking commands do not reserve `help` as a name: `/stop help` targets the agent named `help`. `/ask help` still returns help when the required prompt is missing; `/ask` without required arguments returns usage. Only the first whitespace separator after the agent name belongs to command syntax; the remaining prompt is an original input slice preserving indentation, newlines, and leading/trailing whitespace. Unknown commands, unsubscribed agent names, and invalid arguments receive a channel-native reply and must not reach any agent.

The command subsystem owns parsing, validation, generated help, and command execution. `stop` and `reset` own their command subtrees; the existing ask command owner registers both targeted `/ask` execution and `/agent` management. Each captures only the concrete shared state it needs. `AgentDispatcher` remains responsible for enabled-agent filtering of ordinary input, FIFO admission, agent execution, backend-session mapping, and run-event publication; it exposes no command-specific stop, reset, status, enable, disable, or targeted-ask methods. Daemon composition uses the command runtime's control classification and admitted handler, receiving only neutral pass-through, reply, or selected-agent dispatch outcomes.

Agent enablement is independent of backend session isolation. Its persistent
identity is `(ChannelIdentity, channel_session_id, AgentIdentity)`, including
when the Agent uses `none` isolation; the concrete non-secret identity fields
are defined in `local-store.md`. Disabling an agent does not stop or remove existing running or queued tasks and does not delete its backend session; filtering happens before the daemon opens a run for a later ordinary task. A targeted `/ask {agent_name} {prompt...}` is an explicit per-request override and may open a run for that disabled agent, while leaving the denylist row unchanged. If no subscribed agent is enabled, an ordinary message receives a reply that the conversation has no enabled agents instead of being silently dropped.

Channels may also deliver a structured `CommandRequest` separately from message text. A generic request contains a command path plus named string arguments and carries no channel-specific action enum. Interactive command replies may use a neutral `ChannelButton` containing display text, style, and one request. A running agent instead receives an optional `InterruptCallback` through `ChannelRunContext`; this callback targets that exact run and carries no command path or channel presentation metadata. Telegram registers callbacks in the shared channel-neutral registry behind random 128-bit opaque ids and renders a native `结束任务` button. Lark uses the native COT stop interaction, which sends an ordinary `/stop` message through the existing command path; it does not register a new per-run card callback. Its legacy card callback handler remains available for existing messages. Registrations are process-local and one-shot; random ids do not repeat when a daemon restarts, so a stale button cannot resolve to an unrelated new run. Legacy Lark callbacks use the reserved `agora_interrupt` card value, while Telegram prefixes its callback data with `agora_interrupt:`. Lark retains at most 4096 observed chat conversation classifications for resolving later card actions and evicts the oldest inserted entry when that bound is reached. When an older Lark card omits the embedded conversation marker, admission resolves the classification from this cache and stores it on the neutral task before daemon dispatch, so a cloned sender can render the reply without owning or sharing the receive cache.

The Lark legacy interrupt handler checks actor and conversation permissions outside the ordinary message queue. An authorized legacy card button receives the expired-control guidance. Native runs use the private `/stop` or group `@bot /stop` message. A successful stop updates the existing process to `已停止`. From client 7.73 the native stop control is hidden in groups by default. Typed commands remain available. The Lark `/agent list` card separately uses `ChannelButton` for one `启用` or `禁用` command at the right of each agent row. That request carries the agent name; the management handler applies it to the callback event's channel session and returns a refreshed neutral list. Structured commands must never be forwarded to an agent.

The channel receive loop remains active while agent runs execute and while a `/reset` waits on its scheduler barrier, so later events can enter bounded admission without waiting for agent or channel output. `Channel::recv` returns a neutral task plus a deadline and one-shot receipt; an unresolved receipt requests channel redelivery. The daemon reserves one bounded task slot, routes tasks in receive order through a short admission chain, atomically reserves the complete run fan-out, opens every channel run, and publishes an initial `Queued` state before accepting the receipt. Failure or deadline expiry before that boundary starts no backend. Capacity exhaustion sends `服务繁忙，请稍后重试。` and accepts the source only if that reply succeeds; scheduler closure during shutdown rejects the source for redelivery. The receive-order chain is released immediately after scheduler insertion, reset-barrier insertion and stop signaling, or an ordinary command handler returns, before channel I/O. Later routing and help can proceed while earlier replies or resets drain, without overtaking the earlier task's registration. Eight per-channel control slots remain available independently of ordinary task slots; one bounded busy-reply worker prevents overload notices from blocking intake. The task slot remains held through every terminal publication. On Unix, each command-helper invocation runs in its own process group. Cancelling a command-based run drops its execution future and terminates that complete group rather than only the direct backend child. When the direct child exits, the helper terminates residual members of that process group and drains inherited output pipes for at most one second of pipe waiting. A descendant that escaped through `setsid` or another process group is outside that kill boundary, but cannot indefinitely retain the run through stdout/stderr. On drain expiry the helper logs the condition, finishes the available output, and preserves the direct child's exit status. This is not a whole-process-tree or cgroup guarantee; configured execution timeout and cancellation remain in force. `/stop` does not remove or change the persisted session mapping.

For `/reset`, `/stop`, and `/agent enable|disable`, the daemon identifies the command by channel account, conversation, and source task ID. Concurrent deliveries join one command execution. Reply failures and admission timeouts leave the result available for another delivery to send. The process-local replay window retains up to 4096 outcomes for ten minutes. At capacity, it evicts the oldest confirmed reply first. Active and recently unconfirmed commands keep their entries. Read-only commands and ordinary agent requests keep their own routing behavior.

Each admitted run owns separate execution and completion lifetimes. The execution ticket preserves `SessionKey` FIFO ordering; after reaching the front, it acquires the normalized workspace permit and then the global backend permit. The daemon releases the permits and ticket after backend execution and session-store update but before publishing any terminal state, so later work for the same session or workspace can proceed without waiting for channel I/O. A completion guard retains run capacity and graceful-shutdown accounting until `Completed`, `Failed`, `Stopped`, or `Interrupted` publication returns. Started, later queued-position, and streaming-output publication failures are logged and isolated from backend execution. A failed terminal publication is returned and logged with `terminal_publication_failed=true` before the completion guard is released. Lark awaits its bounded terminal recovery under this guard. Telegram can continue retrying eligible edits in its channel-owned worker.

`/reset` derives the same `SessionKey` used by normal dispatch for each subscribed agent, places a non-run barrier in `ExecutionScheduler` for every affected FIFO, and stops all active or queued runs using those keys before deleting any mapping. New tasks for an affected key wait behind the barrier and therefore cannot resume the old session or race with mapping deletion. Barriers participate in ordering but are not exposed as cancellable agent runs or counted during shutdown. In `session`, only the command's channel-qualified conversation is affected. In `none`, the key is agent-wide, so runs and the shared backend session are reset across all channel conversations for that agent.

Backend session deletion is an optional agent capability. When a mapping exists, the daemon asks the agent to delete the opaque backend session before conditionally removing the SQLite row. `CodexAgent` performs `codex delete --force {session_id}`; a backend “thread/session not found” response is idempotent success. The custom command agent reports deletion as unsupported, in which case the daemon still removes the local mapping. If another supported-backend deletion fails, the mapping is preserved and the channel receives a reset-failed reply. A successful reset leaves no mapping, so the next task starts a fresh backend session.

The agent reply uses the documented native COT APIs on the configured Lark API client, with the existing credentials, proxy, and token cache. It does not depend on a developer CLI or the reference implementation's internal gateway. The [COT integration guide](https://bytedance.larkoffice.com/wiki/ZfyXwOQImiLURtkRPtWcf2B2nNh) specifies a separate final-answer message; the user explicitly accepted that two-message layout. The process and answer are not presented as an interactive card or silently merged.

- Create the process with `POST /open-apis/im/v1/message_cot?receive_id_type=chat_id`, using the task's `chat_id` as `receive_id`, its source `message_id` as `origin_message_id`, `reply_in_thread=false`, `enable_badge=false`, `update_feed_rank=false`, and `cot_hidden=false`. Retain the nonempty returned `cot_id` and `message_id`. The initial `Queued` publication must confirm creation and its initial events before source acceptance; its text identifies the configured agent and queue position.
- Append AG-UI events with `PUT /open-apis/im/v1/message_cot`. The envelope carries both returned identifiers and an `events` array. Each event has `event_type`, stringified JSON `content`, and a monotonically increasing millisecond `timestamp` string. Retrying an event preserves all three fields for the service's deduplication. Append at most 50 events and 256 KiB per request.
- Lark omits backend `Thinking` summaries from the native process. It renders `Commentary` and operational `Progress` as ordinary `TEXT_MESSAGE_*` paragraphs in chronological order, regardless of language or formatting. The bounded recent-text window suppresses exact ID/text repeats. Changed snapshots become new paragraphs.
- `ToolCall` and `CommandExecution` use stable native tool ids with `TOOL_CALL_START`, chunked `TOOL_CALL_ARGS`, `TOOL_CALL_END`, then `TOOL_CALL_RESULT`. `END` closes the argument stream and means execution has begun, not that the tool has completed. A shell retains its command in the argument object; a generic tool retains its description. Titles have at most 96 characters with an explicit ellipsis. A repeated terminal result does not duplicate a tool; changed results use the same tool id. A generic tool with no separate output shows only `完成`, `失败`, or `已停止` beneath its title. A changed failure or stop description that could not be published as a title update is retained in the result. Shells use the terminal-like code layout below. A changed tool title uses `TOOL_CALL_CHUNK` on the existing id with an empty argument delta, followed atomically by `TOOL_CALL_END`; do not restart or recount the tool. Because `CHUNK` reopens the native argument stream, republish any known terminal result after this metadata update, even if its content is unchanged or the later snapshot says running. Unchanged snapshots remain deduplicated, and an exhausted ordinary event budget defers result restoration to reserved terminal capacity. The official 7.75 playground verified an initially unknown query changing to a specific query, with a single tool and a `完成` result. Title updates obey the ordinary event budget and unchanged titles produce no event.
- `ToolOutput` updates that same tool's result as a JSON-stringified native `{type:"code", language, code}` result. Lark shell results use `language=bash`, with `$ command` first, literal output next, and a status footer last (`✓ Success`, `✗ Failed` with a nonzero exit code, or stopped/unconfirmed status). An optional observed interval follows the status and is marked approximate. A completed shell with no output event still shows the command and status; an explicitly empty output is labeled. Shell titles start with `执行命令 ·`. The argument stream carries the full command when the run event budget permits. The panel echoes the full command and gives it priority over output. If the command and footer cannot fit in one result event with a truncation marker, the panel shows a UTF-8-safe prefix and `[命令已截断]`. Generic tool output retains `language=text` and its existing status/time header plus literal output. A missing duration does not invent one or erase a previously known duration. Unknown tool ids and events after the run terminal are ignored. An output received before terminal tool status waits for that status, or closes honestly with the run's unresolved-tool status. Native result updates replace the previous result, not append; never split a single output into replacement events expecting concatenation. The official 7.75 preview verifies this command/output/footer order in monospaced text. The documented native component has no controls for a Codex-style copy button, gradient, custom border, or right-aligned footer, and the language field currently does not enable syntax highlighting. Those client-owned details are not synthesized with card/CSS fields or claimed as implemented.
- Flyout grouping, tool counts, disclosure styling, active animation, and font metrics belong to the native client. Configure Lark lifecycle text as `运行中`, `思考中`, `任务已完成`, `失败`, `已停止`, or `已中断`. The official [AG-UI playground](https://feishu-agui-playground.gf-boe.bytedance.net/?case=success) was visually checked in its 7.75 mode: the collapsed completed process uses a right-pointing arrow. The renderer does not insert a decorative glyph or invent an arrow-direction API field. This preview check is not a live deployment/chat verification, and older clients may render differently.
- Lark and Telegram use the same localized tool labels for structured `WebAction` values. A pending search reads `搜索资料` and updates to `搜索 · <queries>` when terms arrive. Page opening and in-page search include the available URL and pattern. The channels omit typed `Diagnostic` items and preserve ordinary progress, tool failures, final errors, and agent-authored prose. Each renderer escapes text for its own native format.
- Ignore `Usage` in Lark presentation, including inside the process and in every lifecycle state. Preserve the neutral event and backend accounting for other consumers. Never duplicate final answer text inside the process.
- Keep the latest known tool result even if the ordinary event budget is exhausted, publishing it from reserved terminal capacity. At terminal state, close unresolved tools honestly (`未确认完成` if no result was received, or `已停止` after cancellation), append `RUN_FINISHED` (`done` or `interrupted`) or `RUN_ERROR`, and call `POST /open-apis/im/v1/message_cot/complete/{cot_id}?message_id=...&reason=done|error`. Do not label an unfinished tool successful. Late lifecycle/output events cannot mutate terminal content.
- Send the separate final answer through `POST /open-apis/im/v1/messages/{source_message_id}/reply` as `msg_type=post`, `reply_in_thread=false`, and a stable per-answer UUID. Successful runs contain ordinary Markdown answer text without an extra heading; an empty successful answer becomes `任务已完成。`. Failed or interrupted runs contain their safe lifecycle notice and retain available answer text as `部分回答`. A user-stopped run only changes its existing native process to `已停止`: do not send a new answer, stop explanation, or partial-answer post, and do not edit an answer already sent before an image upload. Already published content remains in place. Raw lifecycle errors and credentials stay in daemon logs. A COT failure does not suppress an available terminal answer.
- Accumulate answer deltas locally and send the answer at terminal state. When an image is produced and answer text is already available, send that text before the upload. An early answer may receive one final `PUT /open-apis/im/v1/messages/{answer_message_id}` update after completion. Do not edit a post on every text delta: native posts permit at most 20 updates. Image attachments remain separate native image messages.

The projection retains at most 64 tools, 64 recent text id/snapshot pairs, 8192 UTF-8 bytes per ordinary process text or generic tool output, and 20 KiB of answer text. Shell output uses the shared 500-character preview. A run admits at most 800 ordinary events and 380 of any one type, then reserves enough events to close all retained tools, report omitted process output, and terminate; it stays below the service's 1000 total and 500 per-type bounds even when publication drains the pending queue. Logical text/tool start groups are accepted atomically so a limit cannot leave a partially introduced item. Text and serialized argument deltas are split by their JSON-escaped byte cost, leaving their individual event contents below 1200 bytes. A typed tool result preserves its command and status/time footer for shells within the panel limit above (status/time header for other tools) and the newest UTF-8-safe output tail with an explicit truncation marker, budgeting all nested JSON escaping and the complete wire event within 4096 bytes. The API rejects oversized events before authentication or delivery; append batches remain at most 50 events and 256 KiB. Excess process content is explicitly reported; it cannot displace the independently retained answer. The post builder also budgets both layers of JSON escaping within 30,000 serialized bytes, keeping the leading lifecycle/partial-answer notice and newest answer tail with an explicit truncation marker.

Ordinary process updates use one coalescing worker and a 400-millisecond update interval; initial, started, queued-position, and terminal publications flush immediately. A bounded pending event queue retains unsent immutable events across failures. State locks are released before authentication or HTTP; a separate publication lock serializes process and answer delivery. Sleeping ordinary workers hold only a weak reference. Every configured channel shares its existing tenant-token cache: positive `expire`, ten-percent early refresh capped at 60 seconds, and rejection of empty tokens/invalid lifetimes. HTTP 401 or business codes 99991663, 99991664, or 99991671 invalidate only the matching cached token and retry once with fresh authentication; permission errors do not trigger refresh.

COT creation has no documented idempotency key. Each Feishu source message and subscribed agent share the same pending run across admission retries. An unconfirmed create, including a cancelled request, falls back to a quoted native post with a stable UUID. The post first shows `排队中` and is edited to the final answer or `已停止`. When a provider accepted a COT request but lost its response, its message ID is unavailable for later updates. The daemon defers failed initial admission without publishing a task failure to Feishu. Accepting the source releases the pending run entries. A channel retains at most 4096 pending source messages with a ten-minute retry window.

Append, completion and UUID-protected posts retry transient transport errors, HTTP 408/429/5xx, and business codes 10020/99991400 up to three times per request. A pending post keeps its original UUID and body until a message ID is confirmed. Terminal publication can make three additional flush attempts after one, two and four seconds within a 60-second recovery budget. The run keeps its completion guard throughout those attempts. Normal shutdown uses the existing five-second wait. Permanent or exhausted failures are returned and logged. Images are sent independently of text retries.

Multiple subscribed agents each own their process/answer pair in the same source conversation. Reply targets are derived from incoming events, not daemon CLI flags; `chat_id` identifies the session and COT recipient but never replaces `origin_message_id` or the source answer reply target. Permission denials use the same native `post` shape and shared denial Markdown, without an interactive-card banner/frame, while preserving the reason, observed identifiers, and configuration example.

Every newly sent textual reply quotes the triggering message through the native reply endpoint with `reply_in_thread=false`; process creation and image replies also use false. This preserves the native `回复某人：原消息` reference without forcing a new topic. A source message that already belongs to a topic may still be replied to inside that topic by the provider; this does not migrate historical messages. The native process closes as an expandable `任务已完成` row, with client-owned disclosure styling. Group replies also mention that message's sender; private (`p2p`) replies do not add mentions. `LarkReplyTarget` retains the source `message_id` and an optional `mention_user_id` inside the channel, derived from the incoming conversation type; neither routing metadata nor mention formatting is delegated to the agent. In groups, native posts begin with an `at` node for the sender followed by the Markdown body, ordinary text replies prepend documented `<at user_id="...">` markup, and new management-card replies prepend card-specific `<at id="...">` markup. Private replies omit those prefixes. Terminal post edits and truncation preserve the original mention choice. Callback replies use the embedded/cached conversation classification and mention the actor only for a confirmed group; unresolved callbacks do not guess a group mention. Mention ids are escaped for markup and empty, oversized, or `all` identities are rejected before sending, so this single-person reply path cannot introduce an @all. Concurrent replies retain their individual source/mention pair. Existing management-card edits remain in-place updates rather than new notifications. The process and attachment messages retain the native source-message reference; individual process updates do not repeatedly mention the sender.

Management replies remain JSON 2.0 interactive cards with `body.elements` and `config.update_multi=true`; their existing command buttons and callback protocol are preserved. They quote the source message without forcing a topic and patch their returned card id. They retain the 30,000-byte serialized limit and explicit oversized-list fallback with textual per-agent management guidance. API size checks apply to both card creation and patching. JSON 2.0 Markdown rendering requires client 7.20 or newer.

Structured command replies remain channel-neutral. A successful cancellation request returns typed `StopAccepted` with its existing fallback acknowledgement text. Lark and Telegram acknowledge an accepted stop by updating each affected run in place. They suppress the separate command receipt. No-active-run, invalid-command, permission-denied, and expired-control responses remain visible. The daemon does not inspect the channel name or remove replies globally. For `/agent list`, Lark quotes the source message with a compact JSON 2.0 `当前对话的 Agent 状态` card, displays the current-conversation scope, and renders exactly one right-aligned `启用` or `禁用` button for each subscribed agent. Each command callback value carries the same `agora_conversation` classification as the source task. A button callback patches the original status card using `event.context.open_message_id`; it must not create a new reply for every toggle. `/agent status` and text-based enable or disable commands render a compact single-agent status card without a button. Registry-generated root, node, and leaf help use neutral multiline text replies so every channel can render the same command reference without interactive-card knowledge. Telegram renders the same neutral list or status as text and does not imitate unsupported interactive controls.

The MVP processes `text`, `post`, and `image` messages. A `post` retains its text and every inline `image_key`; text nodes within one post line are joined with spaces and distinct lines remain separated by newlines. An `image` message contributes an image without text. Before dispatch, the Lark channel downloads each image through `/open-apis/im/v1/messages/{message_id}/resources/{image_key}?type=image` and places its MIME type and bytes in a neutral task attachment. One normalized task may contain at most 16 images and 67108864 attachment bytes (64 MiB) across all images. The count is rejected before token acquisition or image download. The HTTP client rejects an oversized declared length before buffering and enforces the same bound while reading a response without a trustworthy length. A permanent download or limit failure discards that event after acknowledgement; a transient failure requests redelivery. Card messages, files, audio, video, and other rich message types remain unsupported.

## Telegram Channel Shape

The Telegram implementation lives under `channel/telegram/`. `telegram_api.rs` owns the concrete Bot API client, token, shared HTTP client, update polling, Rich Message delivery, edits, and rate-limit retry. `channel.rs` owns task normalization, session derivation, command addressing, receive-loop composition, and the generic `Channel` implementation. `rich_message.rs` owns run state, Rich Markdown rendering, coalescing, heartbeat, and terminal delivery. The parent `channel` module can construct and aggregate the Telegram channel boundary types, but the API client and renderer remain private to `channel::telegram`.

The receive side uses `getUpdates` long polling with `limit: 1` and an in-memory offset. It accepts text and photo messages from private chats, groups, and supergroups. A message without Telegram's authenticated `from` object is ignored before permission checks rather than being represented by an empty sender id. For a photo, it downloads the largest variant into one neutral image attachment and applies the same 67108864-byte (64 MiB) per-task declared-and-streamed limit as Lark. A normalized agent or command task retains its update id until the daemon accepts the neutral delivery; acceptance advances the offset, while rejection or receipt drop retains the old offset for ordered redelivery. Permission denials, callbacks, malformed or unsupported updates, and permanent image failures remain channel-owned and advance immediately. A transient transport, body-read, HTTP 429, or HTTP 5xx failure retries that update across at most three polls, then logs, discards, and advances it so one unavailable image cannot block later updates indefinitely. The update id is the task id. A normal chat uses `chat:{chat_id}` as its channel session; a forum topic uses `chat:{chat_id}:topic:{message_thread_id}`. Commands suffixed with the current bot username, such as `/stop@agora_bot`, are normalized before daemon command routing. Commands explicitly addressed to another bot are ignored.

Each subscribed agent opens an independent Telegram run. A run with an interrupt callback uses a persistent Rich Message even in private chats so the stop button stays available; this is the normal daemon path. A private run without an interrupt callback uses `sendRichMessageDraft`; a heartbeat refreshes the draft and a terminal event persists the bounded final content. Groups and forum topics always use persistent send/edit delivery. Content that exceeds one provider message is split into bounded persistent parts and later retries reuse already-created part IDs. Topic output retains the source `message_thread_id`. The channel coalesces ordinary snapshots and flushes terminal state immediately. Its ordinary update worker keeps ownership through delivery and dirty-state checks; its bounded retry worker retains its scheduled flag while awaiting delivery. A slow request must not produce an unbounded queue of update or retry workers. Heartbeats are started only for callback-free private drafts.

Cancelling a pending `Channel::recv` must not lose its deferred Telegram acknowledgement. The channel retains the acknowledgement receiver until its disposition is ready, then clears the pending state and applies acceptance or retry without another await. A later receive therefore advances the offset for an accepted delivery even when an earlier receive future was dropped.

Telegram treats an explicit `editMessageText` business error code `400` with the provider's `Bad Request: message is not modified` diagnostic as an idempotent successful edit. This classification belongs to the Telegram API adapter, not the renderer or daemon. Other errors and non-idempotent sends remain failures. A multipart terminal retry edits known part IDs and can send a missing part again after an explicit API rejection. Before each new persistent send the renderer reserves its part index. An interrupted request or unreadable success response leaves the index unconfirmed. Later flushes preserve that reservation to prevent duplicate messages.

Every rendered Telegram part must fit the renderer's 32768-character and 400-structure-point budgets, including prefixes, separators, escaped characters, and preformatted wrappers. If an already-valid prefix leaves insufficient room for the first wrapped character or line, send that prefix separately before splitting the next section. Splitting preserves every escaped character and never cuts a Unicode character.

Telegram quotes the source and edits one Rich Message per agent for ordinary runs. The answer appears as Markdown at the top. Below it, a native details block opens while waiting for the answer. Its heading shows the latest tool action while that step is running, or the localized run status otherwise. It collapses when the answer arrives and reads `已完成` on success. The expanded timeline starts with the first public step. A configured agent name appears in a footer after the steps. The generic name `agent` is omitted. Before the first step, the body can show the agent name and queue or waiting information. A completed run with no public steps or named agent has a short empty-state message. Telegram controls the disclosure indicator. A manual stop replaces all text with `已停止` and removes the stop button. Failure and system interruption keep their localized notice and any partial answer. Callback-free private drafts show the latest public activity in a native thinking indicator and return to the run status when the latest tool finishes.

Telegram renders public commentary and operational progress in chronological order. A single tool call is directly expandable. Two or more consecutive calls form an `已调用 N 次工具` group, open while waiting for an answer and collapsed once the answer arrives or the run ends. Each call ID keeps its first position as its status and output update. A shell row has a short action title, for example `运行 cargo test` or `请求 qq.com` for a curl command with one recognized literal HTTP target. Ambiguous commands fall back to the executable or `执行命令`. Expanding the row shows a native Bash block with the original command, escaped literal output, status, optional exit code, and an observed duration when supplied. A successful ordinary tool with no output shows `完成`. Both messaging channels preview the last 500 Unicode scalar values of each shell output before escaping. Longer output gets a local `[输出已截断]` marker. This display limit alone does not produce a run-wide omission notice or change the output available to the agent. Both messaging channels omit model summaries, backend diagnostics, and token statistics. Actual errors remain visible.

Telegram retains the latest 64 process entries and up to 256 KiB of answer text. Ordinary process text and generic tool output retain up to 8192 UTF-8 bytes each. A shell command retains up to 16384 escaped characters and 128 newlines before a marked prefix is necessary. The separate output preview leaves room for the input and status. It bounds escaped text and line counts within each process entry, then gives the answer priority over older rows when assembling a message. Omitted process history is marked. Oversized active answers use an escaped tail. Terminal answers that require more than one provider message use the existing bounded multipart delivery. Raw transport failures and credentials stay in local logs. General Telegram file/document input, webhook intake, persisted update offsets, and subscription filters remain outside this MVP.

All fixed node-authored user-facing copy is centralized under `agora-node/src/i18n/`, with the current catalog in `zh_cn.rs`. Channel renderers own only channel-specific Markdown, card JSON, colors, icons, and layout. Protocol field names, internal validation errors, logs, and agent-authored prose are not localized or rewritten. Lark formats structured web-action metadata and its generated attachment placeholder using the scoped display policy above; other channels retain their own rendering.

## Group conversation context

Both messaging channels use the same permission gate and Codex input protocol for authorized group and direct-chat messages. An unmentioned message in a denied group is consumed silently. An explicit mention in that group produces only permission guidance. Native reply targets and group mentions of the human sender belong to the originating channel. Private replies omit the mention. A manual stop keeps its status-only presentation.

For an admitted Feishu or Telegram message, Node writes one JSON object to Codex stdin. The two top-level fields are `user_request` (the message text, or the parsed question for a targeted `/ask`) and `context`. The channel takes `channel`, `chat_type`, `chat_id` and `message_id` from the inbound event. It includes `thread_id`, `reply_to_message_id` and structured member `mentions` when available. IDs are strings. Images on the request still use Codex's `--image` argument. Custom executables keep their existing stdin contract.

```json
{"user_request":"看下这个问题","context":{"channel":"lark","chat_type":"group","chat_id":"oc_demo","message_id":"om_question","reply_to_message_id":"om_parent"}}
```

External skills available to the Codex process can use these identifiers to read the chat or the referenced message. Their configured identities determine access. A skill lookup takes place in the current Codex run and follows its session isolation. Node supplies no instruction prefix or message history, maintains no Telegram transcript, and treats the model's final text as an answer, including JSON.

## HTTP Channel Shape

HTTP task intake should use polling because it works from local nodes behind NAT and firewalls.

Output should use WebSocket because agent execution is naturally streaming.

Expected shape:

```text
node -> server: poll tasks
node -> server: claim task or renew lease
node -> server: open run event WebSocket
node -> server: stream run events
node -> server: complete or fail task
```

WebSocket messages should be structured run events, not raw terminal bytes.

Example event kinds:

- `run.started`
- `output.chunk`
- `output.truncated`
- `run.completed`
- `run.failed`
- `run.cancelled`
- `heartbeat`

Each event should eventually carry a run id and monotonically increasing sequence number so reconnect and replay can be supported.

## Agent Boundary

Channel adapters should communicate with the daemon in terms of tasks and run events.

`ChannelTaskInput` is the channel-independent receive contract from the top-level `task` module. `Message(TaskContent)` carries text plus zero or more typed attachments; `Command(CommandRequest)` carries a structured command path and named arguments. Attachment bytes are shared when one message task is cloned for multiple subscribed agents. Neither variant contains Lark resource keys, callback payload shapes, cards, or agent backend details.

The daemon should communicate with configured agents in terms of agent tasks, optional opaque backend session ids, streamed output, and completion outcomes. It must not construct backend commands or parse backend protocols. The daemon may persist the neutral association between a channel session and an agent session, but only the agent implementation interprets that backend session id.

Each agent implementation owns its execution strategy. A command-based agent may use `agent::command` for child-process startup, stdin, stdout, stderr, and exit status. Another agent may implement a PTY, sandbox, or network execution strategy without changing the daemon or the command helper.

The command helper is deliberately lower level than an agent. It has no output-format enum and knows nothing about Codex JSONL, session resume, channels, cards, or run events.

## Structured Agent Output

The Agent-to-daemon output boundary should carry semantic, backend-neutral events rather than undifferentiated text. The first event model needs these concepts:

- `Thinking`: a user-visible reasoning summary supplied by the agent backend.
- `Commentary`: an identified user-visible intermediate message from the agent. It is distinct from reasoning summaries and tool invocations.
- `ToolCall`: an identified non-command tool operation with display text and a running, completed, failed, or stopped status. Reusing an id updates the existing tool instead of duplicating or recounting it. Optional `web_action` describes search queries, page opening, or page find without channel-specific text; `text` remains the fallback for consumers that do not use those details.
- `Diagnostic` carries an identified nonfatal backend warning. Lark and Telegram omit these events from conversation replies. The agent classifies the diagnostic and retains the local log. Actual task and tool failures use their own event types.
- `Progress`: an identified operational notice (for example plan or attachment delivery status) with concise text and a running, completed, failed, or stopped status. It does not imply a tool invocation. Reusing an id updates the existing notice.
- `CommandExecution`: an identified, complete backend command with a running, completed, failed, or stopped status and an optional backend-reported exit code. The agent adapter must not normalize, sanitize, or truncate the command. Keeping this semantic distinction in the neutral boundary lets each channel choose a terminal-native presentation without parsing agent-authored text.
- `ToolOutput` carries textual output for an existing `ToolCall` or `CommandExecution` ID, with optional agent-observed `elapsed_ms`. Each messaging channel updates the corresponding tool and owns its native formatting and truncation.
- `Answer`: user-facing answer text. Multiple chunks append in order when a backend supports answer deltas.
- `Image`: an identified output image with owned bytes, a detected media type, and a fixed safe filename. `OutputImage` accepts PNG, JPEG, and WebP signatures up to 10,000,000 bytes; it is not a full image decoder. Its debug representation never contains image bytes. Channels do not receive or open backend filesystem paths.
- `Usage`: backend-reported input, cached input, output, and reasoning-output token counts for one completed run.

These events describe presentation semantics without exposing Codex, Lark, JSONL, cards, or another adapter's protocol. An agent owns backend event classification. The daemon forwards the neutral event without interpreting its text. A channel owns presentation, truncation, coalescing, and rate limiting.

The boundary must not expose or reconstruct private chain-of-thought. `Thinking` is limited to reasoning summaries that the backend explicitly emits for user consumption; intermediate user-facing prose uses `Commentary`. Codex owns classification of its JSONL items: pending non-final agent messages become commentary, commands remain `CommandExecution`, file changes/MCP calls/web searches become `ToolCall`, plan notices remain `Progress`, and nonfatal backend warning items become `Diagnostic`. The daemon forwards these events unchanged. Each channel owns the rendering of commentary, tools, and their results.

### Output Images

Generated images are sent as separate native image replies to the source conversation; image delivery does not change each channel's text, code-block, link, or usage-display policy. Final answer text is published before awaiting its image attachment uploads so slow attachment delivery does not hold back an available answer. Lark uploads multipart image bytes to `/open-apis/im/v1/images` with `image_type=message`, then replies to the original message with `msg_type=image` and `reply_in_thread=false`. Telegram calls multipart `sendPhoto` with the original `chat_id`, optional `message_thread_id`, and source `reply_parameters`. Both reuse the configured channel HTTP client and proxy.

Each run accepts at most eight image ids and 32,000,000 aggregate bytes. The channel claims an id before I/O and ignores repeated ids and images arriving after a terminal state. One image delivery, including authentication and upload, has a 30-second budget and holds no run-state lock during network I/O. Agent cancellation can drop the pending delivery; an already accepted external send cannot be recalled. Lark can refresh authentication once after an explicit rejection, retaining an already uploaded image key. Neither channel automatically repeats an uncertain image send, and text/card retry never resends image attachments. The run process shows sending, delivered, or unconfirmed delivery; image failure does not discard the text answer or convert successful agent execution into failure. The provider remains responsible for validating complete image contents and platform-specific dimensions. Unsupported/invalid images produce visible progress warnings, not silent success.

Codex owns image extraction. Completed successful MCP image content is decoded from base64. A final answer's Markdown image references are parsed with `pulldown-cmark`; local/file/sandbox references and supported base64 data URLs become owned images and a short attachment-status placeholder. Inline/fenced code and non-image text/links retain their source bytes. HTTP(S) image references are left unchanged and are never fetched by Node. Lark changes the generated attachment placeholder from “本话题” to “本对话” at rendering; Telegram retains its existing wording. Local paths are canonicalized and must lie in the configured workspace or the current Codex session's generated-image directory; regular-file, size, and no-follow checks reject ordinary symlink escapes and special files. This path policy is not a filesystem sandbox against a malicious process concurrently replacing ancestor directories. Each file read has a five-second limit. Content deduplication, eight-image/32,000,000-byte limits, and a 32-reference attempt budget bound extraction.

The current `codex exec --json` protocol does not expose native image-generation completion items. For that backend path, the adapter also collects new PNG files from exactly `CODEX_HOME/generated_images/<thread_id>` (falling back to `HOME/.codex` for Codex home and honoring agent environment overrides). It snapshots resumed-session filenames **before spawning the child** and collects only newly named files at turn completion or output finalization; existing filenames, other sessions, symlinks, and subdirectories are not replayed. For a fresh thread, whose id is learned from `thread.started`, only files modified since run preparation are considered. Session path components are restricted to safe ASCII ids. Each directory scan is bounded to 4,096 entries and three seconds; failures produce a warning without failing text output. The adapter does not scan the workspace for arbitrary newly created files, delete generated files, or collect a remote executor's private filesystem. Explicit references to older permitted images still support resending in a later turn. These are backend-specific extraction rules; daemon and channels do not know Codex paths or sessions.

## Autonomy And Dependency Rule

Every agent and every channel is autonomous. An agent and a channel must not import, call, configure, or otherwise depend on each other's implementation.

An agent owns:

- Backend execution strategy.
- Backend protocol and output decoding.
- Backend-specific session arguments and session-result classification.

A channel owns:

- External connection and authentication state.
- Message receipt, acknowledgement, retry, and reconnection.
- Channel-native sender, conversation, group, and mention identity extraction.
- Enforcement of the configured channel permission policy before task delivery.
- Channel-specific reply targets, formatting, rate limits, and updates.

Channel permission enforcement happens before the daemon boundary. Private messages require an allowed sender. Group messages require both an allowed sender and an allowed group, followed by the selected group's optional mention requirement. Missing permission configuration denies all access; `*` is the explicit allow-all value. Exact group rules take precedence over the wildcard group rule. Unauthorized events never create a `ChannelTask`. A denied private message receives the denial reason, observed channel-native identifiers, and a configuration example. A denied group message receives that guidance only when it explicitly mentions the current bot; otherwise it is consumed silently to avoid adding noise to ordinary group conversation. Structured actions use the same identity policy without a mention requirement and retain their denial reply. New Lark command callback values carry their `private` or `group` classification, so a newly generated card remains authorizable after channel reconstruction without persisted channel state. Legacy callbacks without the marker fall back to the process-local session classification; if that context is also absent, permission evaluation remains unresolved and fails closed. The shared permission layer owns structured denial content (channel name, user id, optional group id, reason, and the configuration example) and the identical Markdown body used by both platforms. The example keeps the real `channels` and `permission` hierarchy while replacing unrelated existing channel fields with a JSONC `// ...` placeholder; it does not invent channel-specific type, name, or credential fields. Each concrete channel owns its platform container and escaping: Lark sends the shared Markdown body as a native `post`, while Telegram sends it as Rich Markdown. The shared pure token-number formatter remains available for Telegram usage display; Lark does not display token statistics. Process-state accumulation, ordering, clipping, and layout remain platform-owned because their semantics differ.

The daemon is the composition boundary. It converts a channel task into an agent task and adapts agent output and outcomes into channel run events. Shared contracts at this boundary must stay neutral: they must not expose Lark, Codex, PTY, card, or other adapter-specific details.

The daemon derives a neutral process-local isolation scope and a persistent key
from the configured Agent and resolved Channel session. The Store key includes
the non-secret Agent backend/workspace and Channel provider/account identities
defined in `local-store.md`, then associates it with one opaque
`agent_session_id`. The daemon and Store do not inspect the backend ID or decide
how an Agent resumes it. Channel and Agent implementations remain independent
of the Store and of each other.

The daemon is a long-running process. A transient channel receive error, closed channel connection, or failed task reply should be logged and isolated to that channel or task; it should not stop the whole daemon. Channel implementations remain responsible for their own external reconnection semantics.

External setup and I/O failures must be returned or logged as errors; the node must not panic while initializing logging, its async runtime, channel HTTP clients, or reply state.

```text
Channel task
  -> daemon run
  -> session-store lookup
  -> configured agent
  -> agent-owned execution and output decoding
  -> session-store update
  -> channel run event
```

## Command-Based Agents And Session Resume

The first Codex and custom agent implementations execute one child process per accepted channel message. Task input is written to the child stdin, stdout and stderr are delivered to the agent implementation, and child exit terminates the run. Only exit status zero produces `RunEvent::Completed`; a non-zero status, including the conventional `128 + signal` value, produces `RunEvent::Failed` so every channel renders failure rather than a successful terminal state.

The child process remains one-shot, while Codex conversation context is associated with the configured isolation scope and survives daemon restarts. In `none`, all channel conversations for one configured agent resolve to one shared scope. In `session`, each `(channel_name, channel_session_id)` resolves to a separate scope. Both modes execute in the configured workspace. A first turn uses `codex exec --json --color never --skip-git-repo-check [configured options] -`; a mapped turn uses `codex exec resume --json --skip-git-repo-check [configured options] {thread_id} -`. Optional model, effort, and backend `agent_sandbox` settings are owned by `CodexAgent` and applied to both forms. Skipping the repository check permits intentionally non-Git workspaces without changing sandbox or approval settings. The agent sandbox is distinct from session isolation and Agora's future runtime sandbox.

Codex JSONL output provides the thread id through `thread.started`. `CodexAgent` keeps the event format internal and maps supported events to neutral output:

- Completed `reasoning` items become `Thinking` summaries.
- Started, updated, and completed command execution items update one `CommandExecution` entry by item id; every update preserves the complete backend command and terminal items retain their optional `exit_code`.
- Completed commands additionally publish their `aggregated_output` as `ToolOutput`. Completed MCP calls publish joined text content blocks (or a tool error message when no text exists); binary/image/audio content and raw protocol envelopes are not rendered as log text. Image delivery remains independent. The adapter observes intervals from `item.started` to `item.completed` for commands and MCP calls, before publishing completion, using at most 64 active timers and bounded non-empty ids. This is approximate observed wall time, not backend-reported execution/CPU time; missing starts or untracked calls produce no duration. Empty MCP output without a known interval keeps the status-only result.
- File changes, MCP tool calls, and web searches become `ToolCall` entries; todo lists remain `Progress`. Web items additionally carry optional neutral `WebAction` data. Parse `action.type` to distinguish search, open-page, and find-in-page. Search queries use nonempty `action.queries`, then `action.query`, then the legacy top-level `query`; open/find preserve their URL and pattern. Whitespace is normalized and missing values remain absent. Keep existing `ToolCall.text` as the fallback so other channels retain their display policy.
- The latest pending `agent_message` becomes `Answer` when `turn.completed` arrives. A closing `todo_list` or a completion snapshot for an already-started tracked tool preserves that candidate. The JSONL reader scans each new stdout chunk once, including partial lines across reads.
- Supported MCP image content, final-answer local/data image references, and the current session's newly generated native image files become `Image` events as described above.
- A valid `turn.completed.usage` object becomes `Usage`. Input plus Output forms Total; Cached Input and Reasoning Output are subsets and must not be added again.
- An earlier `agent_message` followed by more work becomes `Commentary` instead of a final answer.

`CodexAgent` reports the thread id immediately through the neutral synchronous `AgentOutput::session_started` callback when `thread.started` is parsed, and includes it again in the normal completion outcome. It owns native deletion of that thread when requested. Backend session ids remain opaque outside the agent and must not enter the channel abstraction. The one-shot `codex exec --json` stream does not provide answer token deltas, so the final answer may appear as one update. True answer-delta streaming through Codex app-server is follow-up work.

Raw Codex stderr is backend diagnostic output rather than agent reply content. `CodexAgent` logs chunks immediately, including unterminated lines and recoverable resume errors, and does not publish them to channels. It retains only a 16 KiB diagnostic tail; once any bytes have been discarded, missing-session classification cannot authorize a replay. Nonfatal Codex `item.type=error` warnings become `Diagnostic` and are logged immediately, deduplicated in a per-run window of 64 normalized messages limited to a 2048-character prefix plus a truncation marker. Such items do not flush a pending final answer into commentary. Top-level JSONL `error` and `turn.failed` messages remain user-visible agent output; tool failure status and image-delivery warnings also remain visible. This distinction follows the Codex JSONL event type, not text matching.

The process-local `SessionKey` and persistent `StoreSessionKey` represent the
same configured isolation boundary; the persistent form additionally includes
configuration identities so stale rows cannot match a changed Agent or Channel.
In `none`, all runs for one configured Agent share one FIFO and serialize even when they originate from different Channels. In `session`, each channel-qualified conversation has its own FIFO, but backend executions that resolve to the same normalized workspace still serialize across conversations and configured Agents. Distinct workspaces may execute concurrently up to the configured global limit. Every admitted run first receives `RunEvent::Queued { ahead }`; positive `ahead` reflects preceding same-key work, while zero includes waiting for workspace or global capacity. The daemon publishes `RunEvent::Started` only after FIFO and execution resources are available.

Mappings are persisted in SQLite at `~/.agora/db/store.db`. A successful fresh
Codex run must return a nonempty thread ID, and a resumed run may not report a
different ID from the one it was asked to resume. Conflicts fail without
overwriting the mapping. If an Agent reports that a mapped backend session was
rejected before task execution, the daemon conditionally removes that stale mapping, retries the
task once without a session, and stores the newly returned session ID.
Backend-specific missing-session detection stays inside the Agent implementation.
Codex reports that rejection only for a resumed command with a non-zero exit,
no observed stdout, and an explicit `Error: ... no rollout found for thread id ...`
diagnostic (with the optional `thread/resume failed: ` prefix). Successful exits,
any execution or unrecognized stdout, and generic stderr mentions of missing
sessions must not trigger replay or removal of the attempted mapping. Backend
stderr remains available in local logs, including a recoverable resume rejection.
Persistent PTY execution remains future work.

Current MVP behavior:

- Read stdout and stderr in bounded chunks and let each agent decide what becomes user-visible output. The custom agent keeps independent incomplete UTF-8 tails for stdout and stderr so a multibyte character split across reads remains intact; invalid bytes and an incomplete final tail use lossy replacement only when they are known not to form a later valid character.
- Write child stdin while concurrently draining stdout and stderr so bounded pipes cannot deadlock; report Unix signal termination as `128 + signal`.
- Enforce each agent's execution timeout and combined raw stdout/stderr byte limit after it reaches the front of its FIFO, terminating the whole child process group when either boundary is exceeded.
- Let each agent decode its own output; Codex captures thread ids and normalizes JSONL reasoning, progress, and answer events.
- Persist identity-qualified isolation-scope-to-Agent-session mappings without exposing backend protocol details to the Store or Channel.
- Publish a native COT process and a separate post answer quoting the source Lark message.
- Project Lark commentary, tools, and operational progress into bounded chronological native events. Preserve tool IDs and coalesce intermediate event batches.
- Poll Telegram text updates, preserve forum-topic session boundaries, and stream one independent Rich Message per subscribed agent.
- Use persistent Telegram messages for interruptible private runs, groups and topics; callback-free private runs use drafts followed by bounded persistent terminal parts.
- Publish final status from agent completion or execution failure.
- Route slash commands before agent dispatch, support conversation-scoped `/stop` cancellation with a stopped run event, support barrier-protected `/reset` session deletion, and persist conversation-scoped `/agent` controls.
- Surface same-isolation-scope FIFO position as Queued with a live preceding-task count, then transition to Running when backend execution can start.
- Before graceful process shutdown, interrupt every running or queued run and wait up to five seconds for terminal channel updates. If a channel supervisor fails, perform the same bounded interruption before propagating that error from `Daemon::run`.

Cancellation, graceful shutdown notification, and configured execution limits are process-local; in-flight run recovery across daemon restarts remains deferred. `SIGKILL`, `abort`, power loss, and other ungraceful termination paths cannot update channel state before exit.

## Trait Style

Agora follows the current Rust style used in this repository:

- Do not use `async_trait`.
- Prefer `fn method(...) -> impl Future<Output = Result<...>> + Send` for asynchronous trait methods.
- Prefer enum aggregation for multiple implementations.
- Keep unstable traits inside `agora-node` until there is real reuse pressure.

Illustrative shape:

```rust
pub trait ChannelSender {
    type Task: ChannelTask;
    type Run: ChannelRun;

    fn name(&self) -> &str;
    fn identity(&self) -> ChannelIdentity;

    fn open_run(
        &self,
        task: &Self::Task,
        context: ChannelRunContext,
    ) -> impl Future<Output = anyhow::Result<Self::Run>> + Send;

    fn reply(
        &self,
        task: &Self::Task,
        reply: ChannelReply,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

pub trait Channel: ChannelSender {
    type Sender: ChannelSender<Task = Self::Task, Run = Self::Run>
        + Clone + Send + Sync + 'static;

    fn sender(&self) -> Self::Sender;
    fn recv(
        &mut self,
    ) -> impl Future<Output = anyhow::Result<Option<ChannelDelivery<Self::Task>>>> + Send;
}

pub trait ChannelTask {
    fn task_id(&self) -> &str;
    fn session_id(&self) -> &str;
    fn input(&self) -> &ChannelTaskInput;
}

pub struct ChannelRunContext {
    pub agent: ChannelAgent,
    pub interrupt: Option<InterruptCallback>,
}

pub struct ChannelAgent {
    pub name: String,
}

pub trait ChannelRun: Clone {
    // A retryable admission ended before the backend ran. The default closes
    // the initial presentation. Feishu preserves it across source redelivery.
    fn admission_deferred(&self) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn publish(
        &self,
        event: RunEvent,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

pub trait Agent {
    fn run<O>(
        &self,
        request: AgentRequest,
        output: &mut O,
    ) -> impl Future<Output = anyhow::Result<AgentOutcome>> + Send
    where
        O: AgentOutput + Send;
}
```

The daemon should depend on this boundary or an enum aggregation of it, such as `ConfiguredChannel`. Channel-specific details such as Lark reply targets, cards, WebSocket event frames, and HTTP polling cursors should stay inside the channel implementation.

A channel run represents the reply/output context for one accepted task and one agent. When multiple agents subscribe to a task, the daemon opens one run per agent. The channel decides whether that run becomes a Lark process/answer pair, Telegram messages, one HTTP WebSocket stream, or another channel-specific representation.

This is a direction, not a frozen API.

## Node admission and observation hardening (2026-09-05)

An AgentOutput synchronous session observation is delivered when Codex parses thread.started, before subsequent awaits. Daemon applies the existing Store compare-and-set immediately, independently of successful exit, timeout or cancellation. Channel output does not carry session metadata. Empty/changing IDs are rejected; truncated stderr cannot authorize a fresh replay.

All fan-out runs open before publishing. Initial publications run in the existing JoinSet behind a shared start gate. Only a completely published batch starts execution; failure or cancellation closes the gate and allows up to five seconds for channel-specific admission cleanup. Feishu keeps the initial reply for provider redelivery. No detached cleanup worker or durable transaction is introduced.

Each channel has eight reserved control admissions and one bounded busy-reply worker in addition to ordinary task slots. The receive-order chain is released after scheduler insertion (or reset barrier insertion), not after network publication. This prevents /stop from overtaking a not-yet-registered earlier task while allowing it to proceed despite a slow queued reply or reset completion. Backend dispatch never uses the control pool; shutdown rejects both paths.

Codex stderr is logged as chunks arrive, including unterminated lines; only a 16 KiB diagnostic tail is retained for classification. Lark reconnect diagnostics retain typed HTTP/business codes and transport categories, not credential URLs or peer text. Daemon logs a failed terminal publication, while Channel alone owns retry-budget exhaustion reporting.
