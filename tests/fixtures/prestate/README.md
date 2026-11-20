# Native prestate differential fixtures

`cases.json` runs through the BSC RPC wrapper in `go_bsc_native_prestate_matrix`.
It covers CAS20 code-hash reads after a failed value-call gas calculation, with
control cases, reverted parents, include/omit-empty, and standalone/mux tracers.
`expected.json` comes from Go-BSC `583be4dbf9ab748801ffdd02b4c13e46b103a9f9`.

Regenerate from that Go-BSC checkout using the shared generator in Reth PR #260:

```sh
go run /path/to/reth/crates/rpc/rpc/testdata/prestate/generate.go /path/to/reth-bsc/tests/fixtures/prestate/cases.json
```

Run the consumer test with `cargo test --lib go_bsc_native_prestate_matrix`.
The generator and the complete compatibility contract are in Reth's
`crates/rpc/rpc/testdata/prestate/README.md`. The comparison excludes only the
pre-existing `codeHash` schema difference. Existing native tests retain coverage
of storage, creation, disabled code/storage, and sequential call state.
