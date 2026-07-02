// gpu-map data generation. turns the dissolved network into the demand-paged
// level-of-detail artefacts that map.html reads — straight from the same in-memory
// features, in osgb36 british national grid (metres, emitted as kilometres), no
// reprojection. four gitignored files under the configured dir (default `dist`):
//
//   map.f32       every segment: x0 y0 x1 y1 tone flag (le f32, km). binned to a
//                 grid on its midpoint and sorted by cell id, so each grid cell is
//                 one contiguous byte-range the client http-range-fetches on demand.
//   map.idx       u32 segment count per cell, row-major (ncols*nrows); the client
//                 prefix-sums it into byte offsets.
//   map.base.f32  coarse skeleton of just the longer trunk mains, heavily simplified
//                 and hilbert-sorted; tiny, always resident, shown when zoomed out.
//   map.json      grid + default view + zoom thresholds, so map.html hardcodes none.
//
// tone = categorical material index of the *live carrier* (see MATS: risk-ordered,
// cast iron 0 … polyethylene 7; unknown -1, drawn grey). a pe-lined iron main reads
// as its pe carrier. flag = 1 for live medium-pressure ductile iron (mpdi), the
// killer class — highlighted on top by the shader. map.html's palette + legend
// mirror MATS order.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};

use regex::Regex;
use serde::Deserialize;

use crate::{parse_tip, Config, Geom, Row};

#[derive(Deserialize)]
pub struct MapCfg {
    pub(crate) dir: String,
    cell_km: f64,
    origin: [f64; 2], // grid origin (min easting, min northing) in km
    ncols: usize,
    nrows: usize,
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

fn tone(m: Option<&str>) -> f32 {
    m.and_then(|m| MATS.iter().position(|&x| x == m)).map_or(-1.0, |i| i as f32)
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

/// (x,y) on a 2^16 grid -> hilbert distance, for a cache-friendly base ordering.
fn hilbert(mut x: u32, mut y: u32) -> u64 {
    const N: u32 = 1 << 16;
    let (mut d, mut s) = (0u64, N / 2);
    while s > 0 {
        let rx = ((x & s) > 0) as u32;
        let ry = ((y & s) > 0) as u32;
        d += (s as u64) * (s as u64) * ((3 * rx ^ ry) as u64);
        if ry == 0 {
            if rx == 1 {
                x = N - 1 - x;
                y = N - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
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

    let (minx, miny) = (m.origin[0], m.origin[1]);
    let (nc, nr, cell) = (m.ncols, m.nrows, m.cell_km);
    let span = (nc.max(nr) as f64) * cell; // bounding span (km) for hilbert normalisation

    let mut detail: Vec<(u32, [f32; 6])> = Vec::new();
    let mut base: Vec<(u64, [f32; 6])> = Vec::new();

    for r in rows {
        let mat = re.as_ref().and_then(|re| parse_tip(r.tip.as_deref(), re, mats).1);
        let tn = tone(mat);
        let mp_di = mat == Some("ductile iron") && tcode.get(r.layer.as_str()).copied() == Some("mp");
        let flag = if mp_di { 1.0f32 } else { 0.0 };
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
            // detail: full network, lightly simplified, binned by cell
            for w in simplify(line, m.simplify_detail_m).windows(2) {
                let (x0, y0, x1, y1) =
                    (w[0].0 / 1000.0, w[0].1 / 1000.0, w[1].0 / 1000.0, w[1].1 / 1000.0);
                let c = ((((x0 + x1) * 0.5 - minx) / cell).floor() as i64).clamp(0, nc as i64 - 1) as u32;
                let rr = ((((y0 + y1) * 0.5 - miny) / cell).floor() as i64).clamp(0, nr as i64 - 1) as u32;
                detail.push((c + nc as u32 * rr, [x0 as f32, y0 as f32, x1 as f32, y1 as f32, tn, flag]));
            }
            // base: only longer mains, heavily simplified, hilbert-ordered
            if len_m > m.base_min_len_m {
                for w in simplify(line, m.simplify_base_m).windows(2) {
                    let (x0, y0, x1, y1) =
                        (w[0].0 / 1000.0, w[0].1 / 1000.0, w[1].0 / 1000.0, w[1].1 / 1000.0);
                    let hx = ((((x0 + x1) * 0.5 - minx) / span).clamp(0.0, 1.0) * 65535.0) as u32;
                    let hy = ((((y0 + y1) * 0.5 - miny) / span).clamp(0.0, 1.0) * 65535.0) as u32;
                    base.push((hilbert(hx, hy), [x0 as f32, y0 as f32, x1 as f32, y1 as f32, tn, flag]));
                }
            }
        }
    }

    detail.sort_unstable_by_key(|e| e.0);
    base.sort_unstable_by_key(|e| e.0);
    let mut counts = vec![0u32; nc * nr];
    for (c, _) in &detail {
        counts[*c as usize] += 1;
    }

    std::fs::create_dir_all(&m.dir).ok();
    let path = |f: &str| format!("{}/{}", m.dir, f);
    let write_segs = |f: &str, segs: &[[f32; 6]]| {
        let mut w = BufWriter::new(File::create(path(f)).unwrap());
        for rec in segs {
            for v in rec {
                w.write_all(&v.to_le_bytes()).unwrap();
            }
        }
        w.flush().unwrap();
    };
    write_segs("map.f32", &detail.iter().map(|e| e.1).collect::<Vec<_>>());
    write_segs("map.base.f32", &base.iter().map(|e| e.1).collect::<Vec<_>>());
    let mut wi = BufWriter::new(File::create(path("map.idx")).unwrap());
    for c in &counts {
        wi.write_all(&c.to_le_bytes()).unwrap();
    }
    wi.flush().unwrap();

    // zoom thresholds (ndc-per-km): detail loads when the half-view drops below ~12 km;
    // zoom-out floor shows the whole ~360 km network; ceiling is street level.
    let (detail_scale, smin, smax) = (1.0 / 12.0, 0.5 / span, 80.0);
    let json = format!(
        r#"{{"minx":{},"miny":{},"cell":{},"ncols":{},"nrows":{},"view":{{"cx":{},"cy":{},"s":{}}},"detail_scale":{},"smin":{},"smax":{}}}"#,
        minx, miny, cell, nc, nr, m.view[0], m.view[1], m.view[2], detail_scale, smin, smax
    );
    File::create(path("map.json")).unwrap().write_all(json.as_bytes()).unwrap();

    eprintln!(
        "  map: {} detail segs, {} base segs, {}×{} grid  ->  {}/map.*",
        detail.len(),
        base.len(),
        nc,
        nr,
        m.dir
    );
}
