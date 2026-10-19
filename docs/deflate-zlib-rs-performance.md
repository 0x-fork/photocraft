# Deflate backend comparison

Replace the workspace flate2 Rust backend with zlib-rs and runtime CPU detection. Tiles in .pcraft remain Zstandard; deflate changes its PNG previews.

Retained with explicitly accepted tradeoffs. Some PSD ZIP files grow by up to 0.71%; the 16-bit city PSD import is about 6% slower in confirmation; the small web-layout PNG grows by 598 bytes (+1.40%); one .pcraft preview adds 25 bytes. Real photo PNGs become smaller and encode faster. Compression-effort probes recovered size only by introducing other speed regressions, so no effort change is applied. There is no native-format tile speedup from deflate.

Measured on 2026-10-19 against main `d95fdf3181c01ad41e95456091a394b105a9afb2`, independently of the other splits. Apple M4 Max, 16 CPUs, 128 GiB, macOS arm64, rustc 1.99.1; release O3, LTO off, 16 codegen units, debug off. Builds used nice 10 and warm APFS-cloned caches; measurements used default priority without concurrent builds or profiling. Five alternating paired samples follow a warmup; confirmation and fix runs use nine pairs. Time change compares the two medians; intervals resample paired ratios. Negative time change means faster. Small differences are not automatically established improvements or regressions.

The full-resolution CC0 Pixls photographs are P1000475.RW2 (24 MP foliage centre crop, no resampling) and IMG_4059.CR2 (12 MP city), each converted to U8/U16/F32 with the ProPhoto RGB profile. F32 derives from 16-bit development, not HDR capture. MIT psd-tools masks.psd supplies a 640 x 1394 web layout with 22 imported layers and three masks; mask-index.psd supplies a 1000 x 867 illustrated card with a mask. The sources and hashes are pinned by the existing corpus. These are the available authored layered files, not a large layered-photo corpus. No generated noise, repeating ramps, enlarged charts or duplicated photo layers are used.

The shared harness times in-memory operations. Input loading, image conversion, thumbnail rendering, hashes and equivalence checks are outside timers. Every save uses a fresh PcraftWriter and a real rendered thumbnail. Common reads use main-encoded files; own-output reads are separate. PSD exports use ZIP merged images at U8/U16, Raw merged images at F32, and normal RLE layer channels at U8/U16 / ZIP prediction at F32. Results do not measure filesystem throughput or browser performance.

