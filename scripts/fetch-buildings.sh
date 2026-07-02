#!/bin/sh -e
# fetch the geofabrik england extract for the buildings layer.
#   scripts/fetch-buildings.sh [OUT.osm.pbf]
out=${1:-data/england-latest.osm.pbf}
[ -s "$out" ] && { echo "already fetched: $out"; exit 0; }
curl -fL --retry 3 -o "$out" https://download.geofabrik.de/europe/united-kingdom/england-latest.osm.pbf
echo "-> $out"
