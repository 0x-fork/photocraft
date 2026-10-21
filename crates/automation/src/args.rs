//! The control and automation launch options, as clap types: `photocraft-cli mcp` and `serve`
//! flatten them, and the desktop app is to as well (#7071), so each flag is declared once.

use std::path::PathBuf;

use clap::Args;
use clap::builder::NonEmptyStringValueParser;

use crate::{AuthorizedWorkspace, AutomationError};

/// The control token, for a server or for a client of one.
#[derive(Args)]
pub struct ControlArgs {
    /// The control token (64 hex digits)
    #[arg(long, value_name = "64-HEX", value_parser = NonEmptyStringValueParser::new(), conflicts_with = "control_token_file")]
    pub control_token: Option<String>,
    /// A file holding the control token, used instead of --control-token (a server creates it
    /// when missing)
    #[arg(long, value_name = "PATH")]
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

/// A control port, for clap's `value_parser`: `photocraft-cli serve --port` and `mcp --bridge`, and
/// the desktop app's `--control` once it flattens these (#7071). Spaces around it are ignored, as the
/// desktop app's PHOTOCRAFT_CONTROL_PORT may carry them.
pub fn control_port(s: &str) -> Result<u16, NotAPort> {
    s.trim().parse().map_err(|_| NotAPort)
}

/// A [`control_port`] value that isn't a port.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected a port, 0-65535")]
pub struct NotAPort;

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
