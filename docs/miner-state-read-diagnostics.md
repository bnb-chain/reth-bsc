# Miner 状态读取与交易执行拆分

只在 miner 构建入口启用，标签 `kind=regular/speculative/rebuild/empty`、
`turn=inturn/outofturn`。import、RPC、bid simulation 不启用本组观测。
观测本身不改变交易选择、错误传播、gas 或构建预算。预热接入见下文。

## 状态读取

以下指标均带前缀 `reth_bsc_build_diagnostics_`：

| 指标 | 含义 |
| --- | --- |
| `state_reads_total{stage,layer,resource,outcome,kind,turn}` | 读取调用累计次数，包括返回错误的调用 |
| `state_read_errors_total{stage,layer,resource,outcome,kind,turn}` | 返回错误的调用累计次数，是 reads 的子集 |
| `state_read_seconds{stage,layer,resource,outcome,kind,turn}` | 每次构建该类别读取的累计墙钟耗时；`_sum` 是跨构建累计秒数 |

固定标签：

- `resource=account/storage/code`；不统计 BLOCKHASH。
- `stage=prepare/selection/finish`；prepare 含 pre-execution，selection 为用户交易循环，
  finish 含 post-execution 系统交易。空块只有 prepare/finish。
- `layer=cached`：进入 `CachedReads` 的调用，`outcome=hit/miss`。有实际下层调用为 miss；
  命中缓存中“不存在的账户”也是 hit。缓存查询、克隆、插入和 miss 时下层读取均在耗时内。
- `layer=provider`：缓存未能满足时对 `StateProviderDatabase` 的实际调用，`outcome=read`。
  包括 storage miss 隐式触发的账户加载；不存在的账户可能直接返回零、不调用 storage provider。
  因此不能把 provider 的 account 次数直接当作 cached account miss 次数。

`cached` 是 **REVM State 缓存下面的 CachedReads 边界**。REVM journal/State 内部直接满足的
访问不会到达这里；这些数字不是全部 opcode 的状态访问次数，也不是 MDBX 页缓存命中率。
provider 内可能还经过 overlay/cache，耗时包含这条 provider 路径的等待和处理，不能称为纯磁盘 IO。
同一笔读取的 cached 耗时包含 provider 耗时，两层不能相加。

每个 DB 自有局部计数，Drop 时统一上报，包含失败、取消、丢弃构建中已经返回的读取。
每次新构建重置观测，即使复用同一份 CachedReads 也不会继承上一构建的计数。
未访问的类别不产生 histogram 样本；`_count` 是访问过该类别的构建数，**不是读取次数**。
panic 时已完成的调用仍会汇总，正在 unwind 的那次调用不计入。运行中尚未释放的 DB 未上报。

## 用户交易循环与执行器

`selection_stage_seconds{kind,turn,stage,status}` 每次构建汇总五个互不重叠的阶段，
`status=complete/interrupted` 与 `selection_wall_seconds` 一致；进入循环后未走到的阶段记录零。
五个阶段的 `_sum` 之和等于相同标签/样本集合的 selection_wall `_sum`（浮点舍入除外）：

| stage | 边界 |
| --- | --- |
| control | 每轮取消/预算检查与阶段切换间隙；末次退出到 selection_stopped 的处理 |
| iterator | `best_tx_list.next()`，包含迭代器内部过滤与最后一次返回 None |
| precheck | 黑名单、tip/gas/blob 检查、交易准备、sidecar 获取以及这些分支的日志和 invalidation |
| execute | `builder.execute_transaction`，含参数克隆、校验、EVM、receipt/状态提交 |
| postprocess | 执行返回后的成功/错误处理、费用累计、日志、sidecar 保存和 invalidation |

迭代器初始化在原有 prepare 阶段。已有 skipped_transaction_seconds 与上述阶段有重叠，不能另加。
已有采样 build budget 日志增加 iterator/precheck/execute/postprocess 毫秒字段，限频保持不变。

`transaction_stage_seconds{kind,turn,stage}` 在执行器销毁时汇总以下子阶段，
`transaction_stage_calls_total` 是实际返回的调用次数（含 Err），不是成功或上链交易数：

| stage | 边界 |
| --- | --- |
| executor_prepare | execute_transaction_without_commit 总时长减去其 transact 时长，含前置校验、上下文准备、结果包装、错误处理 |
| evm_transact | `evm.transact`，包含 REVM 自身校验、状态读取、EVM 执行；包含返回校验错误及合约 revert |
| receipt_commit | 非系统用户交易的 commit_transaction，含 receipt 创建、gas/lane 累计和状态提交/hook |

