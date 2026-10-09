//! The rustranslator MCP server.
//!
//! Four tools, and each exists for a reason:
//!
//! * `echo` — proves the server answered just now, rather than a cached value;
//! * `progress` — reports how a translation job is going, read from the registry on disk;
//! * `translate` — translates one piece of text, protecting what must not be translated;
//! * `review` — checks a translation against its source and returns a verdict.
//!
//! ## The golden rule of this module
//!
//! In an MCP server over stdio, **stdout is the protocol**. Any `println!` that is not the
//! transport corrupts the conversation with the client. Here, everything — diagnostics,
//! warnings, errors — goes to **stderr**.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
    transport::stdio,
    ErrorData as McpError, ServiceExt,
};
use rustranslator_core::{config, llm, progress};

/// Parameters of the `echo` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EchoParams {
    /// The text that must come back, unchanged.
    pub text: String,
}

#[derive(Clone)]
pub struct Server;

/// Parameters of the `progress` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProgressParams {
    /// Base name of the document (e.g. "README"), without the extension.
    pub document: String,
    /// Directory holding the registry. If omitted, uses the configured `output`.
    #[serde(default)]
    pub dir: Option<String>,
}

/// Parameters of the `translate` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TranslateParams {
    /// The text to translate.
    pub text: String,
    /// Target locale code (e.g. "pt-BR", "en"). If omitted, uses the configured `target`.
    #[serde(default)]
    pub target: Option<String>,
}

/// Parameters of the `review` tool.
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

/// Loads the configuration and hands back the requested role.
///
/// The reviewer is a distinct role on purpose: it must be a stronger model than the translator,
/// and they may live behind different providers.
fn role(which: &str) -> Result<(String, config::Role), String> {
    let (_, cfg) = config::resolve(None, &config::Environment::real()).map_err(|e| {
        format!(
            "could not resolve the configuration: {e}\n\
             Create ~/.config/rustranslator/config.json (there is an example at \
             /usr/share/doc/rustranslator/config.example.json)."
        )
    })?;
    let role = match which {
        "reviewer" => cfg.reviewer.clone(),
        _ => cfg.translator.clone(),
    };
    Ok((cfg.target.clone(), role))
}

/// The destination for this call: the parameter wins over the configuration.
fn effective_target(requested: Option<String>, from_config: &str) -> Result<String, String> {
    match requested
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
    {
        Some(d) => config::normalize_locale(&d).map_err(|reason| {
            format!(
                "invalid target: {d:?} — {reason}. Use a locale code, e.g.: \"pt-BR\", \"en\""
            )
        }),
        None => Ok(from_config.to_string()),
    }
}

/// Where the registry lives: the caller's `dir`, or the configured `output`.
fn output_dir(dir: Option<String>) -> Result<std::path::PathBuf, String> {
    let env = config::Environment::real();
    match dir {
        Some(d) => Ok(config::expand_tilde(&d, &env)),
        None => config::resolve(None, &env)
            .map(|(_, cfg)| config::expand_tilde(&cfg.output, &env))
            .map_err(|e| {
                format!(
                    "could not resolve the configuration to find the output directory: {e}\n\
                     Pass `dir` explicitly, e.g.: {{\"document\": \"README\", \"dir\": \"~/.local/share/rustranslator\"}}"
                )
            }),
    }
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

    /// How the translation of one document is going.
    ///
    /// It reads the registry on disk — there is no in-memory state. A document that never
    /// started returns zero, and that is an answer, not an error: the MCP must not invent a
    /// total nobody declared.
    #[tool(
        description = "Progress of a document's translation: blocks processed, approved, corrected, undecided, how many remain, percentage and correction rate. Reads the registry on disk; it does not depend on anything running in memory."
    )]
    fn progress(
        &self,
        Parameters(ProgressParams { document, dir }): Parameters<ProgressParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir_path = match output_dir(dir) {
            Ok(d) => d,
            Err(warning) => return Ok(CallToolResult::error(vec![ContentBlock::text(warning)])),
        };

        match progress::Progress::read(&dir_path, &document) {
            Ok(p) => {
                let body = serde_json::json!({
                    "summary": p.summary(),
                    "directory": dir_path.display().to_string(),
                    "progress": p,
                });
                let text = serde_json::to_string_pretty(&body)
                    .unwrap_or_else(|e| format!("error serialising the progress: {e}"));
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "could not read the registry at {}: {e}",
                dir_path.display()
            ))])),
        }
    }

    /// Translates one piece of text into the target locale.
    ///
    /// It protects what must not be translated (whole markdown links, inline code, URLs,
    /// `{{placeholders}}`) and re-asks up to three times if the model drops a slot — a dropped
    /// slot is a lost link.
    #[tool(
        description = "Translates one piece of text into a locale. Protects markdown (whole links, inline code, URLs, {{placeholders}}) and re-asks if the model drops any. Without `target` it uses the configured one; with it, the parameter wins."
    )]
    async fn translate(
        &self,
        Parameters(TranslateParams { text, target }): Parameters<TranslateParams>,
    ) -> Result<CallToolResult, McpError> {
        let (config_target, role) = match role("translator") {
            Ok(pair) => pair,
            Err(warning) => return Ok(CallToolResult::error(vec![ContentBlock::text(warning)])),
        };
        let target = match effective_target(target, &config_target) {
            Ok(t) => t,
            Err(warning) => return Ok(CallToolResult::error(vec![ContentBlock::text(warning)])),
        };

        match llm::translate(&role, &target, &text).await {
            Ok(translated) => Ok(CallToolResult::success(vec![ContentBlock::text(translated)])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())])),
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
        Parameters(ReviewParams { source, translation, target }): Parameters<ReviewParams>,
    ) -> Result<CallToolResult, McpError> {
        let (config_target, role) = match role("reviewer") {
            Ok(pair) => pair,
            Err(warning) => return Ok(CallToolResult::error(vec![ContentBlock::text(warning)])),
        };
        let target = match effective_target(target, &config_target) {
            Ok(t) => t,
            Err(warning) => return Ok(CallToolResult::error(vec![ContentBlock::text(warning)])),
        };

        match llm::review(&role, &target, &source, &translation).await {
            Ok(verdict) => {
                let body = match verdict {
                    llm::Verdict::Approved => serde_json::json!({ "ok": true }),
                    llm::Verdict::Corrected(c) => {
                        serde_json::json!({ "ok": false, "correction": c })
                    }
                    llm::Verdict::Undecided { reason } => {
                        serde_json::json!({ "ok": null, "error": reason })
                    }
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())])),
        }
    }
}

/// Starts the MCP server over stdio and waits until the client closes.
pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "rustranslator mcp: serving over stdio (pid {})",
        std::process::id()
    );
    let service = Server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Starts the MCP server over HTTP (Streamable HTTP) — the mode **systemd** can keep alive.
///
/// Why this exists: a stdio server only lives while a client spawned it and holds the
/// stdin/stdout pipes. systemd keeps the process alive but hands pipes to nobody — so
/// "systemd + stdio" is impossible. Serving over HTTP is the only way for the service to live on
/// its own and answer any harness that points at the address.
pub async fn serve_http(address: &str) -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let mcp_service: StreamableHttpService<Server, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(Server),
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
