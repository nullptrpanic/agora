# Node Reliability Phase 2 Implementation Plan

> **For agentic workers:** Use `superpowers:executing-plans` inline. No subagents, commits, pushes, new dependencies, or worktree changes. User approved continuing all remaining phases; report progress and continue after each Node gate.

**Goal:** 修复 Lark 分片、卡片预算、认证恢复和两端后台投递数量失控。

**Architecture:** Lark 协议缓存由单次连接拥有，完成重组后才参与 admission；认证分类留在 API；卡片按最终 JSON 字节预算裁剪；一个普通快照 worker 持有整个更新循环，不制造等待投递锁的任务队列。保留现有平台差异、即时终态和非幂等首段策略。

**Tech Stack:** Rust 2024，现有 Tokio、prost、serde_json、reqwest。

**Spec:** `spec/architecture/agent-channel.md` 与 [完整审查](node-audit-2026-09-05.md)。

## Global constraints

- 每项先观察实际回归失败，再最小修复，运行聚焦测试；每阶段执行 Node tests/Clippy，最终才运行 workspace coverage。
- Cargo jobs/test threads 16，同一 target 不并行运行 Cargo；不修改同期 sandbox 文件。
- 不改变停止按钮、私聊持久消息、数据库、配置或用户输出语义；超过平台上限仍按既有规则显式提示截断。

## R03 — Lark 分片

Files: `src/channel/lark/lark_api.rs`、新私有 `src/channel/lark/lark_api/fragments.rs` 与相邻测试。

- [x] 回归：实际 WebSocket 先收到 seq=1 再 seq=0，未齐时无 ACK、齐后只投递一次，ACK 复用完成帧头；重复片不能提前完成。
- [x] 运行 `cargo test -p agora-node --lib --jobs 16 fragmented -- --test-threads=16` 观察旧代码错误 ACK。
- [x] 私有重组器接口 `fn reassemble(&mut self, frame: &mut LarkFrame) -> Result<bool>`；单次连接持有。按 `message_id` 缓存，验证 sum/seq，一致重复不计数，冲突拒绝；完整才覆盖 payload 并返回 true。

```rust
match fragments.reassemble(&mut frame) {
    Ok(false) => continue,
    Ok(true) => { /* enter existing bounded event admission */ }
    Err(error) => { /* log non-sensitive diagnostic and ACK 500 */ }
}
```

- [x] 限制同时 64 组、每组 64 片、全部缓存 payload 1 MiB、从首片起 5 秒 TTL；拒绝不完整头、越界、冲突、超限；断连丢弃缓存。测试乱序、重复、到期、重新传输、容量及无片头普通帧。
- [x] 重跑 Lark 接收测试并同步协议 spec。

## R07 — token 过期与业务认证错误

Files: `src/channel/lark/lark_api.rs`、`src/channel/lark/card.rs`、相邻 API 测试。

- [x] 回归 HTTP 200 + 99991663 先失败、刷新 token 后相同消息发送/更新成功；反复认证失败至多刷新一次，权限错误不刷新。
- [x] 请求 token 时保留服务端 `expire`，缓存使用真实 TTL 并提前少量刷新，拒绝空 token / 无效 TTL，失败不缓存；同步 HTTP mock 的真实 token 响应结构。
- [x] 业务错误保留 code，认证判断 `is_unauthorized` 支持官方 tenant token 失效码；非幂等初始发送仅在明确认证拒绝后重试。

```rust
Err(error) if !refreshed && is_token_invalid(&error) => {
    api.invalidate_cached_tenant_access_token(&token).await;
    refreshed = true;
}
```

- [x] 以可控缓存期限测试有效 token 复用、过期刷新、旧请求不能清除新 token；重跑 Lark API/card 测试并更新 spec。

## R04 — 最终卡片 JSON 预算

Files: `src/channel/lark/card.rs`、`card/tests/content.rs`、`lark_api.rs`。

- [x] 回归 20 条长命令、中文和 JSON 转义文本构成卡片；最终 `serde_json::to_vec(&card).unwrap().len() <= 30_000`，保留终态、答案、usage、最新过程与截断提示。
- [x] 渲染只在超限时按优先级删旧过程、缩减最长可显示文本，最后检查序列化字节。普通小卡片保持原结构；header/name 等也受预算约束，不能无限循环。
- [x] 对所有 outgoing card 在 API 边界验证大小；阶段 4 已补齐超大命令列表的显式限额提示与逐个 agent 的文本操作指引，不静默丢失按钮或发送非法卡片。
- [x] 重跑 card 内容与 API 测试，同步最终预算和取舍到 spec。

## R06 — 单 worker 持有整个发送周期

Files: `src/channel/lark/card.rs`、`src/channel/telegram/rich_message.rs` 与相邻 API 测试。

- [x] 回归持有真实投递锁，反复更新并让调度器运行；每 Run 只增加一个普通更新 worker，释放锁后发送最新内容。覆盖 HTTP 等待期间又有更新及终态插入。
- [x] 在 worker 完成一次发送并检查 dirty 之后才清 `flush_scheduled`；若还有新版本则由同一个 worker 延迟后继续，不再 spawn；失败不形成忙循环、不自动重试不安全的首发。

```rust
let version_before_flush = state.version;
// await flush; then under the state lock:
if state.version == state.sent_version || state.version == version_before_flush {
    state.flush_scheduled = false;
    return;
}
// continue in this worker with the normal interval
```

- [x] 终态不能被晚来的中间事件覆盖；保留 Telegram 有限终态重试，worker 不长期持有已被丢弃的 run；测试慢投递、失败、终态和释放。
- [x] Node 全量测试、Clippy；同步 spec、记录真实结果，然后继续阶段 3。

## Primary references

- [官方 Python SDK 分片与 ACK](https://github.com/larksuite/oapi-sdk-python/blob/v2_main/lark_oapi/ws/client.py)：按 `message_id/sum/seq` 重组，未齐时不 ACK，完整后沿用收到的完成帧。
- [官方 tenant token API](https://open.feishu.cn/document/server-docs/authentication-management/access-token/tenant_access_token_internal) 与 [Go SDK token manager](https://github.com/larksuite/oapi-sdk-go/blob/v3_main/core/tokenmanager.go)：有效期来自响应 `expire`。

## 实测验收

- 分片、认证、最终 JSON 字节预算、普通/重试 worker 的目标回归均先观察旧代码断言失败，再修复通过。
- Node 全量：357 passed，0 failed（独立 `target/node-reliability`，jobs/test threads 16）。Node Clippy `-D warnings` 通过。
- HTTP 等待期间新版本不丢失；裁剪保留完整可容纳答案、最新 thinking/普通进度；不改回调身份。
- 测试夹具临时出现类型/导入错误均已修正并重跑，没有豁免测试。
- Spec 已同步分片、TTL、worker、预算和当前私聊停止能力；无新依赖。
- [官方认证错误码](https://github.com/larksuite/oapi-sdk-go/blob/v3_main/core/constants.go)：99991663 / 99991664 / 99991671。
