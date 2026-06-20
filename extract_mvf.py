# /// script
# requires-python = ">=3.11"
# dependencies = ["pyzipper", "shapely", "pyarrow", "pyproj"]
# ///
"""
extract the geospatial vector data from the cadent/national-grid "maps viewer"
windows distribution (MapsViewerApril2026.zip) into geoparquet.

the geometry lives in ~168k .mvf tiles under DATA/GAS/NG/<square>/. each .mvf is
an obfuscated webcgm (iso 8632 binary cgm) graphic produced by the bundled
nmedw1.dll interpreter:

  * the metafile header (metadata strings, layer name, vdc extent) is stored
    xor-0xff + 16-bit-byteswapped.
  * the picture body (the actual map linework) is stored as PLAIN big-endian
    binary cgm, a long run of POLYLINE primitives grouped by EXTERN object-id
    markers, in a virtual device coordinate space (0,0)-(16000,16000).

the header carries the british-national-grid bounds of the 500 m tile
(Xmin/Ymin/Xmax/Ymax). vdc coords map linearly onto those bounds, giving
geometry in epsg:27700.

usage:
  uv run extract_mvf.py [ZIP] [-o OUTDIR] [-p PASSWORD] [--limit N] [--square SK]
"""
import argparse, json, os, re, struct, sys
from multiprocessing import Pool
import pyzipper, pyproj, pyarrow as pa, pyarrow.parquet as pq
from shapely.geometry import LineString, MultiLineString, Polygon
from shapely import wkb

PW = b"reply-Dy7bge"
VDC_DEFAULT = 16000
META_RE = re.compile(rb'"(\w+):([^"]*)"')
STR_RE = re.compile(rb'[\x20-\x7e]{2,}')
# cgm graphical primitives we treat as geometry (class 4)
POLYLINE, DISJOINT, POLYGON, POLYGONSET = 1, 2, 7, 8
# webcgm application-structure delimiters (class 0)
APS_BEGIN, APS_BODY, APS_END = 21, 22, 23
END_PIC, END_META = 5, 2


def deob(d):
    """xor 0xff + swap adjacent byte pairs -> readable header bytes."""
    x = bytearray(b ^ 0xFF for b in d)
    for i in range(0, len(x) - 1, 2):
        x[i], x[i + 1] = x[i + 1], x[i]
    return bytes(x)


def layer_name(db):
    """value of the webcgm 'LayerName' aps attribute (length-prefixed string)."""
    j = db.find(b"LayerName")
    if j < 0:
        return None
    for i in range(j + 9, min(j + 40, len(db))):
        n = db[i]
        if 3 <= n <= 63:
            s = db[i + 1:i + 1 + n]
            if len(s) == n and all(32 <= c < 127 for c in s):
                return s.decode()
    return None


def vdc_extent(db):
    """read PIC DESC / vdc-extent (class 2, id 6) from the deobfuscated header."""
    p = 0
    while p + 2 <= len(db):
        cw = struct.unpack(">H", db[p:p + 2])[0]
        cl, eid, ln, q = cw >> 12, (cw >> 5) & 0x7F, cw & 0x1F, p + 2
        if cl > 9:
            break
        if ln == 31:
            if q + 2 > len(db):
                break
            ln = struct.unpack(">H", db[q:q + 2])[0] & 0x7FFF
            q += 2
        if cl == 2 and eid == 6 and ln == 8:
            x0, y0, x1, y1 = struct.unpack(">4h", db[q:q + 8])
            return abs(x1 - x0) or VDC_DEFAULT, abs(y1 - y0) or VDC_DEFAULT
        if cl == 0 and eid == 4:  # begin picture body -> header done
            break
        p = q + ln + (ln & 1)
    return VDC_DEFAULT, VDC_DEFAULT


def commands(d, p):
    """yield (class, id, parambytes) of plain big-endian cgm from offset p."""
    n = len(d)
    while p + 2 <= n:
        cw = struct.unpack(">H", d[p:p + 2])[0]
        cl, eid, ln, q = cw >> 12, (cw >> 5) & 0x7F, cw & 0x1F, p + 2
        if cl > 9:
            return
        if ln == 31:
            if q + 2 > n:
                return
            L = struct.unpack(">H", d[q:q + 2])[0]
            q += 2
            if L & 0x8000:  # partitioned data not used by this writer
                return
            ln = L
        yield cl, eid, d[q:q + ln]
        p = q + ln + (ln & 1)


