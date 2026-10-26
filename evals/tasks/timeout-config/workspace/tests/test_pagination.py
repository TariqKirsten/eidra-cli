import json
import unittest

from fetchkit import Client
from fetchkit.pagination import collect
from fetchkit.transport import MemoryTransport


def listing(total, size):
    routes = {}
    for page in range(1, total // size + 2):
        start = (page - 1) * size
        items = list(range(start, min(total, start + size)))
        routes[f"http://x/items?page={page}&size={size}"] = (200, json.dumps(items), 0.01)
    return MemoryTransport(routes)


class PaginationTest(unittest.TestCase):
    def test_collects_every_page(self):
        client = Client("http://x", transport=listing(75, 30))
        self.assertEqual(collect(client, "/items"), list(range(75)))

    def test_stops_at_max_items(self):
        client = Client("http://x", transport=listing(75, 30))
        self.assertEqual(collect(client, "/items", max_items=40), list(range(40)))
