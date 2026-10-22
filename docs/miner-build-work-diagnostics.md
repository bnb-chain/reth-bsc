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

`reth_bsc_miner_empty_fallback_wait_skipped_total{turn="inturn/outofturn"}`：已有正常候选，且通过任务 ID 确认唯一尚未收集结果的任务为空块兜底时，每个 job 加一。此时请求中止该空块任务并结束候选等待，随后仍遵守原提交时间。不是上链空块率，也不表示后台同步状态根计算已经停止；已运行的同步计算可能到返回/让出执行权后才响应中止。其他正常构建仍在时继续按原规则收集。

排查顺序：任务窗口小→看父块/任务到达；attempt 窗口正常但 execution 窗口小→看 permit_wait/prepare；窗口正常且到期停止→看 skipped 时间占比和线程 CPU。指标只能定位耗时分布，锁/IO/单核瓶颈仍需线程采样。

## 父块到矿工启动的时间账

新增前缀 `reth_bsc_mining_startup_`，每个实际启动的 job 记录一次，非每个 attempt。固定标签：`source=local/network/unknown` 是父块首次观测来源；`turn=inturn/outofturn` 是子块轮次；`position=last/other` 区分轮次末块；`trigger=canonical/startup/retry` 区分主链通知、启动和定时重试。分析正常路径先过滤 `turn="inturn",trigger="canonical"`，再按 source、position 拆分。

| 指标 | 含义 |
|---|---|
| `stage_seconds{stage="cache_for_next"}` | 从父块执行结果整理账户/storage 缓存 |
| `stage_seconds{stage="prepare_context"}` | 矿工收到主链通知到入队，扣除 cache_for_next；含投票相关通知处理、快照/资格检查、缓存复制 |
| `stage_seconds{stage="mining_queue"}` | mining context 入队到出队 |
| `stage_seconds{stage="prepare_job"}` | 出队到 job 提交；含有效性复查、header/参数准备、缓存复制、job 创建 |
| `stage_seconds{stage="job_schedule"}` | job 提交到第一次运行；与已有 attempt 的 queue_seconds 不同 |
| `stage_seconds{stage="ingress_to_notification"}` | 父块首次观测到矿工收到主链通知，单调时钟 |
| `stage_seconds{stage="ingress_to_import"}` | 父块首次观测到导入处理开始，含入口通道/排队。网络以 import future 首次运行计，本地以已执行区块处理入口计 |
| `stage_seconds{stage="import_to_notification"}` | 导入处理开始到矿工收到主链通知；含引擎工作、fork choice、通知投递及消费等待 |
| `stage_seconds{stage="new_payload"}` | engine.new_payload await 耗时，含引擎排队和执行校验，是上一项的子集。本地已执行块直接插入没有此样本，不能当零耗时执行 |
| `timestamp_offset_seconds{point="parent_at_ingress/parent_at_notification"}` | 观测时刻相对父块毫秒时间戳的偏移；direction=behind 表示观测晚于区块时间，ahead 表示早于。包括出块者晚提交与时钟偏差，不能称为纯网络延迟 |
| `timestamp_offset_seconds{point="target_at_job_start"}` | job 开始相对计划出块时间的偏移，ahead 表示尚有余量，behind 表示已过目标时间 |
| `budget_seconds{component="raw_slot_remaining/fixed_margin/root_reserve/total_budget_cap/transaction_window"}` | 原始剩余时间（过期为零，超期量另见 timestamp_offset）、固定余量、收尾 reserve、构建预算上限、实际交易窗口，均为秒；是预算参数，不是可相加的耗时 |
| `jobs_total{limiter="slot_remaining/last_in_turn_cap/period_cap/recovery_floor"}` | 当前预算受哪个边界限制。slot 与 cap 相等优先记 slot；recovery_floor 表示实际窗口高于原始裁剪结果，兼容恢复预算路径 |

前五个 stage 在同一 job 上可相加得到通知到 job 开始的时间；ingress_to_import + import_to_notification = ingress_to_notification。不要重复加上 new_payload，也不要把独立 P95 相加。fork choice 返回可能晚于通知到达，不以其返回作为通知链路终点。被资格检查拒绝或队列复查丢弃、未启动的 job 不在此样本群体。

来源关联只保存时间戳和固定字段，不持有区块：LRU 最多2048条、有效期120秒，首次 ingress 优先，重复广播不重置。网络入口是 ImportHandle 收到完整块；绕过该 handle 的内部路径在 import service 入口兜底，所以不包含此前的协议解码、下载和网络传输队列。local 是 submit_payload 入口而非实际发包时刻。关联缺失/过期标 unknown，缺少的阶段不上报，不填零。startup/retry 的 notification 实际表示重试开始，必须用 trigger 与 canonical 分开。

新增 INFO `Sampled mining startup budget`：只对交易窗口 <300ms 的 in-turn job 采样，全进程最多10秒一条。含 trace_id、父块 hash、区块号、来源、轮次位置、各阶段耗时和预算。与已有构建工作日志的限频独立。无逐交易埋点，指标不受 INFO 采样限制。

示例：按来源、轮次位置看各阶段平均毫秒（每阶段自己的样本分母）：

```promql
1000 * sum by (instance, source, position, stage) (
  rate(reth_bsc_mining_startup_stage_seconds_sum{turn="inturn",trigger="canonical"}[5m])
) / sum by (instance, source, position, stage) (
  rate(reth_bsc_mining_startup_stage_seconds_count{turn="inturn",trigger="canonical"}[5m])
)
```

诊断：parent_at_notification 偏大→父块提交/导入链路；cache/context/queue/prepare_job/schedule 偏大→矿工内部；原始剩余时间充足但交易窗口小→reserve 或轮次末块上限。时间戳偏移与本机单调时长是两种口径，先看同一条汇总日志对账，再看群体分布。
