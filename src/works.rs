// streetworks incident artefacts. reads the dft street-manager monthly ndjson.gz
// archives mirrored by gas-pipe-risk's streetworks.sh, keeps the configured
// promoter's immediate-category permits (latest event per permit wins — it carries
// actual dates and final status; an immediate permit is a dig the network could not
// plan, the open proxy for an escape/emergency repair), and writes two aligned
// files beside the gpu-map artefacts:
//
//   works.f32  one record per permit: x y (bng km, polygon centroid) day (unix
//              days) flag (1 = emergency, 0 = urgent). sorted by day then permit.
//   works.tsv  same order: permit, category, status, street, town, authority,
//              start, end, traffic management, location type — fetched lazily by
//              map.html for the click card. permits whose final permit_status is
//              cancelled/refused/revoked (digs that never happened) are dropped.
//   works.parquet  the same permits as geoparquet (full-precision bng point,
//              typed dates) for local duckdb analysis.

use std::collections::{hash_map::Entry, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryArray, BooleanArray, Date32Array, StringArray};
use flate2::read::MultiGzDecoder;
use rayon::prelude::*;
use regex::Regex;
use serde::Deserialize;

use crate::map::MapCfg;

#[derive(Deserialize)]
pub struct WorksCfg {
    dir: String,      // where fetch-works.sh streamed the *.ndjson.gz archive
    promoter: String, // regex over promoter_organisation
    #[serde(default = "d_cat")]
    category: String, // work_category_ref prefix
}
fn d_cat() -> String {
    "immediate".into()
}

#[derive(Deserialize)]
struct Ev {
    permit_reference_number: Option<String>,
    promoter_organisation: Option<String>,
    works_location_coordinates: Option<String>,
    street_name: Option<String>,
    town: Option<String>,
    highway_authority: Option<String>,
    work_category: Option<String>,
    work_category_ref: Option<String>,
    work_status: Option<String>,
    permit_status: Option<String>,
    traffic_management_type: Option<String>,
    works_location_type: Option<String>,
    proposed_start_date: Option<String>,
    proposed_end_date: Option<String>,
    actual_start_date_time: Option<String>,
    actual_end_date_time: Option<String>,
    event_time: Option<String>,
}

/// centroid of every coordinate pair in a wkt string (bng metres).
fn centroid(wkt: &str) -> Option<(f64, f64)> {
    let v: Vec<f64> = wkt
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter_map(|t| t.parse().ok())
        .collect();
    let n = v.len() / 2;
    (n > 0).then(|| (v.iter().step_by(2).sum::<f64>() / n as f64, v.iter().skip(1).step_by(2).sum::<f64>() / n as f64))
}

/// unix day number from an iso date prefix (howard hinnant's civil algorithm).
fn day(s: &str) -> i32 {
    let p = |a, b| s.get(a..b).and_then(|t| t.parse::<i64>().ok()).unwrap_or(0);
    let (mut y, m, d) = (p(0, 4), p(5, 7), p(8, 10));
    if m <= 2 {
        y -= 1;
    }
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    (era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468) as i32
}

