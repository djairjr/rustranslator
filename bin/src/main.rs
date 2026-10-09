//! rustranslator — command line and MCP server.
//!
//! Two subcommands:
//!   * `config show` — checks what the program REALLY read, instead of assuming which file it
//!     came from;
//!   * `mcp` — starts the MCP server, over stdio (the client spawns it) or HTTP (systemd keeps
//!     it alive).

mod mcp;

use rustranslator_core::config;
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
rustranslator — markdown translation: the local translator writes, the cloud reviewer checks.

USAGE:
    rustranslator config show [--config <path>]
    rustranslator parse <file.md>
    rustranslator mcp [--transport stdio|http] [--address 127.0.0.1:9720]

  `parse` prints the document's blocks as JSON lines — one per block, with the line it came
  from. It is how the parser is inspected, and how the port is compared against the golden
  corpus: no model is called, so the comparison costs nothing and proves exactly one thing.
  `--transport stdio` (default) — the MCP client spawns the process and holds the pipes.
  `--transport http`            — serves on http://<address>/mcp; this is the mode systemd can
                                  keep alive, because it does not depend on a client.

OPTIONS:
    --config <path>   use this file, ignoring the search order
    -h, --help        this help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }

    // --config may appear anywhere; we pull it out before dispatching.
    let explicit: Option<PathBuf> = match args.iter().position(|a| a == "--config") {
        Some(i) => match args.get(i + 1) {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                eprintln!("error: --config requires a path");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let rest: Vec<&String> = args.iter().filter(|a| *a != "--config").collect();

    match (rest.first().map(|s| s.as_str()), rest.get(1).map(|s| s.as_str())) {
        (Some("config"), Some("show")) => show_config(explicit.as_deref()),
        (Some("parse"), Some(path)) => parse_file(path),
        (Some("mcp"), _) => serve_mcp(&args),
        (Some("config"), _) => {
            eprintln!("error: usage: rustranslator config show [--config <path>]");
            ExitCode::from(2)
        }
        (Some(other), _) => {
            eprintln!("error: unknown subcommand: {other}\n\n{HELP}");
            ExitCode::from(2)
        }
        (None, _) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
    }
}

fn show_config(explicit: Option<&std::path::Path>) -> ExitCode {
    let env = config::Environment::real();
    let (path, cfg) = match config::resolve(explicit, &env) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    println!("file      {}", path.display());
    println!("target    {}", cfg.target);
    println!("output    {}", config::expand_tilde(&cfg.output, &env).display());
    println!();
    for (label, role) in [("translator", &cfg.translator), ("reviewer", &cfg.reviewer)] {
        println!("[{label}]");
        println!("  base_url  {}", role.base_url);
        println!("  model     {}", role.model);
        println!("  api_key   {}", config::mask_secret(&role.api_key));
        if let Some(t) = role.temperature {
            println!("  temperature       {t}");
        }
        if let Some(p) = role.top_p {
            println!("  top_p             {p}");
        }
        if let Some(k) = role.top_k {
            println!("  top_k             {k}");
        }
        if let Some(r) = role.repeat_penalty {
            println!("  repeat_penalty    {r}");
        }
        if let Some(f) = &role.response_format {
            println!("  response_format   {f}");
        }
    }

    if let Some(warning) = config::permission_warning(&path, &cfg) {
        println!();
        eprintln!("{warning}");
    }

    ExitCode::SUCCESS
}

/// Prints the document's blocks as JSON lines — one per block, in order.
///
/// This is the parser made visible, and it is what turns the port into something measurable: the
/// output can be compared against the registry, block by block. The count goes
/// to **stderr**, because stdout is the data and a reader piping it into a comparison should not
/// have to skip a stray line.
fn parse_file(path: &str) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("error: could not read {path}: {e}");
            return ExitCode::from(1);
        }
    };

    let blocks = rustranslator_core::parser::parse_document(&text);
    for block in &blocks {
        match serde_json::to_string(block) {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("error: could not serialise a block: {e}");
                return ExitCode::from(1);
            }
        }
    }

    let prose = blocks.iter().filter(|b| b.kind == rustranslator_core::parser::Kind::Prose).count();
    let table = blocks.len() - prose;
    eprintln!("{} blocks ({prose} prose, {table} table)", blocks.len());
    ExitCode::SUCCESS
}

/// The value that follows a flag, if the flag is present.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Starts the MCP server on the requested transport.
///
/// Over stdio, nothing here may write to stdout: stdout is the protocol channel, and a stray
/// `println!` corrupts the conversation with the client. Diagnostics go to stderr — in both
/// modes.
fn serve_mcp(args: &[String]) -> ExitCode {
    let transport = flag_value(args, "--transport").unwrap_or_else(|| "stdio".to_string());
    let address = flag_value(args, "--address").unwrap_or_else(|| "127.0.0.1:9720".to_string());

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let result = match transport.as_str() {
        "stdio" => rt.block_on(mcp::serve()),
        "http" => rt.block_on(mcp::serve_http(&address)),
        other => {
            eprintln!("error: unknown transport: {other:?} — use \"stdio\" or \"http\"");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error in the MCP server: {e}");
            ExitCode::from(1)
        }
    }
}
