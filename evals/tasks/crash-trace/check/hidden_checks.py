import unittest

from inventory.model import Item
from inventory.report import build_report, needs_reorder


def item(sku, stock, reorder_at, daily_use=1.0, lead_days=2):
    return Item(sku=sku, name=sku.lower(), stock=stock, reorder_at=reorder_at, daily_use=daily_use, lead_days=lead_days)


class NeverReorderedTest(unittest.TestCase):
    def test_an_item_without_a_level_is_never_reordered(self):
        self.assertFalse(needs_reorder(item("N-1", stock=0, reorder_at=None)))
        self.assertFalse(needs_reorder(item("N-2", stock=500, reorder_at=None)))

    def test_it_is_left_out_of_the_report_but_counted(self):
        lines = build_report([
            item("A-1", stock=2, reorder_at=10),
            item("N-1", stock=0, reorder_at=None),
            item("N-2", stock=3, reorder_at=None, daily_use=0),
            item("C-1", stock=13, reorder_at=10),
        ])
        self.assertEqual(len(lines), 3, lines)
        self.assertTrue(lines[0].startswith("A-1"))
        self.assertEqual(lines[1], "Coming up: C-1")
        self.assertEqual(lines[2], "1 of 4 items to reorder")
        self.assertNotIn("N-", "\n".join(lines))

    def test_a_report_of_only_such_items(self):
        self.assertEqual(build_report([item("N-1", stock=0, reorder_at=None)]), ["0 of 1 items to reorder"])
