//! The markdown block parser: turns a document into SHORT, semantically complete blocks — the
//! unit the translator works on.
//!
//! ## Why blocks, and not lines
//!
//! Markdown prose is wrapped at ~78 columns, often mid-sentence. Translating line by line mangles
//! the text (the model sees half a sentence and invents the rest); translating the whole document
//! at once makes it hallucinate. The block is the unit that keeps the meaning intact.
//!
//! Two block kinds reach the translator:
//!
//! * **prose** — paragraphs, list items and headings. A long one is broken into *sentences*;
//! * **table** — one block per non-empty CELL, never a whole row: a row is several independent
//!   statements, and translating them together lets one contaminate the next.
//!
//! Code fences, HTML lines, blank lines and the table's separator row produce NO block at all: they
//! must reach the output byte for byte.
//!
//! ## Fidelity
//!
//! The block boundaries below are pinned by a differential test against a corpus taken from a real
//! document (`tests/parser_differential.py`): the parser must reproduce that corpus block by block.
//! That test is what makes this a port rather than a rewrite — and it is why two behaviours that
//! look like defects are kept, each marked `QUIRK` with its reason. "Fixing" one would break
//! agreement with the corpus, which is the only evidence that the translation behaves as measured.

use regex::Regex;
use std::sync::OnceLock;

/// Above this many characters, a prose block is cut into sentences.
///
/// The threshold is a property of the *model*, not of the language: it is how much text fits in one
/// call without the model losing the thread. Changing it changes translation quality, so it is
/// frozen at the value the corpus was produced with — changing it changes translation quality, and
/// would invalidate the comparison.
pub const SENTENCE_LIMIT: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Paragraph, list item or heading.
    Prose,
    /// A single table cell.
    Table,
}

/// One block: the document line it came from, what it is, and the text to translate.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Block {
    /// Line index in the document, counting the first line as 0 — the same numbering the registry
    /// records in its `linha` field.
    pub line: usize,
    pub kind: Kind,
    pub text: String,
}

// ---------------------------------------------------------------------------
// The line anchors
// ---------------------------------------------------------------------------
//
// Compiled once, on first use. `regex` guarantees linear time — no backtracking — which is why it
// is used here and why lookbehind is unavailable (see `split_sentences`).

