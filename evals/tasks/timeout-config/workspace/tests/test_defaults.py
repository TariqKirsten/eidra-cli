import unittest

from fetchkit import Client, RequestOptions, config, settings
from fetchkit.backoff import delays
from fetchkit.cli import build_parser


class DefaultsTest(unittest.TestCase):
    def test_request_timeout(self):
        self.assertEqual(settings.DEFAULT_TIMEOUT, 30.0)
        self.assertEqual(Client("http://x").timeout, 30.0)
        self.assertEqual(RequestOptions().timeout, 30)
        self.assertEqual(build_parser().parse_args(["http://x"]).timeout, 30.0)

    def test_connect_timeout(self):
        self.assertEqual(settings.CONNECT_TIMEOUT, 5.0)
        self.assertEqual(Client("http://x").connect_timeout, 5.0)

    def test_configuration_defaults(self):
        loaded = config.load()
        self.assertEqual(loaded["request"]["timeout"], 30)
        self.assertEqual(loaded["listing"]["page_size"], 30)

    def test_backoff(self):
        self.assertEqual(delays(3), [0.5, 1.0, 2.0])
        self.assertEqual(delays(8)[-1], 30.0)
