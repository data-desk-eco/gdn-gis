#!/bin/sh -e
# sites layer: static gas installations (agis, compressor stations, terminals)
# from national gas transmission's network route maps shapefile (updm
# siteboundary export, bng; the dated Gas_Site zip is scraped off the page).
# cadent's own above-ground datasets on the open data portal are login-gated
# (anonymous exports come back empty), so the nts sites are the open source.
# writes, for sites whose boundary centroid falls inside the map grid
# (e 300-660, n 156-516 km — see config.toml [map]):
#   dist/sites.f32  x y (bng km) 0 2 per site — map.html appends these to the
#                   works instance buffer (flag 2 = site, day 0 = timeless)
#   dist/sites.tsv  same order: name, kind — the click-card sidecar
d=data/gas-sites
if ! [ -f $d/Gas_Site.shp ]; then
  page=https://www.nationalgas.com/land-and-assets/network-route-maps
  url=$(curl -sL -A Mozilla/5.0 $page | grep -om1 '/sites/default/files/documents/Gas_Site[^"]*\.zip')
  mkdir -p $d && curl -sL -A Mozilla/5.0 "https://www.nationalgas.com$url" -o $d/sites.zip
  unzip -joq $d/sites.zip -d $d && rm $d/sites.zip
fi
duckdb -csv -c "install spatial; load spatial;
select round(st_x(c)/1000,3) x, round(st_y(c)/1000,3) y,
  lower(trim(regexp_replace(replace(location,'_',' '),' [0-9]+$',''))) nm,
  case facility when 'COMP' then 'compressor station'
    when 'AGI' then 'above-ground installation' else 'nts site' end kind
from (select st_centroid(geom) c, facility, location from st_read('$d/Gas_Site.shp'))
where location is not null and st_x(c) between 300000 and 660000
  and st_y(c) between 156000 and 516000 order by 3" | python3 -c "
import struct,sys
rows=[l.rstrip('\n').split(',') for l in sys.stdin][1:]
open('dist/sites.f32','wb').write(b''.join(struct.pack('<4f',float(x),float(y),0,2) for x,y,*_ in rows))
open('dist/sites.tsv','w').write(''.join('\t'.join(r[2:])+'\n' for r in rows))
print(f'  sites: {len(rows)} -> dist/sites.*')"
