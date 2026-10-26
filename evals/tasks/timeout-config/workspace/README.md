# fetchkit

A small HTTP client with timeouts, retries, paging and caching.

```python
from fetchkit import Client

client = Client("https://api.example.com")
client.get("/items")
```

## Timeouts

A request may take 30 seconds in all, from connecting to the last byte, and 5 seconds to connect.
Pass `timeout=` to `Client` or to `get` to change the first, and `connect_timeout=` to `Client`
for the second.

## Retries

Timeouts and server errors are retried 3 times, waiting 0.5 s, 1 s and 2 s. The waits double each
time and never exceed 30 s.

## Paging and caching

`fetchkit.pagination.collect` follows paged listings, 30 items to a page. Responses are cached for
300 seconds, at most 30 of them.

See docs/usage.md for the command line, and docs/configuration.md for the configuration file.
