import datetime
import unittest

from ledger.budget import check_budgets, load_budgets
from ledger.models import BudgetLine, Expense

D = datetime.date


def e(y, m, d, cents, cat):
    return Expense(D(y, m, d), cents, cat, "")


class LoadBudgets(unittest.TestCase):
    def test_values_to_cents(self):
        text = '{"rent": 1200, " Groceries ": 300.5, "coffee": 0.1, "fun": 12.25, "none": 0}'
        self.assertEqual(load_budgets(text),
                         {"rent": 120000, "groceries": 30050, "coffee": 10, "fun": 1225, "none": 0})

    def test_rejects_bad_input(self):
        for text in ['[1, 2]', '"x"', '{"a": "12"}', '{"a": true}', '{"a": -1}', '{"a": 1.234}',
                     '{"a": null}', 'not json']:
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    load_budgets(text)


EXPENSES = [
    e(2026, 1, 3, 120000, "rent"),
    e(2026, 2, 2, 120000, "rent"),
    e(2026, 2, 6, 10210, "groceries"),
    e(2026, 2, 9, -1500, "groceries"),
    e(2026, 2, 14, 6840, "dining"),
    e(2026, 2, 20, 2350, "transport"),
    e(2026, 2, 27, 13175, "utilities"),
    e(2026, 3, 4, 9560, "groceries"),
]
BUDGETS = {"rent": 120000, "groceries": 30000, "dining": 5000, "transport": 4500, "fun": 2550}


class CheckBudgets(unittest.TestCase):
    def test_february(self):
        self.assertEqual(check_budgets(EXPENSES, BUDGETS, "2026-02"), [
            BudgetLine("dining", 5000, 6840, -1840, 136.8, True),
            BudgetLine("rent", 120000, 120000, 0, 100.0, False),
            BudgetLine("transport", 4500, 2350, 2150, 52.2, False),
            BudgetLine("groceries", 30000, 8710, 21290, 29.0, False),
            BudgetLine("fun", 2550, 0, 2550, 0.0, False),
        ])

    def test_month_without_spending(self):
        lines = check_budgets(EXPENSES, {"rent": 100, "a": 50}, "2025-07")
        self.assertEqual(lines, [BudgetLine("a", 50, 0, 50, 0.0, False),
                                 BudgetLine("rent", 100, 0, 100, 0.0, False)])

    def test_zero_budget(self):
        lines = check_budgets(EXPENSES, {"dining": 0}, "2026-02")
        self.assertEqual(lines, [BudgetLine("dining", 0, 6840, -6840, 0.0, True)])

    def test_bad_month(self):
        for month in ["2026-2", "2026/02", "2026-13", "Feb", ""]:
            with self.subTest(month=month):
                with self.assertRaises(ValueError):
                    check_budgets(EXPENSES, BUDGETS, month)


if __name__ == "__main__":
    unittest.main()
