> 本文保留修复前的审查基线；实时进度见 [分阶段修复记录](reliability-repair-roadmap.md)。

# Agora Node 全面审查（只读）

- 日期：2026-09-05
- 项目：`/Users/bytedance/work/code/rust/agora`
- 基线：`dev` / `6f2e5f3`
- 范围：Node 启动、配置、命令、任务准入、调度、Agent 执行与协议、会话存储、Lark / Telegram 输入与输出、权限、代理、资源限制、正常关闭、部署及测试。
- 本次不实现、不提交、不推送；不要求保留旧配置或内部 API 的兼容性。
- 审查与故障注入由当前 agent 完成；未使用子 agent。

## 结论

目前不能仅凭现有测试通过认定可以准出。主要风险集中在异步状态所有权、投递边界、失败分类和配置/命令解析。已有的 Agent / Channel 自治、Daemon 组合、Scheduler 调度、SQLite 条件更新方向合理，无须推倒重写。

下面的 P1 表示建议作为准出阻断项；P2 表示功能或稳定性缺陷，应在受影响功能发布前修复；P3 表示低频配置边界。优先级是审查建议，不表示所有问题已经在线上出现。

“已复现”指仓库外副本中的确定性测试或模拟协议响应，不等于对真实飞书、Telegram、Codex 服务进行过端到端故障验证。没有声称发现了所有可能的 bug。

## 1. 已确认缺陷与校验缺口

