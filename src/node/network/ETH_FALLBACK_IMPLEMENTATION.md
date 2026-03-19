# ETH 协议 Fallback - 使用 Reth 原生 API 实现

## 背景

当 BSC 协议请求单个区块失败时，需要一个备用机制通过标准 ETH 协议重新获取该区块。

## 问题：为什么需要请求-响应机制？

在 P2P 网络中，请求和响应是**两个分离的异步消息**：

```rust
// ❌ 不能这样做（同步调用）
let header = peer.get_block_header(block_hash); // 阻塞等待

// ✅ 实际情况
// 时刻 T1: 发送请求
send_message(peer, GetBlockHeaders { hash: block_hash });

// 时刻 T2: 在未来某个时间收到响应（可能几毫秒到几秒）
// 问题：怎么知道响应对应哪个请求？
```

## ✅ 最佳方案：使用 Reth 的原生 API

### 发现的关键 API

在 `/Users/constbh/rustRepo/reth` 中发现了 Reth 提供的原生客户端：

1. **`HeadersClient` trait**：提供 `get_headers_with_priority()` 方法
2. **`BodiesClient` trait**：提供 `get_block_bodies_with_priority_and_range_hint()` 方法
3. **`FetchClient`**：实现了上述两个 trait
4. **`NetworkHandle::fetch_client()`**：获取 `FetchClient` 实例

### 核心优势

✅ **无需自己实现请求-响应机制**：Reth 已经实现好了
✅ **与 BSC 协议一致的 API**：同样是 async/await 风格
✅ **自动的 peer 选择**：Reth 会自动选择最佳 peer
✅ **完整的错误处理**：超时、重试、peer 管理都已处理
✅ **生产级质量**：Reth 核心团队维护，经过充分测试

### 实现代码

```rust
/// BSC 协议失败后的 ETH 协议 fallback
pub async fn eth_request_block_and_await_import(
    peer: PeerId,
    block_hash: B256,
    block_number: u64,
    request_timeout: Duration,
) -> Result<(), String> {
    tracing::info!(
        "BSC protocol failed, attempting ETH protocol fallback using Reth's native APIs"
    );
    
    // 1. 获取 NetworkHandle
    let net = crate::shared::get_network_handle()
        .ok_or_else(|| "Network handle not available".to_string())?;
    
    // 2. 获取 FetchClient（实现了 HeadersClient 和 BodiesClient）
    use reth_network_api::BlockDownloaderProvider;
    let fetch_client = net.fetch_client().await
        .map_err(|e| format!("Failed to get fetch client: {:?}", e))?;
    
    // 3. 请求区块头（使用 Reth 的 HeadersClient）
    use reth_network_p2p::headers::client::{HeadersClient, HeadersRequest};
    use alloy_eips::BlockHashOrNumber;
    
    let header_request = HeadersRequest::one(BlockHashOrNumber::Hash(block_hash));
    let headers_result = tokio::time::timeout(
        request_timeout,
        fetch_client.get_headers(header_request)
    )
    .await??;
    
    if headers_result.data.is_empty() {
        return Err("No headers returned".to_string());
    }
    let header = headers_result.data[0].clone();
    
    // 4. 请求区块体（使用 Reth 的 BodiesClient）
    use reth_network_p2p::bodies::client::BodiesClient;
    
    let bodies_result = tokio::time::timeout(
        request_timeout,
        fetch_client.get_block_bodies(vec![block_hash])
    )
    .await??;
    
    if bodies_result.data.is_empty() {
        return Err("No bodies returned".to_string());
    }
    let body = bodies_result.data[0].clone();
    
    // 5. 构造完整区块
    let block = BscBlock {
        header,
        body: BscBlockBody { inner: body, sidecars: None },
    };
    
    // 6. 发送到导入服务（和 BSC 协议完全一样）
    let nb = BscNewBlock(NewBlock {
        block: Arc::new(block),
        td: U128::from(0u64),
    });
    
    let msg = NewBlockMessage {
        hash: nb.block.header.hash_slow(),
        block: Arc::new(nb),
        td: Some(U256::ZERO),
    };
    
    if let Some(sender) = crate::shared::get_block_import_sender() {
        sender.send((msg, peer))?;
    }
    
    Ok(())
}
```

## Reth 的请求-响应架构

### 数据流

```
reth-bsc/registry.rs
  ↓ net.fetch_client().await
  ↓
FetchClient (Reth)
  ↓ get_headers() / get_block_bodies()
  ↓
DownloadRequest 发送到内部通道
  ↓
StateFetcher (Reth 内部)
  ↓ 自动选择最佳 peer
  ↓ 发送 GetBlockHeaders/GetBlockBodies
  ↓ 通过 oneshot channel 返回响应
  ↓
reth-bsc/registry.rs
  ↓ 构造 BscBlock
  ↓ 发送到 ImportService
```

