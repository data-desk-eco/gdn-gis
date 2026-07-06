#!/bin/sh -e
# sgn open-data pipe network: every per-authority "distribution pipe network"
# geopackage in sgn's arcgis hub group -> data/sgn/*.gpkg (already-fetched files
# skipped), then the mains merged to one bng tsv -> data/sgn.tsv:
#   wkt \t pressure \t material \t inst_date
dir=${1:-data/sgn}; mkdir -p "$dir"
hub='https://hub.arcgis.com/api'
q="filter=((group%20IN%20(6992e8fd00b740eb9e5a9c7212384d85)))"
ref='Referer: https://open-data-sharing-portal-sgn-uk.hub.arcgis.com/'
total=$(curl -fsS -H "$ref" "$hub/search/v1/collections/all/items?$q&limit=1" |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["numberMatched"])')
i=1; while [ "$i" -le "$total" ]; do
  curl -fsS -H "$ref" "$hub/search/v1/collections/all/items?$q&limit=100&startindex=$i" |
    python3 -c 'import json,sys
for f in json.load(sys.stdin)["features"]:
    p = f["properties"]
    if p.get("type") == "Feature Service" and p["title"].startswith("Distribution Pipe Network"):
        print(f["id"] + "|" + p["title"][25:].strip().lower().replace(" ", "-"))'
  i=$((i+100))
done | sort -u | while IFS='|' read -r id slug; do
  out="$dir/$slug.gpkg"
  [ -s "$out" ] && continue
  # poll the export cache until ready; a few items only offer shapefile
  for fmt in geoPackage shapefile; do
    url=''; n=0
    while [ -z "$url" ] && [ $n -lt 40 ]; do
      r=$(curl -fsS -H "$ref" "$hub/download/v1/items/$id/$fmt?redirect=false&layers=0") || r=''
      case "$r" in *Unsupported*) break ;; esac
      url=$(echo "$r" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("resultUrl") or "")' 2>/dev/null) || true
      [ -n "$url" ] || { n=$((n+1)); sleep 10; }
    done
    [ -n "$url" ] || continue
    if [ "$fmt" = geoPackage ]; then
      curl -fsSL --retry 3 -o "$out.part" "$url" && mv "$out.part" "$out"
    else
      t=$(mktemp -d)
      curl -fsSL --retry 3 -o "$t/z.zip" "$url" && unzip -oq "$t/z.zip" -d "$t" &&
        ogr2ogr -f GPKG "$out" "$(find "$t" -name '*.shp' | head -1)"
      rm -rf "$t"
    fi
    [ -s "$out" ] && { echo "-> $out ($fmt)"; break; }
  done
  [ -s "$out" ] || echo "!! $slug: no export available" >&2
done
# merge mains (service pipes dropped, as in the cadent extract) to one tsv
: > data/sgn.tsv
for g in "$dir"/*.gpkg; do
  ogr2ogr -f CSV /vsistdout/ "$g" -t_srs EPSG:27700 -where "TYPE='Main Pipe'" \
    -select PRESSURE,MATERIAL,INST_DATE -lco GEOMETRY=AS_WKT -lco SEPARATOR=TAB \
    -xyRes 0.1 2>/dev/null | tail -n +2 >> data/sgn.tsv
done
wc -l data/sgn.tsv