| ID | 优先级 / 证据 | 问题、触发条件和影响 | 位置 / 最小正确方向 |
| --- | --- | --- | --- |
| R01 | P1 / 已复现 | Telegram 等待 admission ACK 时先 `take()` 接收器；Daemon 的 `select!` 因另一任务完成而取消 `recv()`，会丢失这次待处理 ACK。已经接纳的 update 没有推进 offset，有重复执行风险。 | `channel/telegram/channel.rs:279`、`daemon/mod.rs:488`；ACK 未处理完前保留其所有权，使接收取消安全。 |
| R02 | P1 / 已复现 | Codex resume 是否缺失会话，按整个 stderr 的字符串包含关系判断。模拟执行已经成功、退出码为 0，仅 stderr 有无关的 `session not found` 警告，就会清映射并重新执行原提示词，可能重复副作用。 | `agent/codex.rs:274`、`:526`，`daemon/mod.rs:292`；只允许在确认尚未执行用户任务的 resume 失败上 fallback，不能仅靠任意文本。 |
| R03 | P1 / 已复现 + 官方 SDK | Lark WebSocket 不重组带 `sum/seq/message_id` 的分片，直接把单片当完整 JSON。半条合法事件被解析失败后 ACK 200，消息无法正常进入任务。 | `channel/lark/lark_api.rs:536`；先做有数量、总字节、超时限制的重组，再解析、确认。 |
| R04 | P1 / 已复现 + 官方 SDK | Lark 只限制文本、阶段和组件数量，没有限制最终序列化卡片大小。20 条较长命令就能得到 178,658 字节的卡片，超过官方消息卡片 30 KB 限制，更新及最终回复可能一直失败。 | `channel/lark/card.rs:465`、`:772`；发送前按最终 JSON 大小裁剪，优先保留答案、终态及最新必要过程。 |
| R05 | P1 / 已复现 + 官方服务端 | Telegram 多段终态先编辑主消息，再补后续段。前段已成功、后段失败后，重试主消息可能返回 `message is not modified`，代码直接退出，后续段永远到不了。 | `channel/telegram/rich_message.rs:282`、`telegram_api.rs:322`；识别明确的幂等无变化结果，并保留每段已发送快照和 ID。 |
| R06 | P1 / 两个 channel 均已复现 | 两个 renderer 都在获取投递锁、完成 HTTP 之前清掉 `flush_scheduled`。慢发送期间新输出会继续创建等待锁的协程。32 次更新可积累 33 个强引用持有者；数据快照合并不等于协程数量受控。 | Lark `card.rs:1140`，TG `rich_message.rs:179`；每个 Run 只保留一个更新 worker，在整个发送/检查 dirty 周期内保持 worker 所有权。 |
| R07 | P1 / 已复现 + 官方错误码 | Lark 卡片 token 刷新只认 HTTP 401。HTTP 200 + 业务码 99991663 被转换成普通字符串错误，不触发刷新。模拟新 token 已可获得时仍只获取一次，卡片无法恢复；缓存还固定为 50 分钟而不使用服务端有效期。 | `channel/lark/card.rs:1189`、`lark_api.rs:624`；保留业务错误码、按真实有效期缓存、认证失效时匹配并失效旧 token，有限重试一次。 |
| R08 | P2 / 已复现 | 新 Codex 会话已发出 `thread.started`，但 ID 到正常 `Command::run` 返回后才取出并存储。超时、取消等提前返回时 ID 丢失，下一次不能接续，`/reset` 也无法按映射删除它。已存在的旧映射不受这个路径影响。 | `agent/codex.rs:145`、`agent/mod.rs:255`、`daemon/mod.rs:292`；会话观察事件与执行成功/失败分离，仍用 CAS 维护映射。 |
| R09 | P2 / 已复现 | 多 Agent fan-out 初始化时，A 的排队卡已成功显示、B 的初始卡失败，整个 admission 返回错误，A 没有执行也没有终态，遗留排队卡。不是该路径上的执行锁泄漏。 | `daemon/mod.rs:159`；为已对用户可见的准备结果做失败收尾，再结束 admission。 |
| R10 | P2 / 静态调用链 | 文本命令也先获取普通 task slot。容量满时 `/stop`、`/reset` 收到 busy，无法通过文本控制积压。独立的结束任务按钮不经过这个准入点，不能把两者混为一谈。 | `daemon/mod.rs:491`；控制命令与普通执行任务分开准入，保留小而有界的控制容量。 |
| R11 | P2 / 已复现 | `/ask` 先 `split_whitespace()`，再用空格拼剩余参数。多行提示词、代码缩进、空行、连续空格全部改变。 | `daemon/command/registry.rs:284`、`:431`；解析命令头后，提示词直接取原始输入切片。 |
| R12 | P2 / 已复现 | 配置允许 agent 叫 `list`，但 `/ask list ...` 被管理子命令截获，无法点名它。`help/status/enable/disable` 及带空白名字也有命名语法冲突。 | `config/mod.rs:83`、`daemon/command/registry.rs:373`；分开任务命令与管理命令命名空间，并明确 agent 名称规则。 |
| R13 | P2 / 两类配置均已复现 | 只拒绝重复 channel name，不拒绝不同名字配置同一 provider 账号。相同 TG bot 会启动独立长轮询器并冲突；Lark 也会创建重复接收端。没有据此声称真实 Lark 上一定重复投递或一定丢失。 | `config/mod.rs:68`；默认拒绝重复的 `(provider, account identity)`，当前无需引入共享接收 broker。 |
| R14 | P2 / 已复现 | 未知配置项静默忽略，`max_concurent_runs: 1` 拼错后实际并发回到默认 4，操作者可能一直以为限制生效。旧 `env` 被忽略是既有设计，但这不应掩盖新字段拼写错误。 | `config/mod.rs:11`；既然不要求旧兼容，配置 schema 应严格拒绝未知字段，错误指出位置。 |
| R15 | P2 / 已复现内部边界 | TG 分段时，首段前缀几乎占满预算，再包 `<pre>` 时预算已为 0，代码仍放入第一个字符，生成 32,781 字符段，超过自身 32,768 限制。该测试针对分段函数边界，不表示普通长度答案都会触发。 | `channel/telegram/rich_message.rs:655`；放不下包装开销时先单独输出前缀，对所有分段做最终预算检查。 |
| R16 | P2 / 静态调用链 | 启动校验找 PATH 中第一个“存在”的候选，生成器却找第一个“可执行”的候选；非可执行文件/目录可遮住后面的合法程序。相对执行路径还按 Node cwd 校验、按 agent workspace 启动，基准不一致。 | `config/mod.rs:197`、`config/generate.rs:338`、`agent/command.rs:141`；统一解析一次，保存已验证的绝对执行路径。 |
| R17 | P2 / 已复现 | 代理用户名密码原样插入 agent 环境变量 URL，但 channel 采用独立 Basic Auth。含 `/` 等保留字符的密码可被配置解析接受，生成给 agent 的 URL 却解析失败。 | `config/mod.rs:446`、`:479`；采用现有 URL 库规范化认证信息和编码，保证两个消费者含义一致。 |
| R18 | P2 / 静态调用链 | Codex stderr 缓冲到正常结束才打印，超时/取消可能漏掉故障上下文；Lark 重连部分路径吞掉具体错误；Daemon 一次终态发布报错就记录 `terminal_delivery_exhausted=true`，而 TG 可能仍有后台重试。 | `agent/codex.rs:546`、`channel/lark/lark_api.rs:311`、`daemon/mod.rs:719`；有界、脱敏、及时诊断，重试状态由 channel 如实报告。 |
| R19 | P3 / 已复现校验缺口 | 超过 `Semaphore::MAX_PERMITS` 的任务容量配置可通过校验，后续构造 semaphore 会 panic，而不是给出明确配置错误。通常只出现在极端误配置，不是正常负载就会发生。 | `config/mod.rs:55`、`daemon/mod.rs:44`；验证组件硬上限，启动前友好报错。 |

