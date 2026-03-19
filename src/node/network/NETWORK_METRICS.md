# BSC 网络协议 Metrics 实现

## 概述

添加了 metrics 打点来跟踪在 `on_new_block_hashes` 事件中使用的网络协议（BSC 还是 ETH）。

## 新增的 Metrics

### BscNetworkMetrics

位置：`src/metrics.rs:229-260`

```rust
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.network")]
pub struct BscNetworkMetrics {
    /// BSC 协议请求总数
    pub bsc_protocol_requests_total: Counter,
    
    /// BSC 协议成功次数
    pub bsc_protocol_success_total: Counter,
    
    /// BSC 协议失败次数
    pub bsc_protocol_failures_total: Counter,
    
    /// ETH 协议请求总数（包括 fallback 和直接请求）
    pub eth_protocol_requests_total: Counter,
    
    /// ETH 协议成功次数
    pub eth_protocol_success_total: Counter,
    
    /// ETH 协议失败次数
    pub eth_protocol_failures_total: Counter,
    
    /// ETH 协议作为 BSC 失败后 fallback 的次数
    pub eth_fallback_after_bsc_total: Counter,
    
    /// ETH 协议直接请求次数（无 BSC peer 可用时）
    pub eth_direct_requests_total: Counter,
}
```

## Metrics 打点位置

### 场景 1：有 BSC peer，先尝试 BSC 协议

**位置**：`src/node/network/block_import/service.rs:366-408`

```rust
// 尝试 BSC 协议
metrics.bsc_protocol_requests_total.increment(1);
let bsc_result = batch_request_range_and_await_import(...).await;

if let Err(e) = bsc_result {
    // BSC 失败
    metrics.bsc_protocol_failures_total.increment(1);
    
    // Fallback 到 ETH 协议
    metrics.eth_protocol_requests_total.increment(1);
    metrics.eth_fallback_after_bsc_total.increment(1);
    
    if let Err(eth_err) = eth_request_block_and_await_import(...).await {
        metrics.eth_protocol_failures_total.increment(1);
    } else {
        metrics.eth_protocol_success_total.increment(1);
    }
} else {
    // BSC 成功
    metrics.bsc_protocol_success_total.increment(1);
}
```

### 场景 2：无 BSC peer 可用，直接使用 ETH 协议

**位置**：`src/node/network/block_import/service.rs:410-437`

```rust
// 直接使用 ETH 协议
metrics.eth_protocol_requests_total.increment(1);
metrics.eth_direct_requests_total.increment(1);

if let Err(eth_err) = eth_request_block_and_await_import(...).await {
    metrics.eth_protocol_failures_total.increment(1);
} else {
    metrics.eth_protocol_success_total.increment(1);
}
```

## Metrics 含义说明

### 关键指标

1. **`bsc.network.bsc_protocol_requests_total`**
   - 含义：尝试使用 BSC 协议请求区块的总次数
   - 预期：在有 BSC peer 的情况下每次新区块通知都会增加

2. **`bsc.network.bsc_protocol_success_total`**
   - 含义：BSC 协议成功获取区块的次数
   - 预期：应该接近 `bsc_protocol_requests_total`（高成功率）

3. **`bsc.network.eth_fallback_after_bsc_total`**
   - 含义：BSC 协议失败后使用 ETH 协议作为 fallback 的次数
   - 预期：应该较低（说明 BSC 协议稳定）
   - **关键指标**：用于监控 BSC 协议的可靠性

4. **`bsc.network.eth_direct_requests_total`**
   - 含义：由于没有 BSC peer 而直接使用 ETH 协议的次数
   - 预期：取决于网络中 BSC peer 的数量
   - 用途：监控 BSC peer 的可用性

### 计算指标

通过这些 metrics 可以计算出：

#### BSC 协议成功率
```
bsc_success_rate = bsc_protocol_success_total / bsc_protocol_requests_total
```

#### ETH 协议成功率
```
eth_success_rate = eth_protocol_success_total / eth_protocol_requests_total
```

#### BSC 协议失败率（需要 fallback）
```
bsc_failure_rate = eth_fallback_after_bsc_total / bsc_protocol_requests_total
```

#### 协议使用分布
```
bsc_usage = bsc_protocol_requests_total
eth_fallback = eth_fallback_after_bsc_total
eth_direct = eth_direct_requests_total
total_requests = bsc_usage + eth_direct
```

## Prometheus 查询示例

### 1. BSC 协议成功率（5 分钟）
```promql
rate(bsc_network_bsc_protocol_success_total[5m]) / 
rate(bsc_network_bsc_protocol_requests_total[5m])
```

### 2. ETH fallback 频率（每分钟）
```promql
rate(bsc_network_eth_fallback_after_bsc_total[1m])
```

### 3. 协议使用分布（饼图）
```promql
# BSC 协议使用
sum(increase(bsc_network_bsc_protocol_success_total[1h]))

# ETH fallback 使用
sum(increase(bsc_network_eth_fallback_after_bsc_total[1h]))

# ETH 直接使用
sum(increase(bsc_network_eth_direct_requests_total[1h]))
```

