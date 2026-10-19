# Rust Zstandard workspace reuse

Only the existing pure-Rust compressor workspace is reused per worker/save. No new backend or C dependency. The kernel compresses 367 independent 256 KiB objects, approximately 24 MP of RGBA8 data, with the fresh main path and reused path alternating order.

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
| 24mp-rgba8 | 125.373 | 121.006 | -3.5% |

Both paths produced 1,352,031 encoded bytes. These are serial compression-kernel timings; they do not measure ZIP assembly, hashing, filesystem work or parallel save latency.

The committed unit test compares each frame byte-for-byte with the previous fresh compress_to_vec path and decodes it, across empty/short/full/partial block inputs and size transitions, including empty input after nonempty input. Partial-block objects keep the fresh-compressor fallback.

Reproduce with the same release profile environment:

```sh
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_INCREMENTAL=0 nice -n 10 cargo test --release -p photocraft-format --lib --no-run
# Run the printed executable at normal priority:
<printed-executable> benchmark_workspace_reuse --ignored --nocapture --test-threads=1
```

## Local validation

- `cargo test -p photocraft-format --locked`: passed (147 passed, 1 ignored).
- `cargo clippy -p photocraft-format --locked --all-targets -- -D warnings`: passed.
- `cargo xtask layers`: passed.
- `cargo check -p photocraft-format --locked --target wasm32-unknown-unknown`: passed.
