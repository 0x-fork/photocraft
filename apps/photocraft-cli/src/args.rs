//! The command line, as clap types. Values stay strings: the handlers check them, with their own
//! messages (#7071). A flag a subcommand doesn't take is a usage error, so a typo such as
//! `--fromat` can't be silently ignored (#423). A value flag never takes the next flag as its
//! value: a value starting with `-` is written `--out=-image.png` (or `./-image.png`); negative
//! numbers are kept where they mean something.

use clap::builder::NonEmptyStringValueParser;
use clap::error::ErrorKind;
use clap::{ArgAction, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};

/// photocraft-cli: headless PhotoCraft
#[derive(Parser)]
#[command(name = "photocraft-cli", arg_required_else_help = true, args_override_self = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    Convert(ConvertArgs),
    Info(InfoArgs),
    Run(RunArgs),
    Batch(BatchArgs),
    Droplet(DropletArgs),
    Commands(CommandsArgs),
    Mcp(McpArgs),
    Serve(ServeArgs),
}

// How an image is written: shared by `convert`, `run` and `batch`.
#[derive(Args)]
pub(crate) struct ExportArgs {
    /// Output format, overriding the output's extension (png, jpg, tif, webp, …)
    #[arg(long, value_name = "EXT")]
    pub(crate) format: Option<String>,
    /// JPEG or WebP quality, 1-100. A WebP written with a quality is lossy, without one lossless
    #[arg(long, value_name = "1-100", allow_negative_numbers = true)]
    pub(crate) quality: Option<String>,
    /// Keep the layers in TIFF output (Photoshop layer data); TIFF output is flat without it
    #[arg(long)]
    pub(crate) tiff_layers: bool,
    /// Compress TIFF output: none (the default), lzw or zip
    #[arg(long, value_name = "none|lzw|zip")]
    pub(crate) tiff_compression: Option<String>,
    /// A TGA's bits per pixel: 32 writes the alpha channel (or else the transparency) as its
    /// alpha, 24 none. Without it: 32 with an alpha channel or transparency, else 24
    #[arg(long, value_name = "24|32")]
    pub(crate) tga_bits: Option<String>,
    /// A DPX's bits per sample. Without it: 8 for an 8-bit document, else 16
    #[arg(long, value_name = "8|10|12|16")]
    pub(crate) dpx_bits: Option<String>,
    /// A DDS texture's compression: none (uncompressed, the default), bc1 (DXT1: alpha on or
    /// off), bc2 (DXT3) or bc3 (DXT5)
    #[arg(long, value_name = "none|bc1|bc2|bc3")]
    pub(crate) dds_compression: Option<String>,
    /// Also write a DDS texture's full mip chain
    #[arg(long)]
    pub(crate) dds_mipmaps: bool,
    /// A JPEG's scans, as JPEG Options picks them: baseline (the default), optimized (Huffman
    /// tables fitted to the image; RGB is written 4:2:0 and keeps the standard tables, so only Gray
    /// and CMYK shrink) or progressive
    #[arg(long, value_name = "baseline|optimized|progressive")]
    pub(crate) jpeg_format: Option<String>,
}

/// Convert between formats (.pcraft, .psd, .png, .jpg, .tif, .webp, .exr, …)
///
/// An <OUT> that is the <IN> file is refused, as the result would replace the original;
/// --in-place allows it.
#[derive(Args)]
pub(crate) struct ConvertArgs {
    /// The file to read
    #[arg(value_name = "IN")]
    pub(crate) input: String,
    /// The file to write; its extension picks the format unless --format is given
    #[arg(value_name = "OUT")]
    pub(crate) output: String,
    /// Allow an <OUT> that is the <IN> file, replacing the original
    #[arg(long)]
    pub(crate) in_place: bool,
    #[command(flatten)]
    pub(crate) export: ExportArgs,
}

/// Print the document as JSON (size, mode, depth, layer tree)
#[derive(Args)]
pub(crate) struct InfoArgs {
    pub(crate) file: String,
    /// One line of JSON instead of pretty-printed
    #[arg(long)]
    pub(crate) compact: bool,
}

