#!/usr/bin/env python3
"""Differential check of the block parser against the recorded corpus.

The corpus is a registry: one JSON line per block, each carrying the document line it came from
(`linha`) and its text (`fonte`). It was recorded on a real document by the implementation this
project replaces, and that is what makes it worth anything — it is not a fixture written to match
the code, it is the behaviour that was actually measured and translated.

The check is by MEMBERSHIP, not by position, and the distinction matters:

  * the registry is append-only and it is not in document order — it was built up over more than
    one run, so the same line can appear near the top and again further down;
  * a registry that happened to be a prefix of the document's blocks would prove nothing anyway.
    The question that means something is whether every block ever recorded is reproduced today.

Two things this deliberately does not do:

  * key on the registry's `id`. That id comes from a hash of the text that is not stable between
    processes, so nothing can use it as a key. The comparison is on `linha` + `fonte`, which is
    what the format actually means.
  * call a model. Parsing costs nothing, so this can run on every commit.

Usage:
    python3 tests/parser_differential.py [document.md] [corpus.jsonl]

Both arguments have defaults under the working data directory, and both can come from the
environment as RUSTRANSLATOR_DOCUMENT and RUSTRANSLATOR_CORPUS.
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


def rust_blocks(path):
    """The parser's blocks, as (linha, kind, text)."""
    out = subprocess.run([BINARY, "parse", path], capture_output=True, text=True, timeout=180)
    if out.returncode != 0:
        raise SystemExit(f"the binary failed: {out.stderr.strip()}")
    return [(b["line"], b["kind"], b["text"]) for b in map(json.loads, out.stdout.splitlines())]


def load_corpus(path):
    if not os.path.exists(path):
        raise SystemExit(f"no corpus at {path} — pass one, or set RUSTRANSLATOR_CORPUS")
    rows = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


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
    prose = sum(1 for _line, kind, _text in blocks if kind == "prose")
    rows = load_corpus(corpus_path)

    print(f"document: {document}")
    print(f"  {len(blocks)} blocks ({prose} prose, {len(blocks) - prose} table)")
    print(f"corpus:   {corpus_path}")
    print(f"  {len(rows)} rows")
    print()

    have = {(line, text) for line, _kind, text in blocks}
    missing = [(r["linha"], r["fonte"]) for r in rows if (r["linha"], r["fonte"]) not in have]

    print(f"blocks the parser does NOT reproduce: {len(missing)}")
    for line, text in missing[:5]:
        print(f"  linha {line}: {text[:90]!r}")

    # Repeated work in the registry is reported rather than hidden: it is a property worth knowing
    # when reading the registry's counts, because 262 rows are not 262 distinct blocks.
    counts = collections.Counter((r["linha"], r["fonte"]) for r in rows)
    distinct = len(counts)
    print(
        f"distinct blocks: {distinct} of {len(rows)} rows "
        f"({len(rows) - distinct} rows are repeated work)"
    )

    if missing:
        print()
        print("FAILED: the parser diverges from the recorded behaviour.")
        return 1

    print()
    print("OK — every block the corpus records is reproduced, with the same line number and text.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
