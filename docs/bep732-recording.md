# BEP-732 consensus and opt-in early child construction

This experimental v2.8 branch includes optional advisory BAL prefetching, custom-chain
BEP-732 recording/commitment enforcement, negotiated full-BAL transport, recent
eth/71 retrieval, and opt-in construction of a child while its parent is executing.
Named BSC networks remain unscheduled. Early construction is disabled by default.
The sections below preserve each milestone's behavior and validation history;
the early-construction section describes the current implementation.

Cargo dependencies pin companion `bnb-chain/reth` commit
`1f95f85f1d235f783a41c1b90c5c9eeb6af92ab9` (`feat/bep732-v2.8`).
No sibling checkout is required. Earlier local tests used the identical companion
source through path patches.

The design follows the recording section of the
[BEP-732 draft](https://raw.githubusercontent.com/flywukong/BEPs/bal-mev-bep/BEPs/BEP-732.md),
which references EIP-7928 revision
[`d4c5755`](https://github.com/ethereum/EIPs/blob/d4c575521d99ec881a444ab3af604e684b5f0743/EIPS/eip-7928.md).
Activation must be checked against the final agreed specification.

## Implemented

`src/node/evm/bal.rs` owns the recording index independently of the transaction
body iterator. It starts at zero, advances after pre-execution and after each
committed user or system transaction, and flushes remaining finalization writes
at the final index. Encountering a deferred system transaction does not advance
it. The index is checked for uint32 overflow.

Commits sharing an index are combined using the first original account and slot
values and the last resulting values. This handles direct protocol credits followed
by system-transaction debits, including net-zero balance and storage changes.
Read-only slots survive coalescing. Canonical sorting and conversion use revm's
existing BAL implementation.

Contract upgrades, CAS20 registry seeding, the history-storage system call,
fee-distribution balance writes, and committed transactions now pass through the
common recording path. Uncommitted protocol queries and omitted slash attempts
do not contribute. Discarded packing candidates do not advance an index or add
their accesses. Reverted included transactions retain accessed storage slots.

The custom block builder enables this recorder under the existing optional BAL
flag. ConfigureEvm's executor constructors also support it when recording is
requested by the supplied State or header. The completed recorder publishes back
to State for existing consumers. Normal block-header validation still rejects BAL
commitments, and the builder still strips values and indices before publishing
the existing advisory `bsc-bal/1` hint. Execution remains sequential.

## Validation

Using Rust 1.99.0:

```sh
cargo +1.99.0 test --offline -p reth_bsc --lib \
  --target-dir ../reth-bsc-bal-v2.8/target -- --test-threads=1
cargo +1.99.0 build --offline -p reth_bsc --bin reth-bsc \
  --target-dir ../reth-bsc-bal-v2.8/target
```

The library suite passed: **792 passed, 0 failed, 1 existing ignored test**.
Seven new tests cover recording indices, net-zero changes, repeated writes and
preserved reads, overflow, actual deferred system execution with an omitted slash,
discarded candidates, and reverted storage writes. The first parallel sandboxed
run had three vote-pool assertion failures and four localhost socket permission
failures; the complete serial run with socket access passed.

The production binary built successfully. Existing dependency macro and
future-compatibility warnings remain. Rustfmt and `git diff --check` passed.

Local-cluster evidence is retained under
`../bep732-recording-cluster-2026-11-13/`. The binary SHA-256 is
`733517a96d5d2d7854d023abac671cf1ed05ee4f2fc8c18c02ae009b3d53d3d2`.
Its exact tracked patch and new recorder source are retained with provenance;
the build preceded the implementation commit. The original advisory binary was
preserved and restored after building.

The fresh chain-714 cluster used three Reth validators (BAL enabled on nodes
0/1, disabled on node 2) and a BSC Geth 1.8.1 observer. Forks through Jenner
were exercised. The reusable `reth-bsc-local-cluster-tests` skill ran these groups:

| Group | Result |
| --- | --- |
| BEP-675 | 15/15 admission cases and 3/3 real-bid cases, including invalid-bid revocation, canonical inclusion, competing bids, and permission recovery. |
| BNB | Exact transfer balances and gas accounting across clients. |
| ERC20 | Deployment, transfers, approvals, transferFrom, and insufficient-balance/allowance rollback. |
| CAS20 | Asset/stablecoin activation and creation, role checks, transfers/allowances, rejection cases, and existing-token use after creation is disabled. |
| Uniswap V2 | ERC20 and CAS20 pools, liquidity, swaps in both directions, exact quotes, slippage/deadline rejection, and liquidity removal. |

The native/token/DEX workload passed **36 checks across 67 transactions**:
48 successes and 19 expected reverts. Every workload receipt and log matched
across all four clients. BEP-675 produced canonical tagged blocks 332, 333, and
368. The final comparison covered **369 blocks, 0 through 368**, with matching
hashes, state/receipt/transaction roots, and transaction lists. At block 368:

```text
hash:      0x554e574e37b6657ee9f1ca57fffb73fc258bdf44d400a17ea4f64a91af691682
stateRoot: 0x9a8bbd5e3f5826dac79c2df376e80d374d1c9a49d82e54934bf593c3c3206b05
```

Node 0 sent 127 hints and received/used 132; node 1 sent 132 and received/used
127. Both logged actual prewarming. The disabled validator had no BAL counters
or prewarming traces. Zero-simulation BidBlock cases still exercise compatibility
and ordinary execution, because they do not yet carry a full consensus BAL.
All four owned processes stopped successfully; original worktrees and chain data
were preserved. See `results/run-status.json`, `functions-report.json`,
`bep675-report.json`, `consensus-report.json`, `log-evidence.json`, and
`provenance.json` in the artifact directory.

The deliberate invalid-nonce bid produced import rejection and aborted-receipt-task
errors, and the underpriced bid produced a permission-revocation error. These
belong to the expected negative cases. The final comparison ran after those cases
settled. Malformed consensus BAL tests remain pending consensus activation;
this run does not claim that coverage.

## Post-execution commitment verification — 2026-11-14

The BSC post-execution consensus validator now checks the hash rebuilt by execution
against the header's BAL commitment. A mismatch returns `BlockAccessListHashMismatch`;
a committed header without a rebuilt hash returns `BlockAccessListHashMissing`;
an execution hash without a header commitment returns `BlockAccessListHashUnexpected`.
Matching hashes and legacy blocks without either hash pass this check.

Both the engine's live-import path and the staged execution path calculate this
argument from the execution-built BAL. It is not taken from an incoming attachment.
The earlier header guard still rejects BAL commitments: this verification change
does not activate the fork or change the Parlia signing preimage.

The focused post-execution tests passed, including all five presence/matching cases
and the existing requests-hash regression. The complete library suite then passed
serially with localhost socket access: **793 passed, 0 failed, 1 ignored**.
Logs are retained in `../bep732-commitment-2026-11-14/`. The cluster results above
belong to the recording increment; the cluster was not rerun for this change.

## Custom-chain activation and signing — 2026-11-14

An explicit genesis `config.bep732Time` enables the prototype. The parser requires
unsigned timestamps for that setting and explicit Jenner, Prague, and Bohr times
at or before it. Named networks have no BEP-732 schedule. Execution still uses
BSC's Osaka rules; Ethereum Amsterdam is not activated.

Before activation, headers cannot carry a BAL commitment. After activation they
must carry one, regardless of `--bal.enabled`. Local and simulated-bid builders
record the full BAL through finalization, validate its structure and gas-derived
item limit, and commit its hash in the header. Parlia signs that hash after the
requests hash. The activated double-sign evidence precompile verifies the extended
seal for BAL-bearing headers and continues to accept legacy evidence. Import
rebuilds the BAL sequentially and checks the commitment. The Ethereum parallel
BAL executor is disabled because it does not implement deferred BSC system transactions.

BidBlock JSON adds optional `blockAccessList`; RLP adds an optional final byte
string. Existing three-field encodings are unchanged. After activation the
attachment is mandatory, including the `0xc0` encoding for an empty BAL. Admission
checks the raw hash, exact RLP, canonical order and indices, and gas-derived item
limit before sealing. Attachment errors do not revoke builder permission; actual
execution failures use the existing invalid-block evidence and permission path.
The local debug builder returns the full attachment for activated blocks.

The separate `bsc-bal/1` advisory protocol continues to send only read hints,
stripping all values and indices from full BALs. Missing hints cannot disable
consensus recording or validation. Full attachments are not yet retained or
forwarded in the network block protocols; receivers rebuild them by execution.

The final full serial library run passed **799 tests, 0 failed, 1 ignored**.
Tests cover activation boundaries, legacy and extended seal verification, header
presence, optional BidBlock wire encoding, missing/incorrect/undecodable BALs,
duplicate/out-of-order accounts, out-of-range indices, the exact gas-item boundary,
and full-BAL-to-advisory conversion. The binary built with Rust 1.99.0; existing
dependency warnings and the macOS large-unwind-section linker warning remain.

Local results are in `../bep732-activation-cluster-2026-11-14-r2/results/`.
Three Reth validators crossed activation at block **169**. Two had advisory BAL
enabled and one disabled. All three produced activated blocks. A dedicated check
verified BAL absence/presence and equal hashes through block 212. The final
canonical comparison covered **329 blocks (0–328)** across all three validators,
including every workload receipt and log. No Geth interoperability is claimed:
the installed Geth binary does not implement the activated rules.

BNB, ERC20, CAS20, and Uniswap V2 passed **36 checks / 67 transactions** (48 successes,
19 intentional reverts). The adapted local BidBlock driver passed **7/7** cases:

- Missing attachment, hash mismatch, and undecodable bytes were rejected before
  sealing without revoking permission.
- A structurally valid empty BAL with a matching header commitment, but incorrect
  for the actual execution, passed admission and failed import at height 323 with
  `BlockAccessListHashMismatch`. Its transaction was excluded and permission revoked.
- After permission recovery, a valid bid became canonical at 324; competing bids
  selected one winner at 325; an underpriced bid became canonical at 328 and
  triggered the existing fee-floor revocation. Permission was restored afterward.

This run exercised the real BidBlock scenarios and four new BAL negatives; it did
not rerun the older 15-case synthetic admission suite or the transaction-list
SendBid path. The only error-level node logs were the intentional false-commitment
and fee-floor revocations. Advisory nodes exchanged and used hints; the disabled
node had no hint metrics. All owned processes stopped and the original advisory
binary was restored. Build patch, binary hashes, test logs, and driver source are
retained with provenance. Subsequent source changes were tests, comments, and docs.

The first attempt, retained in `../bep732-activation-cluster-2026-11-14/`, failed
before activation because the inherited harness passed `67108864` to a cache
setting measured in MiB. The fresh rerun corrected it to `64` MiB. No production
code change was required for that harness failure.

## Negotiated full-BAL transport — 2026-11-14

Local and SendBid payloads now retain the full BAL separately from advisory read
hints. BidBlock tasks preserve the submitted attachment through system-transaction
signing and the delayed import queue; broadcast does not depend on a cache lookup.
After sealing, the full bytes accompany `NewBlockPacket`; retained bytes
also accompany each block in negotiated `BlocksByRange` responses. Canonical block
and body types are unchanged.

The draft does not assign an exact handshake extension encoding. This prototype
puts the RLP byte string `bep732-bal/1` in the existing BSC capability packet's
opaque `extra` field. A matching negotiated BSC version and exact marker enable
attachments; missing, legacy, malformed, and unknown extensions leave them off.
Connections do not wait for the marker before processing ordinary votes/blocks.
Reconnection resets support, and closed registry channels cannot authorize it.
This format requires cross-client agreement before production interoperability.

A default no-op `NewBlockPayload::for_peer` hook in the companion Reth active
session removes attachments for unsupported destinations, including ordinary
broadcasts and EVN/proxy sends. Unchanged payloads retain their shared allocation.
The optional read-hint flag does not control this consensus attachment capability.

Receivers check the attachment commitment, canonical RLP, ordering, indices, and
gas-derived item limit before retaining bytes. A bad attachment is discarded while
the block continues through sequential verification. This fallback covers invalid
BAL bytes inside a valid packet envelope; malformed outer RLP remains a packet
decode error. A commitment match alone never marks execution valid. Retention is keyed by exact block hash, including
forks, and bounded to 128 entries / 128 MiB with oldest-first eviction. Range
attachments have an 8 MiB aggregate soft limit and must fit remaining frame space;
new-block attachments are omitted if they would exceed the frame limit. These are
local transport/cache limits, not additional block validity rules.

At this transport milestone, the cache was in memory only. It did not yet persist
historical BALs, retain every
execution-rebuilt BAL from attachment-free imports, or fetch missing BALs over
eth/71. That milestone deliberately restricted Ethereum protocol negotiation to
eth/68 because BSC's newer status handshake differs; enabling eth/71 requires
integrating and validating that compatibility layer. Existing BSC work at
`f19020cd0` and `6ad8a1bb9` was a candidate for reuse at that point.
Missing/evicted attachments currently fall back to sequential execution.

### Transport validation

The initial transport binaries completed two three-validator runs. Nodes 0/1
negotiated attachments; node 1 had advisory hints disabled. Node 2 used the prior
activation-only binary, enforcing the same commitment rules without recognizing
attachments. Both runs passed all five scenario groups: 36 functional assertions,
67 transactions (48 successful and 19 intentional reverts), and seven BEP-675
cases. The normal run compared 288 canonical blocks through height 287. The fault
run compared 329 through height 328.

In the fault run, only node 0's test binary replaced outgoing attachment bytes
with `0x80`, keeping the sealed block unchanged. The unmodified node 1 counted
59 invalid attachments and continued in consensus. It also received valid
attachments; node 2 continued without attachments. This tests attachment fallback,
not acceptance of an invalid header commitment. The bid suite separately rejects
a commitment inconsistent with execution and checks the permission lifecycle.

Initial evidence is preserved under `results-initial/` in
`../bep732-transport-cluster-2026-11-14/` and
`../bep732-transport-fault-cluster-2026-11-14/`. Range attachment codec/fallback is
unit-tested; these runs do not establish live range attachment delivery or
cross-client BEP-732 interoperability. The companion Reth active-session suite
passed 20 tests. The transport hook is committed in companion Reth at
`a3c7bbd07f`.

The final direct-queue revision passed 806 library tests (one ignored). A restricted
run first hit four localhost bind denials; the full rerun with socket access passed.
The final normal binary then reopened the existing database, passed all seven bid
cases again, and verified 377 canonical blocks through height 376 plus all 67 prior
workload receipts. It exchanged full attachments with advisory hints disabled on
node 1 and retained legacy fallback for node 2. Its only two error-level log entries
were the expected false-commitment and underpriced-bid revocations. Final restart
evidence is in each cluster directory's `results/`; the functional workload was not
resubmitted on restart.

The final fault binary also reopened the database and passed all seven bid cases.
The unmodified receiver counted 33 corrupted attachments, all three nodes agreed
on 425 canonical blocks through height 424, and all 67 prior workload receipts
remained canonical. Its only error-level entries were the same two intentional
bid rejections. Both clusters were stopped, and the shared target's original
advisory binary was restored. Fault injection exists only in the detached test
worktree and artifact binary, not in the implementation commits.

## Missing-BAL retrieval and executed retention — 2026-11-16

The network now negotiates BSC eth/70 and eth/71, retaining eth/68 fallback. It
reuses the BSC status implementation from `f19020cd0` and the database-head status
fix from `6ad8a1bb9`; unlike that earlier release policy, this branch still accepts
eth/68 peers. Newer BSC status retains Parlia total difficulty and skips legacy
UpgradeStatus. Status, fork filtering, head hash, and total difficulty initialize
from the same database head. Malformed or incompatible status fails the handshake.

For a recently announced block whose committed BAL is missing or unusable, the
import service attempts one eth/71 request to the announcing peer. There are at
most eight concurrent requests, each with a two-second deadline including peer
lookup. Unsupported peers, missing data, disconnects, invalid responses, and
capacity exhaustion leave sequential execution running independently. Service
shutdown drops the fetch futures. Responses must contain exactly one matching,
structurally valid BAL (empty/missing response entries mean unavailable).

The companion engine now preserves its rebuilt BAL even when the incoming block
has no attachment. Only successfully validated execution reaches the existing
provider BAL store. Local executed payloads also enter that store; unexecuted
BidBlock/network attachments remain in the separate bounded cache. Canonical
persistence flushes executed BALs to RocksDB and applies the existing retention
policy. The attachment lookup falls back to this provider store after cache misses,
and the existing eth/71 handler serves the same store. No unverified attachment
is promoted to durable execution evidence.

The companion engine retention fix is committed at `1f95f85f1d`. The full BSC
library suite passed 815 tests (one ignored); the companion engine BAL subset
passed 36 tests, including validated-store insertion, flush, and pruning.

Local test evidence is under `../bep732-retrieval-cluster-2026-11-16/`.
The first attempt stopped before workload submission because its temporary npm
cache had been cleaned. Dependencies were pinned and installed in the owned test
directory. The next attempt passed 36 functional checks, 67 transactions and all
seven bid cases, then stopped at metrics collection because its copied metrics
URLs used the previous cluster ports. Those reports are preserved in
`results-workload/`; the corrected verification reuses their canonical receipts
rather than rerunning CAS20 initialization. These were harness failures, with no
production-source changes needed for either correction.

The corrected verification agreed across 430 canonical blocks through height 429
and rechecked all 67 workload receipts. All seven bid cases passed again. The
unmodified receiver (advisory hints disabled) sent 18 eth/71 requests: 14 returned
valid BALs and four returned unavailable, with normal execution continuing. Its
eth/68 fallback path was also exercised; the attachment-omitting producer counted
17 omitted attachments. The negotiated versions were recorded in
`results/retrieval-report.json`.

The immediate restart preflight initially encountered recently closed ports after
all owned processes had stopped; the harness now uses `SO_REUSEADDR` for its bind
check. This change only affects test orchestration.

The follow-up restart passed: the nodes agreed at height 447, and both new nodes
returned the same 69-byte BAL for historical block 172 before and after restart,
matching the block's commitment. The raw-BAL RPC can rebuild on a store miss, so
this verifies restart availability, not a disk-only read; storage insertion,
flushing and pruning are also covered by the engine tests. All owned nodes were
stopped and the original shared-target advisory binary restored. The aggregate
result is `results/final-status.json`; earlier failed harness stages remain intact.

This is a recent-announcement fetch path, not a complete historical downloader:
peer failover, retry scheduling, and historical synchronization requests remain
future work. Fetch completion does not authorize speculative state use or replace
execution verification. The separate attachment handshake marker remains a
prototype requiring cross-client agreement.

## BAL-derived parent state rollout

`BSC_BEP732_CANDIDATE_STATE=true` opts the local transaction builder into reading
parent N from a hash-pinned N−1 provider plus N's retained full BAL. The resulting
account/storage trie root must match N's header before the view is used. A missing
attachment, provider failure, or derivation mismatch falls back to executed N.
The default remains off; this is a correctness rollout, not a performance mode.

The immutable view merges only changed account fields, uses the final indexed
storage writes (including explicit zeroes), preserves read-only account existence,
and prevents deleted accounts from exposing old storage. Changed bytecode is
checked before decoding. The view supplies N's hash for `BLOCKHASH(N)`; older
hashes still come from the base provider. Each child has its own mutable revm
state. Cached reads are reset and sparse-trie background computation is disabled
for this rollout so neither can mask errors in the new parent view. Child roots
are finalized against the imported N provider.

This increment still starts work after N is imported. It does **not** implement
construction overlap, announce an unvalidated candidate as canonical, or change
sealing authorization. Early-announcement scheduling, branch-scoped snapshot and
ancestor handling, parent-validation cancellation, and the pre-signing gate remain
necessary before enabling speculative N+1 construction. Empty fallback and bid
simulation continue using their existing executed-parent paths.

Seven unit tests cover sparse final writes, storage-only updates, code creation,
read-only/absent accounts, deletion, branch-parent hashes, disabled delegation,
and child/sibling isolation. The full library suite passed 822 tests with one
ignored when run serially. The initial concurrent run failed eight existing
shared vote-pool tests; its log is retained alongside the passing serial run.

Local-cluster evidence is under `../bep732-candidate-cluster-2026-11-16/`.
One candidate-enabled validator and two baseline validators passed 36 functional
checks across 67 transactions and all seven bid cases, including malformed BAL
and false execution commitments. The nodes agreed across 328 canonical blocks
through height 327. Candidate metrics recorded 186 successful root-checked
derivations, 105 missing-attachment fallbacks, and no derivation-error fallbacks;
the baseline nodes recorded no candidate-state activity. The initial metrics
assertion omitted Reth's `reth_` prefix and failed after the workloads passed.
The parser was fixed and the saved metrics rechecked without changing source or
rerunning transactions. `results/final-status.json` records the aggregate result;
the original harness failure is preserved. All owned nodes were stopped and the
shared-target baseline binary restored. This does not test construction overlap
or pre-import invalid-parent cancellation.

## Early child construction

`BSC_BEP732_PARALLEL=true` enables early construction for received NewBlock
parents with a retained full BAL. It is independent of the earlier
`BSC_BEP732_CANDIDATE_STATE` rollout switch and remains off by default.

The import service publishes a bounded early-parent notification before awaiting
`new_payload`. The miner accepts only a direct extension of the current canonical
head, checks header/timing rules and the hash-specific Parlia snapshot, and limits
attempts to one candidate per canonical base. Checkpoints, epoch handovers,
breathe boundaries and fork transitions use ordinary sequential construction.
The BAL-derived root must match the parent header; account/storage reads and the
last 256 block hashes are isolated from child writes and later canonical changes.

User-transaction execution can proceed while parent validation is in flight.
System finalization and child root calculation wait for successful independent
execution and fork choice. A live early job is retained when its exact parent
becomes canonical. Before block finalization/signing, the parent must still be the
canonical tip, the import guard must report success, and validator eligibility is
checked against the current snapshot. Invalid/cancelled imports, competing tips,
job cancellation and bounded deadlines discard speculative work. A 50-ms status
check resumes ordinary canonical work after failure rather than waiting for the
normal recovery ticker; the pending-parent wait has a two-second ceiling.

The early snapshot remains in the existing hash-keyed snapshot cache, with
checkpoint/validator transitions excluded; it is never installed as canonical
execution state. Missing attachments and historical synchronization use the
sequential path. Local zero-simulation BidBlock verification does not yet publish
an early-parent notification, although peers receiving its NewBlock can use this
path. This is user-transaction construction overlap, not speculative system
finalization or parallel transaction execution within one block.

Metrics: `reth_bsc_parallel_parent_total` records queued, cancelled, rejected and
signing-rejected work; `reth_bsc_parallel_build_total` distinguishes builds reaching
the derived state before/after parent import; `reth_bsc_parallel_transaction_total`
counts child transactions executed before parent import completes. These counters
are correctness evidence, not throughput or latency benchmarks.

### Early-construction validation

The full library suite passes 830 tests with one ignored. Added cases cover
pending/imported/rejected parent lifecycles, exact-tip and reorg checks, deadline
and explicit cancellation, invalidation during an active wait, pinned ancestor
hashes, retaining a live job on parent import, and releasing unselected BALs.
The import guard holds a weak reference, so queued engine import futures do not
pin evicted attachments outside the bounded early-parent queue.

The normal and delayed local clusters each passed the five scenario groups:
36 functional assertions, 67 workload transactions, seven adversarial/valid bid
cases, and a separate 600-transfer BNB burst with receipts compared on all nodes.
Artifacts are in `../bep732-parallel-cluster-2026-11-16/` and
`../bep732-parallel-delay-cluster-2026-11-16/`. Initial runs and follow-up runs are
preserved separately; source patches and binary hashes identify each revision.

The undelayed run did **not** demonstrate overlap: parent execution completed
before the child reached its derived state. Its initial overlap assertion failed;
subsequent verification explicitly distinguishes compatibility from overlap.
This is not evidence of a speedup on small blocks.

A separate test checkout delays the parent executor's finish by 200 ms. Its
follow-up run recorded 213 builds reaching the derived state before import and
299 child transaction execution attempts before import, with 752 canonical blocks
matching through height 751. Timestamp correlation across both delayed runs found
child transactions executing inside parent executor delay intervals for 215
distinct parents. Rebuild attempts can execute the same transaction repeatedly;
these counts are not counts of distinct canonical transactions. Delay hooks and
extra event instrumentation are not part of the implementation branch.

The live negative driver broadcasts a correctly committed BAL/state root with an
incorrect receipt root from a sequential validator. The parallel receiver starts
the child, cancels it without permitting signing, and rejects the parent. The
recorded child hit its deadline before the parent rejection arrived; rejection
interrupting an active wait is separately unit-tested. Same-height and base reorg
cancellation are unit-tested; the follow-up below adds a deterministic live
canonical-reorg test. The final weak-reference ownership change is covered by
the full suite and a follow-up normal-cluster smoke run; the instrumented delayed
binary predates only that ownership change.

## Live canonical reorg validation — 2026-11-16

A separate test checkout based on `02ed9bd24` passed a controlled three-Reth-validator
reorg test. At height 358, a valid out-of-turn parent A became canonical while
node0's BAL-derived child waited immediately before the signing gate. A valid
in-turn sibling B then replaced A through normal validation and total-difficulty
fork choice. Releasing the barrier produced `ready=false`, `eligible=true`, and
`cancelled=false`: canonical-parent invalidation caused rejection. No validator
signed a child of A. Node0 produced block 359 on B, and all three validators
matched canonical hashes, state/receipt/transaction roots and transactions at 367.

Only the isolated test checkout contains the 200-ms parent-execution delay,
pre-sign barrier, ordinary-work freeze, signed-candidate/import RPC helpers and
signature instrumentation. Consensus validation and fork choice were unchanged.
The test used empty user-transaction candidates with normal system finalization;
it did not rerun the five transaction scenario groups or measure performance.
Geth 1.8.1 initialized genesis only; this is not cross-client BEP-732 coverage.

Evidence, exact source patch, binary hashes, and prior harness failures are retained
in `../bep732-reorg-cluster-2026-11-16/`, especially `results/final-status.json`
and `results/reorg-events.json`. Earlier attempts corrected header JSON names,
placement of the timing pause, and broadcast timing. All owned processes stopped.
The implementation branch required no production-code fix for this test.

## Remaining BEP-732 implementation

1. Complete EIP-7928 conformance and differential vectors, including access-list
   warming, out-of-gas boundaries, code/nonce changes, selfdestruct, CAS20, and
   upgrade/epoch/breathe boundaries. Measure recording overhead before making it
   mandatory.
2. Complete activation and production-path integration coverage, including the
   regular transaction-list SendBid path and cross-client consensus vectors.
3. Broaden BidBlock adversarial transport coverage and signed-system-transaction
   differential vectors; HTTP permission lifecycle is covered by the run above.
4. Agree the attachment handshake wire format with a reference client and complete
   historical eth/71 downloader integration, peer failover, and retries. Recent
   announcement retrieval and executed-BAL persistence are implemented above;
   unexecuted attachments retain their separate bounded memory cache.
5. Broaden the opt-in early construction path to transition blocks and local
   BidBlock verification, and measure resource/latency costs under representative
   workloads. Preserve independent parent validation and pre-signing gates.

The earlier Geth local-cluster compatibility checks exercise existing BSC rules and
advisory hints. They cannot establish BEP-732 interoperability until a reference
client implements the same activated rules. This increment makes no throughput
or latency claim.
