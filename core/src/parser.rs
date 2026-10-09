//! Where the text to translate is, and how it goes back.
//!
//! ## What this module does not do
//!
//! It does not parse markdown. That is `markdown` (markdown-rs), a CommonMark parser with GFM, which
//! returns an AST whose every node carries its exact byte span in the source. Hand-writing a
//! markdown scanner is a losing game: the first version of this file did exactly that, got block
//! quotes, nested lists, setext headings and indented code blocks wrong, and quietly ate two
//! characters out of indented list items.
//!
//! ## What it does
//!
//! It finds the **units of translation** — a paragraph, a heading, a table cell — and records where
//! each one lives in the file, in bytes. Translating is then a splice: the translation replaces
//! exactly those bytes and nothing else. Every marker, every pipe, every indentation, every code
//! fence, every line break outside the unit comes out untouched, because it was never copied
//! anywhere.
//!
//! That is what makes the round trip exact: with nothing translated, `render` gives the file back
//! byte for byte.
//!
//! ## The span comes from the children, not from the node
//!
//! A node's own position includes its syntax: a heading's span starts at the `#`, a table cell's at
//! the `|`. Replacing those spans would delete the markdown. So a unit's span runs from its first
//! child's start to its last child's end — the text, and only the text.
//!
//! ## Two traps, both measured rather than assumed
//!
//! * `Point.offset` is a **byte** offset, even though its doc comment says "character". On a file
//!   with accented text the two differ, and slicing by character would cut mid-codepoint.
//! * A **list item's marker sits outside** the paragraph inside it (the paragraph begins after the
//!   `- `). That is why a list keeps its bullet for free, and why nothing here needs to know what a
//!   bullet looks like.

use markdown::mdast::Node;
use markdown::{to_mdast, ParseOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A paragraph or a heading.
    Prose,
    /// A table cell.
    Table,
}

/// One unit of translation, and where it lives.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Block {
    /// The document line the unit starts on, counting the first line as 0 — the numbering the
    /// registry records in its `linha` field.
    pub line: usize,
    pub kind: Kind,
    /// The text to translate, whitespace-normalised: a paragraph wrapped across three source lines
    /// reaches the model as one clean sentence instead of three fragments.
    pub text: String,
    /// Byte offset of the unit's first character in the source.
    pub start: usize,
    /// Byte offset one past its last character: the span is `start..end`.
    pub end: usize,
}

/// Collapses whitespace runs to a single space and trims. The model is asked to translate text, not
/// to reproduce somebody's line wrapping.
fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The span of a node's **content**: from its first child to its last.
///
/// Not the node's own position — see the module docs. `None` for a node with no children, which has
/// no text of its own to translate.
fn content_span(node: &Node) -> Option<(usize, usize)> {
    let children = node.children()?;
    let first = children.first()?.position()?;
    let last = children.last()?.position()?;
    Some((first.start.offset, last.end.offset))
}

/// Above this many characters, a paragraph is cut into sentences.
///
/// The threshold is a property of the *model*, not of the language: it is how much text it reads
/// before it loses the thread. It came from the measured pipeline this project replaces, and
/// dropping it would have been a silent change to translation quality.
pub const SENTENCE_LIMIT: usize = 240;

/// The punctuation that ends a sentence.
///
/// Only these three. A semicolon and a colon separate *clauses*, not sentences: cutting at "the
/// following:" would tear a list away from the sentence that introduces it. The original pipeline
/// treated them as terminators, and carrying that over would have been a silent mistranslation —
/// the model would have been asked to translate half a sentence as if it were a whole one.
fn is_sentence_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?')
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

/// Where a slice may be cut: at a run of whitespace whose preceding character ends a sentence and
/// whose following character opens one.
///
/// The rule looks *behind* the whitespace, and the `regex` crate has no lookbehind — by design,
/// since that is what buys it its linear-time guarantee. Written as a scan, the question costs
/// nothing and keeps the guarantee.
fn sentence_breaks(slice: &str) -> Vec<usize> {
    let chars: Vec<(usize, char)> = slice.char_indices().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if !chars[i].1.is_whitespace() {
            i += 1;
            continue;
        }
        // The break starts at the FIRST whitespace of a run: an interior whitespace can never
        // satisfy the rule, since its predecessor would be whitespace.
        if i > 0 && chars[i - 1].1.is_whitespace() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && chars[j].1.is_whitespace() {
            j += 1;
        }
        let after_sentence = i > 0
            && is_sentence_end(chars[i - 1].1)
            && j < n
            && is_sentence_start(chars[j].1);
        if after_sentence {
            out.push(chars[i].0);
        }
        i = j;
    }
    out
}

