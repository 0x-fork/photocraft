//! The desktop app's command line: [`parse_launch_args`] reads the arguments over the
//! environment defaults `main` collects, [`USAGE`] is `photocraft --help`, and every usage error
//! (an unknown option, a missing or bad value) exits with status 2 (issues #701, #6429, #6431).
/// Parse a `--control` / `PHOTOCRAFT_CONTROL_PORT` value. An unparseable port is an error that
/// names the value (issue #701): silently running with no control server leaves a launcher or
/// agent unable to tell a typo from a successful grant.
pub(crate) fn parse_control_port(value: &str, source: &str) -> Result<u16, String> {
    value.trim().parse().map_err(|_| format!("{source}: `{value}` is not a valid port (expected a number from 0 to 65535)"))
}

/// Process exit status for the collected usage errors (a bad `--control` /
/// `PHOTOCRAFT_CONTROL_PORT`, an unknown option): `None` when there are none, else 2 (a
/// command-line usage error), never 0.
pub(crate) fn usage_exit_code(errors: &[String]) -> Option<i32> {
    if errors.is_empty() { None } else { Some(2) }
}

/// `photocraft --help`. Keep in step with the options `main` accepts and the module docs.
pub(crate) const USAGE: &str = "\
Usage: photocraft [options] [--] [files…]

Opens PhotoCraft with the given files.

Options:
  -h, --help                       Print this help and exit
      --version                    Print the version, commit and build date and exit
      --safe-gpu                   Start with the CPU renderer for this launch (after a graphics driver crash)
      --in-window-menus            macOS: keep the menus inside the window instead of the menu bar
      --new-instance               Windows and Linux: start a separate PhotoCraft instead of opening
                                   the files in the one already running
      --present-mode <mode>        How frames reach the display: fifo, autovsync or novsync
      --frame-latency <n>          Frames queued for the display: 1 to 3
      --stroke-trace <stderr|file.csv>
                                   Time each brush stroke's frames
      --control <port>             Start the localhost JSON-lines control server on this port
      --control-token <64-hex>     Control server token (default: a new random one, printed)
      --control-token-file <path>  Read the control server token from this file, or create it
                                   with a new one
      --automation-read-root <dir> Directory the control server may read files from
      --automation-write-root <dir>
                                   Directory the control server may write files to
  --                               Treat every later argument as a file, even one starting with `-`

A bad --present-mode, --frame-latency or --stroke-trace value is reported and ignored.
Environment: PHOTOCRAFT_CONTROL_PORT, PHOTOCRAFT_CONTROL_TOKEN, PHOTOCRAFT_CONTROL_TOKEN_FILE,
PHOTOCRAFT_AUTOMATION_READ_ROOT, PHOTOCRAFT_AUTOMATION_WRITE_ROOT, PHOTOCRAFT_PRESENT_MODE,
PHOTOCRAFT_FRAME_LATENCY, PHOTOCRAFT_STROKE_TRACE, PHOTOCRAFT_IN_WINDOW_MENUS, PHOTOCRAFT_NEW_INSTANCE,
PHOTOCRAFT_CONFIG_DIR.
";

/// Print to standard output for `--help` / `--version`. Unlike `println!`, never panics when
/// stdout is closed or a pipe (`photocraft --help | head -1`).
pub(crate) fn print_stdout(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// Everything the desktop launcher takes from the command line, on top of the environment
/// defaults handed in to `parse_launch_args`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchArgs {
    pub(crate) control_port: Option<u16>,
    pub(crate) control_token: Option<String>,
    pub(crate) control_token_file: Option<std::path::PathBuf>,
    pub(crate) automation_read_root: Option<std::path::PathBuf>,
    pub(crate) automation_write_root: Option<std::path::PathBuf>,
    pub(crate) files: Vec<String>,
    /// Arguments that are not valid Unicode (issue #1108): a file manager can hand over a Latin-1
    /// file name on Linux, and `std::env::args()` would panic before any window opened. Paths are
    /// reported in a notice once the app is up; a control token is a usage error like a bad port.
    pub(crate) unreadable_paths: Vec<String>,
    pub(crate) safe_gpu: bool,
    pub(crate) new_instance: bool,
    /// Options that apply only to the process they're given to; a launch with any keeps its own.
    pub(crate) launch_options: Vec<&'static str>,
    pub(crate) present_mode_arg: Option<String>,
    pub(crate) frame_latency_arg: Option<String>,
    pub(crate) stroke_trace_arg: Option<String>,
    pub(crate) in_window_menus: bool,
    /// `--help` / `-h`: print [`USAGE`] and exit 0; parsing stops there.
    pub(crate) help: bool,
    /// `--version`: print the version and exit 0; parsing stops there.
    pub(crate) version: bool,
    /// Usage errors (a bad or missing control/automation value, an unknown option); a non-empty
    /// list fails the launch with exit status 2 (issues #701, #6429, #6431).
    pub(crate) errors: Vec<String>,
}

