#!/usr/bin/env python3
"""Search quality: do well-known place names find the well-known place?

Two checks: `famous` (city names: the top result is an area in the
expected country) and `places` (same-named cities and landmarks: the top
result is near the expected coordinates).

Usage: bench/famous.py [BASE_URL]

Searches each name below without a country or location and checks that the
top result is in the expected country and is an area (city, region,
country). Names whose country is not loaded are skipped. Meant for data
sets spanning several countries, where a same-named village elsewhere
competes (see issue #31).
"""
import json
import sys
import urllib.parse
import urllib.request

base = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:2322"

NAMES = [
    # Asia
    ("Tokyo", "JP"), ("東京", "JP"), ("Osaka", "JP"), ("Kyoto", "JP"), ("Seoul", "KR"),
    ("Beijing", "CN"), ("Shanghai", "CN"), ("Delhi", "IN"), ("Mumbai", "IN"),
    ("Bangkok", "TH"), ("Jakarta", "ID"), ("Manila", "PH"), ("Hanoi", "VN"),
    ("Taipei", "TW"), ("Singapore", "SG"), ("Dubai", "AE"), ("Tehran", "IR"),
    ("Karachi", "PK"), ("Dhaka", "BD"), ("Istanbul", "TR"),
    # Europe
    ("Paris", "FR"), ("London", "GB"), ("Berlin", "DE"), ("Munich", "DE"),
    ("München", "DE"), ("Cologne", "DE"), ("Vienna", "AT"), ("Wien", "AT"),
    ("Rome", "IT"), ("Roma", "IT"), ("Milan", "IT"), ("Madrid", "ES"),
    ("Barcelona", "ES"), ("Amsterdam", "NL"), ("Prague", "CZ"), ("Warsaw", "PL"),
    ("Lisbon", "PT"), ("Dublin", "IE"), ("Stockholm", "SE"), ("Oslo", "NO"),
    ("Helsinki", "FI"), ("Moscow", "RU"),
    # North America
    ("New York", "US"), ("Los Angeles", "US"), ("Chicago", "US"), ("Houston", "US"),
    ("Toronto", "CA"), ("Vancouver", "CA"), ("Montreal", "CA"), ("Mexico City", "MX"),
]
AREAS = {"city", "district", "locality", "county", "state", "country"}

# Names whose well-known place is decided by importance, not text: the top
# result must lie near these coordinates (lat, lon, km). Skipped when the
# country is not loaded.
PLACES = [
    # Same-named cities
    ("Portland", "US", 45.52, -122.68, 30),     # Oregon, not Maine
    ("Kingston", "JM", 17.99, -76.79, 30),
    ("London", "GB", 51.51, -0.13, 30),         # not Ontario
    ("Birmingham", "GB", 52.48, -1.90, 30),     # not Alabama
    ("Cambridge", "GB", 52.21, 0.12, 30),       # not Massachusetts
    ("Frankfurt", "DE", 50.11, 8.68, 30),       # am Main, not (Oder)
    ("Valencia", "ES", 39.47, -0.38, 30),       # not Venezuela
    # Landmarks, searched without a location
    ("Louvre", "FR", 48.861, 2.336, 2),
    ("Eiffel Tower", "FR", 48.858, 2.294, 2),
    ("Colosseum", "IT", 41.890, 12.492, 2),
    ("Brandenburger Tor", "DE", 52.516, 13.378, 2),
    ("Stephansdom", "AT", 48.2085, 16.3731, 2),
    ("Schloss Schönbrunn", "AT", 48.1849, 16.3122, 2),
    ("Sagrada Família", "ES", 41.4036, 2.1744, 2),
    ("Big Ben", "GB", 51.5007, -0.1246, 2),
    ("Statue of Liberty", "US", 40.6892, -74.0445, 2),
    ("Golden Gate Bridge", "US", 37.8199, -122.4783, 3),
    ("Space Needle", "US", 47.6205, -122.3493, 2),
    ("CN Tower", "CA", 43.6426, -79.3871, 2),
    ("Tokyo Tower", "JP", 35.6586, 139.7454, 2),
    ("Taj Mahal", "IN", 27.1751, 78.0421, 2),
]


def km(lat1, lon1, lat2, lon2):
    import math
    la1, la2 = math.radians(lat1), math.radians(lat2)
    h = math.sin((la2 - la1) / 2) ** 2 + math.cos(la1) * math.cos(la2) * math.sin(math.radians(lon2 - lon1) / 2) ** 2
    return 12742 * math.asin(math.sqrt(h))

status = json.load(urllib.request.urlopen(f"{base}/status"))
loaded = {p["country_code"].upper() for p in status["partitions"]}

ok, total, misses = 0, 0, []
for name, cc in NAMES:
    if cc not in loaded:
        continue
    total += 1
    url = f"{base}/search?" + urllib.parse.urlencode({"q": name, "limit": 1})
    fs = json.load(urllib.request.urlopen(url))["features"]
    top = fs[0]["properties"] if fs else {}
    if top.get("countrycode") == cc and top.get("type") in AREAS:
        ok += 1
    else:
        misses.append(f"{name} ({cc}): {top.get('name')} ({top.get('type')}, {top.get('countrycode')})")

print(f"famous  {100 * ok / max(total, 1):5.1f}%  ({ok}/{total})")
for m in misses:
    print("   ", m)

ok, total, misses = 0, 0, []
for name, cc, lat, lon, radius in PLACES:
    if cc not in loaded:
        continue
    total += 1
    url = f"{base}/search?" + urllib.parse.urlencode({"q": name, "limit": 1})
    fs = json.load(urllib.request.urlopen(url))["features"]
    if fs:
        top = fs[0]["properties"]
        flon, flat = fs[0]["geometry"]["coordinates"][:2]
        d = km(lat, lon, flat, flon)
        if d <= radius:
            ok += 1
            continue
        misses.append(f"{name} ({cc}): {top.get('name')} ({top.get('type')}, {top.get('countrycode')}) {d:.0f} km away")
    else:
        misses.append(f"{name} ({cc}): nothing")
print(f"places  {100 * ok / max(total, 1):5.1f}%  ({ok}/{total})")
for m in misses:
    print("   ", m)
