# Node 分阶段修复记录

## 范围与约束

- 用户于 2026-09-05 确认：记录整体审查结论，并分阶段修复；不要求旧配置和内部 API 兼容。
- 基线：`dev` / `6f2e5f3`；完整发现和复现证据见 [审查基线](node-audit-2026-09-05.md)。
- 保留 Agent / Channel 自治、Daemon 组合、Scheduler 资源与 FIFO、Store 条件更新的责任边界。
- 不引入框架、外部任务队列、新依赖或新的 crate；不做掉电恢复、自动清库或无关 UI 改版。
- 全部工作由当前 agent 完成。改动保持未提交，不创建 commit，不 push。
- 每阶段作为独立 Node 验证批次：记录根因 → 回归测试红灯 → 最小正确修复 → 窄测试 → Node 测试与 Clippy → 更新本表并继续。用户随后明确要求把剩余阶段全部推进；同期非 Node 改动不阻止独立修复，但必须记录其验证影响，全部阶段结束后统一执行 workspace 门禁和一次覆盖率。
- Rust 测试/build jobs 为 16；同一 target 不并行运行多个 Cargo 进程；只编译当前架构。
- `spec/` 在本仓库被忽略，因此将可追踪的审查记录和计划放在本 crate 的 `docs/`；受影响规范仍须同步维护，不擅自修改仓库 ignore 策略。

## 阶段划分

当前结论：R01–R19 的实施和本机代码门禁均已完成，阶段 1–5 已全部处理。A04/A06 的有条件整理保留项已写明理由；外部发布验收仍独立列出，不把“代码验证通过”当作真实 Linux/平台长稳验收。

| 阶段 | 目标 | 问题编号 | 状态 / 验收 |
| --- | --- | --- | --- |
| 1 | 接收确认、误重放与 Telegram 多段投递正确性 | R01、R02、R05、R15 | 已实施，341 项 Node 测试和 Clippy 通过；早期 workspace 失败保留在历史记录，最终结果见文末；[详情](reliability-repair-phase-1.md) |
| 2 | Lark 协议与认证、两端投递协程生命周期 | R03、R04、R06、R07 | 已实施，357 项 Node 测试和 Clippy 通过；有界分片、最终序列化预算、业务认证刷新和单 worker 均有回归；[详情](reliability-repair-phase-2.md) |
| 3 | session 生命周期、任务收尾及控制路径 | R08、R09、R10、R18 | 已实施，362 项 Node 测试和 Clippy 通过；会话及时观察、部分初始化收尾、满载控制与诊断回归通过；[详情](reliability-repair-phase-3.md) |
| 4 | 配置和命令边界 | R11、R12、R13、R14、R16、R17、R19 | 已实施，370 项 Node 测试和 Clippy 通过；同一配置模型、严格输入、原文 prompt、命名空间、路径与代理边界完成；[详情及迁移](reliability-repair-phase-4.md) |
| 5 | 经验证有收益的结构整理 | 见下方 A01–A06 | 已实施有收益项并记录保留项；371 项 Node 测试、1 项编译期 doctest、Node Clippy 通过；[详情](reliability-repair-phase-5.md) |
| 发布验收 | 真实服务环境与外部边界 | 见下方 V01–V05 | 未执行；不得仅靠本地测试宣布准出 |

## 缺陷追踪

