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

## 不可变交易池预热快照

启用 `--engine.txpool-prewarming` 后，engine 的既有 worker 发布只读预热快照。
engine builder 将只读获取函数注册给自定义 miner，不另启预热线程，也不共享可变 ExecutionCache。
每次构建只获取一次匹配 parent hash 的已完成快照，克隆的是 Arc 句柄，不复制账户/slot映射。
发布端后续换代不影响进行中的构建；错父块或尚未发布时退回原读取路径，不等待预热。

层次为 REVM State → 本地 CachedReads → 不可变 prewarm snapshot → StateProviderDatabase。
本地缓存 miss 即使由预热满足，仍记在 `layer=cached,outcome=miss`；
新增 `layer=prewarm,outcome=hit/miss` 独立统计快照查找，原 `layer=provider,outcome=read`
只记录真正的后备 provider 调用及耗时。cached 耗时包含其下层，不可相加。
未取得快照时不产生 prewarm 读取样本；`prewarm_snapshot_total{kind,turn,result=available/unavailable}`
按构建计数，包括空候选，不代表上链块数或快照字节命中率。
零 storage 和账户不存在都可命中；缺失 code（含空 code）按快照既有语义回退。

这份快照仅包含父块原始状态的读取，投机执行的写入不被正式执行采用。
不能将不同 parent 的读集合合并，也不绕过 nonce/gas/签名等正式执行校验。
启用 worker 可能增加 CPU/IO/内存占用，收益必须看上述命中及 provider 耗时。
构建启动时快照尚未就绪会立即回退；此实现不会中途刷新快照或阻塞等它就绪。
