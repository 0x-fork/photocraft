use super::*;
use photocraft_psd::atn::{AtnAction, AtnEvent, AtnFile, AtnItem};
use photocraft_psd::descriptor::{Descriptor, Id, ReferenceItem, UnicodeString, Value as D};
use serde_json::json;

fn item(event: &str, label: &str, d: Option<Descriptor>) -> AtnItem {
    let event = match event.as_bytes() {
        [a, b, c, dd] => AtnEvent::Code([*a, *b, *c, *dd]),
        _ => AtnEvent::Name(event.into()),
    };
    AtnItem { event, dictionary: label.into(), descriptor: d, ..Default::default() }
}

fn b64(bytes: &[u8]) -> String {
    photocraft_paint::tile::b64_encode(bytes)
}

/// A set with a mapped filter, an adjustment layer, selection steps, a layer by name, a Stop,
/// a Play, a tool selection (no document effect) and an event PhotoCraft has no command for.
fn sample() -> AtnFile {
    let levels = desc("Lvls").with(
        "Adjs",
        D::List(vec![D::Descriptor(
            desc("LvlA")
                .with("Chnl", channel_ref(None))
                .with("Inpt", D::List(vec![long(10.0), long(240.0)]))
                .with("Gmm ", D::Double(1.2))
                .with("Otpt", D::List(vec![long(0.0), long(250.0)])),
        )]),
    );
    let make_levels = desc("null").with("null", class_ref("AdjL")).with("Usng", D::Descriptor(desc("AdjL").with("Type", D::Descriptor(levels))));
    let select_all =
        desc("null").with("null", D::Reference(vec![ReferenceItem::Property { class: class("Chnl"), key: Id::new("fsel") }])).with("T   ", enm("Ordn", "Al  "));
    let select_layer = desc("null").with("null", name_ref("Lyr ", "Shadow")).with("MkVs", D::Boolean(false));
    let tool = desc("null").with("null", class_ref("MvTl"));
    let play = desc("null").with("null", D::Reference(vec![ReferenceItem::Name { class: class("Actn"), name: UnicodeString::new_nul("Finish") }]));
    let stop = desc("null").with("Msge", text("Check the result")).with("Cntn", D::Boolean(true));
    let fancy = desc("null").with("prompt", text("a cat")).with("DocI", long(5.0));
    let mut blur = item("GsnB", "Gaussian Blur", Some(desc("null").with("Rds ", px(2.5))));
    blur.with_dialog = true;
    let mut off = item("Fltn", "Flatten Image", None);
    off.enabled = false;
    AtnFile {
        name: UnicodeString::new_nul("Test Set"),
        expanded: true,
        actions: vec![
            AtnAction {
                name: UnicodeString::new_nul("Soften"),
                function_key: 3,
                items: vec![
                    blur,
                    item("make", "Make", Some(make_levels)),
                    item("setd", "Set", Some(select_all)),
                    item("select", "Select", Some(select_layer)),
                    item("slct", "Select", Some(tool)),
                    item("stop", "Stop", Some(stop)),
                    item("syntheticFill", "Generative Fill", Some(fancy)),
                    item("play", "Play", Some(play)),
                    off,
                ],
                ..Default::default()
            },
            AtnAction { name: UnicodeString::new_nul("Finish"), items: vec![item("Invs", "Inverse", None)], ..Default::default() },
        ],
        warnings: Vec::new(),
    }
}