它们是 selection `execute` 的进一步拆分；外层交易转换、保存交易和计时开销仍在 execute 中。
空块没有用户执行样本。系统交易的读取记在 prepare/finish，耗时继续看已有 finish_stage 指标。
这些均为墙钟时间，不能当 CPU 时间；不同子阶段样本数量可能不同，不可直接相加 P95 或各自均值。

## PromQL

选择阶段中 CachedReads 每秒命中/未命中次数：

```promql
sum by (instance, resource, outcome) (
  rate(reth_bsc_build_diagnostics_state_reads_total{
    kind="regular",turn="inturn",stage="selection",layer="cached"
  }[5m])
)
```

每个资源命中率：

```promql
sum by (instance, resource) (
  rate(reth_bsc_build_diagnostics_state_reads_total{
    kind="regular",turn="inturn",stage="selection",layer="cached",outcome="hit"
  }[5m])
)
/
sum by (instance, resource) (
  rate(reth_bsc_build_diagnostics_state_reads_total{
    kind="regular",turn="inturn",stage="selection",layer="cached"
  }[5m])
)
```

每次 provider 读取的平均毫秒（按真实调用数作分母）：

```promql
1000 * sum by (instance, resource) (
  rate(reth_bsc_build_diagnostics_state_read_seconds_sum{
    kind="regular",turn="inturn",stage="selection",layer="provider"
  }[5m])
)
/
sum by (instance, resource) (
  rate(reth_bsc_build_diagnostics_state_reads_total{
    kind="regular",turn="inturn",stage="selection",layer="provider"
  }[5m])
)
```

provider account/storage/code 读取占交易循环墙钟时间比例：

```promql
sum by (instance) (
  rate(reth_bsc_build_diagnostics_state_read_seconds_sum{
    kind="regular",turn="inturn",stage="selection",layer="provider"
  }[5m])
)
/
sum by (instance) (
  rate(reth_bsc_build_diagnostics_selection_wall_seconds_sum{
    kind="regular",turn="inturn"
  }[5m])
)
```

该比例量化实际观测的 provider 读取时间占比，不是移除读取即可获得的加速比例。
同一 attempt 的读取和循环统计在释放时先后上报，短窗口 scrape 边界可能稍有错位；
比较时使用同一 instance/kind/turn 和足够长的窗口。没有读取时相关序列可能不存在，不等于监控失败。

每个阶段平均每次构建耗时：用对应 `selection_stage_seconds_sum / selection_stage_seconds_count`。
执行器各子阶段平均每次调用耗时：用 `transaction_stage_seconds_sum / transaction_stage_calls_total`。

## 开销与验证

无逐读/逐笔日志，不在每次读取上注册或更新 Prometheus handle；局部 Cell 保存计数，
阶段标识通过 relaxed atomic 读取。provider miss 有两层计时，命中也需要单调时钟调用。
观测本身有开销，生产性能影响需要同机同负载 A/B 压测；本地单元测试不能给出生产开销结论。

本地测试覆盖冷热读、负账户缓存、storage 隐式账户读取、错误传播/不缓存失败读取、
阶段归属与失败退出汇总、重建计数隔离、执行器禁用路径和循环分段对账。

## 矿工并行预执行与共享读取缓存

启用 `--engine.txpool-prewarming` 后，矿工在正式交易循环前启动当前 attempt 的投机预执行。
对齐 go-bsc `PrefetchMining` 的共享读取机制：MEV（`bid-block-enabled`）开启时3个 worker，
关闭时 `max(3, 3 * available_parallelism / 5)`。线程池在 engine 初始化时创建，跨 attempt 复用。
非矿工节点仍使用 Reth 既有 canonical-head 预热；矿工不同时运行那组后台任务，避免重复执行。

每个 attempt 创建父块 hash 固定的共享读取缓存。层次是：
REVM State → 本地 CachedReads → 共享原始状态读取缓存 → StateProviderDatabase。
worker 和 miner 都能填充读取缓存；miner 从开始就持有同一 Arc，后续填入的数据立即可见，
没有快照发布周期，也没有“启动时未取得就永久回退”的缺口。空候选不启动预执行。
账户不存在、零 slot 和空 bytecode 都可命中，读取错误不缓存。

账户/storage/code 分16片，每片读写锁；provider 读取在锁外，允许并发 miss 重复读取。
不共享 REVM State/journal，不采用投机执行的写结果。只有预执行关闭 nonce 检查，
正式执行的 nonce、余额、gas、签名校验保持不变。worker 显式预读 sender、receiver、code、
access list，再试执行普通交易以发现动态 storage；blob 跳过。试执行结果只提交到该 worker 的私有 State，
随 worker 退出丢弃，正式执行不采用这些结果。

