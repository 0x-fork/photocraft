# Deflate backend comparison

Only the workspace flate2 backend changes from miniz_oxide to zlib-rs with runtime CPU detection. The 24 MP (6000×4000) RGBA8 cases are a repeating ramp and deterministic noise; inputs are prepared outside the timers.

Measured on 2026-10-19 against `d95fdf3181c01ad41e95456091a394b105a9afb2` on an Apple M4 Max
(16 CPUs, 128 GiB), macOS arm64, rustc 1.99.1. Each branch starts directly at that
base; none of the other performance splits is applied. Release uses optimization
level 3, LTO off, 16 codegen units, debug info off and incremental compilation off.
Builds ran at nice 10 with APFS-cloned warm caches; measurements ran at normal
priority with no concurrent builds. Timings are medians of five samples after one
warm-up, except the compositor's ten Criterion samples. These synthetic workloads
are reproducible checks of this change, not a representative image corpus.

| Workload | Main ms | This change ms | Time change |
|---|---:|---:|---:|
| ramp-png-encode | 42.114 | 43.989 | +4.5% |
| ramp-png-decode | 47.100 | 8.697 | -81.5% |
| ramp-psd-encode | 200.218 | 162.012 | -19.1% |
| ramp-psd-decode | 45.092 | 31.839 | -29.4% |
| ramp-pcraft-save | 82.126 | 83.149 | +1.2% |
| ramp-pcraft-load | 1.424 | 1.474 | +3.5% |
| noise-png-encode | 206.339 | 152.328 | -26.2% |
| noise-png-decode | 187.408 | 183.032 | -2.3% |
| noise-psd-encode | 1191.777 | 1002.312 | -15.9% |
| noise-psd-decode | 30.907 | 30.394 | -1.7% |
| noise-pcraft-save | 177.713 | 175.379 | -1.3% |
| noise-pcraft-load | 103.026 | 101.085 | -1.9% |

| Encoded file | Main bytes | This change bytes |
|---|---:|---:|
| ramp-png-encode | 107,506 | 478,751 |
| ramp-psd-encode | 73,268,469 | 73,503,868 |
| ramp-pcraft-save | 88,188 | 88,214 |
| noise-png-encode | 82,731,323 | 82,272,418 |
| noise-psd-encode | 144,983,563 | 144,994,193 |
| noise-pcraft-save | 96,670,862 | 96,670,888 |

Compressed bytes are not identical across backends. The ramp PNG gets larger; the noise case and decode timings must be considered alongside encode time. The native format stores Zstandard tiles and STORE ZIP entries, so this switch only changes its PNG previews; the repeating ramp deduplicates many tiles. No tile-compression speedup is attributed to deflate.

The committed compatibility test compares old miniz_oxide and flate2 inflation of PNG IDAT and PSD ZIP/ZIP-prediction streams, checks exact decoded PNG/PSD pixels at supported depths, and compares complete .pcraft documents at U8/U16/F32 (tiles included) plus their PNG thumbnail. Float PSD composites are explicitly ZIP-encoded in the test because the IO exporter normally chooses Raw. The same test passed on the base and this branch.

Reproduce with the same release profile environment:

```sh
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_INCREMENTAL=0 nice -n 10 cargo test --release -p photocraft-io --test deflate_compatibility --no-run
# Run the printed executable at normal priority:
<printed-executable> benchmark_deflate_formats --ignored --nocapture --test-threads=1
```

## Local validation

- `cargo test -p photocraft-codecs -p photocraft-psd -p photocraft-format -p photocraft-io --locked`: passed (1956 passed, 9 ignored).
- `cargo clippy -p photocraft-codecs -p photocraft-psd -p photocraft-format -p photocraft-io --locked --all-targets -- -D warnings`: passed.
- `cargo xtask layers`: passed.
- `cargo xtask wasm`: passed.
- `cargo xtask test-corpus -p psd -p codecs -p io`: passed (1853 passed, 9 ignored).
- The compatibility test also passed with the main backend before measurements. To reproduce the main comparison, copy only this test and its miniz_oxide dev-dependency into a clean main worktree.