#[test]
fn load_maps_events_keeps_flags_and_plays() {
    let mut s = Session::new();
    let bytes = atn::write(&sample()).unwrap();
    let r = s.execute("actions.load", json!({"data": b64(&bytes)})).unwrap();
    assert_eq!(r["set"], "Test Set");
    assert_eq!(r["actions"], json!(["Soften", "Finish"]));
    assert_eq!(r["steps"], 10);
    assert_eq!(r["functionKeys"], json!([{"action": "Soften", "key": "F3"}]));
    assert_eq!(s.prefs().shortcuts.get("actions.play:Soften").map(String::as_str), Some("F3"));
    let unsupported: Vec<(String, String)> =
        r["unsupported"].as_array().unwrap().iter().map(|u| (u["event"].as_str().unwrap().into(), u["name"].as_str().unwrap().into())).collect();
    assert_eq!(unsupported, [("slct".to_string(), "Select".to_string()), ("syntheticFill".to_string(), "Generative Fill".to_string())]);

    let a = &s.actions.list[0];
    assert_eq!(a.set.as_deref(), Some("Test Set"));
    let ids: Vec<&str> = a.steps.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "filter.blur.gaussianBlur",
            "layer.newAdjustmentLayer.levels",
            "select.all",
            "layer.select",
            PHOTOSHOP_STEP,
            STOP_MESSAGE,
            PHOTOSHOP_STEP,
            "actions.play",
            "layer.flattenImage"
        ]
    );
    assert_eq!(a.steps[0].1, json!({"radius": 2.5}));
    assert_eq!(a.steps[1].1, json!({"inBlack": 10, "inWhite": 240, "gamma": 1.2, "outBlack": 0, "outWhite": 250}));
    assert_eq!(a.steps[3].1, json!({"layer": "Shadow"}));
    assert_eq!(a.steps[5].1, json!({"message": "Check the result", "allowContinue": true}));
    assert_eq!(a.steps[6].1["event"], "syntheticFill");
    assert_eq!(a.steps[7].1, json!({"action": "Finish"}));
    assert_eq!(a.dialogs.iter().copied().collect::<Vec<_>>(), [0], "the dialog toggle is kept");
    assert_eq!(a.disabled.iter().copied().collect::<Vec<_>>(), [4, 8], "tool selections are switched off, as is the step the file had off");
    assert_eq!(s.actions.list[1].steps, [("select.inverse".to_string(), json!({}))]);

    // Playback: the mapped steps run, the layer is found by name, and the kept Photoshop step
    // stops the action with its name.
    s.execute("file.new", json!({"width": 16, "height": 16})).unwrap();
    s.execute("layer.new.layer", json!({"name": "Shadow"})).unwrap();
    s.execute("layer.new.layer", json!({"name": "Top"})).unwrap();
    let played = s.execute("actions.play", json!({"action": "Soften"})).unwrap();
    assert_eq!(played["ran"], 5, "{played}");
    assert_eq!(played["failed"]["step"], 6);
    assert!(played["failed"]["error"].as_str().unwrap().contains("Generative Fill"), "{played}");
    let d = s.active().unwrap();
    assert_eq!(d.doc.layer(d.active_layer.unwrap()).unwrap().name, "Shadow");
    assert!(d.doc.walk().iter().any(|(_, _, l)| l.name.starts_with("Levels")));

    // Loading the same set again replaces it; a different set with the same action names
    // gets unique names and its Play steps follow.
    s.execute("actions.load", json!({"data": b64(&bytes)})).unwrap();
    assert_eq!(s.actions.list.len(), 2);
    let r = s.execute("actions.load", json!({"data": b64(&bytes), "set": "Other"})).unwrap();
    assert_eq!(r["actions"], json!(["Soften 2", "Finish 2"]));
    assert_eq!(s.actions.list[2].steps[7].1, json!({"action": "Finish 2"}));
    assert_eq!(s.actions.list[2].set.as_deref(), Some("Other"));
}

