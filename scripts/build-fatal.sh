#!/bin/sh -e
# fatal layer: hand-rolled dataset of fatal gas-main explosions (scripts/
# fatal.csv, curated from the gas-pipe-risk research — deaths >= 1 only).
# writes, for incidents inside the map grid (read from dist/map.json, so the
# scottish and swansea incidents appear once the grid goes national):
#   dist/fatal.f32  x y (bng km) 0 3 per incident — map.html appends these to
#                   the works instance buffer (flag 3 = fatal, timeless)
#   dist/fatal.tsv  same order: name, place, year, deaths, injuries
duckdb -csv -c "install spatial; load spatial;
with m as (select minx x0, miny y0, minx+ncols*cell x1, miny+nrows*cell y1
  from 'dist/map.json')
select round(st_x(p)/1000,3) x, round(st_y(p)/1000,3) y,
  name, place, year, deaths, coalesce(injuries,0) injuries
from (select st_transform(st_point(lon,lat),'EPSG:4326','EPSG:27700',
  always_xy := true) p, * from read_csv('scripts/fatal.csv')), m
where st_x(p)/1000 between x0 and x1 and st_y(p)/1000 between y0 and y1
order by year" | python3 -c "
import csv,struct,sys
rows=list(csv.reader(sys.stdin))[1:]
open('dist/fatal.f32','wb').write(b''.join(struct.pack('<4f',float(x),float(y),0,3) for x,y,*_ in rows))
open('dist/fatal.tsv','w').write(''.join('\t'.join(r[2:])+'\n' for r in rows))
print(f'  fatal: {len(rows)} -> dist/fatal.*')"
