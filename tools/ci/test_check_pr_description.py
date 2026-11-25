#!/usr/bin/env python3

import unittest

from check_pr_description import changed_lines, check, has_table, ui_files, visible_text

TEMPLATE = """## Summary

Fixes the thing.

## Screenshots

<!-- Required for visible changes. ![example](https://example.com/x.png) -->

- [ ] No visible UI change

## Test plan

- [x] `cargo test --workspace`
"""

DIFF = """diff --git a/crates/typewhisper/src/shell_bridge.rs b/crates/typewhisper/src/shell_bridge.rs
index 1111111..2222222 100644
--- a/crates/typewhisper/src/shell_bridge.rs
+++ b/crates/typewhisper/src/shell_bridge.rs
@@ -10 +10 @@ impl ShellBridge {
-        let name = "com.typewhisper.Old";
+        let name = "com.typewhisper.New";
diff --git a/crates/typewhisper/src/app.rs b/crates/typewhisper/src/app.rs
index 3333333..4444444 100644
--- a/crates/typewhisper/src/app.rs
+++ b/crates/typewhisper/src/app.rs
@@ -80,0 +81,2 @@ fn build_ui(application: &adw::Application) -> Ui {
+    row.set_subtitle("Hold the key and speak");
+    // a comment that mentions gtk::Label
diff --git a/crates/tw-platform/src/insert.rs b/crates/tw-platform/src/insert.rs
index 5555555..6666666 100644
--- a/crates/tw-platform/src/insert.rs
+++ b/crates/tw-platform/src/insert.rs
@@ -20 +20 @@ pub fn insert(text: &str) {
-    clipboard.set_text(text);
+    clipboard.set_text(text.trim());
diff --git a/crates/typewhisper/src/old_badge.rs b/crates/typewhisper/src/old_badge.rs
deleted file mode 100644
index 7777777..0000000
--- a/crates/typewhisper/src/old_badge.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn badge() -> gtk::Label {
---counter
-}
diff --git a/gnome-shell-extension/typewhisper@typewhisper.com/extension.js b/gnome-shell-extension/typewhisper@typewhisper.com/extension.js
index 8888888..9999999 100644
--- a/gnome-shell-extension/typewhisper@typewhisper.com/extension.js
+++ b/gnome-shell-extension/typewhisper@typewhisper.com/extension.js
@@ -47 +47 @@ export default class TypeWhisperExtension extends Extension {
-        this._timer = new St.Label({style_class: 'tw-timer'});
+        this._timer = new St.Label({style_class: 'tw-timer', x_expand: true});
"""


class TitleTests(unittest.TestCase):
    def problems(self, title):
        return [name for name, _ in check(title, TEMPLATE, [], {})[0]]

    def test_categories(self):
        for title in ("(feature) Add a thing", "(bug) Fix a thing", "(perf) Speed up a thing", "(chore) Bump X"):
            self.assertNotIn("Title needs a category", self.problems(title), title)
        for title in ("Add a thing", "(Feature) Add a thing", "(fix) Fix a thing", "(bug)Fix a thing", "feat: thing"):
            self.assertIn("Title needs a category", self.problems(title), title)


class UIFileTests(unittest.TestCase):
    def test_paths(self):
        lines = changed_lines(DIFF)
        paths = [
            "crates/typewhisper/src/style.css",
            "crates/typewhisper/data/window.ui",
            "packaging/com.typewhisper.TypeWhisper.svg",
            "gnome-shell-extension/typewhisper@typewhisper.com/stylesheet.css",
            "crates/typewhisper/po/de.po",
            "crates/typewhisper/src/shell_bridge.rs",
            "crates/typewhisper/src/app.rs",
            "crates/tw-platform/src/insert.rs",
            "crates/typewhisper/src/old_badge.rs",
            "gnome-shell-extension/typewhisper@typewhisper.com/extension.js",
            "crates/tw-formats/tests/fixtures/icon.svg",
            "docs/overview.svg",
            "tools/ci/check_pr_description.py",
            "packaging/flatpak/com.typewhisper.TypeWhisper.json",
        ]
        self.assertEqual(ui_files(paths, lines), [
            "crates/typewhisper/src/style.css",
            "crates/typewhisper/data/window.ui",
            "packaging/com.typewhisper.TypeWhisper.svg",
            "gnome-shell-extension/typewhisper@typewhisper.com/stylesheet.css",
            "crates/typewhisper/po/de.po",
            "crates/typewhisper/src/app.rs",
            "crates/typewhisper/src/old_badge.rs",
            "gnome-shell-extension/typewhisper@typewhisper.com/extension.js",
        ])

    def test_changed_lines_skip_comments_and_keep_deleted_files(self):
        lines = changed_lines(DIFF)
        self.assertEqual(lines["crates/typewhisper/src/app.rs"], ['row.set_subtitle("Hold the key and speak");'])
        self.assertEqual(lines["crates/typewhisper/src/old_badge.rs"],
                         ["fn badge() -> gtk::Label {", "--counter", "}"])

    def test_wrapped_string_argument_is_visible(self):
        # rustfmt puts a long text on its own line; the diff then holds only the string.
        app = "crates/typewhisper/src/app.rs"
        self.assertEqual(ui_files([app], {app: ['"Hold the key and speak",']}), [app])
        self.assertEqual(ui_files([app], {app: ['"Listening");']}), [app])
        self.assertEqual(ui_files([app], {app: ['let name = "com.typewhisper.TypeWhisper";']}), [])
        platform = "crates/tw-platform/src/insert.rs"
        self.assertEqual(ui_files([platform], {platform: ['"Hold the key and speak",']}), [])

    def test_extension_code_without_widgets_is_not_visible(self):
        path = "gnome-shell-extension/typewhisper@typewhisper.com/extension.js"
        self.assertEqual(ui_files([path], {path: ["this._focus = global.display.focus_window;"]}), [])