### Reth 的 FetchClient 实现

```rust
// 在 reth/crates/net/network/src/fetch/client.rs

impl HeadersClient for FetchClient {
    fn get_headers_with_priority(
        &self,
        request: HeadersRequest,
        priority: Priority,
    ) -> Self::Output {
        let (response, rx) = oneshot::channel();
        
        // 发送到内部的下载队列
        self.request_tx.send(DownloadRequest::GetBlockHeaders {
            request,
            response,  // oneshot sender
            priority,
        });
        
        // 返回 future，等待响应
        FlattenedResponse::from(rx)
    }
}

impl BodiesClient for FetchClient {
    fn get_block_bodies_with_priority_and_range_hint(
        &self,
        hashes: Vec<B256>,
        priority: Priority,
        range_hint: Option<RangeInclusive<u64>>,
    ) -> Self::Output {
        let (response, rx) = oneshot::channel();
        
        self.request_tx.send(DownloadRequest::GetBlockBodies {
            request: hashes,
            response,
            priority,
            range_hint,
        });
        
        Box::pin(FlattenedResponse::from(rx))
    }
}
```

### Reth 内部处理

```rust
// StateFetcher 在后台异步处理请求
impl StateFetcher {
    fn handle_download_request(&mut self, req: DownloadRequest) {
        match req {
            DownloadRequest::GetBlockHeaders { request, response, priority } => {
                // 1. 选择最佳 peer
                let peer = self.select_peer_for_headers();
                
                // 2. 发送 ETH 协议消息
                peer.send(EthMessage::GetBlockHeaders(request));
                
                // 3. 记录待处理请求
                self.pending_headers.insert(request_id, response);
            }
        }
    }
    
    fn handle_block_headers_response(&mut self, peer: PeerId, headers: Vec<Header>) {
        // 4. 匹配请求 ID
        if let Some(response_tx) = self.pending_headers.remove(&request_id) {
            // 5. 通过 oneshot channel 返回结果
            let _ = response_tx.send(Ok(WithPeerId::new(peer, headers)));
        }
    }
}
```

## 与之前方案的对比

| 特性 | 方案 1: 完全依赖 Reth 自动同步 | 方案 2: 使用 Reth 原生 API ✅ | 方案 3: 自己实现请求-响应 |
|------|----------------------------|---------------------------|----------------------|
| **代码量** | ~30 行 | ~140 行 | ~300+ 行 |
| **主动性** | ❌ 被动等待 | ✅ 主动请求 | ✅ 主动请求 |
| **可控性** | ❌ 不知道何时完成 | ✅ 可以立即知道成功/失败 | ✅ 完全可控 |
| **实现复杂度** | 极简 | 中等 | 高 |
| **维护成本** | 极低 | ✅ 低（依赖 Reth） | 高（自己维护） |
| **可靠性** | 依赖 Reth gap detection | ✅ Reth 核心功能 | 依赖自己的实现 |
| **与 BSC 协议一致性** | ❌ 不一致 | ✅ 完全一致 | ✅ 一致 |
| **错误反馈** | ❌ 无法知道是否成功 | ✅ 立即返回结果 | ✅ 立即返回结果 |
| **peer 管理** | Reth 自动 | ✅ Reth 自动 | 需要自己实现 |
| **适用场景** | 不紧急的同步 | ✅ 需要立即反馈的场景 | 特殊需求 |

## 为什么方案 2 是最佳选择？

### ✅ 优势

1. **与 BSC 协议对等**
   - BSC 协议：主动请求 → 等待响应 → 发送到导入服务
   - ETH 协议：主动请求 → 等待响应 → 发送到导入服务
   - **完全一致的处理流程**

2. **立即知道结果**
   ```rust
   if let Err(e) = eth_request_block_and_await_import(...).await {
       // 明确知道 ETH fallback 也失败了
       tracing::warn!("ETH protocol fallback also failed: {}", e);
   }
   ```

3. **复用 Reth 的成熟实现**
   - 不需要自己管理 request_id
   - 不需要自己管理 oneshot channels
   - 不需要自己选择 peer
   - 不需要自己处理超时

4. **代码清晰易维护**
   - 使用 Reth 公开的稳定 API
   - 不依赖 Reth 内部实现细节
   - 容易理解和调试

### 与方案 1（完全依赖自动同步）的问题对比

