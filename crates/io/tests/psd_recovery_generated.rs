//! PSD/PSB recovery regression tests using in-memory Rust-generated fixtures.
//!
//! No `.psd` file is checked into the repository and no Python generator runs.
//! Fixtures are created at the beginning of each test, so parallel test order
//! does not matter. See the ignored test for GUI-ready temporary files.

#[path = "support/recovery_psd.rs"]
mod recovery_psd;

use photocraft_io::{IoError, import, import_psd_for_recovery};
use photocraft_psd::PsdFile;

#[test]
fn valid_generated_psd_opens_normally() {
    let bytes = recovery_psd::make_psd(false);
    assert_eq!(bytes.len(), 74_986, "the fixture should retain the reference PSD layout");
    let result = import("valid-two-layers.psd", &bytes).expect("generated healthy PSD must import");
    assert!(!result.source_read_only);
    assert!(!result.preview_only);
    assert_eq!(result.document.layers.len(), 2);
}

#[test]
fn corrupt_generated_rle_channel_requires_opt_in_recovery() {
    let bytes = recovery_psd::make_psd(true);
    assert_eq!(bytes.len(), 74_986, "the fixture should retain the reference PSD layout");
    let (parsed, _) = PsdFile::from_bytes_and_merged(&bytes, |_| false).expect("the PSD container must stay valid");
    let layer = parsed.layers().iter().find(|layer| layer.name() == "BROKEN orange RLE").expect("generated upper layer exists");

    assert!(layer.decode_channel(0, parsed.header.depth, parsed.header.version).is_ok());
    assert!(layer.decode_channel(1, parsed.header.depth, parsed.header.version).is_err());
    assert!(layer.decode_channel(2, parsed.header.depth, parsed.header.version).is_ok());
    assert!(parsed.decode_merged().is_ok(), "the composite must remain recoverable");

    let error = import("corrupt-green-rle.psd", &bytes).unwrap_err();
    assert!(matches!(error, IoError::Unsupported(_)), "unexpected error: {error}");

    let recovered = import_psd_for_recovery("corrupt-green-rle.psd", &bytes).expect("explicit recovery must return readable layers");
    assert!(recovered.source_read_only, "recovery must not save over the source");
    assert!(!recovered.preview_only);
    assert!(!recovered.document.layers.is_empty());
    assert!(recovered.warnings.iter().any(|w| w.starts_with("PSD/PSB RECOVERY:")));
    assert!(recovered.warnings.iter().any(|w| w.contains("could not be decoded")));
}

/// Generate the same two PSDs on demand for manual testing in the GUI.
///
/// Run with:
/// cargo test -p photocraft-io --test psd_recovery_generated \
///   write_psds_for_manual_gui_test -- --ignored --nocapture
///
/// The files go to the system's temp folder, not into the repository.
#[test]
#[ignore = "writes temporary PSD fixtures only when manual GUI testing is requested"]
fn write_psds_for_manual_gui_test() {
    let output = std::env::temp_dir().join(format!("photocraft-psd-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&output).expect("create temporary fixture directory");

    for (name, corrupt) in [("valid-two-layers.psd", false), ("corrupt-green-rle.psd", true)] {
        let path = output.join(name);
        std::fs::write(&path, recovery_psd::make_psd(corrupt)).expect("write generated temporary PSD");
        println!("{}", path.display());
    }
}
