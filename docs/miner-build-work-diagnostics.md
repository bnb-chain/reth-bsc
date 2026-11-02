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

空块兜底启用 sparse-trie 时复用矿工的父状态 overlay 缓存，使用独立的 `miner-empty` root/hash/proof 工作通道，避免等待正常构建的流式任务；每次仍有独立的任务、系统交易更新流和状态根结果槽，不读取正常候选的状态根。`reth_bsc_miner_empty_root_builds_total{path="sparse/sync"}` 在空块构建选择路径时加一：sparse 表示成功创建独立任务，sync 表示未启用或未注册 spawner；不保证 sparse 最终成功，任务失败仍保留同步回退。用已有 `build_diagnostics_root_seconds{kind="empty"}` 的 sum/count 比较完成的空候选收尾均值；中止且未完成的任务不在该分母内。空块不继承正常候选已过期的预算，继续保留兜底能力。

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

读取次数、CachedReads 命中/未命中、provider 累计耗时，以及选交易/EVM 子阶段见 [miner-state-read-diagnostics.md](miner-state-read-diagnostics.md)。

## 未满候选的交易选择证据

新增 `reth_bsc_miner_selection_` 前缀。仅增加计数/快照，不改变迭代器、gas 跳过上限或交易截止时间。每笔只更新本地整数，每次退出统一上报；开始/结束读取 pool_size，不遍历交易池。

- `outcomes_total{stage,kind,turn,reason,fill}`：每次循环出口或选中候选一次。reason 为 `transaction_deadline/cancelled/iterator_exhausted/gas_skip_limit`，在实际退出点确定。选中但没有交易循环的候选另标 `empty_fallback/no_selection_metadata`，不猜原因。
- fill 按 gas used / gas limit 分为 `empty/lt50/50to90/90to95/95to99/ge99`；limit=0 为 unknown。区间左闭右开。
- stage=selection 是结束了交易循环的构建尝试，之后仍可能失败或被弃用；stage=selected 由共享候选标识去重，clone/流水线接管不重复计数。selected 不是 canonical 上链统计；外部 bid 等不经过此候选诊断路径的不在分母内。
- `exhausted_total{supply,fill,...}`：仅 iterator_exhausted；supply=`pool_empty/parked_only/pending_present`。pending_present 不能叫“交易不足”：可能包含祖先已消费交易、nonce/其他资格限制。池快照和迭代器不是原子视图。
- `transactions_total{reason,...}`：iterator_yielded 包括祖先排除；ancestor_excluded 单独记录，executed 为本次成功执行用户交易；其余沿用八个 skip reason。**ancestor_excluded 不计入 gas_skip_limit**。这些值是遇到次数，不是唯一交易数或池删除数。
- `filtered_attempts_total{reason,fill,...}`：该候选曾发生某类过滤；多类可同时出现，不能相加作为未满块数，也不能单独证明因果。迭代器内部自动隐藏的交易仍不计入显式 skip。
- `gas_used/gas_remaining/fill_ratio/remaining_seconds/pool_transactions`：直方图；pool_transactions 按 boundary=start/end、pool=pending/queued/basefee/blob 拆分。无 BuildControl 时 remaining_seconds 记0。selection 的 gas 不含收尾系统交易，selected 使用最终块头 gas。
- selected 且 gas 填充率低于95%时输出 INFO `Selected underfilled candidate selection evidence`，含块号、原始退出原因、两次池快照、yielded/ancestor/executed、八类 skip（顺序同上文 SKIP_REASONS）、剩余窗口。无逐笔日志，无 hash/address/block-number 指标标签。

看每分钟未达到95%填充的选中候选，按原始退出原因拆分：

```promql
sum by (instance, kind, reason, fill) (
  increase(reth_bsc_miner_selection_outcomes_total{job=~"$job",instance=~"$instance",stage="selected",fill=~"empty|lt50|50to90|90to95"}[1m])
)
```

看平均选中候选 gas used（Mgas；不是严格 canonical 平均）：

