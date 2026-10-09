//! Task progress: the structure that answers "how is the work going?".
//!
//! Two sources, and both are files — no hidden in-memory state:
//!
//! * the **registry** (`<base>.pipeline.jsonl`), one line per block already processed, each
//!   carrying the reviewer's verdict;
//! * the **task** (`<base>.task.json`), optional, holding what only the launcher knows: how
//!   many targets the document has, when it started, and whether it is still running.
//!
//! The registry alone answers "how much is done". The task answers "how much is left". With no
//! task, the progress is honest about not knowing the total — instead of inventing one.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Where the task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Situation {
    /// No task record: only what the registry shows is known.
    #[default]
    Unknown,
    Running,
    Finished,
    Cancelled,
    Failed,
}

impl Situation {
    pub fn as_text(self) -> &'static str {
        match self {
            Situation::Unknown => "unknown",
            Situation::Running => "running",
            Situation::Finished => "finished",
            Situation::Cancelled => "cancelled",
            Situation::Failed => "failed",
        }
    }
}

/// What only the launcher knows about the task. File `<base>.task.json`, if present.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub document: String,
    /// How many blocks the document has, in total. This is the denominator.
    pub targets: usize,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub situation: Situation,
}

/// The snapshot of progress. This is what the tool returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Progress {
    pub document: String,
    /// Total targets, when the task declared them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<usize>,
    pub processed: usize,
    pub approved: usize,
    pub corrected: usize,
    pub undecided: usize,
    /// Registry lines that could not be read. If > 0, the snapshot is incomplete — better to
    /// say so than to round down in silence.
    pub unreadable: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    /// Corrected over everything that has a verdict. It measures how much the local
    /// translator gets wrong on its own — the number that validates (or not) the
    /// local-plus-reviewer arrangement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_rate: Option<f64>,
    pub situation: Situation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    /// The highest document line reached so far. Shows how far the batch got.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_line: Option<usize>,
}

impl Progress {
    pub fn registry_path(dir: &Path, base: &str) -> PathBuf {
        dir.join(format!("{base}.pipeline.jsonl"))
    }

    pub fn task_path(dir: &Path, base: &str) -> PathBuf {
        dir.join(format!("{base}.task.json"))
    }

