# Agora Node

[English](README.md) | [简体中文](README.zh-CN.md) | [Workspace](../../README.md)

A chat message should not become a blind remote shell. Agora Node turns it into an authorized, bounded, and observable local agent run: it accepts work only after permission and capacity checks, streams the real execution process in the channel's native format, and carries backend context into the next conversation turn.

## Highlights

- **The conversation shows the work, not just the result.** Native Lark cards and Telegram Rich Messages expose lifecycle state, reasoning summaries, complete commands, progress, answers, and token usage as the run evolves.
- **Context continues across messages.** Codex thread IDs remain opaque to Node but persist locally and resume by shared or per-conversation scope; users can explicitly stop work or reset that context.
- **One channel can host a team of agents.** Subscriptions fan an ordinary message out to several specialists, while `/ask` targets, enables, or disables one agent without deploying another bot.
- **Load becomes visible backpressure, not resource exhaustion.** Task admission, queued runs, and active backends have explicit limits. Capacity is reserved before source delivery is confirmed, and overload receives a clear busy response.
- **Concurrent agents cannot race through the same workspace.** Each session stays FIFO, different scopes can run concurrently, and the same normalized writable workspace is serialized before execution.
- **Permission is enforced at the front door.** User and group allowlists are default-deny and run before an incoming message can create an Agent task.
- **Channel failures remain isolated operational events.** Reconnection, delivery retry, cancellation, terminal publication, and graceful shutdown belong to the channel and daemon boundaries instead of leaking into Agent implementations.

## Usage

### 1. Prerequisites

- A stable Rust toolchain.
- A locally installed and authenticated Codex CLI, or an executable that implements the custom-agent contract described below.
- A Lark app configured for long-connection message events, or a Telegram bot token.

### 2. Build

From the workspace root:

```bash
cargo build --release -p agora-node
```

The binary is written to `target/release/agora-node`.

### 3. Generate a configuration

```bash
target/release/agora-node config --generate node.json
```

The generator prompts for one Lark or Telegram channel, one allowed user, and a Codex executable. It **overwrites an existing output file**. Generated channels remain inaccessible until a valid user allowlist is provided.

A minimal Lark configuration looks like this:

```json
{
  "runtime": {
    "max_in_flight_tasks": 32,
    "max_in_flight_runs": 64,
    "max_concurrent_runs": 4
  },
  "channels": [
    {
      "type": "lark",
      "name": "lark",
      "app_id": "cli_replace_me",
      "secret": "replace_me",
      "permission": {
        "users": [
          { "id": "ou_allowed_user" }
        ],
        "groups": [
          { "id": "oc_allowed_group", "require_mention": true }
        ]
      }
    }
  ],
  "agents": [
    {
      "name": "codex",
      "isolate": "session",
      "workspace": "/absolute/path/to/workspace",
      "type": "codex",
      "path": "codex",
      "agent_sandbox": "workspace-write",
      "timeout_seconds": 3600,
      "max_output_bytes": 67108864,
      "subscribe": [
        { "channel": "lark" }
      ]
    }
  ]
}
```

Do not commit channel secrets or bot tokens. `workspace` must be absolute. A bare executable such as `codex` is resolved through `PATH`.

`agent_sandbox` configures the Codex CLI's own sandbox mode; it is **not** Agora Sandbox and does not make Node launch the agent through `agora-sandbox`.

### 4. Start the daemon

```bash
target/release/agora-node daemon --config node.json
```

The daemon stays in the foreground and logs to standard output. It acquires a single-instance lock for the current user's `~/.agora` state before accepting work.

### 5. Use it from chat

An ordinary authorized message is sent to every enabled agent subscribed to that channel. The built-in commands are:

| Command | Effect |
| --- | --- |
| `/help` | Show all commands |
| `/ask <agent_name> <prompt>` | Send one prompt to a specific agent |
| `/ask list` | List available agents in this conversation |
| `/ask status <agent_name>` | Show whether an agent is enabled |
| `/ask disable <agent_name>` | Stop ordinary messages from reaching that agent |
| `/ask enable <agent_name>` | Re-enable ordinary messages for that agent |
| `/stop [agent_name]` | Stop matching queued or running work in this conversation |
| `/reset` | Stop work and reset backend sessions for this conversation |

Use `<command> help`, such as `/ask help` or `/stop help`, for command-specific guidance.

## Configuration Model

One configuration contains reusable channels and local agents. An agent subscribes to channels by name:

```text
authorized channel message
          |
          v
 bounded task admission
          |
          +---- subscribed Agent A ---- backend process ---- channel-native reply
          |
          `---- subscribed Agent B ---- backend process ---- channel-native reply
```

Important settings:

- `runtime.max_in_flight_tasks` bounds accepted channel tasks; the default is `32`.
- `runtime.max_in_flight_runs` bounds admitted agent runs; the default is `64` and must fit a channel's complete subscribed-agent fan-out.
- `runtime.max_concurrent_runs` bounds executing backend processes; the default is `4`.
- `isolate: "session"` gives each channel conversation its own backend session. `"none"` intentionally shares one backend session across all conversations for that configured agent.
- Empty or omitted permission lists deny access. `"*"` is an explicit allow-all identity and should be used only when that exposure is intentional.
- Group messages require both an allowed sender and an allowed group. `require_mention` can additionally require the bot to be mentioned.
- A top-level `proxy` is inherited by channels and agents unless a component-specific proxy overrides it.

Run `agora-node --help` for the complete field reference.

## Agent Backends

| Type | Status | Behavior |
| --- | --- | --- |
| `codex` | Active | Runs `codex exec --json`, resumes stored threads, maps structured events to channel output, and supports image attachments |
| `custom` | Active | Starts one process per task, writes the prompt to stdin, and streams UTF-8 stdout and stderr as answer output |

Custom agents are intentionally simple: they receive no command-line prompt argument, attachments are unsupported, and they do not provide persistent backend sessions.

## State and Scheduling

Node stores opaque backend session mappings and conversation-scoped enable/disable state in `~/.agora/db/store.db`. On Unix, `~/.agora` directories are restricted to mode `0700`, while the database and lock files are mode `0600`.

The store does not persist task payloads, attachments, queued or running work, channel offsets, or reply attempts. Backend session mappings survive a restart, but unfinished work does not: after a Node restart, the user must send that task again.

Scheduling preserves these boundaries:

- one process-local FIFO per isolation scope;
- one writer at a time for the same normalized workspace;
- concurrency across independent scopes and workspaces, up to the configured global limit;
- bounded admission before a channel confirms that a task has been accepted.

## Current Scope and Security Notes

- Active channels are `lark` and `telegram`; `local` and `http` are reserved but rejected at startup.
- Run Node under a dedicated low-privilege operating-system account when exposing powerful local agents.
- Keep explicit production allowlists. An allow-all channel can become a remote command-execution entry point to whatever the configured backend and workspace can access.
- One Node instance may use the current user's fixed `~/.agora` store at a time.
- The current durable boundary is backend session metadata, not a persistent inbox/outbox. Agora does not promise crash-time replay of accepted tasks.

## Development

Run focused checks from the workspace root:

```bash
cargo test -p agora-node --all-targets --jobs 16
cargo clippy -p agora-node --all-targets --all-features --jobs 16 -- -D warnings
```
