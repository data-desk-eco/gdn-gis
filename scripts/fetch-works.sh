#!/bin/sh
# stream the dft street manager archived permit notifications (open s3 bucket, ogl
# v3) and keep only gas-transporter events, slimmed to object_data + event metadata.
# an emergency permit by a gas network is an open, located, dated proxy for an
# escape/repair dig. nothing big touches disk; re-runs skip months already fetched.
# the monthly zips (~0.1-1gb each) have no central directory, so bsdtar (streaming)
# is required — unzip/python zipfile fail on them.
#
# usage: fetch-works.sh DIR [BUCKET] [MATCH]
set -e
dir=${1:?dir}
base=${2:-https://opendata.manage-roadworks.service.gov.uk}
match=${3:-cadent|gas|wales and west|fulcrum}
mkdir -p "$dir"
export base match dir
curl -s "$base/?list-type=2&prefix=permit/" \
  | grep -oE 'permit/[0-9]{4}/[0-9]{2}\.zip' | sort -u \
  | xargs -P3 -n1 sh -c '
    out=$dir/$(echo "$1" | tr -cd 0-9).ndjson.gz
    [ -s "$out" ] && exit 0
    curl -s "$base/$1" | bsdtar -xOf - 2>/dev/null \
      | jq -c "select(.object_data.promoter_organisation
                      | test(\"$match\"; \"i\"))
               | .object_data + {event: .event_type, event_time: .event_time}" \
      | gzip > "$out.tmp" && mv "$out.tmp" "$out" && echo "$1"' _
