# Block retrieval tuning

BSC flags are under `reth-bsc node --bsc.block-sync.*`. Durations use human-readable units (`50ms`, `2s`, `1m`), must be positive, and are capped at 24 hours. Settings are immutable for a node run; restart with new arguments to change them.

| Flag suffix | Default | Range / meaning |
| --- | --- | --- |
| `quick-timeout` | `400ms` | Deadline for the immediate bsc/2 quick request |
| `eth-fallback-delay` | `200ms` | Start independent ETH header/body requests after this delay |
| `head-timeout` | `5s` | Total lifetime of a near-head download |
| `max-head-fetches` | `32` | 1–64 concurrent head downloads |
| `eth-peer-attempts` | `3` | 1–16 distinct announcing ETH peers per head |
| `recovery-hop-count` | `4` | 1–64 ancestor blocks per request |
| `recovery-timeout` | `5s` | Per BSC peer request and per startup ETH header/body request |
| `recovery-peer-attempts` | `3` | 1–16 sequential BSC peer attempts per ancestor hop |
| `max-recoveries` | `3` | 1–8 slots shared by live ancestor recovery and active startup catch-up |
| `max-depth` | `2048` | 1–8192 ancestor blocks; also the live pipeline handoff distance |
| `recovery-idle-timeout` | `60s` | Reclaim a recovery slot after this long without completed progress |
| `near-head-distance` | `32` | 1–8192 blocks ahead using the single-block fetch path |
| `stale-distance` | `11` | 1–8192 blocks behind accepted for hash announcements and fetched results |

`eth-fallback-delay` must be below `head-timeout`; `quick-timeout` cannot exceed `head-timeout`; `near-head-distance` cannot exceed `max-depth`. Programmatic configuration validates the same limits as CLI input.

The quick request always asks for **one block**. This is part of the near-head algorithm: fetch the announced block first, and walk ancestors only if Engine reports a missing parent. It is intentionally not a batch-size knob. The fallback starts independently of the quick request. At each configured fallback-delay interval it can launch one additional untried announcer, up to eth-peer-attempts concurrent header/body attempts (default approximately 200/400/600ms). It includes BSC blob sidecars in the ETH body. Unlike geth, it requests a body immediately after that peer's header: it does not aggregate bodies for 50ms or skip the body request for a header describing an empty block. Responses must match the announced hash and body roots and pass blob availability checks.

Defaults match geth's quick-fetch count and request timers. Newly fetched blocks wait for Engine `VALID`, successful fork-choice processing, and exact parent-chain TD before full-block then hash propagation. This is more conservative than geth's early forwarding after lightweight header verification. Existing complete `NewBlock` delivery and local mining keep their propagation behavior.

All CLI-launched live and active-startup ancestor downloads use the same depth, hop count, request timeout, and recovery gate. BSC peer attempts only govern BSC range failover among known announcers. Startup discovery uses directed ETH requests and its existing rotating scan of up to 16 peers; it does not apply BSC peer failover. Its separate header-tip probe batch (128), scan interval (3s), and Engine deadline (30s) remain unchanged. The persisted-finality startup recovery's local justified-header lookback (2048) is a separate safety bound and is unaffected by network depth tuning.

Recovery idle timeout measures completed network hops and imported blocks, not total recovery duration. Shortening it below a slow hop's complete peer-attempt window can reclaim a still-running slot and permit overlapping work. There is no enforced ratio: an operator may intentionally choose a more aggressive idle heuristic. The default permits a 15-second BSC hop window with execution margin.

Head download concurrency is separate from the fixed 32 unfinished remote Engine import slots. Downloads completed while Engine import capacity is full remain retryable on later announcements. Raising head concurrency therefore does not raise Engine concurrency. Live gaps above `max-depth` go to staged backfill; gaps outside `near-head-distance` but within `max-depth` use ancestor recovery. Increasing recovery depth retains more ancestor block bodies before import.

Example:

```sh
reth-bsc node --chain bsc \
  --bsc.block-sync.quick-timeout 600ms \
  --bsc.block-sync.eth-fallback-delay 150ms \
  --bsc.block-sync.head-timeout 4s \
  --bsc.block-sync.max-head-fetches 48 \
  --bsc.block-sync.recovery-hop-count 2 \
  --bsc.block-sync.recovery-timeout 3s \
  --bsc.block-sync.max-recoveries 2
```

Library callers that need shared admission must pass the same `Arc<BlockSyncConfig>` to `BscNode::with_block_sync` and `startup_sync::run_with_config`. The standalone `startup_sync::run` compatibility entrypoint creates its own default gate.
