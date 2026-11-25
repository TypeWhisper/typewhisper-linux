"""Tests for update-translations.py: reading Rust literals and PO files."""
import importlib.util
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "update_translations", Path(__file__).with_name("update-translations.py")
)
update = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(update)


def literal(source):
    """Reads the literal in `source`, which starts with its opening quote."""
    return update.rust_string(source, 1)


class RustStringTest(unittest.TestCase):
    def test_line_continuation_drops_the_indentation(self):
        self.assertEqual(literal('"one \\\n         two"'), "one two")

    def test_escapes(self):
        self.assertEqual(literal('"a \\"b\\" c\\n\\u{2026}"'), 'a "b" c\n…')


class PoFileTest(unittest.TestCase):
    def test_rendered_catalog_reads_back(self):
        texts = {'Say "hi"\n': ["a.rs"], "Remove {app}": ["b.rs"], "Untranslated": ["b.rs"]}
        translations = {'Say "hi"\n': '„Hallo“\n', "Remove {app}": "{app} entfernen"}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "de.po"
            path.write_text(update.render(texts, translations, "de"), encoding="utf-8")
            self.assertEqual(update.read_po(path), translations)

    def test_fuzzy_entries_count_as_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "de.po"
            path.write_text('#, fuzzy\nmsgid "Done"\nmsgstr "Fertig"\n', encoding="utf-8")
            self.assertEqual(update.read_po(path), {})


if __name__ == "__main__":
    unittest.main()
