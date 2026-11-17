# Node Reliability Phase 1 Implementation Plan

> **For agentic workers:** Use `superpowers:executing-plans` inline. Repository instructions prohibit subagents. Do not commit or push. This is one independently verified phase of the [repair roadmap](reliability-repair-roadmap.md); report this phase's result before proceeding to the next phase.

**Goal:** 防止 Telegram ACK 取消与 Codex resume 误判引发重复执行，并保证 Telegram 多段终态可正确补发、分段不越过自身预算。

**Architecture:** 保留 Agent、Channel、Daemon、Scheduler、Store 边界。本阶段只在状态所属模块修复根因：pending receipt 由接收端持有到结算；resume fallback 由 Codex 严格分类；Telegram API 识别明确的无变化编辑；renderer 保证每段包含包装后的预算。

**Tech Stack:** Rust 2024、Tokio、现有 reqwest/serde_json、现有 HTTP test server 和临时 shell backend。无新依赖。

**Spec:** `spec/architecture/agent-channel.md`、`spec/architecture/node-config.md`、`spec/architecture/local-store.md`，以及 [已确认审查结论](node-audit-2026-09-05.md)。

## Global Constraints

- 当前架构编译；Cargo jobs/test threads 为 16；同一 target 的 Cargo 命令串行执行。
- 不重写 Daemon 或引入共享平台协议层；不改数据库 schema，不清理存量 session。
- 不改 UI 布局、Draft/停止按钮选择、配置 schema 或公共命令语法。
- 改动保持未提交；测试先失败再修复；workspace 行覆盖率至少 80%。
- 规范只更新本阶段明确改变的行为，其他已发现不一致保持在 roadmap 中可见。

## Task 1: R01 — 接收取消安全

**Files:**
- Modify: `crates/agora-node/src/channel/telegram/channel.rs`
- Test: `crates/agora-node/src/channel/telegram/channel/tests/api.rs`
- Spec: `spec/architecture/agent-channel.md`

**Interfaces:** `Channel::recv` 与 `DeliveryReceipt::accept` 不变；内部 `settle_pending_delivery` 在 await 完成前不移除 pending receiver。

- [x] 写回归：获取 update 701 的 receipt，poll 并 drop 下一次 recv，再 accept receipt；新 poll 的 offset 必须为 702。另测 drop receipt 后同一 update 可重投，连续取消不能跳过 pending 更新。

```rust
let (_, receipt) = channel.recv().await?.unwrap().into_parts();
let mut pending = Box::pin(channel.recv());
assert!(matches!(futures_util::poll!(&mut pending), std::task::Poll::Pending));
drop(pending);
receipt.accept();
let next = channel.recv().await?.unwrap();
assert_eq!(next.task_id(), "702");
// HTTP mock 捕获到的下一次 getUpdates 请求必须携带 offset = 702。
```

- [x] `cargo test -p agora-node --lib --jobs 16 cancelled_receive -- --test-threads=16`，确认因 offset 丢失而失败。
- [x] 改成借用 `pending_acknowledgement.as_mut()` 等待；得到 disposition 和复制 update id 后再置空，并同步结算 offset / pending updates，不在这两步之间 await。

```rust
let Some((update_id, acknowledged)) = self.pending_acknowledgement.as_mut() else {
    return;
};
let disposition = acknowledged.await.unwrap_or(DeliveryDisposition::Retry);
let update_id = *update_id;
self.pending_acknowledgement = None;
```

- [x] 重跑新回归与已有 Telegram channel 测试，确认 acceptance/retry 两种语义都保留。

## Task 2: R02 — 只在明确未开始执行的 resume 失败上 fallback

**Files:**
- Modify: `crates/agora-node/src/agent/codex.rs`
- Test: `crates/agora-node/src/daemon/tests/sessions.rs`
- Test: `crates/agora-node/tests/agent/codex.rs`
- Spec: `spec/architecture/agent-channel.md`、`spec/architecture/node-config.md`、`spec/architecture/local-store.md`

**Interfaces:** `AgentSessionUpdate` 不变；`CodexCommandOutput` 内部记录是否已经观察到 stdout，结合真实 exit code 判断 NotFound。任何 stdout 都表示无法再证明重放安全，而不只统计已知执行事件。删除会话的幂等语义不在本阶段改动。

- [x] 写回归：已映射的任务成功完成，即使 stderr 有 missing-session 文字也只执行一次且映射不变；非零退出但已观察到 thread/turn/item 时同样不得重跑；无执行事件但只有模糊 warning 不得重跑。

```rust
assert_eq!(std::fs::read_to_string(temp.path().join("invocations"))?.lines().count(), 1);
assert_eq!(store.get(&key)?.as_deref(), Some("existing-thread"));
```

- [x] `cargo test -p agora-node --lib --jobs 16 resume_is_not_replayed -- --test-threads=16`，确认当前代码重复执行或错误替换映射。
- [x] 将 fallback 条件收紧为 resume 请求、非零正常退出、尚未观察到 stdout、明确的 backend resume missing 诊断四项同时成立。成功/已开始/模糊错误保持原映射并正常返回执行结果，不猜测重放安全性。

```rust
self.resume_requested
    && exit_code != 0
    && !self.stdout_observed
    && missing_resume_session_message(&String::from_utf8_lossy(&self.stderr_buffer))
```

