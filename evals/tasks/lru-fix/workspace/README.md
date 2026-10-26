# cachekit

Small caches for Python 3, standard library only.

```python
from cachekit import LRUCache

cache = LRUCache(100)
cache.put("user:1", {"name": "Ada"})
cache.get("user:1")
```

Run the tests with `python3 -m unittest -q`.