| Document | Operation | Main ms | Branch ms | Time change | Paired 95% interval | Main / branch bytes |
|---|---|---:|---:|---:|---:|---:|
| canon-16 | psd-import (confirm) | 168.891 | 178.614 | +5.8% | +3.5..+5.9% | 140,673,004 / 140,673,004 |
| panasonic-8 | save | 166.659 | 162.322 | -2.6% | -3.9..-1.0% | 49,643,217 / 49,643,242 |
| panasonic-8 | load-common | 410.794 | 409.622 | -0.3% | -0.5..+0.3% | 49,643,217 / 49,643,217 |
| panasonic-8 | png-encode | 373.880 | 165.381 | -55.8% | -56.3..-47.4% | 24,569,965 / 24,238,416 |
| panasonic-8 | png-decode | 168.141 | 167.247 | -0.5% | -2.0..+1.0% | 24,569,965 / 24,569,965 |
| panasonic-8 | psd-export | 2938.276 | 1115.740 | -62.0% | -62.3..-61.6% | 98,823,859 / 98,837,585 |
| panasonic-8 | psd-import | 275.713 | 174.046 | -36.9% | -38.8..-34.7% | 98,823,859 / 98,823,859 |
| panasonic-16 | save | 357.998 | 355.900 | -0.6% | -1.8..+0.5% | 175,681,852 / 175,681,844 |
| panasonic-16 | load-common | 387.986 | 391.411 | +0.9% | -1.3..+1.1% | 175,681,852 / 175,681,852 |
| panasonic-16 | png-encode | 354.924 | 288.664 | -18.7% | -27.8..-14.2% | 111,583,441 / 109,241,690 |
| panasonic-16 | png-decode | 434.083 | 433.645 | -0.1% | -1.0..+1.7% | 111,583,441 / 111,583,441 |
| panasonic-16 | psd-export | 2645.838 | 2249.281 | -15.0% | -15.6..-13.8% | 274,409,683 / 275,654,405 |
| panasonic-16 | psd-import | 371.291 | 374.777 | +0.9% | -1.1..+1.8% | 274,409,683 / 274,409,683 |
| panasonic-32 | save | 639.677 | 631.556 | -1.3% | -5.2..+1.8% | 252,231,671 / 252,231,663 |
| panasonic-32 | load-common | 1338.160 | 1326.936 | -0.8% | -1.1..-0.2% | 252,231,671 / 252,231,671 |
| panasonic-32 | psd-export | 2522.700 | 1352.144 | -46.4% | -49.6..-44.6% | 461,042,978 / 459,545,262 |
| panasonic-32 | psd-import | 356.077 | 310.425 | -12.8% | -20.2..-4.7% | 461,042,978 / 461,042,978 |
| canon-8 | save | 89.139 | 85.965 | -3.6% | -8.9..+2.7% | 26,151,631 / 26,151,203 |
| canon-8 | load-common | 219.711 | 219.088 | -0.3% | -0.3..+1.0% | 26,151,631 / 26,151,631 |
| canon-8 | png-encode | 179.944 | 80.123 | -55.5% | -60.8..-40.0% | 13,720,268 / 13,551,023 |
| canon-8 | png-decode | 69.704 | 70.040 | +0.5% | -0.4..+0.6% | 13,720,268 / 13,720,268 |
| canon-8 | psd-export | 1074.094 | 475.802 | -55.7% | -56.1..-55.4% | 56,209,597 / 56,608,401 |
| canon-8 | psd-import | 142.523 | 96.323 | -32.4% | -34.4..-26.9% | 56,209,597 / 56,209,597 |
| canon-16 | save | 185.898 | 183.472 | -1.3% | -2.8..-0.2% | 88,750,003 / 88,749,555 |
| canon-16 | load-common | 193.409 | 193.704 | +0.2% | -0.2..+0.8% | 88,750,003 / 88,750,003 |
| canon-16 | png-encode | 169.432 | 135.364 | -20.1% | -26.2..-15.4% | 58,279,529 / 57,568,207 |
| canon-16 | png-decode | 211.947 | 211.829 | -0.1% | -0.4..+0.5% | 58,279,529 / 58,279,529 |
| canon-16 | psd-export | 1295.076 | 1130.697 | -12.7% | -13.5..-9.9% | 140,673,004 / 141,105,829 |
| canon-32 | save | 340.781 | 334.426 | -1.9% | -5.3..+0.7% | 120,957,984 / 120,957,536 |
| canon-32 | load-common | 774.663 | 778.401 | +0.5% | -1.3..+8.5% | 120,957,984 / 120,957,984 |
| canon-32 | psd-export | 1028.651 | 647.483 | -37.1% | -40.5..-36.1% | 228,234,238 / 227,497,106 |
| canon-32 | psd-import | 177.500 | 146.373 | -17.5% | -18.5..-17.1% | 228,234,238 / 228,234,238 |
| layout-masks | save | 18.588 | 18.268 | -1.7% | -2.7..+0.6% | 433,949 / 433,924 |
| layout-masks | load-common | 16.099 | 16.080 | -0.1% | -1.5..+0.6% | 433,949 / 433,949 |
| layout-masks | png-encode | 3.911 | 2.557 | -34.6% | -36.0..-32.3% | 42,842 / 43,440 |
| layout-masks | png-decode | 0.781 | 0.767 | -1.8% | -2.5..+2.2% | 42,842 / 42,842 |
| layout-masks | psd-export | 56.043 | 46.949 | -16.2% | -18.5..-15.9% | 493,006 / 493,145 |
| layout-masks | psd-import | 12.324 | 12.334 | +0.1% | -4.8..+2.3% | 493,006 / 493,006 |
| layout-mask-index | save | 7.367 | 7.202 | -2.2% | -5.5..-0.7% | 344,892 / 344,746 |
| layout-mask-index | load-common | 6.601 | 6.602 | +0.0% | -2.8..+4.7% | 344,892 / 344,892 |
| layout-mask-index | png-encode | 11.520 | 5.971 | -48.2% | -49.5..-46.0% | 115,229 / 114,892 |
| layout-mask-index | png-decode | 1.451 | 1.444 | -0.5% | -4.2..+0.5% | 115,229 / 115,229 |
| layout-mask-index | psd-export | 19.135 | 12.721 | -33.5% | -34.0..-31.5% | 411,435 / 411,594 |
| layout-mask-index | psd-import | 3.362 | 2.721 | -19.1% | -21.3..-11.2% | 411,435 / 411,435 |

Every PNG encode decodes to identical source pixels, each PSD operation produces the same decoded layer/mask samples as main, and complete .pcraft documents compare equal at every depth. Compressed bytes may differ. The committed compatibility test independently inflates PNG IDAT and PSD ZIP/ZIP-prediction data using old miniz_oxide and checks .pcraft tiles plus thumbnails.

Local raw evidence and the shared harness: `.local/perf-real-261019/` in the repository checkout (ignored). This comparison supersedes the earlier synthetic performance table. Small synthetic unit fixtures remain correctness checks; ignored synthetic timing tests were not run.

## Earlier split validation

These checks passed when the split was prepared; they were not all rerun for this benchmark review. Current affected checks are listed above where production code changed.

- `cargo test -p photocraft-codecs -p photocraft-psd -p photocraft-format -p photocraft-io --locked`: passed (1956 passed, 9 ignored).
- `cargo clippy -p photocraft-codecs -p photocraft-psd -p photocraft-format -p photocraft-io --locked --all-targets -- -D warnings`: passed.
- `cargo xtask layers`: passed.
- `cargo xtask wasm`: passed.
- `cargo xtask test-corpus -p psd -p codecs -p io`: passed (1853 passed, 9 ignored).
- The compatibility test also passed with the main backend before measurements. To reproduce the main comparison, copy only this test and its miniz_oxide dev-dependency into a clean main worktree.