/// Parse the desktop launch arguments over the environment defaults in `out`. A control or
/// automation flag with no value is a usage error naming the flag (issue #6429), never a silent
/// fall back to a generated token or to no root: like `--control` with a bad port, it exits 2.
/// So is an unknown option (#6431); a file whose name starts with `-` goes after `--`.
pub(crate) fn parse_launch_args(args: impl Iterator<Item = std::ffi::OsString>, mut out: LaunchArgs) -> LaunchArgs {
    let mut args = args;
    // After `--`, every argument is a file, even one that starts with `-`.
    let mut only_files = false;
    while let Some(a) = args.next() {
        if only_files {
            match a.into_string() {
                Ok(s) => out.files.push(s),
                Err(raw) => out.unreadable_paths.push(raw.to_string_lossy().into_owned()),
            }
            continue;
        }
        match a.to_str() {
            Some("--") => only_files = true,
            Some("--help" | "-h") => {
                out.help = true;
                break;
            }
            Some("--control") => match args.next() {
                Some(value) => match parse_control_port(&value.to_string_lossy(), "--control") {
                    Ok(port) => out.control_port = Some(port),
                    Err(error) => out.errors.push(error),
                },
                None => out.errors.push("--control: missing port value (expected `--control <port>`)".to_string()),
            },
            Some("--control-token") => match args.next().map(std::ffi::OsString::into_string) {
                Some(Ok(token)) => out.control_token = Some(token),
                Some(Err(raw)) => out.errors.push(format!("--control-token: value is not valid Unicode (`{}`)", raw.to_string_lossy())),
                None => out.errors.push("--control-token: missing token value (expected `--control-token <token>`)".to_string()),
            },
            Some("--control-token-file") => match args.next() {
                Some(value) => out.control_token_file = Some(std::path::PathBuf::from(value)),
                None => out.errors.push("--control-token-file: missing file path value (expected `--control-token-file <path>`)".to_string()),
            },
            Some("--automation-read-root") => {
                out.launch_options.push("--automation-read-root");
                match args.next() {
                    Some(value) => out.automation_read_root = Some(std::path::PathBuf::from(value)),
                    None => out.errors.push("--automation-read-root: missing directory value (expected `--automation-read-root <dir>`)".to_string()),
                }
            }
            Some("--automation-write-root") => {
                out.launch_options.push("--automation-write-root");
                match args.next() {
                    Some(value) => out.automation_write_root = Some(std::path::PathBuf::from(value)),
                    None => out.errors.push("--automation-write-root: missing directory value (expected `--automation-write-root <dir>`)".to_string()),
                }
            }
            Some("--safe-gpu") => {
                out.launch_options.push("--safe-gpu");
                out.safe_gpu = true;
            }
            Some(flag @ ("--present-mode" | "--frame-latency" | "--stroke-trace")) => {
                out.launch_options.push(match flag {
                    "--present-mode" => "--present-mode",
                    "--frame-latency" => "--frame-latency",
                    _ => "--stroke-trace",
                });
                let flag = flag.to_string();
                match args.next().map(std::ffi::OsString::into_string) {
                    Some(Ok(v)) => match flag.as_str() {
                        "--present-mode" => out.present_mode_arg = Some(v),
                        "--frame-latency" => out.frame_latency_arg = Some(v),
                        _ => out.stroke_trace_arg = Some(v),
                    },
                    // A bad value is reported and ignored; it never fails the launch.
                    _ => eprintln!("photocraft: {flag}: missing or unreadable value; ignored"),
                }
            }
            Some("--in-window-menus") => {
                out.launch_options.push("--in-window-menus");
                out.in_window_menus = true;
            }
            Some("--new-instance") => out.new_instance = true,
            Some("--version") => {
                out.version = true;
                break;
            }
            // Old macOS passes a process serial number when launched from Finder.
            Some(s) if s.starts_with("-psn_") => {}
            // A typo such as `--safegpu`, or `--help` spelt another way, must not open the window
            // and then fail to open the flag as a file (#6431).
            Some(s) if s.starts_with('-') => out.errors.push(format!("unknown option `{s}` (put `--` before a file name that starts with `-`)")),
            Some(s) => out.files.push(s.to_owned()),
            None => out.unreadable_paths.push(a.to_string_lossy().into_owned()),
        }
    }
    out
}

#[cfg(test)]
mod control_port_tests {
    use super::{parse_control_port, usage_exit_code};

    #[test]
    fn control_port_errors_exit_non_zero() {
        assert_eq!(usage_exit_code(&[]), None);
        let errors = vec![parse_control_port("nope", "--control").unwrap_err()];
        assert_eq!(usage_exit_code(&errors), Some(2));
    }

