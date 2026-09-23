# CAS20 benchmarks

Run from the repository root:

```sh
cargo bench --bench cas20 --features bench-test -- \
  --sample-size 10 --measurement-time 1 --warm-up-time 1
```

Local measurement on 2026-09-23: Apple M1 Pro (8 physical/logical cores),
macOS, rustc 1.98.1 (`48a229cea`, 2026-09-01), default optimized bench profile.
These are short, in-memory microbenchmarks, not full-block or disk-I/O measurements.
The ranges below are Criterion's 95% confidence intervals; the middle value is
its point estimate. No controlled measurement of the old implementation was made.

## Dynamic lookup

| Case | Estimate | 95% interval |
| --- | ---: | ---: |
| Ordinary address miss | 1.325 ns | 1.317–1.346 ns |
| Invalid token prefix layout | 1.694 ns | 1.588–1.836 ns |
| Factory prefix, wrong address | 1.979 ns | 1.849–2.130 ns |
| Registry prefix, wrong address | 3.239 ns | 3.119–3.311 ns |
| Token hit, including handle clone | 9.802 ns | 9.759–9.838 ns |

## Observer cost

All cases execute the same transfer and discard its state changes. `disabled`
skips timing and observation; `no_recorder` explicitly enables observation with
no-op registered handles; `prometheus` uses handles registered with the real
Prometheus recorder. Registration happens outside the timed loop. Normal node
startup selects the disabled path when neither metrics exporter is configured.

| Single-thread transfer | Estimate | 95% interval |
| --- | ---: | ---: |
| Disabled | 1.315 µs | 1.307–1.323 µs |
| No recorder | 1.380 µs | 1.348–1.409 µs |
| Prometheus | 1.444 µs | 1.412–1.501 µs |

The Prometheus point estimate is about 10% above disabled observation in this run.

The parallel benchmark keeps four workers alive, each executing 256 transfers
per sample iteration (1,024 total), and shares metric handles. It excludes thread
creation and database cloning, but includes start/end barriers and scheduling.

| Four-worker batch | Estimate | 95% interval |
| --- | ---: | ---: |
| Disabled | 0.894 ms | 0.887–0.907 ms |
| No recorder | 0.889 ms | 0.877–0.913 ms |
| Prometheus | 1.543 ms | 1.183–2.071 ms |

Prometheus remains measurably more expensive under concurrent updates. Its wide
interval makes a precise parallel overhead percentage unreliable. Removing the
CAS20 observer's global map locks does not remove synchronization inside the
recorder. Use longer runs on deployment hardware for throughput decisions.

The suite also retains transferFrom, a four-child policy union and an announce
with two internal calls. Adversarial-input and full-block timing required before
activation by BEP-702 §3.14 still need separate measurements.
