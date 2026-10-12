//! The operating system's animation setting, for Preferences › Interface › Animation Effects ›
//! Use System Setting (`Services::system_animations`).
//!
//! - **Windows**: Settings › Accessibility › Visual effects › Animation effects, through
//!   `UISettings.AnimationsEnabled`. It answers for the session, so a Remote Desktop connection
//!   that turns animations off counts too.
//! - **macOS**: System Settings › Accessibility › Display › Reduce motion
//!   (`NSWorkspace.accessibilityDisplayShouldReduceMotion`).
//! - **Linux**: the settings portal: `reduced-motion`, then GNOME's `enable-animations` and KDE's
//!   `AnimationDurationFactor` as the portal passes them on. Its change signal keeps the value
//!   current, and it works inside a Flatpak sandbox.
//! - **Linux without the portal, FreeBSD and the other BSDs**: the desktop's setting where it is
//!   stored: KDE's `kdeglobals`, GNOME's or MATE's `enable-animations` (`gsettings`), then GTK's
//!   `gtk-enable-animations`. There is no change signal to follow, so it is read at start-up and
//!   again whenever the window gets the focus back.
//!
//! `None` means the platform gave no answer; the shell then keeps animations on.
//!
//! No `unsafe`: every OS call goes through a safe binding.

pub use platform::service;

/// Windows and macOS answer at once, so the setting is simply asked for again: at most this
/// often, and only on frames that run anyway (coming back from the system's settings is one).
#[cfg(any(target_os = "windows", target_os = "macos"))]
const POLL: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn polled(read: fn() -> Option<bool>) -> photocraft_ui_egui::SystemAnimationsFn {
    let last = std::cell::Cell::new(None::<(std::time::Instant, Option<bool>)>);
    Box::new(move |_| {
        if let Some((at, animations)) = last.get()
            && at.elapsed() < POLL
        {
            return animations;
        }
        let animations = read();
        last.set(Some((std::time::Instant::now(), animations)));
        animations
    })
}

#[cfg(target_os = "windows")]
mod platform {
    use photocraft_ui_egui::SystemAnimationsFn;

    pub fn service() -> Option<SystemAnimationsFn> {
        Some(super::polled(read))
    }

