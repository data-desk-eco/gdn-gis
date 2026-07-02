# gdn-gis — uk gas distribution network geospatial data

`gdn-gis` builds geospatial datasets on uk gas distribution networks from their
various obscure sources. its core reverse-engineers the vector map data shipped
inside the cadent / national grid **MAPS Viewer** windows distribution and
assembles it into a single distribution-ready geoparquet (plus gpu-map artefacts);
it also pulls in the dft street manager permit archive as an incident overlay. it
runs the whole flow — fetch the sources, extract — with nothing downloaded by hand.

`src/main.rs` is a **generic extractor for obfuscated-webcgm (`.mvf`) tile
distributions**: it decodes the tiles, keeps only the layers named in a config,
and dissolves each feature's per-tile fragments into one coherent line by id **as
it goes** — no intermediate. all dataset-specific knowledge (password, layer
filter, attribute vocabulary, labelling, crs) lives in `config.toml`; the gas
network is just one such config. the whole distribution (2.7 GB, 168k tiles) is
processed in ~30 s.

the two source datasets are fetched by standalone shell scripts (curl / bsdtar /
jq); the rust binary is the pure parser.

```sh
# 1. fetch the sources (nothing downloaded by hand)
scripts/fetch-maps.sh data/maps-viewer.zip USER PASS   # dnv veracity login -> latest bundle
scripts/fetch-works.sh data/streetworks                # stream the street manager permit archive

# 2. build
cargo build --release
./target/release/gdn-gis            # extract the bundle -> geoparquet + gpu-map artefacts
./target/release/gdn-gis --works    # (re)build just the incident artefacts from the archive
```

run the binary from the repo root: paths in the config (`zip`, `output`,
`area.file`, `works.dir`) are relative to the working directory.

```
gdn-gis [CONFIG.toml] [ZIP] [options]
  CONFIG.toml   extraction config            (default config.toml)
  ZIP           tile archive                 (overrides config `zip`)
  -o FILE       output geoparquet            (overrides config `output`)
  -p PASSWORD   zip password                 (overrides config `password`)
  --square SK   only this 100 km square      (debug)
  --limit N     only the first N tiles       (debug)
  -j N          worker threads               (default: all cores)
  --works       rebuild only dist/works.* from an already-fetched archive
```

### fetching the sources

```
scripts/fetch-maps.sh OUT USER PASS
  authenticate to dnv veracity (azure b2c) and download the latest maps viewer
  bundle to OUT. USER / PASS are the veracity account credentials.

scripts/fetch-works.sh DIR [BUCKET] [MATCH]
  stream the dft street manager permit archive (open s3 bucket, BUCKET defaults to
  the dft one), keeping every gas transporter (MATCH) into DIR. skips months
  already fetched.
```

### the config

`config.toml` drives the engine. the keys, with the cadent-gas values:

| key | meaning |
|---|---|
| `zip` / `output` / `password` | archive, output geoparquet, aes password |
| `square_index` | path segment giving the 100 km grid square (`DATA/GAS/NG/`**`NY`**`/…`) |
| `crs` | projjson written into the geoparquet `geo` metadata |
| `[aps]` | the webcgm vocabulary: `layer` / `feature` frame types and the `layer_attr` / `id_attr` / `spec_attr` attribute names, `id_null` values, `strip_layer_prefix` |
| `[keep] layer_contains` | only features whose layer contains one of these survive |
| `[tier]` | optional: cleaned layer → `(code, label)`, emitted as two columns |
| `[spec]` | optional: a regex + material table parsing the spec string into `diameter_mm` / `material` / host columns |
| `[area]` | optional: tag each grid tile with an area name from an `.adf`-style ini |

drop `[tier]`, `[spec]` or `[area]` and the corresponding columns simply
disappear from the output — point the binary at a different maps-viewer export by
writing a new config.

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

1. decode each tile's header and locate where the plain-cgm picture body begins
   (the obfuscated/plain boundary isn't a fixed structural marker, so it's found
   by scanning for the earliest offset whose commands are in-range geometry *and*
   that is genuinely command-aligned — it reaches a real `layer`/`grobject` frame
   before the picture ends, without a desync. a points-only test is not enough: a
   few obfuscated-header bytes just before the true body decode as a stray in-range
   primitive run, so an offset a hair too early passes it but then drifts and drops
   — or silently swallows — the tile); walk the APS tree.
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
**~2.31 million pipes, ~140,000 km of main.** the feature symbology is taken from
the viewer's own key (`data/meta/chm/Symbol_Key_MAPSViewer.gif`).

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
| lp | low pressure | 2,112,518 | 112,909 |
| mp | medium pressure | 156,690 | 15,359 |
| nhp | national high pressure | 16,464 | 3,637 |
| ip | intermediate pressure | 12,832 | 3,063 |
| lhp | local high pressure | 7,359 | 4,958 |

### completeness vs cadent's open data

cross-checked against cadent's own *gas pipe infrastructure (gpi)* open-data
geoparquet (≈2.27 m features, lp/mp only). after aligning the two on a 50 m grid
the linework agrees closely: our lp+mp count (≈2.27 m) now matches the open
extract's, and what each side still lacks is **vintage** — the open extract is
newer than this april-2026 viewer snapshot — not missing extraction. (an earlier
revision of the picture-body finder silently dropped ~4,000 km — whole 1 km tiles
left as square holes — wherever the body started a few bytes past where its
points-only heuristic guessed; the alignment-validated finder recovers them.)
note the open file
omits the entire high/intermediate-pressure tier (≈35 k features) we carry, and
segments mains finely by asset id where we dissolve them into one line per os
feature id (the cleaner topological form); its per-segment split is an artefact
of tile/asset granularity, not reproducible from — nor preferable to — ours.

## full-network gpu map

`web/map.html` is a standalone, single-file full-screen webgpu map of the whole
network — every pipe as a gpu line, coloured by the *live carrier* material
(polyethylene→blue … cast iron→red; unknown→grey) with the killer class,
medium-pressure ductile iron (mpdi), highlighted in magenta on top. because a
sliplined main carries gas in its pe insert, lined iron correctly reads as pe and
cools to blue — so the map shows true present-day material risk, not the historic
iron footprint. no maplibre, no basemap, no dependencies: one webgpu shader (two
override-constant pipelines — material base layer, then mpdi highlight) drawing
instanced line segments, pan/zoom by a single uniform transform. opens on ipswich.

it's *demand-paged* (level-of-detail): the extractor emits its artefacts as part of
the same run whenever `config.toml` has a `[map]` section (`src/map.rs`), straight
from the in-memory features in british national grid (metres → km, no reprojection):

- `dist/map.f32` — every segment `x0 y0 x1 y1 tone flag` (le f32, km), binned into a
  2 km grid and sorted by cell id so each cell is one contiguous byte-range.
- `dist/map.idx` — a uint32 segment count per grid cell (180×180) the client
  prefix-sums into byte offsets, http-range-fetching only the cells in view.
- `dist/map.base.f32` — a coarse skeleton of the longer trunk mains (~370k segments),
  hilbert-sorted, always resident, shown when zoomed out.
- `dist/map.json` — grid + default view + zoom thresholds, so `web/map.html` hardcodes
  no constants (change `[map]` and both ends stay in sync).

webgpu needs http(s) and the per-cell fetches need http range, which the stdlib
server ignores — so `scripts/serve.py` is a tiny range-capable static server.

```sh
cargo run --release            # extracts the network *and* writes dist/map.*
python3 scripts/serve.py               # then open http://localhost:8000/web/map.html
```
