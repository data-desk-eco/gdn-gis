#!/usr/bin/env python3
"""crawl the ea 1 m composite dtm (open wcs) into per-cell height grids.

fetches every populated 2 km cell of the map grid (dist/map.idx) at 8 m posts —
251×251 u16 decimetres+1000, le, rows south→north, 65535 = nodata — into
data/terr1/<E>_<N> (cell sw corner in km, so the archive survives grid
changes). checkpointed: existing files are skipped, so it resumes; a zero-byte
file marks a cell the wcs has no coverage for (all of scotland — the ea
composite is england-only). gdal cli required.

    scripts/fetch-lidar.py [WORKERS=6]
"""
import array, json, pathlib, subprocess, sys, tempfile, time, urllib.request
from concurrent.futures import ThreadPoolExecutor

import numpy as np

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "data/terr1"
WCS = ("https://environment.data.gov.uk/spatialdata/lidar-composite-digital-terrain-model-dtm-1m/wcs"
       "?service=WCS&version=2.0.1&request=GetCoverage"
       "&coverageId=13787b9a-26a4-4775-8523-806d13af58fc__Lidar_Composite_Elevation_DTM_1m"
       "&format=image/tiff&scalefactor=0.125")
_m = json.loads((ROOT / "dist/map.json").read_text())  # the grid the emitter built
MINX, MINY, CELL, NC = int(_m["minx"] * 1000), int(_m["miny"] * 1000), int(_m["cell"] * 1000), _m["ncols"]
P = _m["t1"]["p"]  # posts per cell edge (8 m, both edges shared with neighbours)


def fetch(cid):
    e = MINX + (cid % NC) * CELL
    n = MINY + (cid // NC) * CELL
    p = OUT / f"{e // 1000}_{n // 1000}"
    if p.exists():
        return 0
    url = f"{WCS}&subset=E({e-4},{e+CELL+4})&subset=N({n-4},{n+CELL+4})"
    for attempt in range(3):
        try:
            data = urllib.request.urlopen(url, timeout=180).read()
            break
        except Exception:
            if attempt == 2:
                return print(f"  cell {cid}: gave up") or 0
            time.sleep(5 * (attempt + 1))
    if data[:2] not in (b"II", b"MM"):  # xml error → no coverage here
        p.write_bytes(b"")
        return 1
    with tempfile.TemporaryDirectory() as td:
        tif, raw = pathlib.Path(td, "t.tif"), pathlib.Path(td, "t.raw")
        tif.write_bytes(data)
        r = subprocess.run(["gdal_translate", "-q", "-of", "ENVI", "-ot", "Float32", tif, raw],
                           capture_output=True)
        f = array.array("f", raw.read_bytes()) if not r.returncode else None
    if not f or len(f) != P * P:  # decode failure: no marker, so a rerun retries it
        return print(f"  cell {cid}: bad tiff, will retry") or 0
    v = np.asarray(f, dtype=np.float64).reshape(P, P)[::-1]  # tiff rows run north→south; store south→north
    g = np.where((v > -200) & (v < 3000), np.clip(np.rint(v * 10) + 1000, 0, 65534), 65535).astype("<u2")
    p.write_bytes(g.tobytes())
    return 1


if __name__ == "__main__":
    OUT.mkdir(exist_ok=True)
    cnt = array.array("I", (ROOT / "dist/map.idx").read_bytes())
    name = lambda i: f"{(MINX + i % NC * CELL) // 1000}_{(MINY + i // NC * CELL) // 1000}"
    todo = [i for i, c in enumerate(cnt) if c and not (OUT / name(i)).exists()]
    print(f"lidar: {len(todo)} cells to fetch")
    t0, done = time.time(), 0
    with ThreadPoolExecutor(int(sys.argv[1]) if len(sys.argv) > 1 else 6) as ex:
        for r in ex.map(fetch, todo):
            done += 1
            if done % 200 == 0:
                print(f"  {done}/{len(todo)}  ({done/(time.time()-t0):.1f} cells/s)", flush=True)
    print(f"lidar: done, {done} cells in {(time.time()-t0)/60:.0f} min")