对齐的是缓存共享和并行预读，不是逐行翻译 go-bsc：这里用共享 iterator 直接派发，最多每 worker
一笔在途，没有额外发送队列；矿工推进每个 sender 的已选 nonce，worker 跳过已经处理的 nonce。
它等价于 go-bsc Forward 的追赶目的，不修改真实交易池或正式选择顺序；已在途的交易不会中断。
线程池有全局在途额度，过期 worker 尚未返回时新 attempt 只使用空闲额度，不无限排队。
交易循环退出或 attempt drop 会停止新预执行，现有 EVM 调用结束后释放 provider 和额度，
主线程不 join，不在结束阶段额外等待。每个 worker 的执行状态不跨 attempt 复用。

共享缓存最多131072项（账户/slot/code合计），代码原始字节另有32MiB准入预算。
另有最多131072个 sender 的已选 nonce 游标。上述限制不是严格内存上限，
未包含 map/allocator/分析后 bytecode 开销；满额只停止准入，provider
仍可正常返回。缓存随 attempt 和残余在途 worker 释放，不跨父块继承，不跨重组混合读取。

`prewarm_cache_total{kind,turn,result=attached/disabled/parent_mismatch}` 替代旧
`prewarm_snapshot_total`。attached 表示持有缓存，并不表示已经命中。
`layer=prewarm,outcome=hit/miss` 表示共享读取层命中，数据既可能来自 worker，也可能来自 miner。
原 `layer=provider,outcome=read` 只记录 miner 真正调用后备 provider 的次数与耗时；不含 worker 的读取。
`miner_prewarm_workers_total{result=started/busy/provider_error}` 和
`miner_prewarm_transactions_total{result=completed/error}` 记录预执行任务与调用结果；`miner_prewarm_active_workers` 表示进程当前在途额度。
completed 只表示调用返回成功，EVM revert/halt 仍可能包含在其中，不代表正式执行或上链成功。
各层耗时包含关系不变，不可相加。worker 在线程结束时汇总交易计数。

开关开启后的收益需要同条件 A/B 验证，不能由线程存在或共享命中率直接推导TPS增益。

### 并行预执行的 proof 路径提示

启用同一个 `--engine.txpool-prewarming` 开关且本次构建拥有 sparse-trie root handle 时，
miner 从该 handle 获取只读提示能力，交给本次 attempt 的预执行 workers。
预执行返回后，提取 touched、非 selfdestructed 账户的变更路径：有账户信息变化或
storage 变化才发送账户路径，storage 仅包含值发生变化的 slot。发送的只有
Keccak(address)/Keccak(slot)，没有投机写值，也不发送正式更新或结束标记。
正式执行的 StateHook 仍是状态更新和结束信号的唯一来源。

这借鉴 go-bsc `StateDB.Finalise` / `stateObject.finalise` 提前准备脏账户和 slot 的 Trie
路径、在 `IntermediateRoot` 中消费预取 Trie 的做法。Reth 原有正式状态更新已经是流式的，
此处新增的是从预执行提前发送 best-effort access hints；预执行路径可能与实际执行不同，
提示未被使用或被丢弃都不影响结果。现有 Reth `44371a` 已提供此接口，无须升级到2.8或修改依赖。

限制按 attempt 隔离：跨 worker 去重，账户与 slot 合计最多16384个不同路径，最多256批，
每批最多64个路径。每笔投机交易最多检查64个账户、每个账户最多检查64个 slot，最多
提取64个路径；超出的部分不预取，正式 proof 仍走原路径。尚不足一批时，在 worker
正常耗尽候选迭代器时发送，不在取消/预算截止时补发。
正式交易循环结束时关闭发送门，丢弃未发送的尾批；发送门仅保护去重/批处理与非阻塞
channel 提交，不包含 Keccak、EVM 或 provider 操作。关闭返回后，在途预执行也不能再发提示。
关闭前已提交给 root 管线的提示可能仍会被处理，并不会被此门撤回。

新增指标前缀为 `reth_bsc_miner_prewarm_`：

- `proof_hint_batches_total`：通过提示接口提交的批数。
- `proof_hint_targets_total{resource="account|storage"}`：提交的去重路径数。
- `proof_hint_rejections_total{reason="parent_mismatch|duplicate|limit"}`：错父块记录、重复或预算拒绝。
  错父块计数按被拒绝的记录调用次数，其他计数按路径数。
- `proof_hint_budget_exhausted_total{reason="targets|batches"}`：本次 attempt 用尽提示预算。

提交计数不代表后台已消费、proof 命中或上链成功；去重路径数上限也不是整个 proof 管线的
严格字节/任务数上限。当前改动没有跨候选复用可变 sparse trie，也未改变 worker 数量和调度。
需要同时比较 finish 时剩余 proofs、finish drain、root_wait、总构建时间及有压空块率，
不能仅凭新增提示计数断言性能提升。
