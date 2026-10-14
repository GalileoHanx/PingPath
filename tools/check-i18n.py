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
  app.js and the page's other scripts (frontend/*.js), and tr("key")
  calls in the Rust side (client/ui/src), which shows the tray menu and
  Windows notifications;
- words are written into the page instead of coming from the files: text
  in index.html that no data-i18n replaces, an aria-label, title, alt or
  placeholder written in there, a string literal app.js puts on screen
  (`text: "..."`, `textContent = "..."`, setText(node, "...")), or a t()
  call whose key isn't written out, so this script can't check it;
- a code the service or the engine can send has no words in either
  language, or the app never shows them. The codes are read from the Rust
  source, so a new variant fails this check until it is translated and
  app.js has a case for it (see CODES below):
  NoticeKind, FallbackCause, EngineError and Mode
  (client/core/src/engine/types.rs), Refusal (client/core/src/policy.rs),
  SupportStatus (crates/games/src/model.rs), every error code the service
  answers with (ErrorBody::new in client/service/src and
  client/core/src/ipc.rs), the app's own error codes (client/ui/src) and
  the reasons the service can't be reached (client/ui/src/service.rs,
  mod reason).

Keys no code refers to are listed as a warning, not a failure.
Standard library only.
"""

import json
import re
import sys
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UI = ROOT / "client" / "ui"
I18N = UI / "frontend" / "i18n"
LANGUAGES = ("en", "ar")

HTML_KEY = re.compile(r"""\bdata-i18n(?:-[a-z-]+)?\s*=\s*["']([^"']+)["']""")
JS_KEY = re.compile(r"""(?<![\w.$])t\(\s*["']([^"'\\]+)["']""")
RUST_KEY = re.compile(r"""\btr\(\s*"([^"\\]+)"\s*\)""")
PLACEHOLDER = re.compile(r"\{(\w+)\}")
ERROR_BODY = re.compile(r"""\bErrorBody::(?:new\(\s*"([a-z0-9_]+)"|(bad_request)\()""")

# Words a person reads: any letter, Latin or Arabic.
LETTER = re.compile(r"[^\W\d_]")
# Attributes a person reads or hears; index.html sets them through
# data-i18n-<attribute>="key", never written in.
SPOKEN_ATTRIBUTES = ("aria-label", "title", "placeholder", "alt")
VOID_ELEMENTS = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"}
# t(...) whose first argument isn't a string literal (not t's own
# definition).
JS_DYNAMIC_KEY = re.compile(r"""(?<![\w.$])(?<!function )t\(\s*(?!["'\s])([^,)]*)""")
# The only keys app.js may take from elsewhere: index.html's data-i18n
# attributes, which this script checks there.
DYNAMIC_KEYS_ALLOWED = ("node.dataset.",)
# A string literal app.js puts on screen or in a label: `text: "..."` (its
# el() helper), `title:`, `"aria-label":`, `.textContent = "..."` and
# setText(node, "...").
JS_LITERAL_TEXT = re.compile(
    r"""(?:\b(?:text|title|placeholder)|["']aria-label["'])\s*:\s*(["'`])((?:\\.|(?!\1).)*)\1"""
    r"""|\b(?:textContent|innerText|ariaLabel)\s*=\s*(["'`])((?:\\.|(?!\3).)*)\3"""
    r"""|\bsetText\([^,()]+,\s*(["'`])((?:\\.|(?!\5).)*)\5"""
)

# The codes the Rust side sends, and the key each needs. `{}` is the code
# as it goes over the wire (serde's rename_all applied). A key ending in
# `{}*` may be split by the app into several keys that start with it (a
# refusal of a game that is `untested` or `unsupported`, say).
ENUMS = [
    ("client/core/src/engine/types.rs", "NoticeKind", "notice.{}"),
    ("client/core/src/engine/types.rs", "FallbackCause", "notice.cause.{}"),
    ("client/core/src/engine/types.rs", "EngineError", "error.{}"),
    ("client/core/src/engine/types.rs", "Mode", "optimize.mode.{}"),
    ("client/core/src/policy.rs", "Refusal", "refusal.{}*"),
    ("crates/games/src/model.rs", "SupportStatus", "game.status.{}"),
]
# Every error code the service answers a request with (besides the
# engine's), and the app's own (a request that timed out, ...).
ERROR_SOURCES = [
    ("client/service/src", "error.{}"),
    ("client/core/src/ipc.rs", "error.{}"),
    ("client/ui/src", "error.{}"),
]
REASONS = ("client/ui/src/service.rs", "reason", "connection.{}")


class SourceError(Exception):
    """The Rust source isn't shaped as this script expects."""


