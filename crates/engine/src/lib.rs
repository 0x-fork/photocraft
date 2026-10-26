//! The PhotoCraft engine façade: open documents, history, and the command registry.
//!
//! Every user-visible action is a command with a stable id (Photoshop-style, such as
//! `layer.newAdjustmentLayer.invert`) and JSON parameters. Every frontend goes through the same
//! [`Session::execute`] entry point: the egui UI, the CLI, the remote-control channel and the MCP
//! server. This is what makes the UI swappable and the app fully scriptable.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod actions_cmds;
pub mod actions_edit_cmds;
pub mod adjust_cmds;
pub mod adjust_params;
pub mod align_cmds;
mod allocation;
pub mod analysis_cmds;
pub mod artboard_cmds;
pub mod atn_cmds;
pub mod automate_cmds;
pub mod background_cmds;
pub mod brush_cmds;
pub mod brush_key_cmds;
pub mod brush_preset_cmds;
pub mod build_info;
mod canvas_geom;
mod channel_clip;
pub mod channel_cmds;
mod clip_group;
pub mod color_cmds;
pub mod color_to_alpha_cmds;
pub mod commands;
pub mod comps_cmds;
pub mod contour_lib_cmds;
mod crop_fill;
pub mod cutout_cmds;
pub mod display_color;
pub mod distort_cmds;
pub mod doc_path;
pub mod document_preset_cmds;
pub mod edit_cmds;
pub mod edit_menu_cmds;
pub mod eraser_cmds;
pub mod export_preset_cmds;
pub mod exr_cmds;
pub mod extra_cmds;
pub mod face_cmds;
pub mod file_cmds;
pub mod fill_cmds;
pub mod fill_key_cmds;
pub mod filters;
pub mod filters_ext;
pub mod float_cmds;
mod frame_cmds;
pub mod fx_view_cmds;
pub mod fx_visibility_cmds;
pub mod gallery_cmds;
pub mod gradient_fill_cmds;
pub mod group_view_cmds;
pub mod guide_layout;
pub mod hidden_target;
pub mod history_cmds;
pub mod history_menu_cmds;
pub mod history_select_cmds;
pub mod history_source_cmds;
pub mod image_cmds;
pub mod inspect;
pub mod installed_profiles;
pub mod jobs;
pub mod kys;
pub mod layer_copy_cmds;
pub mod layer_label_cmds;
pub mod layer_list_cmds;
pub mod layer_mask_props_cmds;
pub mod layer_menu_cmds;
pub mod layer_multi_cmds;
pub mod layer_nav_cmds;
pub mod layer_style;
pub mod layer_xmp_cmds;
pub mod lens_cmds;
pub mod link_watch;
pub mod magnetic_cmds;
pub mod mask_view_cmds;
mod migrate_cmds;
pub mod mode_cmds;
pub mod multichannel_cmds;
pub mod new_layer;
pub mod notes_cmds;
pub mod paint_cmds;
mod param_check;
mod path_edit_cmds;
pub mod pattern_cmds;
pub mod pattern_fill_cmds;
pub mod perspective_crop_cmds;
pub mod photo_cmds;
pub mod pick_cmds;
mod pixels;
pub mod plugin_cmds;
pub mod prefs;
pub mod preset_import_cmds;
pub mod preset_store;
pub mod presets;
pub mod print_cmds;
pub mod proof_sim;
pub mod rasterize_style_cmds;
pub mod redeye_cmds;
pub mod render_cmds;
pub mod retouch_cmds;
pub mod sample_cmds;
pub mod sampler_cmds;
pub mod select_extra_cmds;
pub mod select_mask_edits;
pub mod selection_cmds;
pub mod shell_commands;
pub mod shortcut_sets;
pub mod slice_cmds;
pub mod smart_cmds;
pub mod smartselect_cmds;
pub mod snap;
pub mod solid_fill_cmds;
pub mod stamp_cmds;
pub mod style_asl_cmds;
pub mod swatch_cmds;
pub mod symmetry_cmds;
mod timeline_cmds;
pub mod transform_cmds;
mod trap_cmds;
pub mod type_area_cmds;
pub mod type_caret_cmds;
pub mod type_cmds;
pub mod type_extra_cmds;
pub mod type_mask_cmds;
pub mod type_spell_cmds;
pub mod type_styles_cmds;
pub mod updates_cmds;
mod variables_cmds;
pub use variables_cmds::{export_data_sets_with, import_data_sets_text};
pub mod vector_cmds;
mod video_cmds;
pub use video_cmds::render_video_with;
pub mod vp_cmds;
pub mod warp_cmds;
pub mod web_cmds;
mod wia_cmds;

