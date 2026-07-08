#!/bin/sh -e
# push dist artefacts to the public bucket. range-served blobs and parquet go
# raw (gcs gzip transcoding ignores Range; parquet is already compressed);
# everything else is a whole-file resident fetch and uploads gzip-transcoded
# (-Z, stored with content-encoding: gzip) for the 2-3x wire saving.
# usage: scripts/sync-data.sh [file...]   (defaults to all of dist/)
cd "$(dirname "$0")/.."
[ $# -gt 0 ] || set -- dist/*
# no-cache: browsers revalidate by etag instead of serving hour-old blobs
for f; do case "${f##*/}" in
  map.bin|terr1.bin|bldg.bin|roof.bin|bldg.tsv|*.parquet) gcloud storage cp --cache-control=no-cache "$f" gs://gdn-gis-data/;;
  *) gcloud storage cp -Z --cache-control=no-cache "$f" gs://gdn-gis-data/;;
esac; done
