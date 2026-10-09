//! Configuration: finds the file, reads it, validates it, and can EXPLAIN what went wrong
//! and where.
//!
//! Search order — the first one that exists wins:
//!
//!   1. `--config <path>`
//!   2. `$RUSTRANSLATOR_CONFIG`
//!   3. `$XDG_CONFIG_HOME/rustranslator/config.json` (or `~/.config/...`)
//!   4. `/etc/rustranslator/config.json`
//!
//! The token lives in the user's path (0600). The system file is the default the .deb
//! installs, with no secret in it.
//!
//! **The environment comes in as a parameter.** No function in this logic reads an
//! environment variable on its own: the only reader is `Environment::real()`, called by the
//! binary alone. That keeps path resolution pure, testable and safe under parallel tests —
//! mutating the process environment inside a test is a trap.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

pub const APP: &str = "rustranslator";
pub const ENV_CONFIG: &str = "RUSTRANSLATOR_CONFIG";
pub const DEFAULT_TARGET: &str = "pt-BR";
pub const DEFAULT_OUTPUT: &str = "~/.local/share/rustranslator";

// ---------------------------------------------------------------------------
// The environment, injected
// ---------------------------------------------------------------------------

/// What path resolution needs to know about the world. It always comes from outside.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub env_config: Option<PathBuf>,
}

