"""lru-cache: get/put, least-recently-used eviction (get counts as use), updates in place."""
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, ".")
try:
    from lru import LRUCache
except Exception as e:  # noqa: BLE001
    sys.exit(f"can't import LRUCache from lru.py: {e}")

c = LRUCache(2)
c.put("a", 1)
c.put("b", 2)
if c.get("a") != 1:
    sys.exit("get('a') after put")
c.put("c", 3)  # evicts b: a was used more recently
if c.get("b") is not None:
    sys.exit("'b' should have been evicted (least recently used; 'a' was read after it)")
if c.get("a") != 1 or c.get("c") != 3:
    sys.exit("'a' and 'c' should be there")
c.put("a", 10)  # update, no eviction
if c.get("c") != 3 or c.get("a") != 10:
    sys.exit("put on an existing key must update it without evicting")
c.put("d", 4)  # evicts c (a was touched last)
if c.get("c") is not None or c.get("a") != 10 or c.get("d") != 4:
    sys.exit("after put('d'), 'c' should be evicted and 'a', 'd' kept")
if c.get("missing") is not None:
    sys.exit("a missing key returns None")
big = LRUCache(1000)
for i in range(100000):
    big.put(i, i)
    big.get(i - 500)
if big.get(99999) != 99999 or big.get(0) is not None:
    sys.exit("wrong contents after 100k operations")
one = LRUCache(1)
one.put(1, 1)
one.put(2, 2)
if one.get(1) is not None or one.get(2) != 2:
    sys.exit("capacity 1")
print("ok")