| 编号 | 问题 | 状态 |
| --- | --- | --- |
| R01 | Telegram 接收取消丢失 pending ACK | 已修复，Node 回归通过（阶段 1） |
| R02 | stderr 子串误判导致 Codex 任务重跑 | 已修复，Node 回归通过（阶段 1） |
| R03 | Lark WebSocket 分片未重组 | 已修复，Node 回归通过（阶段 2） |
| R04 | Lark 最终卡片缺少序列化大小预算 | 已修复，Node 回归通过（阶段 2） |
| R05 | Telegram 无变化编辑阻断后续分段补发 | 已修复，Node 回归通过（阶段 1） |
| R06 | 两端慢投递导致 flush worker 积累 | 已修复，Node 回归通过（阶段 2） |
| R07 | Lark 业务认证错误不刷新 token，TTL 固定 | 已修复，Node 回归通过（阶段 2） |
| R08 | 新 session ID 在提前退出时丢失 | 已修复，Node 回归通过（阶段 3） |
| R09 | fan-out 部分初始化失败留排队卡 | 已修复，Node 回归通过（阶段 3） |
| R10 | 满载时文本控制命令被普通准入挡住 | 已修复，Node 回归通过（阶段 3） |
| R11 | `/ask` 改写空白、缩进和换行 | 已修复，Node 回归通过（阶段 4） |
| R12 | agent 名称和管理子命令冲突 | 已修复，Node 回归通过（阶段 4） |
| R13 | 重复 provider 账号可以启动多个接收端 | 已修复，Node 回归通过（阶段 4） |
| R14 | 未知字段静默忽略 | 已修复，Node 回归通过（阶段 4） |
| R15 | Telegram 分段前缀耗尽包装预算 | 已修复，Node 回归通过（阶段 1） |
| R16 | 执行文件发现/校验/启动基准不一致 | 已修复，Node 回归通过（阶段 4） |
| R17 | 代理认证 URL 编码不一致 | 已修复，Node 回归通过（阶段 4） |
| R18 | stderr / 重连诊断缺失，终态重试日志不准确 | 已修复，Node 回归通过（阶段 3） |
| R19 | 超过 semaphore 硬上限的配置未拒绝 | 已修复，Node 回归通过（阶段 4） |

## 架构整理清单

- A01：已完成。接收端不再 Clone；轻量 ChannelSender 可克隆，Daemon 派发不再携带接收状态。
- A02：已完成。Daemon 启动验证后构建并复用 AgentRegistry；生成器和运行端复用同一 schema、校验和可执行路径发现。
- A03：已完成。会话观察独立于执行终态；普通投递 worker 在等待、发送、检查 dirty 的整个周期保持所有权。
- A04：已完成纯规则复用。token 数字格式、权限拒绝正文各只有一处实现；事件累计的进度分类、排序与裁剪有平台差异，强行共享会增加参数和分支，因此未引入共享渲染状态机。
- A05：已完成。删除无意义的 Option 构造分支、生成器重复 DTO、未实现的 HTTP/Local/Coco/ClaudeCode 配置变体。
- A06：已评估，保留现状。仅把 Store 中的身份值类型搬到新文件，需要扩大字段可见性或增加 getter，不能减少当前行为分支，也没有纠正运行时缺陷；未建立身份服务或新 crate。它是有条件的可选整理，不是遗留 R 类缺陷。

## 需要单独确认的产品选择

下列事项不作为已授权的静默删除或改版：

- Telegram 私聊保持当前带停止按钮的持久消息；callback-free 私聊的 Draft 能力仍保留。已将旧规范改为实际行为，未改变产品选择或删除功能。
- v3 数据库升级和现有旧身份记录维持原行为；本次不改变数据库 schema、迁移代码或自动清理任何会话数据。
- 飞书结束任务按钮的位置；当前顶部是明确代码/测试行为，不在本次稳定性修复中移动。
- 是否需要持久化补送、总内存/请求限流、回调作用域强化、默认日志隐私策略；先明确收益和验收边界，不混入“代码整洁”改动。

## 外部验收

- V01：Linux 非 root user systemd 启动、PATH/nvm、SIGTERM 正常关闭。
- V02：真实 Lark/TG 断网重连、限流、长输出和终态投递窗口。
- V03：真实客户端卡片/多段消息和转发回调权限边界。
- V04：并发内存、磁盘慢操作及 24 小时以上长稳观测。
- V05：依赖漏洞扫描。未运行时必须报告未运行，不能表述为没有漏洞。

## 验证记录

### 阶段 1 开始前

- 原项目 `cargo test -p agora-node --all-targets --jobs 16 --quiet -- --test-threads=16`：331 passed，0 failed。
- 工作区起始干净；审查复现只在仓库外副本中，不直接把带失败用例的副本整体覆盖进来。
- 本阶段完成前，必须记录正式回归红/绿结果、Node Clippy、workspace 测试与覆盖率以及所有未通过的检查。

