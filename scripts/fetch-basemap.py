#!/usr/bin/env python3
"""gb basemap: coastline + place names -> dist/coast.u16 + dist/places.tsv.

coastline from the ons countries boundaries (bgc, 20 m generalised, native
epsg:27700): england/scotland/wales only, shared inland borders xor'd away so
only the coast survives; the remaining chains are douglas-peucker'd at 12 m
and packed as le u16 x0,y0,x1,y1 per segment in 20 m units. places from os
open names populated places (native bng), sorted by least-detail view scale
descending: name\te_km\tn_km\tres rows — res drives the zoom gate in the
viewer.

    scripts/fetch-basemap.py
"""
import csv, io, json, pathlib, struct, urllib.request, zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
ONS = ("https://services1.arcgis.com/ESMARspQHYMw9BZ9/arcgis/rest/services/"
       "Countries_December_2024_Boundaries_UK_BGC/FeatureServer/0/query"
       "?where=1%3D1&outFields=CTRY24NM&outSR=27700&f=geojson")
OSON = "https://api.os.uk/downloads/v1/products/OpenNames/downloads?area=GB&format=CSV&redirect"


def dp(pts, tol2=144.0):
    """iterative douglas-peucker, squared tolerance in metres."""
    keep, st = {0, len(pts) - 1}, [(0, len(pts) - 1)]
    while st:
        i, j = st.pop()
        (ax, ay), (bx, by) = pts[i], pts[j]
        dx, dy = bx - ax, by - ay
        l2 = dx * dx + dy * dy or 1.0
        k, dm = -1, tol2
        for m in range(i + 1, j):
            px, py = pts[m]
            t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / l2))
            d = (px - ax - t * dx) ** 2 + (py - ay - t * dy) ** 2
            if d > dm:
                k, dm = m, d
        if k >= 0:
            keep.add(k)
            st += [(i, k), (k, j)]
    return [pts[i] for i in sorted(keep)]


# coast: xor segments so the shared england/scotland/wales borders cancel,
# then re-walk each ring, simplify the surviving coastal chains, quantize
key = lambda a, b: (a, b) if a < b else (b, a)
segs, rings = set(), []
for f in json.load(urllib.request.urlopen(ONS))["features"]:
    if f["properties"]["CTRY24NM"] == "Northern Ireland":
        continue
    g = f["geometry"]
    polys = g["coordinates"] if g["type"] == "MultiPolygon" else [g["coordinates"]]
    for ring in (r for p in polys for r in p):
        pts = [(round(x), round(y)) for x, y in ring]
        rings.append(pts)
        for a, b in zip(pts, pts[1:]):
            if a != b:
                segs ^= {key(a, b)}
buf, n = bytearray(), 0
def flush(run):
    global n
    for c, d in zip(q := dp(run), q[1:]):
        buf.extend(struct.pack("<4H", *(round(v / 20) for v in c + d)))
        n += 1
for pts in rings:
    run = []
    for a, b in zip(pts, pts[1:]):
        if a != b and key(a, b) in segs:
            run = run or [a]
            run.append(b)
        elif run:
            flush(run)
            run = []
    if run:
        flush(run)
(ROOT / "dist/coast.u16").write_bytes(buf)

# places: os open names populated place points, biggest tier first
places = []
with zipfile.ZipFile(io.BytesIO(urllib.request.urlopen(OSON).read())) as z:
    for nm in z.namelist():
        if nm.startswith("Data/"):
            for r in csv.reader(io.TextIOWrapper(z.open(nm), encoding="utf8")):
                if r[6] == "populatedPlace" and r[2] not in ("City of London", "City of Westminster"):
                    nm = r[4] if r[3] == "cym" and r[4] else r[2]  # english name where welsh is primary
                    places.append((int(r[11]), nm, float(r[8]), float(r[9])))
places.sort(key=lambda p: (-p[0], p[1]))
(ROOT / "dist/places.tsv").write_text(
    "\n".join(f"{nm}\t{x / 1e3:.2f}\t{y / 1e3:.2f}\t{res}" for res, nm, x, y in places) + "\n")
print(f"{n} coast segments ({len(buf) // 1024} KiB), {len(places)} places")
