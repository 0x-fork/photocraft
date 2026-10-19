# Rust Zstandard workspace reuse

Removed locally: complete layered-photo ZIP save changes range from -1.9% to +0.2%. An apparent 4.8% gain in one 16-bit APFS directory save failed independent confirmation, which was 3.2% slower. The other directory cases are neutral. There is no repeatable meaningful whole-operation gain, so store.rs, including its added workspace tests and synthetic microbenchmark, is restored byte for byte to main. The original local tip is recoverable at refs/archive/perf/zstd-workspaces-PRE-LAYERED-REVIEW. Only the performance report remains different from main on this branch.

Measured on 2026-10-19 against frozen main `d95fdf3181c01ad41e95456091a394b105a9afb2`, independently for each branch. Apple M4 Max, 16 CPUs, 128 GiB, macOS arm64, rustc 1.99.1. Release O3, LTO off, 16 codegen units, debug off; builds/tests at nice 10 with warm caches. Timed operations run at normal priority without concurrent builds or profiling. Persistent processes alternate execution order after a warmup. Time change compares the two medians; paired bootstrap intervals from these small samples describe variation, not guarantees.

CC0 Pixls P1000475.RW2 supplies a 6000 x 4000 centre crop (24 MP, no resampling); IMG_4059.CR2 supplies the native 4000 x 3000 city photograph. U8/U16/F32 documents retain ProPhoto RGB; F32 derives from 16-bit development and is not native HDR. No generated noise, repeated ramps, enlarged charts or duplicated photo layers are timed. Source pins and attribution remain in the existing corpus.

Each constructed photo edit has four visible layers: the real raster photograph; global Levels (black 2/255, white 250/255, gamma 1.08); Curves with master points (0,0), (0.25,0.22), (0.5,0.52), (0.75,0.80), (1,1) plus red midtone 0.5→0.515; and foreground Levels (gamma 1.15) through a smooth grayscale mask fading from zero to one over 35–75% of image height. These are authored benchmark edits of licensed photos, not Photoshop-authored oracle files. Saved previews were inspected.

Complete full-resolution CPU rendering and fresh ZIP saves use seven alternating pairs. Thumbnail rendering, hashing and document equality are outside timers. Every render must match main float bit for float bit; every save must match main byte for byte and reload to the complete source document. Directory saves also use seven pairs and fresh directories with a fresh writer; an independent confirmation uses nine pairs. Directory creation/removal, output hashing and full reload equality are outside the timer. These directory timings include APFS writes and fsync, unlike the in-memory ZIP timings. These are CPU/core measurements, not GPU or browser performance claims.

| Document | Operation | Main ms | Branch ms | Change | Paired 95% interval |
|---|---|---:|---:|---:|---:|
| panasonic-8 | save | 167.893 | 165.025 | -1.7% | -5.3..-0.6% |
| panasonic-16 | save | 371.857 | 364.765 | -1.9% | -3.0..+1.8% |
| panasonic-32 | save | 641.312 | 635.064 | -1.0% | -3.0..+4.3% |
| canon-8 | save | 93.469 | 93.592 | +0.1% | -0.8..+1.0% |
| canon-16 | save | 185.048 | 185.385 | +0.2% | -0.2..+4.3% |
| canon-32 | save | 327.223 | 326.519 | -0.2% | -0.2..+2.4% |
| panasonic-8 | save-dir | 5537.227 | 5534.940 | -0.0% | -7.1..+2.2% |
| panasonic-16 | save-dir | 7077.156 | 6739.019 | -4.8% | -5.3..-0.3% |
| canon-32 | save-dir | 3979.360 | 3974.353 | -0.1% | -0.9..+1.6% |
| panasonic-16 | save-dir (confirm) | 6483.861 | 6691.230 | +3.2% | +1.5..+9.2% |

The earlier eight flat-photo/authored-design comparisons were also close to neutral for fresh ZIP saves. All candidate bundles and directory contents match main byte for byte. Retention required a repeatable improvement of at least 3% in a complete operation, with a paired interval excluding zero and no material new regression. Kernel-only differences do not satisfy that criterion.

Post-removal affected checks passed: three RGB ZIP depth round trips; directory round trips across the existing mode/depth matrix; ZIP incremental saves; directory incremental saves and garbage collection (six tests total). Strict all-target format Clippy, affected format WASM and formatting checks also passed. No unrelated full-suite or synthetic timing run was repeated.

Harness, raw observations, exact inputs and binary hashes: `.local/perf-followup-261019/` (ignored local evidence). Previous flat-photo and authored-design results remain in `.local/perf-real-261019/`. Nothing was pushed and no PR was changed.

## Earlier split validation

These checks passed when the split was prepared; they were not all rerun for this benchmark review. Current affected checks are listed above where production code changed.

- `cargo test -p photocraft-format --locked`: passed (147 passed, 1 ignored).
- `cargo clippy -p photocraft-format --locked --all-targets -- -D warnings`: passed.
- `cargo xtask layers`: passed.
- `cargo check -p photocraft-format --locked --target wasm32-unknown-unknown`: passed.
