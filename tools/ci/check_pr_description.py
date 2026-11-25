#!/usr/bin/env python3
"""Check a pull request's title and description against the rules in AGENTS.md.

    PR_TITLE=... PR_BODY=... check_pr_description.py [--base HEAD^1] [--summary FILE]

The workflow sets PR_TITLE and PR_BODY from the event and checks out the merge commit, whose
first parent is the base branch, so `git diff --base HEAD` holds the pull request's changes.

- The title starts with its category: (feature), (bug), (perf) or (chore).
- A pull request that changes something visible has a screenshot or a clip in its
  description, unless "- [x] No visible UI change" is ticked. Visible changes are interface
  files, style sheets, icons and translations, and changed lines that build widgets in the
  app's Rust code or in the GNOME Shell extension.
- A (perf) pull request has a measurement table in its description.

HTML comments do not count for any rule. The same check runs in the macOS repository.
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys

CATEGORIES = ("feature", "bug", "perf", "chore")
TITLE = re.compile(r"^\((?:" + "|".join(CATEGORIES) + r")\) \S")

HTML_COMMENT = re.compile(r"<!--.*?-->", re.S)
MEDIA = (
    re.compile(r"!\[[^\]]*\]\(\s*\S+?\s*\)"),
    re.compile(r"<(?:img|video)\b", re.I),
    re.compile(r"https://github\.com/user-attachments/assets/[0-9A-Za-z-]+"),
    re.compile(r"https://(?:private-)?user-images\.githubusercontent\.com/\S+"),
)
NO_UI_CHANGE = re.compile(r"^\s*[-*]\s*\[[xX]\]\s*No visible UI change\b", re.M)
LIST_ITEM = re.compile(r" {0,3}(?:[-*+]|\d{1,9}[.)])\s")

# Files that are interface by themselves: GTK Builder and Blueprint files, style sheets,
# icons and translations. Only shipped folders count, not tools, docs or tests.
UI_RESOURCES = re.compile(r"\.(?:ui|blp|css|svg|po|pot)$")
SHIPPED = ("crates/", "plugins/", "packaging/", "gnome-shell-extension/")
NOT_SHIPPED = re.compile(r"(?:^|/)(?:tests|examples|benches|fixtures)/")
RUST_VIEW_CODE = re.compile(
    # A widget type, as in gtk::Label::new, adw::ActionRow::builder or a field of that type.
    r"\b(?:gtk|adw)::[A-Z]\w*"
    # Builder and setter calls that change what a widget shows.
    r"|\.(?:set_)?(?:label|title|subtitle|text|markup|tooltip_text|placeholder_text|icon_name|css_classes|"
    r"visible|child|content|title_widget|default_size|pixel_size|spacing|halign|valign|orientation|"
    r"margin_(?:top|bottom|start|end)|width_request|height_request|fraction)\s*\("
    r"|\.(?:append|prepend|add_css_class|remove_css_class|add_prefix|add_suffix|add_toast|add_top_bar|"
    r"add_named|pack_start|pack_end|present|load_from_string)\s*\("
)
JS_VIEW_CODE = re.compile(
    r"\bnew\s+(?:St|Clutter|PanelMenu|PopupMenu)\.\w+"
    r"|\bstyle_class\b|\.(?:set_style|add_style_class_name|remove_style_class_name|add_child|remove_child|"
    r"addTopChrome|addChrome|set_position|set_size|ease|show|hide)\s*\("
    r"|\.(?:text|opacity|visible|icon_name|gicon)\s*="
)
# A line that holds nothing but a string: the wrapped argument of a call such as
# `row.set_subtitle(` on the line above, which the diff does not show.
STRING_ARGUMENT = re.compile(r"""^(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')\s*[,)]*;?$""")

# Widgets are built in the app crate and in the GNOME Shell extension; the other crates and
# the plugins have no interface.
VIEW_CODE = (
    ("crates/typewhisper/", ".rs", (RUST_VIEW_CODE, STRING_ARGUMENT)),
    ("gnome-shell-extension/", ".js", (JS_VIEW_CODE, STRING_ARGUMENT)),
)


def visible_text(body):
    """The description without HTML comments."""
    return HTML_COMMENT.sub("", body or "")


def has_media(text):
    return any(pattern.search(text) for pattern in MEDIA)


def has_table(text):
    """A Markdown table with at least two columns and a row that holds a number."""
    lines = text.splitlines()
    for index in range(1, len(lines)):
        delimiter = lines[index].strip()
        if not ("|" in lines[index - 1] and "---" in delimiter and set(delimiter) <= set("|:- \t")):
            continue
        if len([cell for cell in delimiter.strip("|").split("|") if cell.strip()]) < 2:
            continue
        # As in GitHub Markdown, a row needs no pipes; a blank line or a new block such as a
        # heading, quote, code fence or list item ends the table.
        for row in lines[index + 1:]:
            if not row.strip() or row.lstrip().startswith(("#", ">", "```", "~~~")) or LIST_ITEM.match(row):
                break
            if re.search(r"\d", row):
                return True
    return False


def changed_lines(diff):
    """Added and removed lines per file from `git diff -U0` output, without comment-only lines."""
    result = {}
    current = old = None
    in_header = False
    for line in diff.splitlines():
        if line.startswith("diff --git "):
            current = old = None
            in_header = True
        elif in_header and line.startswith("--- "):
            old = line[6:] if line.startswith("--- a/") else None
        elif in_header and line.startswith("+++ "):
            # A deleted file has no new path; its removed lines still count.
            current = line[6:] if line.startswith("+++ b/") else old
        elif line.startswith("@@"):
            in_header = False
        elif current and not in_header and line[:1] in ("+", "-"):
            code = line[1:].split("//", 1)[0].strip()
            if code:
                result.setdefault(current, []).append(code)
    return result


def ui_files(paths, lines_by_file):
    """The changed paths that show up in the interface."""
    found = []
    for path in paths:
        if not path.startswith(SHIPPED) or NOT_SHIPPED.search(path):
            continue
        patterns = [pattern for folder, suffix, group in VIEW_CODE
                    if path.startswith(folder) and path.endswith(suffix) for pattern in group]
        if UI_RESOURCES.search(path) or any(p.search(line) for p in patterns for line in lines_by_file.get(path, ())):
            found.append(path)
    return found


def check(title, body, paths, lines_by_file):
    """Problems as (title, message) pairs, and notes for the summary."""
    problems = []
    notes = []
    text = visible_text(body)

    if not TITLE.match(title or ""):
        problems.append(("Title needs a category", "Start the title with (feature), (bug), (perf) or (chore) and a "
                         "space, for example \"(bug) Keep the indicator on the active screen\"."))

    visible = ui_files(paths, lines_by_file)
    if visible:
        shown = ", ".join(visible[:8]) + (f" and {len(visible) - 8} more" if len(visible) > 8 else "")
        if NO_UI_CHANGE.search(text):
            notes.append(f"Visible files changed ({shown}); the description says there is no visible UI change.")
        elif has_media(text):
            notes.append(f"Visible files changed ({shown}); the description has screenshots or a clip.")
        else:
            problems.append(("Screenshots missing", f"This pull request changes visible files ({shown}). Add "
                             "screenshots of the final interface in English (dark mode), and a short clip for "
                             "changes in motion, or tick \"- [x] No visible UI change\" in the description."))

    if (title or "").startswith("(perf)"):
        if has_table(text):
            notes.append("The (perf) description has a measurement table.")
        else:
            problems.append(("Measurement table missing", "A (perf) pull request needs a Markdown table with the "
                             "measured metric for main and this pull request, and how it was measured."))
    return problems, notes


def git(*arguments):
    return subprocess.run(["git", *arguments], check=True, capture_output=True, text=True).stdout


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="HEAD^1", help="the base commit to diff against (default: HEAD^1)")
    parser.add_argument("--summary", help="append a Markdown summary to this file")
    args = parser.parse_args(argv)

    paths = [path for path in git("diff", "--name-only", "--no-renames", args.base, "HEAD").splitlines() if path]
    lines_by_file = changed_lines(
        git("diff", "-U0", "--no-color", "--no-renames", args.base, "HEAD", "--", "*.rs", "*.js"))
    problems, notes = check(os.environ.get("PR_TITLE", ""), os.environ.get("PR_BODY", ""), paths, lines_by_file)

    for title, message in problems:
        print(f"::error title={title}::{message}")
    for note in notes:
        print(note)
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as summary:
            summary.write("### Pull request description\n\n")
            summary.writelines(f"- **{title}:** {message}\n" for title, message in problems)
            summary.writelines(f"- {note}\n" for note in notes)
            if not problems and not notes:
                summary.write("- Title and description follow the rules.\n")
    if not problems:
        print("The title and description follow the rules in AGENTS.md.")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
