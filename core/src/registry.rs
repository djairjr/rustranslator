//! The registry on disk: one JSON line per block, appended as the work happens.
//!
//! ## Why a stable id matters
//!
//! The registry is what makes a long batch survivable: stop halfway through a 400-block document,
//! start again, and the finished blocks are skipped instead of paid for twice.
//!
//! That only works if a block's id is the *same* on the second run. The registry this project
//! inherited used a hash of the text that is randomised per process, so its ids could never match
//! on a later run — the resumption silently re-did everything, and the corpus still carries the
//! evidence: 17 of its 262 rows are the same block recorded twice, under two different ids.
//!
//! Here the id is FNV-1a over the text's bytes, and the line goes in front. It is stable across
//! runs, across machines and across implementations, which is the whole point.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::progress::Progress;

/// One recorded block.
///
/// The field names are the registry's own and are **not** translated: `linha`, `fonte`, `traducao`
/// are the data format, and every row already on disk — 262 of them — uses them. Renaming them here
/// would silently orphan that history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    /// The document line the block came from.
    pub linha: usize,
    /// The source text.
    pub fonte: String,
    /// What the machine produced. `None` when the model lost a protected slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traducao: Option<String>,
    /// The reviewer's verdict: `true` approved, `false` corrected, `null` could not answer.
    #[serde(default)]
    pub ok: Option<bool>,
    /// The reviewer's corrected translation, when it rejected the machine's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<String>,
    /// What a person should end up reading.
    #[serde(default, rename = "final", skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    /// Why there is no verdict, when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erro: Option<String>,
}

/// The id of a block: its line, and a hash of its text that does not move.
pub fn stable_id(line: usize, text: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{line}:{hash:08x}")
}

pub fn registry_path(dir: &Path, base: &str) -> PathBuf {
    Progress::registry_path(dir, base)
}

pub fn task_path(dir: &Path, base: &str) -> PathBuf {
    Progress::task_path(dir, base)
}

/// Appends one record, and flushes.
///
/// The flush is not politeness: this line is the only record that a block was paid for, and a
/// process that dies with it still in a buffer loses the work and the evidence together.
pub fn append(path: &Path, record: &Record) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")?;
    file.flush()
}

/// Every recorded block, keyed by id. The last row for an id wins, so a re-done block overrides the
/// earlier attempt instead of being shadowed by it.
pub fn load(path: &Path) -> HashMap<String, Record> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(record) = serde_json::from_str::<Record>(line) {
            out.insert(record.id.clone(), record);
        }
    }
    out
}

/// Just the ids, for the skip test.
pub fn done_ids(path: &Path) -> std::collections::HashSet<String> {
    load(path).into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustranslator-registry-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(line: usize, text: &str) -> Record {
        Record {
            id: stable_id(line, text),
            linha: line,
            fonte: text.to_string(),
            traducao: Some(format!("{text} translated")),
            ok: Some(true),
            correction: None,
            final_text: Some(format!("{text} translated")),
            erro: None,
        }
    }

    #[test]
    fn the_id_is_stable_across_calls() {
        let a = stable_id(12, "the same text");
        let b = stable_id(12, "the same text");
        assert_eq!(a, b);
        // Known value: this must not drift, or every registry on disk stops matching at once.
        assert_eq!(stable_id(0, ""), "0:cbf29ce484222325");
        assert_eq!(a, "12:a4540f77787caad5");
    }

    #[test]
    fn the_id_separates_line_and_text() {
        assert_ne!(stable_id(1, "x"), stable_id(2, "x"));
        assert_ne!(stable_id(1, "x"), stable_id(1, "y"));
    }

    #[test]
    fn what_is_written_can_be_read_back() {
        let dir = test_dir("roundtrip");
        let path = registry_path(&dir, "doc");
        append(&path, &record(3, "alpha")).unwrap();
        append(&path, &record(4, "beta")).unwrap();

        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        let r = &loaded[&stable_id(3, "alpha")];
        assert_eq!(r.linha, 3);
        assert_eq!(r.fonte, "alpha");
        assert_eq!(r.ok, Some(true));
        assert_eq!(r.final_text.as_deref(), Some("alpha translated"));
    }

    #[test]
    fn the_wire_format_matches_the_rows_already_on_disk() {
        // The corpus uses these keys and no others. A rename here would orphan 262 rows.
        let json = serde_json::to_string(&record(7, "text")).unwrap();
        for key in ["\"id\"", "\"linha\"", "\"fonte\"", "\"traducao\"", "\"ok\"", "\"final\""] {
            assert!(json.contains(key), "{key} missing from {json}");
        }
        assert!(!json.contains("correction"), "an absent key must be absent: {json}");
        assert!(!json.contains("erro"), "an absent key must be absent: {json}");
    }

    #[test]
    fn a_later_row_for_the_same_id_wins() {
        let dir = test_dir("lastwins");
        let path = registry_path(&dir, "doc");
        append(&path, &record(3, "alpha")).unwrap();
        let mut second = record(3, "alpha");
        second.ok = Some(false);
        second.final_text = Some("better".to_string());
        append(&path, &second).unwrap();

        let loaded = load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&stable_id(3, "alpha")].final_text.as_deref(), Some("better"));
    }

    #[test]
    fn a_corrupt_line_does_not_lose_the_rest() {
        let dir = test_dir("corrupt");
        let path = registry_path(&dir, "doc");
        append(&path, &record(1, "a")).unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "this is not json").unwrap();
        append(&path, &record(2, "b")).unwrap();

        assert_eq!(load(&path).len(), 2, "one bad line took the good ones with it");
    }

    #[test]
    fn done_ids_reads_what_was_appended() {
        let dir = test_dir("doneids");
        let path = registry_path(&dir, "doc");
        append(&path, &record(1, "a")).unwrap();
        let ids = done_ids(&path);
        assert!(ids.contains(&stable_id(1, "a")));
        assert!(!ids.contains(&stable_id(1, "never written")));
    }
}
