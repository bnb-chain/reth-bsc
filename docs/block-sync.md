# Block retrieval tuning

BSC flags are under `reth-bsc node --bsc.block-sync.*`. Durations use human-readable units (`50ms`, `2s`, `1m`), must be positive, and are capped at 24 hours. Settings are immutable for a node run; restart with new arguments to change them.

| Flag suffix | Default | Range / meaning |
| --- | --- | --- |
| `quick-timeout` | `400ms` | Deadline for the immediate bsc/2 quick request |
| `eth-fallback-delay` | `200ms` | Start independent ETH header/body requests after this delay |
| `head-timeout` | `5s` | Total lifetime of a near-head download |
| `max-head-fetches` | `32` | 1–64 shared single-block downloads, including near-head parents |
| `eth-peer-attempts` | `3` | 1–16 distinct announcing ETH peers per head |
| `recovery-hop-count` | `4` | 1–64 blocks per deep BSC range / startup ancestor request; near-head parents use one block |
| `recovery-timeout` | `5s` | Per BSC peer request and per startup ETH header/body request |
| `recovery-fallback-delay` | `200ms` | Launch another eligible BSC peer while a deep ancestor range is pending |
| `recovery-peer-attempts` | `3` | 1–16 distinct BSC peers per ancestor hop, including concurrent hedges |
| `max-recoveries` | `3` | 1–8 slots shared by live ancestor recovery and active startup catch-up |
| `max-depth` | `2048` | 1–8192 ancestor blocks; also the live pipeline handoff distance |
| `recovery-idle-timeout` | `60s` | Reclaim a recovery slot after this long without completed progress |
| `near-head-distance` | `32` | 1–8192 blocks ahead using the single-block fetch path |
| `stale-distance` | `11` | 1–8192 blocks behind accepted for hash announcements and fetched results |

`eth-fallback-delay` must be below `head-timeout`; `quick-timeout` cannot exceed `head-timeout`; `near-head-distance` cannot exceed `max-depth`. Programmatic configuration validates the same limits as CLI input.

The quick request always asks for **one block**. This is part of the near-head algorithm: fetch the announced block first, and walk ancestors only if Engine reports a missing parent. It is intentionally not a batch-size knob. The fallback starts independently of the quick request. At each configured fallback-delay interval it can launch one additional untried announcer, up to eth-peer-attempts concurrent header/body attempts (default approximately 200/400/600ms). It includes BSC blob sidecars in the ETH body. Unlike geth, it requests a body immediately after that peer's header: it does not aggregate bodies for 50ms or skip the body request for a header describing an empty block. Responses must match the announced hash and body roots and pass blob availability checks.

Defaults match geth's quick-fetch count and request timers. Newly fetched blocks wait for Engine `VALID`, successful fork-choice processing, and exact parent-chain TD before full-block then hash propagation. This is more conservative than geth's early forwarding after lightweight header verification. Existing complete `NewBlock` delivery and local mining keep their propagation behavior.

Live and active-startup recovery share the depth limit and recovery gate. Near-head missing-parent cursors use the same per-node single-block downloader as hash announcements, with quick-timeout, eth-fallback-delay, eth-peer-attempts and head-timeout. Its pool coalesces subscribers by hash and shares max-head-fetches capacity across both entrances. The pool holds weak references: completed, failed and canceled downloads release their slots independently of ancestor execution and fork choice. A valid complete NewBlock delivery can satisfy all current subscribers after hash, body-root and blob checks, canceling outstanding downloads; receiving bytes alone never establishes Engine VALID. The child sender is a safe ETH header/body hint even if it only speaks bsc/1; BSC requests for that parent require an active announcer of the exact parent hash. Receiving a child alone does not establish that its sender can serve a BSC range start. Both entrances use identical hash, height, body-root and blob checks. Canceling a hash-announcement subscriber preserves a download still needed by parent recovery; canceling the last subscriber drops its network waiters.