    pub(super) fn read() -> Option<bool> {
        windows::UI::ViewManagement::UISettings::new().and_then(|settings| settings.AnimationsEnabled()).ok()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use photocraft_ui_egui::SystemAnimationsFn;

    pub fn service() -> Option<SystemAnimationsFn> {
        Some(super::polled(read))
    }

    fn read() -> Option<bool> {
        Some(!objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion())
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
mod platform {
    pub fn service() -> Option<photocraft_ui_egui::SystemAnimationsFn> {
        None
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, OnceLock, mpsc};

    use photocraft_ui_egui::SystemAnimationsFn;

    /// Follows the desktop's setting on a worker thread, so a slow session bus or `gsettings`
    /// never holds up a frame. Until the first answer lands the setting is unknown.
    pub fn service() -> Option<SystemAnimationsFn> {
        let value = Arc::new(AtomicU8::new(super::code_of(None)));
        let wake: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
        let (refresh, refreshes) = mpsc::channel::<()>();
        let publisher = Publisher { value: Arc::clone(&value), wake: Arc::clone(&wake) };
        std::thread::Builder::new().name("animations-setting".into()).spawn(move || worker(&publisher, &refreshes)).ok()?;
        let focused = std::cell::Cell::new(true);
        Some(Box::new(move |ctx| {
            let _ = wake.set(ctx.clone());
            let now = ctx.input(|i| i.focused);
            let was = focused.replace(now);
            // Back from the desktop's settings, perhaps: the worker reads again unless it has
            // the portal's change signal (then nobody listens, and the send fails quietly).
            if now && !was {
                let _ = refresh.send(());
            }
            super::decode(value.load(Ordering::Relaxed))
        }))
    }

    fn worker(publisher: &Publisher, refreshes: &mpsc::Receiver<()>) {
        #[cfg(target_os = "linux")]
        if portal::follow(publisher) {
            return;
        }
        publisher.publish(desktop::read());
        for () in refreshes {
            publisher.publish(desktop::read());
        }
    }

    /// The worker's handle on the shared value. A change wakes the registered UI.
    struct Publisher {
        value: Arc<AtomicU8>,
        wake: Arc<OnceLock<egui::Context>>,
    }

    impl Publisher {
        fn publish(&self, animations: Option<bool>) {
            let code = super::code_of(animations);
            if self.value.swap(code, Ordering::Relaxed) != code
                && let Some(ctx) = self.wake.get()
            {
                ctx.request_repaint();
            }
        }
    }

    #[cfg(target_os = "linux")]
    mod portal {
        use zbus::zvariant::{OwnedValue, Value};

        use super::super::{duration_factor, enabled, reduced_motion};

        /// Whether animations are on according to a setting's value.
        type Reads = fn(&str) -> Option<bool>;

        /// The settings the portal may answer for and how each one's value reads, most general
        /// first: the cross-desktop one, then the two desktops' own that older portals pass on.
        const SETTINGS: [(&str, &str, Reads); 3] = [
            ("org.freedesktop.appearance", "reduced-motion", reduced_motion),
            ("org.gnome.desktop.interface", "enable-animations", enabled),
            ("org.kde.kdeglobals.KDE", "AnimationDurationFactor", duration_factor),
        ];

        /// Publishes the portal's answer, then every change to it. `false` when the portal has
        /// none: no session bus, no portal, or one that knows none of the settings.
        pub fn follow(publisher: &super::Publisher) -> bool {
            let Some(proxy) = crate::appearance::portal_proxy() else { return false };
            // Subscribe before the first read, so a change during start-up isn't lost.
            let signals = proxy.receive_signal("SettingChanged").ok();
            let Some(first) = read(&proxy) else { return false };
            publisher.publish(Some(first));
            for message in signals.into_iter().flatten() {
                let Ok((namespace, key, _)) = message.body().deserialize::<(String, String, OwnedValue)>() else {
                    continue;
                };
                // Read them all again: which setting answers first may have changed too.
                if SETTINGS.iter().any(|(ns, k, _)| *ns == namespace && *k == key) {
                    publisher.publish(read(&proxy));
                }
            }
            true
        }

        fn read(proxy: &zbus::blocking::Proxy<'_>) -> Option<bool> {
            SETTINGS.iter().find_map(|(namespace, key, animations)| {
                let value: OwnedValue = proxy.call("Read", &(*namespace, *key)).ok()?;
                animations(&text(value)?)
            })
        }

        /// A setting's value as the text a settings file would hold.
        fn text(value: OwnedValue) -> Option<String> {
            let mut value: Value<'_> = value.into();
            // The Settings.Read result is a variant; some portals nest another variant inside it.
            for _ in 0..4 {
                match value {
                    Value::Value(inner) => value = *inner,
                    Value::Bool(on) => return Some(on.to_string()),
                    Value::U32(n) => return Some(n.to_string()),
                    Value::I32(n) => return Some(n.to_string()),
                    Value::F64(n) => return Some(n.to_string()),
                    Value::Str(text) => return Some(text.to_string()),
                    _ => return None,
                }
            }
            None
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            fn owned(value: Value<'_>) -> OwnedValue {
                OwnedValue::try_from(value).unwrap()
            }

            #[test]
            fn portal_values_read_as_text() {
                let nested = Value::Value(Box::new(Value::Value(Box::new(Value::U32(1)))));
                assert_eq!(text(owned(nested)).as_deref(), Some("1"));
                assert_eq!(text(owned(Value::from(false))).as_deref(), Some("false"));
                assert_eq!(text(owned(Value::from("0.5"))).as_deref(), Some("0.5"));
                assert_eq!(text(owned(Value::from(vec![1u32, 2]))), None);
                // Each setting reads its own type of value, the cross-desktop one first.
                let [(_, motion, reduced), (_, _, on), (_, _, factor)] = SETTINGS;
                assert_eq!(motion, "reduced-motion");
                assert_eq!(reduced(&text(owned(Value::U32(1))).unwrap()), Some(false));
                assert_eq!(reduced(&text(owned(Value::U32(0))).unwrap()), Some(true));
                assert_eq!(on(&text(owned(Value::from(false))).unwrap()), Some(false));
                assert_eq!(factor(&text(owned(Value::from("0"))).unwrap()), Some(false));
                assert_eq!(factor(&text(owned(Value::F64(0.5))).unwrap()), Some(true));
            }
        }
    }

    mod desktop {
        use std::path::PathBuf;

        /// A settings file is a few kilobytes; a larger one isn't read.
        const MAX_CONFIG_BYTES: u64 = 1 << 20;

        pub fn read() -> Option<bool> {
            let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
            super::super::desktop_setting(&desktop, &config_file, &gsettings)
        }

        fn config_home() -> Option<PathBuf> {
            let dir = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
            dir("XDG_CONFIG_HOME").filter(|path| path.is_absolute()).or_else(|| dir("HOME").map(|home| home.join(".config")))
        }

        fn config_file(name: &str) -> Option<String> {
            use std::io::Read as _;
            let mut text = String::new();
            std::fs::File::open(config_home()?.join(name)).ok()?.take(MAX_CONFIG_BYTES).read_to_string(&mut text).ok()?;
            Some(text)
        }

        /// `gsettings get`, by absolute path (`/usr/local` and `/usr/pkg` are where the BSDs
        /// install it).
        fn gsettings(schema: &str, key: &str) -> Option<String> {
            let exe = ["/usr/bin/gsettings", "/bin/gsettings", "/usr/local/bin/gsettings", "/usr/pkg/bin/gsettings"]
                .into_iter()
                .find(|path| std::path::Path::new(path).is_file())?;
            let output = std::process::Command::new(exe).args(["get", schema, key]).output().ok()?;
            if !output.status.success() {
                return None;
            }
            String::from_utf8(output.stdout).ok()
        }
    }
}

/// `true` or `false` (GSettings, the portal), `1` or `0` (a GTK settings file): animations are on.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn enabled(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// KDE's animation speed factor: 0 turns animations off.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn duration_factor(value: &str) -> Option<bool> {
    value.trim().parse::<f64>().ok().filter(|factor| factor.is_finite()).map(|factor| factor > 0.0)
}

/// The portal's `reduced-motion`: 1 asks for less motion; 0 and the values it may grow don't.
#[cfg(any(test, target_os = "linux"))]
fn reduced_motion(value: &str) -> Option<bool> {
    value.trim().parse::<u32>().ok().map(|code| code != 1)
}

/// The last value of `key` in `[section]` of an INI-style file (KDE's config, GTK's settings).
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn ini_value<'a>(text: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut inside = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            inside = name == section;
        } else if inside && let Some((name, value)) = line.split_once('=') {
            // KDE marks an entry the user may not change as `Key[$i]=value`.
            let name = name.split_once('[').map_or(name, |(name, _)| name);
            if name.trim() == key {
                found = Some(value.trim());
            }
        }
    }
    found
}