use std::sync::Arc;

use photocraft_doc::{Document, LayerId};
use photocraft_ops::{History, LayerTarget};
use serde_json::Value;

pub use commands::{CommandSpec, command_specs};
pub use photocraft_doc as doc;
pub use photocraft_paint as paint;
pub use photocraft_paint::BrushSettings;

mod doc_state;
mod error;
mod session;

pub use doc_state::{CopiedStyle, DocState, PRECHECKED, ToolState};
pub use error::{EngineError, Result};
pub(crate) use session::fix_selection;
pub use session::move_item;
use session::restore_target;

/// The pixels of a raster layer (typically one a command just created), as an error instead
/// of a panic if the layer has none.
pub(crate) fn pixels_mut(l: &mut photocraft_doc::Layer) -> Result<&mut photocraft_raster::Surface> {
    let id = l.id;
    l.surface_mut().ok_or_else(|| EngineError::Other(format!("layer {id:?} has no pixels")))
}

/// The active document's active layer, for `enabled` predicates.
pub(crate) fn active_layer_of(s: &Session) -> std::result::Result<&photocraft_doc::Layer, String> {
    let d = s.active().ok_or("no document open")?;
    d.active_layer.and_then(|id| d.doc.layer(id)).ok_or_else(|| "no active layer".into())
}