### 4. 失败率告警
```promql
# 当 BSC 协议失败率超过 10% 时告警
(
  rate(bsc_network_bsc_protocol_failures_total[5m]) / 
  rate(bsc_network_bsc_protocol_requests_total[5m])
) > 0.1
```

## Grafana 仪表板建议

### Panel 1: 协议使用趋势（时序图）
```promql
# BSC 协议请求
rate(bsc_network_bsc_protocol_requests_total[1m])

# ETH fallback
rate(bsc_network_eth_fallback_after_bsc_total[1m])

# ETH 直接请求
rate(bsc_network_eth_direct_requests_total[1m])
```

### Panel 2: 成功率仪表（Gauge）
```promql
# BSC 成功率
(
  sum(rate(bsc_network_bsc_protocol_success_total[5m])) /
  sum(rate(bsc_network_bsc_protocol_requests_total[5m]))
) * 100

# ETH 成功率
(
  sum(rate(bsc_network_eth_protocol_success_total[5m])) /
  sum(rate(bsc_network_eth_protocol_requests_total[5m]))
) * 100
```

### Panel 3: 协议使用统计（Stat）
```promql
# 总请求数（1 小时）
sum(increase(bsc_network_bsc_protocol_requests_total[1h])) + 
sum(increase(bsc_network_eth_direct_requests_total[1h]))

# BSC 失败次数
sum(increase(bsc_network_bsc_protocol_failures_total[1h]))

# ETH fallback 次数
sum(increase(bsc_network_eth_fallback_after_bsc_total[1h]))
```

## 监控告警规则

### 高优先级告警

1. **BSC 协议高失败率**
```yaml
alert: BSCProtocolHighFailureRate
expr: |
  (rate(bsc_network_bsc_protocol_failures_total[5m]) / 
   rate(bsc_network_bsc_protocol_requests_total[5m])) > 0.2
for: 5m
severity: warning
annotations:
  summary: "BSC 协议失败率超过 20%"
  description: "BSC 协议失败率: {{ $value | humanizePercentage }}"
```

2. **ETH Fallback 频繁触发**
```yaml
alert: FrequentETHFallback
expr: rate(bsc_network_eth_fallback_after_bsc_total[5m]) > 10
for: 10m
severity: warning
annotations:
  summary: "ETH 协议 fallback 频繁触发"
  description: "ETH fallback 频率: {{ $value }} 次/秒"
```

3. **ETH 协议高失败率**
```yaml
alert: ETHProtocolHighFailureRate
expr: |
  (rate(bsc_network_eth_protocol_failures_total[5m]) / 
   rate(bsc_network_eth_protocol_requests_total[5m])) > 0.3
for: 5m
severity: critical
annotations:
  summary: "ETH 协议失败率超过 30%"
  description: "ETH 协议失败率: {{ $value | humanizePercentage }}"
```

### 信息性告警

4. **无 BSC Peer 可用**
```yaml
alert: NoBSCPeersAvailable
expr: rate(bsc_network_eth_direct_requests_total[5m]) > 0
for: 30m
severity: info
annotations:
  summary: "持续没有 BSC peer 可用"
  description: "在过去 30 分钟内持续使用 ETH 协议"
```

## 使用示例

### 场景 1：正常情况
```
bsc_protocol_requests_total:  1000
bsc_protocol_success_total:   980
eth_fallback_after_bsc_total: 20
eth_protocol_success_total:   18

结论：BSC 协议成功率 98%，偶尔需要 fallback，整体健康
```

### 场景 2：BSC 协议不稳定
```
bsc_protocol_requests_total:  1000
bsc_protocol_success_total:   700
eth_fallback_after_bsc_total: 300
eth_protocol_success_total:   280

结论：BSC 协议失败率 30%，需要调查 BSC peer 或网络问题
```

### 场景 3：无 BSC Peer
```
bsc_protocol_requests_total:  0
eth_direct_requests_total:    1000
eth_protocol_success_total:   950

结论：没有 BSC peer 可用，完全依赖 ETH 协议
```

## 实现细节

### ImportService 结构体变更

添加了 `network_metrics` 字段：

```rust
pub struct ImportService<Provider> {
    // ... 其他字段
    network_metrics: BscNetworkMetrics,
}
```

### 初始化

在 `ImportService::new()` 中：

```rust
network_metrics: BscNetworkMetrics::default(),
```

### Metrics 传递

使用 `clone()` 将 metrics 传入异步任务：

```rust
let metrics = self.network_metrics.clone();
tokio::spawn(async move {
    metrics.bsc_protocol_requests_total.increment(1);
    // ...
});
```

## 验证

编译通过，无 linter 错误：
- ✅ `src/metrics.rs` - 新增 `BscNetworkMetrics`
- ✅ `src/node/network/block_import/service.rs` - 集成 metrics 打点

## 总结

这套 metrics 系统提供了：

1. **完整的协议使用追踪**：BSC vs ETH
2. **成功率监控**：每个协议的成功/失败次数
3. **Fallback 行为分析**：了解何时以及多频繁地使用 fallback
4. **告警能力**：可以基于这些指标设置告警

通过这些 metrics，可以：
- 监控 BSC 协议的稳定性
- 评估 ETH fallback 的有效性
- 发现网络问题（peer 可用性）
- 优化协议选择策略

