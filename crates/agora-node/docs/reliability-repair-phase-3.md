# Phase 3 — Session、准入和收尾

在已批准的路线图内执行，不新增依赖、服务或兼容分支。所有修改由当前 agent 完成，保持未提交。

1. R08：给 AgentOutput 增加中立的同步 session 观察回调；Codex 在解析 thread.started 时通知，Daemon 使用现有 Store CAS 当场保存。保持 session ID 不进入 Channel 渲染事件。先测试 timeout / cancel / 非零退出，以及后续 resume。
2. R09：fan-out 所有 Run 打开后，在已有 JoinSet 内发布初始排队状态，用一次性批次启动信号阻止提前执行；任何初始失败或准入 future 被取消都会关闭启动信号，已发布的卡片做有时限的失败收尾。无需 async Drop、脱管任务或持久事务。
3. R10：停止、重置等管理 handler 标记为控制操作，使用独立有界准入。保留接收顺序，但尾链在 scheduler 插入任务 / reset 插入屏障后释放，不等待网络投递；避免 stop 抢在先前消息登记前执行而漏停。普通任务与 /ask 执行仍受现有容量限制。忙碌回复使用单独一个槽，不能阻塞接收循环。
4. R18：stderr 收到即记录并保留有限诊断；连接失败记录可辨别但不泄露 URL 凭据的原因。Daemon 只报告本次终态发布失败，不冒充 Channel 的重试耗尽结论。

验证：每项先观察旧代码目标断言失败，再运行聚焦测试；完成阶段后 Node 全量测试 / Clippy，更新架构与数据流规范并继续阶段 4。最终 workspace 验证统一在全部阶段后执行。

## 实施记录

- 新 session 在超时、取消和非零退出后均已写入 Store；不靠最终输出回调保存。
- 部分 fan-out 初始失败 / 准入超时测试均收敛为终态，未启动 backend，并释放 scheduler capacity。
- 满载且 busy reply 阻塞时 stop 仍有效；reset 等待期间帮助可回复，接收顺序仍保留到 scheduler 登记。
- 独立测试进程验证无换行 stderr 在取消前即可看到；重连日志测试验证保留 code 且不泄露 peer URL。
- Node 全量 362 passed，0 failed。所有目标回归已观察红灯后通过；实施中的借用/测试方法可见性编译错误已修正，未跳过检查。进程测试启动等待按已有真实进程测试量级调整，避免并行运行时过短的夹具期限。
- 本阶段 Node Clippy `-D warnings` 通过，0 warning；最终 workspace 结果见 roadmap。
