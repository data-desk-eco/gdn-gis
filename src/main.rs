// generic extractor for obfuscated-webcgm (.mvf) tile distributions, such as the
// cadent / national-grid "maps viewer" windows product.
//
// decodes the tiles, keeps only the layers named in the config, and dissolves
// every feature's per-tile fragments into one coherent line by id **as it goes**
// — writing a single geoparquet (no intermediate). all dataset-specific knowledge
// (password, layer filter, attribute vocabulary, labelling, crs) lives in a toml
// config; the engine here is generic.
//
// usage: gdn-gis [CONFIG.toml] [ZIP] [-o OUT] [-p PASSWORD] [--square SK] [--limit N] [-j JOBS] [--works]
//
// the two source datasets are fetched by the standalone scripts under scripts/
// (fetch-maps.sh, fetch-works.sh); this binary is the pure parser.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use rayon::prelude::*;
use regex::bytes::Regex;
use serde::Deserialize;
use zip::ZipArchive;

use arrow::array::{ArrayRef, BinaryArray, BooleanArray, Float64Array, StringArray};
use arrow::datatypes::{Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

mod map; // gpu-map artefact generation (dist/map.f32 / .idx / .base.f32 / .json)
mod works; // streetworks incident artefacts (dist/works.f32 / .tsv)

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
// tile-parse failure diagnostics: a decode failure loses a whole tile's features
static FAIL_READ: AtomicUsize = AtomicUsize::new(0);
static FAIL_BOUNDS: AtomicUsize = AtomicUsize::new(0);
static FAIL_START: AtomicUsize = AtomicUsize::new(0);
static FAIL_START_GAS: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------- config

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    zip: Option<String>,
    #[serde(default)]
    output: Option<String>,
    password: String,
    square_index: usize,
    crs: String,
    aps: Aps,
    keep: Keep,
    #[serde(default)]
    pub(crate) tier: Option<Tier>,
    #[serde(default)]
    pub(crate) spec: Option<Spec>,
    #[serde(default)]
    area: Option<Area>,
    #[serde(default)]
    map: Option<map::MapCfg>,
    #[serde(default)]
    works: Option<works::WorksCfg>,
}

#[derive(Deserialize)]
struct Aps {
    layer: String,
    feature: String,
    layer_attr: String,
    id_attr: String,
    spec_attr: String,
    #[serde(default)]
    id_null: Vec<String>,
    #[serde(default)]
    strip_layer_prefix: bool,
}

#[derive(Deserialize)]
struct Keep {
    layer_contains: Vec<String>,
}

#[derive(Deserialize)]
struct Tier {
    code_column: String,
    label_column: String,
    pub(crate) map: Vec<TierRow>,
}

#[derive(Deserialize)]
struct TierRow {
    #[serde(rename = "match")]
    pub(crate) m: String,
    pub(crate) code: String,
    label: String,
}