**方案 1 的问题：**
```rust
// ❌ 方案 1
pub async fn eth_request_block_and_await_import(...) -> Result<(), String> {
    tracing::info!("Letting Reth auto-sync...");
    Ok(())  // 立即返回 Ok，但实际上什么都没做
}

// 调用者不知道是否真的会同步
if let Err(e) = eth_request_block_and_await_import(...).await {
    // 这里永远不会执行，因为总是返回 Ok
}
```

**方案 2 的改进：**
```rust
// ✅ 方案 2
pub async fn eth_request_block_and_await_import(...) -> Result<(), String> {
    let header = fetch_client.get_headers(...).await?;  // 真实的请求
    let body = fetch_client.get_block_bodies(...).await?;  // 真实的请求
    sender.send((block, peer))?;  // 真实的导入
    Ok(())  // 真正完成了工作
}

// 调用者可以明确知道结果
if let Err(e) = eth_request_block_and_await_import(...).await {
    // 如果执行到这里，说明 ETH 协议确实失败了
    tracing::warn!("ETH protocol also failed: {}", e);
}
```

## 使用的 Reth API

### HeadersClient

```rust
pub trait HeadersClient {
    /// 请求区块头
    fn get_headers(&self, request: HeadersRequest) -> Self::Output;
    
    /// 带优先级的请求
    fn get_headers_with_priority(
        &self,
        request: HeadersRequest,
        priority: Priority,
    ) -> Self::Output;
    
    /// 请求单个区块头
    fn get_header(&self, start: BlockHashOrNumber) -> SingleHeaderRequest<Self::Output>;
}

// 构造请求
HeadersRequest::one(BlockHashOrNumber::Hash(block_hash))
```

### BodiesClient

```rust
pub trait BodiesClient {
    /// 请求区块体
    fn get_block_bodies(&self, hashes: Vec<B256>) -> Self::Output;
    
    /// 带优先级和范围提示
    fn get_block_bodies_with_priority_and_range_hint(
        &self,
        hashes: Vec<B256>,
        priority: Priority,
        range_hint: Option<RangeInclusive<u64>>,
    ) -> Self::Output;
}
```

### BlockDownloaderProvider

```rust
pub trait BlockDownloaderProvider {
    type Client;
    
    /// 获取 FetchClient
    async fn fetch_client(&self) -> Result<Self::Client, RecvError>;
}

// NetworkHandle 实现了这个 trait
impl BlockDownloaderProvider for NetworkHandle<BscNetworkPrimitives> {
    type Client = FetchClient<BscNetworkPrimitives>;
    
    async fn fetch_client(&self) -> Result<Self::Client, RecvError> { ... }
}
```

## 调用流程

```rust
// 在 block_import/service.rs 中
tokio::spawn(async move {
    // 1. 先尝试 BSC 协议（快速、批量）
    let bsc_result = batch_request_range_and_await_import(
        bsc_peer,
        start_height,
        start_hash,
        1,
        req_timeout,
    ).await;
    
    // 2. BSC 失败则使用 ETH fallback（兼容性好）
    if let Err(e) = bsc_result {
        tracing::debug!("BSC protocol failed: {}, trying ETH fallback", e);
        
        // 3. 使用 Reth 原生 API 主动请求
        if let Err(eth_err) = eth_request_block_and_await_import(
            announcing_peer,
            start_hash,
            start_height,
            req_timeout,
        ).await {
            // 4. 如果 ETH 也失败，我们明确知道了
            tracing::warn!("ETH protocol fallback also failed: {}", eth_err);
        } else {
            // 5. 成功！区块已经发送到导入服务
            tracing::info!("Successfully fetched block via ETH protocol");
        }
    }
});
```

## 总结

**使用 Reth 的 `HeadersClient` 和 `BodiesClient` 是实现 ETH 协议 fallback 的最佳方案：**

✅ **主动请求**：不依赖被动的 gap detection
✅ **立即反馈**：知道请求是否成功
✅ **与 BSC 一致**：同样的处理流程和数据通道
✅ **复用 Reth**：不重复造轮子，利用成熟实现
✅ **简单可靠**：~140 行代码，生产级质量

这个方案充分利用了您发现的 Reth 原生 API，既简单又可靠！

## 代码位置

- **实现**：`src/node/network/bsc_protocol/registry.rs:178-316`
- **调用**：`src/node/network/block_import/service.rs:383-394`
- **Reth API**：
  - `reth/crates/net/p2p/src/headers/client.rs` - HeadersClient trait
  - `reth/crates/net/p2p/src/bodies/client.rs` - BodiesClient trait
  - `reth/crates/net/network/src/fetch/client.rs` - FetchClient 实现