### 阶段 1 实施结果（2026-09-05，验收受阻）

- R01：pending ACK 在等待期间仍由接收端持有；连续三次取消 `recv` 后，accept 正确推进 offset，drop receipt 仍可重投。
- R02：只有 resume、非零退出、完全未观察到 stdout、明确的预执行 rollout 缺失诊断同时成立，才允许一次 fresh fallback。成功执行、已开始输出和模糊 stderr 均不重放，保留原会话映射。删除会话路径不变。
- R05：在 Telegram API 内识别 `editMessageText` 的明确 400/no-op 响应；渲染器继续补发缺失的后段。其他错误及发送接口的同文案仍失败。没有新增每段快照缓存：已有 message ID 加明确幂等分类已足够解决本缺陷，避免重复状态。
- R15：前缀无法容纳包装和第一个转义字符/换行时，先单独发送前缀；每段同时遵守字符和结构预算，Unicode 与转义内容完整保留。
- Node 改动为 4 个生产文件、4 个测试文件；生产代码 +58/-22（净增 36 行），测试净增 311 行。新增 10 项测试（其中包含表驱动边界场景），另补强 1 项已有测试。
- 无新依赖、无数据库 schema 或配置变更、无 UI 改版；不提交、不 push。受影响的三个本地 spec 已同步。

| 验证 | 实测结果 |
| --- | --- |
| 四项缺陷的回归红灯 | 均在修复前观察到目标断言失败：ACK offset、重复调用计数、多段补发、分段预算 |
| 聚焦测试 | Telegram channel 42 passed；daemon sessions 9 passed；R05 后 Telegram 80 passed；R15 后 rich_message 38 passed |
| Node 全量 | `cargo test -p agora-node --all-targets --jobs 16 --quiet -- --test-threads=16`：341 passed，0 failed；补强连续取消测试后再跑仍通过 |
| Node Clippy | `cargo clippy -p agora-node --all-targets --jobs 16 -- -D warnings`：通过，0 warning |
| workspace 普通测试 | `cargo test --workspace --all-targets --jobs 16 --quiet -- --test-threads=16`：当时通过；随后 sandbox 文件继续变化，因此不能作为最终工作区的验收结论 |
| workspace Clippy | `cargo clippy --workspace --all-targets --jobs 16 -- -D warnings`：当时通过，0 warning；后续 sandbox 改动尚未纳入最终复验 |
| workspace 覆盖率 | 按规定命令执行一次，退出码 101；sandbox `decryption_rejects_malformed_headers_and_incomplete_blocks` 失败（实际文件长度 4298，期望 4284），未完成覆盖率报告，不能声称达到 80% |
| 格式 | 早期 workspace fmt check 通过；最终复查被同期 sandbox 文件的格式差异阻断；本阶段 8 个 Node Rust 文件独立 `rustfmt --edition 2024 --check` 通过 |
| diff / 覆盖率产物 | `git diff --check` 通过；`rg --files -uu -g '*.profraw' -g '!target/**'` 无输出（无匹配返回 1） |
| 规范检查 | 本仓库无 Justfile / spec-check 命令，未运行 `just spec-check`；人工核对本阶段相关条款 |

实施中的临时失败也已处理：R05 HTTP mock 夹具曾因传入 `String` 而非 `&str` 编译失败，修正夹具后重新观察目标红灯，再完成绿灯；R15 红灯断言改为等价的短消息以避免输出超长字符串。这些不是被跳过的测试。

整体验证阻断不来自本阶段 Node diff：工作区在验证期间新增/修改了 `crates/agora-sandbox/src/filesystem/crypto/content.rs`、对应测试及 macOS 文件 hook/runner。加密文件打开逻辑不再裁掉尾部数据，而已有测试仍断言打开后长度收缩，导致上述确定性断言不一致。本任务未修改、格式化或回退这些文件，也不判断另一项修复的目标行为。

