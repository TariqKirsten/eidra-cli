import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

import todo


class TodoTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.file = os.path.join(self.dir.name, "todo.json")

    def tearDown(self):
        self.dir.cleanup()

    def run_cli(self, *args):
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(io.StringIO()):
            code = todo.main(["--file", self.file, *args])
        return code, out.getvalue()

    def test_add_and_list(self):
        self.assertEqual(self.run_cli("add", "buy milk"), (0, "Added 1.\n"))
        self.run_cli("add", "call mum")
        self.assertEqual(self.run_cli("list"), (0, "  1. [ ] buy milk\n  2. [ ] call mum\n"))

    def test_done(self):
        self.run_cli("add", "buy milk")
        self.assertEqual(self.run_cli("done", "1"), (0, "Done 1.\n"))
        self.assertEqual(self.run_cli("list"), (0, "  1. [x] buy milk\n"))

    def test_done_with_an_unknown_id(self):
        self.assertEqual(self.run_cli("done", "9"), (1, ""))

    def test_an_empty_list(self):
        self.assertEqual(self.run_cli("list"), (0, "Nothing to do.\n"))

    def test_items_are_saved_as_json(self):
        self.run_cli("add", "buy milk")
        with open(self.file, encoding="utf-8") as f:
            self.assertEqual(json.load(f), [{"id": 1, "text": "buy milk", "done": False}])
