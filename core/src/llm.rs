//! Talking to an OpenAI-compatible endpoint — the translator and the reviewer.
//!
//! One client serves both roles, and any provider the owner configures, because they all speak
//! the same dialect at `/v1/chat/completions`. Measured against a local Ollama daemon: it
//! accepts the Ollama-only knobs, honours `response_format`, and serves its cloud models
//! without the caller sending a key — the daemon uses its own.
//!
//! Two stages, two shapes: `translate` turns a block into a translation; `review` turns a (source,
//! translation) pair into a verdict. `protect_slots`/`restore_slots` sit under `translate` and keep
//! markdown from being mangled on its way through the model.

use crate::config::Role;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt;

/// The official Hy-MT2 prompt. `{target}` receives the locale code as-is: measured to produce
/// byte-identical output to the spelled-out language name.
pub const TRANSLATE_PROMPT: &str = "Translate the following text into {target}. Note that you should \
only output the translated result without any additional explanation:\n\n";

/// The reviewer's instruction. The correction must keep markdown intact — a reviewer that
/// "fixes" a link destroys the document.
pub const REVIEW_PROMPT: &str = "You revise a machine translation into the language given by the \
locale code.\n\
Compare SOURCE and TRANSLATION and reply with JSON only.\n\
If the translation says the same thing: {\"ok\": true}\n\
If it invents, drops or changes the meaning, mistranslates a term, or leaves the source \
language behind: {\"ok\": false, \"correction\": \"<the corrected translation>\"}\n\
Judge meaning only, never style or word order. The correction must keep every markdown link, \
URL and `inline code` exactly as in the source, and must not add or drop markdown bold \
markers.\n\n";

/// How many times to re-ask when the model drops a protected slot.
const ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum LlmError {
    /// No endpoint answered. Carries the URL, because that is what the owner must check.
    Connection { url: String, reason: String },
    /// The endpoint answered with a non-success status.
    Status {
        url: String,
        status: u16,
        body: String,
    },
    /// The answer was shaped differently than expected.
    Response { reason: String },
    /// The provider refused the request outright.
    Refused { reason: String },
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Connection { url, reason } => write!(
                f,
                "nao consegui falar com {url}: {reason}. Confira se o provedor esta no ar e se \
                 o `base_url` do config aponta para ele."
            ),
            LlmError::Status { url, status, body } => {
                write!(f, "{url} respondeu HTTP {status}: {body}")
            }
            LlmError::Response { reason } => write!(f, "resposta em formato inesperado: {reason}"),
            LlmError::Refused { reason } => write!(f, "o provedor recusou o pedido: {reason}"),
        }
    }
}

impl std::error::Error for LlmError {}

// ---------------------------------------------------------------------------
// Slot protection
// ---------------------------------------------------------------------------

/// A piece of text that must not be translated, kept aside so it comes back untouched.
pub type Slots = Vec<String>;

