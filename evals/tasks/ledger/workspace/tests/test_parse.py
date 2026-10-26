import datetime
import unittest

from ledger.models import Expense
from ledger.parse import parse_amount, parse_expenses

D = datetime.date


class ParseAmount(unittest.TestCase):
    def test_valid_forms(self):
        cases = {"12": 1200, "12.5": 1250, "12.50": 1250, "$12.50": 1250, "1,234.56": 123456,
                 "$1,234.56": 123456, "-3": -300, "-$3.00": -300, " 7.05 ": 705, "0.07": 7,
                 "1,000,000": 100000000}
        for text, cents in cases.items():
            with self.subTest(text=text):
                self.assertEqual(parse_amount(text), cents)

    def test_exact_without_float_rounding(self):
        for whole in range(0, 3):
            for frac in range(100):
                text = f"{whole}.{frac:02d}"
                self.assertEqual(parse_amount(text), whole * 100 + frac, text)

    def test_invalid_forms(self):
        for text in ["", "   ", "abc", "1.234", "12.3.4", "$", "--3", "3-", "$-3", ".5", "1.", "12a"]:
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    parse_amount(text)


class ParseExpenses(unittest.TestCase):
    def test_basic(self):
        text = ("date,amount,category,note\n"
                "2026-01-03,1200.00,Rent,January rent\n"
                "2026-01-05,84.20,groceries,weekly shop\n")
        self.assertEqual(parse_expenses(text), [
            Expense(D(2026, 1, 3), 120000, "rent", "January rent"),
            Expense(D(2026, 1, 5), 8420, "groceries", "weekly shop"),
        ])

    def test_quoted_fields_and_empty_note(self):
        text = ('date,amount,category,note\n'
                '2026-01-03,"1,200.00",rent,"January rent, flat 2"\n'
                '2026-02-06,102.10,groceries,\n')
        self.assertEqual(parse_expenses(text), [
            Expense(D(2026, 1, 3), 120000, "rent", "January rent, flat 2"),
            Expense(D(2026, 2, 6), 10210, "groceries", ""),
        ])

    def test_header_any_order_and_case(self):
        text = "Category,Date,NOTE,Amount\n  Groceries ,2026-03-04, weekly ,95.60\n"
        self.assertEqual(parse_expenses(text), [Expense(D(2026, 3, 4), 9560, "groceries", "weekly")])

    def test_blank_and_comment_lines_skipped(self):
        text = ("\n# exported from bank\n"
                "date,amount,category,note\n"
                "\n"
                "   # February\n"
                "2026-02-09,-15.00,groceries,refund\n"
                "   \n")
        self.assertEqual(parse_expenses(text), [Expense(D(2026, 2, 9), -1500, "groceries", "refund")])

    def test_empty_inputs(self):
        self.assertEqual(parse_expenses(""), [])
        self.assertEqual(parse_expenses("  \n \n"), [])
        self.assertEqual(parse_expenses("date,amount,category,note\n"), [])

    def test_missing_column_names_it(self):
        with self.assertRaises(ValueError) as ctx:
            parse_expenses("date,category,note\n2026-01-01,food,x\n")
        self.assertIn("amount", str(ctx.exception))

    def test_bad_rows_report_line_numbers(self):
        cases = {
            "bad date": ("date,amount,category,note\n2026-01-01,1.00,a,\n2026-13-01,1.00,a,\n", "line 3"),
            "bad amount": ("date,amount,category,note\n2026-01-01,1.2.3,a,\n", "line 2"),
            "empty category": ("date,amount,category,note\n2026-01-01,1.00,a,\n\n2026-01-02,1.00,  ,\n", "line 4"),
            "after comment": ("# c\ndate,amount,category,note\n# c\n2026-01-01,x,a,\n", "line 4"),
        }
        for name, (text, where) in cases.items():
            with self.subTest(name):
                with self.assertRaises(ValueError) as ctx:
                    parse_expenses(text)
                self.assertIn(where, str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
