# Validator transaction broadcast control

For a validator fed by fullnodes in a controlled cross-region topology, enable:

```sh
reth-bsc node <existing options> --bsc.disable-proactive-tx-broadcast
```

The flag is off by default and applies to all peers, including ordinary fullnodes,
independently of EVN membership. It suppresses unsolicited `Transactions` bodies,
`NewPooledTransactionHashes`, and the pool announcement on new connections. Manual
or forced propagation through `TransactionsHandle` also respects it, including
locally submitted transactions. Removing the flag and restarting restores the
existing propagation policy and EVN exclusions.

Inbound bodies, inbound hash announcements, fetching unknown transactions,
validation, pool admission, and block building remain enabled. Peers can still
request available pool transactions with `GetPooledTransactions` and receive
`PooledTransactions`. Existing ingress and propagation permissions still apply:
this flag does not override `--tx-propagation-policy none` or other restrictions.
Already mined, evicted, or otherwise unavailable transactions cannot be served.
Blocks and votes use separate propagation paths and are unaffected.

Do not substitute `--disable-tx-gossip` or `--tx-propagation-policy none`: those
also restrict transaction request responses. The new flag does not ask remote
fullnodes to stop sending transactions and does not alter the EVN handshake.

The network startup log includes `disable_proactive_tx_broadcast`. Prometheus
exposes `network_tx_proactive_broadcast_disabled` (1 when enabled) and
`network_tx_proactive_broadcast_filtered_peer_attempts_total`. The counter records
filtered peer/batch eligibility checks, not unique transactions or lost messages.
The existing EVN filter counter remains separate.

Enable only where the fullnodes provide the required transaction dissemination.
The setting also suppresses locally originated transaction gossip, so a topology
with isolated regional pools may need additional input paths. This is a traffic
control; throughput or queue-latency improvements require a separate comparison.
