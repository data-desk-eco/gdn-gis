// gpu-map data generation, format v2. turns the dissolved network into the
// demand-paged artefacts map.html reads — straight from the same in-memory
// features, osgb36 british national grid. every record is 12 bytes:
//
//   u16 x0 y0 x1 y1   endpoint coords local to the record's 2 km cell
//                     (segments are clipped at cell boundaries, so 0..65535
//                     spans exactly one cell — ~3 cm posts, seam-exact)
//   u16 cell          grid cell id (col + ncols*row) — the shader rebuilds
//                     world coords from this, no per-draw uniforms
//   u8  tone|0x80     material index (MATS order; 0xff unknown), high bit =
//                     live medium-pressure ductile iron, the killer class
//   u8  year          laid year - 1848 (0 = unknown → always visible), an
//                     estimate joined from cadent's open gpi data (build-years.sh)
//
//   map.bin       every segment, sorted by cell id: each cell is one contiguous
//                 byte-range the client http-range-fetches on demand.
//   map.idx       u32 segment count per cell, row-major; client prefix-sums.
//   map.base.bin  coarse skeleton of the longer trunk mains, always resident.
//   map.json      grid + default view + thresholds + year span, so map.html
//                 hardcodes none of it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};

use regex::Regex;
use serde::Deserialize;

use crate::{parse_tip, Config, Geom, Row};

#[derive(Deserialize)]
pub struct MapCfg {
    pub(crate) dir: String,
    pub(crate) cell_km: f64,
    pub(crate) origin: [f64; 2], // grid origin (min easting, min northing) in km
    pub(crate) ncols: usize,
    pub(crate) nrows: usize,
    view: [f64; 3], // default centre + scale: cx_km, cy_km, ndc-per-km
    #[serde(default = "d_detail")]
    simplify_detail_m: f64,
    #[serde(default = "d_base")]
    simplify_base_m: f64,
    #[serde(default = "d_blen")]
    base_min_len_m: f64,
}
fn d_detail() -> f64 {
    0.5
}
fn d_base() -> f64 {
    45.0
}
fn d_blen() -> f64 {
    110.0
}

/// live-carrier materials, ordered by mrps-style risk (worst first); the tone a
/// segment carries is its index here, and map.html's palette follows the same order.
const MATS: [&str; 8] =
    ["cast iron", "spun iron", "asbestos cement", "lead", "ductile iron", "steel", "pvc", "polyethylene"];
/// the same materials as gpi two-letter codes, for the laid-year join.
const CODES: [&str; 8] = ["CI", "SI", "AS", "LE", "DI", "ST", "PV", "PE"];
pub(crate) const YR0: i32 = 1848; // year byte origin; 0 = unknown/always visible

// 8 = unknown material (must leave bit 7 clear — it carries the mpdi flag)
fn tone(m: Option<&str>) -> u8 {
    m.and_then(|m| MATS.iter().position(|&x| x == m)).map_or(8, |i| i as u8)
}

/// perpendicular distance from p to segment a-b.
fn perp(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let l2 = dx * dx + dy * dy;
    if l2 == 0.0 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0);
    ((p.0 - (a.0 + t * dx)).powi(2) + (p.1 - (a.1 + t * dy)).powi(2)).sqrt()
}

/// ramer-douglas-peucker, iterative; tol in the geometry's units (metres).
fn simplify(p: &[(f64, f64)], tol: f64) -> Vec<(f64, f64)> {
    if p.len() < 3 || tol <= 0.0 {
        return p.to_vec();
    }
    let last = p.len() - 1;
    let mut keep = vec![false; p.len()];
    keep[0] = true;
    keep[last] = true;
    let mut st = vec![(0usize, last)];
    while let Some((a, b)) = st.pop() {
        if b <= a + 1 {
            continue;
        }
        let (mut idx, mut dmax) = (0usize, 0.0);
        for i in a + 1..b {
            let d = perp(p[i], p[a], p[b]);
            if d > dmax {
                dmax = d;
                idx = i;
            }
        }
        if dmax > tol {
            keep[idx] = true;
            st.push((a, idx));
            st.push((idx, b));
        }
    }
    (0..p.len()).filter(|&i| keep[i]).map(|i| p[i]).collect()
}

