# maps-viewer geospatial extraction

extracts the vector map data shipped inside the cadent / national grid **MAPS
Viewer** windows distribution (`MapsViewerApril2026.zip`) into geoparquet.

## what's in the distribution

the viewer is an old (mfc / msxml-era) windows gis. its geospatial data lives in
**168,458 `.mvf` tiles** under `DATA/GAS/NG/<square>/`, one tile per 500 m cell of
the ordnance survey british national grid, covering 15 100 km squares across the
cadent gas-distribution licence area. the bundled dlls (`MVFInterpreter.dll`,
`MVFTile.dll`, `nmedw1.dll`, `CoordinateMapper.dll`) are only the *interpreter* —
the data itself is in the tiles.

## the `.mvf` format (reverse-engineered)

each `.mvf` is an **obfuscated webcgm** (iso 8632 binary cgm) graphic:

| region | encoding |
|---|---|
| metafile header (metadata strings, layer name, vdc extent) | `xor 0xff` then **swap adjacent byte pairs** → readable webcgm |
| picture body (the map linework) | **plain** big-endian binary cgm |

* the header carries `ProfileId:WebCGM`, `Source`, `Date`, `Facet`, and the tile's
  british national grid bounds `Xmin/Ymin/Xmax/Ymax` (epsg:27700).
* the body is a long run of `POLYLINE` primitives (class 4 / id 1) grouped by
  `EXTERN` application-data markers carrying a layer code and an os feature
  object-id, all in a virtual device coordinate space `(0,0)-(16000,16000)`.
* vdc coords map linearly onto the tile's bng bounds:
  `easting = Xmin + vx/16000 · (Xmax-Xmin)`, likewise northing.

the zip is aes-256 encrypted; the password is passed with `-p` (or hard-coded
default in the script).

## usage

```sh
uv run extract_mvf.py MapsViewerApril2026.zip -o output      # all 15 squares
uv run extract_mvf.py --square SK --limit 100 -o /tmp/test   # a quick subset
```

output: one geoparquet per 100 km square, `output/NG_<square>.parquet`, geometry
in **epsg:27700** (osgb36 / british national grid). columns: `square`, `facet`,
`source`, `survey_date`, `layer`, `feat_class`, `object_id`, `etype`, `geometry`.

## result

113,723,758 line/polygon features extracted; every square's geometry validates
into its correct national-grid 100 km square.
