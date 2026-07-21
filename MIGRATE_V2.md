# Migrating storage layout v1 → v2 (`db migrate-v2`)

`reth-bsc db migrate-v2` upgrades an existing node's on-disk storage from the
legacy **v1** layout (everything in MDBX) to the **v2** layout (cold,
append-only data lives in static files, hot state lives in MDBX/RocksDB). v2 is
the layout the node uses going forward; running on v1 keeps you on a code path
that is being phased out and that is slower and larger on disk.

You only need to do this **once per datadir**. New datadirs created by a recent
build are already v2 — `migrate-v2` will detect that and exit immediately.

---

## TL;DR

```bash
# 1. Stop the node (the migration needs exclusive access to the database).

# 2. Back up — or at least make sure you can re-sync — before migrating.

# 3. Run the migration (point it at the SAME chain + datadir the node uses).
./target/release/reth-bsc db migrate-v2 \
    --chain bsc \
    --datadir /path/to/your/datadir

# 4. Restart the node normally. It rebuilds the recomputable data via the
#    pipeline on first run; the first start after migration takes longer.
./target/release/reth-bsc node --chain bsc --datadir /path/to/your/datadir
```

> The migration is **idempotent against an already-migrated datadir**: if the
> storage is already v2, the command logs `Storage is already v2, nothing to do`
> and returns without touching anything.

---

## What it actually does

The command runs in phases. Understanding them helps you read the logs and know
what is safe to interrupt.

| Phase | Action | Notes |
| ----- | ------ | ----- |
| 0 — Preflight | Reads current `StorageSettings`, finds the chain tip, and verifies the target static-file segments are empty. | Bails out if storage is already v2, or if the `AccountChangeSets` / `StorageChangeSets` static-file segments already contain data. |
| 1 — Changesets | Moves `AccountChangeSets` and `StorageChangeSets` from MDBX into static files. | These cannot be recomputed, so they are *migrated*, not cleared. Respects existing prune checkpoints. |
| 2 — Receipts | Moves `Receipts` into static files. | **Skipped** (receipts kept in MDBX) if you run with a receipt-log-filter prune config. Skipped if receipts are already in static files. |
| 3 — Flip metadata | Writes `StorageSettings::v2()`. | After this point the datadir is marked v2. |
| 4 — Clear recomputable tables | Clears senders, tx-hash/history indices, plain state, and trie tables, then resets the relevant stage checkpoints to 0. | This data is rebuilt by the pipeline on next node start. |
| 5 — Compact MDBX | Copies MDBX to a compacted `db_compact`, then atomically swaps it in (keeping a temporary `db_pre_compact` backup that is removed on success). | This is where most of the disk-space reduction comes from. |
| 6 — Pipeline rebuild | **Done by the node, not the command.** | On the next `node` start, stages with reset checkpoints (sender recovery, tx lookup, history indices, merkle) re-run to rebuild what was cleared. |

So the data that *cannot* be recomputed (changesets + receipts) is preserved by
moving it to static files; everything that *can* be recomputed is dropped and
rebuilt, which is what lets the database compact down significantly.

---

## Datadir layout: which stores are involved

A v2 datadir uses **three** stores, but the `migrate-v2` command itself only
touches two of them. The third (RocksDB) is created by the node on the next
restart.

```
<datadir>/<chain>/
├── db/             # MDBX  — touched by migrate-v2 (read, cleared, compacted)
├── static_files/   # static files — written by migrate-v2 (changesets, receipts)
└── rocksdb/        # RocksDB — NOT created by migrate-v2; appears on first node restart
```

What ends up where under v2:

| Store | Holds (v2) | Created/populated by |
| ----- | ---------- | -------------------- |
| `static_files/` | changesets, receipts, transaction senders | `migrate-v2` (changesets, receipts) + pipeline |
| `rocksdb/` | history indices — `AccountsHistory`, `StoragesHistory`, `TransactionHashNumbers` | the **node**, on the first restart after migration |
| `db/` (MDBX) | hashed state + remaining hot tables | the node |