/// The desktop's setting without the portal. `desktop` is `XDG_CURRENT_DESKTOP`, `file` reads a
/// file under the user's config directory and `gsettings` runs `gsettings get <schema> <key>`.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn desktop_setting(desktop: &str, file: &dyn Fn(&str) -> Option<String>, gsettings: &dyn Fn(&str, &str) -> Option<String>) -> Option<bool> {
    let running = |name: &str| desktop.split(':').any(|part| part.eq_ignore_ascii_case(name));
    let in_file = |name: &str, section: &str, key: &str, animations: fn(&str) -> Option<bool>| animations(ini_value(&file(name)?, section, key)?);
    if running("KDE")
        && let Some(animations) = in_file("kdeglobals", "KDE", "AnimationDurationFactor", duration_factor)
    {
        return Some(animations);
    }
    let schema = if running("MATE") { "org.mate.interface" } else { "org.gnome.desktop.interface" };
    let shell = gsettings(schema, "enable-animations").and_then(|out| enabled(&out));
    if shell == Some(false) {
        return shell;
    }
    // `true` may only be the schema's default on a desktop that doesn't use it. GTK's own file
    // is what a bare window manager's user writes, and what KDE keeps in step with its setting.
    ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"].iter().find_map(|name| in_file(name, "Settings", "gtk-enable-animations", enabled)).or(shell)
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn code_of(animations: Option<bool>) -> u8 {
    match animations {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    }
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn decode(code: u8) -> Option<bool> {
    match code {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_setting_reads_its_own_values() {
        for (s, animations) in
            [("true\n", Some(true)), ("false\n", Some(false)), ("1", Some(true)), ("0", Some(false)), ("FALSE", Some(false)), ("", None), ("maybe", None)]
        {
            assert_eq!(enabled(s), animations, "{s:?}");
        }
        for (s, animations) in [
            ("0", Some(false)),
            ("0.0", Some(false)),
            ("0.5", Some(true)),
            ("1", Some(true)),
            ("-1", Some(false)),
            ("", None),
            ("fast", None),
            ("NaN", None),
            ("inf", None),
        ] {
            assert_eq!(duration_factor(s), animations, "{s:?}");
        }
        // reduced-motion: only 1 asks for less motion; unknown values mean no preference.
        for (s, animations) in [("0", Some(true)), ("1", Some(false)), ("2", Some(true)), ("true", None), ("-1", None), ("", None)] {
            assert_eq!(reduced_motion(s), animations, "{s:?}");
        }
        // The shared value round-trips, and a stray code is "unknown".
        for animations in [None, Some(true), Some(false)] {
            assert_eq!(decode(code_of(animations)), animations);
        }
        assert_eq!(decode(9), None);
    }

    #[test]
    fn ini_values_come_from_their_own_section() {
        let kde = "[General]\nAnimationDurationFactor=3\n\n[KDE]\nSingleClick=false\n AnimationDurationFactor = 0 \n[KDE][Sub]\nAnimationDurationFactor=7\n";
        assert_eq!(ini_value(kde, "KDE", "AnimationDurationFactor"), Some("0"));
        assert_eq!(ini_value(kde, "KDE", "Missing"), None);
        assert_eq!(ini_value("[KDE]\nAnimationDurationFactor[$i]=0.5\n", "KDE", "AnimationDurationFactor"), Some("0.5"), "an immutable entry");
        assert_eq!(
            ini_value("[Settings]\ngtk-enable-animations=1\ngtk-enable-animations=0\n", "Settings", "gtk-enable-animations"),
            Some("0"),
            "the last one wins"
        );
        // Hostile text: no section, no `=`, stray brackets, non-ASCII.
        for junk in ["", "AnimationDurationFactor=0", "[KDE", "[KDE]\n=\n[]\n]]][[[\n", "[KDE]\nклавиша=значение\n\u{0}\u{feff}"] {
            assert_eq!(ini_value(junk, "KDE", "AnimationDurationFactor"), None, "{junk:?}");
        }
    }

    #[test]
    fn the_desktop_setting_is_read_where_each_desktop_keeps_it() {
        let files = |kde: Option<&'static str>, gtk: Option<&'static str>| {
            move |name: &str| match name {
                "kdeglobals" => kde.map(String::from),
                "gtk-3.0/settings.ini" => gtk.map(String::from),
                _ => None,
            }
        };
        let none = |_: &str, _: &str| -> Option<String> { None };
        let gnome = |on: &'static str| {
            move |schema: &str, key: &str| (schema == "org.gnome.desktop.interface" && key == "enable-animations").then(|| format!("{on}\n"))
        };
        let kde_off = Some("[KDE]\nAnimationDurationFactor=0\n");
        let gtk_off = Some("[Settings]\ngtk-enable-animations=false\n");

        // KDE Plasma: its own file, only when Plasma is the desktop running.
        assert_eq!(desktop_setting("KDE", &files(kde_off, None), &none), Some(false));
        assert_eq!(desktop_setting("KDE", &files(Some("[KDE]\nAnimationDurationFactor=0.5\n"), gtk_off), &none), Some(true));
        assert_eq!(desktop_setting("ubuntu:GNOME", &files(kde_off, None), &gnome("true")), Some(true), "a leftover kdeglobals is not GNOME's setting");
        // Plasma without the entry (animations at their default speed) falls through to GTK's file.
        assert_eq!(desktop_setting("KDE", &files(Some("[KDE]\nSingleClick=false\n"), gtk_off), &none), Some(false));
        // GNOME and the desktops that share its schema.
        assert_eq!(desktop_setting("GNOME", &files(None, None), &gnome("false")), Some(false));
        assert_eq!(desktop_setting("GNOME", &files(None, None), &gnome("true")), Some(true));
        // MATE keeps the same key in its own schema.
        let mate = |schema: &str, _: &str| (schema == "org.mate.interface").then(|| "false\n".to_string());
        assert_eq!(desktop_setting("MATE", &files(None, None), &mate), Some(false));
        assert_eq!(desktop_setting("GNOME", &files(None, None), &mate), None);
        // A bare window manager: GTK's settings file, which also outranks a schema default.
        assert_eq!(desktop_setting("sway", &files(None, gtk_off), &none), Some(false));
        assert_eq!(desktop_setting("", &files(None, gtk_off), &gnome("true")), Some(false));
        assert_eq!(desktop_setting("", &files(None, Some("[Settings]\ngtk-enable-animations=1\n")), &none), Some(true));
        // Nothing to read, or nothing readable: unknown, never a panic.
        assert_eq!(desktop_setting("", &files(None, None), &none), None);
        assert_eq!(
            desktop_setting("KDE:\u{0}::", &files(Some("\u{0}[[[=]]]"), Some("=\n[Settings]\ngtk-enable-animations=perhaps")), &|_, _| Some("\u{fffd}".into())),
            None
        );
    }

    /// The real setting of the machine the tests run on: whatever it is, reading it doesn't
    /// panic and the service passes it on (`--nocapture` shows it).
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reports_its_animation_setting() {
        let animations = platform::read();
        println!("UISettings.AnimationsEnabled: {animations:?}");
        let service = platform::service().expect("service");
        assert_eq!(service(&egui::Context::default()), animations);
    }
}