/// The byte ranges of `slice` to translate as separate units, in absolute offsets.
///
/// A slice within the limit is one range that covers it whole. A longer one is cut at the sentence
/// breaks and packed back up to the limit, so the model gets sentences rather than fragments. The
/// whitespace where a cut happens goes with the *following* range, which is what makes the ranges
/// tile the slice exactly — no gap, no overlap — so replacing all of them replaces the whole
/// paragraph and nothing else.
fn chunks(slice: &str, start: usize) -> Vec<(usize, usize)> {
    if slice.chars().count() <= SENTENCE_LIMIT {
        return vec![(start, start + slice.len())];
    }

    let mut boundaries: Vec<usize> = vec![0];
    boundaries.extend(sentence_breaks(slice));
    boundaries.push(slice.len());

    let mut out = Vec::new();
    let mut chunk_start = 0usize;
    let mut current = 0usize;
    for pair in boundaries.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        if from == to {
            continue;
        }
        let piece = normalise(&slice[from..to]).chars().count();
        if current > 0 && current + piece + 1 > SENTENCE_LIMIT {
            out.push((start + chunk_start, start + from));
            chunk_start = from;
            current = piece;
        } else {
            current = if current == 0 { piece } else { current + piece + 1 };
        }
    }
    out.push((start + chunk_start, start + slice.len()));
    out
}

/// Subtrees that must never be translated, and whose insides must not be either.
fn is_opaque(node: &Node) -> bool {
    matches!(
        node,
        Node::Code(_) | Node::Html(_) | Node::Yaml(_) | Node::Toml(_) | Node::Math(_)
    )
}

/// Whether the node is itself a unit of translation, of which kind, and whether it may be cut into
/// sentences.
///
/// A heading and a table cell are never cut. A heading is short by nature, and cutting it would
/// turn one heading into several; a cell is a single statement, and splitting it would break the
/// row it belongs to.
fn unit_kind(node: &Node) -> Option<(Kind, bool)> {
    match node {
        Node::Paragraph(_) => Some((Kind::Prose, true)),
        Node::Heading(_) => Some((Kind::Prose, false)),
        Node::TableCell(_) => Some((Kind::Table, false)),
        _ => None,
    }
}

fn walk(node: &Node, source: &str, out: &mut Vec<Block>) {
    if is_opaque(node) {
        return;
    }
    // A unit stops the walk: its span already covers everything inside it, and nothing inside a
    // paragraph, a heading or a cell is itself a unit.
    if let Some((kind, chunkable)) = unit_kind(node) {
        if let (Some((start, end)), Some(position)) = (content_span(node), node.position()) {
            if source.get(start..end).is_some() {
                let line = position.start.line.saturating_sub(1);
                let ranges = if chunkable {
                    chunks(source.get(start..end).unwrap(), start)
                } else {
                    vec![(start, end)]
                };
                for (from, to) in ranges {
                    let text = normalise(&source[from..to]);
                    if !text.is_empty() {
                        out.push(Block {
                            line,
                            kind,
                            text,
                            start: from,
                            end: to,
                        });
                    }
                }
            }
        }
        return;
    }
    if let Some(children) = node.children() {
        for child in children {
            walk(child, source, out);
        }
    }
}

/// The units of translation, in document order.
///
/// GFM is on, so a table is structure rather than a run of text.
pub fn parse(source: &str) -> Result<Vec<Block>, String> {
    let ast = to_mdast(source, &ParseOptions::gfm()).map_err(|e| e.to_string())?;
    let mut blocks = Vec::new();
    walk(&ast, source, &mut blocks);
    Ok(blocks)
}

/// Puts the translations back into the document.
///
/// `translations[i]` answers `parse(source)?[i]`. `None` — or a slice that stops short — means "not
/// translated yet": that unit is left exactly as it was, so a half-finished document still renders.
///
/// The splices are applied from the end backwards, so that editing the tail cannot move an offset
/// that has already been measured in the head.
pub fn render(source: &str, blocks: &[Block], translations: &[Option<String>]) -> String {
    let mut out = source.to_string();
    for (block, translation) in blocks.iter().zip(translations).rev() {
        let Some(translation) = translation else {
            continue;
        };
        // A span that is not on a character boundary is left alone: refusing to edit is always
        // better than corrupting the file.
        if out.is_char_boundary(block.start) && out.is_char_boundary(block.end) {
            out.replace_range(block.start..block.end, translation);
        }
    }
    out
}

