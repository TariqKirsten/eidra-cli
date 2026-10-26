import unittest

from fetchkit import Client, FetchError, RequestTimeout
from fetchkit.transport import MemoryTransport


class ClientTest(unittest.TestCase):
    def test_get(self):
        transport = MemoryTransport({"http://x/items": (200, "[]", 0.1)})
        self.assertEqual(Client("http://x", transport=transport).get("/items"), "[]")

    def test_a_response_slower_than_the_default_timeout_times_out(self):
        transport = MemoryTransport({"http://x/slow": (200, "ok", 50.0)})
        with self.assertRaises(RequestTimeout):
            Client("http://x", transport=transport, retries=0).get("/slow")

    def test_timeouts_are_retried(self):
        transport = MemoryTransport({"http://x/slow": (200, "", 99.0)})
        with self.assertRaises(RequestTimeout):
            Client("http://x", transport=transport, retries=1).get("/slow")
        self.assertEqual(len(transport.sent), 2)

    def test_a_per_request_timeout(self):
        transport = MemoryTransport({"http://x/slow": (200, "ok", 70.0)})
        self.assertEqual(Client("http://x", transport=transport).get("/slow", timeout=90), "ok")

    def test_client_errors_are_not_retried(self):
        transport = MemoryTransport()
        with self.assertRaises(FetchError):
            Client("http://x", transport=transport).get("/missing")
        self.assertEqual(len(transport.sent), 1)

    def test_responses_are_cached(self):
        transport = MemoryTransport({"http://x/items": (200, "[1]", 0.1)})
        client = Client("http://x", transport=transport)
        client.get("/items")
        client.get("/items")
        self.assertEqual(len(transport.sent), 1)