#[test]
fn save_round_trips_through_photoshop_events() {
    let mut s = Session::new();
    let bytes = atn::write(&sample()).unwrap();
    s.execute("actions.load", json!({"data": b64(&bytes)})).unwrap();
    let first = s.actions.list.clone();
    let r = s.execute("actions.save", json!({"set": "Test Set"})).unwrap();
    assert_eq!(r["actions"], json!(["Soften", "Finish"]));
    assert_eq!(r["skipped"], json!([]));
    let saved = photocraft_paint::tile::b64_decode(r["data"].as_str().unwrap()).unwrap();
    let file = atn::parse(&saved).unwrap();
    assert_eq!(file.name.to_string_lossy(), "Test Set");
    assert_eq!(file.actions[0].function_key, 3);
    let events: Vec<String> = file.actions[0].items.iter().map(|i| i.event.id()).collect();
    assert_eq!(events, ["GsnB", "Mk  ", "setd", "slct", "slct", "Stop", "syntheticFill", "Ply ", "Fltn"]);
    assert!(file.actions[0].items[0].with_dialog);
    assert!(!file.actions[0].items[8].enabled);
    // The kept Photoshop step is written back byte for byte.
    assert_eq!(file.actions[0].items[6].descriptor, sample().actions[0].items[6].descriptor);
    // Play names the set too, as Photoshop does.
    assert_eq!(get_ref(file.actions[0].items[7].descriptor.as_ref().unwrap(), "null").unwrap().len(), 2);

    // Loading what was saved gives the same actions.
    let mut t = Session::new();
    t.execute("actions.load", json!({"data": b64(&saved)})).unwrap();
    assert_eq!(t.actions.list, first);

    // A step with no Photoshop equivalent fails unless skipped.
    s.actions.list.push(Action::new("Ours", vec![("filter.render.clouds".into(), json!({}))]));
    let e = s.execute("actions.save", json!({"action": "Ours"})).unwrap_err().to_string();
    assert!(e.contains("filter.render.clouds"), "{e}");
    let r = s.execute("actions.save", json!({"action": "Ours", "skipUnsupported": true})).unwrap();
    assert_eq!(r["skipped"], json!([{"action": "Ours", "step": 0, "id": "filter.render.clouds"}]));
    assert_eq!(r["set"], "Ours");
}

