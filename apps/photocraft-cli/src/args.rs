use std::io::Write;

use crate::{
    Result,
    automation::{commands, mcp, serve},
    batch::{batch, droplet},
    document::{convert, info, run_cmds},
};

pub const USAGE: &str = "\
photocraft-cli: headless PhotoCraft

USAGE:
  photocraft-cli convert <in> <out> [--format <ext>] [--quality <1-100>] [--in-place] [--tiff-layers] [--tiff-compression <none|lzw|zip>] [--tga-bits <24|32>] [--dpx-bits <8|10|12|16>] [--dds-compression <none|bc1|bc2|bc3>] [--dds-mipmaps] [--jpeg-format <baseline|optimized|progressive>]
      Convert between formats (.pcraft, .psd, .png, .jpg, .tif, .webp, .exr, …).
      An <out> that is the <in> file is refused, as the result would replace the original;
      --in-place allows it.
      TIFF output is flat unless --tiff-layers keeps the layers (Photoshop layer data);
      --tiff-compression compresses it (none, the default; lzw; zip).
      --quality sets the JPEG or WebP quality; a WebP written with a quality is lossy, without one lossless.
      --tga-bits sets a TGA's bits per pixel: 32 writes the alpha channel (or else the transparency) as
      its alpha, 24 none. Without it: 32 with an alpha channel or transparency, else 24.
      --dpx-bits sets a DPX's bits per sample. Without it: 8 for an 8-bit document, else 16.
      --dds-compression sets a DDS texture's compression: none (uncompressed, the default), bc1
      (DXT1: alpha on or off), bc2 (DXT3) or bc3 (DXT5). --dds-mipmaps also writes the full mip chain.
      --jpeg-format picks a JPEG's scans, as JPEG Options does: baseline (the default), optimized
      (Huffman tables fitted to the image; RGB is written 4:2:0 and keeps the standard tables, so
      only Gray and CMYK shrink) or progressive.
  photocraft-cli info <file> [--compact]
      Print the document as JSON (size, mode, depth, layer tree).
  photocraft-cli run (<file> | --new <json>) --cmd <id> [--params <json>] [--cmd …] [--out <file>] [--format <ext>] [--quality <1-100>] [--in-place] [--tiff-layers] [--tiff-compression <none|lzw|zip>] [--tga-bits <24|32>] [--dpx-bits <8|10|12|16>] [--dds-compression <none|bc1|bc2|bc3>] [--dds-mipmaps] [--jpeg-format <baseline|optimized|progressive>]
      Open a file, run engine commands in order, save the result. Each --params
      applies to the preceding --cmd. Prints each command's JSON result. An --out that is the
      <file> is refused, as the result would replace the original; --in-place allows it.
  photocraft-cli batch --actions <actions.json|set.atn> [--action <name>] --in <dir> --out <dir> [--format <ext>] [--quality <1-100>] [--in-place] [--tiff-layers] [--tiff-compression <none|lzw|zip>] [--tga-bits <24|32>] [--dpx-bits <8|10|12|16>] [--dds-compression <none|bc1|bc2|bc3>] [--dds-mipmaps] [--jpeg-format <baseline|optimized|progressive>]
      Apply an action list to every image in a directory. A Photoshop Actions file (.atn) is a
      set: its action --action names plays, as below. JSON steps are [id, params] pairs,
      {\"command\": id, \"params\": {…}} objects or bare ids, as a recorded action or droplet stores them
      (a list, or wrapped in {\"actions\": …}, {\"steps\": …} or a droplet). An Actions set (the Actions
      panel's actions.json, {\"actions\": [{\"name\", \"steps\"}…]}) plays the action --action names (needed
      when the set holds more than one) with the whole set loaded, so its steps can play other actions
      of the set. An --out folder that is the --in folder is refused, as the results would replace
      the originals; --in-place allows it.
  photocraft-cli droplet <file.pcdroplet> <file-or-dir>… [--out <dir>]
      Run a droplet (File › Automate › Create Droplet) on images and folders.
  photocraft-cli commands [--json] [--filter <text>]
      List the engine command registry.
  photocraft-cli mcp [--bridge <127.0.0.1:port>] [--control-token <64-hex> | --control-token-file <path>]
      [--automation-read-root <dir>] [--automation-write-root <dir>]
      Run the MCP server on stdio (headless engine, or bridge to a running `photocraft --control <port>`).
      Headless: each root grants that direction of file access; an omitted root denies it.
      Prefer absolute root paths in MCP client configurations.
      Bridge: set roots on the desktop app; CLI root flags are rejected. Explicit token flags
      require --bridge; credentials can also use PHOTOCRAFT_CONTROL_TOKEN or PHOTOCRAFT_CONTROL_TOKEN_FILE.
  photocraft-cli serve [--port <port>] [--control-token <64-hex> | --control-token-file <path>]
      [--automation-read-root <dir>] [--automation-write-root <dir>]
      Keep one headless session open and answer JSON lines ({\"id\",\"method\",\"params\"}) on stdio,
      or on 127.0.0.1:<port>. Methods: engine.execute, jobs.list/cancel, engine.commands,
      doc.open/new/place/save/exportLayers/exportDataSets/importDataSets/inspect/render/select/close, session.list, batch, methods
      (docs/control-protocol.md#headless-server).
      Explicit token flags require --port. Stdio needs no control token.

  photocraft-cli <subcommand> --help (or -h) prints this text. A flag the subcommand doesn't take is
  an error. mcp and serve take no positional arguments; their options cannot be repeated or empty.
";

pub(crate) struct Args {
    pub(crate) positional: Vec<String>,
    pub(crate) flags: Vec<(String, Option<String>)>,
}

/// A subcommand: the flags it reads (taking a value, or given bare) and what it runs. A flag
/// outside these is a usage error, so a typo such as `--fromat` can't be silently ignored (#423).
pub(crate) struct Subcommand {
    pub(crate) name: &'static str,
    pub(crate) values: &'static [&'static str],
    pub(crate) bare: &'static [&'static str],
    pub(crate) run: fn(&Args, &mut dyn Write, &mut dyn Write) -> Result,
}

pub(crate) const SUBCOMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "convert",
        values: &["--format", "--quality", "--tga-bits", "--dpx-bits", "--dds-compression", "--tiff-compression", "--jpeg-format"],
        bare: &["--in-place", "--tiff-layers", "--dds-mipmaps"],
        run: convert,
    },
    Subcommand { name: "info", values: &[], bare: &["--compact"], run: |a, out, _| info(a, out) },
    Subcommand {
        name: "run",
        values: &[
            "--new",
            "--cmd",
            "--params",
            "--out",
            "--format",
            "--quality",
            "--tga-bits",
            "--dpx-bits",
            "--dds-compression",
            "--tiff-compression",
            "--jpeg-format",
        ],
        bare: &["--in-place", "--tiff-layers", "--dds-mipmaps"],
        run: run_cmds,
    },
    Subcommand {
        name: "batch",
        values: &[
            "--actions",
            "--action",
            "--in",
            "--out",
            "--format",
            "--quality",
            "--tga-bits",
            "--dpx-bits",
            "--dds-compression",
            "--tiff-compression",
            "--jpeg-format",
        ],
        bare: &["--in-place", "--tiff-layers", "--dds-mipmaps"],
        run: batch,
    },
    Subcommand { name: "droplet", values: &["--out"], bare: &[], run: droplet },
    Subcommand { name: "commands", values: &["--filter"], bare: &["--json"], run: |a, out, _| commands(a, out) },
    Subcommand {
        name: "mcp",
        values: &["--bridge", "--control-token", "--control-token-file", "--automation-read-root", "--automation-write-root"],
        bare: &[],
        run: |a, _, _| mcp(a),
    },
    Subcommand {
        name: "serve",
        values: &["--port", "--control-token", "--control-token-file", "--automation-read-root", "--automation-write-root"],
        bare: &[],
        run: |a, _, err| serve(a, err),
    },
];

/// A command line [`parse`] refuses: printed with the usage text, exit 2.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UsageError {
    #[error("unknown flag {flag} for {sub}")]
    UnknownFlag { flag: String, sub: &'static str },
    #[error("{0} takes no value")]
    TakesNoValue(String),
    #[error("{0} needs a value")]
    NeedsValue(String),
    #[error("{0} does not take positional arguments")]
    Positional(&'static str),
    #[error("{0} may only be specified once")]
    Repeated(String),
    #[error("{0} needs a non-empty value")]
    EmptyValue(String),
    #[error("use either --control-token or --control-token-file, not both")]
    TokenAndTokenFile,
    #[error("{flag} requires {transport}")]
    Requires { flag: &'static str, transport: &'static str },
    #[error("configure automation roots on the desktop app; --automation-read-root and --automation-write-root require headless MCP mode")]
    BridgeRoots,
}

/// The subcommand's arguments, or `None` when they ask for help (`--help` or `-h`).
pub(crate) fn parse(sub: &Subcommand, args: &[String]) -> Result<Option<Args>, UsageError> {
    let mut a = Args { positional: Vec::new(), flags: Vec::new() };
    let unknown = |flag: &str| UsageError::UnknownFlag { flag: flag.to_owned(), sub: sub.name };
    let mut i = 0;
    while let Some(s) = args.get(i) {
        if s == "--help" || s == "-h" {
            return Ok(None);
        }
        if let Some((k, v)) = s.split_once('=').filter(|(k, _)| k.starts_with("--")) {
            if sub.bare.contains(&k) {
                return Err(UsageError::TakesNoValue(k.to_owned()));
            }
            if !sub.values.contains(&k) {
                return Err(unknown(k));
            }
            a.flags.push((k.to_owned(), Some(v.to_owned())));
        } else if sub.values.contains(&s.as_str()) {
            // A flag where the value should be is a missing value, not the value; help still wins.
            let v = args.get(i + 1);
            if v.is_some_and(|v| v == "--help" || v == "-h") {
                return Ok(None);
            }
            let v = v.filter(|v| !v.starts_with("--")).ok_or_else(|| UsageError::NeedsValue(s.clone()))?;
            a.flags.push((s.clone(), Some(v.clone())));
            i += 1;
        } else if sub.bare.contains(&s.as_str()) {
            a.flags.push((s.clone(), None));
        } else if s.starts_with("--") {
            return Err(unknown(s));
        } else {
            a.positional.push(s.clone());
        }
        i += 1;
    }
    validate_server_args(sub.name, &a)?;
    Ok(Some(a))
}

/// Refuse launch options that would otherwise be silently ignored. In particular, bridge
/// roots belong to the desktop process: accepting them here promises authority we cannot set.
fn validate_server_args(sub: &'static str, a: &Args) -> Result<(), UsageError> {
    if !matches!(sub, "mcp" | "serve") {
        return Ok(());
    }
    if !a.positional.is_empty() {
        return Err(UsageError::Positional(sub));
    }
    let mut seen = std::collections::HashSet::new();
    for (flag, value) in &a.flags {
        if !seen.insert(flag) {
            return Err(UsageError::Repeated(flag.clone()));
        }
        if value.as_deref().is_none_or(str::is_empty) {
            return Err(UsageError::EmptyValue(flag.clone()));
        }
    }
    if a.has("--control-token") && a.has("--control-token-file") {
        return Err(UsageError::TokenAndTokenFile);
    }
    let transport = if sub == "mcp" { "--bridge" } else { "--port" };
    for flag in ["--control-token", "--control-token-file"] {
        if a.has(flag) && !a.has(transport) {
            return Err(UsageError::Requires { flag, transport });
        }
    }
    if sub == "mcp" && a.has("--bridge") && (a.has("--automation-read-root") || a.has("--automation-write-root")) {
        return Err(UsageError::BridgeRoots);
    }
    Ok(())
}

impl Args {
    pub(crate) fn get(&self, k: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(f, _)| f == k).and_then(|(_, v)| v.as_deref())
    }
    pub(crate) fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|(f, _)| f == k)
    }
}
