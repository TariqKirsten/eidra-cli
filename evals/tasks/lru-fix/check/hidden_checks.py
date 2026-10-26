import random
import unittest
from collections import OrderedDict

from cachekit import LRUCache


class Model:
    """What the docstring describes, on an OrderedDict."""

    def __init__(self, capacity):
        self.capacity = capacity
        self.items = OrderedDict()

    def get(self, key, default=None):
        if key not in self.items:
            return default
        self.items.move_to_end(key)
        return self.items[key]

    def put(self, key, value):
        if key in self.items:
            self.items.move_to_end(key)
        elif len(self.items) >= self.capacity:
            self.items.popitem(last=False)
        self.items[key] = value

    def delete(self, key):
        del self.items[key]

    def keys(self):
        return list(self.items)


class HiddenTest(unittest.TestCase):
    def test_put_on_an_existing_key_counts_as_use(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("a", 10)
        cache.put("c", 3)
        self.assertEqual(cache.keys(), ["a", "c"])
        self.assertEqual(cache.get("a"), 10)

    def test_get_makes_the_key_most_recent(self):
        cache = LRUCache(3)
        for key in "abc":
            cache.put(key, key)
        cache.get("a")
        self.assertEqual(cache.keys(), ["b", "c", "a"])

    def test_updates_keep_every_item(self):
        cache = LRUCache(3)
        for key in "abc":
            cache.put(key, key)
        cache.put("a", "A")
        cache.put("b", "B")
        self.assertEqual(cache.keys(), ["c", "a", "b"])
        self.assertEqual(len(cache), 3)

    def test_capacity_of_one(self):
        cache = LRUCache(1)
        cache.put("a", 1)
        cache.put("a", 2)
        cache.put("b", 3)
        self.assertEqual(cache.keys(), ["b"])
        self.assertIsNone(cache.get("a"))

    def test_a_long_random_run_matches_the_docstring(self):
        rng = random.Random(7)
        for capacity in (1, 2, 3, 5):
            cache, model = LRUCache(capacity), Model(capacity)
            for step in range(2000):
                key = rng.choice("abcdefg")
                roll = rng.random()
                if roll < 0.45:
                    cache.put(key, step)
                    model.put(key, step)
                elif roll < 0.9:
                    self.assertEqual(cache.get(key), model.get(key))
                elif key in model.items:
                    cache.delete(key)
                    model.delete(key)
                self.assertEqual(cache.keys(), model.keys(), f"capacity {capacity}, step {step}")
                self.assertEqual(len(cache), len(model.items))
