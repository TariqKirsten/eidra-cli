import unittest

from inventory.model import Item
from inventory.report import build_report, days_left, margin, needs_reorder


def item(sku="X-1", stock=10, reorder_at=5, daily_use=1.0, lead_days=2):
    return Item(sku=sku, name=sku.lower(), stock=stock, reorder_at=reorder_at, daily_use=daily_use, lead_days=lead_days)


class ReportTest(unittest.TestCase):
    def test_reorder_at_or_below_the_level(self):
        self.assertTrue(needs_reorder(item(stock=5, reorder_at=5)))
        self.assertTrue(needs_reorder(item(stock=0, reorder_at=5)))
        self.assertFalse(needs_reorder(item(stock=6, reorder_at=5)))

    def test_days_left(self):
        self.assertEqual(days_left(item(stock=10, daily_use=2)), 5)
        self.assertIsNone(days_left(item(daily_use=0)))

    def test_margin_is_negative_when_the_reorder_arrives_late(self):
        self.assertEqual(margin(item(stock=4, daily_use=1, lead_days=6)), -2)

    def test_most_urgent_first(self):
        lines = build_report([
            item("A-1", stock=4, reorder_at=10, daily_use=1, lead_days=2),
            item("B-1", stock=1, reorder_at=10, daily_use=1, lead_days=2),
            item("C-1", stock=50, reorder_at=10),
        ])
        self.assertTrue(lines[0].startswith("B-1"))
        self.assertTrue(lines[1].startswith("A-1"))
        self.assertEqual(lines[-1], "2 of 3 items to reorder")
