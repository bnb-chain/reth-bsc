# reth-bsc archive api list

⚠️ **Error format differs from BSC/Erigon**

⚠️ **`pending` tag is planned, temporarily not supported**

| API | Status | Notes |
|----------|------|------|
| `eth_blockNumber` | Support |  |
| `eth_chainID` | Support |  |
| `eth_syncing` | Support |  |
| `eth_health` | Support | Healthy when sampled completed RPC latency p75 is below 5 seconds; independent of metrics export. See below. |
| `eth_gasPrice` | Support |  |
| `eth_maxPriorityFeePerGas` | Support |  |
| `eth_feeHistory` | Support |  |
| `eth_getBlockByHash` | Support | Size field contains block + header size |
| `eth_getBlockByNumber` | Support | Size field contains block + header size |
| `eth_getBlockTransactionCountByHash` | Support |  |
| `eth_getBlockTransactionCountByNumber` | Support |  |
| `eth_getUncleByBlockHashAndIndex` | Support |  |
| `eth_getUncleByBlockNumberAndIndex` | Support |  |
| `eth_getUncleCountByBlockHash` | Support |  |
| `eth_getUncleCountByBlockNumber` | Support |  |
| `eth_getTransactionByHash` | Support |  |
| `eth_getRawTransactionByHash` | Support |  |
| `eth_getTransactionByBlockHashAndIndex` | Support |  |
| `eth_getRawTransactionByBlockHashAndIndex` | Support |  |
| `eth_getTransactionByBlockNumberAndIndex` | Support |  |
| `eth_getRawTransactionByBlockNumberAndIndex` | Support |  |
| `eth_getTransactionReceipt` | Support |  |
| `eth_getBlockReceipts` | Support |  |
| `eth_estimateGas` | Support |  |
| `eth_getBalance` | Support |  |
| `eth_getCode` | Support |  |
| `eth_getTransactionCount` | Support |  |
| `eth_getStorageAt` | Support |  |
| `eth_call` | Support |  |
| `eth_createAccessList` | Support |  |
| `eth_simulateV1` | Support |  |
| `eth_newFilter` | Support |  |
| `eth_newBlockFilter` | Support |  |
| `eth_newPendingTransactionFilter` | Support |  |
| `eth_getFilterLogs` | Support |  |
| `eth_getFilterChanges` | Support |  |
| `eth_uninstallFilter` | Support |  |
| `eth_getLogs` | Support | Query logs less 20000 |
| `eth_accounts` | Support |  |
| `eth_sendRawTransaction` | Support |  |
| `eth_sendTransaction` | Support |  |
| `eth_signTransaction` | Support |  |
| `eth_signTypedData` | Not Support |  |
| `eth_getProof` | Not Support |  |
| `eth_mining` | Support |  |
| `eth_coinbase` | Support |  |
| `eth_hashrate` | Not Support |  |
| `eth_submitHashrate` | Not Support |  |
| `eth_getWork` | Not Support |  |
| `eth_submitWork` | Not Support |  |
| `eth_subscribe` | Support |  |
| `eth_unsubscribe` | Support |  |
| `debug_getRawReceipts` | Support |  |
| `debug_accountRange` | Planned |  |
| `debug_accountAt` | Planned |  |
| `debug_getModifiedAccountsByNumber` | Planned |  |
| `debug_getModifiedAccountsByHash` | Planned |  |
| `debug_storageRangeAt` | Planned |  |
| `debug_traceBlockByHash` | Support | `muxTracer` not support |
| `debug_traceBlockByNumber` | Support | `muxTracer` not support |
| `debug_traceTransaction` | Support | `muxTracer` not support |
| `debug_traceCall` | Support | `muxTracer` not support |
| `txpool_content` | Support |  |
| `txpool_contentFrom` | Support |  |
| `txpool_status` | Support |  |
| `eth_getHeaderByNumber` | Support | Size field contains block + header size |
| `eth_getBlockByNumber` | Support | Size field contains block + header size |
| `eth_newFinalizedHeaderFilter` | Support |  |
| `eth_getFinalizedHeader` | Support |  |
| `eth_getFinalizedBlock` | Support |  |
| `eth_getBlobSidecarByTxHash` | Planned |  |
| `eth_getBlobSidecars` | Planned |  |
| `eth_getTransactionsByBlockNumber` | Support |  |
| `eth_getTransactionDataAndReceipt` | Support |  |

### RPC health

`eth_health` returns `false` when the 75th percentile of sampled, completed RPC
method durations is at least five seconds. Sampling uses Go-BSC's exponentially
decaying reservoir (1,028 observations, alpha 0.015) and percentile interpolation.
It runs even when metrics export is disabled. An empty sample returns `true`.

HTTP, WebSocket, and IPC share the sample. Each executed batch entry contributes
separately. Successful and failed method calls are included, including health
probes after their response is computed. Subscribe/unsubscribe callbacks and
subscription lifetimes are excluded.

Pending calls do not contribute until they complete. Fast calls, including health
probes, can restore a healthy result; idle time alone does not clear old samples.
This endpoint does not measure synchronization progress, chain-head age, or
in-flight saturation, and is not a complete readiness check.