#[derive(Default)]
pub struct Session {
    docs: Vec<DocState>,
    active: Option<usize>,
    /// Monotonic counter, bumped whenever a different document becomes active (including a
    /// switch away and back inside one pass). UI work that must not cross documents (control
    /// screenshots, automation input) binds to it.
    active_generation: u64,
    /// The document id [`Self::active_generation`] was last bumped for.
    active_doc_for_generation: Option<photocraft_doc::DocId>,
    pub tools: ToolState,
    /// Log of executed commands (for action recording and debugging).
    pub journal: Vec<(String, Value)>,
    /// Coalescing key of the command being executed.
    coalesce_request: Option<String>,
    /// Pixels copied with Edit › Copy / Cut (shared by all documents, like Photoshop).
    pub clipboard: Option<edit_cmds::Clip>,
    /// Layer › Layer Style › Copy Layer Style: the effects and the Blending Options
    /// ([`CopiedStyle`]: blend mode, fill opacity, Advanced Blending, Blend If, channels).
    pub style_clipboard: Option<CopiedStyle>,
    /// Shape path context menu: copied vector fill and stroke styles.
    pub path_fill_clipboard: Option<photocraft_doc::Fill>,
    pub path_stroke_clipboard: Option<photocraft_doc::ShapeStroke>,
    /// Colour management: proofing state, monitor profile, display transforms.
    pub color: color_cmds::ColorState,
    /// Open Edit Contents documents and the smart objects they update.
    pub smart_links: Vec<smart_cmds::SmartLink>,
    /// Type › Save Default Type Styles: character and paragraph style new type layers start from.
    pub type_defaults: Option<(photocraft_doc::text::CharStyle, photocraft_doc::text::ParagraphStyle)>,
    /// Quick Mask Options (colour, opacity, colour indicates) used when entering Quick Mask.
    pub quick_mask_options: channel_cmds::QuickMaskOptions,
    /// Colour channel the running command may change (a single colour channel is targeted).
    color_restrict: Option<usize>,
    /// Smart object whose filter mask the running command edits (`"target":"filterMask"`): each
    /// edit re-renders it, in the same history step.
    filter_mask_layer: Option<LayerId>,
    /// Edit › Preferences, keyboard shortcuts, menu and toolbar customisation (see `prefs`).
    pub prefs: prefs::PrefsStore,
    /// Release checks use an injected platform transport; no network I/O in the engine.
    pub updates: updates_cmds::Updates,
    /// Edit menu state: Fade source, custom shape library (see `edit_menu_cmds`).
    pub edit_state: edit_menu_cmds::EditState,
    /// The pattern library (Window › Patterns; see `pattern.*`).
    pub patterns: pattern_cmds::PatternLibrary,
    /// Image › Analysis: Measurement Log and Select Data Points (see `analysis_cmds`).
    pub analysis: analysis_cmds::AnalysisState,
    /// Window › Gradients, Patterns (groups), Styles, Shapes, Tool Presets and Clone Source.
    pub presets: presets::PresetState,
    /// File menu state: Lock Slices, Image Assets, last Print / Save for Web settings, script
    /// event log (see `automate_cmds`).
    pub file_menu: automate_cmds::FileMenuState,
    /// Persistent brush preset store (desktop only; `None` keeps presets session-only, as in
    /// headless and test sessions). See `preset_store`. The same store holds the Actions list.
    pub preset_store: Option<preset_store::PresetStore>,
    /// Window › Actions. The list persists with the preset store when one is attached.
    pub actions: actions_cmds::ActionState,
    /// Gate for every command the session runs, including the ones a command runs on its own
    /// behalf and `actions.play` steps. Untrusted sessions (MCP, the control channel) install
    /// the same check a top-level request sees. `None` runs everything, which is what a local
    /// UI and `photocraft-cli run` do.
    pub authorize: Option<fn(&str, &serde_json::Value) -> Result<()>>,
    /// A further route for File › Print when neither `lp` nor a CUPS server over IPP is
    /// reachable (the XDG Print portal in a Flatpak); see `print_cmds::Spooler`.
    pub print_spooler: Option<print_cmds::Spooler>,
    /// Background jobs (see [`jobs`]).
    jobs: jobs::Jobs,
    /// Color Sampler points dropped by the last canvas-size change (crop, Canvas Size, Image Size,
    /// rotation), for the status message. Read with [`Session::take_samplers_removed`].
    samplers_removed: usize,
    /// The canvas-geometry map the next [`Session::edit`] applies to the Color Sampler points (crop,
    /// Canvas Size, Image Size, rotation set it just before their edit). Consumed by the edit.
    sampler_map: Option<photocraft_geom::Affine>,
    /// The same edit's map in edge coordinates, for the painting symmetry (Image Rotation maps the
    /// samplers' pixel centres instead). Consumed by the edit.
    symmetry_map: Option<photocraft_geom::Affine>,
    /// Each open document's linked smart objects' files: size and modification time when last
    /// seen ([`link_watch`]).
    pub link_stamps: std::collections::HashMap<(u64, String), link_watch::Stamp>,
    /// The stamps being taken on a worker thread ([`Session::start_link_scan`]).
    pub link_scan: Option<link_watch::LinkScan>,
    /// Select Subject's last result, reused while the pixels are unchanged (#2865).
    subject_cache: smartselect_cmds::SubjectCache,
    /// Object Selection's Object Finder results (#4087), shared by the hover preview and the
    /// click that commits it; valid while the pixels are unchanged.
    pub object_finder: smartselect_cmds::ObjectFinderCache,
    /// A finished live stroke offered to the `paint.stroke` / `paint.pencil` that commits it
    /// ([`Session::offer_live_stroke`]).
    pub(crate) live_commit: Option<brush_cmds::LiveCommit>,
    /// Documents a background open decoded but held back until the user answers the profile
    /// prompt ([`ColorState::open_question`](color_cmds::ColorState::open_question)): `(id,
    /// document, source read-only)`. The shell takes them with [`Session::take_held_open`].
    held_opens: Vec<(u64, Document, bool)>,
    next_held_open: u64,
}

#[cfg(test)]
mod fill_layer_mode_tests;
#[cfg(test)]
mod pattern_mode_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod fx_mode_tests;
