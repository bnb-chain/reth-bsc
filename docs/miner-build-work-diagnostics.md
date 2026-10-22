# 构建预算与无效扫描诊断

用于解释“CPU 总利用率不高、缩短 reserve 却没有明显增加打包量”。本次只增加观测，不改变打包预算、交易校验或选块规则。

下表省略 Prometheus 前缀 `reth_bsc_build_diagnostics_`。耗时单位均为秒；直方图的 `_sum / _count` 可计算平均值。`kind=regular/speculative/rebuild`，`turn=inturn/outofturn`；空块兜底不走交易循环，不纳入这些构建工作指标。重建共用原任务截止时间。

| 指标 | 含义与计时边界 |
|---|---|
| `job_slot_remaining_seconds` | 每个任务开始时距原提交截止的剩余时间，已减去现有安全 margin、截断到零；在 BuildControl 计算最终预算之前采样，仅按 turn 区分 |
| `job_transaction_window_seconds` | 每个任务经 BuildControl 计算后得到的交易选择预算，仅按 turn 区分 |
| `attempt_transaction_window_seconds` | 每次构建进入函数时，距交易截止的剩余时间；尚未等待许可/准备状态 |
| `permit_waiters` | 当前正在获取许可的构建数（Gauge），任务尚未结束时也能看见等待；成功获取或中断退出时减回 |
| `permit_wait_seconds` | 等待容量为 1 的本地构建许可；status=acquired 为获得许可，interrupted 为获得前退出的部分等待时间 |
| `prepare_seconds` | 获得许可到进入交易循环，含状态/provider/root hook 初始化；不含许可等待。status=complete/interrupted 区分完整和中断准备 |
| `execution_window_seconds` | 真正进入交易循环时剩余的交易窗口；过期记零，没进入循环则无样本 |
| `selection_wall_seconds` | 交易循环墙钟耗时，含扫描、校验、执行及现有逐笔日志开销；status=complete/interrupted；失败构建也上报，区别于仅成功候选的 execution_seconds |
| `scanned_transactions_total` | 迭代器实际返回的候选交易次数；同一交易在不同构建中重复计数，不是入池 TPS |
| `executed_transactions_total` | 成功通过交易校验并完成执行的用户交易次数；含合约 revert，不含系统交易，不代表上链 TPS |
| `skipped_transactions_total` | 本次构建显式跳过的交易次数，按固定 reason 分类；不含迭代器自动排除的后续交易，不表示全局池删除 |
| `skipped_transaction_seconds` | 每次构建显式跳过交易所花时间之和，含对应迭代器选择、校验、已有日志和 invalidation；是 selection_wall 的子集。无跳过记零 |

跳过原因：`nonce_too_low`、`invalid_transaction`（其他校验失败）、`gas_limit`、`underpriced`、`blacklisted`、`blob_limit`、`blob_ineligible`、`blob_sidecar`。仅选块迭代器内部过滤的交易不会产生显式 skip。遇到致命错误时，scanned 可能大于 executed+skipped。中断阶段耗时以观测对象释放为终点，可能包含退出清理；尚未结束的构建不产生结束时汇总。

已有 `queue_seconds` 是提交任务到开始运行的延迟，**不包含获取构建许可的等待**。比较许可等待、准备时间、执行窗口时，应按同一 kind/turn 看平均值或具体日志，不用不同分布的 P95 做减法。

INFO 日志 `Sampled mining build budget breakdown`：许可等待或准备 ≥20ms、执行窗口 <50ms、跳过交易耗时 ≥20ms、或未完成候选已越过完成截止时触发。整个进程最多每10秒一条，不是每个区块一条。包含 trace_id、区块号、kind/turn、是否完成、初始/实际执行窗口、等待/准备/执行/选交易之后的耗时、停止原因、扫描/成功/各类跳过数量，以及首个非 nonce-low 校验错误。`completion_deadline_elapsed` 仅表示退出时越过截止，不证明退出由超时引起。`post_selection_ms` 包含收尾及构建函数剩余工作，不是纯状态根计算。

指标不受日志级别/日志限频影响。不增加逐笔日志；交易循环仅更新本地计数并测量时间，构建结束时统一上报。现有 DEBUG 日志仍遵从原过滤规则。

排查顺序：任务窗口小→看父块/任务到达；attempt 窗口正常但 execution 窗口小→看 permit_wait/prepare；窗口正常且到期停止→看 skipped 时间占比和线程 CPU。指标只能定位耗时分布，锁/IO/单核瓶颈仍需线程采样。
