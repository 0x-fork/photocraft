//! Keep editable stops separate from the dense stops used by the compositors.

use photocraft_doc::effects::GradientInterpolation;
use photocraft_doc::{Color, Gradient};
use serde_json::Value;

use super::{color_of, f_of, overlay_gradient};

fn edit_endpoint(stop: Option<&mut (f32, Color)>, params: &Value, key: &str) {
    if let (Some(stop), Some(color)) = (stop, color_of(params, key)) {
        // The dialog displays hex colours. Opening it must not quantize the saved source.
        if !params.get(key).is_some_and(Value::is_string) || color.to_rgba8() != stop.1.to_rgba8() {
            stop.1 = color;
        }
    }
}

pub(super) fn overlay(gradient: &mut Gradient, interpolation: &mut Option<GradientInterpolation>, params: &Value) {
    let requested = params.get("method").and_then(Value::as_str).filter(|name| photocraft_io::GradientMethod::from_name(name).is_some());
    if interpolation.is_none() && requested.is_none() {
        overlay_gradient(gradient, params);
        return;
    }
    let previous = interpolation.clone();
    let source = interpolation.get_or_insert_with(|| GradientInterpolation {
        method: requested.unwrap_or("classic").to_ascii_lowercase(),
        smoothness: 100.0,
        stops: gradient.stops.clone(),
    });
    if let Some(method) = requested {
        source.method = method.to_ascii_lowercase();
    }
    if let Some(smoothness) = f_of(params, "smoothness").filter(|n| n.is_finite()) {
        source.smoothness = smoothness.clamp(0.0, 100.0);
    }
    edit_endpoint(source.stops.first_mut(), params, "from");
    edit_endpoint(source.stops.last_mut(), params, "to");
    if previous.as_ref() != Some(source)
        && let Some(method) = photocraft_io::GradientMethod::from_name(&source.method)
    {
        // Missing midpoint entries mean 50 %, including for multi-stop source gradients.
        gradient.stops = photocraft_io::bake_gradient(source.stops.clone(), &[], source.smoothness / 100.0, method);
    }
    // Geometry edits must not replace just the ends of the baked curve.
    let mut geometry = params.clone();
    if let Some(object) = geometry.as_object_mut() {
        for key in ["method", "smoothness", "from", "to"] {
            object.remove(key);
        }
    }
    overlay_gradient(gradient, &geometry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changing_method_keeps_all_source_stops_and_transparency() {
        let mut gradient = Gradient {
            stops: vec![(0.0, Color::rgb(1.0, 0.0, 0.0)), (0.3, Color::rgb(0.0, 1.0, 0.0)), (1.0, Color::rgb(0.0, 0.0, 1.0))],
            opacity_stops: vec![(0.0, 0.25), (1.0, 0.8)],
            align: false,
            offset: (0.2, -0.1),
            ..Gradient::default()
        };
        let original = gradient.clone();
        let mut source = None;
        for method in ["classic", "smooth", "perceptual", "linear", "classic"] {
            overlay(&mut gradient, &mut source, &json!({"method": method}));
            let source = source.as_ref().unwrap();
            assert_eq!(source.method, method);
            assert_eq!(source.stops, original.stops);
            let expected = photocraft_io::bake_gradient(original.stops.clone(), &[0.5, 0.5], 1.0, photocraft_io::GradientMethod::from_name(method).unwrap());
            assert_eq!(gradient.stops, expected);
            assert_eq!(gradient.opacity_stops, original.opacity_stops);
            assert_eq!(gradient.align, original.align);
            assert_eq!(gradient.offset, original.offset);
        }
    }

    #[test]
    fn colour_edit_rebakes_the_curve_and_geometry_edits_leave_it_exact() {
        let mut gradient = Gradient::default();
        let mut source = None;
        overlay(&mut gradient, &mut source, &json!({"method": "smooth", "from": "#ff0000", "to": "#0000ff"}));
        let before = gradient.stops.clone();
        overlay(&mut gradient, &mut source, &json!({"from": "#00ff00", "angle": 45}));
        assert_ne!(gradient.stops, before);
        let expected = photocraft_io::bake_gradient(
            vec![(0.0, Color::rgb(0.0, 1.0, 0.0)), (1.0, Color::rgb(0.0, 0.0, 1.0))],
            &[0.5],
            1.0,
            photocraft_io::GradientMethod::Smooth,
        );
        assert_eq!(gradient.stops, expected);
        overlay(&mut gradient, &mut source, &json!({"angle": 90, "reverse": true, "scale": 75}));
        assert_eq!(gradient.stops, expected);
        assert_eq!(gradient.angle, 90.0);
        assert!(gradient.reverse);
        assert_eq!(gradient.scale, 0.75);
    }

    #[test]
    fn reopening_does_not_quantize_source_colours_or_rebake() {
        let mut gradient = Gradient::default();
        let mut source = None;
        overlay(&mut gradient, &mut source, &json!({"from": [0.123456, 0.234567, 0.345678], "method": "perceptual"}));
        let before = (gradient.clone(), source.clone());
        let [r, g, b, _] = source.as_ref().unwrap().stops[0].1.to_rgba8();
        overlay(&mut gradient, &mut source, &json!({"from": format!("#{r:02x}{g:02x}{b:02x}"), "to": "#ffffff", "method": "perceptual", "smoothness": 100}));
        assert_eq!((gradient, source), before);
    }

    #[test]
    fn old_gradients_are_unchanged_until_a_method_is_chosen() {
        let mut gradient = Gradient::default();
        gradient.stops.insert(1, (0.4, Color::rgb(0.1, 0.9, 0.2)));
        let original = gradient.clone();
        let mut source = None;
        overlay(&mut gradient, &mut source, &json!({"method": "custom", "opacity": 50}));
        assert_eq!(gradient, original);
        assert!(source.is_none());
    }
}