## 2. 尚需明确或实测的运行边界

这些不是上述确定性复现的同级“已证实线上 bug”。

| 项目 | 当前证据与影响 | 建议 |
| --- | --- | --- |
| 重连不等于结果补送 | Lark 与 TG 接收连接能重试；但卡片终态投递重试有有限窗口，窗口结束后网络恢复不保证旧结果补到。 | 明确产品允许的投递重试窗口、终态可见性及日志；不默认引入数据库 outbox。 |
| 同 channel 准入队头阻塞 | `route_tail` 把同 channel 所有聊天的路由准备串起来，慢回复或 reset 可拖住其他聊天的 admission；执行本身不一直持有该路由门。 | 在确有跨会话延迟问题时按必要的会话/执行顺序约束排序，不把所有聊天无条件绑一起。 |
| 总内存预算 | 单任务附件上限 64 MiB，默认 32 个在途 task；每 Run 又保留阶段、条目和文本，各自有限但组合预算很大。 | 做并发峰值测量与总预算；不能把理论乘积直接称为已测 RSS 或内存泄漏。 |
| 整个 channel 的 API 流量 | 每 Run 的 400 ms 节流不等于 bot/app 级限流；多个运行并发可能一起撞 429。 | 需要时在 channel 内做请求并发/速率预算，利用服务端 Retry-After。 |
| 阻塞式准备工作 | workspace 创建、图片落盘及 SQLite 锁在 Tokio 工作线程上同步进行；准备动作部分位于执行 timeout/cancel 之外。 | 对较重文件操作使用有界 `spawn_blocking`，明确整体执行截止时间；不因为 SQLite 是同步就立即上 actor/连接池。 |
| 消息日志隐私 | receipt 日志默认记录最多约 2 KiB 用户文本，可能包含密钥或私有代码；Config Debug 还具备输出凭证的能力，但未发现当前调用直接打印整个配置。 | 默认日志记元数据；内容日志显式开启且脱敏。 |
| 卡片回调授权范围 | 回调 ID 关联任务，操作者经过权限检查，但注册表未显式绑定原 chat/user。转发卡片及上下文恢复语义尚未做真实平台验证。 | 对已允许用户跨会话操作的边界做安全验证；不能将其直接定性为已复现越权。 |
| 隔离定义 | `isolate=session` 分隔会话映射和调度 key，不提供独立 workspace、OS 用户或文件权限。workspace 锁只防并发写，不防跨会话读取。 | 明确产品信任边界，不能称其为多租户安全沙箱。 |
| 会话留存 | 配置身份改变后旧映射不再命中，旧行和 v3 隔离数据可能保留；这与活跃会话串线或数据库损坏不同。 | 需要时提供显式列出/清理旧身份功能，而非自动猜测或清空数据库。 |
| 无有效路由启动 | 空配置、没有有效订阅时可成功退出，配合 `Restart=always` 会反复启动。已有测试明确允许空配置。 | 不兼容约束下可改为明确的配置错误；属于启动语义选择。 |
| Linux 与供应链 | 本轮未在 Linux 服务环境做长稳、真实 API 限流/断网、真实客户端 UI 验证；未安装 cargo-audit/deny。 | 发布前补这些检查；未扫描不能表述成没有 CVE。 |
| 异常退出持久性 | 任务、offset、待投递快照不提供完整掉电恢复，属于当前明确范围；正常存活和正常关闭仍应正确。 | 不为普通 Node 优化默认加入 WAL 任务日志、分布式队列、事务补偿框架。 |

