#!/usr/bin/env python3
"""Generate packaging/copyright (Debian machine-readable, format 1.0).

Why a generator and not a hand-written file: the list of what the binary carries has to match the
build, and a hand-written list drifts the first time a dependency moves. So the crates come from
`cargo tree` FOR THE TARGET THIS PACKAGE IS BUILT FOR — a lock file lists Windows and wasm crates
too, and naming those as linked would be a lie — and the license texts are copied from the crate
sources in the cargo registry, never retyped.

Usage:  python3 packaging/gen-copyright.py [--target x86_64-unknown-linux-gnu] [--check]

--check writes nothing and exits non-zero if the file on disk differs from what would be generated.
"""
from __future__ import annotations

import argparse
import collections
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REGISTRY = os.path.expanduser("~/.cargo/registry/src")
COMMON = "/usr/share/common-licenses"
# Texts Debian ships system-wide; the policy lets a package point at these instead of copying them.
COMMON_LICENSES = {
    "Apache-2.0": "Apache-2.0",
    "MPL-2.0": "MPL-2.0",
    "BSD-3-Clause": "BSD",  # the common-licenses "BSD" file is the 3-clause one
}
# Where inside a crate the text may live, in the order worth trying.
TEXT_NAMES = ("LICENSE-MIT", "LICENSE", "LICENSE.md", "LICENSE.txt", "COPYING", "LICENCE")


def linked_crates(target: str) -> list[tuple[str, str]]:
    """The crates this ELF is actually linked with, for one target, from cargo itself."""
    out = subprocess.run(
        ["cargo", "tree", "-e", "normal", "--target", target, "--prefix", "none", "--no-dedupe"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    crates: dict[str, str] = {}
    for line in out.splitlines():
        m = re.match(r"^([A-Za-z0-9_.\-]+) v([0-9][^\s(]*)", line.strip())
        if not m:
            continue
        name, version = m.group(1), m.group(2)
        if name in ("rustranslator", "rustranslator-core"):
            continue
        crates[name] = version
    return sorted(crates.items())


def license_of() -> dict[tuple[str, str], str]:
    """name+version -> the license expression cargo knows."""
    import json

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    return {(p["name"], p["version"]): (p.get("license") or "UNKNOWN") for p in meta["packages"]}


def crate_dir(name: str, version: str) -> str | None:
    if not os.path.isdir(REGISTRY):
        return None
    for src in os.listdir(REGISTRY):
        candidate = os.path.join(REGISTRY, src, f"{name}-{version}")
        if os.path.isdir(candidate):
            return candidate
    return None


def text_for(license_expr: str, name: str, version: str) -> tuple[str, str]:
    """(where the terms come from, the text). Copied from disk — never retyped."""
    target = crate_dir(name, version)
    if target:
        for candidate in TEXT_NAMES:
            path = os.path.join(target, candidate)
            if os.path.isfile(path):
                with open(path, encoding="utf-8", errors="replace") as fh:
                    return (f"{os.path.basename(path)} of {name} {version}", fh.read().strip())
    for key, common in COMMON_LICENSES.items():
        if key in license_expr:
            path = os.path.join(COMMON, common)
            if os.path.isfile(path):
                return (f"/usr/share/common-licenses/{common}", path)
    return ("", "")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="x86_64-unknown-linux-gnu")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    crates = linked_crates(args.target)
    versions = license_of()
    by_license: dict[str, list[str]] = collections.defaultdict(list)
    for name, version in crates:
        by_license[versions.get((name, version), "UNKNOWN")].append(f"{name} {version}")

    own = ""
    lic_path = os.path.join(ROOT, "LICENSE")
    if os.path.isfile(lic_path):
        with open(lic_path, encoding="utf-8") as fh:
            own = fh.read().strip()

    lines: list[str] = [
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/",
        "Upstream-Name: rustranslator",
        "Source: https://github.com/djairjr/rustranslator",
        "Comment: The binary is statically linked with the Rust crates named below. The list comes",
        f" from `cargo tree --target {args.target}` — the crates that go into THIS ELF — and not from",
        " the lock file, which also lists crates for targets this package does not build for.",
        " Each license text was copied from the crate source, never retyped. Where an expression",
        " offers a choice (`A OR B`), either applies; the text below is the one that crate ships.",
        "",
        "Files: *",
        "Copyright: 2026 Djair Guilherme da Silva Junior <djair.jr@gmail.com>",
        "License: MIT",
    ]
    if own:
        lines += [f" {l}" if l else " ." for l in own.splitlines()]
    lines.append("")

    for expr in sorted(by_license):
        names = by_license[expr]
        where, text = text_for(expr, *names[0].split())
        lines += [
            "Files: *",
            "Copyright: the crates.io authors of the crates listed below",
            f"License: {expr}",
            f"Comment: {len(names)} crates: " + ", ".join(names),
        ]
        if text.startswith("/"):
            lines += [
                f" The complete text of {expr} is installed on Debian systems as {text}.",
            ]
        elif text:
            lines += [f" Terms as distributed by {where}:", *[f" {l}" if l else " ." for l in text.splitlines()]]
        else:
            lines += [
                " NO TEXT FOUND ON THIS MACHINE for this license expression — the gap is recorded",
                " here rather than hidden. Install the crate source (or read it on crates.io) before",
                " publishing this package.",
            ]
        lines.append("")

    out = "\n".join(lines).rstrip() + "\n"
    dest = os.path.join(ROOT, "packaging", "copyright")
    if args.check:
        current = open(dest, encoding="utf-8").read() if os.path.isfile(dest) else ""
        if current != out:
            print("packaging/copyright is out of date — regenerate it", file=sys.stderr)
            return 1
        print("packaging/copyright is up to date")
        return 0
    with open(dest, "w", encoding="utf-8") as fh:
        fh.write(out)
    print(f"wrote {dest}: {len(crates)} crates, {len(by_license)} distinct license expressions")
    missing = [e for e in by_license if e == "UNKNOWN"]
    for expr in sorted(by_license):
        mark = "  <- no text on disk" if not text_for(expr, *by_license[expr][0].split())[1] else ""
        print(f"  {len(by_license[expr]):4d}  {expr}{mark}")
    if missing:
        print("ERROR: crates with no license declared by cargo:", missing, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
