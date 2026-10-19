# CAS20 fixtures

`cas20_golden.json` contains 145 calls recorded from
[bnb-chain/bsc](https://github.com/bnb-chain/bsc) commit
`057868dc17fde98fcecee6a59c472eeb05faf576`, including #3826 and #3829.
The fixture matches the BEP-702 revision in
[BEPs #716](https://github.com/bnb-chain/BEPs/pull/716), rather than the earlier master text.

`cas20_golden_test.go` is the generator. It originated in Go commit
`95c388a04ac910ff5ed692c04d836220d86fedba` and uses the updated
`scopeSeizeExempt` name. It is kept here because the recorder is not part of
Go `develop`. To reproduce, set the two paths below to local checkouts:

```sh
RETH_BSC=/path/to/reth-bsc
GO_BSC=/path/to/bsc
FIXTURES="$RETH_BSC/src/evm/precompiles/cas20/testdata"
git -C "$GO_BSC" worktree add --detach /tmp/cas20-golden-go 057868dc17fde98fcecee6a59c472eeb05faf576
cp "$FIXTURES/cas20_golden_test.go" /tmp/cas20-golden-go/core/vm/
cd /tmp/cas20-golden-go
CAS20_GOLDEN=/tmp/cas20_golden.json go test -count=1 -run '^TestRecordCAS20Golden$' ./core/vm
cmp "$FIXTURES/cas20_golden.json" /tmp/cas20_golden.json
cd "$RETH_BSC"
cargo test --lib cas20::tests::golden
```

The Go harness uses chain ID 1 and one unfinalized transaction: original slot
values start at zero and no accounts or slots start warm. Separate Rust tests
cover committed storage and transaction boundaries.

`cas20_golden_footprint.json` records Rust's per-call SLOAD, SSTORE and paid
keccak counts. It is not an independent Go oracle. After an intentional change,
regenerate it with `BLESS_GOLDEN=1 cargo test --lib cas20::tests::golden`.

`cas20_layout.json` pins the shared ERC-7201 storage layout. Golden replay and
unit tests verify semantics; they do not replace the adversarial-input and
full-block timing measurements required before activation by BEP-702 §3.14.
