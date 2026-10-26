import inspect
import tomllib
import unittest

from fetchkit import Client, RequestOptions, config, settings
from fetchkit.backoff import delays
from fetchkit.cache import DEFAULT_TTL, MAX_ENTRIES, ResponseCache
from fetchkit.cli import build_parser
from fetchkit.transport import MemoryTransport


def option_help(name):
    return next(action.help for action in build_parser()._actions if name in action.option_strings)


class RequestTimeoutTest(unittest.TestCase):
    def test_the_default_is_45_seconds_everywhere(self):
        self.assertEqual(settings.DEFAULT_TIMEOUT, 45)
        self.assertEqual(Client("http://x").timeout, 45)
        self.assertEqual(RequestOptions().timeout, 45)
        self.assertEqual(config.load()["request"]["timeout"], 45)
        with open("config/default.toml", "rb") as f:
            self.assertEqual(tomllib.load(f)["request"]["timeout"], 45)
        self.assertEqual(build_parser().parse_args(["http://x"]).timeout, 45)

    def test_what_describes_it_says_45(self):
        self.assertIn("45", option_help("--timeout"))
        self.assertNotIn("30", option_help("--timeout"))
        self.assertIn("45 seconds", Client.get.__doc__)
        self.assertNotIn("30 seconds", Client.get.__doc__)

    def test_a_40_second_response_now_arrives(self):
        transport = MemoryTransport({"http://x/slow": (200, "ok", 40.0)})
        self.assertEqual(Client("http://x", transport=transport, retries=0).get("/slow"), "ok")


class EverythingElseTest(unittest.TestCase):
    def test_other_settings_are_as_they_were(self):
        self.assertEqual((settings.CONNECT_TIMEOUT, settings.PAGE_SIZE, settings.MAX_RETRIES), (5, 30, 3))
        self.assertEqual(Client("http://x").connect_timeout, 5)
        self.assertEqual((RequestOptions().connect_timeout, RequestOptions().max_items), (5, 30))
        self.assertEqual(inspect.signature(delays).parameters["cap"].default, 30.0)
        self.assertEqual((DEFAULT_TTL, MAX_ENTRIES, ResponseCache().max_entries), (300, 30, 30))
        self.assertIn("(default: 5)", option_help("--connect-timeout"))
        self.assertIn("(default: 30)", option_help("--page-size"))

    def test_the_configuration_is_otherwise_as_it_was(self):
        loaded = config.load()
        with open("config/default.toml", "rb") as f:
            from_file = tomllib.load(f)
        for configuration in (loaded, from_file):
            self.assertEqual(configuration["request"]["connect_timeout"], 5)
            self.assertEqual(configuration["request"]["retries"], 3)
            self.assertEqual(configuration["cache"], {"ttl": 300, "max_entries": 30})
            self.assertEqual(configuration["listing"], {"page_size": 30})