def lex(text):
    """Rust source without comments, with every string literal replaced by
    `"<n>"` (its index in the returned list), so brackets can be matched
    and attributes read without tripping over either."""
    out, strings = [], []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif (raw := re.match(r'(b?r)(#*)"', text[i:])) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            close = '"' + raw.group(2)
            start = i + raw.end()
            end = text.find(close, start)
            if end < 0:
                raise SourceError("unterminated raw string")
            strings.append(text[start:end])
            out.append(f'"{len(strings) - 1}"')
            i = end + len(close)
        elif c == '"':
            j, value = i + 1, []
            while j < n and text[j] != '"':
                if text[j] == "\\":
                    value.append(text[j : j + 2])
                    j += 2
                else:
                    value.append(text[j])
                    j += 1
            strings.append("".join(value))
            out.append(f'"{len(strings) - 1}"')
            i = j + 1
        elif c == "'" and re.match(r"'(\\.|[^\\'])'", text[i:]):
            # A character literal ('"' or '{' would confuse the rest);
            # lifetimes ('a) don't match and pass through.
            i += len(re.match(r"'(\\.|[^\\'])'", text[i:]).group(0))
            out.append("' '")
        else:
            out.append(c)
            i += 1
    return "".join(out), strings


def matching(text, start):
    """The index of the bracket that closes the one at `start`."""
    pairs = {"{": "}", "(": ")", "[": "]"}
    stack = []
    for i in range(start, len(text)):
        if text[i] in pairs:
            stack.append(pairs[text[i]])
        elif text[i] in pairs.values():
            if not stack or stack.pop() != text[i]:
                raise SourceError(f"unbalanced {text[i]!r}")
            if not stack:
                return i
    raise SourceError("unclosed bracket")


def split_top(text, sep=","):
    """Splits on `sep` outside brackets."""
    parts, depth, last = [], 0, 0
    for i, c in enumerate(text):
        if c in "{([":
            depth += 1
        elif c in "})]":
            depth -= 1
        elif c == sep and depth == 0:
            parts.append(text[last:i])
            last = i + 1
    parts.append(text[last:])
    return parts


def attributes(text):
    """The leading `#[...]` attributes of `text` (contents only), and the rest."""
    found = []
    text = text.lstrip()
    while text.startswith("#["):
        end = matching(text, 1)
        found.append(text[2:end])
        text = text[end + 1 :].lstrip()
    return found, text


def serde_value(attrs, name, strings):
    """`name = "..."` from a `serde(...)` attribute, if there is one."""
    for attr in attrs:
        if not attr.startswith("serde"):
            continue
        match = re.search(rf'\b{name}\s*=\s*"(\d+)"', attr)
        if match:
            return strings[int(match.group(1))]
    return None


def renamed(variant, rule):
    words = re.findall(r"[A-Z]+(?![a-z])|[A-Z][a-z0-9]*|[a-z0-9]+", variant)
    if rule == "snake_case":
        return "_".join(word.lower() for word in words)
    if rule == "kebab-case":
        return "-".join(word.lower() for word in words)
    if rule in (None, ""):
        return variant
    raise SourceError(f"rename_all = {rule!r} isn't handled here")


def enum_codes(path, name):
    """The wire names of enum `name`'s variants in `path`."""
    code, strings = lex((ROOT / path).read_text(encoding="utf-8"))
    match = re.search(rf"\bpub\s+enum\s+{name}\s*\{{", code)
    if not match:
        raise SourceError(f"no `pub enum {name}` in {path}")
    # The enum's own attributes: from the end of the previous item.
    previous = max(code.rfind("}", 0, match.start()), code.rfind(";", 0, match.start()))
    own, _ = attributes(code[previous + 1 : match.start()])
    rule = serde_value(own, "rename_all", strings)
    body_start = match.end() - 1
    body = code[body_start + 1 : matching(code, body_start)]
    codes = []
    for part in split_top(body):
        attrs, rest = attributes(part)
        ident = re.match(r"([A-Z]\w*)", rest)
        if not ident:
            if rest.strip():
                raise SourceError(f"{path}: can't read a variant of {name} from {rest.strip()[:40]!r}")
            continue
        codes.append(serde_value(attrs, "rename", strings) or renamed(ident.group(1), rule))
    if not codes:
        raise SourceError(f"{path}: {name} has no variants")
    return codes


def rust_files(path):
    path = ROOT / path
    return sorted(path.rglob("*.rs")) if path.is_dir() else [path]


def without_tests(text):
    """Rust source up to its unit tests, which use made-up codes and keys."""
    return text.split("#[cfg(test)]")[0]


def error_codes(path):
    found = set()
    for file in rust_files(path):
        for match in ERROR_BODY.finditer(without_tests(file.read_text(encoding="utf-8"))):
            found.add(match.group(1) or match.group(2))
    if not found:
        raise SourceError(f"no ErrorBody codes found in {path}")
    return sorted(found)


def module_consts(path, module):
    code, strings = lex((ROOT / path).read_text(encoding="utf-8"))
    match = re.search(rf"\bpub\s+mod\s+{module}\s*\{{", code)
    if not match:
        raise SourceError(f"no `pub mod {module}` in {path}")
    body = code[match.end() : matching(code, match.end() - 1)]
    found = [strings[int(i)] for i in re.findall(r'\bconst\s+\w+\s*:\s*&str\s*=\s*"(\d+)"', body)]
    if not found:
        raise SourceError(f"{path}: mod {module} has no string constants")
    return found


