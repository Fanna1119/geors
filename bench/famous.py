#!/usr/bin/env python3
"""Search quality: do well-known place names find the well-known place?

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
