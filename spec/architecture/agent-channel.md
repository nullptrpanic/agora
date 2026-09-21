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

The concrete receive drivers (`ConfiguredChannel`, `LarkChannel`, and `TelegramChannel`) are not `Clone`. Only their lightweight senders are clonable: API client, non-secret identity, and shared interrupt registry. Receiver tasks, ACK/offset state, fragment/reconnect state, permissions, and receive caches remain exclusively on the receive side. The daemon routes through `ChannelSender`; receiver send conveniences delegate to that same implementation instead of constructing half-initialized channel copies.

## Initial Channels

Planned order:

1. Lark channel for the first end-to-end MVP.
2. Telegram channel as the second active adapter and boundary validation.
3. HTTP polling channel for generic integration.
4. Local channel when a dedicated development adapter becomes useful.

## Lark Channel Shape

The first Lark MVP should receive IM messages through a local long-lived event subscription and send output through an interactive card.

The receive side must be implemented inside `agora-node` as a native WebSocket long connection based on the official Lark/Feishu long-connection documentation and official SDK protocol behavior. It must not depend on local developer CLIs, shell commands, or third-party Lark channel wrapper crates. The channel adapter should request the Lark WebSocket endpoint with the configured app credentials and consume `im.message.receive_v1` events through a capacity-one delivery boundary. A supported message must contain a non-empty sender identity before it can reach permission checks. WebSocket frame reading, Ping/Pong, and ACK writes remain independent from task admission: at most 64 ordinary admission jobs and eight separately reserved interrupt-action jobs may be in flight across all reconnects of that configured Channel, and overload receives code `500` without creating an unbounded task queue. One admission has a 60-second end-to-end deadline covering local queueing, permission checks, normalization, attachment downloads, daemon capacity reservation, and initial visible publication. Deliverable events are deduplicated by their stable event id; an in-progress duplicate shares the original result, while a successful code `200` result is replayed for ten minutes from a 4096-entry cache. Transient code `500` results are not cached and remain eligible for redelivery. A normalized task crosses the channel boundary as a neutral deferred delivery. Lark returns code `200` only after the daemon has bounded the task, atomically admitted its complete run fan-out, and published every initial `Queued` card, or has successfully sent a command or overload reply. Dropping or rejecting that delivery returns code `500`. A permanent attachment failure such as a non-retryable HTTP 4xx response or the task size/count limit is logged and acknowledged with code `200` so it cannot redeliver forever; transient transport, HTTP 408/429/5xx, body-read, queue-overload, and admission-timeout failures use code `500` for redelivery. Malformed or unsupported event payloads are logged when appropriate and acknowledged with code `200` because replay cannot make those provider payloads valid.

The Lark implementation lives under `channel/lark/`. `lark_api.rs` owns the concrete Lark transport client, app credentials, shared HTTP client, WebSocket protocol, tenant token acquisition, card sending, and card patching. `channel.rs` owns event parsing, task normalization, receive-loop composition, and the generic `Channel` implementation. `card.rs` owns card state, rendering, and update delivery. `mod.rs` publicly re-exports only the `LarkChannel`, `LarkSender`, `LarkTask`, and `LarkRun` boundary types required by the public channel traits. Lark configuration belongs to the public `config` module. The API client, protocol frames, event payloads, reply targets, receiver, and card implementation are visible only inside `channel::lark`, not to other crate modules.

The Lark receive loop is long-lived. It should log channel startup, connection attempts, successful connections, normal disconnects, and startup or post-connection failures without logging credentials or WebSocket endpoint URLs. If the WebSocket is closed, endpoint bootstrap fails, the local network is temporarily unavailable, or an established connection stops receiving frames, the channel should log the failure and reconnect indefinitely with bounded backoff. The complete direct or proxied WebSocket connection and TLS handshake has a 10-second deadline, every WebSocket write has a 10-second deadline, and an established connection fails when no inbound frame arrives within `max(2 * effective PingInterval, 30 seconds)`. Any inbound data, Ping, Pong, or Close frame refreshes the idle deadline. Establishing a WebSocket resets reconnect backoff even when the established connection later ends with an error, so an earlier startup outage cannot delay recovery after a healthy connection. A bad or unsupported event payload must not terminate the daemon process. If the receiver worker returns, fails, or panics, `Channel::recv` clears the closed receiver and its next supervisor attempt starts a new worker. Dropping the receiver aborts its owned worker; cancellation while joining a worker must retain that ownership.

The transport should classify frames by `header.event_type` before parsing an agent task. `im.message.receive_v1` becomes a message task. Other well-formed event types, including `im.message.message_read_v1`, produce an explicit ignore result that retains the event type, are acknowledged with code `200`, and are neither queued nor logged as errors. Malformed JSON and events without `header.event_type` are permanent parsing errors: they are logged, acknowledged with code `200`, and never delivered to the daemon.

