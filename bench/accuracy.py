#!/usr/bin/env python3
"""Search quality: is the intended place the first result?

Usage: geors export --data DATA --every 50 CC | bench/accuracy.py [BASE_URL] [N]

For N sampled places per kind it queries the server and checks rank 1:
  addr    "street housenumber city"      -> same house (street + number)
  typo    same with one typo             -> same house
  city    city name                      -> a city with that name
  name    POI name, no location          -> a place with that name (names are
                                            ambiguous without a location)
  poi     POI name, searched from ~1 km away -> that POI (or a same-named one
                                            within 250 m, e.g. the other bus stop)
  prefix  first 60% of a POI name, from ~1 km away -> that POI in the top 5
"""
import json, math, random, sys, urllib.parse, urllib.request

random.seed(7)
base = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:2322"
n = int(sys.argv[2]) if len(sys.argv) > 2 else 300
houses, cities, pois = [], [], []
for line in sys.stdin:
    p = json.loads(line)
    city = next((x.split(":", 1)[1] for x in p.get("parents", []) if x.startswith("city:")), None)
    if p["layer"] == "house" and p.get("street") and p.get("housenumber") and city:
        houses.append((p, city))
    elif p["layer"] == "city" and p.get("name"):
        cities.append(p)
    elif p["layer"] == "poi" and p.get("name") and len(p["name"]) >= 6:
        pois.append(p)

def search(q, **kw):
    url = f"{base}/search?" + urllib.parse.urlencode({"q": q, **kw})
    return json.load(urllib.request.urlopen(url))["features"]

def dist(a, b):
    la1, la2 = math.radians(a[1]), math.radians(b[1])
    h = math.sin((la2 - la1) / 2) ** 2 + math.cos(la1) * math.cos(la2) * math.sin(math.radians(b[0] - a[0]) / 2) ** 2
    return 12742000 * math.asin(math.sqrt(h))

def typo(s):
    words = s.split()
    i = max(range(len(words)), key=lambda k: len(words[k]))
    w = words[i]
    if len(w) >= 5:
        k = random.randrange(1, len(w) - 1)
        w = w[:k] + w[k + 1] + w[k] + w[k + 2:]
    words[i] = w
    return " ".join(words)

def same_house(f, p):
    pr = f["properties"]
    return pr.get("housenumber") == p["housenumber"] and pr.get("street") == p["street"]

results = {}
def check(kind, ok):
    hit, tot = results.get(kind, (0, 0))
    results[kind] = (hit + bool(ok), tot + 1)

for p, city in random.sample(houses, min(n, len(houses))):
    q = f'{p["street"]} {p["housenumber"]} {city}'
    fs = search(q)
    check("addr", fs and same_house(fs[0], p))
    fs = search(typo(q))
    check("typo", fs and same_house(fs[0], p))
for p in random.sample(cities, min(n, len(cities))):
    fs = search(p["name"])
    check("city", fs and fs[0]["properties"]["type"] == "city" and fs[0]["properties"].get("name") == p["name"])
def near(p):
    # A focus point about 1 km away in a random direction.
    a = random.uniform(0, 2 * math.pi)
    lat = p["center"]["lat"] + 0.009 * math.sin(a)
    lon = p["center"]["lon"] + 0.009 * math.cos(a) / math.cos(math.radians(lat))
    return {"lat": f"{lat:.5f}", "lon": f"{lon:.5f}"}

for p in random.sample(pois, min(n, len(pois))):
    c = (p["center"]["lon"], p["center"]["lat"])
    fs = search(p["name"])
    check("name", fs and fs[0]["properties"].get("name") == p["name"])
    fs = search(p["name"], **near(p))
    check("poi", fs and (fs[0]["properties"]["osm_id"] == p["osm_id"] or
                         (fs[0]["properties"].get("name") == p["name"] and dist(fs[0]["geometry"]["coordinates"], c) < 250)))
    fs = search(p["name"][: max(3, int(len(p["name"]) * 0.6))], limit=5, **near(p))
    check("prefix", any(f["properties"]["osm_id"] == p["osm_id"] or
                        (f["properties"].get("name") == p["name"] and dist(f["geometry"]["coordinates"], c) < 250) for f in fs))

for kind, (hit, tot) in results.items():
    print(f"{kind:7} {100 * hit / tot:5.1f}%  ({hit}/{tot})")