pub fn write(w: &WorksCfg, m: &MapCfg, crs: &str) {
    let prom = Regex::new(&w.promoter).unwrap();
    let files: Vec<_> = std::fs::read_dir(&w.dir)
        .expect("works dir")
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.to_string_lossy().ends_with(".ndjson.gz"))
        .collect();

    // latest event per permit, filtered to the promoter's immediate digs
    let best = files
        .par_iter()
        .fold(HashMap::new, |mut b: HashMap<String, Ev>, p| {
            for ln in BufReader::new(MultiGzDecoder::new(File::open(p).unwrap())).lines().map_while(Result::ok) {
                let e: Ev = match serde_json::from_str(&ln) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !e.work_category_ref.as_deref().unwrap_or("").starts_with(&w.category)
                    || !prom.is_match(e.promoter_organisation.as_deref().unwrap_or(""))
                {
                    continue;
                }
                let k = match &e.permit_reference_number {
                    Some(k) => k.clone(),
                    None => continue,
                };
                match b.entry(k) {
                    Entry::Vacant(v) => {
                        v.insert(e);
                    }
                    Entry::Occupied(mut o) => {
                        if e.event_time > o.get().event_time {
                            o.insert(e);
                        }
                    }
                }
            }
            b
        })
        .reduce(HashMap::new, |mut a, b| {
            for (k, v) in b {
                match a.entry(k) {
                    Entry::Vacant(e) => {
                        e.insert(v);
                    }
                    Entry::Occupied(mut e) => {
                        if v.event_time > e.get().event_time {
                            e.insert(v);
                        }
                    }
                }
            }
            a
        });

    let mut rows: Vec<([f64; 2], i32, bool, [String; 10])> = best
        .into_values()
        .filter_map(|e| {
            if matches!(e.permit_status.as_deref(), Some("cancelled" | "refused" | "revoked")) {
                return None;
            }
            let (x, y) = centroid(e.works_location_coordinates.as_deref()?)?;
            let start = e.actual_start_date_time.clone().or(e.proposed_start_date.clone()).unwrap_or_default();
            let end = e.actual_end_date_time.clone().or(e.proposed_end_date.clone()).unwrap_or_default();
            let em = e.work_category_ref.as_deref().unwrap_or("").contains("emergency");
            let f = |s: Option<String>| s.unwrap_or_default().replace(['\t', '\n'], " ").trim().to_string();
            Some((
                [x, y],
                day(&start),
                em,
                [
                    f(e.permit_reference_number),
                    f(e.work_category),
                    f(e.work_status),
                    f(e.street_name),
                    f(e.town),
                    f(e.highway_authority),
                    start.get(..10).unwrap_or("").into(),
                    end.get(..10).unwrap_or("").into(),
                    f(e.traffic_management_type),
                    f(e.works_location_type),
                ],
            ))
        })
        .collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.3[0].cmp(&b.3[0])));

    let mut wf = BufWriter::new(File::create(format!("{}/works.f32", m.dir)).unwrap());
    let mut wt = BufWriter::new(File::create(format!("{}/works.tsv", m.dir)).unwrap());
    let n = rows.len();
    let mut nem = 0usize;
    let (mut sd, mut ed): (Vec<Option<i32>>, Vec<Option<i32>>) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut geom = Vec::with_capacity(n);
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for ([x, y], d, em, det) in &rows {
        for v in [(*x / 1000.0) as f32, (*y / 1000.0) as f32, *d as f32, *em as u8 as f32] {
            wf.write_all(&v.to_le_bytes()).unwrap();
        }
        writeln!(wt, "{}", det.join("\t")).unwrap();
        nem += *em as usize;
        sd.push((det[6].len() == 10).then(|| day(&det[6])));
        ed.push((det[7].len() == 10).then(|| day(&det[7])));
        let mut g = vec![1u8];
        g.extend(1u32.to_le_bytes());
        g.extend(x.to_le_bytes());
        g.extend(y.to_le_bytes());
        geom.push(g);
        (lo[0], lo[1], hi[0], hi[1]) = (lo[0].min(*x), lo[1].min(*y), hi[0].max(*x), hi[1].max(*y));
    }
    wf.flush().unwrap();
    wt.flush().unwrap();

    let s = |i: usize| Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.3[i].as_str()))) as ArrayRef;
    let cols = vec![
        ("permit".into(), s(0)),
        ("category".into(), s(1)),
        ("emergency".into(), Arc::new(BooleanArray::from_iter(rows.iter().map(|r| Some(r.2)))) as _),
        ("status".into(), s(2)),
        ("street".into(), s(3)),
        ("town".into(), s(4)),
        ("authority".into(), s(5)),
        ("start_date".into(), Arc::new(Date32Array::from(sd)) as _),
        ("end_date".into(), Arc::new(Date32Array::from(ed)) as _),
        ("traffic_management".into(), s(8)),
        ("location_type".into(), s(9)),
        ("geometry".into(), Arc::new(BinaryArray::from_iter_values(geom.iter())) as _),
    ];
    crate::geoparquet(&format!("{}/works.parquet", m.dir), vec![cols], r#""Point""#, crs, [lo[0], lo[1], hi[0], hi[1]]);
    eprintln!("  works: {} immediate digs ({} emergency)  ->  {}/works.*", n, nem, m.dir);
}
