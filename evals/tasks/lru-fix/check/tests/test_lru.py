import unittest

from cachekit import LRUCache


class LRUCacheTest(unittest.TestCase):
    def test_put_then_get(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        self.assertEqual(cache.get("a"), 1)

    def test_missing_key_returns_the_default(self):
        cache = LRUCache(2)
        self.assertIsNone(cache.get("x"))
        self.assertEqual(cache.get("x", 0), 0)

    def test_put_replaces_the_value(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("a", 2)
        self.assertEqual(cache.get("a"), 2)
        self.assertEqual(len(cache), 1)

    def test_evicts_the_least_recently_used_when_full(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("c", 3)
        self.assertNotIn("a", cache)
        self.assertEqual(cache.keys(), ["b", "c"])

    def test_get_counts_as_use(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.get("a")
        cache.put("c", 3)
        self.assertIn("a", cache)
        self.assertNotIn("b", cache)

    def test_len_and_in_do_not_count_as_use(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        self.assertIn("a", cache)
        self.assertEqual(len(cache), 2)
        cache.put("c", 3)
        self.assertNotIn("a", cache)

    def test_delete(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.delete("a")
        self.assertNotIn("a", cache)
        self.assertEqual(cache.keys(), [])
        with self.assertRaises(KeyError):
            cache.delete("a")

    def test_capacity_must_be_positive(self):
        with self.assertRaises(ValueError):
            LRUCache(0)