```promql
sum by (instance) (rate(reth_bsc_miner_selection_gas_used_sum{job=~"$job",instance=~"$instance",stage="selected"}[1m]))
/
sum by (instance) (rate(reth_bsc_miner_selection_gas_used_count{job=~"$job",instance=~"$instance",stage="selected"}[1m])) / 1e6
```

严格上链平均仍应对固定时间窗 canonical 区块求 sum(gasUsed)/区块数（含空块），不要累加三个 validator 的重复链数据。

## 50ms 兜底起点

启用原有 `BSC_MINING_ALLOW_LATE_FINISH` 的 in-turn 构建，在状态/provider/迭代器准备完成、真正进入交易执行时，首次将交易截止时间提高到至少 now+50ms；已有更长窗口不缩短。job 创建时保留原调度用预留，避免过期 job 在首个构建派发前就退出，但它不再是实际执行兜底的计时起点。

同一 job 的所有 clone/重建/预热线程共享一次性截止时间，重试不能续杯；关闭开关和 out-of-turn 保持原行为。取消/换父仍生效。内部 finish 截止相应保留配置的收尾 reserve；这不保证50ms内一定有可执行交易，也不保证候选最终上链。

`reth_bsc_miner_execution_floor_total{outcome="extended/unchanged"}` 每个首次进入执行的已启用 job 一次；`reth_bsc_miner_execution_floor_extension_seconds` 为相对原截止时间的增量。已有 execution_window_seconds 现在读取更新后的共享截止时间，job_transaction_window_seconds 仍是任务创建时的初始窗口。

## 交易去向与候选作废

本节需要同时构建本地 Reth 依赖修改；只部署 reth-bsc 的改动不会产生池移除原因指标。

`reth_transaction_pool_removal_transactions_total{reason}` 在实际移除路径计数，不按请求 hash 数计数：
- canonical_mined：canonical 更新时确实从池中找到并移除的交易，不是全部链上交易。
- replaced：同 sender/nonce 的旧交易被成功替换。
- capacity：容量淘汰，包含因此一并移除的后续 nonce 交易。
- state_update_invalidated：状态更新/入池联动检查导致不再有效而移除，不等同于入口拒收。
- explicit_remove / explicit_remove_with_descendants / explicit_remove_sender：调用相应移除接口；不能仅凭接口名推断业务原因。
- explicit_prune：显式按已挖出语义移池，不证明 canonical 上链。

`reth_transaction_pool_removal_context_transactions_total{reason="queued_expired"}` 记录维护任务实际删除的过期交易，是 explicit_remove 的子集，不能再次加到移除总数中。没有额外事件订阅队列或逐笔日志，不增加 hash 标签。已有入池失败/invalid 指标与以上池内移除不是同一个口径。

`reth_bsc_miner_pipeline_invalidated_branches_total{reason}` 记录分支首次作废，原因包括 parent_changed、candidate_changed、parameters_changed、plan_rejected、window_unavailable、compute_error、compute_cancelled、result_receiver_dropped、launch_failed、adoption_abandoned。保留首次原因，后续失败不覆盖。`invalidated_entries_total` 是作废时协调器仍排队的 entry 数，可能尚未完成构建，不是区块数或交易数；接管任务自行放弃时没有该排队快照。

`reth_bsc_miner_unselected_candidates_total{kind,reason}` 与 `reth_bsc_miner_unselected_candidate_transactions_total{kind,reason}` 在已完成候选的最后一个诊断引用释放、且从未被选中时记录一次。交易数包含候选系统交易，不是唯一 hash 数；不同候选可能重用同一交易。流水线候选引用分支的首次作废原因，未获得原因或正常未选中为 not_selected_or_unattributed。只持有少量计数/原因，不额外保留交易和执行状态。运行中的分支在完成前已移除、因而无法关联原因的结果可能落入未归因项。

这些不是“丢交易计数”：候选作废后，其交易可能仍在池内或已被其他块打包；已选中但之后未发布/被 reorg 的区块也不在 unselected 中。确认丢交易仍需 driver 接收成功的 hash 与 receipt、pool、同 nonce 替换对账，并考虑重组重入池。当前矿工的 best iterator mark_invalid 只过滤该迭代器，不直接移池。