def find_start(d, db):
    """offset of the first plain-cgm picture-body command (after the header)."""
    j = db.find(b"Ymax")
    lo = (j + 12) if j >= 0 else 380
    for s in range(lo, min(len(d), lo + 4000), 2):
        tot = ok = nprim = 0
        good = True
        for cl, eid, par in _peek(d, s, 25):
            if cl is None:
                good = False
                break
            if cl == 4 and eid in (POLYLINE, DISJOINT, POLYGON):
                nprim += 1
                v = struct.unpack(">%dh" % (len(par) // 2), par[:len(par) // 2 * 2])
                for i in range(0, len(v) - 1, 2):
                    tot += 1
                    if -3000 <= v[i] <= 19000 and -3000 <= v[i + 1] <= 19000:
                        ok += 1
        if good and nprim >= 2 and tot >= 20 and ok / tot > 0.95:
            return s
    return None


def _peek(d, p, lim):
    n = len(d)
    for _ in range(lim):
        if p + 2 > n:
            return
        cw = struct.unpack(">H", d[p:p + 2])[0]
        cl, eid, ln, q = cw >> 12, (cw >> 5) & 0x7F, cw & 0x1F, p + 2
        if cl > 9:
            yield None, None, None
            return
        if ln == 31:
            if q + 2 > n:
                yield None, None, None
                return
            L = struct.unpack(">H", d[q:q + 2])[0]
            q += 2
            if L & 0x8000:
                yield None, None, None
                return
            ln = L
        yield cl, eid, d[q:q + ln]
        p = q + ln + (ln & 1)


def pts(par, x0, y0, sx, sy):
    """decode int16 vdc pairs -> list of (easting, northing) in epsg:27700."""
    m = len(par) // 4 * 2
    v = struct.unpack(">%dh" % m, par[:m * 2])
    return [(x0 + v[i] * sx, y0 + v[i + 1] * sy) for i in range(0, m, 2)]


def parse_tile(name):
    try:
        return _parse_tile(name)
    except Exception:
        return None, []


def _parse_tile(name):
    """worker: read+decode one .mvf, return (square, [rows]).

    the picture body is a tree of webcgm application structures (APS): a
    `layer` APS (LayerName = pressure tier / annotation class) wrapping
    `grobject` APS (one per asset, carrying Name = os feature id and
    ScreenTip = pipe spec). every polyline inherits the LayerName of the
    enclosing layer APS and the Name/ScreenTip of the enclosing grobject.
    """
    d = _ZIP.read(name)
    db = deob(d)
    meta = {k.decode("latin1"): v.decode("latin1") for k, v in META_RE.findall(db)}
    try:
        x0, y0, x1, y1 = (int(meta[k]) for k in ("Xmin", "Ymin", "Xmax", "Ymax"))
    except (KeyError, ValueError):
        return None, []
    vw, vh = vdc_extent(db)
    sx, sy = (x1 - x0) / vw, (y1 - y0) / vh
    start = find_start(d, db)
    if start is None:
        return None, []
    facet = meta.get("Facet", os.path.splitext(os.path.basename(name))[0])
    square = name.split("/")[3] if name.count("/") >= 3 else facet[:2]
    date, src = meta.get("Date"), meta.get("Source")
    rows, stack = [], []  # stack of [type, layer, name, tip]
    for cl, eid, par in commands(d, start):
        if cl == 0 and eid == APS_BEGIN:
            s = STR_RE.findall(par)
            stack.append([s[1].decode("latin1") if len(s) > 1 else "?", None, None, None])
            continue
        if cl == 0 and eid == APS_END:
            if stack:
                stack.pop()
            continue
        if cl == 0 and eid in (END_PIC, END_META):
            break
        if cl == 9 and eid == 1:  # APS attribute: <name><SDR value>
            s = STR_RE.findall(par)
            if s and stack:
                k = s[0].rstrip(b'"&)(').strip().decode("latin1")
                v = s[1].decode("latin1") if len(s) > 1 else None
                if k == "LayerName":
                    for fr in stack:
                        if fr[0] == "layer":
                            fr[1] = v
                elif k == "Name":
                    stack[-1][2] = v
                elif k == "ScreenTip":
                    stack[-1][3] = v
            continue
        if cl != 4 or eid not in (POLYLINE, DISJOINT, POLYGON):
            continue
        cs = pts(par, x0, y0, sx, sy)
        if len(cs) < 2:
            continue
        if eid == POLYLINE:
            g, t = LineString(cs), "LineString"
        elif eid == DISJOINT:
            segs = [cs[i:i + 2] for i in range(0, len(cs) - 1, 2)]
            g, t = MultiLineString([s for s in segs if len(s) == 2]), "MultiLineString"
        else:
            if len(cs) < 3:
                continue
            g, t = Polygon(cs), "Polygon"
        layer = next((fr[1] for fr in reversed(stack) if fr[1]), None)
        gr = next((fr for fr in reversed(stack) if fr[0] == "grobject"), None)
        rows.append((square, facet, src, date, layer,
                     gr[2] if gr else None, gr[3] if gr else None, t, wkb.dumps(g)))
    return square, rows


def init_worker(zip_path):
    global _ZIP
    _ZIP = pyzipper.AESZipFile(zip_path)
    _ZIP.setpassword(PW)


SCHEMA = pa.schema(
    [
        ("square", pa.string()),
        ("facet", pa.string()),
        ("source", pa.string()),
        ("survey_date", pa.string()),
        ("layer", pa.string()),
        ("name", pa.string()),
        ("screentip", pa.string()),
        ("etype", pa.string()),
        ("geometry", pa.binary()),
    ]
)


def geo_meta():
    crs = json.loads(pyproj.CRS.from_epsg(27700).to_json())
    return {
        b"geo": json.dumps(
            {
                "version": "1.1.0",
                "primary_column": "geometry",
                "columns": {
                    "geometry": {
                        "encoding": "WKB",
                        "geometry_types": ["LineString", "MultiLineString", "Polygon"],
                        "crs": crs,
                    }
                },
            }
        ).encode()
    }


def batch(rows):
    cols = list(zip(*rows))
    return pa.record_batch(
        [
            pa.array(cols[0]), pa.array(cols[1]), pa.array(cols[2]),
            pa.array(cols[3]), pa.array(cols[4]), pa.array(cols[5]),
            pa.array(cols[6]), pa.array(cols[7]), pa.array(cols[8], pa.binary()),
        ],
        schema=SCHEMA.with_metadata(geo_meta()),
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("zip", nargs="?", default="MapsViewerApril2026.zip")
    ap.add_argument("-o", "--outdir", default="output")
    ap.add_argument("-p", "--password", default=None)
    ap.add_argument("--limit", type=int, default=0, help="max tiles per square (debug)")
    ap.add_argument("--square", default=None, help="only this 100km square")
    ap.add_argument("-j", "--jobs", type=int, default=os.cpu_count())
    a = ap.parse_args()
    if a.password:
        global PW
        PW = a.password.encode()
    os.makedirs(a.outdir, exist_ok=True)

    with pyzipper.AESZipFile(a.zip) as zf:
        names = [n for n in zf.namelist() if n.lower().endswith(".mvf")]
    by_sq = {}
    for n in names:
        sq = n.split("/")[3] if n.count("/") >= 3 else "??"
        by_sq.setdefault(sq, []).append(n)

    squares = [a.square] if a.square else sorted(by_sq)
    grand = 0
    with Pool(a.jobs, initializer=init_worker, initargs=(a.zip,)) as pool:
        for sq in squares:
            files = by_sq.get(sq, [])
            if a.limit:
                files = files[: a.limit]
            if not files:
                continue
            out = os.path.join(a.outdir, f"NG_{sq}.parquet")
            writer = pq.ParquetWriter(out, SCHEMA.with_metadata(geo_meta()), compression="zstd")
            buf, nfeat, ntile = [], 0, 0
            for square, rows in pool.imap_unordered(parse_tile, files, chunksize=64):
                ntile += 1
                if rows:
                    buf.extend(rows)
                    nfeat += len(rows)
                if len(buf) >= 200_000:
                    writer.write_batch(batch(buf))
                    buf = []
            if buf:
                writer.write_batch(batch(buf))
            writer.close()
            grand += nfeat
            print(f"  {sq}: {ntile} tiles -> {nfeat} features  {out}", flush=True)
    print(f"done: {grand} features across {len(squares)} squares -> {a.outdir}/")


if __name__ == "__main__":
    main()
