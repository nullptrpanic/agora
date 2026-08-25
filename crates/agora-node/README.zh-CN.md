# Agora Node

[English](README.md) | [简体中文](README.zh-CN.md) | [工作区](../../README.zh-CN.md)

一条聊天消息不应该变成看不见过程的远程 Shell。Agora Node 将它变成一次经过授权、有明确容量边界、全过程可见的本地 Agent 执行：权限与容量检查通过后才接纳任务，以聊天平台原生样式持续呈现真实执行过程，并把后端上下文延续到下一轮对话。

## 核心亮点

- **对话里看到的不只是结果，而是完整工作过程。** 飞书原生卡片与 Telegram Rich Message 会随执行持续展示生命周期、思考摘要、完整命令、进展、回答和 Token 用量。
- **上下文可以跨消息延续。** Codex Thread ID 对 Node 保持不透明，但会在本地持久保存，并按共享或按对话 Scope 恢复；用户也可以显式停止任务或重置上下文。
- **一个 Channel 就能承载一组 Agent。** 普通消息可以同时分发给多个已订阅的专业 Agent；`/ask` 可以精准指定、启用或停用某个 Agent，无需为每个 Agent 单独部署机器人。
- **流量高峰表现为明确背压，而不是资源耗尽。** 任务接纳、排队 Run 和实际执行都有上限；只有预留容量后才确认消息已被接收，满载时会返回清晰的繁忙提示。
- **并发 Agent 不会在同一个 Workspace 中互相踩踏。** 同一会话保持 FIFO，不同 Scope 可以并行，同一个规范化可写 Workspace 会在执行前自动串行化。
- **权限在入口处生效。** 用户与群组白名单默认拒绝，并且在消息生成 Agent 任务之前完成校验。
- **Channel 故障被限制在运行边界内。** 重连、投递重试、取消、终态发布和优雅关闭由 Channel 与 Daemon 负责，不会把平台细节泄漏到 Agent 实现中。

## 使用方法

### 1. 前置条件

- stable Rust 工具链。
- 已在本机安装并登录的 Codex CLI，或一个符合下文自定义 Agent 约定的可执行程序。
- 已配置长连接消息事件的飞书应用，或 Telegram Bot Token。

### 2. 构建

在工作区根目录执行：

```bash
cargo build --release -p agora-node
```

二进制文件会生成到 `target/release/agora-node`。

### 3. 生成配置

```bash
target/release/agora-node config --generate node.json
```

生成器会依次询问一个飞书或 Telegram Channel、一个允许访问的用户，以及 Codex 可执行文件。它会**覆盖同名输出文件**。只有配置了有效用户白名单后，生成的 Channel 才能接收消息。

最小飞书配置示例如下：

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

不要提交 Channel Secret 或 Bot Token。`workspace` 必须是绝对路径；`codex` 这样的纯可执行文件名会通过 `PATH` 解析。

`agent_sandbox` 配置的是 Codex CLI 自身的沙箱模式，它**不是** Agora Sandbox，也不会让 Node 自动通过 `agora-sandbox` 启动 Agent。

### 4. 启动守护进程

```bash
target/release/agora-node daemon --config node.json
```

守护进程以前台方式运行，并将日志写到标准输出。开始接收任务前，它会为当前用户的 `~/.agora` 状态目录取得单实例锁。

### 5. 在聊天中使用

普通的已授权消息会发送给订阅该 Channel 且当前启用的所有 Agent。内置命令如下：

| 命令 | 作用 |
| --- | --- |
| `/help` | 查看全部命令 |
| `/ask <agent_name> <prompt>` | 向指定 Agent 发送一次提问 |
| `/ask list` | 列出当前对话可用的 Agent |
| `/ask status <agent_name>` | 查看指定 Agent 是否启用 |
| `/ask disable <agent_name>` | 阻止后续普通消息发送给指定 Agent |
| `/ask enable <agent_name>` | 重新允许普通消息发送给指定 Agent |
| `/stop [agent_name]` | 停止当前对话中匹配的排队中或运行中任务 |
| `/reset` | 停止任务并重置当前对话的后端会话 |

