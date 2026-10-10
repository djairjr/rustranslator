//! The loop: it walks a document, translates it block by block, records every block, and writes
//! the translated document back out.
//!
//! ## The shape of a run
//!
//! 1. the document is parsed into units to translate — paragraphs, headings, table cells;
//! 2. the registry on disk is read, and every unit already in it is skipped — this is the
//!    resumption, and it is why a 400-block document does not have to be done in one sitting;
//! 3. each remaining unit goes through the translator and then the reviewer; the verdict is
//!    appended to the registry immediately, so nothing done is ever lost;
//! 4. the document is rebuilt by splicing the whole set of translations — the ones from the
//!    registry and the ones from this run — back into the source, and written next to it;
//! 5. the flagged blocks, and only those, are written to a report. That is the residue a person
//!    reads.
//!
//! ## What stops a run
//!
//! A block whose protected slot the model dropped is **recorded and skipped**, not fatal: the loss
//! is local to that block and a person will see it in the report. Anything else — a dead endpoint, a
//! refused connection — aborts the run, because if the server is down then every remaining block
//! will fail the same way, and grinding through 400 of them to say so is not progress.
//!
//! ## Cancellation
//!
//! The caller owns an `AtomicBool`. It is read between blocks — never in the middle of one, since
//! a half-recorded block is worse than a block not started. When it is set, the run stops, the
//! registry keeps everything done so far, the document is still rebuilt from what is there, and
//! the task is marked cancelled. Stopping is cheap, and resuming is what the registry is for.

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{self, Config};
use crate::llm::{self, LlmError, Verdict};
use crate::output;
use crate::parser::{self, Block};
use crate::progress::{Situation, Task};
use crate::registry::{self, Record};

/// What the caller decides, and what the engine is not allowed to guess.
#[derive(Debug, Clone)]
pub struct Options {
    /// Overrides the configured target locale. Already normalised by the caller.
    pub target: Option<String>,
    /// Skip the first N units (`--from`).
    pub from: usize,
    /// Translate at most N units; `0` means all of them (`--limit`).
    pub limit: usize,
    /// Ignore the registry and translate everything again.
    pub force: bool,
    /// Where the registry, the task file and the report live. Resolved by the caller, which is the
    /// only layer that knows the environment.
    pub out_dir: PathBuf,
    /// Where the translated document goes; `None` means next to the source.
    pub doc_destination: Option<String>,
    /// Print a line per block as it is done.
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// How many units the document has, in total.
    pub units: usize,
    /// How many this run translated.
    pub translated: usize,
    /// How many were already in the registry and were skipped.
    pub skipped: usize,
    pub approved: usize,
    pub corrected: usize,
    pub undecided: usize,
    /// How many blocks the reviewer never saw, because a protected slot was lost.
    pub lost: usize,
    /// True when the run stopped early because the caller cancelled it.
    pub cancelled: bool,
    /// The translated document.
    pub output: PathBuf,
    /// The report of flagged blocks.
    pub report: PathBuf,
    pub registry: PathBuf,
}

#[derive(Debug)]
pub enum EngineError {
    Io(std::io::Error),
    Llm(llm::LlmError),
    Destination(String),
    Parse(String),
    MissingStem(PathBuf),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Io(e) => write!(f, "{e}"),
            EngineError::Llm(e) => write!(f, "{e}"),
            EngineError::Destination(e) => write!(f, "{e}"),
            EngineError::Parse(e) => write!(f, "could not read the markdown: {e}"),
            EngineError::MissingStem(p) => {
                write!(f, "{} has no file name to build the output name from", p.display())
            }
        }
    }
}

impl std::error::Error for EngineError {}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        EngineError::Io(e)
    }
}
impl From<llm::LlmError> for EngineError {
    fn from(e: llm::LlmError) -> Self {
        EngineError::Llm(e)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_task(path: &Path, task: &Task) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(task).unwrap_or_default())
}

/// The units this run has to translate: everything, minus `from`, capped at `limit`.
fn selected(units: &[Block], from: usize, limit: usize) -> Vec<usize> {
    let iter = (from.min(units.len()))..units.len();
    match limit {
        0 => iter.collect(),
        n => iter.take(n).collect(),
    }
}

/// The record for a block whose protected slot the model dropped. The text is not kept: a
/// translation missing a link or a path is not a translation, and keeping it would invite someone
/// to ship it.
fn lost_record(id: String, unit: &Block, reason: String) -> Record {
    Record {
        id,
        line: unit.line,
        source: unit.text.clone(),
        translation: None,
        ok: None,
        correction: None,
        final_text: None,
        error: Some(reason),
    }
}