#[derive(Deserialize)]
struct Spec {
    pub(crate) regex: String,
    pub(crate) materials: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Area {
    file: String,
    column: String,
    #[serde(default)]
    skip_section: Option<String>,
}

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

/// score a candidate body offset: read up to `cmds` commands and return
/// `(primitives, points, in-range points)`. obfuscated header bytes read as plain
/// cgm give ~random coordinates, so a high in-range ratio marks the real body.
fn body_score(d: &[u8], s: usize, cmds: u32) -> (u32, u32, u32) {
    let (mut tot, mut ok, mut nprim, mut p, mut n) = (0u32, 0u32, 0u32, s, 0);
    while n < cmds {
        let (cl, eid, qs, qe, np) = match read_cmd(d, p) {
            Some(c) => c,
            None => break,
        };
        if qe - qs > 8192 && cl != 4 {
            break; // a multi-kb non-polyline command means we are desynced (real long
                   // commands are class-4 polylines; the desync tell is a giant class-7 span)
        }
        if cl == 4 && (eid == POLYLINE || eid == DISJOINT || eid == POLYGON) {
            nprim += 1;
            let par = &d[qs..qe];
            let m = par.len() / 2;
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
    (nprim, tot, ok)
}

/// is `s` a genuinely command-aligned body offset? walk the whole tile and require
/// it to reach a real application-structure frame — an APS_BEGIN whose type is the
/// config's `layer` or `feature` keyword — without first hitting a multi-kb command
/// (the tell of a desync). this is the decisive check: read as plain cgm, a handful
/// of obfuscated-header bytes just before the true body decode as a short in-range
/// primitive run, so a points-only score accepts an offset a few bytes early; that
/// shadow framing can even coast to a clean end, but it never spells the exact ascii
/// frame keywords. matching one proves true alignment; garbage cannot fake it.
fn aligned(d: &[u8], s: usize, a: &Aps) -> bool {
    let mut p = s;
    let mut n = 0u32;
    while let Some((cl, eid, qs, qe, np)) = read_cmd(d, p) {
        if qe - qs > 8192 && cl != 4 {
            return false; // a multi-kb non-polyline command means we are desynced
        }
        if cl == 0 && eid == APS_BEGIN {
            let r = runs(&d[qs..qe]);
            if r.len() > 1 {
                let t = latin1(r[1]);
                if t == a.layer || t == a.feature {
                    return true;
                }
            }
        }
        if cl == 0 && (eid == END_PIC || eid == END_META) {
            return false; // picture ended before any real frame => a misframed start
        }
        p = np;
        n += 1;
        if n > 300_000 {
            return false;
        }
    }
    false
}

/// offset of the plain-cgm picture body (after the obfuscated header). the
/// obfuscated/plain boundary is not a fixed structural marker, so it is found by
/// scanning: the earliest offset whose next commands are in-range geometry (random
/// header bytes read as plain cgm score ~0.33 in range) **and** which is genuinely
/// command-aligned (`aligned`). the alignment check is essential — a points-only test
/// accepts a false positive a few bytes before the true start, where stray header
/// bytes decode as an in-range primitive run, and that silently swallows or drops the
/// whole tile. two tiers: a strict dense pass, then a lenient one for sparse
/// single-pipe tiles. tiles whose only gas token is an empty layer label keep nothing.
fn find_start(d: &[u8], db: &[u8], a: &Aps) -> Option<usize> {
    let lo = find(db, b"Ymax").map(|j| j + 12).unwrap_or(380);
    let hi = (lo + 4000).min(d.len());
    let scan = |min_prim, min_pts| {
        (lo..hi).step_by(2).find(|&s| {
            let (nprim, tot, ok) = body_score(d, s, 64);
            nprim >= min_prim && tot >= min_pts && ok as f64 > 0.95 * tot as f64 && aligned(d, s, a)
        })
    };
    scan(2, 20).or_else(|| scan(1, 2))
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

/// decode int16 vdc pairs -> (easting, northing) in the tile's crs.
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

pub(crate) enum Geom {
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
                let mut seg = frags[f].clone(); // orient to start at nd
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

/// one output feature: a dissolved named pipe or an as-is unnamed primitive.
pub(crate) struct Row {
    fid: Option<String>,
    pub(crate) layer: String,
    pub(crate) tip: Option<String>,
    square: String,
    facet: String,
    src: Option<String>,
    date: Option<String>,
    pub(crate) geom: Geom,
}

thread_local! {
    // one archive per worker thread: opening parses the huge central directory,
    // so we cache it rather than reopen per tile.
    static ARCH: RefCell<Option<ZipArchive<BufReader<File>>>> = const { RefCell::new(None) };
}

/// read one tile's decrypted+inflated bytes via the thread-local archive.
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

/// merge one tile's kept features into the per-thread bucket.
fn parse_tile(idx: usize, name: &str, b: &mut Bucket, cfg: &Config) -> usize {
    let d = match read_tile(idx) {
        Some(d) => d,
        None => {
            FAIL_READ.fetch_add(1, Relaxed);
            return 0;
        }
    };
    // a tile keeps nothing unless a kept layer's name appears (as plain ascii) in its
    // body — so skip the ~half of tiles that carry only os background/annotation. this
    // is both a large speed-up and what lets find_start's alignment check stay strict.
    if !cfg.keep.layer_contains.iter().any(|k| find(&d, k.as_bytes()).is_some()) {
        return 0;
    }
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
        _ => {
            FAIL_BOUNDS.fetch_add(1, Relaxed);
            return 0;
        }
    };
    let (vw, vh) = vdc_extent(&db);
    let (sx, sy) = ((x1 - x0) / vw as f64, (y1 - y0) / vh as f64);
    let start = match find_start(&d, &db, &cfg.aps) {
        Some(s) => s,
        None => {
            FAIL_START.fetch_add(1, Relaxed);
            // diagnostic: did we just drop a tile that actually carries gas
            // linework? the layer name lives as plain ascii in the picture body.
            if cfg.keep.layer_contains.iter().any(|k| find(&d, k.as_bytes()).is_some()) {
                FAIL_START_GAS.fetch_add(1, Relaxed);
            }
            return 0;
        }
    };
    let facet = meta.get("Facet").cloned().unwrap_or_else(|| {
        std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
    });
    let square = name
        .split('/')
        .nth(cfg.square_index)
        .unwrap_or(&facet[..facet.len().min(2)])
        .to_string();
    let src = meta.get("Source").cloned();
    let date = meta.get("Date").cloned();
    let a = &cfg.aps;

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
                if k == a.layer_attr {
                    for fr in stack.iter_mut() {
                        if fr.0 == a.layer {
                            fr.1 = v.clone();
                        }
                    }
                } else if k == a.id_attr {
                    stack.last_mut().unwrap().2 = v;
                } else if k == a.spec_attr {
                    stack.last_mut().unwrap().3 = v;
                }
            }
            continue;
        }
        if cl != 4 || (eid != POLYLINE && eid != DISJOINT && eid != POLYGON) {
            continue;
        }
        // effective layer; keep only those matching the config filter
        let layer = match stack.iter().rev().find_map(|fr| fr.1.as_ref()) {
            Some(l) if cfg.keep.layer_contains.iter().any(|k| l.contains(k)) => l,
            _ => continue,
        };
        let layer_clean = if a.strip_layer_prefix {
            layer.trim_start_matches(|c: char| !c.is_ascii_alphabetic()).to_string()
        } else {
            layer.clone()
        };
        let mut cs = pts(par, x0, y0, sx, sy);
        if eid != DISJOINT {
            cs.dedup(); // drop consecutive duplicate vertices (zero-length segments)
        }
        if cs.len() < 2 {
            continue;
        }
        let gr = stack.iter().rev().find(|fr| fr.0 == a.feature);
        let fid = gr
            .and_then(|f| f.2.clone())
            .filter(|n| !n.is_empty() && !a.id_null.iter().any(|z| z == n));
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

pub(crate) fn dia_mm(d: Option<&str>, u: Option<&str>) -> Option<f64> {
    let v: f64 = d?.parse().ok()?;
    match u {
        Some("\"") => Some((v * 25.4 * 10.0).round() / 10.0),
        Some("MM") => Some(v),
        _ if v >= 100.0 => Some(v),
        _ => None,
    }
}

/// screentip -> (diameter_mm, material, host_diameter_mm, host_material)
pub(crate) fn parse_tip<'a>(
    t: Option<&str>,
    re: &regex::Regex,
    mats: &'a HashMap<String, String>,
) -> (Option<f64>, Option<&'a str>, Option<f64>, Option<&'a str>) {
    let c = match t.and_then(|t| re.captures(t)) {
        Some(c) => c,
        None => return (None, None, None, None),
    };
    let s = |i| c.get(i).map(|m| m.as_str());
    let mat = |i| s(i).and_then(|m| mats.get(m).map(|s| s.as_str()));
    (dia_mm(s(1), s(2)), mat(3), dia_mm(s(4), s(5)), mat(6))
}

fn tenk(facet: &str) -> String {
    let c: Vec<char> = facet.chars().collect();
    if c.len() >= 5 {
        format!("{}{}{}{}", c[0], c[1], c[2], c[4])
    } else {
        facet.to_string()
    }
}

/// grid-tile -> area name, from an .adf-style ini (sections are area names,
/// `Tile<n>=<tile>` entries list the tiles they contain).
fn areas(a: &Area) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let txt = std::fs::read_to_string(&a.file).unwrap_or_default();
    let hdr = Regex::new(r"^\[(\w+)\]").unwrap();
    let tile = Regex::new(r"^Tile\d+=(\w+)").unwrap();
    let mut cur: Option<String> = None;
    for ln in txt.lines() {
        if let Some(h) = hdr.captures(ln.as_bytes()) {
            let name = latin1(&h[1]);
            if a.skip_section.as_deref() != Some(name.as_str()) {
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
    // args: [config] [zip] [-o out] [-p pw] [--square s] [--limit n] [-j j]
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut config = "config.toml".to_string();
    let (mut zip, mut out, mut pw): (Option<String>, Option<String>, Option<String>) = (None, None, None);
    let (mut square, mut limit, mut jobs) = (None, 0usize, 0usize);
    let mut positional = 0;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().unwrap().clone()),
            "-p" => pw = Some(it.next().unwrap().clone()),
            "--square" => square = Some(it.next().unwrap().clone()),
            "--limit" => limit = it.next().unwrap().parse().unwrap(),
            "-j" => jobs = it.next().unwrap().parse().unwrap(),
            "--config" => config = it.next().unwrap().clone(),
            s if !s.starts_with('-') => {
                match positional {
                    0 if s.ends_with(".toml") => config = s.to_string(),
                    _ => zip = Some(s.to_string()),
                }
                positional += 1;
            }
            _ => {}
        }
    }

    let cfg: Config = toml::from_str(&std::fs::read_to_string(&config).expect("read config"))
        .expect("parse config");
    let zip = zip.or_else(|| cfg.zip.clone()).expect("no zip path (config or arg)");
    let out = out.or_else(|| cfg.output.clone()).unwrap_or_else(|| "out.parquet".into());
    let pw = pw.unwrap_or_else(|| cfg.password.clone());

    // --works: regenerate only the streetworks incident artefacts (no zip needed)
    if argv.iter().any(|a| a == "--works") {
        return works::write(cfg.works.as_ref().expect("no [works]"), cfg.map.as_ref().expect("no [map]"));
    }

    if jobs > 0 {
        rayon::ThreadPoolBuilder::new().num_threads(jobs).build_global().unwrap();
    }
    ZIP_PATH.set(zip.clone()).unwrap();
    PASSWORD.set(pw.into_bytes()).unwrap();
    let t0 = Instant::now();

    // index the tile entries
    eprint!("opening {} ...\r", zip);
    let arch = ZipArchive::new(BufReader::new(File::open(&zip).expect("open zip"))).expect("read zip");
    let mut entries: Vec<(usize, String)> = (0..arch.len())
        .filter_map(|i| arch.name_for_index(i).map(|n| (i, n.to_string())))
        .filter(|(_, n)| n.to_lowercase().ends_with(".mvf"))
        .filter(|(_, n)| square.as_ref().map_or(true, |s| n.split('/').nth(cfg.square_index) == Some(s.as_str())))
        .collect();
    if limit > 0 {
        entries.truncate(limit);
    }
    let total = entries.len();
    eprintln!("\rfound {total} tiles{}            ", square.as_ref().map(|s| format!(" in {s}")).unwrap_or_default());

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
            eprint!("\r  parsing  {n:>7}/{total} tiles  {pct:5.1}%   {f:>9} features   {rate:6.0} tiles/s   ");
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
            let k = parse_tile(*idx, name, &mut b, &cfg);
            done.fetch_add(1, Relaxed);
            kept.fetch_add(k, Relaxed);
            b
        })
        .reduce(Bucket::default, merge);
    mon.join().ok();
    eprintln!();
    let (fr, fb, fs, fg) =
        (FAIL_READ.load(Relaxed), FAIL_BOUNDS.load(Relaxed), FAIL_START.load(Relaxed), FAIL_START_GAS.load(Relaxed));
    if fr + fb + fs > 0 {
        eprintln!("  dropped tiles: {fr} unreadable, {fb} no-bounds, {fs} no-picture-start ({fg} of which contain gas linework)");
    }
    eprintln!("  dissolving {} named features, {} unnamed ...", bucket.named.len(), bucket.unnamed.len());

    // dissolve named fragments into coherent features; keep unnamed as-is
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

    write_parquet(&rows, &cfg, &out, t0);
    if let Some(m) = &cfg.map {
        map::write(&rows, &cfg, m);
        if let Some(w) = &cfg.works {
            works::write(w, m);
        }
    }
}