#[test]
fn every_mapped_command_exports_and_imports_back() {
    let cases: Vec<(&str, Value)> = vec![
        ("filter.blur.gaussianBlur", json!({"radius": 3.5})),
        ("filter.sharpen.unsharpMask", json!({"amount": 120, "radius": 1.5, "threshold": 3})),
        ("filter.noise.addNoise", json!({"amount": 12.5, "distribution": "gaussian", "monochromatic": true, "seed": 7})),
        ("filter.noise.median", json!({"radius": 2})),
        ("filter.blur.motionBlur", json!({"angle": 45, "distance": 20})),
        ("filter.other.offset", json!({"horizontal": 10, "vertical": -5, "undefinedAreas": "wrap"})),
        ("filter.blur.blur", json!({})),
        (
            "image.adjustments.levels",
            json!({"inBlack": 5, "inWhite": 250, "gamma": 0.9, "outBlack": 0, "outWhite": 255, "red": {"inBlack": 2, "inWhite": 255, "gamma": 1, "outBlack": 0, "outWhite": 255}}),
        ),
        ("image.adjustments.curves", json!({"points": [[0, 0], [128, 140], [255, 255]], "blue": [[0, 10], [255, 245]]})),
        (
            "image.adjustments.hueSaturation",
            json!({"colorize": false, "hue": 10, "saturation": -20, "lightness": 0, "reds": {"hue": 5, "saturation": 0, "lightness": 0, "range": [315, 345, 15, 45]}}),
        ),
        ("image.adjustments.brightnessContrast", json!({"brightness": 10, "contrast": -5, "legacy": false})),
        ("image.adjustments.threshold", json!({"level": 100})),
        ("image.adjustments.posterize", json!({"levels": 4})),
        ("image.adjustments.invert", json!({})),
        ("image.adjustments.colorBalance", json!({"shadows": [1, 2, 3], "midtones": [0, 0, 0], "highlights": [-5, 0, 5], "preserveLuminosity": true})),
        ("image.adjustments.vibrance", json!({"vibrance": 30, "saturation": 5})),
        ("image.adjustments.exposure", json!({"exposure": 0.5, "offset": 0.01, "gamma": 1.1})),
        ("image.adjustments.photoFilter", json!({"color": "#ec8a00", "density": 25, "preserveLuminosity": true})),
        (
            "image.adjustments.blackWhite",
            json!({"reds": 40, "yellows": 60, "greens": 40, "cyans": 60, "blues": 20, "magentas": 80, "tint": true, "tintColor": "#e4d5b0"}),
        ),
        ("image.adjustments.channelMixer", json!({"red": [100, 0, 0, 0], "green": [0, 90, 10, 0], "blue": [0, 0, 100, 5], "monochrome": false})),
        ("image.adjustments.selectiveColor", json!({"method": "absolute", "reds": [10, 0, -5, 0], "neutrals": [0, 0, 0, 3]})),
        ("image.adjustments.desaturate", json!({})),
        ("image.autoTone", json!({})),
        ("image.imageSize", json!({"width": 800, "height": 600, "resolution": 72, "resample": "bicubicSharper"})),
        ("image.canvasSize", json!({"width": 100, "height": 50, "relative": true, "anchor": "topLeft", "extensionColor": "white"})),
        ("image.imageRotation.90cw", json!({})),
        ("image.rotation.arbitrary", json!({"angle": 12.5, "direction": "ccw"})),
        ("edit.transform.flipHorizontal", json!({})),
        ("image.imageRotation.flipCanvasVertical", json!({})),
        ("image.crop", json!({"x": 10, "y": 20, "width": 100, "height": 50, "deleteCroppedPixels": true})),
        ("image.trim", json!({"basedOn": "transparent", "top": true, "bottom": true, "left": false, "right": true})),
        ("image.mode.grayscale", json!({})),
        ("image.mode.bits16", json!({})),
        ("layer.duplicate", json!({"name": "copy"})),
        ("image.duplicate", json!({"mergedOnly": true})),
        ("layer.new.layer", json!({"name": "Ink", "opacity": 0.5, "mode": "Multiply"})),
        ("layer.new.group", json!({"name": "G"})),
        ("layer.groupLayers", json!({})),
        ("layer.new.layerViaCopy", json!({})),
        ("layer.newFillLayer.solidColor", json!({"color": "#336699"})),
        ("layer.newAdjustmentLayer.hueSaturation", json!({})),
        ("layer.newAdjustmentLayer.threshold", json!({"level": 77})),
        ("file.new", json!({"width": 640, "height": 480, "resolution": 72, "mode": "rgb", "depth": 8, "background": "transparent", "name": "Doc"})),
        ("select.all", json!({})),
        ("select.deselect", json!({})),
        ("select.loadSelection", json!({"channel": "transparency"})),
        ("select.loadSelection", json!({"channel": "Alpha 1"})),
        ("select.rect", json!({"x": 1, "y": 2, "width": 30, "height": 40})),
        ("select.rect", json!({"x": 1, "y": 2, "width": 30, "height": 40, "ellipse": true})),
        ("layer.setProps", json!({"name": "Renamed", "opacity": 0.4, "blend": "Linear Light"})),
        ("layer.setProps", json!({"layer": "Shadow", "visible": false})),
        ("layer.setLabelColor", json!({"color": "red"})),
        ("layer.select", json!({"layer": "Shadow", "mode": "add"})),
        ("layer.selectTop", json!({})),
        ("layer.selectBelow", json!({})),
        ("layer.addBelowToSelection", json!({})),
        ("layer.addAboveToSelection", json!({})),
        ("layer.delete", json!({})),
        ("edit.clear", json!({})),
        ("layer.hideLayers", json!({"layer": "Background"})),
        ("layer.showLayers", json!({})),
        ("layer.translate", json!({"dx": 3, "dy": -4})),
        ("layer.arrange.sendToBack", json!({})),
        ("layer.flattenImage", json!({})),
        ("layer.stampVisible", json!({})),
        ("layer.mergeVisible", json!({})),
        ("layer.matting.defringe", json!({"width": 2})),
        ("layer.align.leftEdges", json!({"to": "canvas"})),
        ("select.inverse", json!({})),
        ("select.modify.feather", json!({"radius": 4})),
        ("select.modify.contract", json!({"radius": 1})),
        ("edit.fill", json!({"contents": "color", "color": "#ff0080", "opacity": 50, "mode": "Multiply", "preserveTransparency": true})),
        ("edit.fill", json!({"contents": "foreground", "opacity": 100, "mode": "Normal"})),
        ("edit.stroke", json!({"width": 3, "location": "outside", "opacity": 100, "mode": "Normal", "color": "#000000", "preserveTransparency": false})),
        ("edit.copyMerged", json!({})),
        ("edit.pasteSpecial.pasteInPlace", json!({})),
        ("edit.paste", json!({})),
        ("edit.purge.histories", json!({})),
        ("tools.defaultColors", json!({})),
        ("edit.convertToProfile", json!({"profile": "display-p3", "intent": "perceptual", "bpc": false})),
        ("edit.assignProfile", json!({"profile": "srgb"})),
        ("file.close", json!({})),
        ("file.save", json!({})),
        ("actions.play", json!({"action": "Other", "set": "S"})),
        (STOP_MESSAGE, json!({"message": "Hi", "allowContinue": false})),
    ];
    for (id, params) in &cases {
        let item = export_item(id, params, "S").unwrap_or_else(|| panic!("{id} does not export"));
        let e = event_for(&item.event).unwrap_or_else(|| panic!("{id} exported as an unknown event {}", item.event.id()));
        match (e.from)(item.descriptor.as_ref().unwrap()) {
            Mapped::Steps(steps) => {
                assert_eq!(steps.len(), 1, "{id}: {steps:?}");
                assert_eq!(steps[0].0, *id, "{id} came back as another command");
                let mut back = steps[0].1.clone();
                normalise(&mut back);
                if *id == "actions.play" {
                    back["set"] = json!("S");
                }
                assert_eq!(back, *params, "{id}");
            }
            Mapped::Unsupported { .. } => panic!("{id} does not import back"),
        }
    }
    // Every command the table claims to export has a case above.
    let covered: std::collections::BTreeSet<&str> = cases.iter().map(|(id, _)| *id).collect();
    let mut missing: Vec<&str> = EVENTS.iter().flat_map(|e| e.ids.iter().copied()).filter(|id| !covered.contains(id)).collect();
    missing.sort_unstable();
    let allowed = [
        "edit.transform.flipVertical",
        "edit.transform.rotate180",
        "edit.transform.rotate90Ccw",
        "edit.transform.rotate90Cw",
        "image.autoColor",
        "image.autoContrast",
        "image.imageRotation.180",
        "image.imageRotation.90ccw",
        "image.imageRotation.flipCanvasHorizontal",
        "image.mode.bits32",
        "image.mode.bits8",
        "image.mode.cmyk",
        "image.mode.lab",
        "image.mode.multichannel",
        "image.mode.rgb",
        "layer.align.bottomEdges",
        "layer.align.horizontalCenters",
        "layer.align.rightEdges",
        "layer.align.topEdges",
        "layer.align.verticalCenters",
        "layer.arrange.bringForward",
        "layer.arrange.bringToFront",
        "layer.arrange.sendBackward",
        "layer.mergeLayers",
        "layer.new.layerViaCut",
        "layer.newAdjustmentLayer.blackWhite",
        "layer.newAdjustmentLayer.brightnessContrast",
        "layer.newAdjustmentLayer.channelMixer",
        "layer.newAdjustmentLayer.colorBalance",
        "layer.newAdjustmentLayer.curves",
        "layer.newAdjustmentLayer.exposure",
        "layer.newAdjustmentLayer.invert",
        "layer.newAdjustmentLayer.levels",
        "layer.newAdjustmentLayer.photoFilter",
        "layer.newAdjustmentLayer.posterize",
        "layer.newAdjustmentLayer.selectiveColor",
        "layer.newAdjustmentLayer.vibrance",
        "layer.rasterize.layer",
        "layer.renameLayer",
        "layer.selectAbove",
        "layer.selectBottom",
        "layer.setAdjustment",
        "select.allLayers",
        "select.deselectLayers",
        "select.grow",
        "select.modify.border",
        "select.modify.expand",
        "select.modify.smooth",
        "select.reselect",
        "select.similar",
        "tools.swapColors",
        "edit.cut",
        "edit.copy",
        "edit.purge.all",
        "edit.purge.clipboard",
        "edit.purge.undo",
        "file.newFromClipboard",
        "file.revert",
        "image.adjustments.equalize",
        "image.revealAll",
        "filter.blur.blurMore",
        "filter.blur.boxBlur",
        "filter.blur.radialBlur",
        "filter.blur.smartBlur",
        "filter.blur.surfaceBlur",
        "filter.noise.despeckle",
        "filter.noise.dustAndScratches",
        "filter.other.highPass",
        "filter.other.maximum",
        "filter.other.minimum",
        "filter.sharpen.sharpen",
        "filter.sharpen.sharpenEdges",
        "filter.sharpen.sharpenMore",
    ];
    let unexpected: Vec<&str> = missing.iter().copied().filter(|id| !allowed.contains(id)).collect();
    assert!(unexpected.is_empty(), "add round-trip cases for {unexpected:?}");
    // Every command the table names exists.
    // (Save is the desktop shell's: recorded by it, replayed by it, written by Batch.)
    for id in EVENTS.iter().flat_map(|e| e.ids.iter()) {
        assert!(crate::commands::find(id).is_some() || crate::actions_cmds::shell_save_command(id), "the .atn table names an unknown command `{id}`");
    }
}