## 3. 不要求兼容性时，建议的架构整理

### 3.1 保留已有五个责任边界

- Agent：子进程、backend 协议、会话事实、执行结果。
- Channel：连接、接收确认、身份权限、附件下载、回复渲染和投递。
- Daemon：将中立任务路由到 Agent，组合输出与控制。
- Scheduler：任务准入、会话顺序、workspace 与全局资源额度、停止/reset 屏障。
- Store：身份化会话映射、启停状态、唯一约束与条件更新。

不需要新框架、新队列产品、微服务拆分或一个理解所有平台细节的“万能运行时”。

### 3.2 真正有收益的接口调整

| 调整 | 原因 / 收益 | 控制复杂度的边界 |
| --- | --- | --- |
| 配置从原始输入一次性编译为已验证运行配置 | 严格字段、账号去重、命名语法、资源上限、backend 支持、绝对路径与代理语义一次闭合；生成器复用同一 schema。 | 一个清晰验证流程即可，不要配置框架。校验尽量在打开数据库和启动任务之前完成。 |
| 接收端不可 Clone，发送句柄可 Clone | 当前两个 Channel 手写 Clone 都丢弃接收状态；同一类型同时表示完整 channel 和仅能发送的半初始化副本，所有权不清晰。 | 一个 receiver/driver + 一个轻量共享 sender，协议仍留在各自 channel。 |
| 会话观察与执行终态分离 | `thread.started` 是已经发生的状态事实，不应依赖正常退出才能保存；也可收紧 resume fallback 的安全前提。 | 使用少量有类型的 Agent 事件；不要把 session 信息塞进面向 UI 的文本。 |
| 每 Run 一个受控快照投递者 | 合并最新状态、发送、终态重试和退出由同一所有者负责，避免 detached flush 累积。 | 用现有 Tokio 原语足够；保留两平台不同的 token/分片/限流语义。 |
| 命令头与原始 prompt 分离，管理命令独立命名空间 | 消除 `/ask` 破坏空白、管理子命令遮蔽 agent 名以及满载时文本控制失效的问题。 | 不必建设可动态装载的命令框架；少量固定 typed commands 即可。 |
| 错误采用小而明确的分类 | 恢复/重放/用户文案不能依赖 arbitrary error string；保留 provider code、HTTP status、retry hint。 | 平台错误在 channel 内分类，backend 错误在 agent 内分类；不要向中立层泄漏平台细节。 |
| 中立身份值类型摆脱 Store 命名所有权 | Agent/Scheduler 当前引用 Store 中的 session key 类型；可以移动少量中立值类型或让 Daemon 负责 key 组合。 | 可选的小调整；不能为它新建 IdentityService 或多个 crate。 |

### 3.3 已存在的冗余与可删候选

| 位置 / 内容 | 判断 | 建议 |
| --- | --- | --- |
| Lark/TG `format_tokens` | 真实重复的纯规则。 | 提取一个小函数即可。 |
| Lark/TG 权限拒绝中的公共文字与标识列表 | 真实重复，平台容器不同。 | 共用内容数据/文案，保留各自布局与转义。 |
| Lark/TG 阶段、进度条目、状态累计和统计 | 存在重复状态机，已有相同调度缺陷显示重复维护成本。 | 可共用有限的事件累计模型；排序、裁剪、卡片结构和 Markdown 必须仍由各平台决定。 |
| 配置生成器独立 DTO 与运行配置 | schema 重复，执行文件发现逻辑已经发生行为漂移。 | 从同一受验证配置模型生成，不维护第二套规则。 |
| `ConfiguredChannel::from_config -> Result<Option<_>>` | 当前只有 Some 或 Err，没有真正的 None 分支。 | 改为 `Result<_>`，删除调用端无意义分支。 |
| 尚不支持的 Coco/ClaudeCode/HTTP/Local 等配置形状 | 暴露了不能运行的配置空间，增加分支与文档负担。 | 当前没需求就移除；未来实现时再加入。 |
| Telegram draft/heartbeat 与主业务路径 | Daemon 总传入 interrupt，因此私聊主流程选择持久消息；draft 路径仍保留大量代码和测试，文档却承诺主流程使用 draft。 | 先确定产品要哪一种；不要求旧兼容时可收敛，但不能在审查中直接删功能。 |
| v3 SQLite 迁移与隔离表 | 若确实不再支持旧数据库升级，可减少迁移代码和测试。 | 这是产品兼容范围选择；仍须明确现有数据处理，不能静默清数据库。 |