    #[test]
    fn control_port_accepts_valid_numbers() {
        assert_eq!(parse_control_port("50494", "--control"), Ok(50494));
        assert_eq!(parse_control_port(" 8080 ", "--control"), Ok(8080));
        assert_eq!(parse_control_port("0", "PHOTOCRAFT_CONTROL_PORT"), Ok(0));
    }

    #[test]
    fn control_port_rejects_unparseable_values_naming_them() {
        // Out of range, a following flag eaten as the value, and an empty value all fail,
        // naming the offending value in the message.
        let err = parse_control_port("78787", "--control").unwrap_err();
        assert!(err.contains("78787"), "{err}");
        assert!(parse_control_port("--safe-gpu", "--control").is_err());
        assert!(parse_control_port("", "--control").is_err());
    }
}

#[cfg(test)]
mod launch_args_tests {
    use super::{LaunchArgs, parse_launch_args, usage_exit_code};
    use std::ffi::OsString;

    fn parse(values: &[&str]) -> LaunchArgs {
        parse_launch_args(values.iter().map(OsString::from), LaunchArgs::default())
    }

    #[test]
    fn flags_without_a_value_are_usage_errors() {
        // Issue #6429: a missing value must fail the launch, not silently fall back to a
        // generated token or to no root.
        for flag in ["--control", "--control-token", "--control-token-file", "--automation-read-root", "--automation-write-root"] {
            let parsed = parse(&[flag]);
            assert_eq!(parsed.errors.len(), 1, "{flag}: {:?}", parsed.errors);
            assert!(parsed.errors[0].starts_with(flag), "{}", parsed.errors[0]);
            assert!(parsed.errors[0].contains("missing"), "{}", parsed.errors[0]);
            assert_eq!(usage_exit_code(&parsed.errors), Some(2));
        }
    }

    #[test]
    fn missing_value_does_not_clear_an_environment_default() {
        // `--automation-read-root` with no value must not drop a root set through the env var.
        let defaults = LaunchArgs { automation_read_root: Some("/env/read".into()), automation_write_root: Some("/env/write".into()), ..Default::default() };
        let parsed = parse_launch_args(["--automation-read-root"].iter().map(OsString::from), defaults);
        assert_eq!(parsed.automation_read_root.as_deref(), Some(std::path::Path::new("/env/read")));
        assert_eq!(parsed.automation_write_root.as_deref(), Some(std::path::Path::new("/env/write")));
        assert_eq!(parsed.errors.len(), 1, "{:?}", parsed.errors);
    }

    #[test]
    fn values_are_parsed_and_no_errors_remain() {
        let parsed = parse(&[
            "--control",
            "50494",
            "--control-token",
            "abcd",
            "--control-token-file",
            "/tmp/token",
            "--automation-read-root",
            "/r",
            "--automation-write-root",
            "/w",
            "--safe-gpu",
            "--in-window-menus",
            "--new-instance",
            "--present-mode",
            "mailbox",
            "photo.psd",
        ]);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.control_port, Some(50494));
        assert_eq!(parsed.control_token.as_deref(), Some("abcd"));
        assert_eq!(parsed.control_token_file.as_deref(), Some(std::path::Path::new("/tmp/token")));
        assert_eq!(parsed.automation_read_root.as_deref(), Some(std::path::Path::new("/r")));
        assert_eq!(parsed.automation_write_root.as_deref(), Some(std::path::Path::new("/w")));
        assert!(parsed.safe_gpu);
        assert!(parsed.in_window_menus);
        assert!(parsed.new_instance);
        assert_eq!(parsed.present_mode_arg.as_deref(), Some("mailbox"));
        assert_eq!(parsed.launch_options, vec!["--automation-read-root", "--automation-write-root", "--safe-gpu", "--in-window-menus", "--present-mode"]);
        assert_eq!(parsed.files, vec!["photo.psd".to_string()]);
    }

    #[test]
    fn version_stops_parsing() {
        let parsed = parse(&["--version", "--control"]);
        assert!(parsed.version);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    }

    #[test]
    fn help_stops_parsing_and_double_dash_ends_options() {
        let parsed = parse(&["-h", "--safegpu", "--control"]);
        assert!(parsed.help);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);

        let parsed = parse(&["--safegpu", "--", "--help", "-x"]);
        assert!(!parsed.help);
        assert_eq!(parsed.errors.len(), 1, "{:?}", parsed.errors);
        assert!(parsed.errors[0].contains("unknown option `--safegpu`"), "{}", parsed.errors[0]);
        assert_eq!(parsed.files, vec!["--help".to_string(), "-x".to_string()]);
    }
}