#[test]
fn unsupported_shapes_become_photoshop_steps_not_errors() {
    // Percent image size, a Paint Bucket fill, a move in points, an unknown adjustment class.
    let items = vec![
        item("ImgS", "Image Size", Some(desc("null").with("Wdth", pct(200.0)))),
        item("Fl  ", "Fill", Some(desc("null").with("From", D::Descriptor(desc("Pnt "))).with("Usng", enm("FlCn", "FrgC")))),
        item(
            "move",
            "Move",
            Some(desc("null").with("null", target_ref("Lyr ")).with(
                "T   ",
                D::Descriptor(desc("Ofst").with("Hrzn", D::UnitFloat { unit: *b"#Rlt", value: 3.0 }).with("Vrtc", D::UnitFloat { unit: *b"#Rlt", value: 1.0 })),
            )),
        ),
        item(
            "make",
            "Make",
            Some(desc("null").with("null", class_ref("AdjL")).with("Usng", D::Descriptor(desc("AdjL").with("Type", D::Descriptor(desc("GdMp")))))),
        ),
        item("GsnB", "Gaussian Blur", None),
    ];
    let file = AtnFile {
        name: UnicodeString::new_nul("Odd"),
        expanded: false,
        actions: vec![AtnAction { name: UnicodeString::new_nul("A"), items, ..Default::default() }],
        warnings: vec![],
    };
    let imp = import(&atn::write(&file).unwrap()).unwrap();
    let ids: Vec<&str> = imp.actions[0].steps.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, [PHOTOSHOP_STEP, PHOTOSHOP_STEP, PHOTOSHOP_STEP, PHOTOSHOP_STEP, "filter.blur.gaussianBlur"]);
    assert_eq!(imp.unsupported.len(), 4);
    assert!(imp.actions[0].disabled.is_empty(), "steps with a document effect stay on so playback reports them");
}