**后续执行调整**：用户明确要求继续完成其余阶段。因此保留上述失败记录，继续阶段 2–5 的独立 Node 修复；全部实施后重新执行最终 workspace 格式、测试、Clippy 和覆盖率门禁。任何尚未通过的最终门禁仍阻止“验收完成/准出”结论。

### 最终实施与本机验收汇总（2026-09-07 收尾）

R01–R19 均已修复并有回归覆盖；A01/A02/A03/A05 与 A04 纯规则复用已实施。A04 平台事件状态机和 A06 身份值类型搬迁经评估保留，理由见阶段 5；没有把正确性问题降级为“低收益优化”。

以下结果来自全部阶段后的实际验证，取代上方早期 workspace 验收受阻的当前状态；历史失败仍保留。全部运行当前本机架构，未交叉编译。为避免与同期 sandbox 工作竞争产物，普通测试和 Clippy 使用 `target/node-reliability`；覆盖率使用工具默认的 `target/llvm-cov-target`。本任务没有修改、格式化或回退 sandbox 文件。

| 验证命令 | 最终实测结果 |
| --- | --- |
| `cargo test -p agora-node --all-targets --jobs 16 --target-dir target/node-reliability --quiet -- --test-threads=16` | 371 passed，0 failed，退出 0 |
| `cargo test -p agora-node --doc --jobs 16 --target-dir target/node-reliability -- --test-threads=16` | 1 passed；接收端不可 Clone 的 compile-fail 测试通过，退出 0 |
| `cargo clippy -p agora-node --all-targets --jobs 16 --target-dir target/node-reliability -- -D warnings` | 退出 0，0 warning |
| `cargo test --workspace --all-targets --jobs 16 --target-dir target/node-reliability --quiet -- --test-threads=16` | 全部通过，0 failed，退出 0；包含同期 sandbox 当前工作区 |
| `cargo clippy --workspace --all-targets --jobs 16 --target-dir target/node-reliability -- -D warnings` | 退出 0，0 warning |
| `LLVM_PROFILE_FILE="$PWD/target/agora-%p-%12m.profraw" RUST_TEST_THREADS=16 cargo llvm-cov --no-clean --workspace --all-targets --jobs 16 --fail-under-lines 80` | 本次最终覆盖率运行退出 0，所有测试通过；workspace 行覆盖率 90.38%（46,366 行中覆盖 41,906 行），超过 80% 门槛 |
| Node 覆盖率分项汇总 | 同一默认报告中 34 个 Node 文件共 9,574 行，未覆盖 524 行，加权行覆盖率 94.53%；未改变任何排除规则或生产行为来凑覆盖率 |
| `cargo fmt -p agora-node -- --check` / `cargo fmt --all -- --check` | 通过，最终复查退出 0 |
| `git diff --check` | 通过，退出 0 |
| `rg --files -uu -g '*.profraw' -g '!target/**'` | 最终无输出（无匹配退出 1）；根目录扫描发现的 6 个 LLVM profile 已移入 `target/node-coverage-residuals.8Wx7Ds/`，仍可恢复，没有删除源码或运行数据 |
| 规范检查 | 人工核对并更新 agent-channel、node-config、local-store；仓库没有 Justfile / spec-check 入口，未运行不存在的 `just spec-check` |
| 漏洞扫描 | `cargo --list` 中没有 audit/deny；未安装新工具，也未执行依赖漏洞扫描，不声称没有漏洞 |

最终风险边界仍为：Linux user systemd/PATH/nvm/SIGTERM，真实 Lark/TG 断网、限流与终态窗口，真实客户端显示/转发回调，以及并发资源和 24 小时长稳。当前没有这些外部验收的结果，因此结论是“修复及本机代码验收完成”，不是“生产环境已准出”。持久化补送、全局流量/内存预算和日志隐私策略是单独的产品/运行策略，不在本次引入。

配置和命令的升级影响见 [阶段 4](reliability-repair-phase-4.md#升级注意事项)。所有改动保持未提交、未 push；没有修改依赖、Cargo feature、Store schema、现有迁移或 service。

Spec consistency: updated spec/architecture/agent-channel.md, spec/architecture/node-config.md, spec/architecture/local-store.md