/// Open a file (or start a new document), run engine commands in order, save the result
///
/// Prints each command's JSON result. Give exactly one of <FILE> or --new. An --out that is the
/// <FILE> is refused, as the result would replace the original; --in-place allows it.
#[derive(Args)]
pub(crate) struct RunArgs {
    /// The document to open
    pub(crate) file: Option<String>,
    /// Start from a new document with these JSON parameters instead of opening a file
    #[arg(long, value_name = "JSON")]
    pub(crate) new: Option<String>,
    /// An engine command to run; repeat for several, run in order
    #[arg(long, value_name = "ID", action = ArgAction::Append)]
    pub(crate) cmd: Vec<String>,
    /// JSON parameters for the --cmd before it (default {})
    #[arg(long, value_name = "JSON", action = ArgAction::Append)]
    pub(crate) params: Vec<String>,
    /// Save the result to this file
    #[arg(long, value_name = "FILE")]
    pub(crate) out: Option<String>,
    /// Allow an --out that is the <FILE>, replacing the original
    #[arg(long)]
    pub(crate) in_place: bool,
    #[command(flatten)]
    pub(crate) export: ExportArgs,
    /// `--cmd` and `--params` values in command-line order (`true` for a `--cmd`), set by [`parse`].
    #[arg(skip)]
    pub(crate) steps: Vec<(bool, String)>,
}

/// Apply an action list to every image in a directory
///
/// A Photoshop Actions file (.atn) is a set: its action --action names plays, as below. JSON steps
/// are [id, params] pairs, {"command": id, "params": {…}} objects or bare ids, as a recorded
/// action or droplet stores them (a list, or wrapped in {"actions": …}, {"steps": …} or a
/// droplet). An Actions set (the Actions panel's actions.json, {"actions": [{"name", "steps"}…]})
/// plays the action --action names (needed when the set holds more than one) with the whole set
/// loaded, so its steps can play other actions of the set.
///
/// An --out folder that is the --in folder is refused, as the results would replace the
/// originals; --in-place allows it.
#[derive(Args)]
pub(crate) struct BatchArgs {
    /// The action list: actions.json, a droplet, or a Photoshop Actions file (.atn)
    #[arg(long, value_name = "FILE")]
    pub(crate) actions: Option<String>,
    /// Which action of an Actions set to play
    #[arg(long, value_name = "NAME")]
    pub(crate) action: Option<String>,
    /// The folder of images to process
    #[arg(long = "in", value_name = "DIR")]
    pub(crate) in_dir: Option<String>,
    /// The folder to write the results to, as <stem>.<ext>
    #[arg(long, value_name = "DIR")]
    pub(crate) out: Option<String>,
    /// An --out folder that is the --in folder is refused, as the results would replace the
    /// originals; --in-place allows it
    #[arg(long)]
    pub(crate) in_place: bool,
    #[command(flatten)]
    pub(crate) export: ExportArgs,
}

/// Run a droplet (File › Automate › Create Droplet) on images and folders
#[derive(Args)]
pub(crate) struct DropletArgs {
    /// The droplet to run
    #[arg(value_name = "FILE.pcdroplet")]
    pub(crate) file: String,
    /// Images and folders to run it on
    #[arg(value_name = "FILE-OR-DIR", required = true)]
    pub(crate) inputs: Vec<String>,
    /// The folder to write the results to
    #[arg(long, value_name = "DIR")]
    pub(crate) out: Option<String>,
}

/// List the engine command registry
#[derive(Args)]
pub(crate) struct CommandsArgs {
    /// Print the entries as JSON
    #[arg(long)]
    pub(crate) json: bool,
    /// Only the commands matching this text
    #[arg(long, value_name = "TEXT", allow_negative_numbers = true)]
    pub(crate) filter: Option<String>,
}

// The control token and the folders automation may touch: shared by `mcp` and `serve`.
#[derive(Args)]
pub(crate) struct ControlArgs {
    /// The control token (64 hex digits)
    #[arg(long, value_name = "64-HEX", value_parser = NonEmptyStringValueParser::new(), conflicts_with = "control_token_file")]
    pub(crate) control_token: Option<String>,
    /// A file holding the control token, used instead of --control-token (a server creates it
    /// when missing)
    #[arg(long, value_name = "PATH", value_parser = NonEmptyStringValueParser::new())]
    pub(crate) control_token_file: Option<String>,
    /// The folder automation may read from (none without it)
    #[arg(long, value_name = "DIR", value_parser = NonEmptyStringValueParser::new())]
    pub(crate) automation_read_root: Option<String>,
    /// The folder automation may write to (none without it)
    #[arg(long, value_name = "DIR", value_parser = NonEmptyStringValueParser::new())]
    pub(crate) automation_write_root: Option<String>,
}

/// Run the MCP server on stdio (headless engine, or bridge to a running `photocraft --control <port>`)
#[derive(Args)]
pub(crate) struct McpArgs {
    /// Bridge to the PhotoCraft app listening here instead of a headless engine
    #[arg(long, value_name = "127.0.0.1:PORT", value_parser = NonEmptyStringValueParser::new())]
    pub(crate) bridge: Option<String>,
    #[command(flatten)]
    pub(crate) control: ControlArgs,
}

