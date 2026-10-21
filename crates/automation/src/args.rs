//! The control and automation launch options, as clap types: `photocraft-cli mcp` and `serve`
//! flatten them, and the desktop app is to as well (#7071), so each flag and its
//! environment variable are declared once. A flag wins over its variable.
//!
//! Parse with [`blank_env_is_unset`] applied to the arguments, so an empty variable counts as unset,
//! and pass a parse error through [`name_env_vars`], so a bad value names the variable it came from.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::NonEmptyStringValueParser;
use clap::error::{ContextKind, ContextValue};
use clap::{Arg, Args, Command};

use crate::{AuthorizedWorkspace, AutomationError};

/// The control token, for a server or for a client of one.
#[derive(Args)]
pub struct ControlArgs {
    /// The control token (64 hex digits)
    #[arg(long, value_name = "64-HEX", env = "PHOTOCRAFT_CONTROL_TOKEN", hide_env_values = true, value_parser = NonEmptyStringValueParser::new(), conflicts_with = "control_token_file")]
    pub control_token: Option<String>,
    /// A file holding the control token, used instead of --control-token (a server creates it
    /// when missing)
    #[arg(long, value_name = "PATH", env = "PHOTOCRAFT_CONTROL_TOKEN_FILE")]
    pub control_token_file: Option<PathBuf>,
}

/// The folders automation may touch. Without a root, that direction of access is denied. No
/// variable stands in for them here: one exported for the desktop app must not widen headless
/// `photocraft-cli` access (#1551); the desktop app adds its own.
#[derive(Args)]
pub struct AutomationArgs {
    /// The folder automation may read from (none without it)
    #[arg(long, value_name = "DIR")]
    pub automation_read_root: Option<PathBuf>,
    /// The folder automation may write to (none without it)
    #[arg(long, value_name = "DIR")]
    pub automation_write_root: Option<PathBuf>,
}

impl AutomationArgs {
    /// The roots, opened as capabilities.
    pub fn workspace(&self) -> Result<AuthorizedWorkspace, AutomationError> {
        AuthorizedWorkspace::new(self.automation_read_root.as_deref(), self.automation_write_root.as_deref())
    }
}

/// A control port, for clap's `value_parser`: `photocraft --control`, `photocraft-cli serve --port`
/// and PHOTOCRAFT_CONTROL_PORT, which may carry spaces.
pub fn control_port(s: &str) -> Result<u16, NotAPort> {
    s.trim().parse().map_err(|_| NotAPort)
}

/// A [`control_port`] value that isn't a port.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected a port, 0-65535")]
pub struct NotAPort;

/// For `Command::mut_args`: an empty or blank variable counts as unset, so `PHOTOCRAFT_CONTROL_PORT=`
/// turns a fallback off without `unset` (#2491). clap would pass the empty value to the parser.
pub fn blank_env_is_unset(arg: Arg) -> Arg {
    let blank = arg.get_env().and_then(std::env::var_os).is_some_and(|v| v.to_str().is_some_and(|v| v.trim().is_empty()));
    if blank { arg.env(None::<&str>) } else { arg }
}

/// clap names the flag when a value from its variable is bad or conflicts with another, as if the
/// flag had been given: name the variable instead when the flag isn't among `args` (without the
/// binary name; a first one naming a subcommand selects whose arguments to look in) (#2491).
pub fn name_env_vars(mut error: clap::Error, cmd: &Command, args: &[OsString]) -> clap::Error {
    // `--flag value` or `--flag=value`.
    let given = |long: &str| args.iter().filter_map(|a| a.to_str()?.strip_prefix("--")?.strip_prefix(long)).any(|r| r.is_empty() || r.starts_with('='));
    let sub = args.first().and_then(|name| cmd.find_subcommand(name)).unwrap_or(cmd);
    for kind in [ContextKind::InvalidArg, ContextKind::PriorArg] {
        let Some(ContextValue::String(shown)) = error.get(kind) else { continue };
        let Some(arg) = sub.get_arguments().find(|a| a.to_string() == *shown) else { continue };
        let (Some(long), Some(var)) = (arg.get_long(), arg.get_env()) else { continue };
        if !given(long) {
            error.insert(kind, ContextValue::String(var.to_string_lossy().into_owned()));
        }
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_port_accepts_valid_numbers() {
        assert_eq!(control_port("50494"), Ok(50494));
        assert_eq!(control_port(" 8080 "), Ok(8080));
        assert_eq!(control_port("0"), Ok(0));
    }

    #[test]
    fn control_port_rejects_anything_else() {
        for bad in ["78787", "-1", "", "x"] {
            assert_eq!(control_port(bad), Err(NotAPort), "{bad:?}");
        }
    }
}