#[test]
fn step_commands_and_bad_params_fail_gracefully() {
    let mut s = Session::new();
    for p in [
        json!({}),
        json!({"path": ""}),
        json!({"path": "/nonexistent/set.atn"}),
        json!({"data": 42}),
        json!({"data": "%%%"}),
        json!({"data": b64(b"")}),
        json!({"data": b64(&[0, 0, 0, 12, 0, 0, 0, 0])}),
        json!({"data": b64(&atn::write(&sample()).unwrap()[..40])}),
        json!({"data": b64(&atn::write(&sample()).unwrap()), "set": ""}),
        json!({"data": b64(&atn::write(&sample()).unwrap()), "set": 3}),
        json!({"data": b64(&atn::write(&sample()).unwrap()), "replace": "yes"}),
    ] {
        assert!(s.execute("actions.load", p.clone()).is_err(), "{p}");
    }
    assert!(s.actions.list.is_empty());
    for p in [json!({}), json!({"set": "Nope"}), json!({"action": 3}), json!({"path": 7}), json!({"skipUnsupported": "no"}), json!({"set": ""})] {
        assert!(s.execute("actions.save", p.clone()).is_err(), "{p}");
    }
    s.actions.list.push(Action::new("One", vec![("select.all".into(), json!({}))]));
    assert!(s.execute("actions.save", json!({"path": ""})).is_err());
    assert!(s.execute("actions.save", json!({})).unwrap()["data"].is_string());

    assert!(s.execute(STOP_MESSAGE, json!({"message": "halt"})).unwrap_err().to_string().contains("halt"));
    assert_eq!(s.execute(STOP_MESSAGE, json!({"message": "go on", "allowContinue": true})).unwrap()["continued"], true);
    assert!(s.execute(STOP_MESSAGE, json!({"message": 5, "allowContinue": "x"})).is_err());
    for p in [json!({}), json!({"event": 1}), json!({"event": "GsnB", "name": "Gaussian Blur", "descriptor": "not base64"})] {
        assert!(s.execute(PHOTOSHOP_STEP, p.clone()).is_err(), "{p}");
    }
    // A Photoshop step with a damaged descriptor is left out of a save rather than crashing it.
    s.actions.list.push(Action::new("Broken", vec![(PHOTOSHOP_STEP.into(), json!({"event": "GsnB", "name": "Blur", "descriptor": b64(b"\x00\x00\x00")}))]));
    assert!(s.execute("actions.save", json!({"action": "Broken"})).is_err());
    assert_eq!(s.execute("actions.save", json!({"action": "Broken", "skipUnsupported": true})).unwrap()["skipped"].as_array().unwrap().len(), 1);
}