fn write_parquet(rows: &[Row], cfg: &Config, out: &str, t0: Instant) {
    let area = cfg.area.as_ref().map(areas);
    let spec_re = cfg.spec.as_ref().map(|s| regex::Regex::new(&s.regex).unwrap());
    let tier_idx: HashMap<&str, (&str, &str)> = cfg
        .tier
        .as_ref()
        .map(|t| t.map.iter().map(|r| (r.m.as_str(), (r.code.as_str(), r.label.as_str()))).collect())
        .unwrap_or_default();

    let n = rows.len();
    let mut feature_id = Vec::with_capacity(n);
    let (mut tcode, mut tlabel) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut dia, mut mat, mut hdia, mut hmat, mut ins) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let mut screentip = Vec::with_capacity(n);
    let mut net = Vec::with_capacity(n);
    let (mut sq, mut tk, mut len_m) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut source, mut sdate, mut et) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let mut geomb: Vec<Vec<u8>> = Vec::with_capacity(n);
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut summary: HashMap<String, (usize, f64)> = HashMap::new();
    let mut totlen = 0.0;
    let empty_mats = HashMap::new();
    let mats = cfg.spec.as_ref().map(|s| &s.materials).unwrap_or(&empty_mats);

    for r in rows {
        let g = &r.geom;
        let t = tenk(&r.facet);
        let l = (length(g) * 100.0).round() / 100.0;
        let visit = |f: &mut dyn FnMut((f64, f64))| match g {
            Geom::Line(p) | Geom::Poly(p) => p.iter().for_each(|&pt| f(pt)),
            Geom::Multi(ps) => ps.iter().flatten().for_each(|&pt| f(pt)),
        };
        visit(&mut |p| {
            minx = minx.min(p.0);
            miny = miny.min(p.1);
            maxx = maxx.max(p.0);
            maxy = maxy.max(p.1);
        });

        feature_id.push(r.fid.clone());
        if cfg.tier.is_some() {
            let tier = tier_idx.get(r.layer.as_str()).copied();
            tcode.push(tier.map(|(c, _)| c.to_string()));
            tlabel.push(Some(tier.map_or_else(|| r.layer.to_lowercase(), |(_, l)| l.to_string())));
            let e = summary.entry(tier.map_or("other", |(c, _)| c).to_string()).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += l;
        }
        if let Some(re) = &spec_re {
            let (d, m, hd, hm) = parse_tip(r.tip.as_deref(), re, mats);
            dia.push(d);
            mat.push(m.map(str::to_string));
            hdia.push(hd);
            hmat.push(hm.map(str::to_string));
            ins.push(Some(hd.is_some() || hm.is_some()));
        }
        screentip.push(r.tip.clone());
        if let Some(a) = &area {
            net.push(a.get(&t).cloned());
        }
        sq.push(Some(r.square.clone()));
        tk.push(Some(t));
        len_m.push(Some(l));
        source.push(r.src.clone());
        sdate.push(r.date.clone());
        et.push(Some(etype(g)));
        geomb.push(wkb(g));
        totlen += l;
    }

    // assemble columns in a stable order; domain groups appear only if configured
    let mut cols: Vec<(String, ArrayRef)> = Vec::new();
    cols.push(("feature_id".into(), Arc::new(StringArray::from(feature_id))));
    if let Some(t) = &cfg.tier {
        cols.push((t.code_column.clone(), Arc::new(StringArray::from(tcode))));
        cols.push((t.label_column.clone(), Arc::new(StringArray::from(tlabel))));
    }
    if cfg.spec.is_some() {
        cols.push(("diameter_mm".into(), Arc::new(Float64Array::from(dia))));
        cols.push(("material".into(), Arc::new(StringArray::from(mat))));
        cols.push(("host_diameter_mm".into(), Arc::new(Float64Array::from(hdia))));
        cols.push(("host_material".into(), Arc::new(StringArray::from(hmat))));
        cols.push(("inserted".into(), Arc::new(BooleanArray::from(ins))));
    }
    cols.push(("screentip".into(), Arc::new(StringArray::from(screentip))));
    if let Some(a) = &cfg.area {
        cols.push((a.column.clone(), Arc::new(StringArray::from(net))));
    }
    cols.push(("square".into(), Arc::new(StringArray::from(sq))));
    cols.push(("tenk".into(), Arc::new(StringArray::from(tk))));
    cols.push(("length_m".into(), Arc::new(Float64Array::from(len_m))));
    cols.push(("source".into(), Arc::new(StringArray::from(source))));
    cols.push(("survey_date".into(), Arc::new(StringArray::from(sdate))));
    cols.push(("etype".into(), Arc::new(StringArray::from(et))));
    cols.push(("geometry".into(), Arc::new(BinaryArray::from_iter(geomb.iter().map(Some)))));

    let fields: Vec<Field> = cols.iter().map(|(name, arr)| Field::new(name, arr.data_type().clone(), true)).collect();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), cols.into_iter().map(|(_, a)| a).collect()).unwrap();

    if let Some(p) = std::path::Path::new(out).parent() {
        std::fs::create_dir_all(p).ok();
    }
    let geo = format!(
        r#"{{"version":"1.1.0","primary_column":"geometry","columns":{{"geometry":{{"encoding":"WKB","geometry_types":["LineString","MultiLineString","Polygon"],"crs":{},"bbox":[{},{},{},{}]}}}}}}"#,
        cfg.crs, minx, miny, maxx, maxy
    );
    let props = WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::default())).build();
    let mut w = ArrowWriter::try_new(File::create(out).unwrap(), schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.append_key_value_metadata(KeyValue::new("geo".into(), geo));
    w.close().unwrap();

    // summary
    eprintln!("\n  {n} features  ->  {out}");
    if cfg.tier.is_some() {
        let mut tiers: Vec<_> = summary.into_iter().collect();
        tiers.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        eprintln!("  {:<10} {:>10} {:>12}", "tier", "features", "km");
        for (code, (cnt, km)) in &tiers {
            eprintln!("  {:<10} {:>10} {:>12.1}", code, cnt, km / 1000.0);
        }
    }
    eprintln!("  {:<10} {:>10} {:>12.1}", "total", n, totlen / 1000.0);
    eprintln!("  done in {:.1}s", t0.elapsed().as_secs_f64());
}
