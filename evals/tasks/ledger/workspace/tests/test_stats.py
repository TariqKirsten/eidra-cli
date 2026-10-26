import datetime
import unittest

from ledger.models import Expense
from ledger.stats import monthly_average, top_categories, totals_by_category, totals_by_month

D = datetime.date


def e(y, m, d, cents, cat):
    return Expense(D(y, m, d), cents, cat, "")


DATA = [
    e(2026, 1, 3, 120000, "rent"),
    e(2026, 1, 5, 8420, "groceries"),
    e(2026, 1, 24, 4600, "dining"),
    e(2026, 2, 2, 120000, "rent"),
    e(2026, 2, 6, 10210, "groceries"),
    e(2026, 2, 9, -1500, "groceries"),
    e(2026, 3, 4, 9560, "groceries"),
    e(2026, 3, 9, -500, "books"),
]


class Totals(unittest.TestCase):
    def test_by_category(self):
        result = totals_by_category(DATA)
        self.assertEqual(result, {"books": -500, "dining": 4600, "groceries": 26690, "rent": 240000})
        self.assertEqual(list(result), ["books", "dining", "groceries", "rent"])

    def test_by_month_sorted(self):
        shuffled = [DATA[6], DATA[0], DATA[3], DATA[7], DATA[1]]
        result = totals_by_month(shuffled)
        self.assertEqual(result, {"2026-01": 128420, "2026-02": 120000, "2026-03": 9060})
        self.assertEqual(list(result), ["2026-01", "2026-02", "2026-03"])

    def test_empty(self):
        self.assertEqual(totals_by_category([]), {})
        self.assertEqual(totals_by_month([]), {})


class Top(unittest.TestCase):
    def test_order_limit_and_exclusions(self):
        self.assertEqual(top_categories(DATA), [("rent", 240000), ("groceries", 26690), ("dining", 4600)])
        self.assertEqual(top_categories(DATA, n=2), [("rent", 240000), ("groceries", 26690)])
        self.assertEqual(top_categories(DATA, n=10), [("rent", 240000), ("groceries", 26690), ("dining", 4600)])

    def test_ties_by_name(self):
        data = [e(2026, 1, 1, 500, "zoo"), e(2026, 1, 1, 500, "art"), e(2026, 1, 1, 0, "free")]
        self.assertEqual(top_categories(data, n=5), [("art", 500), ("zoo", 500)])


class Average(unittest.TestCase):
    def test_counts_gap_months(self):
        data = [e(2026, 1, 15, 5000, "a"), e(2026, 3, 1, 5001, "a")]
        self.assertEqual(monthly_average(data), 3334)  # 10001 / 3 = 3333.67

    def test_half_rounds_up(self):
        data = [e(2025, 12, 31, 1, "a"), e(2026, 1, 1, 2, "a")]
        self.assertEqual(monthly_average(data), 2)  # 3 / 2 = 1.5

    def test_single_month_and_empty(self):
        self.assertEqual(monthly_average([e(2026, 5, 1, 999, "a"), e(2026, 5, 30, 1, "b")]), 1000)
        self.assertEqual(monthly_average([]), 0)

    def test_full_dataset(self):
        self.assertEqual(monthly_average(DATA), 90263)  # 270790 / 3 = 90263.33


if __name__ == "__main__":
    unittest.main()
