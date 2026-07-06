#!/bin/sh -e
# national gas transmission nts: pipe corridors + site boundaries (shapefile
# zips published on nationalgas.com, bng) -> data/nts.tsv (same schema as
# sgn.tsv: wkt \t NTS \t ST \t) and data/nts-sites.tsv (wkt \t facility \t location).
# new editions appear under dated names — pass fresh urls as $1 $2 when stale.
pipe=${1:-https://www.nationalgas.com/sites/default/files/documents/Gas_Pipe_03062026_0.zip}
site=${2:-https://www.nationalgas.com/sites/default/files/documents/Gas_Site_03062026.zip}
d=data/nts; mkdir -p $d
[ -s $d/Gas_Pipe.zip ] || curl -fsSL --retry 3 -o $d/Gas_Pipe.zip "$pipe"
[ -s $d/Gas_Site.zip ] || curl -fsSL --retry 3 -o $d/Gas_Site.zip "$site"
unzip -oq $d/Gas_Pipe.zip -d $d/pipe && unzip -oq $d/Gas_Site.zip -d $d/site
ogr2ogr -f CSV /vsistdout/ "$(find $d/pipe -name '*.shp')" -select PIPE_NAME \
  -lco GEOMETRY=AS_WKT -lco SEPARATOR=TAB -xyRes 0.1 2>/dev/null |
  tail -n +2 | awk -F'\t' -v OFS='\t' '{print $1,"NTS","ST",""}' > data/nts.tsv
ogr2ogr -f CSV /vsistdout/ "$(find $d/site -name '*.shp')" -select FACILITY,LOCATION \
  -dim XY -lco GEOMETRY=AS_WKT -lco SEPARATOR=TAB -xyRes 0.1 2>/dev/null |
  tail -n +2 > data/nts-sites.tsv
wc -l data/nts.tsv data/nts-sites.tsv