不建议因为“代码量多”而删除：session CAS/唯一约束、reset 屏障、workspace 锁、全局额度、进程组清理、附件/输出上限、UTF-8 分块处理、平台协议所需的差异代码及有效测试。

## 4. 本次没有发现应推翻的既有设计

1. `isolate=none` 下跨 channel 共享同一 agent 会话并串行执行，是已明确的产品行为，不是串会话 bug。
2. `isolate=session` 的身份化 key、SQLite 唯一约束及条件 observe/remove 具有实际价值；没有证据说明当前常规 reset 路径会任意误删别人的活跃映射。
3. reset 先停止目标范围、通过屏障协调、backend 删除失败保留映射，这一方向正确。
4. workspace 规范化与串行资源锁避免相同目录并发写；全局额度和单会话 FIFO 解决不同问题，不应合并成一个大锁。
5. 已有单实例 guard、HTTP/代理握手超时、输出及附件上限、进程组清理、正常关闭路径，是稳定性资产。
6. Lark/TG 使用不同的渲染器是合理差异；复用状态和纯规则不等于统一平台布局。

## 5. 复现和验证记录

### 5.1 原项目基线

重新编译原项目后：

```text
cargo test -p agora-node --all-targets --jobs 16 -- --test-threads=16
331 passed, 0 failed; exit 0

cargo clippy -p agora-node --all-targets --jobs 16 -- -D warnings
exit 0; no warnings

git diff --check
exit 0

git status --short
empty
```

没有运行或宣称通过 workspace 覆盖率、Linux 交叉编译、真实服务端故障注入、真实客户端 UI 或依赖漏洞扫描。实际仓库 Rust 源码和测试均未修改。

诊断副本与原项目曾共用 `target`，一次基线调用复用了包含临时失败测试的产物。已执行 `cargo clean -p agora-node` 清除该 crate 的可再生构建缓存，并重新编译原项目，以上 331 项是重编译后的有效基线结果。清理不涉及用户源码或运行时数据。

### 5.2 仓库外测试副本

位置：`/tmp/agora-node-review.rW4NLV`。只有复制的 Node crate 增加测试，其他 crate 仅为依赖引用。复现测试保留供后续转成回归测试。

18 项期望行为断言全部编译成功并失败；其中两个 provider 的重复账号测试和两个 renderer 的协程上限测试各覆盖同一类问题，因此不把 18 个 case 直接叫做 18 个独立 bug。

前轮 6 项：

```text
review_cancelled_receive_retains_pending_acknowledgement
review_multipart_retry_survives_unchanged_primary
review_fragmented_event_is_not_acknowledged_before_reassembly
review_lark_card_respects_serialized_size_budget
review_fresh_session_id_survives_timeout
review_partial_fanout_clears_previously_published_queue
```

本轮新增 12 项：

```text
audit2_lark_slow_flush_has_only_one_pending_worker
audit2_telegram_slow_flush_has_only_one_pending_worker
audit2_lark_business_token_error_refreshes_cached_token
audit2_duplicate_telegram_accounts_are_rejected
audit2_duplicate_lark_accounts_are_rejected
audit2_unknown_runtime_option_is_not_silently_ignored
audit2_limits_cannot_exceed_semaphore_capacity
audit2_telegram_split_budget_includes_prefix_and_wrapper
audit2_successful_resume_is_not_replayed_for_unrelated_stderr
audit2_ask_preserves_prompt_whitespace
audit2_valid_agent_named_list_is_addressable_by_ask
audit2_proxy_credentials_keep_same_destination_for_agent
```

