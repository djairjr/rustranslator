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
    rustranslator translate <file.md> [--target ll-XX] [--from N] [--limit N] [--force] [--quiet]
    rustranslator parse <file.md>
    rustranslator mcp [--transport stdio|http] [--address 127.0.0.1:9720]

  `translate` walks the document, translates every block, records each verdict, and writes the
  translated document next to the source as `<name>_<locale>.md`. It is resumable: a block already
  in the registry is skipped, so stopping and starting again costs nothing.
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
        (Some("translate"), Some(path)) => translate_file(path, &args, explicit.as_deref()),
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

    let blocks = match rustranslator_core::parser::parse(&text) {
        Ok(blocks) => blocks,
        Err(e) => {
            eprintln!("error: could not read the markdown: {e}");
            return ExitCode::from(1);
        }
    };
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

/// Translates a document, end to end.
///
/// The exit code says what happened without anyone having to read the output: 0 everything was
/// approved, 1 something needs a person, 2 the run could not even start. A caller that pipes this
/// into a script can branch on it.
fn translate_file(path: &str, args: &[String], explicit: Option<&std::path::Path>) -> ExitCode {
    use rustranslator_core::{config, engine};
    use std::sync::atomic::AtomicBool;

    let env = config::Environment::real();
    let (config_path, cfg) = match config::resolve(explicit, &env) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    if let Some(warning) = config::permission_warning(&config_path, &cfg) {
        eprintln!("{warning}");
    }

    let number = |flag: &str| -> usize {
        flag_value(args, flag).and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    let options = engine::Options {
        target: flag_value(args, "--target"),
        from: number("--from"),
        limit: number("--limit"),
        force: args.iter().any(|a| a == "--force"),
        out_dir: config::expand_tilde(&cfg.output, &env),
        doc_destination: None,
        verbose: !args.iter().any(|a| a == "--quiet"),
    };

    let document = std::path::Path::new(path);
    if !document.is_file() {
        eprintln!("error: {path} is not a file");
        return ExitCode::from(2);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };

    // Nothing installs a signal handler here on purpose: Ctrl-C kills the process, the registry
    // already holds every finished block, and the next run picks up from there.
    let cancel = AtomicBool::new(false);
    let summary = match runtime.block_on(engine::run(document, &cfg, &options, &cancel)) {
        Ok(summary) => summary,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    println!("document   {}", summary.output.display());
    println!("report     {}", summary.report.display());
    println!("registry   {}", summary.registry.display());
    println!(
        "blocks     {} in the document, {} translated now, {} already done",
        summary.units, summary.translated, summary.skipped
    );
    println!(
        "verdicts   {} approved, {} corrected, {} undecided, {} refused",
        summary.approved, summary.corrected, summary.undecided, summary.lost
    );
    if summary.cancelled {
        println!("cancelled  yes — the registry holds what was finished");
    }

    // Something to read means something to answer for.
    if summary.corrected + summary.undecided + summary.lost > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
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
