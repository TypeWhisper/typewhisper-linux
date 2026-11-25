#!/usr/bin/env python3
"""Update the gettext catalogs in po/ from the Rust sources.

As in the macOS app's string catalog, the English text is the message ID. The
script collects every text passed to tr(), tr_with() and n_() in crates/*/src,
and with xgettext the translatable texts of the desktop file and AppStream
templates in packaging/, writes them to po/typewhisper.pot and to every
po/<language>.po listed in po/LINGUAS. Translations already in a catalog are
kept, texts no longer used are dropped. xgettext needs the AppStream ITS rules,
which Linux distributions install with AppStream.

With the path to the macOS app's Localizable.xcstrings, it also fills empty
translations from the Mac's when the English texts match and neither side has
placeholders:

    tools/i18n/update-translations.py path/to/typewhisper-mac/TypeWhisper/Resources/Localizable.xcstrings

It ends by listing the texts that still need a translation. With --check it
changes nothing and exits with 1 if a catalog is out of date or a translation
is missing.
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
PO_DIR = ROOT / "po"
DOMAIN = "typewhisper"
# gettext language code -> macOS string catalog language.
MAC_LANGUAGES = {"de": "de", "es": "es", "ja": "ja", "zh_CN": "zh-Hans"}
CALL = re.compile(r'\b(?:tr|tr_with|n_)\(\s*"')
ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", "\\": "\\", '"': '"', "'": "'"}
MAC_SPECIFIER = re.compile(r"%")
LINUX_PLACEHOLDER = re.compile(r"\{[a-z_]+\}")
# Templates that tools/i18n/translate-packaging.sh fills with the translations.
PACKAGING = [
    "packaging/com.typewhisper.TypeWhisper.desktop.in",
    "packaging/com.typewhisper.TypeWhisper.metainfo.xml.in",
    "packaging/flatpak/com.typewhisper.TypeWhisper.Plugin.Parakeet.metainfo.xml.in",
]


def rust_string(source, start):
    """Reads a Rust string literal whose opening quote is just before `start`."""
    text = []
    index = start
    while True:
        char = source[index]
        if char == '"':
            return "".join(text)
        if char != "\\":
            text.append(char)
            index += 1
            continue
        escape = source[index + 1]
        if escape == "\n":
            # A line continuation also drops the next line's indentation.
            index += 2
            while source[index] in " \t\n\r":
                index += 1
        elif escape == "u":
            end = source.index("}", index)
            text.append(chr(int(source[index + 3 : end], 16)))
            index = end + 1
        else:
            text.append(ESCAPES[escape])
            index += 2


def source_texts():
    """Returns {text: [files]} in the order the texts first appear."""
    texts = {}
    for path in sorted(ROOT.glob("crates/*/src/**/*.rs")):
        source = path.read_text(encoding="utf-8")
        # Tests sit at the end of a file and need no translations.
        source = source.split("#[cfg(test)]\nmod tests")[0]
        relative = path.relative_to(ROOT).as_posix()
        for match in CALL.finditer(source):
            text = rust_string(source, match.end())
            files = texts.setdefault(text, [])
            if relative not in files:
                files.append(relative)
    return texts


def packaging_texts():
    """Returns {text: [files]} for the packaging templates, as xgettext reads them.

    msgfmt looks the texts up exactly as xgettext extracts them, with the
    whitespace of the XML paragraphs folded, so the script takes xgettext's word.
    """
    result = subprocess.run(
        ["xgettext", "--from-code=UTF-8", "--omit-header", "--no-wrap", "-o", "-", *PACKAGING],
        cwd=ROOT, capture_output=True, text=True,
    )
    if result.returncode != 0:
        sys.exit(f"xgettext failed:\n{result.stderr}")
    texts = {}
    files = []
    for line in result.stdout.splitlines():
        if line.startswith("#:"):
            files += [reference.rsplit(":", 1)[0] for reference in line[2:].split()]
        elif line.startswith("msgid "):
            text = po_string(line[6:])
            if text:
                entry = texts.setdefault(text, [])
                entry += [file for file in dict.fromkeys(files) if file not in entry]
            files = []
    return texts


def read_po(path):
    """Returns {msgid: msgstr} of a catalog, without the header and fuzzy entries."""
    entries = {}
    if not path.exists():
        return entries
    msgid = msgstr = None
    field = None
    fuzzy = False

    def finish():
        if msgid and msgstr and not fuzzy:
            entries[msgid] = msgstr

    for raw in path.read_text(encoding="utf-8").splitlines() + [""]:
        line = raw.strip()
        if not line:
            finish()
            msgid = msgstr = field = None
            fuzzy = False
        elif line.startswith("#,"):
            fuzzy = "fuzzy" in line
        elif line.startswith("#"):
            continue
        elif line.startswith("msgid "):
            msgid, field = po_string(line[6:]), "msgid"
        elif line.startswith("msgstr "):
            msgstr, field = po_string(line[7:]), "msgstr"
        elif line.startswith('"') and field == "msgid":
            msgid += po_string(line)
        elif line.startswith('"') and field == "msgstr":
            msgstr += po_string(line)
    return entries


def po_string(quoted):
    """The text of one quoted PO string."""
    body = quoted.strip()[1:-1]
    return re.sub(r'\\(.)', lambda m: ESCAPES.get(m.group(1), m.group(1)), body)


def po_quote(text):
    escaped = text.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t")
    return f'"{escaped}"'


def header(language):
    lines = [
        "Project-Id-Version: typewhisper",
        "Report-Msgid-Bugs-To: https://github.com/TypeWhisper/typewhisper-linux/issues",
        "Language: " + language,
        "MIME-Version: 1.0",
        "Content-Type: text/plain; charset=UTF-8",
        "Content-Transfer-Encoding: 8bit",
    ]
    return 'msgid ""\nmsgstr ""\n' + "".join(po_quote(line + "\n") + "\n" for line in lines)


def render(texts, translations, language):
    title = f"translations into {language}" if language else "message template"
    parts = [
        f"# TypeWhisper for Linux: {title}.\n"
        "# Generated by tools/i18n/update-translations.py; edit only the msgstr lines.\n"
        "# This file is distributed under the same license as TypeWhisper (GPL-3.0-or-later).\n"
        + header(language)
    ]
    for text, files in texts.items():
        placeholders = sorted(set(LINUX_PLACEHOLDER.findall(text)))
        comment = f"#. Leave {' and '.join(placeholders)} as it is.\n" if placeholders else ""
        parts.append(
            comment
            + f"#: {' '.join(files)}\n"
            + f"msgid {po_quote(text)}\n"
            + f"msgstr {po_quote(translations.get(text, ''))}\n"
        )
    return "\n".join(parts)


def mac_translations(path, mac_language):
    """{English text: translation} for every translated Mac string without placeholders."""
    result = {}
    for key, entry in json.loads(Path(path).read_text(encoding="utf-8"))["strings"].items():
        localizations = entry.get("localizations", {})
        english = localizations.get("en", {}).get("stringUnit", {}).get("value", key)
        unit = localizations.get(mac_language, {}).get("stringUnit")
        if not unit or unit.get("state") != "translated" or not unit.get("value"):
            continue
        value = unit["value"]
        if MAC_SPECIFIER.search(english) or MAC_SPECIFIER.search(value):
            continue
        result.setdefault(english, value)
        result.setdefault(key, value)
    return result


def placeholder_problems(text, translation):
    """What is wrong with the placeholders of `translation`; empty when it keeps those of `text`."""
    expected = sorted(LINUX_PLACEHOLDER.findall(text))
    found = sorted(LINUX_PLACEHOLDER.findall(translation))
    problems = [f"missing {name}" for name in expected if name not in found]
    problems += [f"unknown {name}" for name in found if name not in expected]
    if not problems and expected != found:
        problems.append("placeholders repeated")
    # The desktop file's keywords are a list that ends with a semicolon.
    if text.endswith(";") and not translation.endswith(";"):
        problems.append("keyword list without the final ;")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("catalog", nargs="?", help="the macOS Localizable.xcstrings to take translations from")
    parser.add_argument("--check", action="store_true", help="change nothing; fail if a catalog is out of date or incomplete")
    arguments = parser.parse_args()

    texts = source_texts()
    for text, files in packaging_texts().items():
        entry = texts.setdefault(text, [])
        entry += [file for file in files if file not in entry]
    languages = (PO_DIR / "LINGUAS").read_text(encoding="utf-8").split()
    stale = []
    missing = 0
    broken = 0
    outputs = {PO_DIR / f"{DOMAIN}.pot": render(texts, {}, "")}
    for language in languages:
        path = PO_DIR / f"{language}.po"
        existing = read_po(path)
        translations = {text: existing[text] for text in texts if text in existing}
        imported = 0
        if arguments.catalog:
            mac = mac_translations(arguments.catalog, MAC_LANGUAGES.get(language, language))
            for text in texts:
                if text not in translations and text in mac and not LINUX_PLACEHOLDER.search(text):
                    translations[text] = mac[text]
                    imported += 1
        outputs[path] = render(texts, translations, language)
        todo = [text for text in texts if text not in translations]
        dropped = sorted(existing.keys() - texts.keys())
        missing += len(todo)
        print(f"{language}: {imported} imported, {len(todo)} missing, {len(dropped)} no longer used")
        for text in todo:
            print("  missing: " + json.dumps(text, ensure_ascii=False))
        for text, translation in translations.items():
            problems = placeholder_problems(text, translation)
            broken += bool(problems)
            if problems:
                print(f"  {', '.join(problems)}: " + json.dumps(text, ensure_ascii=False))
    for path, content in outputs.items():
        current = path.read_text(encoding="utf-8") if path.exists() else None
        if current == content:
            continue
        if arguments.check:
            stale.append(path.relative_to(ROOT).as_posix())
        else:
            with path.open("w", encoding="utf-8", newline="\n") as file:
                file.write(content)
    for path in stale:
        print(f"out of date: {path} (run tools/i18n/update-translations.py)")
    return 1 if stale or broken or (arguments.check and missing) else 0


if __name__ == "__main__":
    sys.exit(main())
