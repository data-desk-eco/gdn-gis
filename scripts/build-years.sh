#!/bin/sh -e
# laid-year sidecar from cadent's open gpi data: every pipe vertex binned to a
# 100 m grid over the map extent, median install year per (cell, material code)
# plus an any-material '*' fallback per cell -> data/years.tsv (cell mat year).
# map.rs stamps each emitted segment from this (midpoint cell + tone, ring search).
duckdb -c "install spatial; load spatial;
copy (
  with p as (
    select material, year(inst_date) y,
      unnest(st_dump(st_points(st_transform(geo_shape,'EPSG:4326','EPSG:27700',true)))).geom g
    from 'data/gas-pipe-infrastructure-gpi_open.parquet'
    where inst_date is not null and year(inst_date) between 1850 and 2026),
  c as (select material, y, floor((st_x(g)-300000)/100)::int cx, floor((st_y(g)-156000)/100)::int cy from p
        where st_x(g) between 300000 and 659999 and st_y(g) between 156000 and 515999)
  select cx+3600*cy cell, material mat, median(y)::int y from c group by 1,2
  union all
  select cx+3600*cy, '*', median(y)::int from c group by 1,2
) to 'data/years.tsv' (delimiter '\t', header false);"
wc -l data/years.tsv
