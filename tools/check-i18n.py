#!/usr/bin/env python3
"""Checks the desktop app's translations (PLAN.md Phase 3: every
user-facing string comes from translation files, in Arabic and English).

    python3 tools/check-i18n.py

Fails if:

- client/ui/frontend/i18n/en.json and ar.json don't have the same keys,
  or a file isn't a flat JSON object of strings, or repeats a key;
- a value is empty;
- a key and its translation don't use the same {placeholders};
- a key the app uses is missing: data-i18n attributes (data-i18n="key",
  data-i18n-aria-label="key", ...) in index.html, t("key") calls in
  app.js, and tr("key") calls in the Rust side (client/ui/src), which
  shows the tray menu and Windows notifications.

Keys no code refers to are listed as a warning, not a failure.
Standard library only.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UI = ROOT / "client" / "ui"
I18N = UI / "frontend" / "i18n"
LANGUAGES = ("en", "ar")

HTML_KEY = re.compile(r"""\bdata-i18n(?:-[a-z-]+)?\s*=\s*["']([^"']+)["']""")
JS_KEY = re.compile(r"""(?<![\w.$])t\(\s*["']([^"'\\]+)["']""")
RUST_KEY = re.compile(r"""\btr\(\s*"([^"\\]+)"\s*\)""")
PLACEHOLDER = re.compile(r"\{(\w+)\}")


def load(path, problems):
    """A translation file as a dict; problems are appended to `problems`."""
    seen = []

    def no_duplicates(pairs):
        keys = [key for key, _ in pairs]
        seen.extend(key for key in set(keys) if keys.count(key) > 1)
        return dict(pairs)

    try:
        data = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=no_duplicates)
    except (OSError, ValueError) as error:
        problems.append(f"{path.relative_to(ROOT)}: cannot read: {error}")
        return {}
    if not isinstance(data, dict):
        problems.append(f"{path.relative_to(ROOT)}: not a JSON object")
        return {}
    for key in sorted(seen):
        problems.append(f"{path.relative_to(ROOT)}: key {key!r} appears more than once")
    for key, value in data.items():
        if not isinstance(value, str):
            problems.append(f"{path.relative_to(ROOT)}: {key!r} is not a string")
        elif not value.strip():
            problems.append(f"{path.relative_to(ROOT)}: {key!r} is empty")
    return data


def blank(match):
    """Replaces a comment with as many newlines, so line numbers still match."""
    return "\n" * match.group(0).count("\n")


def code_only(path, text):
    """The text without comments (which mention keys as examples), and for
    Rust without its unit tests (which look up keys that don't exist)."""
    if path.suffix == ".html":
        return re.sub(r"<!--.*?-->", blank, text, flags=re.S)
    if path.suffix == ".rs":
        text = text.split("#[cfg(test)]")[0]
    text = re.sub(r"/\*.*?\*/", blank, text, flags=re.S)
    return re.sub(r"(^|\s)//[^\n]*", lambda m: m.group(1), text, flags=re.M)


def used_keys():
    """{key: [where it is used]} for every key the app refers to."""
    sources = [(UI / "frontend" / "index.html", HTML_KEY), (UI / "frontend" / "app.js", JS_KEY)]
    sources += [(path, RUST_KEY) for path in sorted((UI / "src").glob("*.rs"))]
    used = {}
    for path, pattern in sources:
        try:
            text = code_only(path, path.read_text(encoding="utf-8"))
        except OSError as error:
            print(f"error: cannot read {path.relative_to(ROOT)}: {error}", file=sys.stderr)
            sys.exit(1)
        for match in pattern.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            used.setdefault(match.group(1), []).append(f"{path.relative_to(ROOT)}:{line}")
    return used


def main():
    problems = []
    files = {lang: load(I18N / f"{lang}.json", problems) for lang in LANGUAGES}

    en, ar = files["en"], files["ar"]
    for key in sorted(set(en) - set(ar)):
        problems.append(f"ar.json lacks {key!r} (en.json has it)")
    for key in sorted(set(ar) - set(en)):
        problems.append(f"en.json lacks {key!r} (ar.json has it)")
    for key in sorted(set(en) & set(ar)):
        if not (isinstance(en[key], str) and isinstance(ar[key], str)):
            continue
        en_names = set(PLACEHOLDER.findall(en[key]))
        ar_names = set(PLACEHOLDER.findall(ar[key]))
        if en_names != ar_names:
            problems.append(
                f"{key!r}: placeholders differ: en {sorted(en_names)}, ar {sorted(ar_names)}"
            )

    used = used_keys()
    for key in sorted(used):
        for lang in LANGUAGES:
            if key not in files[lang]:
                problems.append(f"{lang}.json lacks {key!r}, used at {', '.join(used[key])}")

    unused = sorted(set(en) - set(used))
    for key in unused:
        print(f"warning: {key!r} is not used by index.html, app.js or the Rust side")

    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        print(f"check-i18n: {len(problems)} problem(s)", file=sys.stderr)
        return 1
    print(f"check-i18n: {len(en)} keys in both languages, {len(used)} used, all present")
    return 0


if __name__ == "__main__":
    sys.exit(main())