For supported incoming messages, Lark and Telegram log operational identifiers, normalized text, its original UTF-8 byte length, and the attachment count before handing the task to the daemon. The logged text is escaped onto one line and bounded to 2048 UTF-8 bytes including a `[truncated]` marker; attachment bytes are never logged.

Lark HTTP calls, including WebSocket endpoint bootstrap and card API calls, should allow at most 10 idle connections per host, close idle connections after 300 seconds, use a 10-second connect timeout, and use a 60-second total request timeout.

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
  -> channel replies to the source message with one interactive card for that agent
output from one agent
  -> channel updates that agent's card by message_id
each agent run completes, fails, or is stopped
  -> channel performs that agent card's final update
```

For Lark group messages with mentions, normalization resolves the current bot identity and removes leading mention placeholders only when they address that bot and precede a slash command. Each exact provider `key` must have a whitespace/end boundary. Ordinary prompts, standalone mentions, mentions of other users, embedded mentions, and the remaining command/prompt text are preserved; permission checks still use the original mention identities. Messages whose trimmed text starts with `/` are node commands, not ordinary agent input. `CommandRuntime` resolves them through an immutable recursive command registry before the daemon opens agent runs. Each registered node may have a default handler, child commands, or both; an exact child match takes precedence over the current node's default handler. `/stop` cancels every active or queued run in the current `(channel_name, session_id)` conversation; `/stop {agent_name}` limits cancellation to that configured agent in the same conversation. `/reset` resets every agent subscribed to the current channel for the command's conversation and does not accept an agent argument. `/ask {agent_name} {prompt...}` routes the prompt only to the named agent subscribed to the current channel. This one invocation bypasses the current frontend conversation's disabled-agent filter without changing its persisted enablement state. `/agent disable {agent_name}` and `/agent enable {agent_name}` control whether one subscribed agent receives subsequent ordinary messages in the current frontend conversation. `/agent list` reports every current subscription and `/agent status {agent_name}` reports one. `/help` lists top-level command entries only. `/help {command path}` generates help for the selected node, including `/help stop`, `/help reset`, `/help ask`, and `/help agent enable`. Argument-taking commands do not reserve `help` as a name: `/stop help` targets the agent named `help`. `/ask help` still returns help when the required prompt is missing; `/ask` without required arguments returns usage. Only the first whitespace separator after the agent name belongs to command syntax; the remaining prompt is an original input slice preserving indentation, newlines, and leading/trailing whitespace. Unknown commands, unsubscribed agent names, and invalid arguments receive a channel-native reply and must not reach any agent.

The command subsystem owns parsing, validation, generated help, and command execution. `stop` and `reset` own their command subtrees; the existing ask command owner registers both targeted `/ask` execution and `/agent` management. Each captures only the concrete shared state it needs. `AgentDispatcher` remains responsible for enabled-agent filtering of ordinary input, FIFO admission, agent execution, backend-session mapping, and run-event publication; it exposes no command-specific stop, reset, status, enable, disable, or targeted-ask methods. Daemon composition uses the command runtime's control classification and admitted handler, receiving only neutral pass-through, reply, or selected-agent dispatch outcomes.

Agent enablement is independent of backend session isolation. Its persistent
identity is `(ChannelIdentity, channel_session_id, AgentIdentity)`, including
when the Agent uses `none` isolation; the concrete non-secret identity fields
are defined in `local-store.md`. Disabling an agent does not stop or remove existing running or queued tasks and does not delete its backend session; filtering happens before the daemon opens a run for a later ordinary task. A targeted `/ask {agent_name} {prompt...}` is an explicit per-request override and may open a run for that disabled agent, while leaving the denylist row unchanged. If no subscribed agent is enabled, an ordinary message receives a reply that the conversation has no enabled agents instead of being silently dropped.

Channels may also deliver a structured `CommandRequest` separately from message text. A generic request contains a command path plus named string arguments and carries no channel-specific action enum. Interactive command replies may use a neutral `ChannelButton` containing display text, style, and one request. A running agent instead receives an optional `InterruptCallback` through `ChannelRunContext`; this callback targets that exact run and carries no command path or channel presentation metadata. Lark and Telegram register callbacks in the shared channel-neutral registry behind random 128-bit opaque ids and render channel-native `结束任务` buttons. Registrations are process-local and one-shot; random ids do not repeat when a daemon restarts, so a stale button cannot resolve to an unrelated new run. Lark uses the reserved `agora_interrupt` card value, while Telegram prefixes its callback data with `agora_interrupt:`. Lark retains at most 4096 observed chat conversation classifications for resolving later card actions and evicts the oldest inserted entry when that bound is reached. When an older Lark card omits the embedded conversation marker, admission resolves the classification from this cache and stores it on the neutral task before daemon dispatch, so a cloned sender can render the reply without owning or sharing the receive cache.

The Lark run card uses this boundary for its `结束任务` button. Each card owns one registration whose opaque id resolves to the callback for exactly one execution ticket's `AgentRunControl`; triggering it is one-shot, removes the registration, and stops only that run. Reaching a terminal card state or dropping the card run removes an unused registration, independently of later delivery retries. Interrupt admission runs in the channel-owned action handler outside the capacity-one ordinary message queue, so image downloads, bot-identity lookups, and ordinary-message denial replies cannot block a stop button. The handler retains the same actor/conversation permission checks and shares only the bounded conversation-classification cache and callback registry. After signaling, it attempts a threaded text receipt distinguishing an accepted stop request from an ended or expired button with `/stop` guidance; feedback failure is logged and never reverses cancellation. Ordinary slash-command messages still pass through message normalization and admission. This callback is handled entirely inside the Lark channel and never enters `CommandRuntime`. The Lark `/agent list` card separately uses `ChannelButton` for one `启用` or `禁用` command at the right of each agent row. That request carries the agent name; the management handler applies it to the callback event's channel session and returns a refreshed neutral list. Structured commands are consumed by `CommandRuntime` and must never be forwarded to an agent.

The channel receive loop remains active while agent runs execute and while a `/reset` waits on its scheduler barrier, so later events can enter bounded admission without waiting for agent or channel output. `Channel::recv` returns a neutral task plus a deadline and one-shot receipt; an unresolved receipt requests channel redelivery. The daemon reserves one bounded task slot, routes tasks in receive order through a short admission chain, atomically reserves the complete run fan-out, opens every channel run, and publishes an initial `Queued` state before accepting the receipt. Failure or deadline expiry before that boundary starts no backend. Capacity exhaustion sends `服务繁忙，请稍后重试。` and accepts the source only if that reply succeeds; scheduler closure during shutdown rejects the source for redelivery. The receive-order chain is released immediately after scheduler insertion, reset-barrier insertion and stop signaling, or an ordinary command handler returns, before channel I/O. Later routing and help can proceed while earlier replies or resets drain, without overtaking the earlier task's registration. Eight per-channel control slots remain available independently of ordinary task slots; one bounded busy-reply worker prevents overload notices from blocking intake. The task slot remains held through every terminal publication. On Unix, each command-helper invocation runs in its own process group. Cancelling a command-based run drops its execution future and terminates that complete group rather than only the direct backend child. When the direct child exits, the helper terminates residual members of that process group and drains inherited output pipes for at most one second of pipe waiting. A descendant that escaped through `setsid` or another process group is outside that kill boundary, but cannot indefinitely retain the run through stdout/stderr. On drain expiry the helper logs the condition, finishes the available output, and preserves the direct child's exit status. This is not a whole-process-tree or cgroup guarantee; configured execution timeout and cancellation remain in force. `/stop` does not remove or change the persisted session mapping.

Each admitted run owns separate execution and completion lifetimes. The execution ticket preserves `SessionKey` FIFO ordering; after reaching the front, it acquires the normalized workspace permit and then the global backend permit. The daemon releases the permits and ticket after backend execution and session-store update but before publishing any terminal state, so later work for the same session or workspace can proceed without waiting for channel I/O. A completion guard retains run capacity and graceful-shutdown accounting until `Completed`, `Failed`, `Stopped`, or `Interrupted` publication returns. Started, later queued-position, and streaming-output publication failures are logged and isolated from backend execution. A failed terminal publication is returned and logged with `terminal_publication_failed=true` before the completion guard is released. This does not assert that channel-owned background retries have finished; Lark and Telegram separately report when their retry budgets are actually exhausted.

`/reset` derives the same `SessionKey` used by normal dispatch for each subscribed agent, places a non-run barrier in `ExecutionScheduler` for every affected FIFO, and stops all active or queued runs using those keys before deleting any mapping. New tasks for an affected key wait behind the barrier and therefore cannot resume the old session or race with mapping deletion. Barriers participate in ordering but are not exposed as cancellable agent runs or counted during shutdown. In `session`, only the command's channel-qualified conversation is affected. In `none`, the key is agent-wide, so runs and the shared backend session are reset across all channel conversations for that agent.

Backend session deletion is an optional agent capability. When a mapping exists, the daemon asks the agent to delete the opaque backend session before conditionally removing the SQLite row. `CodexAgent` performs `codex delete --force {session_id}`; a backend “thread/session not found” response is idempotent success. The custom command agent reports deletion as unsupported, in which case the daemon still removes the local mapping. If another supported-backend deletion fails, the mapping is preserved and the channel receives a reset-failed reply. A successful reset leaves no mapping, so the next task starts a fresh backend session.

Each card must use Lark card JSON 2.0, place components under `body.elements`, and set `config.update_multi` to `true` before and after updates. JSON 2.0 is required so standard Markdown block quotes render instead of displaying the `>` prefix literally; this requires Lark client 7.20 or newer. The initial card should reply to the incoming Lark message through `/open-apis/im/v1/messages/{message_id}/reply` with `msg_type=interactive` and `reply_in_thread=true`. This creates or joins the message thread rooted at the incoming message, so the user's request remains the visible topic context. The returned reply message id becomes the target for later card patches. The channel must not send the initial card as an unrelated chat message.

When multiple agents consume the same incoming message, each agent creates its own card reply in that message thread. All cards therefore share the same user-message topic while remaining independently updateable by reply message id.

The card header is the lifecycle indicator. It contains the agent name and a compact `排队中`, `运行中`, `已完成`, `失败`, `已停止`, or `已中断` status instead of repeating lifecycle prose in the body. The body uses one lightly framed process panel and keeps the final answer visually distinct:

- Retain at most 64 process phases, 64 progress entries per phase, and 8192 UTF-8 bytes per thinking/progress/command text. Render retained state in one native JSON 2.0 `collapsible_panel` titled `任务过程`, with a `grey-50` background, `grey-200` border, `8px` corner radius, and compact body padding. Do not render separate thinking and progress panels. When retention or the rendered 160-element budget drops older content, preserve the newest state and show a visible truncation notice.
- Treat each `Thinking` event as the start of a numbered display phase. Attach subsequent new `CommandExecution` and `Progress` events to that phase until another `Thinking` event arrives. This grouping communicates event order only; it must not be described as an agent-provided causal relationship. Execution received before the first thinking event belongs to an untitled phase rather than fabricated reasoning.
- Order phases oldest first so new thinking appears at the bottom, and number them sequentially from `01`. Order entries within each phase newest first. Updating an existing command or progress id keeps it in its original phase, updates its status and text, and moves it to the top of that phase.
- In the `任务过程` header, show the phase count and every nonzero progress status count. Use the same status symbols as the entries: a green check for completed, a blue dot for running, a red cross for failed, and a grey square for stopped.
- Render each phase as an unframed group inside the single process panel. Prefix its reasoning summary with a compact chronological phase number and matching `✦` marker, both in blue. Separate phases with whitespace instead of nesting additional cards or panels.
- Render every retained `CommandExecution` as one compact light console under its phase. A single light-grey outer container must wrap both the toolbar and command block so they read as one component. Put `SHELL` on the left and the semantic-color command status on the right of the toolbar, then render the retained backend command inside the same container as a native fenced `bash` Markdown code block with a `$` prompt. The native code block owns its client-rendered dark terminal surface, monospace syntax presentation, and horizontal scrolling for long lines; the renderer must not replace it with a custom image or WebView. The outer process panel owns collapse and expansion. Show `Running` while active, `Stopped` after cancellation, and `exit {code}` when the backend supplies a terminal exit code. Keep file changes, plans, intermediate agent messages, and other `Progress` entries outside the console as concise status-marked text. Use a green check for completed entries, a red cross for failed entries, a blue dot for running entries, and a grey square for stopped entries.
- Publish one initial queued card before source confirmation. When `ahead > 0`, render a grey `排队中` header and `> 正在排队，前面还有 {ahead} 个任务...`, refreshing the positive count whenever earlier same-key work leaves. At `ahead == 0`, render `> 已接收，正在等待可用执行资源...` while the run waits for its workspace and global permits. Publish `Started` and switch to the blue `运行中` state only after those resources are acquired.
- While a run is active and no answer is available, keep `任务过程` expanded. As soon as answer content begins, render the answer before `任务过程` and collapse the process by default; keep that answer-first hierarchy after completion, failure, stop, or interruption.
- Render the final answer in a separate, visually prominent `最终回答` section whose heading starts with a blue vertical marker.
- On failure, keep the red `失败` lifecycle header and render an always-visible `任务失败` section with a red vertical marker, a safe error category, and retry guidance. Put the category's technical note in a collapsed `技术详情` panel. Do not expose the raw error, command, stack trace, credentials, or other potentially sensitive details in the card; keep the complete error in daemon logs.
- If answer content already exists when a run fails, preserve it after the failure section but label it `部分回答` instead of `最终回答`.
- On stop, use a neutral grey `已停止` header, preserve all existing `Thinking` and `Progress` output, mark still-running progress entries as stopped, and show an always-visible `任务已停止` section. Preserve existing answer content as `部分回答`.
- While a run is queued or running, render one top-right danger button labeled `结束任务` before the visible output so long process content cannot move the action out of reach. Use a Lark card callback behavior whose value contains `agora_interrupt`, an opaque id owned by that channel instance, and `agora_conversation` set to `private` or `group`. Remove the button from completed, failed, stopped, and interrupted snapshots. The Lark app must subscribe to `card.action.trigger` over the same long connection; interrupt frames have eight reserved in-flight admission slots and invoke the channel-owned callback handler without creating a command task or waiting for ordinary-message normalization. The handler signals cancellation before attempting user feedback; the admission and ACK remain bounded by the event deadline.
- When the node begins a graceful shutdown, send `RunEvent::Interrupted` to every running or queued run before dropping the daemon future. Use an orange `已中断` header, preserve existing output, mark still-running progress entries as stopped, and show `任务已中断` with `Agora Node 即将退出，本次任务已中断，当前输出已保留。` followed by `Node 恢复后，请重新发送消息继续。`. Preserve existing answer content as `部分回答`. Do not expose the raw signal or failure reason in the card; keep it in daemon logs.
- When token usage is available, render the completed run's values as four equal-width, notation-sized columns without a separate usage heading: `Total`, `Input`, `Output`, and `Reasoning`. Center each label, emphasized value, and subdued detail; show cached input below `Input`, `tokens` below total and output, and `of output` below reasoning.
- Keep the answer and token usage outside the process panel so they remain visible. Before any answer is available, keep the active expanded process as the primary content. Once answer content begins, render it before the collapsed process panel; after completion, failure, stop, or interruption, preserve that hierarchy and render token usage last. Preserve the latest progress entries after completion while keeping the result as the primary content.

Before the first agent output arrives, a running card should render one blockquote placeholder, `> 正在等待 Agent 输出...`, so Lark does not display an empty card body. The placeholder disappears as soon as `Thinking`, `Progress`, or `Answer` content is available. Starting or successfully completing a run must not append additional lifecycle prose to the body.

Card updates should use latest-state coalescing rather than a backlog of output patches. The channel should keep only the newest card snapshot, publish ordinary intermediate changes no more often than once every 400 milliseconds, and flush terminal completed, failed, stopped, or interrupted state immediately. State mutation and snapshot construction must release the card state lock before any token or card HTTP call; a separate flush lock serializes publication. A single ordinary update worker owns the entire delay, send, and dirty-state check cycle, including time waiting for the flush lock; newer updates are handled by that same worker. Sleeping ordinary update workers hold only a weak reference, and a terminal card ignores later lifecycle/output mutations. One tenant-token cache is shared by all cards of the configured Lark Channel and uses the positive response `expire`, refreshing early by ten percent of its lifetime, capped at 60 seconds. Empty tokens and invalid lifetimes are rejected. HTTP 401 or explicit business codes 99991663, 99991664, or 99991671 invalidate only the matching cached token and retry the same publication once with a fresh token; permission errors do not trigger refresh. The initial reply and terminal flush must report delivery failures to the caller. Intermediate patch failures should be logged and may be superseded by a later snapshot. Because PATCH is idempotent, transient transport failures, HTTP 429 responses, and HTTP 5xx responses are retried up to three times with a short exponential backoff; apart from an explicit authentication rejection, the non-idempotent initial reply is not retried. If a terminal publication for an existing message fails transiently (transport/body-read failure or HTTP 408/429/5xx), one channel-owned worker retains the terminal snapshot after the run owner is released. It performs at most three additional flush attempts, delayed by one, two, and four seconds, under a 60-second total recovery deadline; each PATCH retains its existing three-attempt transport policy. It reuses the message id, never repeats an initial creation, never mutates terminal state, and releases the card on success, permanent rejection, attempt exhaustion, or deadline expiry. The interrupt registration is already removed before this worker starts. Permanent permission/business failures are not retried; exhausted recovery is explicitly logged with `terminal_publication_failed=true`. Recovery is process-local and does not survive daemon exit.

When multiple agents subscribe to the same Lark channel, the daemon should open one channel run per agent. Each run produces an independent card whose content and title identify the replying agent.

The Lark renderer retains at most 64 process phases, 64 progress entries per phase, 8192 bytes per process text, and 20 KiB of answer text. A rendered snapshot additionally reserves at most 160 nested process elements within the platform's 200-element card limit. When either bound is exceeded, it retains newest content on a UTF-8 boundary and prepends a visible `输出已截断` or omitted-history notice.

The final serialized Lark card JSON must also fit 30,000 UTF-8 bytes, including JSON escaping. Oversized run snapshots first drop old process phases/entries, then shorten the remaining process text, and only then shorten other display text if necessary. Retain lifecycle status, answer/partial-answer headings, newest content, usage, and an explicit truncation notice. Budgeting operates on the rendered snapshot, not retained history, and never rewrites callback IDs or command arguments. Oversized management replies use a small explicit limit notice and per-agent textual management guidance; they do not silently omit arbitrary buttons. The API rejects any still-oversized card before making an HTTP request; both initial reply and PATCH use the same size check.

Reply targets should be derived from incoming channel events, not daemon CLI flags. For `im.message.receive_v1`, the source `message_id` is the reply target. The `chat_id` remains the channel session id and must not be used to send an unrelated initial card.

Structured command replies remain channel-neutral. For `/agent list`, Lark replies in the source message thread with a compact JSON 2.0 `当前对话的 Agent 状态` card, displays the current-conversation scope, and renders exactly one right-aligned `启用` or `禁用` button for each subscribed agent. Each command callback value carries the same `agora_conversation` classification as the source task. A button callback patches the original status card using `event.context.open_message_id`; it must not create a new reply for every toggle. `/agent status` and text-based enable or disable commands render a compact single-agent status card without a button. Registry-generated root, node, and leaf help use neutral multiline text replies so every channel can render the same command reference without interactive-card knowledge. Telegram renders the same neutral list or status as text and does not imitate unsupported interactive controls.

The MVP processes `text`, `post`, and `image` messages. A `post` retains its text and every inline `image_key`; text nodes within one post line are joined with spaces and distinct lines remain separated by newlines. An `image` message contributes an image without text. Before dispatch, the Lark channel downloads each image through `/open-apis/im/v1/messages/{message_id}/resources/{image_key}?type=image` and places its MIME type and bytes in a neutral task attachment. One normalized task may contain at most 16 images and 67108864 attachment bytes (64 MiB) across all images. The count is rejected before token acquisition or image download. The HTTP client rejects an oversized declared length before buffering and enforces the same bound while reading a response without a trustworthy length. A permanent download or limit failure discards that event after acknowledgement; a transient failure requests redelivery. Card messages, files, audio, video, and other rich message types remain unsupported.

## Telegram Channel Shape

The Telegram implementation lives under `channel/telegram/`. `telegram_api.rs` owns the concrete Bot API client, token, shared HTTP client, update polling, Rich Message delivery, edits, and rate-limit retry. `channel.rs` owns task normalization, session derivation, command addressing, receive-loop composition, and the generic `Channel` implementation. `rich_message.rs` owns run state, Rich Markdown rendering, coalescing, heartbeat, and terminal delivery. The parent `channel` module can construct and aggregate the Telegram channel boundary types, but the API client and renderer remain private to `channel::telegram`.

The receive side uses `getUpdates` long polling with `limit: 1` and an in-memory offset. It accepts text and photo messages from private chats, groups, and supergroups. A message without Telegram's authenticated `from` object is ignored before permission checks rather than being represented by an empty sender id. For a photo, it downloads the largest variant into one neutral image attachment and applies the same 67108864-byte (64 MiB) per-task declared-and-streamed limit as Lark. A normalized agent or command task retains its update id until the daemon accepts the neutral delivery; acceptance advances the offset, while rejection or receipt drop retains the old offset for ordered redelivery. Permission denials, callbacks, malformed or unsupported updates, and permanent image failures remain channel-owned and advance immediately. A transient transport, body-read, HTTP 429, or HTTP 5xx failure retries that update across at most three polls, then logs, discards, and advances it so one unavailable image cannot block later updates indefinitely. The update id is the task id. A normal chat uses `chat:{chat_id}` as its channel session; a forum topic uses `chat:{chat_id}:topic:{message_thread_id}`. Commands suffixed with the current bot username, such as `/stop@agora_bot`, are normalized before daemon command routing. Commands explicitly addressed to another bot are ignored.

Each subscribed agent opens an independent Telegram run. A run with an interrupt callback uses a persistent Rich Message even in private chats so the stop button stays available; this is the normal daemon path. A private run without an interrupt callback uses `sendRichMessageDraft`; a heartbeat refreshes the draft and a terminal event persists the bounded final content. Groups and forum topics always use persistent send/edit delivery. Content that exceeds one provider message is split into bounded persistent parts and later retries reuse already-created part IDs. Topic output retains the source `message_thread_id`. The channel coalesces ordinary snapshots and flushes terminal state immediately. Its ordinary update worker keeps ownership through delivery and dirty-state checks; its bounded retry worker retains its scheduled flag while awaiting delivery. A slow request must not produce an unbounded queue of update or retry workers. Heartbeats are started only for callback-free private drafts.

Cancelling a pending `Channel::recv` must not lose its deferred Telegram acknowledgement. The channel retains the acknowledgement receiver until its disposition is ready, then clears the pending state and applies acceptance or retry without another await. A later receive therefore advances the offset for an accepted delivery even when an earlier receive future was dropped.

Telegram treats an explicit `editMessageText` business error code `400` with the provider's `Bad Request: message is not modified` diagnostic as an idempotent successful edit. This classification belongs to the Telegram API adapter, not the renderer or daemon. Other errors and non-idempotent sends remain failures. A multipart terminal retry reuses known message IDs and proceeds past an unchanged earlier part to deliver missing later parts.

Every rendered Telegram part must fit the renderer's 32768-character and 400-structure-point budgets, including prefixes, separators, escaped characters, and preformatted wrappers. If an already-valid prefix leaves insufficient room for the first wrapped character or line, send that prefix separately before splitting the next section. Splitting preserves every escaped character and never cuts a Unicode character.

Every persistent Rich Message starts with one compact paragraph that keeps the bold agent name and the regular-weight lifecycle marker and localized status on the same baseline. Use `○` for queued, `●` for running, `✓` for completed, `×` for failed, `■` for stopped, and `!` for interrupted. While a run is active and no answer is available, the expanded process is primary. As soon as answer content begins, the answer precedes the collapsed process, and the terminal message retains that hierarchy after completion, failure, stop, or interruption. Completed answers use a default-open `最终回答` details block, while failed, stopped, and interrupted answers use a default-open `部分回答` details block. If an answer contains `details` or `summary` markup, or cannot fit safely inside one provider message, retain the answer through the existing safe unwrapped heading fallback instead of nesting conflicting or oversized disclosure markup. Private streaming drafts remain a single native `<tg-thinking>` element and do not repeat the persistent heading.

Rich Markdown renders one collapsible `任务过程` `<details>` section without an additional decorative marker in its summary. Telegram owns the native disclosure indicator and its direction; the renderer controls only whether the block is open by default and must not add a competing marker. Each `Thinking` event starts a numbered phase, and subsequent new `CommandExecution` and `Progress` events remain in that phase until the next thinking event. Updating an existing entry by id keeps it in its original phase. Phases and their entries are rendered oldest first so the latest activity appears at the bottom. Commands use native preformatted terminal-style `SHELL` blocks with retained escaped command text, status, and an exit code when supplied; Telegram owns the client-specific code-block colors and spacing. The process section stays open while running before answer content exists, then collapses when an answer begins or the run reaches a terminal event. The renderer retains at most 64 process phases, 64 progress entries per phase, 8192 UTF-8 bytes per process text, and 256 KiB of answer text. A snapshot that approaches provider limits removes the oldest complete retained phases first and reports omission while preserving newest state, answer, and usage. Truncated text retains the newest UTF-8-safe tail with a visible marker. Terminal usage retains total, input, cached input, output, and reasoning metrics in one compact native two-line footer without an uppercase diagnostic heading. Terminal failed, stopped, and interrupted states retain available process and partial-answer content while presenting the same centralized Chinese user-facing copy as Lark. Raw transport failures and credentials stay in local logs. General Telegram file/document input, webhook intake, persisted update offsets, and subscription filters remain outside this MVP.

All fixed node-authored user-facing copy is centralized under `agora-node/src/i18n/`, with the current catalog in `zh_cn.rs`. Channel renderers own only channel-specific Markdown, card JSON, colors, icons, and layout. Protocol field names, internal validation errors, logs, and agent-produced `Thinking`, `Progress`, and `Answer` text are not localized or rewritten.

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
- `Progress`: an identified operation with concise text and a running, completed, failed, or stopped status. Reusing an id updates an existing operation instead of duplicating it.
- `CommandExecution`: an identified, complete backend command with a running, completed, failed, or stopped status and an optional backend-reported exit code. The agent adapter must not normalize, sanitize, or truncate the command. Keeping this semantic distinction in the neutral boundary lets each channel choose a terminal-native presentation without parsing agent-authored text.
- `Answer`: user-facing answer text. Multiple chunks append in order when a backend supports answer deltas.
- `Image`: an identified output image with owned bytes, a detected media type, and a fixed safe filename. `OutputImage` accepts PNG, JPEG, and WebP signatures up to 10,000,000 bytes; it is not a full image decoder. Its debug representation never contains image bytes. Channels do not receive or open backend filesystem paths.
- `Usage`: backend-reported input, cached input, output, and reasoning-output token counts for one completed run.

These events describe presentation semantics without exposing Codex, Lark, JSONL, cards, or another adapter's protocol. An agent owns backend event classification. The daemon forwards the neutral event without interpreting its text. A channel owns presentation, truncation, coalescing, and rate limiting.

The boundary must not expose or reconstruct private chain-of-thought. `Thinking` is limited to reasoning summaries or intermediate commentary that the backend explicitly emits for user consumption.

### Output Images

Generated images are sent as separate native image replies to the source conversation; existing text cards, code blocks, links, and token layouts are unchanged. Final answer text is published before awaiting its image attachment uploads so slow attachment delivery does not hold back an available answer. Lark uploads multipart image bytes to `/open-apis/im/v1/images` with `image_type=message`, then replies to the original message with `msg_type=image` and `reply_in_thread=true`. Telegram calls multipart `sendPhoto` with the original `chat_id`, optional `message_thread_id`, and source `reply_parameters`. Both reuse the configured channel HTTP client and proxy.

Each run accepts at most eight image ids and 32,000,000 aggregate bytes. The channel claims an id before I/O and ignores repeated ids and images arriving after a terminal state. One image delivery, including authentication and upload, has a 30-second budget and holds no run-state lock during network I/O. Agent cancellation can drop the pending delivery; an already accepted external send cannot be recalled. Lark can refresh authentication once after an explicit rejection, retaining an already uploaded image key. Neither channel automatically repeats an uncertain image send, and text/card retry never resends image attachments. The run process shows sending, delivered, or unconfirmed delivery; image failure does not discard the text answer or convert successful agent execution into failure. The provider remains responsible for validating complete image contents and platform-specific dimensions. Unsupported/invalid images produce visible progress warnings, not silent success.

Codex owns image extraction. Completed successful MCP image content is decoded from base64. A final answer's Markdown image references are parsed with `pulldown-cmark`; local/file/sandbox references and supported base64 data URLs become owned images and a short attachment-status placeholder. Inline/fenced code and non-image text/links retain their source bytes. HTTP(S) image references are left unchanged and are never fetched by Node. Local paths are canonicalized and must lie in the configured workspace or the current Codex session's generated-image directory; regular-file, size, and no-follow checks reject ordinary symlink escapes and special files. This path policy is not a filesystem sandbox against a malicious process concurrently replacing ancestor directories. Each file read has a five-second limit. Content deduplication, eight-image/32,000,000-byte limits, and a 32-reference attempt budget bound extraction.

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

Channel permission enforcement happens before the daemon boundary. Private messages require an allowed sender. Group messages require both an allowed sender and an allowed group, followed by the selected group's optional mention requirement. Missing permission configuration denies all access; `*` is the explicit allow-all value. Exact group rules take precedence over the wildcard group rule. Unauthorized events never create a `ChannelTask`. A denied private message receives the denial reason, observed channel-native identifiers, and a configuration example. A denied group message receives that guidance only when it explicitly mentions the current bot; otherwise it is consumed silently to avoid adding noise to ordinary group conversation. Structured actions use the same identity policy without a mention requirement and retain their denial reply. New Lark command and interrupt callback values carry their `private` or `group` classification, so a newly generated card remains authorizable after channel reconstruction without persisted channel state. Legacy callbacks without the marker fall back to the process-local session classification; if that context is also absent, permission evaluation remains unresolved and fails closed. The shared permission layer owns structured denial content (channel name, user id, optional group id, reason, and the configuration example) and the identical Markdown body used by both platforms. The example keeps the real `channels` and `permission` hierarchy while replacing unrelated existing channel fields with a JSONC `// ...` placeholder; it does not invent channel-specific type, name, or credential fields. Each concrete channel owns its platform container and escaping: Lark wraps the shared Markdown body in its JSON 2.0 card, while Telegram sends it as Rich Markdown. Both renderers also use the one shared pure token-number formatter. Process-state accumulation, ordering, clipping, and layout remain platform-owned because their semantics differ.

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
- File changes, todo lists, and other concise execution milestones become `Progress` entries.
- The latest pending `agent_message` becomes `Answer` when `turn.completed` arrives.
- Supported MCP image content, final-answer local/data image references, and the current session's newly generated native image files become `Image` events as described above.
- A valid `turn.completed.usage` object becomes `Usage`. Input plus Output forms Total; Cached Input and Reasoning Output are subsets and must not be added again.
- An earlier `agent_message` followed by more work becomes intermediate progress instead of a final answer.