fn fence() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*```").unwrap())
}

fn html() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*<").unwrap())
}

fn table() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*\|").unwrap())
}

/// The `| --- | :--: |` row: it delimits the table and carries no text.
fn separator() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*\|[\s:|-]+\|\s*$").unwrap())
}

fn heading() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*#{1,6}\s").unwrap())
}

/// A list item, capturing the marker AND its indentation: `- `, `* `, `1. `, `   - `.
fn item() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(\s*(?:[-*+]|\d+\.)\s+)").unwrap())
}

// ---------------------------------------------------------------------------
// Sentence splitting
// ---------------------------------------------------------------------------

fn is_sentence_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | ';' | ':')
}

/// The characters that can open a sentence: uppercase (ASCII or Latin-1 accented), a digit, `*` or
/// `[`.
fn is_sentence_start(c: char) -> bool {
    c.is_ascii_uppercase()
        || ('\u{C0}'..='\u{DE}').contains(&c)
        || c.is_ascii_digit()
        || c == '*'
        || c == '['
}

/// The split points, as delimiter ranges over `chars` — `(first, last_exclusive)`.
///
/// ## Why this is a scan and not a regex
///
/// The rule: break at a run of whitespace when EITHER
///
///   * the character before it ends a sentence (`.`, `!`, `?`, `;`, `:`) and the character after it
///     opens one; OR
///   * the character after it is a list marker (`-`, `*`, `+`) followed by whitespace.
///
/// The first rule looks *behind* the whitespace, and the `regex` crate has no lookaround — by
/// design, since that is exactly what buys it its linear-time guarantee. `fancy-regex` would
/// restore the syntax and reintroduce backtracking, a bad trade for something that runs on every
/// block of every document.
///
/// Written as a scan, "was the previous character sentence-ending punctuation?" costs nothing and
/// keeps the guarantee.
fn split_points(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if !chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // The delimiter starts at the FIRST whitespace of a run: an interior whitespace character
        // can never satisfy the lookbehind (the previous character would be whitespace).
        if i > 0 && chars[i - 1].is_whitespace() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }

        // Alternative 1: after sentence-ending punctuation, before a sentence start.
        let after_sentence = i > 0 && is_sentence_end(chars[i - 1])
            && j < n
            && is_sentence_start(chars[j]);
        // Alternative 2: before a list marker.
        let before_marker =
            j + 1 < n && matches!(chars[j], '-' | '*' | '+') && chars[j + 1].is_whitespace();

        if after_sentence || before_marker {
            out.push((i, j));
        }
        i = j;
    }
    out
}

/// Cuts a prose block into sentences, then packs them back up to `SENTENCE_LIMIT` characters.
///
/// The packing is not decoration: cutting every sentence into its own block would multiply the
/// number of model calls, and each call carries fixed overhead. Sentences are the split point; the
/// limit is the packing size.
pub fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= SENTENCE_LIMIT {
        return vec![text.to_string()];
    }

    let mut parts: Vec<String> = Vec::new();
    let mut start = 0;
    for (i, j) in split_points(&chars) {
        parts.push(chars[start..i].iter().collect());
        start = j;
    }
    parts.push(chars[start..].iter().collect());

    let mut out = Vec::new();
    let mut current = String::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if !current.is_empty()
            && current.chars().count() + part.chars().count() + 1 > SENTENCE_LIMIT
        {
            out.push(current.clone());
            current = part;
        } else {
            current = format!("{current} {part}").trim().to_string();
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

// ------------------------------------------------------------------------- --
// The parser
// ---------------------------------------------------------------------------

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Walks the document and returns its blocks, in order.
pub fn parse_document(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < n {
        let line = lines[i];

        if fence().is_match(line) {
            // A code fence goes through untouched: skip to the closing fence.
            let mut j = i + 1;
            while j < n && !fence().is_match(lines[j]) {
                j += 1;
            }
            i = j + 1;
        } else if html().is_match(line) || line.trim().is_empty() {
            // Raw HTML and blank lines are not text to translate.
            i += 1;
        } else if table().is_match(line) {
            if !separator().is_match(line) {
                for cell in line.split('|') {
                    let cell = cell.trim();
                    if !cell.is_empty() {
                        blocks.push(Block {
                            line: i,
                            kind: Kind::Table,
                            text: cell.to_string(),
                        });
                    }
                }
            }
            i += 1;
        } else if heading().is_match(line) {
            blocks.push(Block {
                line: i,
                kind: Kind::Prose,
                text: line.trim().to_string(),
            });
            i += 1;
        } else {
            // A paragraph, possibly a list item whose continuation lines are indented.
            let captures = item().captures(line);
            let is_item = captures.is_some();
            let marker = captures
                .as_ref()
                .map(|c| c.get(1).unwrap().as_str().to_string())
                .unwrap_or_default();

            let mut body = line.to_string();
            let mut j = i;
            while j + 1 < n
                && !lines[j + 1].trim().is_empty()
                && !(fence().is_match(lines[j + 1])
                    || html().is_match(lines[j + 1])
                    || table().is_match(lines[j + 1])
                    || heading().is_match(lines[j + 1])
                    // A new list item ends the current one — unless this block is not a list item
                    // at all, in which case the following item continues the paragraph.
                    || (item().is_match(lines[j + 1]) && !is_item))
            {
                j += 1;
                body.push(' ');
                body.push_str(lines[j].trim());
            }

            let mut body = squash(&body);

            // QUIRK, kept on purpose. The marker is removed BY LENGTH, and by then the body has
            // been whitespace-normalised — which stripped the marker's own indentation. For an
            // indented item (`  - text`, a marker of 4 characters) the removal therefore eats two
            // characters of the item's own text. Non-indented items (`- text`) are unaffected.
            //
            // It stays because the differential test pins it against the corpus: "the port
            // matches" has to mean something. It is NOT worth keeping forever — once the corpus is
            // regenerated, the fix is one line.
            if !marker.is_empty() {
                let skip = marker.chars().count();
                body = body.chars().skip(skip).collect();
            }

            for (k, sentence) in split_sentences(&body).into_iter().enumerate() {
                let text = if !marker.is_empty() && k == 0 {
                    format!("{marker}{sentence}")
                } else {
                    sentence
                };
                blocks.push(Block {
                    line: i,
                    kind: Kind::Prose,
                    text,
                });
            }
            i = j + 1;
        }
    }

    blocks
}

/// Only the blocks that must be translated.
pub fn translatable(blocks: &[Block]) -> Vec<&Block> {
    blocks
        .iter()
        .filter(|b| matches!(b.kind, Kind::Prose | Kind::Table))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(blocks: &[Block]) -> Vec<String> {
        blocks.iter().map(|b| b.text.clone()).collect()
    }

    #[test]
    fn a_short_paragraph_is_one_block() {
        let b = parse_document("Just a short line.\n");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, Kind::Prose);
        assert_eq!(b[0].line, 0);
        assert_eq!(b[0].text, "Just a short line.");
    }

    #[test]
    fn a_fence_goes_through_untouched() {
        let doc = "before\n```rust\nlet x = 1; // not translated\n```\nafter\n";
        let b = parse_document(doc);
        assert_eq!(texts(&b), vec!["before", "after"]);
    }

    #[test]
    fn an_unterminated_fence_swallows_the_rest() {
        // A fence with no closing line swallows the rest of the document as code. Kept: the corpus
        // was produced with this behaviour.
        let b = parse_document("before\n```\nnever closed\nstill code\n");
        assert_eq!(texts(&b), vec!["before"]);
    }

    #[test]
    fn each_table_cell_is_its_own_block() {
        let b = parse_document("| a | b |\n| --- | --- |\n| c | d |\n");
        assert_eq!(texts(&b), vec!["a", "b", "c", "d"]);
        assert!(b.iter().all(|x| x.kind == Kind::Table));
        // The separator row produced nothing at all.
        assert_eq!(b.len(), 4);
    }

    #[test]
    fn a_heading_is_prose_and_keeps_its_hashes() {
        let b = parse_document("## Title\n");
        assert_eq!(b[0].kind, Kind::Prose);
        assert_eq!(b[0].text, "## Title");
    }

    #[test]
    fn html_and_blank_lines_produce_nothing() {
        let b = parse_document("<div>\n\n<img src=\"x\">\n");
        assert!(b.is_empty(), "got {b:?}");
    }

    #[test]
    fn a_wrapped_paragraph_becomes_one_block() {
        let b = parse_document("first half of the sentence\nsecond half of it\n");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "first half of the sentence second half of it");
    }

    #[test]
    fn a_list_item_keeps_its_marker() {
        let b = parse_document("- the item continues\n  on the next line\n");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "- the item continues on the next line");
    }

    #[test]
    fn consecutive_items_merge_into_one_block() {
        // MEASURED, not reasoned about. A following item line does NOT break the paragraph when the
        // current line is itself an item — so the two items become ONE block and only the first
        // keeps its marker. Surprising, and pinned by the differential test; see the note on the
        // `!is_item` condition in `parse_document`.
        let b = parse_document("- first\n- second\n");
        assert_eq!(texts(&b), vec!["- first - second"]);
    }

    #[test]
    fn an_item_after_prose_starts_a_new_block() {
        let b = parse_document("text\n- item\n");
        assert_eq!(texts(&b), vec!["text", "- item"]);
    }

    #[test]
    fn the_quirky_marker_removal_is_reproduced() {
        // MEASURED, not reasoned about. Two spaces of indentation: the marker is 4 characters
        // ("  - "), but normalisation has already stripped those two spaces, so 4 characters are
        // eaten from the item's own text — and then the marker is put back in front. The measured
        // result is "  - e item": the "e" is what is left of "the".
        let b = parse_document("  - the item\n");
        assert_eq!(b[0].text, "  - e item", "the quirk changed shape — check `parse_document`");
    }

    #[test]
    fn sentences_are_packed_up_to_the_limit() {
        let doc = "One sentence here. Another sentence there. And a third one as well, long enough \
                   to matter.";
        let parts = split_sentences(doc);
        // Under the limit: a single part, untouched.
        assert_eq!(parts.len(), 1);

        // Above the limit, cut after the punctuation.
        let long = "A ".to_string() + &"word ".repeat(60) + "Done. Then another sentence follows.";
        let parts = split_sentences(&long);
        assert!(parts.len() > 1, "did not split: {parts:?}");
        assert!(parts.iter().all(|p| !p.is_empty()));
    }

    #[test]
    fn a_split_never_loses_characters() {
        // The packing must be lossless apart from the delimiters it consumes.
        let text = "Alpha beta gamma. Delta epsilon zeta. Eta theta iota. Kappa lambda mu. \
                    Nu xi omicron. Pi rho sigma. Tau upsilon phi.";
        let parts = split_sentences(text);
        let joined = parts.join(" ");
        for word in ["Alpha", "iota", "sigma", "phi"] {
            assert!(joined.contains(word), "{word} was lost: {joined}");
        }
    }

    #[test]
    fn splitting_counts_characters_not_bytes() {
        // "ção" is 4 bytes and 3 characters. The threshold is measured in characters, as in the
        // original. 61 accents = 122 bytes but only 61 characters, so this must NOT split.
        let text = "ção".repeat(61);
        assert_eq!(split_sentences(&text).len(), 1);
    }
}
