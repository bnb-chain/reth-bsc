# Native prestate differential fixtures

`cases.json` runs through the BSC RPC wrapper in `go_bsc_native_prestate_matrix`.
It covers CAS20 code-hash reads after a failed value-call gas calculation, with
control cases, reverted parents, include/omit-empty, and standalone/mux tracers.
`expected.json` comes from Go-BSC `54afaff4da31ac9e49ba646a4f65104edac5a2f3`.

Regenerate from that Go-BSC checkout with Go 1.25.7. The generator at the pinned
Reth revision strips `codeHash` for Reth's own fixtures. In a temporary copy,
omit that filter to keep Go-BSC's original hashes:

```sh
fixture_tmp=$(mktemp -d)
sed '/for _, a := range accounts {/,/^[[:space:]]*}/d' \
    /path/to/reth/crates/rpc/rpc/testdata/prestate/generate.go > "$fixture_tmp/generate.go"
GOTOOLCHAIN=go1.25.7 go run "$fixture_tmp/generate.go" /path/to/reth-bsc/tests/fixtures/prestate/cases.json
```

Run the consumer test with `cargo test --lib go_bsc_native_prestate_matrix`.
The generator and the complete compatibility contract are in Reth's
`crates/rpc/rpc/testdata/prestate/README.md`. The comparison includes the account
`codeHash`. Existing native tests retain coverage of storage, creation, disabled
code/storage, and sequential call state.
