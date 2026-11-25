# CIRCL cache benchmark fixtures

Each `.bin` concatenates 17 complete, valid precompile inputs in the existing
experimental `message || signature || public_key` format. Chunk sizes are 3,764
bytes for ML-DSA-44 and 7,920 bytes for SLH-DSA-SHA2-128s.

For zero-based index `i`, the seed is byte `i+1` repeated to the scheme's seed
length, and the 32-byte message is byte `i+128` repeated. These are public test
seeds, never production keys. Signatures use the pure scheme and empty context;
SLH signing is explicitly deterministic. CIRCL is pinned to 1.6.5 by the
existing generator module. The generator self-verifies all signatures, and the
Rust benchmark checks each complete input with RustCrypto before timing.

Regenerate from `tests/fixtures/generate` with `go run ./cache`.

The first key supplies repeated-key timing, the first eight supply a working
set below the 16-key cache limit, and all 17 force LRU eviction on every call
when cycled. Messages/signatures differ between keys, so comparisons must use
the same workload on both backends; SLH work can vary with the signed message.