可以使用 `<命令> help` 查看具体帮助，例如 `/ask help` 或 `/stop help`。

## 配置模型

一个配置文件包含可复用的 Channel 与本地 Agent，Agent 通过名称订阅 Channel：

```text
已授权的 Channel 消息
          |
          v
      有界任务接纳
          |
          +---- 已订阅 Agent A ---- 后端进程 ---- Channel 原生回复
          |
          `---- 已订阅 Agent B ---- 后端进程 ---- Channel 原生回复
```

重要配置项：

- `runtime.max_in_flight_tasks` 限制已接纳的 Channel 任务数，默认值为 `32`。
- `runtime.max_in_flight_runs` 限制已接纳的 Agent Run 数，默认值为 `64`，并且必须容纳任一 Channel 的完整 Agent 扇出。
- `runtime.max_concurrent_runs` 限制实际执行中的后端进程数，默认值为 `4`。
- `isolate: "session"` 为每个 Channel 对话建立独立后端会话；`"none"` 会让该配置 Agent 的所有对话有意共享同一个后端会话。
- 权限列表为空或缺失时默认拒绝访问。`"*"` 代表显式允许全部身份，只应在确实需要这种暴露范围时使用。
- 群消息必须同时匹配允许的发送者与允许的群；还可以通过 `require_mention` 要求必须提及机器人。
- 顶层 `proxy` 会被 Channel 和 Agent 继承，组件自己的 Proxy 配置优先级更高。

运行 `agora-node --help` 可以查看完整字段说明。

## Agent 后端

| 类型 | 状态 | 行为 |
| --- | --- | --- |
| `codex` | 可用 | 运行 `codex exec --json`、恢复已保存 Thread、将结构化事件映射为 Channel 输出，并支持图片附件 |
| `custom` | 可用 | 每个任务启动一个进程，把 Prompt 写入 stdin，并将 UTF-8 stdout 与 stderr 持续作为回答输出 |

自定义 Agent 的约定有意保持简单：Prompt 不会作为命令行参数传入，不支持附件，也不提供可持久化的后端会话。

## 状态与调度

Node 将不透明的后端会话映射，以及当前对话对 Agent 的启用/停用状态，保存到 `~/.agora/db/store.db`。在 Unix 上，`~/.agora` 目录权限为 `0700`，数据库和锁文件权限为 `0600`。

Store 不会持久化任务正文、附件、排队或执行中的任务、Channel Offset 或回复尝试。后端会话映射可以跨重启保留，但未完成任务不会恢复：Node 重启后，用户需要重新发送该任务。

调度器保持以下边界：

- 每个隔离 Scope 有一个进程内 FIFO；
- 同一个规范化 Workspace 同时只有一个写入者；
- 不同 Scope 与 Workspace 可以并发，但受全局并发上限约束；
- Channel 确认任务已接纳之前，先完成有界接纳。

## 当前范围与安全说明

- 当前可用的 Channel 是 `lark` 和 `telegram`；`local` 与 `http` 为预留值，启动时会被拒绝。
- 当 Node 暴露能力较强的本地 Agent 时，建议使用专用的低权限操作系统账号运行。
- 正式环境应使用明确白名单。允许所有人的 Channel 可能成为远程命令执行入口，其权限范围取决于后端程序和 Workspace 能访问的内容。
- 当前用户固定的 `~/.agora` Store 同时只能由一个 Node 实例使用。
- 当前持久化边界是后端会话元数据，而不是持久化 Inbox/Outbox；Agora 不承诺异常退出后的已接纳任务回放。

## 开发

在工作区根目录运行聚焦检查：

```bash
cargo test -p agora-node --all-targets --jobs 16
cargo clippy -p agora-node --all-targets --all-features --jobs 16 -- -D warnings
```
