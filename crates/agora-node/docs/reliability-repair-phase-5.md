# Phase 5 — 有收益的结构整理与最终验收

只收敛已经确认的重复维护点；不重写 Agent、Scheduler 或 Store。

- A01：Channel 作为独占接收驱动，不实现 Clone；ChannelSender 只含 API/身份/回调共享句柄，可 Clone，不含 receiver、offset、ACK、重连任务和接收缓存。Daemon 派发只借用/克隆 Sender。正常 Channel 的发送便利方法复用 Sender；Lark 接收端在出队前已解析会话类别，发送端不重新查询接收缓存。
- A02/A05：阶段 4 已统一配置模型、可执行路径发现，并在启动前构建 AgentRegistry；去掉未实现配置枚举、无效 Option 构造分支。
- A03：阶段 2 的投递 worker 与阶段 3 的 session 观察已落实。
- A04：复用纯 token 数字格式和两端完全相同的权限提示正文；平台布局、HTML/Markdown 转义、裁剪及过程排序保留在 Channel。事件累计若不能实质减少分支，则不为抽象而创建共享状态机。
- A06：中立身份依赖方向评估后记录取舍；不为移动类型新建服务或 crate，也不改数据库迁移/历史数据。

验收：相关测试和 Node Clippy 后，执行全 workspace 格式检查、测试、Clippy、一次 coverage（行覆盖率至少 80%），检查 target 外 profraw。独立于此任务的 sandbox 变更不修改、不回退；最终 gate 失败如实报告，不能声称准出。Linux 服务、真实 provider/客户端和长稳验收不能用本机 HTTP mock 代替。

## 实施结果

- A01 已完成：新增的 ChannelSender 是发送能力边界，不是新调度层。LarkSender / TelegramChannelSender 只保存 API、身份、共享回调注册表。生产接收端不再 Clone，offset/ACK/receiver/接收缓存不能因派发而被复制；Daemon 接收循环仅派发 sender。原有发送便利方法直接委托，未复制投递实现。
- A04 已完成纯规则复用：i18n 的 format_tokens 和 PermissionDenial 的 Markdown 正文各只有一个实现。两平台原有文案、布局与数据顺序不变。
- A04 状态机与 A06 身份搬迁不实施：过程的排序、进度种类、裁剪及布局在两个平台确实不同，共享状态机会新增分支/参数；仅移动身份值类型会扩大可见性或增加 getter，却不降低当前复杂度。保留现有模块，并非待修复的正确性问题。
- A02/A03/A05 已在前面阶段落地；没有新依赖、服务、crate、数据库 schema 或持久化恢复机制。
- 配置和管理命令迁移说明见阶段 4；CLI 内置 usage 已同步严格字段、路径、filter、容量和新命令。

## 本阶段验证

- `cargo test -p agora-node --all-targets --jobs 16 --target-dir target/node-reliability --quiet -- --test-threads=16`：371 passed，0 failed。
- `cargo test -p agora-node --doc --jobs 16 --target-dir target/node-reliability -- --test-threads=16`：1 passed，确认 ConfiguredChannel 不满足 Clone 的编译期约束。
- `cargo clippy -p agora-node --all-targets --jobs 16 --target-dir target/node-reliability -- -D warnings`：通过，0 warning。
- `cargo fmt -p agora-node -- --check` 和 `cargo fmt --all -- --check`：通过。
- 拆分后的第一次编译暴露了测试缺少 ChannelSender 导入及冗余导入；均已修正，之后完整重跑测试和 Clippy，无未处理的编译警告。
- 全 workspace 最终门禁结果统一记录在 roadmap，以上 Node 结果不能替代最终覆盖率和外部发布验收。

## 复杂度与复核

- 当前 Node diff（含新增 Rust 文件，不含文档）生产代码 +1256/-536，净增 720 行；测试 +1699/-241，净增 1458 行。主要新增是有界分片、序列化预算、准入批次收尾和 sender 所有权边界，不是新框架。测试数量从基线 331 增至 371，另有 1 项 compile-fail doctest。
- 更小的局部字符串补丁不能解决 ACK future 所有权、fan-out 部分可见状态、接收/发送混合 Clone 等问题；这些地方需要明确生命周期。其余地方复用现有锁、JoinSet、watch、CAS 和配置类型，未引入第二套执行器。
- 由当前 agent 按代码评审检查点复核计划与实现、错误分类、取消路径、接收顺序、卡片预算、配置迁移、公开 sender 边界和相邻测试；遵循仓库禁止子 agent 的要求，未声称有独立第三方评审。
- 保留的取舍：平台进度状态分开；同步 Store 访问和现有身份类型位置不做无收益搬迁；终态投递仍是有限重试，不保证超出窗口后的持久补送；不改变私聊停止能力、卡片布局或数据库兼容范围。
