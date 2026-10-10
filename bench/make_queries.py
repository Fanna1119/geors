#!/usr/bin/env python3
"""Generate benchmark query mixes from `geors export` output.

Usage: make_queries.py OUT_DIR [EXPORT.jsonl ...]   (no files: read stdin)

Writes one file per mix (one URL path per line) into OUT_DIR:
  full     street + housenumber + city, POI names, city names
  prefix   1-6 character prefixes (search-as-you-type)
  typo     names with one edit (swap, drop or replace a letter)
  bias     full queries with a focus point (lat/lon) near the target
  radius   category words within 2 km of a random place
  reverse  points within ~200 m of a random place
  nearest  random points, 10 nearest places
"""
import json, os, random, sys
from urllib.parse import quote

random.seed(42)
N = 2000
out_dir, exports = sys.argv[1], sys.argv[2:]
os.makedirs(out_dir, exist_ok=True)

houses, pois, cities, streets, points = [], [], [], [], []
# Reservoir-sample so huge exports do not need to fit in memory.
def sample(lst, item, seen, k=20000):
    if len(lst) < k:
        lst.append(item)
    else:
        j = random.randrange(seen)
        if j < k:
            lst[j] = item

counts = {}
def lines():
    if not exports:
        yield from sys.stdin
    for path in exports:
        yield from open(path, encoding="utf-8")

if True:
    for line in lines():
        p = json.loads(line)
        layer = p["layer"]
        counts[layer] = counts.get(layer, 0) + 1
        seen = counts[layer]
        c = p["center"]
        city = next((x.split(":", 1)[1] for x in p.get("parents", []) if x.startswith("city:")), None)
        if layer == "house" and p.get("street") and p.get("housenumber") and city:
            sample(houses, (f'{p["street"]} {p["housenumber"]} {city}', c), seen)
        elif layer == "poi" and p.get("name"):
            sample(pois, (p["name"], c), seen)
        elif layer == "city" and p.get("name"):
            sample(cities, (p["name"], c), seen)
        elif layer == "street" and p.get("name"):
            sample(streets, (p["name"], c), seen)
        sample(points, c, sum(counts.values()), k=50000)

def q(text, extra=""):
    return f"/search?q={quote(text)}{extra}"

def pick(lst, n):
    return [random.choice(lst) for _ in range(n)] if lst else []

def typo(s):
    words = s.split()
    i = max(range(len(words)), key=lambda k: len(words[k]))
    w = words[i]
    if len(w) < 5:
        return s
    k = random.randrange(1, len(w) - 1)
    op = random.choice("swap drop replace".split())
    if op == "swap":
        w = w[:k] + w[k + 1] + w[k] + w[k + 2:]
    elif op == "drop":
        w = w[:k] + w[k + 1:]
    else:
        w = w[:k] + random.choice("aeiourstn") + w[k + 1:]
    words[i] = w
    return " ".join(words)

def jitter(c, deg):
    return c["lat"] + random.uniform(-deg, deg), c["lon"] + random.uniform(-deg, deg)

names = houses + pois + cities + streets
mixes = {
    "full": [q(t) for t, _ in pick(houses, N // 2) + pick(pois, N // 4) + pick(cities, N // 4)],
    "prefix": [q(t[: random.randint(1, 6)]) for t, _ in pick(cities + streets + pois, N)],
    "typo": [q(typo(t)) for t, _ in pick(houses + pois + cities, N)],
    "bias": [
        q(t, "&lat=%.5f&lon=%.5f" % jitter(c, 0.05))
        for t, c in pick(pois + streets, N)
    ],
    "radius": [
        q(w, "&lat=%.5f&lon=%.5f&radius=2000" % (c["lat"], c["lon"]))
        for w, c in zip(
            random.choices(["restaurant", "apotheke", "schule", "bäckerei", "kirche", "hotel", "bank", "café"], k=N),
            pick(points, N),
        )
    ],
    "reverse": ["/reverse?lat=%.5f&lon=%.5f" % jitter(c, 0.002) for c in pick(points, N)],
    "nearest": ["/nearest?lat=%.5f&lon=%.5f&limit=10" % jitter(c, 0.05) for c in pick(points, N)],
}
for name, paths in mixes.items():
    with open(os.path.join(out_dir, f"{name}.txt"), "w") as f:
        f.write("\n".join(paths) + "\n")
print("places:", counts)
print("mixes:", {k: len(v) for k, v in mixes.items()})