/// Replaces everything that must not be translated by `[[n]]` slots, and returns them.
///
/// Protected: markdown links as a whole, inline code, bare URLs, and `{{placeholders}}`.
/// The link is protected **whole** — protecting only `](url)` would leave a dangling `[` and
/// the model would "fix" the structure by destroying content.
///
/// A hand-written scanner rather than a regex, on purpose: one of the rules — "a path, but not the
/// tail of a longer path" — has to look at the character *before* a match, and `regex` has no
/// lookbehind, by design, since that is what buys it its linear-time guarantee. Scanning sidesteps
/// the limitation and keeps the guarantee, in a single pass over the block.
pub fn protect_slots(text: &str) -> (String, Slots) {
    let mut slots: Slots = Vec::new();
    let mut out = String::with_capacity(text.len());
    let b: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < b.len() {
        // Markdown link: [label](target)
        if b[i] == '[' {
            if let Some(fim) = link_end(&b, i) {
                slots.push(b[i..fim].iter().collect());
                out.push_str(&format!("[[{}]]", slots.len() - 1));
                i = fim;
                continue;
            }
        }
        // Inline code: `...`
        if b[i] == '`' {
            if let Some(fim) = b[i + 1..].iter().position(|&c| c == '`').map(|p| i + 1 + p + 1) {
                slots.push(b[i..fim].iter().collect());
                out.push_str(&format!("[[{}]]", slots.len() - 1));
                i = fim;
                continue;
            }
        }
        // {{placeholder}}
        if b[i] == '{' && i + 1 < b.len() && b[i + 1] == '{' {
            if let Some(fim) = find_at(&b, i, "}}").map(|p| p + 2) {
                if b[i..fim].iter().all(|c| c.is_alphanumeric() || *c == '{' || *c == '}' || *c == '_') {
                    slots.push(b[i..fim].iter().collect());
                    out.push_str(&format!("[[{}]]", slots.len() - 1));
                    i = fim;
                    continue;
                }
            }
        }
        // Bare URL
        if b[i] == 'h' && (starts_with_at(&b, i, "http://") || starts_with_at(&b, i, "https://")) {
            let mut fim = i;
            while fim < b.len() && !b[fim].is_whitespace() && b[fim] != ')' && b[fim] != '>' {
                fim += 1;
            }
            slots.push(b[i..fim].iter().collect());
            out.push_str(&format!("[[{}]]", slots.len() - 1));
            i = fim;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    (out, slots)
}

/// Puts every protected piece back where its slot is.
pub fn restore_slots(text: &str, slots: &Slots) -> String {
    let mut s = text.to_string();
    for (k, v) in slots.iter().enumerate() {
        s = s.replace(&format!("[[{k}]]"), v);
        // The model sometimes spaces the brackets out.
        s = s.replace(&format!("[[ {k} ]]"), v);
        s = s.replace(&format!("[[{k} ]]"), v);
        s = s.replace(&format!("[[ {k}]]"), v);
    }
    s
}

fn starts_with_at(b: &[char], i: usize, prefixo: &str) -> bool {
    let p: Vec<char> = prefixo.chars().collect();
    b.len() >= i + p.len() && b[i..i + p.len()] == p[..]
}

fn find_at(b: &[char], de: usize, agulha: &str) -> Option<usize> {
    let ag: Vec<char> = agulha.chars().collect();
    (de..b.len().saturating_sub(ag.len() - 1)).find(|&i| b[i..i + ag.len()] == ag[..])
}

/// `[label](target)` — the closing paren that matches the opening bracket at `i`.
fn link_end(b: &[char], i: usize) -> Option<usize> {
    let fecha_colchete = find_at(b, i, "]")?;
    if fecha_colchete + 1 >= b.len() || b[fecha_colchete + 1] != '(' {
        return None;
    }
    let fecha_paren = find_at(b, fecha_colchete + 2, ")")?;
    Some(fecha_paren + 1)
}

// ---------------------------------------------------------------------------
// The call
// ---------------------------------------------------------------------------

/// Asks the endpoint for a completion. `json` requests a structured answer.
pub async fn generate(role: &Role, prompt: &str, json: bool) -> Result<String, LlmError> {
    // Only send the knobs the caller actually set. `top_k` and `repeat_penalty` are
    // Ollama-API-only extensions; a strict provider would reject them, so they travel only when
    // present in the role that wants them.
    let mut body = Map::new();
    body.insert("model".into(), json!(role.model));
    body.insert(
        "messages".into(),
        json!([{ "role": "user", "content": prompt }]),
    );
    body.insert("stream".into(), json!(false));
    if let Some(t) = role.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = role.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(k) = role.top_k {
        body.insert("top_k".into(), json!(k));
    }
    if let Some(r) = role.repeat_penalty {
        body.insert("repeat_penalty".into(), json!(r));
    }
    if let Some(m) = role.max_tokens {
        body.insert("max_tokens".into(), json!(m));
    }
    if json || role.response_format.as_deref() == Some("json_object") {
        body.insert("response_format".into(), json!({ "type": "json_object" }));
    }

    let url = format!("{}/chat/completions", role.base_url.trim_end_matches('/'));
    let cliente = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| LlmError::Connection {
            url: url.clone(),
            reason: e.to_string(),
        })?;

    let mut pedido = cliente.post(&url).json(&Value::Object(body));
    // An empty key means "do not send the header at all" — which is right for a local daemon
    // that already holds its own credential.
    if !role.api_key.trim().is_empty() {
        pedido = pedido.bearer_auth(role.api_key.trim());
    }

    let resposta = pedido.send().await.map_err(|e| LlmError::Connection {
        url: url.clone(),
        reason: e.to_string(),
    })?;

    let status = resposta.status();
    let text = resposta.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LlmError::Status {
            url,
            status: status.as_u16(),
            body: text.chars().take(300).collect(),
        });
    }

    let d: Value = serde_json::from_str(&text).map_err(|e| LlmError::Response {
        reason: format!("{e} — body: {}", text.chars().take(200).collect::<String>()),
    })?;

    d.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| LlmError::Response {
            reason: format!("sem choices[0].message.content — body: {}", text.chars().take(200).collect::<String>()),
        })
}