When the cursor walks below the near-head window (local tip minus near-head-distance at admission), it uses BSC ranges with recovery-hop-count, recovery-timeout and recovery-peer-attempts. Thus changing recovery-hop-count continues to tune deep batching and startup, while near-head gaps remain single-block requests. BSC peer attempts only govern deep BSC range failover among known announcers. The hash single-flight guarantee covers the two near-head single-block entrances; deep range walks remain separately bounded by the recovery gate and can overlap single-block requests or other walks. They are not claimed to be globally deduplicated. Startup discovery uses directed ETH requests and its existing rotating scan of up to 16 peers; it does not apply BSC peer failover. Its separate header-tip probe batch (128), scan interval (3s), and Engine deadline (30s) remain unchanged. The persisted-finality startup recovery's local justified-header lookback (2048) is a separate safety bound and is unaffected by network depth tuning.

Recovery idle timeout measures completed network hops and imported blocks, not total recovery duration. Shortening it below a slow hop's complete peer-attempt window can reclaim a still-running slot and permit overlapping work. There is no enforced ratio: an operator may intentionally choose a more aggressive idle heuristic. The default permits a 5.4-second BSC hop window with execution margin.

Deep BSC ancestor requests use the walked peer and live bsc/2 announcers of the exact cursor hash. Candidates are re-read every 25ms; no arbitrary bsc/2 peer is queried. A slow request can be hedged after `recovery-fallback-delay`; an invalid, empty, or failed response permits immediate failover. Every response must match the cursor height/hash and the complete backward parent links, fit the requested count, and pass data availability checks before winning. There are at most `recovery-peer-attempts` requests per hop, all losers are canceled, and the hop has an upper network deadline of `recovery-timeout + recovery-fallback-delay * (recovery-peer-attempts - 1)` (5.4s by default). This is distinct from the 5s total head-download deadline. A multi-hop recovery has no fixed total deadline: depth, hop size, per-hop deadlines and the shared idle gate bound it. The idle timeout is a silence heuristic, not a whole-recovery timer.

While ancestor requests wait, discovery checks local canonical progress every 50ms and once more before accepting an error. Side-chain headers alone do not establish executed ancestry; their bodies still go through Engine. For complete target payloads, the import task also observes parent-header availability and submits at most one target execution recheck at a time. This can finish recovery after another entrance executes the missing parent, without waiting for the stale range request. Only target `VALID` allows the existing BSC fork-choice rules to evaluate it; a better current head or finality rule can retain the current chain. Network failure also gets a final target recheck. Completion and cancellation release the task's queue entry, single-flight guard and recovery permit; late range responses cannot restart the task. A near-head parent can be recovered through ETH even with no eligible BSC source.

Slow head downloads (at least 500ms) log number/hash, first announcer, winner, quick/ETH route, time since local hash admission, fetch time and actual fallback-start offset. Slow ETH responses additionally log header/body durations. Failed head downloads log their deadline and last error. Slow remote payload validation separates import queue and Engine time. Ancestor logs identify hedge winners and local-completion cancellation without logging each local poll. These observations can distinguish late hash ingress from slow downloading or Engine processing in future runs; historical logs without route/winner fields cannot establish which peer won.

The Geth comparison is limited: its fetcher forwards after header verification and announces hashes after insertion. A missing-parent block is forgotten and requeued after 200ms while the independent chain sync path runs; near-head chain sync may itself wait 2s. Reth's bounded ancestor walk and target Engine revalidation are different mechanisms, not a claim of identical fetching or timing behavior. This change affects retrieval/fork-choice scheduling, not execution gas or error validation rules.

Shared single-block download concurrency is separate from the fixed 32 unfinished remote Engine import slots. Downloads completed while Engine import capacity is full remain retryable on later announcements. Raising head concurrency therefore does not raise Engine concurrency. Live gaps above `max-depth` go to staged backfill; gaps outside `near-head-distance` but within `max-depth` use ancestor recovery. Increasing recovery depth retains more ancestor block bodies before import.

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