Lark token 测试初版 HTTP mock 的路径匹配过宽，曾造成假通过；改成区分 POST 与 PATCH 后，最终断言稳定失败。这是测试夹具修正，没有修改生产逻辑。

为避免以后重复使用同名 package 的旧构建产物，重新运行临时副本时建议单独使用 `target/node-review-probes`，不要与基线同时运行 Cargo。

### 5.3 发布验收应新增的检查

- recv future 被取消后，ACK / offset 状态仍正确，已接受任务不会因此重新执行。
- 成功执行、已开始执行与真正的 resume missing 三种结果分类，特别是禁止误重放。
- Lark 分片乱序/重复/超时/超限，认证业务码失效和 token 有效期。
- 多段消息部分成功再重试、无变化编辑及最终字节/字符预算。
- 慢 HTTP + 高频输出下，worker 数量、保留内存、终态到达性有上界。
- 多 Agent 部分初始化失败，已显示的卡片能收尾。
- fresh session 在 timeout/stop/错误后可寻址，reset 仍满足条件删除约束。
- 满载时控制路径、跨会话准入延迟、代理密码编码、命令原文及配置错误。
- Linux user systemd 环境的 PATH/nvm、SIGTERM、网络恢复及 24 小时以上真实使用观测。

覆盖率用于发现空白，不代替以上行为断言。

## 6. 规范一致性

- `spec/architecture/agent-channel.md:133` 及 `spec/architecture/node-config.md:183` 描述私聊主流程使用 Draft；但 `daemon/mod.rs:172` 总提供 interrupt，`telegram/rich_message.rs:249` 选择持久消息。这是明确的代码/规范不一致，必须先确定期望行为。
- 规范承诺慢 Telegram 请求不会积压无界的过时更新任务，现有 scheduled flag 生命周期不满足这个所有权约束。
- 飞书“结束任务”按钮在顶部不是服务端随机挪动：`channel/lark/card.rs:505` 将 action row 放在正文元素之前，且测试明确要求顶部。改变位置属于产品布局选择，不应包装成未知平台故障。
- 本次只读审查，不修改 spec。未发现项目提供 Justfile / `just spec-check` 入口，因此没有声称运行过它。

Spec consistency: mismatch found, clarification needed

## 7. 协议依据（官方来源）

- Lark WebSocket SDK 在解析前重组分片：[官方 Python SDK](https://github.com/larksuite/oapi-sdk-python/blob/v2_main/lark_oapi/ws/client.py)。对应 R03。
- Lark 消息卡片 30 KB 限制：[官方 Go SDK 请求模型说明](https://github.com/larksuite/oapi-sdk-go/blob/v3_main/service/im/v1/model.go#L14444)。对应 R04。
- Telegram 无变化编辑返回错误及单 bot 长轮询冲突：[官方 Bot API 服务端](https://github.com/tdlib/telegram-bot-api/blob/master/telegram-bot-api/Client.cpp)。对应 R05 / R13。
- 飞书无效 access token 业务码 99991663：[官方 CLI 错误码定义](https://github.com/larksuite/cli/blob/main/internal/output/lark_errors.go)。对应 R07；HTTP 200 场景由本地 mock 验证本代码的行为。

## 8. 推荐实施顺序（尚未实施）

1. 把现有复现转成正式回归测试，先修 ACK、误重放、协议分片、大小与多段投递、token 刷新。
2. 整理每 Run 投递所有权与会话观察边界，解决 worker 累积、会话 ID 丢失及失败收尾。
3. 一次收紧配置/命令边界，解决字段静默忽略、重复账号、命名冲突、原始 prompt 和执行路径问题。
4. 删除有证据的冗余分支、复用纯规则及公共状态，保留平台差异与并发不变量。
5. 更新规范，再做 Linux 服务环境、真实 provider 限流/断网/长输出与长稳验收。

允许不兼容能减少适配层，但不需要一次重写全部 Node；也不等于允许静默删除现有 session 数据。
