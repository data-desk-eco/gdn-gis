# /// script
# requires-python = ">=3.11"
# dependencies = ["duckdb", "pyarrow", "pyproj"]
# ///
"""
assemble the per-square geoparquet extracted by extract_mvf.py into one
distribution-ready geoparquet of the cadent gas distribution network.

steps:
  * keep the gas-asset features (the "... Mains & Plant" webcgm layers); the
    bulk un-attributed background linework (os as-built geography) and the
    cartographic annotation layers (Dimensions / Notes) are dropped.
  * normalise the pressure tier from the layer name (strips the draw-order
    sigils $ ! # the writer prepends): lp / mp / ip / lhp / nhp.
  * stitch each pipe's per-tile fragments into one coherent line by dissolving
    on the os feature id (Name), across tile *and* 100 km square boundaries,
    with ST_LineMerge. fragments with no id (UNKNOWN / null, mostly high
    pressure plant) are kept as individual features.
  * parse the ScreenTip into diameter_mm + material (+ the host pipe when a PE
    main has been inserted into a legacy iron/steel main).
  * tag each pipe with its cadent network area (EM/WM/EA/NW/NL) via NG.ADF.

output: dist/cadent_gas_network.parquet (geoparquet 1.1, epsg:27700).
"""
import glob, json, os, re, sys
import duckdb, pyproj, pyarrow as pa, pyarrow.parquet as pq

PRESSURE = {  # cleaned layer name -> (code, full)
    "Low Pressure Mains & Plant": ("lp", "low pressure"),
    "Medium Pressure Mains & Plant": ("mp", "medium pressure"),
    "Intermediate Pressure Mains & Plant": ("ip", "intermediate pressure"),
    "Local High Pressure Mains & Plant": ("lhp", "local high pressure"),
    "National High Pressure Mains & Plant": ("nhp", "national high pressure"),
}
MATERIAL = {
    "PE": "polyethylene", "CI": "cast iron", "SI": "spun iron", "DI": "ductile iron",
    "ST": "steel", "PV": "pvc", "AS": "asbestos cement", "LE": "lead",
    "UN": "unknown", "NA": "not available",
}
TIP = re.compile(
    r'^(?P<d>\d+(?:\.\d+)?)\s*(?P<u>MM|")?\s*(?P<m>[A-Z]{2})?'
    r'(?:\s*\(IN\s*(?P<hd>\d+(?:\.\d+)?)\s*(?P<hu>MM|")?\s*(?P<hm>[A-Z]{2})?\s*\))?\s*$')


def dia_mm(d, u):
    """normalise a diameter token to mm; ambiguous bare inch-scale -> none."""
    if d is None:
        return None
    v = float(d)
    if u == '"':
        return round(v * 25.4, 1)
    if u == "MM" or v >= 100:  # bare large numbers are mm (transmission mains)
        return v
    return None  # bare < 100: inch/mm ambiguous, leave to raw screentip


def parse_tip(t):
    """ScreenTip -> (diameter_mm, material, host_diameter_mm, host_material)."""
    if not t:
        return None, None, None, None
    m = TIP.match(t)
    if not m:
        return None, None, None, None
    return (dia_mm(m["d"], m["u"]), MATERIAL.get(m["m"]),
            dia_mm(m["hd"], m["hu"]), MATERIAL.get(m["hm"]))


def areas():
    """10 km national-grid tile -> cadent network area, from NG.ADF."""
    txt = open("meta/NG.ADF", errors="replace").read()
    out, cur = {}, None
    for ln in txt.splitlines():
        h = re.match(r"\[(\w+)\]", ln)
        if h and h.group(1) != "Areas":
            cur = h.group(1)
        t = re.match(r"Tile\d+=(\w+)", ln)
        if t and cur:
            out[t.group(1)] = cur
    return out


def geo_meta(bbox):
    crs = json.loads(pyproj.CRS.from_epsg(27700).to_json())
    return {b"geo": json.dumps({
        "version": "1.1.0", "primary_column": "geometry",
        "columns": {"geometry": {
            "encoding": "WKB",
            "geometry_types": ["LineString", "MultiLineString", "Polygon"],
            "crs": crs, "bbox": bbox,
        }},
    }).encode()}