impl Environment {
    /// Reads the process's REAL environment. This is the only place in the module that touches
    /// the environment — and nothing in this library calls it; the binary does.
    pub fn real() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        Self {
            home: var("HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            env_config: var(ENV_CONFIG),
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// One role (translator or reviewer): an OpenAI-compatible endpoint, plus the knobs that role
/// accepts.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub base_url: String,
    /// Empty means "do NOT send the Authorization header" — which is right for a local Ollama
    /// daemon, who already holds its own credential.
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Only Ollama accepts this. That is why it lives in the translator role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Same: only Ollama accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// E.g. "json_object", so the reviewer answers with structured JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Locale code (BCP 47): "pt-BR", "en", "es-419". It goes straight into the prompt.
    #[serde(default = "default_target")]
    pub target: String,
    pub translator: Role,
    pub reviewer: Role,
    #[serde(default = "default_output")]
    pub output: String,
    /// Where the translated document lands: `next_to_original` (the default) or a path.
    /// A call may override this value — the call wins.
    #[serde(default = "default_doc_destination")]
    pub doc_destination: String,
}

fn default_target() -> String {
    DEFAULT_TARGET.to_string()
}
fn default_output() -> String {
    DEFAULT_OUTPUT.to_string()
}
fn default_doc_destination() -> String {
    crate::output::NEXT_TO_ORIGINAL.to_string()
}

// ---------------------------------------------------------------------------
// Errors: each one says where to look, or what to fix.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    /// No file existed. Names EVERY path tried.
    NoFile { tried: Vec<PathBuf> },
    Read { path: PathBuf, source: std::io::Error },
    /// Invalid JSON or an unknown field. Names the path, the line and the column.
    Json {
        path: PathBuf,
        line: usize,
        column: usize,
        msg: String,
    },
    Locale { value: String, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoFile { tried } => {
                writeln!(f, "no configuration file was found. Looked in:")?;
                for p in tried {
                    writeln!(f, "  {}", p.display())?;
                }
                write!(
                    f,
                    "create one of them, or pass --config <path>, or set ${ENV_CONFIG}"
                )
            }
            ConfigError::Read { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            ConfigError::Json { path, line, column, msg } => write!(
                f,
                "{} is invalid at line {line}, column {column}: {msg}",
                path.display()
            ),
            ConfigError::Locale { value, reason } => write!(
                f,
                "invalid target: {value:?} — {reason}. Use a locale code, e.g.: \
                 \"pt-BR\", \"en\", \"es-419\""
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// Locale
// ---------------------------------------------------------------------------

/// Normalises and validates a locale code in the reduced BCP 47 form: `ll` or `ll-XX`.
///
/// It accepts the POSIX form with an underscore (`pt_BR`) and loose casing (`PT-br`), because
/// that is how the owner and the system tools write it. Refusing over an `_` would be hostile.
pub fn normalize_locale(value: &str) -> Result<String, String> {
    let v = value.trim().replace('_', "-");
    let mut parts = v.split('-');
    let language = parts.next().unwrap_or_default();
    let region = parts.next();

    let two_letters = |s: &str| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic());

    if !two_letters(language) {
        return Err("the language must be two letters".to_string());
    }
    let mut out = language.to_ascii_lowercase();
    if let Some(r) = region {
        if !two_letters(r) {
            return Err("the region must be two letters".to_string());
        }
        out.push('-');
        out.push_str(&r.to_ascii_uppercase());
    }
    if parts.next().is_some() {
        return Err("expected 'll' or 'll-XX'".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Paths — pure: everything comes from `Environment`
// ---------------------------------------------------------------------------

/// A leading `~` becomes the environment's home. Nothing else is touched.
pub fn expand_tilde(p: &str, env: &Environment) -> PathBuf {
    let rest = p.strip_prefix("~").map(|r| r.strip_prefix('/').unwrap_or(r));
    match (rest, &env.home) {
        (Some(r), Some(home)) => home.join(r),
        _ => PathBuf::from(p),
    }
}

/// `$XDG_CONFIG_HOME/rustranslator/config.json`, falling back to `~/.config/...`.
pub fn user_config_path(env: &Environment) -> Option<PathBuf> {
    let base = env
        .xdg_config_home
        .clone()
        .or_else(|| env.home.as_ref().map(|h| h.join(".config")))?;
    Some(base.join(APP).join("config.json"))
}

pub fn system_config_path() -> PathBuf {
    PathBuf::from("/etc").join(APP).join("config.json")
}

/// The paths to try, in order. The environment one only enters if it is defined.
pub fn candidates(explicit: Option<&Path>, env: &Environment) -> Vec<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .into_iter()
        .chain(env.env_config.clone())
        .chain(user_config_path(env))
        .chain(std::iter::once(system_config_path()))
        .collect()
}

/// Finds the first candidate that exists, reads it and validates it. It returns the originating
/// path too, so the program can say where the configuration in force came from.
pub fn resolve(
    explicit: Option<&Path>,
    env: &Environment,
) -> Result<(PathBuf, Config), ConfigError> {
    let tried = candidates(explicit, env);
    let Some(path) = tried.iter().find(|p| p.is_file()).cloned() else {
        return Err(ConfigError::NoFile { tried });
    };

    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;

    let mut cfg: Config = serde_json::from_str(&text).map_err(|e| ConfigError::Json {
        path: path.clone(),
        line: e.line(),
        column: e.column(),
        msg: e.to_string(),
    })?;

    cfg.target = normalize_locale(&cfg.target).map_err(|reason| ConfigError::Locale {
        value: cfg.target.clone(),
        reason,
    })?;

    Ok((path, cfg))
}

/// A warning (not an error) when the file holding a token is readable by others.
pub fn permission_warning(path: &Path, cfg: &Config) -> Option<String> {
    let has_secret = !cfg.translator.api_key.is_empty() || !cfg.reviewer.api_key.is_empty();
    if !has_secret {
        return None;
    }
    let mode = file_mode(path)?;
    if mode & 0o077 == 0 {
        return None;
    }
    Some(format!(
        "warning: {} holds a token and its mode is {mode:o} — other users on this machine can \
         read it. Recommended: chmod 600 {}",
        path.display(),
        path.display()
    ))
}

fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Shows just enough for the owner to recognise the key, never the key itself.
pub fn mask_secret(secret: &str) -> String {
    let n = secret.chars().count();
    match n {
        0 => "(empty)".to_string(),
        1..=8 => "•".repeat(n),
        _ => {
            let head: String = secret.chars().take(3).collect();
            let tail: String = secret.chars().skip(n - 4).collect();
            format!("{head}…{tail}")
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — none of them touches the process environment.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake environment, complete and deterministic.
    fn fake_env() -> Environment {
        Environment {
            home: Some(PathBuf::from("/home/fake")),
            xdg_config_home: None,
            env_config: None,
        }
    }

    /// Creates an isolated test directory and returns the config path inside it.
    fn test_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustranslator-test-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, content).unwrap();
        file
    }

    const OK: &str = r#"{
  "target": "pt_BR",
  "translator": { "base_url": "http://127.0.0.1:11434/v1", "model": "hy-mt2-7b", "top_k": 20 },
  "reviewer": { "base_url": "http://127.0.0.1:11434/v1", "model": "deepseek-v4.1-flash:cloud" }
}"#;

    #[test]
    fn normalizes_the_posix_form() {
        assert_eq!(normalize_locale("pt_BR").unwrap(), "pt-BR");
        assert_eq!(normalize_locale("pt-BR").unwrap(), "pt-BR");
        assert_eq!(normalize_locale("PT-br").unwrap(), "pt-BR");
        assert_eq!(normalize_locale("en").unwrap(), "en");
    }

    #[test]
    fn rejects_a_malformed_locale() {
        for bad in ["portuguese-br", "pt-BRA", "p1", "", "pt-BR-x"] {
            assert!(normalize_locale(bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(
            normalize_locale("es-419"),
            Err("the region must be two letters".to_string())
        );
    }

    #[test]
    fn tilde_expansion() {
        let env = fake_env();
        assert_eq!(expand_tilde("~/x/y", &env), PathBuf::from("/home/fake/x/y"));
        assert_eq!(expand_tilde("/absolute", &env), PathBuf::from("/absolute"));
        // With no known home the path stays literal — we do not invent one.
        let no_home = Environment::default();
        assert_eq!(expand_tilde("~/x", &no_home), PathBuf::from("~/x"));
    }

    #[test]
    fn the_user_config_path_prefers_xdg() {
        let env = Environment {
            home: Some(PathBuf::from("/home")),
            xdg_config_home: Some(PathBuf::from("/etc/xdg-of-the-user")),
            env_config: None,
        };
        assert_eq!(
            user_config_path(&env).unwrap(),
            PathBuf::from("/etc/xdg-of-the-user/rustranslator/config.json")
        );
        assert_eq!(
            user_config_path(&fake_env()).unwrap(),
            PathBuf::from("/home/fake/.config/rustranslator/config.json")
        );
    }

    #[test]
    fn candidate_order_without_env() {
        let c = candidates(Some(Path::new("/tmp/explicit.json")), &fake_env());
        assert_eq!(
            c,
            vec![
                PathBuf::from("/tmp/explicit.json"),
                PathBuf::from("/home/fake/.config/rustranslator/config.json"),
                PathBuf::from("/etc/rustranslator/config.json"),
            ]
        );
    }

    #[test]
    fn candidate_order_with_env() {
        let env = Environment {
            env_config: Some(PathBuf::from("/tmp/from-env.json")),
            ..fake_env()
        };
        let c = candidates(None, &env);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], PathBuf::from("/tmp/from-env.json"));
        assert_eq!(c.last().unwrap(), &system_config_path());
    }

    #[test]
    fn an_unknown_field_errors_and_names_the_line() {
        let file = test_file(
            "typo",
            r#"{
  "translator": {
    "base_ur": "http://127.0.0.1:11434/v1",
    "model": "hy-mt2-7b"
  },
  "reviewer": { "base_url": "x", "model": "y" }
}"#,
        );
        let msg = resolve(Some(&file), &fake_env()).unwrap_err().to_string();
        assert!(msg.contains("config.json"), "msg: {msg}");
        assert!(msg.contains("line 3"), "msg: {msg}");
        assert!(msg.contains("base_ur"), "msg: {msg}");
    }

    #[test]
    fn a_missing_file_lists_every_path_tried() {
        let env = Environment {
            env_config: Some(PathBuf::from("/tmp/does-not-exist.json")),
            ..fake_env()
        };
        let e = resolve(None, &env).unwrap_err();
        match e {
            ConfigError::NoFile { tried } => {
                assert_eq!(tried.len(), 3);
                let text = ConfigError::NoFile { tried }.to_string();
                assert!(text.contains("/etc/rustranslator/config.json"), "text: {text}");
            }
            other => panic!("expected NoFile, got {other:?}"),
        }
    }

    #[test]
    fn masks_the_token() {
        assert_eq!(mask_secret(""), "(empty)");
        assert_eq!(mask_secret("abc"), "•••");
        // An EXAMPLE value, not a key: a literal that looks like a real key in a public
        // repository alarms scanners and readers for no reason.
        assert_eq!(mask_secret("EXAMPLE-not-a-key-0000"), "EXA…0000");
    }

    #[test]
    fn reads_a_full_config_and_normalizes_the_target() {
        let file = test_file("ok", OK);
        let (path, cfg) = resolve(Some(&file), &fake_env()).unwrap();
        assert_eq!(path, file);
        assert_eq!(cfg.target, "pt-BR");
        assert_eq!(cfg.translator.model, "hy-mt2-7b");
        assert_eq!(cfg.translator.top_k, Some(20));
        assert_eq!(cfg.output, DEFAULT_OUTPUT);
    }

    #[test]
    fn precedence_honours_the_explicit_file() {
        // Two valid and DIFFERENT files: the explicit one must beat the environment one.
        let explicit = test_file("precedence-explicit", OK);
        let from_env = test_file(
            "precedence-env",
            r#"{
  "target": "en",
  "translator": { "base_url": "http://127.0.0.1:11434/v1", "model": "other" },
  "reviewer": { "base_url": "http://127.0.0.1:11434/v1", "model": "other" }
}"#,
        );
        let env = Environment {
            env_config: Some(from_env),
            ..fake_env()
        };
        let (path, cfg) = resolve(Some(&explicit), &env).unwrap();
        assert_eq!(path, explicit);
        assert_eq!(cfg.translator.model, "hy-mt2-7b");
        assert_eq!(cfg.target, "pt-BR");
    }
}
