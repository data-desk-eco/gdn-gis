# cadent gas distribution network — geospatial extraction

reverse-engineers the vector map data shipped inside the cadent / national grid
**MAPS Viewer** windows distribution (`MapsViewerApril2026.zip`) and assembles it
into a single distribution-ready geoparquet of the gas distribution network.

two stages:

1. **`extract_mvf.py`** — decode the 168,458 obfuscated `.mvf` tiles into one
   geoparquet per 100 km square (`output/NG_<square>.parquet`), one row per
   drawn line/polygon, carrying the per-feature webcgm attributes.
2. **`build.py`** — keep the gas-asset features, stitch each pipe's per-tile
   fragments into one coherent line, label them, attach metadata from the
   distribution, and write **`dist/cadent_gas_network.parquet`**.

```sh
uv run extract_mvf.py MapsViewerApril2026.zip -o output   # all 15 squares
uv run build.py                                           # -> dist/
```

## what's in the distribution

the viewer is an old (mfc / msxml-era) windows gis. its geospatial data lives in
`.mvf` tiles under `DATA/GAS/NG/<square>/<10km>/`, one tile per 500 m cell of the
ordnance survey british national grid, covering 15 100 km squares across the
cadent gas-distribution licence area. the bundled dlls are only the *interpreter*
— the data is in the tiles.

## the `.mvf` format (reverse-engineered)

each `.mvf` is an **obfuscated webcgm** (iso 8632 binary cgm) graphic:

| region | encoding |
|---|---|
| metafile header (metadata strings, layer name, vdc extent) | `xor 0xff` then **swap adjacent byte pairs** → readable webcgm |
| picture body (the map linework) | **plain** big-endian binary cgm |

* the header carries `ProfileId:WebCGM`, `Source`, `Date`, `Facet`, and the
  tile's british national grid bounds `Xmin/Ymin/Xmax/Ymax` (epsg:27700).
* the body is a tree of webcgm **application structures** (APS). a `layer` APS
  (`LayerName` = pressure tier or annotation class) wraps `grobject` APS, one per
  asset, each carrying `Name` (the os feature id) and `ScreenTip` (the pipe spec,
  e.g. `180MM PE (IN 8" CI)`). every `POLYLINE` inherits the layer of its
  enclosing layer APS and the name/spec of its enclosing grobject. coordinates
  are int16 in a virtual device space `(0,0)-(16000,16000)` mapped linearly onto
  the tile's bng bounds.

the zip is aes-256 encrypted; the password is passed with `-p` (default
hard-coded).

## the gas network dataset — `dist/cadent_gas_network.parquet`

geoparquet 1.1, geometry in **epsg:27700** (osgb36 / british national grid).
**2.25 million pipes, ~136,000 km of main.** the feature symbology is taken from
the viewer's own key (`meta/chm/Symbol_Key_MAPSViewer.gif`).

each row is one coherent pipe: the per-tile `POLYLINE` fragments of a single os
feature (`Name`) are dissolved together with `ST_LineMerge`, across tile *and*
100 km square boundaries. fragments with no id (mostly high-pressure plant) are
kept individually. the dense unattributed background linework (os as-built
geography) and the cartographic annotation layers (`Dimensions`, `Notes`) are
dropped — they remain in the per-square `output/` files.

| column | type | description |
|---|---|---|
| `feature_id` | string | os feature id (`Name`); null for unidentified plant |
| `pressure_code` | string | `lp` / `mp` / `ip` / `lhp` / `nhp` |
| `pressure` | string | low / medium / intermediate / local high / national high pressure |
| `diameter_mm` | double | nominal bore in mm (inches converted ×25.4); null where the spec gives no unambiguous size |
| `material` | string | polyethylene, cast iron, spun iron, ductile iron, steel, pvc, asbestos cement, lead, unknown |
| `host_diameter_mm` | double | for an inserted main, the bore of the legacy main it was relined through |
| `host_material` | string | material of that host main |
| `inserted` | bool | true when a (usually PE) main has been inserted into an older iron/steel main |
| `screentip` | string | the raw spec string, retained verbatim |
| `network_area` | string | cadent network: `NW` / `WM` / `EM` / `EA` / `NL` (from `NG.ADF`) |
| `square` | string | 100 km national-grid square |
| `tenk` | string | 10 km national-grid tile |
| `length_m` | double | dissolved length in metres |
| `source` | string | provenance string from the tile header (`NG,GDFO,1.0.0`) |
| `survey_date` | string | latest survey date (`yyyymmdd`) among the pipe's tiles |
| `etype` | string | `LineString` / `MultiLineString` / `Polygon` |
| `geometry` | binary | wkb |

### pressure tiers

| code | tier | pipes | km |
|---|---|---:|---:|
| lp | low pressure | 2,066,208 | 109,859 |
| mp | medium pressure | 152,009 | 14,846 |
| lhp | local high pressure | 7,166 | 4,762 |
| nhp | national high pressure | 15,592 | 3,439 |
| ip | intermediate pressure | 12,626 | 2,981 |

## the per-square extract — `output/NG_<square>.parquet`

every drawn primitive, including the os background and annotation. columns:
`square`, `facet`, `source`, `survey_date`, `layer`, `name`, `screentip`,
`etype`, `geometry` (wkb, epsg:27700). 111.5 million rows total.