- [x] 保留已有真正 `no rollout found for thread id` 的一次 fresh fallback 测试；补成功退出但无 thread 事件仍不得误判的边界。
- [x] 重跑 daemon sessions 与 Agent Codex 集成测试。

## Task 3: R05 — 无变化编辑不能阻断多段补发

**Files:**
- Modify: `crates/agora-node/src/channel/telegram/telegram_api.rs`
- Test: `crates/agora-node/src/channel/telegram/channel/tests/api.rs`
- Test: `crates/agora-node/src/channel/telegram/rich_message/tests/api.rs`
- Spec: `spec/architecture/agent-channel.md`

**Interfaces:** `edit_rich_message` 仍返回 `Result<()>`；API 的私有错误类型保留“明确无变化编辑”分类，不在 renderer 或 Daemon 匹配错误字符串。

- [x] 写回归：首段成功、第二段失败，下一次终态投递遇到主消息明确 no-op 后仍发送缺失段。断言总共三次 send（一次主段、一次失败后段、一次成功后段），最终 terminal 已送达。

```rust
assert!(message.publish(RunEvent::Completed { exit_code: 0 }).await.is_err());
message.publish(RunEvent::Completed { exit_code: 0 }).await?;
assert_eq!(server.endpoint_count("sendRichMessage").await, 3);
```

- [x] `cargo test -p agora-node --lib --jobs 16 multipart_retry -- --test-threads=16`，确认补发被 no-op 错误阻断。
- [x] 只把 editMessageText 的业务 code 400 且明确 `Bad Request: message is not modified` 响应归为 no-op 成功。权限错误、其他 400、超时、发送接口返回同样文字仍必须失败。

```rust
match result {
    Ok(_) => Ok(()),
    Err(error) if error.message_not_modified => Ok(()),
    Err(error) => Err(error.into()),
}
```

- [x] API 边界测试验证 no-op 与其他错误分类；重跑整个 rich_message 测试，保留已有消息 ID、有限重试与不重发非幂等首段的行为。

## Task 4: R15 — 包装开销计入每个分段

**Files:**
- Modify: `crates/agora-node/src/channel/telegram/rich_message.rs`
- Test: `crates/agora-node/src/channel/telegram/rich_message/tests/content.rs`
- Spec: `spec/architecture/agent-channel.md`

**Interfaces:** `split_sections` / `safe_section_chunks` 仍返回完整字符串分段；保留现有安全转义，不截断有效 Unicode 字符。

- [x] 写边界表：字符前缀、结构点前缀接近上限；第一字符为普通字符、换行、`&`、中文；超长 section 分段后所有段满足预算且内容不丢。

```rust
let parts = TelegramRichContent::split_sections(vec!["x".repeat(32_767), "y".repeat(32_769)]);
assert!(parts.iter().all(|part| part.chars().count() <= 32_768));
assert_eq!(parts[0], "x".repeat(32_767));
```

- [x] `cargo test -p agora-node --lib --jobs 16 split_budget -- --test-threads=16`，确认包装使首段超限。
- [x] 当剩余首段放不下包装及第一个转义字符时先单独输出前缀，然后以完整预算构造新块；字符和结构预算同时处理，不降低限制，不丢字符。
- [x] 重跑全部 renderer 内容测试，包括中文、HTML 转义和现有大答案分段。

## Phase Gate

当前状态：**四项修复及 Node 回归已落地，workspace 整体验收受同期 sandbox 改动阻断，阶段 1 尚未验收完成。** 命令结果、失败原因及未完成事项见 [验证记录](reliability-repair-roadmap.md#阶段-1-实施结果2026-09-05验收受阻)。

- [ ] 最终 `cargo fmt --all -- --check`。此前通过；最终检查发现同期 sandbox 文件的格式差异，未代改其他任务文件。
- [x] 按改动的 8 个 Node Rust 文件运行 rustfmt；仅对这 8 个文件做独立格式复查。
- [x] `cargo test -p agora-node --all-targets --jobs 16 --quiet -- --test-threads=16`：341 passed。
- [x] `cargo clippy -p agora-node --all-targets --jobs 16 -- -D warnings`：通过。
- [ ] 最终 `cargo test --workspace --all-targets --jobs 16 --quiet -- --test-threads=16`：此前通过，但其后 sandbox 实现变化，待工作区稳定后复验。
- [ ] 最终 `cargo clippy --workspace --all-targets --jobs 16 -- -D warnings`：此前通过，但其后 sandbox 实现变化，待复验。
- [ ] workspace coverage：`LLVM_PROFILE_FILE="$PWD/target/agora-%p-%12m.profraw" cargo llvm-cov --no-clean --workspace --all-targets --jobs 16 --fail-under-lines 80`。已运行一次，被 sandbox 测试断言失败阻断，未取得有效覆盖率结果。
- [x] `rg --files -uu -g '*.profraw' -g '!target/**'` 无输出；`git diff --check` 通过。
- [x] 更新本阶段 spec 与 roadmap 的真实状态；记录代码增量、必要性、回归结果、验证阻断及尚未实施的阶段。

不将 Node 窄范围通过替代 workspace 门禁，不修复、格式化或回退同期 sandbox 改动来让本阶段“变绿”，不排除生产代码、不降低覆盖率阈值。门禁恢复后再进入阶段 2。
