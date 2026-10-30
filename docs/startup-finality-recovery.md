# Startup finality recovery

Reth 2.8 may persist block data ahead of the partial state/trie frontier. Startup consistency
recovery then unwinds to that frontier, including lowering `LastFinalizedBlock` and deleting
headers above it. A connected peer does not prove that the previous finalized history has
been replayed. Previously, the BSC miner could immediately produce replacement blocks below
the pre-restart finalized checkpoint.

The BSC launcher now wraps the ordinary pipeline backfill. Its pre-consistency recovery hook
reads the persisted finalized number and canonical hash, and commits them under the existing
Metadata key `bsc_startup_finalized` before Reth can unwind. A previous recovery marker takes
precedence over a lowered finalized number. Advancing that marker on a later startup requires
the old checkpoint hash to match the persisted canonical history. Unreadable metadata, missing
finalized headers without an existing saved hash, or conflicting hashes cause startup to fail
instead of discarding finality. A saved checkpoint can still be recovered when its local header
has disappeared before storage healing lowers the ChainState pointer.

Before local mining or voting is allowed, the startup task checks that the executed canonical
head has reached the checkpoint, its canonical hash matches, and the Engine's finalized marker
has been restored. It requests the saved hash through the Engine while recovery is pending.
Neither connected peers, header presence, `eth_syncing=false`, elapsed time, nor an FCU
`SYNCING`/`VALID` response alone releases signing. Invalid execution or a canonical hash conflict
fails the critical startup task. An unavailable chain keeps signing disabled and reports the
pending checkpoint; request timeouts retry without bypassing finality. The task observes node
shutdown through Reth's critical-task cancellation.

The marker deliberately remains after successful recovery. Engine finality persistence is
asynchronous; deleting it immediately could lose the constraint on another restart before
that persistence batch completes. A later startup can replace it with a newer persisted
finalized checkpoint on the same history. Do not delete this marker or lower finality as an
automatic recovery action.

## Cost and guarantee boundary

- Startup reads one metadata value, the existing finalized number, and up to two headers.
  A new or advanced checkpoint adds one small metadata transaction at startup. An unchanged
  checkpoint adds no write. First startup with no finalized checkpoint (or only genesis)
  creates no marker and does not wait for network recovery.
- Recovery checks execution/finality and waits three seconds between pinned Engine requests,
  with a ten-second request timeout. The task exits after successful recovery.
- Ordinary mining scheduling, header signing, vote production, and vote signing each check
  the shared in-memory gate. There is no added normal-operation database read, write, timer,
  lock, or wait. The normal scheduled signing path therefore adds two gate checks for a block
  or vote. No throughput or latency improvement is claimed from this code-path budget.
- This protects finality already committed by Reth, plus an outstanding startup recovery
  marker. It cannot recover a newer finalized checkpoint that existed only in memory at
  shutdown. Existing Reth batches persist finalized numbers asynchronously and may clamp
  them to the persisted block tip. Protecting all such in-memory finality would require a
  separate runtime durability change.
- Installing this fix does not reconstruct finalized history already discarded before the
  fix was installed. It also does not establish why the incident's shutdown left a partial
  frontier. It makes no changes to Geth state pruning or offline recovery.

The local Geth incident source at `817e61dc` pauses mining and voting on downloader start,
resumes on completion/failure, and retains the 40-block voting warm-up. Its fork choice compares
justified height before total difficulty. This fix retains BSC's existing voting warm-up and
uses an explicit persisted hash constraint for Reth's startup rewind; Geth's downloader events
are not treated as proof of that history. Transaction execution and gas accounting are unchanged.

## Local regression checks

`cargo test --locked --no-default-features --lib startup_recovery` checks real temporary MDBX
metadata and header static files, repeated recovery, exact hash conflicts, cold start, Engine
responses, timeout behavior, and normal readiness. These model the unwind's persisted effects;
they do not run a full multi-node partial-trie crash/restart experiment.

`cargo test --locked --no-default-features --test startup_recovery_signing` owns an isolated
process-wide gate, connects two local devp2p peers, and exercises the actual startup hook and
both ECDSA/BLS signing boundaries. The Engine responses and replay completion are controlled
by the test; this is not a full BSC consensus/replay integration test.

Validation on 2026-10-30: `cargo test --locked --no-default-features --lib --test
startup_recovery_signing -- --test-threads=1` passed 913 library tests (one existing vector
generator ignored) and the isolated signing test. An initial parallel library run passed 906
tests and failed seven vote-pool/protocol/producer tests; all seven passed in the sequential
run. That parallel failure is retained as a validation limitation, not reported as a clean
parallel-suite pass. No remote node restart, deployment, or load test was performed.
