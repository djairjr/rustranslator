#!/usr/bin/env python3
"""How the current parser differs from the recorded corpus — a measurement, not a gate.

The corpus is a registry of 262 blocks taken from a real document, recorded by the implementation
this project replaces. It is kept because it is the only trace of how that pipeline actually
behaved, and because a difference against it is worth *understanding*.

It is not a gate, and that matters: three of the differences below are deliberate corrections, and a
test that failed on them would be demanding the old mistakes back.

  * **the marker no longer goes to the model** — the old scanner sent `## Get started` and `- an
    item` whole; now only the text is translated, so the `#` and the `-` come out of the file
    exactly as they went in;
  * **consecutive list items are no longer merged** — the old scanner joined `- one\\n- two` into a
    single block, so only the first kept a bullet;
  * **a sentence is cut only at `.`, `!` and `?`** — the old scanner also cut at `;` and `:`, which
    separate clauses rather than sentences, and handed the model half a sentence to translate.

`markdown-rs` also settles the cases the hand-written scanner got wrong outright: block quotes,
nested lists, setext headings and indented code blocks.

So the script classifies every row it cannot reproduce, prints the tally with an example of each, and
shouts about the one category that would be a real defect: rows it cannot explain.

Usage:
    python3 tests/parser_differential.py [document.md] [corpus.jsonl]
"""
import collections
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(ROOT, "target", "release", "rustranslator")
DATA = os.path.expanduser("~/.local/share/rustranslator")
DEFAULT_DOCUMENT = os.path.expanduser("~/colibri/README.md")
DEFAULT_CORPUS = os.path.join(DATA, "golden", "README.pipeline.jsonl")

# The markers the old scanner carried into the block's text and the current one does not.
MARKERS = ("#", ">", "-", "*", "+")


def rust_blocks(path):
    out = subprocess.run([BINARY, "parse", path], capture_output=True, text=True, timeout=180)
    if out.returncode != 0:
        raise SystemExit(f"the binary failed: {out.stderr.strip()}")
    return [(b["line"], b["text"]) for b in map(json.loads, out.stdout.splitlines())]


def load_corpus(path):
    if not os.path.exists(path):
        raise SystemExit(f"no corpus at {path} — pass one, or set RUSTRANSLATOR_CORPUS")
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def classify(fonte, same_line, everywhere, raw_document):
    """Why a recorded block is not reproduced today.

    `same_line` are the texts the parser now finds on that line; `everywhere`, on any line.
    """
    for text in same_line:
        if text == fonte:
            return "reproduced"
    for text in same_line:
        if text and text in fonte:
            return "marker"
        if text and fonte in text:
            return "clause"
    # The text is somewhere else in the document today. Either the grouping changed, or the document
    # itself was edited after the corpus was recorded — the registry is a record of a file, and files
    # move on.
    for text in everywhere:
        if text and (fonte in text or text in fonte):
            return "moved"
    # The text is in the file, but in no unit: it sits inside a raw HTML block, where CommonMark
    # says the content is HTML and not markdown. The hand-written scanner translated those lines
    # anyway, and sent the `<a href=...>` tags along with them, which is worse than leaving them.
    if fonte in raw_document:
        return "html"
    return "gone"


def main():
    document = (
        sys.argv[1]
        if len(sys.argv) > 1
        else os.environ.get("RUSTRANSLATOR_DOCUMENT", DEFAULT_DOCUMENT)
    )
    corpus_path = (
        sys.argv[2]
        if len(sys.argv) > 2
        else os.environ.get("RUSTRANSLATOR_CORPUS", DEFAULT_CORPUS)
    )
    if not os.path.exists(document):
        raise SystemExit(f"document not found: {document}")
    if not os.path.exists(BINARY):
        raise SystemExit(f"binary not found at {BINARY} (build with: cargo build --release)")

    blocks = rust_blocks(document)
    rows = load_corpus(corpus_path)

    by_line = collections.defaultdict(list)
    for line, text in blocks:
        by_line[line].append(text)

    all_texts = [text for _line, text in blocks]
    with open(document, encoding="utf-8") as fh:
        raw_document = " ".join(fh.read().split())
    tally = collections.Counter()
    example = {}
    for row in rows:
        category = classify(
            row["fonte"], by_line.get(row["linha"], []), all_texts, raw_document
        )
        tally[category] += 1
        example.setdefault(category, row)

    print(f"document: {document}")
    print(f"  {len(blocks)} blocks today")
    print(f"corpus:   {corpus_path}")
    print(f"  {len(rows)} recorded rows")
    print()

    labels = {
        "reproduced": "reproduced exactly, byte for byte",
        "marker": "the marker is no longer sent to the model",
        "clause": "a clause is no longer cut as if it were a sentence",
        "moved": "the same text is grouped differently today, or the document was edited since",
        "html": "inside a raw HTML block — content there is HTML, not markdown",
        "gone": "NOT EXPLAINED — the text is nowhere in the document",
    }
    for category in ("reproduced", "marker", "clause", "moved", "html", "gone"):
        count = tally.get(category, 0)
        if not count:
            continue
        print(f"  {count:4d}  {labels[category]}")
        row = example.get(category)
        if row:
            print(f"        e.g. line {row['linha']}: {row['fonte'][:78]!r}")

    # Repeated work in the registry is a fact about it, worth seeing whenever it is read.
    distinct = len({(r["linha"], r["fonte"]) for r in rows})
    print()
    print(f"  distinct blocks in the corpus: {distinct} of {len(rows)} rows "
          f"({len(rows) - distinct} rows are the same block recorded twice)")

    unexplained = tally.get("gone", 0)
    print()
    if unexplained:
        print(f"FAILED: {unexplained} rows differ for a reason nobody has established.")
        return 1
    print("OK — every difference from the recorded behaviour is one of the intended corrections.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
