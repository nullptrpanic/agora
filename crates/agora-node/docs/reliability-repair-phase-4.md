# Phase 4 — 配置与命令边界

按已批准的“不需要兼容旧配置/API”边界执行，保留实际已实现功能。无新依赖、不提交、不改变数据库。

1. R11/R12：参数解析保留原始尾部 prompt 的空格、换行及缩进；`/ask <name> <prompt>` 专职执行，管理命令移至 `/agent list|status|enable|disable`，同步卡片回调、菜单、帮助及测试。仅独立 `help` 作为帮助，`/ask help <prompt>` 可寻址名为 help 的 agent。拒绝有空白的 agent 名称，避免不可寻址。
2. R13/R14/R19：所有配置对象拒绝未知字段；同 provider 账号不能重复监听（TG 用归一化数字 bot ID）；容量不超过 Tokio Semaphore 硬上限；先补无外部服务的配置回归。
3. R16/A02：共用可执行文件发现规则，PATH 跳过不可执行项；启动时固定绝对路径（不解开可执行文件 symlink，避免 CLI shim 语义变化）。配置的结构校验、默认代理合并和路径准备只由 Daemon 启动边界负责一次；生成器使用同一配置模型和校验规则。
4. R17：统一代理用户名/密码 URL 百分号编码及解码；HTTP Basic/CONNECT 和子进程代理 env 表示相同凭据。测试保留字符、非 ASCII、无认证与 IPv6。
5. A05：删去从未实现的配置枚举及构造器的无效 Option 层，保留明确的反序列化错误测试。

每项回归红/绿；阶段完成执行 Node 全量测试、Clippy、规范同步，然后进入最后的结构整理和 workspace 门禁。

迁移：管理命令统一改为 `/agent ...`；未知配置键需要删除或拼写正确；重复 bot 配置需要合并为一个 channel；local/http/coco/claude_code 从类型层面拒绝。不自动删除任何历史会话数据。

## 实测进展

- 配置、命令、代理、相对执行路径、极端容量和超大管理卡片均观察旧行为红灯后修复；额外覆盖 PATH 前部不可执行文件遮蔽、各级未知字段、相同账号凭据轮换、Unicode / 保留字符代理凭据。
- `/help <command path>` 为统一无歧义帮助入口；`/ask help <prompt>`、`/agent status help` 可操作名为 help 的 agent。仅消耗 agent 名后的第一个分隔符，其余提示词原文保留。
- 生成器 DTO 已删除，复用 NodeConfig 及结构校验；手工路径的文件系统检查在 Daemon 启动时进行。可执行路径固定为绝对路径但不解析 symlink，AgentRegistry 在打开 state 前构建，run 时不重复构建。
- 已移除未实现枚举和 from_config 的 Option 层；对应拒绝测试移到反序列化边界。旧 mock 中非法 bot token 和旧命令帮助断言已按新契约更新，未放宽生产校验。
- Node 全量 370 passed，0 failed；本阶段 Node Clippy `-D warnings` 通过，0 warning。最终 workspace 门禁结果见 roadmap。

## 升级注意事项

- 执行保持 `/ask <agent_name> <prompt>`；管理改为 `/agent list|status|enable|disable`，帮助统一使用 `/help <command path>`。旧卡片上的管理按钮仍携带旧命令路径，升级后请重新发送 `/agent list` 获取新按钮；不会把旧路径误发给 Agent。
- 删除旧 `env` 字段，修正拼错的键；所有层级均严格解析。此前未实现的 HTTP/Local/Coco/ClaudeCode 类型不再出现在 schema 中。
- 一个 Lark app 或 Telegram bot 只保留一个 channel 配置；多个 Agent 仍可订阅它。Agent 名称不能包含空白，`help/list/status/enable/disable` 都可正常寻址。
- Backend 相对路径以 Node 启动目录为基准并在启动时固定为绝对路径；PATH 选择首个可执行文件，不解析 shim 的符号链接。
- 代理凭据中的合法 `%HH` 现在按 URL 语义解码；如果密码字面含 `%40`，应写 `%2540`。无认证代理如 `http://127.0.0.1:8118` 不受影响。
- 不改变 Store schema、v3 迁移或历史记录，不自动删除 session 数据；这里的“不要求兼容”不包含数据清库授权。
