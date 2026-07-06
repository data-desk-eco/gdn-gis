# gdn-gis

`gdn-gis` reverse-engineers the vector map data shipped inside the Cadent /
National Gas **MAPS Viewer** Windows distribution and assembles it into a single
geoparquet of the Cadent network (~2.31 million pipes, ~140,000 km
of main), plus the artefacts for a WebGPU map of the whole network. All sources
are fetched automatically — nothing is downloaded by hand.

## Usage

Run everything from the repo root; paths in `config.toml` are relative to it.

### 1. Fetch the sources

```sh
scripts/fetch-maps.sh data/maps-viewer.zip USER PASS   # MAPS Viewer bundle (DNV Veracity login)
scripts/fetch-works.sh data/streetworks                # Street Manager permit archive (incidents)
scripts/fetch-terrain.sh                               # OS Terrain 50 (national relief)
scripts/fetch-lidar.py                                 # EA 1 m LiDAR DTM (resumable, ~1 h)
scripts/fetch-buildings.sh                             # Geofabrik England OSM extract
scripts/fetch-basemap.py                               # GB coastline (ONS) + place names (OS Open Names)
scripts/build-years.sh                                 # laid-year sidecar (needs duckdb)
```

`USER` / `PASS` are your Veracity account credentials. `fetch-lidar.py` and
`fetch-works.sh` are checkpointed — rerun to resume or pick up new data.

### 2. Build

```sh
cargo build --release
./target/release/gdn-gis                # extract the bundle -> geoparquet + map artefacts
./target/release/gdn-gis --works        # rebuild just the incident layer
./target/release/gdn-gis --terrain      # rebuild the relief tiers from the fetched DTM
./target/release/gdn-gis --buildings    # rebuild the buildings layer from the pbf
```

Options:

```
gdn-gis [CONFIG.toml] [ZIP] [options]
  CONFIG.toml   extraction config       (default config.toml)
  ZIP           tile archive            (overrides config `zip`)
  -o FILE       output geoparquet       (overrides config `output`)
  -p PASSWORD   zip password            (overrides config `password`)
  --square SK   only this 100 km square (debug)
  --limit N     only the first N tiles  (debug)
  -j N          worker threads          (default: all cores)
```

The full extraction (2.7 GB, 168k tiles) takes ~30 s.

### 3. View the map

`web/map.html` (plus its ES modules alongside — no build step) is a WebGPU map
of the whole network. WebGPU needs
http(s), and the map's per-cell fetches need HTTP range requests, so serve it
with the bundled range-capable server:

```sh
python3 scripts/serve.py               # then open http://localhost:8000/web/map.html
```

## Output

`dist/cadent_gas_network.parquet` — GeoParquet 1.1, geometry in **EPSG:27700**
(OSGB36 / British National Grid). One row per pipe, stitched from the per-tile
fragments of each OS feature. Key columns: `feature_id`, `pressure` /
`pressure_code`, `diameter_mm`, `material`, `inserted` (+ `host_*` for relined
mains), `network_area`, `length_m`, `survey_date`, `geometry` (WKB). See
`config.toml` for the full schema.

Alongside it, the extractor writes the WebGPU map artefacts (`dist/map.*`,
`terr*.bin`, `bldg.*`, `works.*`) served by `web/map.html`;
`scripts/fetch-basemap.py` adds the basemap pair (`coast.u16`, `places.tsv`)
directly to `dist/`.
