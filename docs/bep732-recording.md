# BEP-732 recording foundation

This is the first implementation increment on `feat/bep732-v2.8`, based on
`feat/bal-v2.8` at `fad0ff89b448d7ef1acf35e2a0fa6e842264f63e`. It implements
BSC recording boundaries; it does **not** activate BEP-732 consensus or
speculative block production.

The companion worktree is `../reth-bep732-v2.8`, also on `feat/bep732-v2.8`,
at `9278c0cddbd86dc9c85ee0b8a2d530eb037faaae`. This increment makes no Rust
changes in that companion. Cargo's existing local Reth patches now point to it.

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

## Remaining BEP-732 implementation

1. Complete EIP-7928 conformance and differential vectors, including access-list
   warming, out-of-gas boundaries, code/nonce changes, selfdestruct, CAS20, and
   upgrade/epoch/breathe boundaries. Measure recording overhead before making it
   mandatory.
2. Add fork activation, header commitment generation and independent execution
   comparison, extended Parlia seal encoding, and compatible `0x68` evidence
   verification. Keep verification sequential until the engine's BAL parallel
   path supports BSC's deferred system transactions. Add fork-boundary and
   malformed-commitment tests.
3. Extend BidBlock RLP/JSON, validate canonical BAL data before sealing, preserve
   commitments when signing system transactions, and connect confirmed execution
   failures to the existing permission lifecycle. Test all three production paths.
4. Add negotiated block attachments, retention/forwarding, and eth/71 retrieval.
   Missing or damaged attachments must remain recoverable through execution.
5. Implement isolated candidate-parent and child state views, branch-aware Parlia
   queries, and construction overlap. Sealing must wait for successful parent
   import and matching canonical state; reorgs or failures cancel dependent work.

Current Geth local-cluster compatibility checks exercise existing BSC rules and
advisory hints. They cannot establish BEP-732 interoperability until a reference
client implements the same activated rules. This increment makes no throughput
or latency claim.