    /// Reads the registry (and the task, if any) and builds the snapshot.
    ///
    /// A missing registry is not an error: a document that was never started has zero progress,
    /// and that is an answer, not a failure.
    pub fn read(dir: &Path, base: &str) -> io::Result<Self> {
        let task = std::fs::read_to_string(Self::task_path(dir, base))
            .ok()
            .and_then(|t| serde_json::from_str::<Task>(&t).ok());

        let mut p = Self {
            document: task
                .as_ref()
                .map(|t| t.document.clone())
                .unwrap_or_else(|| base.to_string()),
            targets: task.as_ref().map(|t| t.targets),
            processed: 0,
            approved: 0,
            corrected: 0,
            undecided: 0,
            unreadable: 0,
            remaining: None,
            percent: None,
            correction_rate: None,
            situation: task.as_ref().map(|t| t.situation).unwrap_or_default(),
            started_at: task.as_ref().and_then(|t| t.started_at),
            updated_at: None,
            last_line: None,
        };

        let registry = Self::registry_path(dir, base);
        if let Ok(text) = std::fs::read_to_string(&registry) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) => p.count(&v),
                    Err(_) => p.unreadable += 1,
                }
            }
            if let Ok(meta) = std::fs::metadata(&registry) {
                p.updated_at = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
            }
        }

        let with_verdict = p.approved + p.corrected;
        if with_verdict > 0 {
            p.correction_rate = Some(100.0 * p.corrected as f64 / with_verdict as f64);
        }
        if let Some(targets) = p.targets {
            p.remaining = Some(targets.saturating_sub(p.processed));
            if targets > 0 {
                p.percent = Some(100.0 * p.processed as f64 / targets as f64);
            }
        }
        Ok(p)
    }

    fn count(&mut self, v: &serde_json::Value) {
        self.processed += 1;
        match v.get("ok").and_then(|o| o.as_bool()) {
            Some(true) => self.approved += 1,
            Some(false) => self.corrected += 1,
            None => self.undecided += 1,
        }
        if let Some(line) = v.get("line").and_then(|l| l.as_u64()) {
            let line = line as usize;
            self.last_line = Some(self.last_line.map_or(line, |m| m.max(line)));
        }
    }

    /// One line that fits a terminal, in the same tone as the old pipeline.
    pub fn summary(&self) -> String {
        let mut s = match self.targets {
            Some(a) => format!("{}/{}", self.processed, a),
            None => format!("{} processed (the total was never declared)", self.processed),
        };
        s.push_str(&format!(
            " · {} approved · {} corrected",
            self.approved, self.corrected
        ));
        if self.undecided > 0 {
            s.push_str(&format!(" · {} undecided", self.undecided));
        }
        if let Some(t) = self.correction_rate {
            s.push_str(&format!(" ({t:.0}%)"));
        }
        if let Some(p) = self.percent {
            s.push_str(&format!(" · {p:.0}% do document"));
        }
        s.push_str(&format!(" · {}", self.situation.as_text()));
        if self.unreadable > 0 {
            s.push_str(&format!(" · ATENCAO: {} lines unreadable", self.unreadable));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(nome: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rustranslator-progresso-{nome}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Three lines: approved, corrected, undecided.
    const REGISTRO: &str = concat!(
        r#"{"id":"12:1","line":12,"fonte":"a","traducao":"a","ok":true,"final":"a"}"#,
        "\n",
        r#"{"id":"15:2","line":15,"fonte":"b","traducao":"b","ok":false,"final":"b2"}"#,
        "\n",
        r#"{"id":"20:3","line":20,"fonte":"c","traducao":null,"ok":null,"erro":"slot perdido"}"#,
        "\n",
    );

    #[test]
    fn counts_the_three_verdicts() {
        let d = test_dir("conta");
        std::fs::write(Progress::registry_path(&d, "doc"), REGISTRO).unwrap();
        let p = Progress::read(&d, "doc").unwrap();
        assert_eq!((p.processed, p.approved, p.corrected, p.undecided), (3, 1, 1, 1));
        assert_eq!(p.last_line, Some(20));
        assert_eq!(p.correction_rate, Some(50.0)); // 1 corrigido de 2 com veredito
        assert_eq!(p.targets, None, "sem task, o total e' desconhecido — nao inventamos");
        assert_eq!(p.percent, None);
        assert!(p.summary().contains("50%"), "summary: {}", p.summary());
    }

    #[test]
    fn the_task_provides_the_denominator() {
        let d = test_dir("task");
        std::fs::write(Progress::registry_path(&d, "doc"), REGISTRO).unwrap();
        std::fs::write(
            Progress::task_path(&d, "doc"),
            r#"{"document":"README.md","targets":12,"situation":"running","pid":4242,"started_at":1000}"#,
        )
        .unwrap();
        let p = Progress::read(&d, "doc").unwrap();
        assert_eq!(p.targets, Some(12));
        assert_eq!(p.remaining, Some(9));
        assert_eq!(p.percent, Some(25.0));
        assert_eq!(p.situation, Situation::Running);
        assert_eq!(p.document, "README.md");
        assert!(p.summary().contains("3/12"), "summary: {}", p.summary());
    }

    #[test]
    fn a_document_never_started_is_zero_not_an_error() {
        let d = test_dir("vazio");
        let p = Progress::read(&d, "inexistente").unwrap();
        assert_eq!(p.processed, 0);
        assert_eq!(p.situation, Situation::Unknown);
        assert!(p.summary().contains("never declared"), "summary: {}", p.summary());
    }

    #[test]
    fn an_unreadable_line_is_counted_and_flagged() {
        let d = test_dir("ilegivel");
        std::fs::write(
            Progress::registry_path(&d, "doc"),
            format!("{REGISTRO}isto nao e json\n"),
        )
        .unwrap();
        let p = Progress::read(&d, "doc").unwrap();
        assert_eq!(p.processed, 3);
        assert_eq!(p.unreadable, 1);
        assert!(p.summary().contains("unreadable"), "summary: {}", p.summary());
    }
}