/// Keep one headless session open and answer JSON lines on stdio, or on 127.0.0.1:<PORT>
///
/// Each request is a JSON line {"id", "method", "params"}. Methods: engine.execute,
/// jobs.list/cancel, engine.commands,
/// doc.open/new/place/save/exportLayers/exportDataSets/importDataSets/inspect/render/select/close,
/// session.list, batch, methods (docs/control-protocol.md#headless-server).
#[derive(Args)]
pub(crate) struct ServeArgs {
    /// Listen on 127.0.0.1:<PORT> instead of stdio
    #[arg(long, value_name = "PORT", value_parser = NonEmptyStringValueParser::new())]
    pub(crate) port: Option<String>,
    #[command(flatten)]
    pub(crate) control: ControlArgs,
}

/// `--cmd` and `--params` values back in command-line order: clap keeps each flag's values apart,
/// their indices say how they interleave.
fn steps(m: &ArgMatches) -> Vec<(bool, String)> {
    let mut steps: Vec<(usize, bool, String)> = Vec::new();
    for (id, is_cmd) in [("cmd", true), ("params", false)] {
        if let (Some(at), Some(values)) = (m.indices_of(id), m.get_many::<String>(id)) {
            steps.extend(at.zip(values).map(|(i, v)| (i, is_cmd, v.clone())));
        }
    }
    steps.sort_by_key(|s| s.0);
    steps.into_iter().map(|(_, is_cmd, v)| (is_cmd, v)).collect()
}

/// Parse the arguments (without the binary name). Help where a flag's value should be
/// (`--out --help`) is still help: clap reports the missing value first.
pub(crate) fn parse(args: &[String]) -> Result<Cli, clap::Error> {
    let mut cmd = Cli::command();
    let given: Vec<&str> = std::iter::once("photocraft-cli").chain(args.iter().map(String::as_str)).collect();
    let matches = match cmd.try_get_matches_from_mut(given.iter().copied()) {
        Err(e) if e.kind() == ErrorKind::InvalidValue && given.iter().take_while(|a| **a != "--").any(|a| matches!(*a, "--help" | "-h")) => {
            let help = ["photocraft-cli", given.get(1).copied().unwrap_or_default(), "--help"];
            return Err(Cli::command().try_get_matches_from(help).err().unwrap_or(e));
        }
        matches => matches?,
    };
    let mut cli = Cli::from_arg_matches(&matches)?;
    match (&mut cli.command, matches.subcommand()) {
        (Command::Run(r), Some((_, m))) => r.steps = steps(m),
        (Command::Mcp(_), Some((name, m))) => check_server(cmd.find_subcommand_mut(name), &given, m, "bridge")?,
        (Command::Serve(_), Some((name, m))) => check_server(cmd.find_subcommand_mut(name), &given, m, "port")?,
        _ => {}
    }
    Ok(cli)
}

/// Refuse `mcp` and `serve` launch options that would otherwise be silently ignored (#8035): a
/// repeated one (the last wins everywhere else), a token without the bridge or port that reads it,
/// and bridge roots, which belong to the desktop process: accepting them here promises authority
/// the bridge cannot set.
fn check_server(sub: Option<&mut clap::Command>, given: &[&str], m: &ArgMatches, transport: &str) -> Result<(), clap::Error> {
    let Some(sub) = sub else { return Ok(()) };
    let mut seen = std::collections::HashSet::new();
    let flags = given.iter().skip(1).take_while(|a| **a != "--").filter(|a| a.starts_with("--"));
    if let Some(flag) = flags.map(|a| a.split_once('=').map_or(*a, |(k, _)| k)).find(|k| !seen.insert(*k)) {
        return Err(sub.error(ErrorKind::ArgumentConflict, format!("the argument '{flag}' cannot be used multiple times")));
    }
    for (id, flag) in [("control_token", "--control-token"), ("control_token_file", "--control-token-file")] {
        if m.contains_id(id) && !m.contains_id(transport) {
            return Err(sub.error(ErrorKind::MissingRequiredArgument, format!("{flag} requires --{transport}")));
        }
    }
    if transport == "bridge" && m.contains_id(transport) && (m.contains_id("automation_read_root") || m.contains_id("automation_write_root")) {
        let message = "configure automation roots on the desktop app; --automation-read-root and --automation-write-root require headless MCP mode";
        return Err(sub.error(ErrorKind::ArgumentConflict, message));
    }
    Ok(())
}
