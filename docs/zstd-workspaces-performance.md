# Rust Zstandard workspace reuse

Reuse the existing pure-Rust compressor workspace per save worker. Partial-block objects keep the fresh-compressor fallback. No new backend or C dependency.

Fresh real-document saves are close to neutral. Reusing compressor allocation does not establish a meaningful end-to-end save improvement on these inputs. Every bundle remains byte-identical.

Measured on 2026-10-19 against main `d95fdf3181c01ad41e95456091a394b105a9afb2`, independently of the other splits. Apple M4 Max, 16 CPUs, 128 GiB, macOS arm64, rustc 1.99.1; release O3, LTO off, 16 codegen units, debug off. Builds used nice 10 and warm APFS-cloned caches; measurements used default priority without concurrent builds or profiling. Five alternating paired samples follow a warmup; confirmation and fix runs use nine pairs. Time change compares the two medians; intervals resample paired ratios. Negative time change means faster. Small differences are not automatically established improvements or regressions.

The full-resolution CC0 Pixls photographs are P1000475.RW2 (24 MP foliage centre crop, no resampling) and IMG_4059.CR2 (12 MP city), each converted to U8/U16/F32 with the ProPhoto RGB profile. F32 derives from 16-bit development, not HDR capture. MIT psd-tools masks.psd supplies a 640 x 1394 web layout with 22 imported layers and three masks; mask-index.psd supplies a 1000 x 867 illustrated card with a mask. The sources and hashes are pinned by the existing corpus. These are the available authored layered files, not a large layered-photo corpus. No generated noise, repeating ramps, enlarged charts or duplicated photo layers are used.

The shared harness times in-memory operations. Input loading, image conversion, thumbnail rendering, hashes and equivalence checks are outside timers. Every save uses a fresh PcraftWriter and a real rendered thumbnail. Common reads use main-encoded files; own-output reads are separate. PSD exports use ZIP merged images at U8/U16, Raw merged images at F32, and normal RLE layer channels at U8/U16 / ZIP prediction at F32. Results do not measure filesystem throughput or browser performance.

| Document | Operation | Main ms | Branch ms | Time change | Paired 95% interval | Main / branch bytes |
|---|---|---:|---:|---:|---:|---:|
| panasonic-8 | save | 163.156 | 162.549 | -0.4% | -1.4..+0.1% | 49,643,217 / 49,643,217 |
| panasonic-8 | load-own | 396.986 | 397.099 | +0.0% | -0.2..+1.1% | 49,643,217 / 49,643,217 |
| panasonic-16 | save | 355.760 | 353.995 | -0.5% | -0.9..+0.2% | 175,681,852 / 175,681,852 |
| panasonic-16 | load-own | 386.774 | 386.373 | -0.1% | -0.9..+1.0% | 175,681,852 / 175,681,852 |
| panasonic-32 | save | 627.383 | 630.287 | +0.5% | -0.6..+2.2% | 252,231,671 / 252,231,671 |
| panasonic-32 | load-own | 1320.923 | 1320.769 | -0.0% | -0.1..+0.7% | 252,231,671 / 252,231,671 |
| canon-8 | save | 86.768 | 86.708 | -0.1% | -0.9..+1.8% | 26,151,631 / 26,151,631 |
| canon-8 | load-own | 221.917 | 224.088 | +1.0% | -1.2..+1.2% | 26,151,631 / 26,151,631 |
| canon-16 | save | 185.739 | 186.190 | +0.2% | -1.5..+0.9% | 88,750,003 / 88,750,003 |
| canon-16 | load-own | 198.518 | 198.190 | -0.2% | -0.8..+1.0% | 88,750,003 / 88,750,003 |
| canon-32 | save | 335.712 | 338.159 | +0.7% | -1.1..+2.0% | 120,957,984 / 120,957,984 |
| canon-32 | load-own | 787.610 | 786.982 | -0.1% | -1.8..+2.3% | 120,957,984 / 120,957,984 |
| layout-masks | save | 18.907 | 19.230 | +1.7% | -1.7..+4.0% | 433,949 / 433,949 |
| layout-masks | load-own | 16.310 | 16.411 | +0.6% | -2.6..+3.3% | 433,949 / 433,949 |
| layout-mask-index | save | 7.598 | 7.446 | -2.0% | -7.7..+6.3% | 344,892 / 344,892 |
| layout-mask-index | load-own | 6.848 | 6.840 | -0.1% | -2.6..+3.7% | 344,892 / 344,892 |

Every saved real bundle is byte-identical to main and complete documents round-trip equal. The committed unit oracle compares reused frames with fresh compression, including partial blocks and size transitions.

Local raw evidence and the shared harness: `.local/perf-real-261019/` in the repository checkout (ignored). This comparison supersedes the earlier synthetic performance table. Small synthetic unit fixtures remain correctness checks; ignored synthetic timing tests were not run.

## Earlier split validation

These checks passed when the split was prepared; they were not all rerun for this benchmark review. Current affected checks are listed above where production code changed.

- `cargo test -p photocraft-format --locked`: passed (147 passed, 1 ignored).
- `cargo clippy -p photocraft-format --locked --all-targets -- -D warnings`: passed.
- `cargo xtask layers`: passed.
- `cargo check -p photocraft-format --locked --target wasm32-unknown-unknown`: passed.
