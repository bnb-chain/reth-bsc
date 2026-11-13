# BAL common local-cluster suite — 2026-11-13

All five requested groups passed on `feat/bal-v2.8`, source revision
`5c9da45a82b70e1b27536833209e4a3ee4254156`, with companion Reth
`9278c0cddb`. No Rust changes were needed for this run.

## Setup

Three Reth validators (BAL enabled on nodes 0/1, disabled on node 2), plus a
non-mining BSC Geth 1.8.1 observer (`583be4db`). All used an isolated chain-714
genesis, matching forks through Jenner and fresh data. All Reth validators used
the default `BSC_SUBMIT_BUILT_PAYLOAD=false` path. The debug binary has the same
Rust source as the recorded revision; it was built before the preceding commit.

CAS20's test activation administrator was seeded in fresh genesis; feature
activation itself was performed through transactions. A separate public
development-fixture account was funded for the existing BEP-675 driver. No keys
or passwords are included in reports. The driver was rebuilt from local source;
its source file hashes are retained in `bep675-report.json`.

HTTP endpoints were localhost ports 28460–28463. BEP-675 targeted node 0, with
node 1 as the debug candidate builder. The HTTP `mev` namespace was added to node
0's exposed API list by a scoped restart; the other nodes continued producing.

## Results

| Group | Result | Coverage |
| --- | --- | --- |
| BEP-675 admission | 15/15 passed | Height, parent, turn, gas, registration, system-transaction shape, permissions, timing, accepted invalid bid and automatic revocation. |
| BEP-675 real bids | 3/3 passed | Valid bid canonical inclusion/tag, competing-bid selection and double-sign guard, low-average-gas-price permission revocation. |
| CAS20 | Passed | Asset/stablecoin activation, creation/address prediction, duplicate salt, authorization, role-gated mint, supply, decimals/currency, transfers/allowances, deactivation and existing-token operation. |
| ERC20 | Passed | Deploy, transfer, approve, transferFrom, exact balances/allowance, insufficient-balance/allowance reverts and rollback. |
| BNB | Passed | 2 BNB transfer and 0.25 BNB return; exact recipient credit, sender debit including gas, 21,000 gas and cross-client balances pinned to receipt blocks. |
| Uniswap V2 | Passed | Official factory/router/WETH artifacts; ERC20/ERC20, CAS20 asset/ERC20 and CAS20 stablecoin/ERC20 pools; liquidity, swaps in both directions, exact quotes, slippage/deadline rejection and liquidity removal. |

The native/token/DEX workload made **36 explicit assertions across 67 transactions**:
48 succeeded and 19 deliberately reverted. Every receipt and log matched on all
four clients, including block placement, gas usage and cumulative gas usage.
The separate BEP-675 workload produced canonical tagged blocks 764, 765 and 801;
the test builder's permission was restored after both driver modes.

After settling 12 blocks behind the common head, **all blocks 0–833** matched
across all four clients: hashes, parent hashes, state/receipt/transaction roots,
transaction lists, gas, difficulty, timestamps and requests hashes. All 67 workload
receipts were checked again after the deliberate invalid-bid cases.

```text
block:     833
hash:      0x9eaccbd02984dfee37a3f0a47cfd7baa679441b57b3cc6b15d312241be4b60d0
stateRoot: 0x4c976bcab2d3a1847352aa24c41eca4ca7a8c113203b8d679dbaaada0490c5fe
```

BAL metrics at the final comparison showed node 0 sent 50/received 65 hints with
65 lookup hits, and node 1 sent 290/received 263 with 263 hits. Node 0's counters
reset during its restart. Both enabled nodes logged actual BAL prewarm invocation;
node 2 had neither BAL counters nor BAL prewarm traces. The disabled validator
produced 291 of the 833 non-genesis compared blocks.

Expected error logs accompanied the intentional invalid nonce bid (aborted receipt
work and builder revocation) and the underpriced bid. No settled-chain divergence
or sustained stall was observed. The invalid bid is not counted as a successful
transaction. Zero-simulation BEP-675 bids currently carry no BAL hints; these cases
verify compatibility and fallback, not acceleration of that path.

## Artifacts and reusable skill

Artifacts remain in `/Users/chee/Documents/repo/bsc-common-suite-bal-2026-11-13/`:

- `cluster.py`, `suite-config.json`, `functions.cjs`, `run_bep675.py`,
  `verify_cluster.py`: setup, workload and assertions.
- `results/functions-report.json`, `functions.log`: checks, contracts, transactions
  and receipts; `bep675-tier1.log`, `bep675-tier2.log`, `bep675-report.json`: bid evidence.
- `results/consensus-report.json`, `verified-blocks.json`, `node*-metrics.txt`,
  `log-evidence.json`, node logs, genesis and owned process records.
- `package-lock.json`: pinned dependency record. Used ethers 6.17.0, Uniswap V2
  core 1.0.1 and periphery 1.1.0-beta.0 from the existing local package cache.

Installed reusable skill:
`/Users/chee/.codex/skills/reth-bsc-local-cluster-tests/SKILL.md`.
It includes scenario/setup references, the tested configurable transaction runner
and the canonical-chain verifier. Skill-creator validation and script syntax checks
passed. The packaged verifier was executed for this run, and the packaged
transaction runner matches the executed workload.

All four owned test processes were stopped. Existing clusters/data were preserved.
This is functional local-cluster coverage, not performance/load testing. It does
not cover BEP-675 gRPC, production governance appointment, Uniswap V3/V4, flash
swaps or native-BNB router routes.
