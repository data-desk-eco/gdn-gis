# national map — terrain + buildings at cadent scale

a design note on fusing the sheffield experiment (3D wireframe city: EA LIDAR
terrain, OSM building extrusions, orbit camera) into the gdn-gis web map
(cadent-wide pipe network, demand-paged over a 2 km grid) — one national map
with a real sense of place that still first-paints in seconds.

## where the two experiments stand

**gdn-gis** (`web/map.html`, `src/map.rs`) already solved the streaming problem
for one heavy layer. the 155 MB detail blob (`map.f32`, 6.45 M segments) is
binned by 2 km cell into a 180×180 grid over the licence area (E 300–660 km,
N 156–516 km, BNG), sorted so each cell is one contiguous byte range; the
client prefix-sums `map.idx`, issues coalesced HTTP range requests for in-view
cells only, LRU-evicts above 700 resident cells, and keeps a 9 MB
RDP-simplified trunk skeleton (`map.base.f32`) always resident for the
zoomed-out view. flat 24-byte instance records go straight from disk to GPU
buffer; one instanced line-list draw per cell. no libraries, no build step,
2D top-down.

**sheffield** (`gpu.js`, `proj.js`, `app.js`) solved the *look*: EA 1 m
composite DTM streamed from the open WCS, resampled and packed to a single
int16-decimetre height grid (6.5 MB for 32×23 km); OSM footprints + heights
packed to a quantised binary (22 MB, 175 k buildings) and drawn as
base-edge / roof-edge / riser wireframes; an orbit camera (pan, dolly-to-
cursor, pitch, twist); everything 1 px hairlines on black, one draw call per
layer. but it loads *everything up front* — no tiling, no LOD, no culling —
which is exactly what cannot scale 30× to the cadent footprint.

so the synthesis is mechanical rather than inventive: **keep sheffield's
aesthetic and camera, and put every heavy layer behind gdn-gis's
base-plus-paged-cells contract.**

## can the user download it all? (no — but the *base* can be tiny)

back-of-envelope at cadent extent (360×360 km, 32 400 cells, though only
~15 k cells contain network):

| layer | naive full download | with the tiered scheme |
|---|---|---|
| pipes | 155 MB | 9 MB resident base + ranged cells (already shipped) |
| terrain | EA 1 m everywhere ≈ 250 GB | 6.5 MB resident 200 m grid + 3–120 KB/cell detail |
| buildings | ~5 M footprints ≈ 300–600 MB | nothing resident + ~50–200 KB/cell, deepest zoom only |

