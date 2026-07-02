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
scripts/fetch-terrain.sh                               # os terrain 50 (coarse national relief)
scripts/fetch-lidar.py                                 # ea 1 m lidar dtm, per-cell wcs crawl
scripts/fetch-buildings.sh                             # geofabrik england osm extract
scripts/build-years.sh                                 # laid-year sidecar from cadent's open gpi data

# 2. build
cargo build --release
./target/release/gdn-gis                # extract the bundle -> geoparquet + gpu-map artefacts
./target/release/gdn-gis --works       # (re)build just the incident artefacts from the archive
./target/release/gdn-gis --terrain     # (re)build the relief tiers from the fetched dtm
./target/release/gdn-gis --buildings   # (re)build the buildings layer from the pbf
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
  --terrain     rebuild only dist/terr*  from data/terr50.zip + data/terr1/
  --buildings   rebuild only dist/bldg.* from data/england-latest.osm.pbf
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

scripts/fetch-terrain.sh [OUT.zip]
  os terrain 50 (open data), one national ascii-grid zip -> data/terr50.zip.

scripts/fetch-lidar.py [WORKERS]
  crawl the ea 1 m composite dtm wcs at 8 m posts over every populated map cell
  -> data/terr1/<cellid>. checkpointed per cell; rerun to resume.

scripts/fetch-buildings.sh [OUT.pbf]
  the geofabrik england .osm.pbf -> data/england-latest.osm.pbf.

scripts/build-years.sh
  laid-year sidecar: median gpi install year per (100 m cell, material)
  -> data/years.tsv (needs duckdb; the emitter picks it up automatically).
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
network — every pipe as a gpu hairline, coloured by the *live carrier* material
(polyethylene→blue … cast iron→red; unknown→grey) with the killer class,
medium-pressure ductile iron (mpdi), highlighted in magenta on top. because a
sliplined main carries gas in its pe insert, lined iron correctly reads as pe and
cools to blue — so the map shows true present-day material risk, not the historic
iron footprint. no maplibre, no basemap, no framework, no build step. opens on
ipswich, autoplaying a laid-year build-out of the network from the 1850s to today
(scrubbable from the bottom-centre timeline); street-manager incidents pop in at
the end.

the camera is a full 3d orbit (drag pans, right-drag / two-finger twist orbits,
wheel dollies to the cursor). far out a pitch clamp holds it top-down and the map
reads as a flat 2d one; past the detail zoom the clamp opens onto a wireframe
city: **ea 1 m lidar relief** drawn as a draped wire grid and **osm building
wireframes**, all 1 px lines on black, with the pipes and incident rings draped
over the terrain by the vertex shaders.

### the artefact contract

every heavy layer is *(base, blob, idx)* against the same 2 km × 180×180 grid
(`map.json`): a small always-resident base, a cell-sorted blob the client
http-range-fetches per visible cell, and a per-cell index. one paging loop, one
lru per layer. pipes and buildings share a single 12-byte record: `u16 x0 y0 x1
y1` cell-local coords (segments clipped at cell boundaries — ~3 cm posts,
seam-exact), `u16 cell id` (the shader rebuilds world coords, no per-draw
uniforms), and two payload bytes — pipes carry material tone (+ mpdi flag) and
the laid year as an offset from 1848; buildings carry height and min-height in
half-metres. terrain cells are fixed-size, so their index degenerates to a
presence bitmap and a cell fetch is one arithmetic range.

| artefact | contents |
|---|---|
| `map.bin` / `map.idx` / `map.base.bin` | pipe segments (12 B records; base = trunk skeleton, resident) |
| `terr0.bin` | resident coarse relief: 1801×1801 u16 decimetre posts at 200 m (os terrain 50) |
| `terr1.bin` / `terr1.idx` | paged lidar: 251×251 posts at 8 m per populated cell (ea 1 m composite dtm) + presence bitmap |
| `bldg.bin` / `bldg.idx` | osm building footprint edges (12 B records), street zoom only |
| `bldg.tsv` / `bldg.tofs` | named buildings for the click card, range-fetched per cell |
| `works.f32` / `works.tsv` | street-manager incidents: `x y day flag` + lazy detail rows |
| `map.json` | grid, default view, zoom thresholds, year span — the client hardcodes none of it |

the laid years are an *estimate*: `scripts/build-years.sh` bins every vertex of
cadent's open gpi dataset (2.27 m rows, `inst_date` 96 % populated, materials in
the same two-letter codes as the screentips) to a 100 m grid and takes the median
install year per (cell, material); the emitter stamps each segment from the
nearest match, ring-searching two cells out, with an any-material fallback.
unmatched segments (≈1 %, mostly the high-pressure tier the open data omits) are
always visible.

terrain heights are u16 decimetres + 1000 (the fens sit below sea level), rows
south→north; the client uploads them as `r16uint` textures (one coarse national
texture + a 224-layer array for lidar cells, indexed by a cell→layer lut) and
every layer's vertex shader drapes through the same bilinear sample.

### building it

```sh
scripts/fetch-terrain.sh               # os terrain 50 (one 160 MB open-data zip)
scripts/fetch-buildings.sh             # geofabrik england .osm.pbf (~1.7 GB)
scripts/fetch-lidar.py                 # ea 1 m dtm wcs crawl, checkpointed per cell (~1 h)
scripts/build-years.sh                 # gpi laid-year sidecar (duckdb, seconds)

cargo run --release                    # network extract -> geoparquet + map.* (+ works.*)
cargo run --release -- --terrain       # terr0/terr1 from the fetched relief
cargo run --release -- --buildings     # bldg.* from the pbf (~3 min)
```

`fetch-lidar.py` resumes where it left off, and `--terrain` folds in whatever
cells have landed — each step ships a working map. webgpu needs http(s) and the
per-cell fetches need http range, which the stdlib server ignores — so
`scripts/serve.py` is a tiny range-capable static server. for hosting, range
requests are load-bearing: the blobs want an s3/r2-style bucket (github pages
caps files at 100 MB); quantisation *is* the ranged blobs' compression, and the
small resident files gzip/brotli well.

```sh
python3 scripts/serve.py               # then open http://localhost:8000/web/map.html
```