`CodexAgent` reports the thread id immediately through the neutral synchronous `AgentOutput::session_started` callback when `thread.started` is parsed, and includes it again in the normal completion outcome. It owns native deletion of that thread when requested. Backend session ids remain opaque outside the agent and must not enter the channel abstraction. The one-shot `codex exec --json` stream does not provide answer token deltas, so the final answer may appear as one update. True answer-delta streaming through Codex app-server is follow-up work.

Raw Codex stderr is backend diagnostic output rather than agent reply content. `CodexAgent` logs chunks immediately, including unterminated lines and recoverable resume errors, and does not publish them to channels. It retains only a 16 KiB diagnostic tail; once any bytes have been discarded, missing-session classification cannot authorize a replay. Structured Codex JSONL `error` and `turn.failed` messages remain user-visible agent output.

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
- Reply to the source Lark message and patch the resulting card message.
- Retain bounded Lark Thinking and Progress state, order retained phases oldest first and progress within each phase newest first, and coalesce intermediate card updates.
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

pub trait ChannelRun {
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

A channel run represents the reply/output context for one accepted task and one agent. When multiple agents subscribe to a task, the daemon opens one run per agent. The channel decides whether that run becomes a Lark card, Telegram messages, one HTTP WebSocket stream, or another channel-specific representation.

This is a direction, not a frozen API.

## Node admission and observation hardening (2026-09-05)

An AgentOutput synchronous session observation is delivered when Codex parses thread.started, before subsequent awaits. Daemon applies the existing Store compare-and-set immediately, independently of successful exit, timeout or cancellation. Channel output does not carry session metadata. Empty/changing IDs are rejected; truncated stderr cannot authorize a fresh replay.

All fan-out runs open before publishing. Initial publications run in the existing JoinSet behind a shared start gate. Only a completely published batch starts execution; failure or cancellation closes the gate and bounds cleanup of partially visible cards to five seconds. No detached cleanup worker or durable transaction is introduced.

Each channel has eight reserved control admissions and one bounded busy-reply worker in addition to ordinary task slots. The receive-order chain is released after scheduler insertion (or reset barrier insertion), not after network publication. This prevents /stop from overtaking a not-yet-registered earlier task while allowing it to proceed despite a slow queued card or reset completion. Backend dispatch never uses the control pool; shutdown rejects both paths.

Codex stderr is logged as chunks arrive, including unterminated lines; only a 16 KiB diagnostic tail is retained for classification. Lark reconnect diagnostics retain typed HTTP/business codes and transport categories, not credential URLs or peer text. Daemon logs a failed terminal publication, while Channel alone owns retry-budget exhaustion reporting.