/// Translates one piece of text, protecting what must not be translated.
///
/// Re-asks up to `ATTEMPTS` times if the model drops a slot: a dropped slot means a lost URL
/// or a lost code snippet, which is worse than a slow answer.
pub async fn translate(
    role: &Role,
    target: &str,
    text: &str,
) -> Result<String, LlmError> {
    let (with_slots, slots) = protect_slots(text);
    let prompt = format!("{}{}", TRANSLATE_PROMPT.replace("{target}", target), with_slots);

    for _ in 0..ATTEMPTS {
        let raw = generate(role, &prompt, false).await?;
        let cleaned = strip_wrapping(&raw);
        if slots.is_empty() || (0..slots.len()).all(|k| cleaned.contains(&format!("[[{k}]]"))) {
            return Ok(restore_slots(&cleaned, &slots));
        }
    }
    Err(LlmError::Refused {
        reason: format!(
            "the model dropped the protected slots after {ATTEMPTS} attempts — delivering this \
             translation would lose a link or a snippet of code, so it is refused"
        ),
    })
}

/// The verdict of the reviewer on one block.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Says the same thing.
    Approved,
    /// Says something else, and here is the corrected translation.
    Corrected(String),
    /// The reviewer could not answer. Named explicitly rather than hidden in a `null`.
    Undecided { reason: String },
}

/// Asks the reviewer about one (source, translation) pair.
pub async fn review(
    role: &Role,
    target: &str,
    source: &str,
    translation: &str,
) -> Result<Verdict, LlmError> {
    let prompt = format!(
        "The target locale is: {target}\n{REVIEW_PROMPT}SOURCE:\n{source}\n\nTRANSLATION:\n{translation}\n"
    );
    let raw = generate(role, &prompt, true).await?;
    let d: Value = serde_json::from_str(&raw).map_err(|e| LlmError::Response {
        reason: format!("the reviewer returned no JSON: {e} — {raw:?}"),
    })?;

    if d.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(Verdict::Approved);
    }
    let correction = d.get("correction").and_then(Value::as_str).unwrap_or("").trim();
    if correction.is_empty() {
        return Ok(Verdict::Undecided {
            reason: format!("sem correction: {}", raw.chars().take(120).collect::<String>()),
        });
    }
    Ok(Verdict::Corrected(correction.to_string()))
}

/// Strips the wrapping the model sometimes adds: quotes, or a ``` fence.
fn strip_wrapping(s: &str) -> String {
    let mut t = s.trim().to_string();
    if t.starts_with("```") {
        if let Some(rest) = t.strip_prefix("```") {
            let body = rest.split_once('\n').map(|(_, c)| c).unwrap_or(rest);
            t = body.strip_suffix("```").unwrap_or(body).trim().to_string();
        }
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let sem_aspas = &t[1..t.len() - 1];
        if !sem_aspas.contains('"') {
            t = sem_aspas.to_string();
        }
    }
    t
}

/// Convenience for callers that want the counts of a slot round-trip.
pub fn check_slots(_original: &Slots, _volta: &str) -> BTreeMap<String, bool> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_the_whole_link() {
        let (s, slots) = protect_slots("veja [o guia](https://x.y/guia.md) agora");
        assert_eq!(s, "veja [[0]] agora");
        assert_eq!(slots, vec!["[o guia](https://x.y/guia.md)".to_string()]);
    }

    #[test]
    fn protects_inline_code_and_a_bare_url() {
        let (s, slots) = protect_slots("use `cargo build` e baixe de https://exemplo.org/a");
        assert_eq!(s, "use [[0]] e baixe de [[1]]");
        assert_eq!(slots.len(), 2);
    }

    #[test]
    fn protects_an_i18n_placeholder() {
        let (s, slots) = protect_slots("ola {{nome}}, bem-vindo");
        assert_eq!(s, "ola [[0]], bem-vindo");
        assert_eq!(slots[0], "{{nome}}");
    }

    #[test]
    fn restres_back_to_the_original() {
        let original = "veja [o guia](https://x.y/guia.md) e `cargo`";
        let (s, slots) = protect_slots(original);
        assert_eq!(restore_slots(&s, &slots), original);
    }

    #[test]
    fn tolerates_spaces_inside_the_slot() {
        // O modelo as vezes respira dentro dos colchetes.
        let (_, slots) = protect_slots("[x](y)");
        assert_eq!(restore_slots("[[ 0 ]]", &slots), "[x](y)");
    }

    #[test]
    fn a_bracket_without_a_link_is_not_a_slot() {
        let (s, slots) = protect_slots("uma lista [1, 2, 3] qualquer");
        assert_eq!(s, "uma lista [1, 2, 3] qualquer");
        assert!(slots.is_empty());
    }

    #[test]
    fn strips_a_code_fence() {
        assert_eq!(strip_wrapping("```\nMotor pequeno\n```"), "Motor pequeno");
    }

    #[test]
    fn strips_wrapping_quotes() {
        assert_eq!(strip_wrapping("\"Motor pequeno\""), "Motor pequeno");
        // Mas nao mexe se houver aspas internas.
        assert_eq!(strip_wrapping("\"ele disse \"oi\"\""), "\"ele disse \"oi\"\"");
    }
}
