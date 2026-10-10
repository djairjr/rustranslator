//! The rustranslator MCP server.
//!
//! ## The tools, and why each one exists
//!
//! * `echo` — proves the server answered just now, rather than a cached value;
//! * `translate` — translates one piece of text, protecting what must not be translated;
//! * `review` — checks one translation against its source and returns a verdict;
//! * `submit` — starts a whole document, in the background, and returns immediately;
//! * `status` — how a document is going, read from the registry on disk;
//! * `result` — the flagged blocks: the residue a person has to read;
//! * `cancel` — stops a running document, keeping everything finished.
//!
//! ## Why a task, and not a long call
//!
//! A 400-block document takes the better part of an hour. An MCP call that took an hour would be a
//! hung conversation: the client would time out, the agent would have nothing to say, and a
//! dropped connection would throw the work away. So `submit` returns as soon as the job starts,
//! and the state lives **on disk** — the registry — where any later call, in any session, can read
//! it. Nothing important is held in memory, which is also why restarting the service costs nothing.
//!
//! ## The golden rule of this module
//!
//! Over stdio, **stdout is the protocol**. Any `println!` that is not the transport corrupts the
//! conversation with the client. Everything here — diagnostics, warnings, errors — goes to
//! **stderr**.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
    transport::stdio,
    ErrorData as McpError, ServiceExt,
};
use rustranslator_core::{config, engine, llm, progress, registry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// The parameters of each tool
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EchoParams {
    /// The text that must come back, unchanged.
    pub text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TranslateParams {
    /// The text to translate.
    pub text: String,
    /// Target locale code (e.g. "pt-BR", "en"). If omitted, uses the configured `target`.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReviewParams {
    /// The original text.
    pub source: String,
    /// The translation to check.
    pub translation: String,
    /// Target locale code. If omitted, uses the configured `target`.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    /// Path to the markdown document to translate.
    pub document: String,
    /// Target locale code. If omitted, uses the configured `target`.
    #[serde(default)]
    pub target: Option<String>,
    /// Where the registry and the report live. If omitted, uses the configured `output`.
    #[serde(default)]
    pub dir: Option<String>,
    /// Translate every block again, even the ones already in the registry.
    #[serde(default)]
    pub force: Option<bool>,
    /// Translate at most this many blocks. `0`, or omitted, means all of them.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    /// Base name of the document (e.g. "README"), without the extension.
    pub document: String,
    /// Directory holding the registry. If omitted, uses the configured `output`.
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ResultParams {
    /// Base name of the document (e.g. "README"), without the extension.
    pub document: String,
    /// Directory holding the registry. If omitted, uses the configured `output`.
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CancelParams {
    /// Base name of the document (e.g. "README"), without the extension.
    pub document: String,
}

// ---------------------------------------------------------------------------
// The jobs that are running
// ---------------------------------------------------------------------------

/// One flag per document being translated, so `cancel` has something to set.
///
/// A `Mutex` and not a channel: this is written on submit, read on cancel, and never contended —
/// one agent submits a handful of documents in an hour.
#[derive(Default)]
struct Jobs {
    running: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Jobs {
    fn start(&self, key: &str) -> Option<Arc<AtomicBool>> {
        let mut running = self.running.lock().unwrap();
        if running.contains_key(key) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        running.insert(key.to_string(), flag.clone());
        Some(flag)
    }

    fn cancel(&self, key: &str) -> bool {
        match self.running.lock().unwrap().get(key) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    fn finish(&self, key: &str) {
        self.running.lock().unwrap().remove(key);
    }
}

/// The MCP server. The job table is shared by every session, so a `submit` from one connection can
/// be cancelled from another.
#[derive(Clone)]
pub struct Server {
    jobs: Arc<Jobs>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Jobs::default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration, resolved the same way everywhere
// ---------------------------------------------------------------------------

fn settings() -> Result<(config::Config, config::Environment), String> {
    let environment = config::Environment::real();
    let (_, cfg) = config::resolve(None, &environment).map_err(|e| {
        format!(
            "could not resolve the configuration: {e}\n\
             Create ~/.config/rustranslator/config.json (there is an example at \
             /usr/share/doc/rustranslator/config.example.json)."
        )
    })?;
    Ok((cfg, environment))
}

/// Loads the configuration and hands back the requested role.
///
/// The reviewer is a distinct role on purpose: it must be a stronger model than the translator, and
/// the two may live behind different providers.
fn role(which: &str) -> Result<(String, config::Role), String> {
    let (cfg, _) = settings()?;
    let role = match which {
        "reviewer" => cfg.reviewer.clone(),
        _ => cfg.translator.clone(),
    };
    Ok((cfg.target.clone(), role))
}

/// The target for this call: the parameter wins over the configuration.
fn effective_target(requested: Option<String>, from_config: &str) -> Result<String, String> {
    match requested
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
    {
        Some(d) => config::normalize_locale(&d).map_err(|reason| {
            format!("invalid target: {d:?} — {reason}. Use a locale code, e.g.: \"pt-BR\", \"en\"")
        }),
        None => Ok(from_config.to_string()),
    }
}

/// Where the registry lives: the caller's `dir`, or the configured `output`.
fn output_dir(dir: Option<String>) -> Result<PathBuf, String> {
    let environment = config::Environment::real();
    match dir {
        Some(d) => Ok(config::expand_tilde(&d, &environment)),
        None => config::resolve(None, &environment)
            .map(|(_, cfg)| config::expand_tilde(&cfg.output, &environment))
            .map_err(|e| {
                format!(
                    "could not resolve the configuration to find the output directory: {e}\n\
                     Pass `dir` explicitly, e.g.: {{\"document\": \"README\", \"dir\": \"~/.local/share/rustranslator\"}}"
                )
            }),
    }
}

/// The base name of a document path: what the registry, the task file and the report are named
/// after.
fn base_name(document: &Path) -> Result<String, String> {
    document
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{} has no file name to name the job after", document.display()))
}

fn error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn json(value: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|e| format!("{{}}\n{e}"));
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// The current instant in seconds since the epoch, with no date/time crate.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tool_router(server_handler)]
impl Server {
    /// Returns the text it received, together with the instant of the call.
    ///
    /// The instant is not decoration: it is what tells "the server answered just now" apart from
    /// "the client returned a cached value".
    #[tool(
        description = "Returns the received text together with the instant of the call. Living proof that the server answered just now."
    )]
    fn echo(&self, Parameters(EchoParams { text }): Parameters<EchoParams>) -> String {
        format!("{text} | echo at {} | pid {}", now_secs(), std::process::id())
    }

    /// Translates one piece of text into the target locale.
    ///
    /// It protects what must not be translated (whole markdown links, inline code, URLs,
    /// `{{placeholders}}`) and re-asks up to three times if the model drops a slot — a dropped slot
    /// is a lost link.
    #[tool(
        description = "Translates one piece of text into a locale. Protects markdown (whole links, inline code, URLs, {{placeholders}}) and re-asks if the model drops any. Without `target` it uses the configured one; with it, the parameter wins."
    )]
    async fn translate(
        &self,
        Parameters(TranslateParams { text, target }): Parameters<TranslateParams>,
    ) -> Result<CallToolResult, McpError> {
        let (config_target, role) = match role("translator") {
            Ok(pair) => pair,
            Err(warning) => return Ok(error(warning)),
        };
        let target = match effective_target(target, &config_target) {
            Ok(t) => t,
            Err(warning) => return Ok(error(warning)),
        };

        match llm::translate(&role, &target, &text).await {
            Ok(translated) => Ok(CallToolResult::success(vec![ContentBlock::text(translated)])),
            Err(e) => Ok(error(e.to_string())),
        }
    }

    /// Checks a translation against the original and returns the verdict.
    ///
    /// The reviewer must be a stronger model than the translator; here it is the `reviewer` role
    /// from the configuration, which may point at another provider.
    #[tool(
        description = "Checks a translation against the original text and returns JSON: {\"ok\": true} if it says the same, {\"ok\": false, \"correction\": \"...\"} if not, or {\"ok\": null, \"error\": \"...\"} if the reviewer could not answer."
    )]
    async fn review(
        &self,
        Parameters(ReviewParams {
            source,
            translation,
            target,
        }): Parameters<ReviewParams>,
    ) -> Result<CallToolResult, McpError> {
        let (config_target, role) = match role("reviewer") {
            Ok(pair) => pair,
            Err(warning) => return Ok(error(warning)),
        };
        let target = match effective_target(target, &config_target) {
            Ok(t) => t,
            Err(warning) => return Ok(error(warning)),
        };

        match llm::review(&role, &target, &source, &translation).await {
            Ok(verdict) => Ok(json(match verdict {
                llm::Verdict::Approved => serde_json::json!({ "ok": true }),
                llm::Verdict::Corrected(c) => serde_json::json!({ "ok": false, "correction": c }),
                llm::Verdict::Undecided { reason } => {
                    serde_json::json!({ "ok": null, "error": reason })
                }
            })),
            Err(e) => Ok(error(e.to_string())),
        }
    }

    /// Starts translating a whole document, in the background, and returns at once.
    ///
    /// It returns immediately on purpose. A 400-block document runs for the better part of an hour,
    /// and a call that ran for an hour would be a hung conversation. Ask `status` for progress,
    /// `result` for what needs reading, and `cancel` to stop.
    #[tool(
        description = "Starts translating a whole document in the background. Returns immediately with the job's name and block count; ask `status` for progress. A block already in the registry is skipped, so stopping and starting again costs nothing."
    )]
    async fn submit(
        &self,
        Parameters(SubmitParams {
            document,
            target,
            dir,
            force,
            limit,
        }): Parameters<SubmitParams>,
    ) -> Result<CallToolResult, McpError> {
        let (cfg, environment) = match settings() {
            Ok(pair) => pair,
            Err(warning) => return Ok(error(warning)),
        };
        let document = PathBuf::from(&document);
        if !document.is_file() {
            return Ok(error(format!("{} is not a file", document.display())));
        }
        let stem = match base_name(&document) {
            Ok(stem) => stem,
            Err(warning) => return Ok(error(warning)),
        };
        let target = match effective_target(target, &cfg.target) {
            Ok(t) => t,
            Err(warning) => return Ok(error(warning)),
        };

        let source = match std::fs::read_to_string(&document) {
            Ok(source) => source,
            Err(e) => return Ok(error(format!("could not read {}: {e}", document.display()))),
        };
        let blocks = match rustranslator_core::parser::parse(&source) {
            Ok(blocks) => blocks,
            Err(e) => return Ok(error(format!("could not read the markdown: {e}"))),
        };

        let Some(flag) = self.jobs.start(&stem) else {
            return Ok(error(format!(
                "{stem} is already being translated. Ask `status` for progress, or `cancel` first."
            )));
        };

        let options = engine::Options {
            target: Some(target.clone()),
            from: 0,
            limit: limit.unwrap_or(0),
            force: force.unwrap_or(false),
            out_dir: output_dir(dir).unwrap_or_else(|_| {
                config::expand_tilde(&cfg.output, &environment)
            }),
            doc_destination: None,
            // Nothing reads a library's stdout: over stdio it would corrupt the protocol.
            verbose: false,
        };

        let jobs = self.jobs.clone();
        let job_key = stem.clone();
        let path = document.clone();
        tokio::spawn(async move {
            match engine::run(&path, &cfg, &options, &flag).await {
                Ok(summary) => eprintln!(
                    "rustranslator: {job_key} finished — {} translated, {} already done, \
                     {} corrected, {} undecided, {} refused",
                    summary.translated,
                    summary.skipped,
                    summary.corrected,
                    summary.undecided,
                    summary.lost
                ),
                Err(e) => eprintln!("rustranslator: {job_key} failed: {e}"),
            }
            jobs.finish(&job_key);
        });

        Ok(json(serde_json::json!({
            "job": stem,
            "document": document.display().to_string(),
            "target": target,
            "blocks": blocks.len(),
            "state": "running",
        })))
    }

    /// How a document's translation is going, read from the registry on disk.
    ///
    /// There is no in-memory state to consult: the registry is the truth, which is why this answers
    /// the same way from any session, and why restarting the service loses nothing.
    #[tool(
        description = "Progress of a document's translation: blocks processed, approved, corrected, undecided, how many remain, percentage and correction rate. Reads the registry on disk; it does not depend on anything running in memory."
    )]
    fn status(
        &self,
        Parameters(StatusParams { document, dir }): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir_path = match output_dir(dir) {
            Ok(d) => d,
            Err(warning) => return Ok(error(warning)),
        };

        match progress::Progress::read(&dir_path, &document) {
            Ok(p) => Ok(json(serde_json::json!({
                "summary": p.summary(),
                "directory": dir_path.display().to_string(),
                "running": self.jobs.running.lock().unwrap().contains_key(&document),
                "progress": p,
            }))),
            Err(e) => Ok(error(format!(
                "could not read the registry at {}: {e}",
                dir_path.display()
            ))),
        }
    }

    /// The flagged blocks: the residue a person still has to read.
    ///
    /// This is where the arrangement pays off. The reviewer exists so that somebody reads the
    /// *residue* and not the document; if this returned every block, having a reviewer would have
    /// bought nothing. Three things land here — a block the reviewer corrected, one it could not
    /// judge, and one whose translation was refused — each with its source, so a verdict can be
    /// checked against something.
    #[tool(
        description = "The blocks that still need a person: those the reviewer corrected, those it could not judge, and those whose translation was refused. Approved blocks are not included — that is the point of having a reviewer."
    )]
    fn result(
        &self,
        Parameters(ResultParams { document, dir }): Parameters<ResultParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir_path = match output_dir(dir) {
            Ok(d) => d,
            Err(warning) => return Ok(error(warning)),
        };
        let records = registry::load(&registry::registry_path(&dir_path, &document));

        let mut flagged: Vec<&registry::Record> = records
            .values()
            .filter(|r| r.ok != Some(true))
            .collect();
        // Document order, so the list can be read against the file.
        flagged.sort_by_key(|r| r.line);

        Ok(json(serde_json::json!({
            "document": document,
            "directory": dir_path.display().to_string(),
            "recorded": records.len(),
            "flagged": flagged.len(),
            "blocks": flagged,
        })))
    }

    /// Stops a running document, keeping everything finished.
    ///
    /// The flag is read between blocks and never inside one, so nothing half-recorded is left
    /// behind: the registry keeps every block that was done, and the next run continues from there.
    #[tool(
        description = "Stops a document that is being translated. Everything already recorded is kept: the registry is not rolled back, and the next run continues from where this one stopped."
    )]
    fn cancel(&self, Parameters(CancelParams { document }): Parameters<CancelParams>) -> CallToolResult {
        if self.jobs.cancel(&document) {
            json(serde_json::json!({
                "document": document,
                "state": "cancelling",
                "note": "the current block finishes first; everything recorded is kept",
            }))
        } else {
            json(serde_json::json!({
                "document": document,
                "state": "not running",
            }))
        }
    }
}

/// Starts the MCP server over stdio and waits until the client closes.
pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "rustranslator mcp: serving over stdio (pid {})",
        std::process::id()
    );
    let service = Server::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Starts the MCP server over HTTP (Streamable HTTP) — the mode **systemd** can keep alive.
///
/// Why this exists: a stdio server only lives while a client spawned it and holds the stdin/stdout
/// pipes. systemd keeps the process alive but hands pipes to nobody — so "systemd + stdio" is
/// impossible. Serving over HTTP is the only way for the service to live on its own and answer any
/// harness that points at the address.
pub async fn serve_http(address: &str) -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    // One job table for the whole process: a session may submit and another may cancel, and both
    // must see the same documents.
    let jobs = Arc::new(Jobs::default());

    let mcp_service: StreamableHttpService<Server, LocalSessionManager> = StreamableHttpService::new(
        move || {
            Ok(Server {
                jobs: jobs.clone(),
            })
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().nest_service("/mcp", mcp_service);
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "rustranslator mcp: serving on http://{address}/mcp (pid {})",
        std::process::id()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