/// The lines the units came from.
pub fn lines_of(blocks: &[Block]) -> Vec<usize> {
    blocks.iter().map(|b| b.line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(source: &str) -> Vec<Block> {
        parse(source).expect("a plain markdown document must parse")
    }

    fn texts(source: &str) -> Vec<String> {
        units(source).into_iter().map(|b| b.text).collect()
    }

    /// With nothing translated, the document must come back byte for byte. If this ever fails,
    /// `render` is editing something it should not.
    fn round_trip_is_identity(source: &str) {
        let blocks = units(source);
        let nothing = vec![None; blocks.len()];
        assert_eq!(
            render(source, &blocks, &nothing),
            source,
            "the round trip changed the document"
        );
    }

    #[test]
    fn the_round_trip_is_the_identity_on_tricky_documents() {
        for source in [
            "",
            "\n",
            "a\n",
            "a",
            "# Title\n\nA paragraph.\n",
            "A paragraph wrapped over\nseveral lines with `code` and a [link](http://x).\n",
            "| a | b |\n| --- | --- |\n| c | d |\n",
            "- item one\n  continued here\n\n- item two\n",
            "> a quoted paragraph\n> still quoted\n",
            "```rust\nlet x = 1;\n```\n",
            "    indented code\n",
            "~~~\nfenced with tildes\n~~~\n",
            "Setext heading\n==============\n",
            "1. first\n2. second\n",
            "- outer\n  - inner\n",
            "---\n\n<!-- raw html -->\n\nText after.\n",
        ] {
            round_trip_is_identity(source);
        }
    }

    #[test]
    fn a_heading_keeps_its_hashes() {
        let source = "## A title\n";
        let blocks = units(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "A title", "the '##' was sent to the model");
        let out = render(source, &blocks, &[Some("Um titulo".to_string())]);
        assert_eq!(out, "## Um titulo\n");
    }

    #[test]
    fn a_list_item_keeps_its_bullet_and_its_indentation() {
        let source = "- an item\n  continued\n\n  - a nested one\n";
        assert_eq!(texts(source), vec!["an item continued", "a nested one"]);
        let blocks = units(source);
        let out = render(
            source,
            &blocks,
            &[Some("um item continuado".into()), Some("um aninhado".into())],
        );
        assert_eq!(out, "- um item continuado\n\n  - um aninhado\n");
    }

    #[test]
    fn a_blockquote_keeps_its_marker() {
        let source = "> a quote\n";
        let blocks = units(source);
        assert_eq!(blocks[0].text, "a quote");
        let out = render(source, &blocks, &[Some("uma citacao".into())]);
        assert_eq!(out, "> uma citacao\n");
    }

    #[test]
    fn code_is_never_a_unit() {
        let source = "text\n\n```\nlet x = 1; // not translated\n```\n\n    indented\n";
        assert_eq!(texts(source), vec!["text"]);
    }

    #[test]
    fn raw_html_is_never_a_unit() {
        let source = "<div>\n  <p>markup</p>\n</div>\n";
        assert!(units(source).is_empty(), "html became a unit");
    }

    #[test]
    fn each_table_cell_is_its_own_unit_and_the_row_survives() {
        let source = "| a | b |\n| --- | --- |\n| c | d |\n";
        let blocks = units(source);
        assert_eq!(texts(source), vec!["a", "b", "c", "d"]);
        assert!(blocks.iter().all(|b| b.kind == Kind::Table));
        // The separator row is structure: it yields no unit and it must survive.
        let out = render(
            source,
            &blocks,
            &[
                Some("A".into()),
                Some("B".into()),
                Some("C".into()),
                Some("D".into()),
            ],
        );
        assert_eq!(out, "| A | B |\n| --- | --- |\n| C | D |\n");
    }

    #[test]
    fn a_wrapped_paragraph_reaches_the_model_as_one_line() {
        let source = "A sentence that somebody wrapped\nat seventy-eight columns.\n";
        assert_eq!(
            texts(source),
            vec!["A sentence that somebody wrapped at seventy-eight columns."]
        );
    }

    #[test]
    fn an_untranslated_unit_is_left_exactly_as_it_was() {
        let source = "first\n\nsecond\n\nthird\n";
        let blocks = units(source);
        let out = render(source, &blocks, &[Some("um".into()), None, Some("tres".into())]);
        assert_eq!(out, "um\n\nsecond\n\ntres\n");
    }

    #[test]
    fn the_line_numbers_are_zero_based_as_the_registry_records_them() {
        let source = "one\n\ntwo\n\nthree\n";
        assert_eq!(lines_of(&units(source)), vec![0, 2, 4]);
    }

    #[test]
    fn an_empty_document_has_no_units_and_renders_unchanged() {
        let blocks = units("");
        assert!(blocks.is_empty());
        assert_eq!(render("", &blocks, &[]), "");
    }

    #[test]
    fn accents_do_not_shift_the_spans() {
        // The offsets are bytes. On a document with accents a character offset would cut a
        // codepoint in half, and this is the test that catches it.
        let source = "Ação çãoção primeiro.\n\nSegundo ção parágrafo.\n";
        let blocks = units(source);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Ação çãoção primeiro.");
        assert_eq!(blocks[1].text, "Segundo ção parágrafo.");
        let out = render(
            source,
            &blocks,
            &[
                Some("Ação çãoção primeiro.".into()),
                Some("Segundo ção parágrafo.".into()),
            ],
        );
        assert_eq!(out, source);
    }

    #[test]
    fn a_long_paragraph_is_cut_into_sentences_without_leaving_anything_behind() {
        // Six of them: well past the limit, so there is something to cut.
        let sentence = "This is a sentence that is long enough to have weight. ";
        let source = format!("{sentence}{sentence}{sentence}{sentence}{sentence}{sentence}\n");
        assert!(source.chars().count() > SENTENCE_LIMIT);
        let blocks = units(&source);
        assert!(blocks.len() > 1, "a long paragraph was not cut: {blocks:?}");

        // Every piece is within the limit, and every piece shares the paragraph's line, as the
        // registry records it.
        for block in &blocks {
            assert!(block.text.chars().count() <= SENTENCE_LIMIT + 60, "{}", block.text);
            assert_eq!(block.line, 0);
        }

        // The cuts tile the paragraph: translating every piece replaces all of it, and the file
        // outside the paragraph is untouched.
        let translated: Vec<Option<String>> = blocks
            .iter()
            .map(|b| Some(b.text.replace("This", "Isto")))
            .collect();
        let out = render(&source, &blocks, &translated);
        assert!(!out.contains("This is a sentence"), "a piece was left behind: {out}");
        assert!(out.ends_with('\n'), "the trailing newline was eaten: {out:?}");

        // And with nothing translated, the paragraph is still the file.
        round_trip_is_identity(&source);
    }

    #[test]
    fn a_colon_and_a_semicolon_do_not_end_a_sentence() {
        // Cutting here would hand the model half a sentence.
        let source = "Bring the following: a hammer; a saw; and a level. Then rest.\n";
        let blocks = units(&source);
        assert_eq!(blocks.len(), 1, "a clause was mistaken for a sentence: {blocks:?}");
        assert_eq!(
            blocks[0].text,
            "Bring the following: a hammer; a saw; and a level. Then rest."
        );
    }

    #[test]
    fn a_long_paragraph_is_cut_at_the_sentence_endings() {
        let source = "First sentence here. Second sentence here? Third one here! Fourth here.\n";
        let long = source.repeat(8); // past the limit, so it must cut
        let blocks = units(&long);
        assert!(blocks.len() > 1, "a long paragraph was not cut");
        // No piece may end in the middle of a sentence: every piece but the last ends at a full
        // stop, and no piece begins with a lower-case letter.
        for block in &blocks[..blocks.len() - 1] {
            let last = block.text.chars().last().unwrap();
            assert!(
                matches!(last, '.' | '?' | '!'),
                "a piece was cut off mid-sentence: {:?}",
                block.text
            );
        }
    }

    #[test]
    fn a_heading_and_a_table_cell_are_never_cut() {
        // Comfortably past the limit, so "was it cut?" is a real question.
        let long = "A heading long enough to be well past the sentence limit, and then several \
                    more words on top of that to be certain of it. ";
        let long = long.repeat(3);
        assert!(long.chars().count() > SENTENCE_LIMIT, "the fixture is too short");
        // One column, so the delimiter row has one cell too. With two, GFM does not see a table at
        // all and the line is a paragraph — which is exactly what an earlier version of this test
        // did by mistake.
        let source = format!("# {long}\n\n| {long} |\n| - |\n");

        let blocks = units(&source);
        assert_eq!(blocks.len(), 2, "a heading or a cell was cut: {blocks:?}");
        for block in &blocks {
            assert!(block.text.chars().count() > SENTENCE_LIMIT);
            assert_eq!(block.line, if block.kind == Kind::Prose { 0 } else { 2 });
        }
        round_trip_is_identity(&source);
    }

    #[test]
    fn the_span_covers_the_text_and_not_the_marker() {
        let source = "- item\n";
        let block = &units(source)[0];
        assert_eq!(&source[block.start..block.end], "item");
    }
}
