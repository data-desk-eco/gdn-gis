#!/bin/sh -e
# fetch os terrain 50 (open data, ascii grid) for the tier-0 relief.
#   scripts/fetch-terrain.sh [OUT.zip]
out=${1:-data/terr50.zip}
[ -s "$out" ] && { echo "already fetched: $out"; exit 0; }
curl -fL --retry 3 -o "$out" \
  'https://api.os.uk/downloads/v1/products/Terrain50/downloads?area=GB&format=ASCII+Grid+and+GML+(Grid)&redirect'
echo "-> $out"
