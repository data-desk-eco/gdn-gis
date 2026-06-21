# cadent gas distribution network — geospatial extraction

reverse-engineers the vector map data shipped inside the cadent / national grid
**MAPS Viewer** windows distribution (`MapsViewerApril2026.zip`) and assembles it
into a single distribution-ready geoparquet of the gas distribution network.

a single rust pass (`extract.rs`) decodes the 168,458 obfuscated `.mvf` tiles,
keeps only the gas-asset linework, and dissolves each pipe's per-tile fragments
into one coherent line by os feature id **as it goes** — no intermediate. the
whole distribution (2.7 GB, 168k tiles) is processed in ~30 s.

```sh
cargo build --release
./target/release/mvf-extract MapsViewerApril2026.zip -o dist/cadent_gas_network.parquet
```

run it from the repo root: it reads `meta/NG.ADF` to tag network areas.

```
options:
  -o FILE       output geoparquet            (default dist/cadent_gas_network.parquet)
  -p PASSWORD   zip password                 (default hard-coded)
  --square SK   only this 100 km square      (debug)
  --limit N     only the first N tiles       (debug)
  -j N          worker threads               (default: all cores)
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

## how the extractor works

one streaming pass, parallel over tiles (rayon, one cached zip archive per
thread):

1. decode each tile's header and picture body; walk the APS tree.
2. **keep only the gas assets** — primitives whose enclosing layer is a
   `... Mains & Plant` tier. the dense unattributed background linework (os
   as-built geography) and the cartographic annotation layers (`Dimensions`,
   `Notes`) never leave the parser.
3. dissolve as we go: line fragments of a named pipe (os feature id) accumulate
   into one bucket keyed by `(feature id, pressure tier)`, across tile *and*
   100 km square boundaries; fragments with no id (mostly high-pressure plant)
   are emitted individually.
4. at the end, stitch each pipe's fragments end-to-end at degree-2 nodes (a
   `ST_LineMerge`-alike), parse the `ScreenTip` into diameter / material /
   insertion, tag the cadent network area from `NG.ADF`, and write the
   geoparquet.

## the gas network dataset — `dist/cadent_gas_network.parquet`

geoparquet 1.1, geometry in **epsg:27700** (osgb36 / british national grid).
**~2.25 million pipes, ~136,000 km of main.** the feature symbology is taken from
the viewer's own key (`meta/chm/Symbol_Key_MAPSViewer.gif`).

each row is one coherent pipe: the per-tile `POLYLINE` fragments of a single os
feature (`Name`) are stitched together, across tile *and* 100 km square
boundaries. fragments with no id are kept individually.

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
| `length_m` | double | stitched length in metres |
| `source` | string | provenance string from the tile header (`NG,GDFO,1.0.0`) |
| `survey_date` | string | latest survey date (`yyyymmdd`) among the pipe's tiles |
| `etype` | string | `LineString` / `MultiLineString` / `Polygon` |
| `geometry` | binary | wkb |

### pressure tiers

| code | tier | pipes | km |
|---|---|---:|---:|
| lp | low pressure | 2,066,087 | 109,857 |
| mp | medium pressure | 152,022 | 14,848 |
| nhp | national high pressure | 15,585 | 3,439 |
| ip | intermediate pressure | 12,620 | 2,981 |
| lhp | local high pressure | 7,166 | 4,762 |
