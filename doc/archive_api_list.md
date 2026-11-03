# reth-bsc archive api list

⚠️ **Error format differs from BSC/Erigon**

⚠️ **`pending` tag is planned, temporarily not supported**

For `eth_getBlockByNumber`, `eth_getHeaderByNumber`, `eth_getTransactionsByBlockNumber`, and
`eth_getBlobSidecars`, `earliest` selects genesis (block `0x0`), even after history pruning.
It has the same data availability and error behavior as an explicit `0x0` request.

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
| `eth_newFinalizedHeaderFilter` | Support |  |
| `eth_getFinalizedHeader` | Support |  |
| `eth_getFinalizedBlock` | Support |  |
| `eth_getBlobSidecarByTxHash` | Planned |  |
| `eth_getBlobSidecars` | Planned |  |
| `eth_getTransactionsByBlockNumber` | Support |  |
| `eth_getTransactionDataAndReceipt` | Support |  |

