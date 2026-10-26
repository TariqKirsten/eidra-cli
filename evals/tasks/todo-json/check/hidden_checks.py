import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

import todo

A = {"id": 1, "text": "write report", "done": False}
B = {"id": 2, "text": "café ☕", "done": True}
C = {"id": 3, "text": "call the bank", "done": False}


class HiddenTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.file = os.path.join(self.dir.name, "todo.json")

    def tearDown(self):
        self.dir.cleanup()

    def cli(self, *args):
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(io.StringIO()):
            code = todo.main(["--file", self.file, *args])
        return code, out.getvalue()

    def three_items(self):
        for item in (A, B, C):
            self.cli("add", item["text"])
        self.cli("done", "2")

    def test_pending_shows_only_items_not_done(self):
        self.three_items()
        self.assertEqual(self.cli("list", "--pending"), (0, "  1. [ ] write report\n  3. [ ] call the bank\n"))

    def test_json_prints_every_item_and_nothing_else(self):
        self.three_items()
        code, out = self.cli("list", "--json")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out), [A, B, C])

    def test_the_options_combine_in_either_order(self):
        self.three_items()
        self.assertEqual(json.loads(self.cli("list", "--pending", "--json")[1]), [A, C])
        self.assertEqual(json.loads(self.cli("list", "--json", "--pending")[1]), [A, C])

    def test_empty_selections(self):
        self.assertEqual(json.loads(self.cli("list", "--json")[1]), [])
        self.assertEqual(self.cli("list", "--pending"), (0, "Nothing to do.\n"))
        self.cli("add", "only")
        self.cli("done", "1")
        self.assertEqual(self.cli("list", "--pending"), (0, "Nothing to do.\n"))
        self.assertEqual(json.loads(self.cli("list", "--pending", "--json")[1]), [])

    def test_the_plain_listing_is_unchanged(self):
        self.three_items()
        self.assertEqual(self.cli("list"), (0, "  1. [ ] write report\n  2. [x] café ☕\n  3. [ ] call the bank\n"))