def required_codes():
    """[(what, code, key pattern)] for every code the app must put in words."""
    required = []
    for path, name, key in ENUMS:
        required += [(f"{name} in {path}", code, key) for code in enum_codes(path, name)]
    for path, key in ERROR_SOURCES:
        required += [(f"an error code in {path}", code, key) for code in error_codes(path)]
    path, module, key = REASONS
    required += [(f"mod {module} in {path}", code, key) for code in module_consts(path, module)]
    return required


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
        text = without_tests(text)
    text = re.sub(r"/\*.*?\*/", blank, text, flags=re.S)
    return re.sub(r"(^|\s)//[^\n]*", lambda m: m.group(1), text, flags=re.M)


class VisibleText(HTMLParser):
    """Words in index.html that no data-i18n attribute replaces: text, and
    attributes a person reads or hears. app.js replaces the text of an
    element with data-i18n="key", children included."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.open = []  # (tag, translated) for each element still open
        self.found = []  # (line, what)

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name in SPOKEN_ATTRIBUTES and value and LETTER.search(value):
                self.found.append((self.getpos()[0], f'{name}="{value}"'))
        if tag not in VOID_ELEMENTS:
            self.open.append((tag, any(name == "data-i18n" for name, _ in attrs)))

    def handle_endtag(self, tag):
        while self.open:
            if self.open.pop()[0] == tag:
                break

    def handle_data(self, data):
        if LETTER.search(data) and not any(translated for _, translated in self.open):
            self.found.append((self.getpos()[0], data.strip()))


def js_files():
    """The page's scripts: app.js and the files it shares the page with."""
    return sorted((UI / "frontend").glob("*.js"))


def check_hard_coded(problems):
    """Words written into index.html or app.js instead of coming from the
    translations, and t() calls whose key this script can't read."""
    html_path = UI / "frontend" / "index.html"
    try:
        html = code_only(html_path, html_path.read_text(encoding="utf-8"))
        scripts = [(path, code_only(path, path.read_text(encoding="utf-8"))) for path in js_files()]
    except OSError as error:
        problems.append(f"cannot read the frontend: {error}")
        return
    parser = VisibleText()
    parser.feed(html)
    parser.close()
    for line, what in parser.found:
        problems.append(
            f"{html_path.relative_to(ROOT)}:{line}: {what!r} isn't translated: "
            "put it in en.json and ar.json and use data-i18n (or data-i18n-<attribute>)"
        )
    for js_path, js in scripts:
        for match in JS_DYNAMIC_KEY.finditer(js):
            argument = match.group(1).strip()
            if not argument.startswith(DYNAMIC_KEYS_ALLOWED):
                line = js.count("\n", 0, match.start()) + 1
                problems.append(
                    f"{js_path.relative_to(ROOT)}:{line}: t({argument}) has a key this check can't read; "
                    'call t("key") with the key written out'
                )
        for match in JS_LITERAL_TEXT.finditer(js):
            literal = next(group for group in match.groups()[1::2] if group is not None)
            if LETTER.search(re.sub(r"\$\{[^}]*\}", "", literal)):
                line = js.count("\n", 0, match.start()) + 1
                problems.append(
                    f"{js_path.relative_to(ROOT)}:{line}: {literal!r} is shown as written; "
                    'put it in en.json and ar.json and use t("key")'
                )


def used_keys():
    """{key: [where it is used]} for every key the app refers to."""
    sources = [(UI / "frontend" / "index.html", HTML_KEY)]
    sources += [(path, JS_KEY) for path in js_files()]
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


def check_codes(files, used, problems):
    """Every code the Rust side sends has words in both languages, and the
    app uses them."""
    try:
        required = required_codes()
    except (OSError, SourceError) as error:
        problems.append(f"cannot read the codes from the Rust source: {error}")
        return 0
    for what, code, pattern in required:
        key = pattern.replace("{}", code)
        if key.endswith("*"):
            prefix = key[:-1]
            keys = sorted(k for k in files["en"] if k == prefix or k.startswith(prefix + "_"))
        else:
            keys = [key] if key in files["en"] else []
        if not keys:
            problems.append(f"{code!r} ({what}) has no translation: add {key.rstrip('*')!r} to en.json and ar.json")
            continue
        for k in keys:
            if k not in files["ar"]:
                problems.append(f"ar.json lacks {k!r}, the words for {code!r} ({what})")
        if not any(k in used for k in keys):
            problems.append(f"{code!r} ({what}): app.js never shows {' or '.join(map(repr, keys))}")
    return len(required)


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

    codes = check_codes(files, used, problems)
    check_hard_coded(problems)

    unused = sorted(set(en) - set(used))
    for key in unused:
        print(f"warning: {key!r} is not used by index.html, app.js or the Rust side")

    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        print(f"check-i18n: {len(problems)} problem(s)", file=sys.stderr)
        return 1
    print(
        f"check-i18n: {len(en)} keys in both languages, {len(used)} used, all present; "
        f"{codes} codes from the Rust source, all translated and shown"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