#[test]
fn save_writes_the_file_and_the_set_of_the_selected_action() {
    let mut s = Session::new();
    s.execute("actions.load", json!({"data": b64(&atn::write(&sample()).unwrap())})).unwrap();
    s.actions.list.push(Action::new("Loose", vec![("select.all".into(), json!({}))]));
    let dir = std::env::temp_dir().join(format!("pc-atn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Saved.atn");
    let r = s.execute("actions.save", json!({"action": "Finish", "path": path.to_string_lossy()})).unwrap();
    assert_eq!((r["set"].as_str(), r["actions"].as_array().map(Vec::len)), (Some("Test Set"), Some(1)));
    let file = atn::parse(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(file.actions.len(), 1);
    assert_eq!(file.actions[0].name.to_string_lossy(), "Finish");
    // Without a set, everything goes out under the default name.
    let r = s.execute("actions.save", json!({})).unwrap();
    assert_eq!((r["set"].as_str(), r["actions"].as_array().map(Vec::len)), (Some(DEFAULT_SET), Some(3)));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn layer_param_accepts_a_name() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    s.execute("layer.new.layer", json!({"name": "A"})).unwrap();
    s.execute("layer.new.layer", json!({"name": "B"})).unwrap();
    s.execute("layer.select", json!({"layer": "A"})).unwrap();
    let d = s.active().unwrap();
    assert_eq!(d.doc.layer(d.active_layer.unwrap()).unwrap().name, "A");
    assert!(s.execute("layer.select", json!({"layer": "Nope"})).is_err());
    assert!(s.execute("layer.select", json!({"layer": true})).is_err());
    assert!(s.execute("layer.select", json!({"layer": -1})).is_err());
}

#[test]
fn colours_convert_both_ways() {
    assert_eq!(color_hex(&desc("RGBC").with("Rd  ", D::Double(255.0)).with("Grn ", D::Double(127.6)).with("Bl  ", D::Double(0.0))).as_deref(), Some("#ff8000"));
    assert_eq!(color_hex(&desc("HSBC").with("H   ", ang(0.0)).with("Strt", D::Double(0.0)).with("Brgh", D::Double(100.0))).as_deref(), Some("#ffffff"));
    assert_eq!(color_hex(&desc("HSBC").with("H   ", ang(120.0)).with("Strt", D::Double(100.0)).with("Brgh", D::Double(100.0))).as_deref(), Some("#00ff00"));
    assert_eq!(color_hex(&desc("Grsc").with("Gry ", D::Double(100.0))).as_deref(), Some("#000000"));
    assert_eq!(color_hex(&desc("CMYC")), None);
    assert_eq!(color_desc("#12ab9F").map(|d| color_hex(&d)), Some(Some("#12ab9f".into())));
    assert_eq!(color_desc("12ab9f"), None);
    assert_eq!(color_desc("#12ab9"), None);
    assert_eq!(color_desc("#ééééé"), None);
}