/// grid geometry shared by every layer emitter (pipes here, buildings in bldg.rs).
pub(crate) struct Grid {
    pub minx: f64,
    pub miny: f64,
    pub cell: f64,
    pub nc: usize,
    pub nr: usize,
}
impl Grid {
    pub fn new(m: &MapCfg) -> Grid {
        assert!(m.ncols * m.nrows <= 65536, "cell ids are u16 in the 12 B record");
        Grid { minx: m.origin[0], miny: m.origin[1], cell: m.cell_km, nc: m.ncols, nr: m.nrows }
    }
    /// clip segment a-b (km) at cell boundaries and quantise each piece to u16
    /// cell-local coords; push one 12 B record per piece via `emit(cell, [x0 y0 x1 y1])`.
    pub fn clip(&self, a: (f64, f64), b: (f64, f64), mut emit: impl FnMut(u32, [u16; 4])) {
        let mut ts = vec![0.0f64];
        for (p, q, o) in [(a.0, b.0, self.minx), (a.1, b.1, self.miny)] {
            let (c0, c1) = ((p - o) / self.cell, (q - o) / self.cell);
            let (lo, hi) = (c0.min(c1).ceil() as i64, c0.max(c1).floor() as i64);
            for k in lo..=hi {
                let t = (k as f64 - c0) / (c1 - c0);
                if t > 0.0 && t < 1.0 {
                    ts.push(t);
                }
            }
        }
        ts.push(1.0);
        ts.sort_unstable_by(|x, y| x.total_cmp(y));
        let at = |t: f64| (a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1));
        for w in ts.windows(2) {
            if w[1] - w[0] < 1e-12 {
                continue;
            }
            let (p, q) = (at(w[0]), at(w[1]));
            let mid = ((p.0 + q.0) * 0.5, (p.1 + q.1) * 0.5);
            let c = (((mid.0 - self.minx) / self.cell).floor() as i64).clamp(0, self.nc as i64 - 1) as u32;
            let r = (((mid.1 - self.miny) / self.cell).floor() as i64).clamp(0, self.nr as i64 - 1) as u32;
            let (ox, oy) = (self.minx + c as f64 * self.cell, self.miny + r as f64 * self.cell);
            let qz = |v: f64, o: f64| (((v - o) / self.cell * 65535.0).round().clamp(0.0, 65535.0)) as u16;
            let rec = [qz(p.0, ox), qz(p.1, oy), qz(q.0, ox), qz(q.1, oy)];
            if rec[0] != rec[2] || rec[1] != rec[3] {
                emit(c + self.nc as u32 * r, rec);
            }
        }
    }
}

/// the laid-year sidecar (data/years.tsv from build-years.sh): median install
/// year per (100 m cell, material) from cadent's open gpi data, '*' = any.
struct Years(HashMap<u32, u8>, f64, f64);
impl Years {
    const N: u32 = 3600; // 100 m cells across the 360 km grid
    fn load(minx: f64, miny: f64) -> Years {
        let mut m = HashMap::new();
        // nb: build-years.sh bins against the same [map] origin — keep the two in step
        for ln in std::fs::read_to_string("data/years.tsv").unwrap_or_default().lines() {
            let mut f = ln.split('\t');
            let (Some(c), Some(mat), Some(y)) = (f.next(), f.next(), f.next()) else { continue };
            let (Ok(c), Ok(y)) = (c.parse::<u32>(), y.parse::<i32>()) else { continue };
            let mi = if mat == "*" { 15 } else { match CODES.iter().position(|&x| x == mat) { Some(i) => i as u32, None => continue } };
            m.insert(c * 16 + mi, (y - YR0).clamp(1, 255) as u8);
        }
        Years(m, minx, miny)
    }
    /// year byte for a segment: midpoint 100 m cell + material, ring-searching
    /// two cells out, then the any-material fallback; 0 = unknown.
    fn stamp(&self, x: f64, y: f64, tone: u8) -> u8 {
        if self.0.is_empty() {
            return 0;
        }
        let (cx, cy) = (((x - self.1) * 10.0) as i64, ((y - self.2) * 10.0) as i64);
        for mi in [tone as u32, 15].iter().filter(|&&m| m <= 15) {
            for r in 0..=2i64 {
                for dy in -r..=r {
                    for dx in -r..=r {
                        if dx.abs().max(dy.abs()) != r {
                            continue;
                        }
                        let (nx, ny) = (cx + dx, cy + dy);
                        if nx < 0 || ny < 0 || nx >= Self::N as i64 || ny >= Self::N as i64 {
                            continue;
                        }
                        if let Some(&v) = self.0.get(&((nx as u32 + Self::N * ny as u32) * 16 + mi)) {
                            return v;
                        }
                    }
                }
            }
        }
        0
    }
}

pub(crate) fn write_records(path: &str, recs: &[(u32, [u16; 4], u8, u8)]) {
    let mut w = BufWriter::new(File::create(path).unwrap());
    for (cell, q, b0, b1) in recs {
        for v in q {
            w.write_all(&v.to_le_bytes()).unwrap();
        }
        w.write_all(&(*cell as u16).to_le_bytes()).unwrap();
        w.write_all(&[*b0, *b1]).unwrap();
    }
    w.flush().unwrap();
}

pub(crate) fn write_idx(path: &str, counts: &[u32]) {
    let mut w = BufWriter::new(File::create(path).unwrap());
    for c in counts {
        w.write_all(&c.to_le_bytes()).unwrap();
    }
    w.flush().unwrap();
}

