// single-pass extractor for the cadent / national-grid "maps viewer" distribution.
//
// decodes the obfuscated webcgm .mvf tiles, keeps only the gas-asset linework
// (the "... mains & plant" layers), and dissolves every pipe's per-tile
// fragments into one coherent line by os feature id as it goes — writing a
// single distribution-ready geoparquet (epsg:27700) with no intermediate.
//
// usage: mvf-extract [ZIP] [-o OUT.parquet] [-p PASSWORD] [--square SK] [--limit N] [-j JOBS]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use rayon::prelude::*;
use regex::bytes::Regex;
use zip::ZipArchive;

use arrow::array::{ArrayRef, BinaryArray, BooleanArray, Float64Array, StringArray};
use arrow::datatypes::{Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

// cgm class-4 graphical primitives we treat as geometry
const POLYLINE: u8 = 1;
const DISJOINT: u8 = 2;
const POLYGON: u8 = 7;
// webcgm class-0 application-structure delimiters
const APS_BEGIN: u8 = 21;
const APS_END: u8 = 23;
const END_META: u8 = 2;
const END_PIC: u8 = 5;
const VDC_DEFAULT: i64 = 16000;

static ZIP_PATH: OnceLock<String> = OnceLock::new();
static PASSWORD: OnceLock<Vec<u8>> = OnceLock::new();

// ---------------------------------------------------------------- cgm decode

/// xor 0xff + swap adjacent byte pairs -> readable header bytes.
fn deob(d: &[u8]) -> Vec<u8> {
    let mut x: Vec<u8> = d.iter().map(|b| b ^ 0xFF).collect();
    let mut i = 0;
    while i + 1 < x.len() {
        x.swap(i, i + 1);
        i += 2;
    }
    x
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// read one big-endian cgm command at p: (class, id, par_start, par_end, next_p).
fn read_cmd(d: &[u8], p: usize) -> Option<(u8, u8, usize, usize, usize)> {
    if p + 2 > d.len() {
        return None;
    }
    let cw = u16::from_be_bytes([d[p], d[p + 1]]);
    let cl = (cw >> 12) as u8;
    let eid = ((cw >> 5) & 0x7F) as u8;
    let mut ln = (cw & 0x1F) as usize;
    let mut q = p + 2;
    if cl > 9 {
        return None;
    }
    if ln == 31 {
        if q + 2 > d.len() {
            return None;
        }
        let l = u16::from_be_bytes([d[q], d[q + 1]]);
        q += 2;
        if l & 0x8000 != 0 {
            return None; // partitioned data not used by this writer
        }
        ln = (l & 0x7FFF) as usize;
    }
    let end = q + ln;
    if end > d.len() {
        return None;
    }
    Some((cl, eid, q, end, end + (ln & 1)))
}

/// vdc extent (class 2, id 6) from the deobfuscated header.
fn vdc_extent(db: &[u8]) -> (i64, i64) {
    let mut p = 0;
    while let Some((cl, eid, qs, qe, np)) = read_cmd(db, p) {
        if cl == 2 && eid == 6 && qe - qs == 8 {
            let r = |o: usize| i16::from_be_bytes([db[qs + o], db[qs + o + 1]]) as i64;
            let (x0, y0, x1, y1) = (r(0), r(2), r(4), r(6));
            let w = (x1 - x0).abs();
            let h = (y1 - y0).abs();
            return (if w == 0 { VDC_DEFAULT } else { w }, if h == 0 { VDC_DEFAULT } else { h });
        }
        if cl == 0 && eid == 4 {
            break; // begin picture body -> header done
        }
        p = np;
    }
    (VDC_DEFAULT, VDC_DEFAULT)
}

/// offset of the first plausible plain-cgm picture-body command (after header).
fn find_start(d: &[u8], db: &[u8]) -> Option<usize> {
    let lo = find(db, b"Ymax").map(|j| j + 12).unwrap_or(380);
    let mut s = lo;
    let hi = (lo + 4000).min(d.len());
    while s < hi {
        let (mut tot, mut ok, mut nprim, mut good, mut p, mut n) = (0u32, 0u32, 0u32, true, s, 0);
        while n < 25 {
            match read_cmd(d, p) {
                None => {
                    good = false;
                    break;
                }
                Some((cl, eid, qs, qe, np)) => {
                    if cl == 4 && (eid == POLYLINE || eid == DISJOINT || eid == POLYGON) {
                        nprim += 1;
                        let par = &d[qs..qe];
                        let m = par.len() / 2; // total int16 values
                        for i in (0..m.saturating_sub(1)).step_by(2) {
                            let x = i16::from_be_bytes([par[i * 2], par[i * 2 + 1]]) as i64;
                            let y = i16::from_be_bytes([par[i * 2 + 2], par[i * 2 + 3]]) as i64;
                            tot += 1;
                            if (-3000..=19000).contains(&x) && (-3000..=19000).contains(&y) {
                                ok += 1;
                            }
                        }
                    }
                    p = np;
                    n += 1;
                }
            }
        }
        if good && nprim >= 2 && tot >= 20 && (ok as f64) / (tot as f64) > 0.95 {
            return Some(s);
        }
        s += 2;
    }
    None
}

/// runs of >=2 printable ascii bytes.
fn runs(par: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < par.len() {
        if (0x20..=0x7e).contains(&par[i]) {
            let j = i + par[i..].iter().take_while(|&&b| (0x20..=0x7e).contains(&b)).count();
            if j - i >= 2 {
                out.push(&par[i..j]);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// decode int16 vdc pairs -> (easting, northing) in epsg:27700.
fn pts(par: &[u8], x0: f64, y0: f64, sx: f64, sy: f64) -> Vec<(f64, f64)> {
    let n = par.len() / 4;
    (0..n)
        .map(|k| {
            let x = i16::from_be_bytes([par[k * 4], par[k * 4 + 1]]) as f64;
            let y = i16::from_be_bytes([par[k * 4 + 2], par[k * 4 + 3]]) as f64;
            (x0 + x * sx, y0 + y * sy)
        })
        .collect()
}

// ------------------------------------------------------------------ geometry

enum Geom {
    Line(Vec<(f64, f64)>),
    Multi(Vec<Vec<(f64, f64)>>),
    Poly(Vec<(f64, f64)>),
}

fn node(p: (f64, f64)) -> (i64, i64) {
    ((p.0 * 1000.0).round() as i64, (p.1 * 1000.0).round() as i64)
}

/// stitch line fragments end-to-end at degree-2 nodes (st_linemerge-alike).
fn linemerge(frags: Vec<Vec<(f64, f64)>>) -> Vec<Vec<(f64, f64)>> {
    // drop empties and exact (direction-agnostic) duplicates
    let mut seen = std::collections::HashSet::new();
    let frags: Vec<Vec<(f64, f64)>> = frags
        .into_iter()
        .filter(|f| f.len() >= 2)
        .filter(|f| {
            let a: Vec<(i64, i64)> = f.iter().map(|&p| node(p)).collect();
            let mut b = a.clone();
            b.reverse();
            seen.insert(if a <= b { a } else { b })
        })
        .collect();
    let n = frags.len();
    let mut by: HashMap<(i64, i64), Vec<(usize, u8)>> = HashMap::new();
    for (i, f) in frags.iter().enumerate() {
        by.entry(node(f[0])).or_default().push((i, 0));
        by.entry(node(*f.last().unwrap())).or_default().push((i, 1));
    }
    let mut visited = vec![false; n];
    let mut out = Vec::new();
    for s in 0..n {
        if visited[s] {
            continue;
        }
        visited[s] = true;
        let mut chain: std::collections::VecDeque<(f64, f64)> = frags[s].iter().copied().collect();
        for dir in 0..2u8 {
            loop {
                let nd = node(if dir == 0 { *chain.back().unwrap() } else { *chain.front().unwrap() });
                let occ = match by.get(&nd) {
                    Some(o) if o.len() == 2 => o,
                    _ => break,
                };
                let cand = occ.iter().find(|&&(f, _)| !visited[f]).copied();
                let (f, end) = match cand {
                    Some(x) => x,
                    None => break,
                };
                visited[f] = true;
                // orient fragment to start at nd
                let mut seg = frags[f].clone();
                if end == 1 {
                    seg.reverse();
                }
                if dir == 0 {
                    chain.extend(seg.into_iter().skip(1));
                } else {
                    for p in seg.into_iter().skip(1) {
                        chain.push_front(p);
                    }
                }
            }
        }
        out.push(chain.into_iter().collect());
    }
    out
}

fn put_pt(b: &mut Vec<u8>, p: (f64, f64)) {
    b.extend_from_slice(&p.0.to_le_bytes());
    b.extend_from_slice(&p.1.to_le_bytes());
}
fn put_ls(b: &mut Vec<u8>, pts: &[(f64, f64)]) {
    b.push(1);
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for &p in pts {
        put_pt(b, p);
    }
}

fn wkb(g: &Geom) -> Vec<u8> {
    let mut b = Vec::new();
    match g {
        Geom::Line(p) => put_ls(&mut b, p),
        Geom::Multi(ps) => {
            b.push(1);
            b.extend_from_slice(&5u32.to_le_bytes());
            b.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            for p in ps {
                put_ls(&mut b, p);
            }
        }
        Geom::Poly(p) => {
            b.push(1);
            b.extend_from_slice(&3u32.to_le_bytes());
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&(p.len() as u32).to_le_bytes());
            for &pt in p {
                put_pt(&mut b, pt);
            }
        }
    }
    b
}

fn seglen(p: &[(f64, f64)]) -> f64 {
    p.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).sum()
}
fn length(g: &Geom) -> f64 {
    match g {
        Geom::Line(p) => seglen(p),
        Geom::Multi(ps) => ps.iter().map(|p| seglen(p)).sum(),
        Geom::Poly(_) => 0.0,
    }
}
fn etype(g: &Geom) -> &'static str {
    match g {
        Geom::Line(_) => "LineString",
        Geom::Multi(_) => "MultiLineString",
        Geom::Poly(_) => "Polygon",
    }
}

// -------------------------------------------------------------- accumulation

struct Acc {
    frags: Vec<Vec<(f64, f64)>>,
    tip: Option<String>,
    square: String,
    facet: String,
    src: Option<String>,
    date: Option<String>,
}

struct Unnamed {
    tip: Option<String>,
    square: String,
    facet: String,
    src: Option<String>,
    date: Option<String>,
    geom: Geom,
}

#[derive(Default)]
struct Bucket {
    named: HashMap<(String, String), Acc>, // (feature id, cleaned layer) -> fragments + meta
    unnamed: Vec<(String, Unnamed)>,       // (cleaned layer, feature)
}

thread_local! {
    // one archive per worker thread: opening parses the 168k-entry central
    // directory, so we cache it rather than reopen per tile.
    static ARCH: RefCell<Option<ZipArchive<BufReader<File>>>> = const { RefCell::new(None) };
}

/// read one .mvf entry's decrypted+inflated bytes via the thread-local archive.
fn read_tile(idx: usize) -> Option<Vec<u8>> {
    ARCH.with(|c| {
        let mut o = c.borrow_mut();
        let a = o.get_or_insert_with(|| {
            ZipArchive::new(BufReader::new(File::open(ZIP_PATH.get().unwrap()).unwrap())).unwrap()
        });
        let mut d = Vec::new();
        a.by_index_decrypt(idx, PASSWORD.get().unwrap())
            .and_then(|mut f| f.read_to_end(&mut d).map_err(Into::into))
            .ok()
            .map(|_| d)
    })
}

/// merge one tile's gas features into the per-thread bucket.
fn parse_tile(idx: usize, name: &str, b: &mut Bucket) -> usize {
    let d = match read_tile(idx) {
        Some(d) => d,
        None => return 0,
    };
    let db = deob(&d);
    static META: OnceLock<Regex> = OnceLock::new();
    let re = META.get_or_init(|| Regex::new(r#""(\w+):([^"]*)""#).unwrap());
    let mut meta: HashMap<String, String> = HashMap::new();
    for c in re.captures_iter(&db) {
        meta.insert(latin1(&c[1]), latin1(&c[2]));
    }
    let g = |k: &str| meta.get(k).and_then(|v| v.parse::<f64>().ok());
    let (x0, y0, x1, y1) = match (g("Xmin"), g("Ymin"), g("Xmax"), g("Ymax")) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return 0,
    };
    let (vw, vh) = vdc_extent(&db);
    let (sx, sy) = ((x1 - x0) / vw as f64, (y1 - y0) / vh as f64);
    let start = match find_start(&d, &db) {
        Some(s) => s,
        None => return 0,
    };
    let facet = meta.get("Facet").cloned().unwrap_or_else(|| {
        std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
    });
    let square = name.split('/').nth(3).unwrap_or(&facet[..facet.len().min(2)]).to_string();
    let src = meta.get("Source").cloned();
    let date = meta.get("Date").cloned();

    // walk the application-structure tree; stack frames: (type, layer, name, tip)
    let mut stack: Vec<(String, Option<String>, Option<String>, Option<String>)> = Vec::new();
    let mut p = start;
    let mut kept = 0;
    while let Some((cl, eid, qs, qe, np)) = read_cmd(&d, p) {
        p = np;
        let par = &d[qs..qe];
        if cl == 0 && eid == APS_BEGIN {
            let r = runs(par);
            let t = if r.len() > 1 { latin1(r[1]) } else { "?".into() };
            stack.push((t, None, None, None));
            continue;
        }
        if cl == 0 && eid == APS_END {
            stack.pop();
            continue;
        }
        if cl == 0 && (eid == END_PIC || eid == END_META) {
            break;
        }
        if cl == 9 && eid == 1 {
            let r = runs(par);
            if !r.is_empty() && !stack.is_empty() {
                let k = latin1(r[0]);
                let k = k.trim_end_matches(['"', '&', ')', '(']).trim();
                let v = if r.len() > 1 { Some(latin1(r[1])) } else { None };
                match k {
                    "LayerName" => {
                        for fr in stack.iter_mut() {
                            if fr.0 == "layer" {
                                fr.1 = v.clone();
                            }
                        }
                    }
                    "Name" => stack.last_mut().unwrap().2 = v,
                    "ScreenTip" => stack.last_mut().unwrap().3 = v,
                    _ => {}
                }
            }
            continue;
        }
        if cl != 4 || (eid != POLYLINE && eid != DISJOINT && eid != POLYGON) {
            continue;
        }
        // effective layer; keep only the gas mains & plant layers
        let layer = match stack.iter().rev().find_map(|fr| fr.1.as_ref()) {
            Some(l) if l.contains("Mains & Plant") => l,
            _ => continue,
        };
        let layer_clean = layer.trim_start_matches(|c: char| !c.is_ascii_alphabetic()).to_string();
        let mut cs = pts(par, x0, y0, sx, sy);
        if eid != DISJOINT {
            cs.dedup(); // drop consecutive duplicate vertices (zero-length segments)
        }
        if cs.len() < 2 {
            continue;
        }
        let gr = stack.iter().rev().find(|fr| fr.0 == "grobject");
        let fid = gr
            .and_then(|f| f.2.clone())
            .filter(|n| !n.is_empty() && n != "UNKNOWN");
        let tip = gr.and_then(|f| f.3.clone());
        kept += 1;

        if let Some(fid) = fid {
            let acc = b.named.entry((fid, layer_clean)).or_insert_with(|| Acc {
                frags: Vec::new(),
                tip: None,
                square: square.clone(),
                facet: facet.clone(),
                src: None,
                date: None,
            });
            match eid {
                DISJOINT => {
                    for c in cs.chunks(2) {
                        if c.len() == 2 {
                            acc.frags.push(c.to_vec());
                        }
                    }
                }
                _ => acc.frags.push(cs),
            }
            if acc.tip.is_none() {
                acc.tip = tip;
            }
            if square < acc.square {
                acc.square = square.clone();
            }
            if facet < acc.facet {
                acc.facet = facet.clone();
            }
            if acc.src.is_none() {
                acc.src = src.clone();
            }
            match &acc.date {
                Some(d0) if date.as_ref().map_or(true, |d1| d1 <= d0) => {}
                _ => acc.date = date.clone(),
            }
        } else {
            let geom = match eid {
                DISJOINT => Geom::Multi(cs.chunks(2).filter(|c| c.len() == 2).map(|c| c.to_vec()).collect()),
                POLYGON if cs.len() >= 3 => Geom::Poly(cs),
                POLYGON => continue,
                _ => Geom::Line(cs),
            };
            b.unnamed.push((
                layer_clean,
                Unnamed { tip, square: square.clone(), facet: facet.clone(), src: src.clone(), date: date.clone(), geom },
            ));
        }
    }
    kept
}

fn merge(mut a: Bucket, b: Bucket) -> Bucket {
    for (k, v) in b.named {
        match a.named.entry(k) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(v);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let acc = e.get_mut();
                acc.frags.extend(v.frags);
                if acc.tip.is_none() {
                    acc.tip = v.tip;
                }
                if v.square < acc.square {
                    acc.square = v.square;
                }
                if v.facet < acc.facet {
                    acc.facet = v.facet;
                }
                if acc.src.is_none() {
                    acc.src = v.src;
                }
                if v.date.is_some() && (acc.date.is_none() || v.date > acc.date) {
                    acc.date = v.date;
                }
            }
        }
    }
    a.unnamed.extend(b.unnamed);
    a
}

// ----------------------------------------------------------------- labelling

fn pressure(layer: &str) -> (Option<&'static str>, String) {
    match layer {
        "Low Pressure Mains & Plant" => (Some("lp"), "low pressure".into()),
        "Medium Pressure Mains & Plant" => (Some("mp"), "medium pressure".into()),
        "Intermediate Pressure Mains & Plant" => (Some("ip"), "intermediate pressure".into()),
        "Local High Pressure Mains & Plant" => (Some("lhp"), "local high pressure".into()),
        "National High Pressure Mains & Plant" => (Some("nhp"), "national high pressure".into()),
        _ => (None, layer.to_lowercase()),
    }
}

fn material(m: &str) -> Option<&'static str> {
    Some(match m {
        "PE" => "polyethylene",
        "CI" => "cast iron",
        "SI" => "spun iron",
        "DI" => "ductile iron",
        "ST" => "steel",
        "PV" => "pvc",
        "AS" => "asbestos cement",
        "LE" => "lead",
        "UN" => "unknown",
        "NA" => "not available",
        _ => return None,
    })
}

fn dia_mm(d: Option<&str>, u: Option<&str>) -> Option<f64> {
    let v: f64 = d?.parse().ok()?;
    match u {
        Some("\"") => Some((v * 25.4 * 10.0).round() / 10.0),
        Some("MM") => Some(v),
        _ if v >= 100.0 => Some(v),
        _ => None,
    }
}

/// screentip -> (diameter_mm, material, host_diameter_mm, host_material)
fn parse_tip(t: Option<&str>, re: &regex::Regex) -> (Option<f64>, Option<&'static str>, Option<f64>, Option<&'static str>) {
    let t = match t {
        Some(t) => t,
        None => return (None, None, None, None),
    };
    let c = match re.captures(t) {
        Some(c) => c,
        None => return (None, None, None, None),
    };
    let f = |i| c.get(i).map(|m| m.as_str());
    (
        dia_mm(f(1), f(2)),
        f(3).and_then(material),
        dia_mm(f(4), f(5)),
        f(6).and_then(material),
    )
}

fn tenk(facet: &str) -> String {
    let c: Vec<char> = facet.chars().collect();
    if c.len() >= 5 {
        format!("{}{}{}{}", c[0], c[1], c[2], c[4])
    } else {
        facet.to_string()
    }
}

/// 10 km national-grid tile -> cadent network area, from meta/NG.ADF.
fn areas() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let txt = std::fs::read_to_string("meta/NG.ADF").unwrap_or_default();
    let hdr = Regex::new(r"^\[(\w+)\]").unwrap();
    let tile = Regex::new(r"^Tile\d+=(\w+)").unwrap();
    let mut cur: Option<String> = None;
    for ln in txt.lines() {
        if let Some(h) = hdr.captures(ln.as_bytes()) {
            let name = latin1(&h[1]);
            if name != "Areas" {
                cur = Some(name);
            }
        } else if let (Some(t), Some(c)) = (tile.captures(ln.as_bytes()), &cur) {
            out.insert(latin1(&t[1]), c.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------- main

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut zip = "MapsViewerApril2026.zip".to_string();
    let mut out = "dist/cadent_gas_network.parquet".to_string();
    let mut pw = "reply-Dy7bge".to_string();
    let mut square: Option<String> = None;
    let mut limit = 0usize;
    let mut jobs = 0usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = it.next().unwrap().clone(),
            "-p" => pw = it.next().unwrap().clone(),
            "--square" => square = Some(it.next().unwrap().clone()),
            "--limit" => limit = it.next().unwrap().parse().unwrap(),
            "-j" => jobs = it.next().unwrap().parse().unwrap(),
            s if !s.starts_with('-') => zip = s.to_string(),
            _ => {}
        }
    }
    if jobs > 0 {
        rayon::ThreadPoolBuilder::new().num_threads(jobs).build_global().unwrap();
    }
    ZIP_PATH.set(zip.clone()).unwrap();
    PASSWORD.set(pw.into_bytes()).unwrap();
    let t0 = Instant::now();

    // index the .mvf entries
    eprint!("opening {} ...\r", zip);
    let arch = ZipArchive::new(BufReader::new(File::open(&zip).expect("open zip"))).expect("read zip");
    let mut entries: Vec<(usize, String)> = (0..arch.len())
        .filter_map(|i| arch.name_for_index(i).map(|n| (i, n.to_string())))
        .filter(|(_, n)| n.to_lowercase().ends_with(".mvf"))
        .filter(|(_, n)| square.as_ref().map_or(true, |s| n.split('/').nth(3) == Some(s.as_str())))
        .collect();
    if limit > 0 {
        entries.truncate(limit);
    }
    let total = entries.len();
    eprintln!("\rfound {total} gas tiles{}            ", square.as_ref().map(|s| format!(" in {s}")).unwrap_or_default());

    // progress monitor
    let done = Arc::new(AtomicUsize::new(0));
    let kept = Arc::new(AtomicUsize::new(0));
    let (d2, k2) = (done.clone(), kept.clone());
    let mon = std::thread::spawn(move || {
        let s = Instant::now();
        loop {
            let n = d2.load(Relaxed);
            let f = k2.load(Relaxed);
            let pct = 100.0 * n as f64 / total.max(1) as f64;
            let rate = n as f64 / s.elapsed().as_secs_f64().max(1e-3);
            eprint!("\r  parsing  {n:>7}/{total} tiles  {pct:5.1}%   {f:>8} gas features   {rate:6.0} tiles/s   ");
            let _ = std::io::stderr().flush();
            if n >= total {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    });

    // parse all tiles in parallel, dissolving by feature id as we go
    let bucket = entries
        .par_iter()
        .fold(Bucket::default, |mut b, (idx, name)| {
            let k = parse_tile(*idx, name, &mut b);
            done.fetch_add(1, Relaxed);
            kept.fetch_add(k, Relaxed);
            b
        })
        .reduce(Bucket::default, merge);
    mon.join().ok();
    eprintln!();

    let n_named = bucket.named.len();
    let n_unnamed = bucket.unnamed.len();
    eprintln!("  dissolving {} named pipes, {} unnamed features ...", n_named, n_unnamed);

    // dissolve named fragments into coherent pipes (parallel), keep unnamed as-is
    let area = areas();
    let tip_re = regex::Regex::new(
        r#"^(?P<d>\d+(?:\.\d+)?)\s*(?P<u>MM|")?\s*(?P<m>[A-Z]{2})?(?:\s*\(IN\s*(?P<hd>\d+(?:\.\d+)?)\s*(?P<hu>MM|")?\s*(?P<hm>[A-Z]{2})?\s*\))?\s*$"#,
    )
    .unwrap();

    struct Row {
        fid: Option<String>,
        layer: String,
        tip: Option<String>,
        square: String,
        facet: String,
        src: Option<String>,
        date: Option<String>,
        geom: Geom,
    }

    let mut rows: Vec<Row> = bucket
        .named
        .into_par_iter()
        .filter_map(|((fid, layer), acc)| {
            let mut merged = linemerge(acc.frags);
            merged.retain(|m| m.len() >= 2);
            if merged.is_empty() {
                return None;
            }
            let geom = if merged.len() == 1 {
                Geom::Line(merged.pop().unwrap())
            } else {
                Geom::Multi(merged)
            };
            Some(Row { fid: Some(fid), layer, tip: acc.tip, square: acc.square, facet: acc.facet, src: acc.src, date: acc.date, geom })
        })
        .collect();
    rows.extend(bucket.unnamed.into_iter().map(|(layer, u)| Row {
        fid: None,
        layer,
        tip: u.tip,
        square: u.square,
        facet: u.facet,
        src: u.src,
        date: u.date,
        geom: u.geom,
    }));

    // build columns
    let n = rows.len();
    let mut feature_id = Vec::with_capacity(n);
    let mut pressure_code = Vec::with_capacity(n);
    let mut pressure_v = Vec::with_capacity(n);
    let mut diameter = Vec::with_capacity(n);
    let mut mat = Vec::with_capacity(n);
    let mut hdia = Vec::with_capacity(n);
    let mut hmat = Vec::with_capacity(n);
    let mut inserted = Vec::with_capacity(n);
    let mut screentip = Vec::with_capacity(n);
    let mut net = Vec::with_capacity(n);
    let mut sq = Vec::with_capacity(n);
    let mut tk = Vec::with_capacity(n);
    let mut len_m = Vec::with_capacity(n);
    let mut source = Vec::with_capacity(n);
    let mut sdate = Vec::with_capacity(n);
    let mut et = Vec::with_capacity(n);
    let mut geomb: Vec<Vec<u8>> = Vec::with_capacity(n);
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut by_pressure: HashMap<&str, (usize, f64)> = HashMap::new();
    let mut totlen = 0.0;

    for r in &rows {
        let (pc, pf) = pressure(&r.layer);
        let (d, m, hd, hm) = parse_tip(r.tip.as_deref(), &tip_re);
        let t = tenk(&r.facet);
        let l = (length(&r.geom) * 100.0).round() / 100.0;
        let each = |g: &Geom, f: &mut dyn FnMut((f64, f64))| match g {
            Geom::Line(p) | Geom::Poly(p) => p.iter().for_each(|&pt| f(pt)),
            Geom::Multi(ps) => ps.iter().flatten().for_each(|&pt| f(pt)),
        };
        each(&r.geom, &mut |p| {
            minx = minx.min(p.0);
            miny = miny.min(p.1);
            maxx = maxx.max(p.0);
            maxy = maxy.max(p.1);
        });
        let e = by_pressure.entry(pc.unwrap_or("other")).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += l;
        totlen += l;

        feature_id.push(r.fid.clone());
        pressure_code.push(pc.map(|s| s.to_string()));
        pressure_v.push(Some(pf));
        diameter.push(d);
        mat.push(m.map(|s| s.to_string()));
        hdia.push(hd);
        hmat.push(hm.map(|s| s.to_string()));
        inserted.push(Some(hd.is_some() || hm.is_some()));
        screentip.push(r.tip.clone());
        net.push(area.get(&t).cloned());
        sq.push(Some(r.square.clone()));
        tk.push(Some(t));
        len_m.push(Some(l));
        source.push(r.src.clone());
        sdate.push(r.date.clone());
        et.push(Some(etype(&r.geom)));
        geomb.push(wkb(&r.geom));
    }

    // assemble geoparquet
    let cols: Vec<(&str, ArrayRef)> = vec![
        ("feature_id", Arc::new(StringArray::from(feature_id)) as ArrayRef),
        ("pressure_code", Arc::new(StringArray::from(pressure_code))),
        ("pressure", Arc::new(StringArray::from(pressure_v))),
        ("diameter_mm", Arc::new(Float64Array::from(diameter))),
        ("material", Arc::new(StringArray::from(mat))),
        ("host_diameter_mm", Arc::new(Float64Array::from(hdia))),
        ("host_material", Arc::new(StringArray::from(hmat))),
        ("inserted", Arc::new(BooleanArray::from(inserted))),
        ("screentip", Arc::new(StringArray::from(screentip))),
        ("network_area", Arc::new(StringArray::from(net))),
        ("square", Arc::new(StringArray::from(sq))),
        ("tenk", Arc::new(StringArray::from(tk))),
        ("length_m", Arc::new(Float64Array::from(len_m))),
        ("source", Arc::new(StringArray::from(source))),
        ("survey_date", Arc::new(StringArray::from(sdate))),
        ("etype", Arc::new(StringArray::from(et))),
        ("geometry", Arc::new(BinaryArray::from_iter(geomb.iter().map(Some)))),
    ];
    let fields: Vec<Field> = cols
        .iter()
        .map(|(name, arr)| Field::new(*name, arr.data_type().clone(), true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), cols.into_iter().map(|(_, a)| a).collect()).unwrap();

    if let Some(p) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(p).ok();
    }
    let geo = format!(
        r#"{{"version":"1.1.0","primary_column":"geometry","columns":{{"geometry":{{"encoding":"WKB","geometry_types":["LineString","MultiLineString","Polygon"],"crs":{},"bbox":[{},{},{},{}]}}}}}}"#,
        CRS, minx, miny, maxx, maxy
    );
    let props = WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::default())).build();
    let mut w = ArrowWriter::try_new(File::create(&out).unwrap(), schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.append_key_value_metadata(KeyValue::new("geo".into(), geo));
    w.close().unwrap();

    // summary
    eprintln!("\n  {n} pipes  ->  {out}");
    let mut tiers: Vec<_> = by_pressure.into_iter().collect();
    tiers.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    eprintln!("  {:<22} {:>10} {:>12}", "tier", "pipes", "km");
    for (code, (cnt, km)) in &tiers {
        eprintln!("  {:<22} {:>10} {:>12.1}", code, cnt, km / 1000.0);
    }
    eprintln!("  {:<22} {:>10} {:>12.1}", "total", n, totlen / 1000.0);
    eprintln!("  done in {:.1}s", t0.elapsed().as_secs_f64());
}

const CRS: &str = r#"{"$schema":"https://proj.org/schemas/v0.7/projjson.schema.json","type":"ProjectedCRS","name":"OSGB36 / British National Grid","base_crs":{"name":"OSGB36","datum":{"type":"GeodeticReferenceFrame","name":"Ordnance Survey of Great Britain 1936","ellipsoid":{"name":"Airy 1830","semi_major_axis":6377563.396,"inverse_flattening":299.3249646}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"},{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"}]},"id":{"authority":"EPSG","code":4277}},"conversion":{"name":"British National Grid","method":{"name":"Transverse Mercator","id":{"authority":"EPSG","code":9807}},"parameters":[{"name":"Latitude of natural origin","value":49,"unit":"degree","id":{"authority":"EPSG","code":8801}},{"name":"Longitude of natural origin","value":-2,"unit":"degree","id":{"authority":"EPSG","code":8802}},{"name":"Scale factor at natural origin","value":0.9996012717,"unit":"unity","id":{"authority":"EPSG","code":8805}},{"name":"False easting","value":400000,"unit":"metre","id":{"authority":"EPSG","code":8806}},{"name":"False northing","value":-100000,"unit":"metre","id":{"authority":"EPSG","code":8807}}]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"E","direction":"east","unit":"metre"},{"name":"Northing","abbreviation":"N","direction":"north","unit":"metre"}]},"id":{"authority":"EPSG","code":27700}}"#;