first load = html + pipe base + coarse national terrain ≈ **≤ 10 MB**
(pre-brotli'd, since resident files are whole-file fetches and compress 2–3×).
after that, panning at street zoom pulls per-cell ranges comparable in weight
to an ordinary slippy-map's PNG tiles. that is the whole answer: nothing
clever, just the existing paging contract applied three more times.

## the unified artifact contract

every layer becomes *(base, blob, idx)* against the **same** 180×180 grid from
`map.json` — one spatial index to rule all layers, one paging code path in the
client, one LRU keyed by (layer, cell). two shapes of blob:

- **variable-record layers** (pipes, buildings): sorted-by-cell blob + u32
  count index, exactly as `map.f32`/`map.idx` today.
- **fixed-record layers** (terrain): every populated cell is the same size, so
  `offset = rank(cell) × cellbytes` — the idx degenerates to a presence bitmap
  + rank, and a cell fetch is a single arithmetic range.

each layer carries its own scale threshold in `map.json` (pipes detail at
s ≥ 1/12 as now; terrain detail somewhat later; buildings last, at
street-level s). the client's existing `update()` loop — visible cell rect →
missing cells → coalesced range fetches → GPU buffers — just iterates layers.

## terrain: two tiers, one format

cell format: row-major **u16 decimetres** height posts (sheffield's proven
encoding, minus its per-file header — grid dims are constants per tier),
uploaded as an `r16uint` texture per cell, plus one resident coarse texture.

- **tier 0 (resident): OS terrain 50.** open data, one small national
  download, resampled to 200 m posts over the full grid → 1800×1800×2 B =
  6.5 MB (≈2.5 MB brotli'd). this is the always-there relief *and* the
  elevation authority every other layer drapes against at low zoom.
- **tier 1 (paged): EA 1 m composite DTM, resampled to ~8–16 m.** fetched by
  a script that scales sheffield's `lidar.py` WCS-block streaming to the
  licence area (populated cells only). at 16 m posts a cell is 125×125×2 B =
  31 KB; at 8 m, 122 KB. sheffield's terrain wire renders at an effective
  ~28 m stride, so even the 16 m tier is *richer* than the thing we're trying
  to match. ~15 k populated cells → 0.5–1.8 GB hosted; fetched once,
  server-side, at build time.

sequencing hedge: tier 0 alone (with 50 m posts per cell as an interim
tier 1) already buys the hills for pennies — OS terrain 50 native resolution
per cell is 40×40×2 B = 3.2 KB, 104 MB for the *entire* grid. ship that
first; swap the EA tier in behind the identical cell contract when the WCS
crawl completes. the client cannot tell the difference except in the data.

**draping for free:** pipes stay 2D 24-byte records. the vertex shader lifts
z by sampling the terrain texture at the endpoint (coarse texture, or the
cell's detail texture when resident) — so the *existing* pipe pipeline gains
3D without a format change, and buried-depth offset (sheffield's gas layer
trick) is one uniform. buildings sit on terrain the same way, which kills
sheffield's biggest CPU cost (per-vertex `terr.elev()` at fold time).

## buildings: OSM, packed per cell, drawn as instances

- **source:** geofabrik england `.osm.pbf` (one ~1.7 GB download) rather than
  30 abusive overpass megaqueries. extraction lives in the rust binary beside
  `map.rs` — footprints, `height` / `building:levels`×3 / 8 m default,
  `min_height` — one new module, one new dep (`osmpbf`), same
  fetch-by-script / parse-in-rust split as the pipe data.
- **encoding:** per-cell records with **u16 cell-local coords** (2 km / 65 536
  ≈ 3 cm posts — better than sheffield's int32 microdegrees at a quarter the
  bytes), heights as u8 half-metres. per *edge*: x0 y0 x1 y1 h base tone →
  12 B. ~5 M footprints × ~6 edges ≈ 360 MB blob, ranged, never resident,
  fetched only at the deepest zoom tier where a view covers a handful of
  cells.
- **rendering:** sheffield draws 3 segments per footprint edge (base, roof,
  riser). instead of expanding those on the CPU into a giant vertex buffer,
  keep the gdn-gis idiom: the 12 B edge record is the instance, and the
  vertex shader emits the wireframe from `vertex_index` (6 verts: base pair,
  roof pair, riser pair) with z from the terrain sample. one instanced draw
  per resident cell, straight from disk bytes, zero CPU tessellation.
- click metadata (names/tags) follows the `works.tsv` pattern: a sidecar
  fetched lazily per pick, never with the geometry.

## camera and renderer

port sheffield's orbit camera (`proj.js`) into the map's uniform: replace the
2D `ctr/s/k` transform with a mat4x4 view-projection in BNG-km world space.
far out, pitch clamps to top-down and the map behaves exactly as today; past
the detail threshold the pitch clamp opens and the wheel becomes
dolly-to-cursor. sheffield proves the hairline aesthetic needs **no depth
buffer, no lighting, no surface meshes** — lines composite fine on black — so
the render pass stays as cheap as the current one. pipeline count lands
around six (pipes ×2 passes, incidents, tooltip, terrain wire, building
wire), all one shared shader module in the single html file.

what deliberately stays: no map library, no framework, no build step, no npm,
data as dumb static files, the 110 ms fetch debounce, the LRU cap, CPU
picking (150 k incidents brute-forces fine; buildings pick only against the
handful of resident cells).

## costs and constraints worth naming

- **hosting:** range requests are load-bearing. github pages caps files at
  100 MB (`map.f32` already exceeds it); the blobs want an
  s3-compatible/r2-style public bucket, which serves ranges natively. resident
  files get pre-compressed (`.br`) copies; ranged blobs are served raw —
  quantisation *is* their compression (ranges and transfer-encoding don't
  compose).
- **pipe format v2 (optional, big win):** re-cutting `map.f32` records to the
  same u16 cell-local scheme as buildings (12 B vs 24 B) halves the detail
  blob to ~77 MB and doubles effective cache capacity, at the cost of one
  breaking format bump while there are zero users. do it in the same change
  that generalises `map.rs` into a multi-layer emitter.
- **EA crawl:** the WCS is genteel; budget hours-to-days for the licence-area
  crawl and checkpoint per cell (the fetch scripts already skip
  already-fetched months for streetworks — same idiom).
- **empty-cell truthfulness:** terrain presence-bitmap cells with no pipes
  simply don't exist; the client must treat absent as flat-at-coarse, not
  error.

## build order

1. **format v2 + multi-layer emitter** — generalise `map.rs` cell binning to
   emit *(base, blob, idx)* per layer; quantise pipe records to 12 B.
2. **terrain tier 0 + 3D camera** — OS terrain 50 fetch script + packer;
   mat4 uniform, orbit controls, terrain wire grid, pipe draping. this is the
   moment the map stops being flat.
3. **buildings** — pbf extract → per-cell edge blob; instanced wireframe
   pipeline; lazy tag sidecar for picking.
4. **terrain tier 1** — EA WCS crawl into the identical cell contract.
5. **deploy** — bucket with range support, brotli'd residents, cache-control
   immutable + content-hashed filenames.

each step ships a working map; nothing blocks on the EA crawl.