/// sort a record set by cell, write blob + per-cell count idx — the shape every
/// variable-record layer (pipes, buildings) ships in.
pub(crate) fn write_layer(dir: &str, stem: &str, recs: &mut Vec<(u32, [u16; 4], u8, u8)>, ncell: usize) {
    use rayon::prelude::*;
    recs.par_sort_unstable_by_key(|e| e.0);
    let mut counts = vec![0u32; ncell];
    for (c, ..) in recs.iter() {
        counts[*c as usize] += 1;
    }
    std::fs::create_dir_all(dir).ok();
    write_records(&format!("{dir}/{stem}.bin"), recs);
    write_idx(&format!("{dir}/{stem}.idx"), &counts);
}

pub fn write(rows: &[Row], cfg: &Config, m: &MapCfg) {
    let re = cfg.spec.as_ref().map(|s| Regex::new(&s.regex).unwrap());
    let empty = HashMap::new();
    let mats = cfg.spec.as_ref().map(|s| &s.materials).unwrap_or(&empty);
    // layer name -> pressure code, so we can pick out medium-pressure ductile iron
    let tcode: HashMap<&str, &str> = cfg
        .tier
        .as_ref()
        .map(|t| t.map.iter().map(|r| (r.m.as_str(), r.code.as_str())).collect())
        .unwrap_or_default();
    let g = Grid::new(m);
    let years = Years::load(g.minx, g.miny);

    let mut detail: Vec<(u32, [u16; 4], u8, u8)> = Vec::new();
    let mut base: Vec<(u32, [u16; 4], u8, u8)> = Vec::new();
    let (mut ymin, mut ymax) = (255u8, 0u8);

    for r in rows {
        let mat = re.as_ref().and_then(|re| parse_tip(r.tip.as_deref(), re, mats).1);
        let tn = tone(mat);
        let mp_di = mat == Some("ductile iron") && tcode.get(r.layer.as_str()).copied() == Some("mp");
        let tf = tn | if mp_di { 0x80 } else { 0 };
        let lines: Vec<&[(f64, f64)]> = match &r.geom {
            Geom::Line(p) => vec![p.as_slice()],
            Geom::Multi(ps) => ps.iter().map(|p| p.as_slice()).collect(),
            Geom::Poly(_) => continue,
        };
        for line in lines {
            if line.len() < 2 {
                continue;
            }
            let len_m: f64 = line
                .windows(2)
                .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
                .sum();
            let km = |p: (f64, f64)| (p.0 / 1000.0, p.1 / 1000.0);
            // detail: full network, lightly simplified, clipped + binned by cell
            for w in simplify(line, m.simplify_detail_m).windows(2) {
                let (a, b) = (km(w[0]), km(w[1]));
                let yr = years.stamp((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5, tn);
                if yr > 0 {
                    ymin = ymin.min(yr);
                    ymax = ymax.max(yr);
                }
                g.clip(a, b, |c, q| detail.push((c, q, tf, yr)));
            }
            // base: only longer mains, heavily simplified, always resident
            if len_m > m.base_min_len_m {
                for w in simplify(line, m.simplify_base_m).windows(2) {
                    let (a, b) = (km(w[0]), km(w[1]));
                    let yr = years.stamp((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5, tn);
                    g.clip(a, b, |c, q| base.push((c, q, tf, yr)));
                }
            }
        }
    }

    write_layer(&m.dir, "map", &mut detail, g.nc * g.nr);
    base.sort_unstable_by_key(|e| e.0); // resident, but cell order keeps vertex fetches local
    write_records(&format!("{}/map.base.bin", m.dir), &base);

    // no sidecar → no year span; keep the timeline sane rather than inverted
    if ymax < ymin {
        (ymin, ymax) = (2, 178);
    }
    // zoom thresholds (ndc-per-km): pipe detail + detail terrain when the half-view
    // drops below ~12 km; buildings at street level; floor shows the whole network.
    let span = (g.nc.max(g.nr) as f64) * g.cell;
    let (detail_scale, bldg_scale, smin, smax) = (1.0 / 12.0, 0.5, 0.5 / span, 80.0);
    let json = format!(
        concat!(
            r#"{{"minx":{},"miny":{},"cell":{},"ncols":{},"nrows":{},"#,
            r#""view":{{"cx":{},"cy":{},"s":{}}},"detail_scale":{},"bldg_scale":{},"smin":{},"smax":{},"#,
            r#""yr0":{},"yr":[{},{}],"t0":{{"n":{},"step":{}}},"t1":{{"p":{}}}}}"#
        ),
        g.minx, g.miny, g.cell, g.nc, g.nr, m.view[0], m.view[1], m.view[2],
        detail_scale, bldg_scale, smin, smax,
        YR0, YR0 + ymin as i32, YR0 + ymax as i32,
        crate::terrain::N0, crate::terrain::STEP, crate::terrain::P
    );
    File::create(format!("{}/map.json", m.dir)).unwrap().write_all(json.as_bytes()).unwrap();

    eprintln!(
        "  map: {} detail segs, {} base segs, {}×{} grid, years {}–{}  ->  {}/map.*",
        detail.len(), base.len(), g.nc, g.nr, YR0 + ymin as i32, YR0 + ymax as i32, m.dir
    );
}
