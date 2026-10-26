import unittest

from ledger.fmt import format_money, render_table


class FormatMoney(unittest.TestCase):
    def test_values(self):
        cases = {0: "$0.00", 7: "$0.07", 50: "$0.50", 123450: "$1,234.50", -300: "-$3.00",
                 100000000: "$1,000,000.00", -123456789: "-$1,234,567.89", 99999: "$999.99"}
        for cents, text in cases.items():
            with self.subTest(cents=cents):
                self.assertEqual(format_money(cents), text)


class RenderTable(unittest.TestCase):
    def test_mixed_alignment(self):
        out = render_table(["Category", "Spent"], [["groceries", "$123.45"], ["rent", "$1,200.00"]], "lr")
        self.assertEqual(out, "Category       Spent\n"
                              "---------  ---------\n"
                              "groceries    $123.45\n"
                              "rent       $1,200.00")

    def test_default_left_and_trailing_spaces_removed(self):
        out = render_table(["A", "Longer header"], [["xyz", "1"], ["", ""]])
        self.assertEqual(out, "A    Longer header\n"
                              "---  -------------\n"
                              "xyz  1\n"
                              "")

    def test_header_only(self):
        self.assertEqual(render_table(["Month", "Total"], [], "lr"), "Month  Total\n-----  -----")

    def test_errors(self):
        with self.assertRaises(ValueError):
            render_table(["a", "b"], [["1"]])
        with self.assertRaises(ValueError):
            render_table(["a", "b"], [["1", "2"]], "l")
        with self.assertRaises(ValueError):
            render_table(["a", "b"], [["1", "2"]], "lc")


if __name__ == "__main__":
    unittest.main()