/// Runs the whole thing. See the module docs for the shape of a run.
pub async fn run(
    document: &Path,
    cfg: &Config,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<Summary, EngineError> {
    let target = match &options.target {
        Some(t) => config::normalize_locale(t).map_err(EngineError::Destination)?,
        None => cfg.target.clone(),
    };

    let text = std::fs::read_to_string(document)?;
    let units: Vec<Block> = parser::parse(&text).map_err(EngineError::Parse)?;

    let stem = document
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| EngineError::MissingStem(document.to_path_buf()))?;

    let registry_path = registry::registry_path(&options.out_dir, stem);
    let task_path = registry::task_path(&options.out_dir, stem);

    // The registry is read once, here, and it answers two questions: what is already done, and what
    // the document already says — the second one matters because rebuilding needs the earlier
    // translations too, not only this run's.
    let recorded = registry::load(&registry_path);
    let mut translations: Vec<Option<String>> = units
        .iter()
        .map(|u| {
            recorded
                .get(&registry::stable_id(u.line, &u.text))
                .and_then(|r| r.final_text.clone().or_else(|| r.translation.clone()))
        })
        .collect();

    let todo = selected(&units, options.from, options.limit);
    write_task(
        &task_path,
        &Task {
            document: document.display().to_string(),
            targets: units.len(),
            pid: Some(std::process::id()),
            started_at: Some(now_secs()),
            situation: Situation::Running,
        },
    )?;

    let output_path =
        output::resolve_destination(document, &target, options.doc_destination.as_deref())
            .map_err(EngineError::Destination)?;
    let report_path = options.out_dir.join(format!("{stem}.review.md"));

    let mut summary = Summary {
        units: units.len(),
        translated: 0,
        skipped: 0,
        approved: 0,
        corrected: 0,
        undecided: 0,
        lost: 0,
        cancelled: false,
        output: output_path.clone(),
        report: report_path.clone(),
        registry: registry_path.clone(),
    };

    let mut cancelled = false;
    for index in todo {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let unit = &units[index];
        let id = registry::stable_id(unit.line, &unit.text);

        // Already done, and we were not asked to redo it: the translation is already in
        // `translations`, so there is nothing to write and nothing to pay for.
        if !options.force && recorded.contains_key(&id) {
            summary.skipped += 1;
            continue;
        }

        let record = match llm::translate(&cfg.translator, &target, &unit.text).await {
            // A lost slot is local: record it, flag it, carry on.
            Err(LlmError::Refused { reason }) => {
                summary.lost += 1;
                lost_record(id.clone(), unit, reason)
            }
            // Anything else is the endpoint failing, and it will fail for every remaining block
            // too. Say so now instead of grinding through the rest of the document.
            Err(e) => return Err(EngineError::Llm(e)),
            Ok(machine) => match llm::review(&cfg.reviewer, &target, &unit.text, &machine).await? {
                Verdict::Approved => {
                    summary.approved += 1;
                    Record {
                        id: id.clone(),
                        line: unit.line,
                        source: unit.text.clone(),
                        translation: Some(machine.clone()),
                        ok: Some(true),
                        correction: None,
                        final_text: Some(machine),
                        error: None,
                    }
                }
                Verdict::Corrected(correction) => {
                    summary.corrected += 1;
                    Record {
                        id: id.clone(),
                        line: unit.line,
                        source: unit.text.clone(),
                        translation: Some(machine),
                        ok: Some(false),
                        correction: Some(correction.clone()),
                        final_text: Some(correction),
                        error: None,
                    }
                }
                Verdict::Undecided { reason } => {
                    summary.undecided += 1;
                    Record {
                        id: id.clone(),
                        line: unit.line,
                        source: unit.text.clone(),
                        translation: Some(machine.clone()),
                        ok: None,
                        correction: None,
                        // Without a verdict the machine's text is all there is; a person decides.
                        final_text: Some(machine),
                        error: Some(reason),
                    }
                }
            },
        };

        registry::append(&registry_path, &record)?;
        translations[index] = record.final_text.clone().or_else(|| record.translation.clone());
        summary.translated += 1;

        if options.verbose {
            let mark = match record.ok {
                Some(true) => '.',
                Some(false) => 'C',
                None => '?',
            };
            let text: String = unit.text.chars().take(56).collect();
            eprintln!("  [{}] {mark}  {text}", summary.translated);
        }
    }

    let rebuilt = parser::render(&text, &units, &translations);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&output_path, rebuilt)?;

    // The report is written from the registry, freshly loaded, so it reflects the whole document
    // and not only this run.
    write_report(&report_path, stem, &units, &registry::load(&registry_path))?;

    write_task(
        &task_path,
        &Task {
            document: document.display().to_string(),
            targets: units.len(),
            pid: Some(std::process::id()),
            started_at: Some(now_secs()),
            situation: if cancelled {
                summary.cancelled = true;
                Situation::Cancelled
            } else {
                Situation::Finished
            },
        },
    )?;

    Ok(summary)
}

