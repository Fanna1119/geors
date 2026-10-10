#!/usr/bin/env python3
"""Ambiguous place names across countries (e.g. Germany and Austria).

Usage: bench/ambiguity.py BASE_URL A.jsonl B.jsonl [N]
  (A/B: `geors export` of two partitions, e.g. de and at)

Picks city-layer names that exist in both exports and checks:
  focus    searching near one instance returns that instance first
  country  with country=XX only that country's places come back
  default  without hints: share of rank-1 answers per country
"""
import collections, json, math, random, sys, urllib.parse, urllib.request

random.seed(3)
base, fa, fb = sys.argv[1:4]
n = int(sys.argv[4]) if len(sys.argv) > 4 else 200

def cities(path):
    out = collections.defaultdict(list)
    for line in open(path, encoding="utf-8"):
        p = json.loads(line)
        if p["layer"] == "city" and p.get("name"):
            out[p["name"]].append(p)
    return out

A, B = cities(fa), cities(fb)
shared = sorted(set(A) & set(B))
print(f"city names: {len(A)} vs {len(B)}, shared: {len(shared)}  e.g. {', '.join(random.sample(shared, min(8, len(shared))))}")

def search(q, **kw):
    url = f"{base}/search?" + urllib.parse.urlencode({"q": q, "limit": 10, **kw})
    return json.load(urllib.request.urlopen(url))["features"]

def dist(a, b):
    la1, la2 = math.radians(a[1]), math.radians(b[1])
    h = math.sin((la2 - la1) / 2) ** 2 + math.cos(la1) * math.cos(la2) * math.sin(math.radians(b[0] - a[0]) / 2) ** 2
    return 12742000 * math.asin(math.sqrt(h))

stats = collections.Counter()
default_cc = collections.Counter()
for name in random.sample(shared, min(n, len(shared))):
    for src in (A, B):
        target = random.choice(src[name])
        c = (target["center"]["lon"], target["center"]["lat"])
        fs = search(name, lat=c[1] + 0.02, lon=c[0])  # ~2 km away
        ok = fs and fs[0]["properties"].get("name") == name and dist(fs[0]["geometry"]["coordinates"], c) < 3000
        stats["focus ok"] += bool(ok)
        stats["focus total"] += 1
        cc = target["country_code"]
        fs = search(name, country=cc)
        stats["country ok"] += all(f["properties"]["countrycode"] == cc for f in fs) and bool(fs)
        stats["country total"] += 1
    fs = search(name)
    if fs:
        default_cc[fs[0]["properties"]["countrycode"]] += 1

print(f"focus    {100 * stats['focus ok'] / stats['focus total']:5.1f}%  near-instance first")
print(f"country  {100 * stats['country ok'] / stats['country total']:5.1f}%  filter respected")
print(f"default  rank-1 by country: {dict(default_cc)}")