So during the migration, **only `db/` and `static_files/` are required and
touched** (plus the transient `db_compact` / `db_pre_compact` directories created
next to `db/` during compaction). The migration *clears* the history-index
tables from MDBX but does **not** write RocksDB — those indices are rebuilt into
a freshly created `rocksdb/` directory by the pipeline when you next start the
node (the stages whose checkpoints were reset to 0 in phase 4). This is why the
first restart after migration does extra work and takes longer.

> You do not need to pre-create `rocksdb/`. If you keep your RocksDB on a
> separate path via `--datadir.rocksdb <PATH>`, pass the same flag to the `node`
> command after migrating so the node builds it where you expect.

---

## Requirements & precautions

- **Stop the node first.** The migration opens the database read-write and needs
  exclusive access. Running it against a live node will fail or contend on the
  lock.
- **Use the same `--chain` and `--datadir` your node uses.** Otherwise the
  command operates on the wrong (or a non-existent) database. It errors out if
  the datadir or database directory does not exist.
- **Have enough free disk.** Phase 5 writes a *second*, compacted copy of MDBX
  (`db_compact`) alongside the original before swapping. Make sure you have free
  space roughly equal to your current MDBX size before starting. The compacted
  copy plus the temporary `db_pre_compact` backup exist briefly at the same
  time.
- **Back up / be ready to re-sync.** The migration mutates the database in place.
  It is designed to be safe and keeps a temporary backup during the swap, but a
  snapshot of the datadir (or confidence that you can re-sync) is the cheapest
  insurance.
- **First restart is slower.** Because cleared tables are rebuilt by the
  pipeline, the first `node` run after migration does extra work before it
  reaches the tip. This is expected.

---

## Verifying the result

Check the storage version with the `db settings` subcommand (or look for
`Storage settings updated to v2` in the migration logs):

```bash
./target/release/reth-bsc db settings --chain bsc --datadir /path/to/your/datadir
```

Re-running `migrate-v2` on a migrated datadir is a safe no-op and is a quick way
to confirm migration completed:

```bash
./target/release/reth-bsc db migrate-v2 --chain bsc --datadir /path/to/your/datadir
# -> "Storage is already v2, nothing to do"
```

---

## Troubleshooting

- **`Static file segment ... already contains data. Cannot migrate — target
  must be empty.`** — The changeset static-file segments already hold data,
  meaning a previous (possibly partial) migration ran, or the datadir is in an
  unexpected state. Do not force it; restore from backup or re-sync into a fresh
  datadir.
- **`Datadir does not exist` / `Database does not exist`** — You pointed the
  command at the wrong path or chain. Match exactly what your `node` command
  uses.
- **Migration interrupted before the metadata flip (phase 3).** The original
  data is still intact in MDBX. Re-running the command starts over; the preflight
  empty-segment check protects against double-writing changesets.
- **Disk filled up during compaction (phase 5).** Free up space and re-run.
  The swap is atomic — on failure it restores the original database from the
  `db_pre_compact` backup.

---

## FAQ

**Do I have to migrate?** Not immediately, but v2 is the supported forward path.
v1 is being phased out, runs slower, and uses more disk. Migrate when convenient.

**Will it lose data?** No data that matters is discarded. Non-recomputable data
(changesets, receipts) is moved to static files; recomputable data is rebuilt by
the pipeline on next start.

**How long does it take?** Proportional to database size — the changeset/receipt
copy and the MDBX compaction dominate. Plan for a maintenance window on a
mainnet-sized node, and expect the first node restart afterward to also take
extra time for the pipeline rebuild.

**Can I run it on a fresh/new node?** You don't need to. Recent builds create v2
datadirs directly; `migrate-v2` will just report it's already v2.
