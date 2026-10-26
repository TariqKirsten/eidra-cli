# The configuration file

fetchkit reads a TOML file. config/default.toml has every key, with its default.

| key | meaning | default |
|---|---|---|
| `request.timeout` | seconds the whole request may take | 45 |
| `request.connect_timeout` | seconds to wait for the connection | 5 |
| `request.retries` | retries after the first attempt | 3 |
| `cache.ttl` | seconds a cached response stays fresh | 300 |
| `cache.max_entries` | responses kept at most | 30 |
| `listing.page_size` | items per page | 30 |
