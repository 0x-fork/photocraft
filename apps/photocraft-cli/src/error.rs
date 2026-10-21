use std::path::PathBuf;

use photocraft_automation::AutomationError;

use crate::actions::ActionsError;

/// Why a subcommand failed: printed as `error: {e}`, exit 1.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    #[error(transparent)]
    Automation(#[from] AutomationError),
    #[error(transparent)]
    Engine(#[from] photocraft_engine::EngineError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Actions(#[from] ActionsError),
    /// A required argument is missing: `convert needs <in> <out>`.
    #[error("{sub} needs {what}")]
    Needs { sub: &'static str, what: &'static str },
    #[error("bad {flag} `{value}`: expected {expected}")]
    BadValue { flag: &'static str, value: String, expected: &'static str },
    #[error("bad --port `{0}`")]
    BadPort(String),
    #[error("bad --new JSON: {0}")]
    BadNew(#[source] serde_json::Error),
    #[error("bad --params JSON for `{id}`: {source}")]
    BadParams { id: String, source: serde_json::Error },
    #[error("{}: {source}", path.display())]
    ReadDir { path: PathBuf, source: std::io::Error },
    #[error("--params must follow a --cmd")]
    ParamsBeforeCmd,
    #[error("--new: {0}")]
    New(#[source] AutomationError),
    #[error("`{id}`: {source}")]
    Command { id: String, source: AutomationError },
    /// A step inside an action the command played failed ([`photocraft_engine::actions_cmds::nested_failure`]).
    #[error("`{id}`: {message}")]
    NestedFailure { id: String, message: String },
    #[error("{0} is the input file, so the result would replace the original: choose another output, or pass --in-place to overwrite it")]
    OutIsInput(String),
    #[error("--out {} is the --in folder, so the results would replace the originals: choose another --out folder, or pass --in-place to overwrite them", .0.display())]
    OutIsIn(PathBuf),
    /// Another input already wrote this output ([`photocraft_engine::file_cmds::OutputClaims::check`]).
    #[error("{0}")]
    OutputClaimed(String),
    #[error("{0} file(s) failed")]
    FilesFailed(usize),
}

pub(crate) type Result<T = (), E = CliError> = std::result::Result<T, E>;