class DescriptionTests(unittest.TestCase):
    VIEW = ["crates/typewhisper/src/style.css"]

    def problems(self, title, body, paths=()):
        return [name for name, _ in check(title, body, list(paths), {})[0]]

    def test_visible_change_needs_media(self):
        self.assertEqual(self.problems("(feature) New settings", TEMPLATE, self.VIEW), ["Screenshots missing"])

    def test_media_kinds(self):
        for media in ("![Settings](https://github.com/user-attachments/assets/0f6c1d4e-aaaa-bbbb-cccc-ddddeeeeffff)",
                      '<img width="600" alt="Settings" src="https://github.com/user-attachments/assets/abc">',
                      "https://github.com/user-attachments/assets/0f6c1d4e-aaaa-bbbb-cccc-ddddeeeeffff",
                      "<video src=\"https://example.com/clip.mp4\"></video>"):
            body = TEMPLATE.replace("Fixes the thing.", f"Fixes the thing.\n\n{media}")
            self.assertEqual(self.problems("(feature) New settings", body, self.VIEW), [], media)

    def test_opt_out(self):
        body = TEMPLATE.replace("- [ ] No visible UI change", "- [x] No visible UI change")
        table = "\n| Metric | main | PR |\n| --- | --- | --- |\n| Rebuilt rows | 300 | 1 |\n"
        self.assertEqual(self.problems("(perf) Faster rows", body + table, self.VIEW), [])

    def test_comments_do_not_count(self):
        body = TEMPLATE + "\n<!-- - [x] No visible UI change -->\n"
        self.assertNotIn("https://example.com/x.png", visible_text(body))
        self.assertEqual(self.problems("(perf) Faster rows", body, self.VIEW),
                         ["Screenshots missing", "Measurement table missing"])

    def test_no_visible_change_needs_nothing(self):
        self.assertEqual(self.problems("(bug) Fix a race", TEMPLATE, ["crates/tw-plugin-host/src/lib.rs"]), [])

    def test_perf_needs_a_table(self):
        self.assertEqual(self.problems("(perf) Faster launch", TEMPLATE), ["Measurement table missing"])
        body = TEMPLATE + "\n| Metric | main | PR |\n|:---|---:|---:|\n| CPU | 10 ms | 5 ms |\n"
        self.assertEqual(self.problems("(perf) Faster launch", body), [])

    def test_table_detection(self):
        self.assertTrue(has_table("| Metric | main | PR |\n| --- | --- | --- |\n| CPU | 10 ms | 5 ms |"))
        self.assertTrue(has_table("Metric | main\n--- | ---\nCPU | 10 ms"))
        # A row without pipes still belongs to the table; a blank line ends it.
        self.assertTrue(has_table("| Metric | main | PR |\n| --- | --- | --- |\nWarmup\nCPU | 10 ms | 8 ms"))
        self.assertFalse(has_table("| Metric | main | PR |\n| --- | --- | --- |\n\nWe saved 10 ms."))
        # A header without measurements, a single column, or a lone delimiter is no measurement table.
        self.assertFalse(has_table("| Metric | main | PR |\n| --- | --- | --- |"))
        self.assertFalse(has_table("| Metric | main | PR |\n| --- | --- | --- |\n| CPU | faster | slower |"))
        self.assertFalse(has_table("| CPU |\n| --- |\n| 10 ms |"))
        # A list item starts a new block, so its numbers are outside the table.
        self.assertFalse(has_table("| a | b |\n| --- | --- |\n- Measured on 10 ms"))
        self.assertFalse(has_table("| a | b |\n| --- | --- |\n2. Ran 10 launches"))
        self.assertFalse(has_table("a | b\n\n| --- |"))
        self.assertFalse(has_table("Some text\n---\nMore text"))


if __name__ == "__main__":
    unittest.main()
