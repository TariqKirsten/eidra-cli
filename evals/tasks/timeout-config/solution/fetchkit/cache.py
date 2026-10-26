"""A small cache of responses, so that repeated GETs in a short time cost nothing."""

import time

# Seconds a cached response stays fresh.
DEFAULT_TTL = 300
# Responses kept at most; the oldest goes first.
MAX_ENTRIES = 30


class ResponseCache:
    def __init__(self, ttl=DEFAULT_TTL, max_entries=MAX_ENTRIES, clock=time.monotonic):
        self.ttl = ttl
        self.max_entries = max_entries
        self.clock = clock
        self._entries = {}

    def get(self, url):
        entry = self._entries.get(url)
        if entry is None:
            return None
        stored, response = entry
        if self.clock() - stored > self.ttl:
            del self._entries[url]
            return None
        return response

    def put(self, url, response):
        if len(self._entries) >= self.max_entries and url not in self._entries:
            oldest = min(self._entries, key=lambda key: self._entries[key][0])
            del self._entries[oldest]
        self._entries[url] = (self.clock(), response)