def main():
    files = sorted(glob.glob("output/NG_*.parquet"))
    if not files:
        sys.exit("no output/NG_*.parquet — run extract_mvf.py first")
    con = duckdb.connect()
    con.install_extension("spatial"); con.load_extension("spatial")
    lst = "[" + ",".join("'%s'" % f for f in files) + "]"
    con.execute(f"CREATE VIEW raw AS SELECT * FROM read_parquet({lst})")
    # gas assets only; strip the leading draw-order sigil from the layer name.
    con.execute("""
        CREATE TABLE asset AS SELECT
            regexp_replace(layer, '^[^A-Za-z]+', '') AS layer,
            CASE WHEN name IS NULL OR name = 'UNKNOWN' THEN NULL ELSE name END AS fid,
            screentip, square, facet, source AS src, survey_date,
            geometry AS g
        FROM raw WHERE layer LIKE '%Mains & Plant%'
    """)
    # dissolve named pipes across all tiles/squares; keep unnamed as-is.
    con.execute("""
        CREATE TABLE pipe AS
        SELECT fid, layer,
               any_value(screentip) screentip,
               min(square) square, min(facet) facet,
               any_value(src) src, max(survey_date) survey_date,
               ST_LineMerge(ST_Union_Agg(g)) g
        FROM asset WHERE fid IS NOT NULL GROUP BY fid, layer
        UNION ALL
        SELECT fid, layer, screentip, square, facet, src, survey_date, g
        FROM asset WHERE fid IS NULL
    """)
    rows = con.execute("""
        SELECT fid, layer, screentip, square,
               substr(facet,1,2)||substr(facet,3,1)||substr(facet,5,1) AS tenk,
               src, survey_date,
               round(ST_Length(g), 2) AS length_m,
               ST_AsWKB(g) AS wkb,
               lower(ST_GeometryType(g)::VARCHAR) AS gtype
        FROM pipe WHERE NOT ST_IsEmpty(g)
    """).fetchall()
    area = areas()
    cols = {k: [] for k in (
        "feature_id", "pressure_code", "pressure", "diameter_mm", "material",
        "host_diameter_mm", "host_material", "inserted", "screentip",
        "network_area", "square", "tenk", "length_m", "source", "survey_date",
        "etype", "geometry")}
    minx = miny = 1e18; maxx = maxy = -1e18
    for fid, layer, tip, square, tenk, src, date, length, wkb, gtype in rows:
        pc, pf = PRESSURE.get(layer, (None, layer.lower()))
        d, mat, hd, hm = parse_tip(tip)
        cols["feature_id"].append(fid)
        cols["pressure_code"].append(pc); cols["pressure"].append(pf)
        cols["diameter_mm"].append(d); cols["material"].append(mat)
        cols["host_diameter_mm"].append(hd); cols["host_material"].append(hm)
        cols["inserted"].append(hd is not None or hm is not None)
        cols["screentip"].append(tip)
        cols["network_area"].append(area.get(tenk))
        cols["square"].append(square); cols["tenk"].append(tenk)
        cols["length_m"].append(length); cols["source"].append(src)
        cols["survey_date"].append(date)
        cols["etype"].append("MultiLineString" if gtype.startswith("multi")
                             else "Polygon" if "polygon" in gtype else "LineString")
        cols["geometry"].append(wkb)
    ext = con.execute("""SELECT ST_XMin(e),ST_YMin(e),ST_XMax(e),ST_YMax(e) FROM
        (SELECT ST_Extent_Agg(g) e FROM pipe WHERE NOT ST_IsEmpty(g))""").fetchone()
    schema = pa.schema([
        ("feature_id", pa.string()), ("pressure_code", pa.string()),
        ("pressure", pa.string()), ("diameter_mm", pa.float64()),
        ("material", pa.string()), ("host_diameter_mm", pa.float64()),
        ("host_material", pa.string()), ("inserted", pa.bool_()),
        ("screentip", pa.string()), ("network_area", pa.string()),
        ("square", pa.string()), ("tenk", pa.string()),
        ("length_m", pa.float64()), ("source", pa.string()),
        ("survey_date", pa.string()), ("etype", pa.string()),
        ("geometry", pa.binary()),
    ], metadata=geo_meta([ext[0], ext[1], ext[2], ext[3]]))
    tbl = pa.table({k: pa.array(cols[k], schema.field(k).type) for k in cols}, schema=schema)
    os.makedirs("dist", exist_ok=True)
    pq.write_table(tbl, "dist/cadent_gas_network.parquet", compression="zstd")
    print(f"{tbl.num_rows} pipes -> dist/cadent_gas_network.parquet")


if __name__ == "__main__":
    main()
