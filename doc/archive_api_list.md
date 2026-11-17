# reth-bsc archive api list

⚠️ **Error format differs from BSC/Erigon**

⚠️ **`pending` tag is planned, temporarily not supported**

| API | Status | Notes |
|----------|------|------|
| `eth_blockNumber` | Support |  |
| `eth_chainID` | Support |  |
| `eth_syncing` | Support |  |
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
| `eth_newFinalizedHeaderFilter` | Support | Poll finalized header hashes with `eth_getFilterChanges`; uninstall with `eth_uninstallFilter`. |
| `eth_getFinalizedHeader` | Support |  |
| `eth_getFinalizedBlock` | Support |  |
| `eth_getBlobSidecarByTxHash` | Planned |  |
| `eth_getBlobSidecars` | Planned |  |
| `eth_getTransactionsByBlockNumber` | Support |  |
| `eth_getTransactionDataAndReceipt` | Support |  |

Finalized-header filters return hashes from finality updates received after filter creation,
in event order. `eth_getFilterChanges` drains the queued hashes and returns `[]` when no
updates are available; every successful poll refreshes the filter's configured inactivity
timeout. The event buffer retains 4,096 updates. A filter that falls behind this buffer is
invalidated and returns `filter not found`, so clients must create a new filter and reconcile
their last observed finalized head. No hashes are synthesized for heights skipped by a
finality update.

