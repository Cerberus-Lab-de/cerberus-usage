#!/usr/bin/env python3
"""Writes a test language "zz" from lang/en.json.

Every text is wrapped as [[...]] with its {placeholders} untouched, so a text that is still
hard-coded (and therefore not wrapped) stands out in the window, and a text that gets too long
shows where the layout breaks. One text (button.browse) is Cyrillic, to check that the font
can draw it.

Usage: python tools/make_test_language.py <out-file>
Put the result into the `lang` folder next to the app and pick "Test (zz)" in the settings.
Standard library only.
"""

import json
import sys
from pathlib import Path

CYRILLIC_KEY = "button.browse"
CYRILLIC_TEXT = "Обзор"


def build(source: Path) -> dict:
    en = json.loads(source.read_text(encoding="utf-8-sig"))
    out = {"_meta": {"code": "zz", "name": "Test (zz)", "format": 1}}
    for key, value in en.items():
        if key == "_meta":
            continue
        out[key] = "[[" + (CYRILLIC_TEXT if key == CYRILLIC_KEY else value) + "]]"
    if CYRILLIC_KEY not in out:
        raise SystemExit(f"{CYRILLIC_KEY} is missing in {source}")
    return out


def main(argv: list) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    source = Path(__file__).resolve().parent.parent / "lang" / "en.json"
    result = build(source)
    target = Path(argv[1])
    target.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
    print(f"wrote {target} with {len(result) - 1} texts")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
