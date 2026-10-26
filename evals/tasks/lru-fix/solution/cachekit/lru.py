"""A least-recently-used cache."""


class _Node:
    __slots__ = ("key", "value", "prev", "next")

    def __init__(self, key=None, value=None):
        self.key = key
        self.value = value
        self.prev = None
        self.next = None


class LRUCache:
    """A mapping that holds at most ``capacity`` items.

    ``get`` and ``put`` both count as using an item: afterwards it is the most recently used one.
    When ``put`` adds a new key to a full cache, the least recently used item is evicted to make
    room. ``keys()`` lists the keys from the least to the most recently used. ``len()``, ``in`` and
    ``keys()`` do not count as using anything.
    """

    def __init__(self, capacity):
        if capacity < 1:
            raise ValueError("capacity must be at least 1")
        self.capacity = capacity
        self._nodes = {}
        # Sentinels: the node after _head is the least recently used, the one before _tail the most.
        self._head = _Node()
        self._tail = _Node()
        self._head.next = self._tail
        self._tail.prev = self._head

    def __len__(self):
        return len(self._nodes)

    def __contains__(self, key):
        return key in self._nodes

    def get(self, key, default=None):
        node = self._nodes.get(key)
        if node is None:
            return default
        self._unlink(node)
        self._append(node)
        return node.value

    def put(self, key, value):
        node = self._nodes.get(key)
        if node is not None:
            node.value = value
            self._unlink(node)
            self._append(node)
            return
        if len(self._nodes) >= self.capacity:
            self._evict()
        node = _Node(key, value)
        self._nodes[key] = node
        self._append(node)

    def delete(self, key):
        """Removes ``key``; raises KeyError if it is not there."""
        node = self._nodes.pop(key)
        self._unlink(node)

    def keys(self):
        keys = []
        node = self._head.next
        while node is not self._tail:
            keys.append(node.key)
            node = node.next
        return keys

    def _append(self, node):
        last = self._tail.prev
        last.next = node
        node.prev = last
        node.next = self._tail
        self._tail.prev = node

    def _unlink(self, node):
        node.prev.next = node.next
        node.next.prev = node.prev
        node.prev = node.next = None

    def _evict(self):
        oldest = self._head.next
        self._unlink(oldest)
        del self._nodes[oldest.key]