/// The report: the flagged blocks, and only those.
///
/// This is where the whole arrangement pays off. The reviewer exists so that a person reads the
/// *residue* and not the document: if this file listed every block, having a reviewer would have
/// bought nothing. Three things land here — a block the reviewer corrected, a block it could not
/// judge, and a block whose translation was refused — and each one carries its source, so the
/// verdict can be checked against something.
pub fn write_report(
    path: &Path,
    stem: &str,
    units: &[Block],
    recorded: &HashMap<String, Record>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut flagged: Vec<(&Block, Option<&Record>)> = Vec::new();
    for unit in units {
        let record = recorded.get(&registry::stable_id(unit.line, &unit.text));
        match record {
            Some(r) if r.ok == Some(true) => {} // approved: not residue
            _ => flagged.push((unit, record)),
        }
    }

    let mut out = String::new();
    out.push_str(&format!("# Review — {stem}\n\n"));
    out.push_str(&format!(
        "{} blocks, {} flagged.\n\n",
        units.len(),
        flagged.len()
    ));
    out.push_str(
        "The registry holds one JSON line per block, with every verdict. This file holds only the \
         blocks a person still has to look at.\n\n",
    );

    for (unit, record) in flagged {
        let (state, detail) = match record {
            None => ("not translated".to_string(), String::new()),
            Some(r) if r.translation.is_none() => (
                "translation refused".to_string(),
                r.error.clone().unwrap_or_default(),
            ),
            Some(r) if r.ok.is_none() => (
                "no verdict".to_string(),
                r.error.clone().unwrap_or_default(),
            ),
            Some(r) => (
                "corrected".to_string(),
                r.correction.clone().unwrap_or_default(),
            ),
        };
        out.push_str(&format!("## line {} · {state}\n\n", unit.line));
        out.push_str(&format!("**source:** {}\n\n", unit.text));
        if let Some(r) = record {
            if let Some(machine) = &r.translation {
                if machine != &unit.text {
                    out.push_str(&format!("**machine:** {machine}\n\n"));
                }
            }
        }
        if !detail.is_empty() {
            out.push_str(&format!("**{state}:** {detail}\n\n"));
        }
    }

    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn block(line: usize, text: &str) -> Block {
        Block {
            line,
            kind: parser::Kind::Prose,
            text: text.to_string(),
            // The report only reads `line` and `text`; the span is what the splicing needs, and
            // these fixtures never reach it.
            start: 0,
            end: 0,
        }
    }

    #[test]
    fn the_selection_honours_from_and_limit() {
        let units: Vec<Block> = (0..10).map(|i| block(i, &format!("u{i}"))).collect();
        assert_eq!(selected(&units, 0, 0).len(), 10);
        assert_eq!(selected(&units, 3, 0), vec![3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(selected(&units, 0, 4), vec![0, 1, 2, 3]);
        assert_eq!(selected(&units, 8, 4), vec![8, 9]);
        // `from` past the end is empty, not a panic.
        assert!(selected(&units, 99, 0).is_empty());
    }

    fn approved(line: usize, text: &str) -> Record {
        Record {
            id: registry::stable_id(line, text),
            line,
            source: text.to_string(),
            translation: Some("translated".to_string()),
            ok: Some(true),
            correction: None,
            final_text: Some("translated".to_string()),
            error: None,
        }
    }

    #[test]
    fn the_report_lists_the_residue_and_never_the_approved() {
        let dir = std::env::temp_dir().join("rustranslator-report-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("r.md");

        let units = vec![
            block(1, "approved source"),
            block(2, "corrected source"),
            block(3, "never done"),
        ];
        let mut recorded = HashMap::new();
        recorded.insert(registry::stable_id(1, "approved source"), approved(1, "approved source"));
        let mut corrected = approved(2, "corrected source");
        corrected.ok = Some(false);
        corrected.correction = Some("the right version".to_string());
        corrected.final_text = Some("the right version".to_string());
        recorded.insert(registry::stable_id(2, "corrected source"), corrected);

        write_report(&path, "doc", &units, &recorded).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        assert!(text.contains("3 blocks, 2 flagged"), "{text}");
        assert!(text.contains("corrected source"), "{text}");
        assert!(text.contains("the right version"), "{text}");
        assert!(text.contains("never done"), "{text}");
        assert!(!text.contains("approved source"), "an approved block was listed: {text}");
    }

    #[test]
    fn a_refused_translation_says_why() {
        let dir = std::env::temp_dir().join("rustranslator-report-refused");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("r.md");
        let units = vec![block(5, "a source")];
        let mut recorded = HashMap::new();
        recorded.insert(
            registry::stable_id(5, "a source"),
            lost_record(
                registry::stable_id(5, "a source"),
                &units[0],
                "a protected slot was lost".to_string(),
            ),
        );
        write_report(&path, "doc", &units, &recorded).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("translation refused"), "{text}");
        assert!(text.contains("a protected slot was lost"), "{text}");
    }
}
