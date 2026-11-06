#!/usr/bin/env python3
"""Generates installer/THIRD-PARTY-LICENSES.txt for the Windows build of Cerberus Usage.

Python standard library only. Run it from anywhere after a dependency change:

    python tools/third_party_licenses.py          # (re)writes installer/THIRD-PARTY-LICENSES.txt
    python tools/third_party_licenses.py --check  # exits 1 when the file is out of date

Which packages: `cargo metadata --locked --format-version 1 --filter-platform <windows target>`,
then everything reachable from the root package over normal (non-build, non-dev) dependencies.
That is the set that ends up in cerberus-usage.exe.

What is written for each package: name, version, license expression and the text of every license
or notice file found in the crate source (registry checkout), including font licenses that ship
inside a crate (for example epaint_default_fonts). A package without any such file is listed by
name and expression at the end, so the gap is visible.

The first line holds the SHA-256 of Cargo.lock (line endings normalised to LF). release.ps1 stops
when that hash no longer matches the current Cargo.lock.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

TARGET = "x86_64-pc-windows-msvc"
ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "installer" / "THIRD-PARTY-LICENSES.txt"
HASH_PREFIX = "Cargo.lock SHA-256: "

# File names (without directory) that count as a license or notice text.
LICENSE_NAME = re.compile(
    r"^(licen[cs]e|copying|notice|unlicense|patents|copyright|copyright-notice|ofl|ufl)"
    r"([-_. ].*)?$",
    re.IGNORECASE,
)
# A name that says "license" somewhere, e.g. emoji-icon-font-mit-license.txt.
LICENSE_WORD = re.compile(r"licen[cs]e", re.IGNORECASE)
FONT_SUFFIXES = {".ttf", ".otf", ".woff", ".woff2"}
TEXT_SUFFIXES = {"", ".txt", ".md", ".spdx"}
# Folders never searched inside a crate.
SKIP_DIRS = {".git", "target", "node_modules", "tests", "test", "benches", "examples", "fuzz"}
MAX_DEPTH = 4
MAX_FILE_BYTES = 256 * 1024


def lock_hash() -> str:
    data = (ROOT / "Cargo.lock").read_bytes().replace(b"\r\n", b"\n")
    return hashlib.sha256(data).hexdigest()


def find_cargo() -> str:
    for candidate in (os.environ.get("CARGO"), shutil.which("cargo")):
        if candidate:
            return candidate
    home = Path(os.path.expanduser("~")) / ".cargo" / "bin" / "cargo.exe"
    if home.exists():
        return str(home)
    sys.exit("cargo not found (set CARGO or put it on PATH)")


def metadata() -> dict:
    out = subprocess.run(
        [find_cargo(), "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET],
        cwd=ROOT,
        check=True,
        capture_output=True,
    ).stdout
    return json.loads(out)


def shipped_packages(meta: dict) -> list:
    """Packages reachable from the root over normal dependencies, root excluded, sorted."""
    by_id = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    seen = {root}
    stack = [root]
    while stack:
        node = nodes.get(stack.pop())
        if node is None:
            continue
        for dep in node["deps"]:
            # kind None = normal dependency; "build" and "dev" are not linked into the exe.
            if any(k.get("kind") is None for k in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    seen.discard(root)
    return sorted((by_id[i] for i in seen), key=lambda p: (p["name"].lower(), p["version"]))


def license_files(package_dir: Path) -> list:
    """Relative paths of license/notice files inside the crate, shallowest first."""
    found = []
    for current, dirs, files in os.walk(package_dir):
        depth = len(Path(current).relative_to(package_dir).parts)
        dirs[:] = [d for d in sorted(dirs) if d not in SKIP_DIRS] if depth < MAX_DEPTH else []
        # Font licenses sit next to the font under the font's name (Hack-Regular.ttf + Hack-Regular.txt).
        font_stems = {Path(n).stem for n in files if Path(n).suffix.lower() in FONT_SUFFIXES}
        for name in sorted(files):
            path = Path(current) / name
            suffix = path.suffix.lower()
            stem_ok = (
                LICENSE_NAME.match(name)
                or LICENSE_NAME.match(path.stem)
                or (suffix in TEXT_SUFFIXES and LICENSE_WORD.search(name))
                or (suffix in TEXT_SUFFIXES - {""} and path.stem in font_stems)
            )
            if stem_ok and path.stat().st_size <= MAX_FILE_BYTES:
                found.append(path.relative_to(package_dir))
    return sorted(found, key=lambda p: (len(p.parts), str(p).lower()))


def read_text(path: Path) -> str:
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8-sig")
    except UnicodeDecodeError:
        text = raw.decode("latin-1")
    return text.replace("\r\n", "\n").replace("\r", "\n").strip("\n")


def build() -> tuple:
    meta = metadata()
    packages = shipped_packages(meta)
    lines = [
        HASH_PREFIX + lock_hash(),
        "",
        "Third-party licenses of Cerberus Usage",
        "======================================",
        "",
        "Cerberus Usage itself is MIT licensed (see LICENSE). The executable contains the Rust",
        f"packages below (Windows build, target {TARGET}). Their license texts follow.",
        "Generated by tools/third_party_licenses.py from Cargo.lock. Do not edit by hand.",
        "",
        f"Packages: {len(packages)}",
        "",
    ]
    missing = []
    emitted = {}  # text hash -> "name version / file" of the first occurrence
    for p in packages:
        expr = p.get("license") or ("see " + p["license_file"] if p.get("license_file") else "UNSPECIFIED")
        package_dir = Path(p["manifest_path"]).parent
        files = license_files(package_dir)
        if not files:
            missing.append((p["name"], p["version"], expr))
        lines += ["-" * 78, f"{p['name']} {p['version']}", f"License: {expr}"]
        if p.get("repository"):
            lines.append(f"Source: {p['repository']}")
        for rel in files:
            text = read_text(package_dir / rel)
            digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
            lines += ["", f"[{rel.as_posix()}]"]
            if digest in emitted:
                lines.append(f"(same text as {emitted[digest]})")
            else:
                emitted[digest] = f"{p['name']} {p['version']}, {rel.as_posix()}"
                lines.append(text)
        lines.append("")
    if missing:
        lines += [
            "-" * 78,
            "Packages without a license file in their source (license expression as declared):",
            "",
        ]
        lines += [f"  {n} {v}: {e}" for n, v, e in missing]
        lines.append("")
    return "\n".join(lines).rstrip("\n") + "\n", missing


def main() -> int:
    text, missing = build()
    if "--check" in sys.argv:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current.split("\n", 1)[0] != HASH_PREFIX + lock_hash():
            print("installer/THIRD-PARTY-LICENSES.txt is out of date: run tools/third_party_licenses.py")
            return 1
        return 0
    OUTPUT.write_text(text, encoding="utf-8", newline="\n")
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(text.encode('utf-8'))} bytes)")
    if missing:
        print("packages without a license file:")
        for name, version, expr in missing:
            print(f"  {name} {version}: {expr}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
