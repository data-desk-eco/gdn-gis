// buildings layer (gdn-gis --buildings): osm footprints from the geofabrik
// england extract (data/england-latest.osm.pbf, fetch-buildings.sh), drawn by
// the client as instanced wireframes. per footprint *edge*, the same 12 B
// record shape as the pipes (see map.rs): u16 cell-local x0 y0 x1 y1 clipped at
// cell bounds, u16 cell id, then h and min-height as u8 half-metres.
//
//   bldg.bin   edges sorted by cell — http-range-fetched at street zoom only.
//   bldg.idx   u32 edge count per cell.
//   roof.bin   earcut roof triangles, 16 B: u16 x0 y0 x1 y1 x2 y2 cell, u8 h+pad.
//   roof.idx   u32 triangle count per cell.
//   bldg.tsv   named buildings, "x y name" (bng km), sorted by cell — the lazy
//              click sidecar, fetched per pick.
//   bldg.tofs  u32 byte offset per cell +1 into bldg.tsv.
//
// heights: `height` tag, else `building:levels`×3 m, else 8 m. multipolygon
// relation members inherit the relation's tags; rings are never assembled —
// every member way's edges draw the same wireframe walls stitching would.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};

use osmpbf::{Element, ElementReader};
use rayon::prelude::*;

use crate::map::{write_idx, write_layer, Grid, MapCfg};

const PBF: &str = "data/england-latest.osm.pbf";

/// wgs84 lon/lat → osgb36 british national grid metres: geodetic→cartesian,
/// 7-param helmert (epsg:1314, ≤ ~3 m), cartesian→airy geodetic, transverse
/// mercator. good to the width of a kerb — the national ostn15 grid is not worth
/// its 15 MB here.
fn bng(lon: f64, lat: f64) -> (f64, f64) {
    let d = std::f64::consts::PI / 180.0;
    // wgs84 → cartesian
    let (a, f) = (6378137.0, 1.0 / 298.257223563);
    let e2 = f * (2.0 - f);
    let (sp, cp) = ((lat * d).sin_cos().0, (lat * d).cos());
    let nu = a / (1.0 - e2 * sp * sp).sqrt();
    let (x, y, z) = (nu * cp * (lon * d).cos(), nu * cp * (lon * d).sin(), nu * (1.0 - e2) * sp);
    // helmert wgs84→osgb36
    let s = 20.4894e-6;
    let (rx, ry, rz) = (-0.1502 / 3600.0 * d, -0.2470 / 3600.0 * d, -0.8421 / 3600.0 * d);
    let (x, y, z) = (-446.448 + (1.0 + s) * x - rz * y + ry * z,
                     125.157 + rz * x + (1.0 + s) * y - rx * z,
                     -542.060 - ry * x + rx * y + (1.0 + s) * z);
    // cartesian → airy geodetic (two newton steps suffice at this precision)
    let (a, b) = (6377563.396, 6356256.909);
    let e2 = 1.0 - (b * b) / (a * a);
    let p = x.hypot(y);
    let mut la = (z / (p * (1.0 - e2))).atan();
    for _ in 0..3 {
        let nu = a / (1.0 - e2 * la.sin().powi(2)).sqrt();
        la = ((z + e2 * nu * la.sin()) / p).atan();
    }
    let lo = y.atan2(x);
    // transverse mercator (osgb36 national grid)
    let (f0, la0, lo0, e0, n0) = (0.9996012717, 49.0 * d, -2.0 * d, 400000.0, -100000.0);
    let n = (a - b) / (a + b);
    let (sl, cl, tl) = (la.sin(), la.cos(), la.tan());
    let nu = a * f0 / (1.0 - e2 * sl * sl).sqrt();
    let rho = a * f0 * (1.0 - e2) / (1.0 - e2 * sl * sl).powf(1.5);
    let eta2 = nu / rho - 1.0;
    let m = b * f0
        * ((1.0 + n + 1.25 * n * n * (1.0 + n)) * (la - la0)
            - (3.0 * n * (1.0 + n) + 2.625 * n * n * n) * (la - la0).sin() * (la + la0).cos()
            + 1.875 * n * n * (1.0 + n) * (2.0 * (la - la0)).sin() * (2.0 * (la + la0)).cos()
            - 35.0 / 24.0 * n * n * n * (3.0 * (la - la0)).sin() * (3.0 * (la + la0)).cos());
    let dl = lo - lo0;
    let e = e0 + nu * cl * dl
        + nu / 6.0 * cl.powi(3) * (nu / rho - tl * tl) * dl.powi(3)
        + nu / 120.0 * cl.powi(5) * (5.0 - 18.0 * tl * tl + tl.powi(4) + 14.0 * eta2 - 58.0 * tl * tl * eta2) * dl.powi(5);
    let nn = n0 + m + nu / 2.0 * sl * cl * dl * dl
        + nu / 24.0 * sl * cl.powi(3) * (5.0 - tl * tl + 9.0 * eta2) * dl.powi(4);
    (e, nn)
}

fn num(t: Option<&str>) -> Option<f64> {
    t?.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).find(|s| !s.is_empty())?.parse().ok()
}

/// (height, min_height) in half-metre u8s from a tag lookup.
fn heights<'a>(tag: impl Fn(&str) -> Option<&'a str>) -> (u8, u8) {
    let h = num(tag("height")).or_else(|| num(tag("building:levels")).map(|l| l * 3.0)).filter(|&h| h > 0.0).unwrap_or(8.0);
    let mh = num(tag("min_height")).unwrap_or(0.0).max(0.0);
    ((h.max(2.0) * 2.0).min(255.0) as u8, (mh * 2.0).min(255.0) as u8)
}

struct Way {
    refs: Vec<i64>,
    h: u8,
    mh: u8,
    name: Option<String>,
    roof: bool,
}

/// clip a roof triangle (km) at cell bounds (sutherland-hodgman per overlapped
/// cell), fan the convex piece, quantise to u16 cell-local — the 16 B analogue
/// of Grid::clip for edges.
fn clip_tri(g: &Grid, t: [(f64, f64); 3], mut emit: impl FnMut(u32, [u16; 6])) {
    let (x0, x1) = t.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
    let (y0, y1) = t.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
    let cl = |v: f64, o: f64, n: usize| (((v - o) / g.cell).floor() as i64).clamp(0, n as i64 - 1);
    for r in cl(y0, g.miny, g.nr)..=cl(y1, g.miny, g.nr) {
        for c in cl(x0, g.minx, g.nc)..=cl(x1, g.minx, g.nc) {
            let (ox, oy) = (g.minx + c as f64 * g.cell, g.miny + r as f64 * g.cell);
            let mut poly: Vec<(f64, f64)> = t.to_vec();
            for s in 0..4 {
                let lim = [ox, ox + g.cell, oy, oy + g.cell][s];
                let inside = |p: &(f64, f64)| if s < 2 { (p.0 - lim) * [1.0, -1.0][s] >= 0.0 } else { (p.1 - lim) * [1.0, -1.0][s - 2] >= 0.0 };
                let hit = |a: (f64, f64), b: (f64, f64)| {
                    let t = if s < 2 { (lim - a.0) / (b.0 - a.0) } else { (lim - a.1) / (b.1 - a.1) };
                    (a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1))
                };
                let mut out = Vec::new();
                for k in 0..poly.len() {
                    let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
                    match (inside(&a), inside(&b)) {
                        (true, true) => out.push(b),
                        (true, false) => out.push(hit(a, b)),
                        (false, true) => { out.push(hit(a, b)); out.push(b) }
                        _ => {}
                    }
                }
                poly = out;
                if poly.len() < 3 { break }
            }
            if poly.len() < 3 { continue }
            let qz = |v: f64, o: f64| ((v - o) / g.cell * 65535.0).round().clamp(0.0, 65535.0) as u16;
            let q: Vec<[u16; 2]> = poly.iter().map(|p| [qz(p.0, ox), qz(p.1, oy)]).collect();
            for k in 1..q.len() - 1 {
                let ar = (q[k][0] as i64 - q[0][0] as i64) * (q[k + 1][1] as i64 - q[0][1] as i64)
                    - (q[k][1] as i64 - q[0][1] as i64) * (q[k + 1][0] as i64 - q[0][0] as i64);
                if ar != 0 {
                    emit(c as u32 + g.nc as u32 * r as u32, [q[0][0], q[0][1], q[k][0], q[k][1], q[k + 1][0], q[k + 1][1]]);
                }
            }
        }
    }
}

pub fn write(m: &MapCfg) {
    let g = Grid::new(m);
    let reader = || ElementReader::from_path(PBF).expect("data/england-latest.osm.pbf — run scripts/fetch-buildings.sh");

    // pass 1: building multipolygon relations → member way id → inherited tags.
    // members of relations with inner rings are also flagged roofless — we never
    // assemble rings, so triangulating those outers would slab over courtyards.
    eprintln!("  buildings: relations…");
    let (rel, holey): (HashMap<i64, (u8, u8, Option<String>)>, HashSet<i64>) = reader()
        .par_map_reduce(
            |el| {
                let mut out = (HashMap::new(), HashSet::new());
                if let Element::Relation(r) = el {
                    let tag = |k: &str| r.tags().find(|(a, _)| *a == k).map(|(_, v)| v);
                    if tag("building").is_some() {
                        let (h, mh) = heights(tag);
                        let name = tag("name").map(str::to_string);
                        let ways = || r.members().filter(|mb| mb.member_type == osmpbf::RelMemberType::Way);
                        for mb in ways() {
                            out.0.insert(mb.member_id, (h, mh, name.clone()));
                        }
                        if ways().any(|mb| mb.role().map_or(false, |r| r == "inner")) {
                            out.1.extend(ways().map(|mb| mb.member_id));
                        }
                    }
                }
                out
            },
            Default::default,
            |mut a, b: (HashMap<_, _>, HashSet<_>)| {
                a.0.extend(b.0);
                a.1.extend(b.1);
                a
            },
        )
        .unwrap();

    // pass 2: building ways (own tag, or inherited from a relation)
    eprintln!("  buildings: ways…");
    let ways: Vec<Way> = reader()
        .par_map_reduce(
            |el| {
                let mut out = Vec::new();
                if let Element::Way(w) = el {
                    let tag = |k: &str| w.tags().find(|(a, _)| *a == k).map(|(_, v)| v);
                    let own = tag("building").is_some();
                    if own || rel.contains_key(&w.id()) {
                        let (h, mh) = if own { heights(tag) } else { let r = &rel[&w.id()]; (r.0, r.1) };
                        let name = tag("name").map(str::to_string)
                            .or_else(|| rel.get(&w.id()).and_then(|r| r.2.clone()));
                        let refs: Vec<i64> = w.refs().collect();
                        let roof = refs.len() > 3 && refs.first() == refs.last() && !holey.contains(&w.id());
                        out.push(Way { refs, h, mh, name, roof });
                    }
                }
                out
            },
            Vec::new,
            |mut a, mut b| {
                a.append(&mut b);
                a
            },
        )
        .unwrap();

    // pass 3: node coords for exactly the ids those ways use (sorted-array join)
    eprintln!("  buildings: {} ways; nodes…", ways.len());
    let mut ids: Vec<i64> = ways.iter().flat_map(|w| w.refs.iter().copied()).collect();
    ids.par_sort_unstable();
    ids.dedup();
    let hits: Vec<(u32, [f32; 2])> = reader()
        .par_map_reduce(
            |el| {
                let mut out = Vec::new();
                let mut push = |id, lon: f64, lat: f64| {
                    if let Ok(i) = ids.binary_search(&id) {
                        out.push((i as u32, [lon as f32, lat as f32]));
                    }
                };
                match el {
                    Element::Node(n) => push(n.id(), n.lon(), n.lat()),
                    Element::DenseNode(n) => push(n.id(), n.lon(), n.lat()),
                    _ => {}
                }
                out
            },
            Vec::new,
            |mut a, mut b| {
                a.append(&mut b);
                a
            },
        )
        .unwrap();
    let mut coord = vec![[f32::NAN; 2]; ids.len()];
    for (i, c) in hits {
        coord[i as usize] = c;
    }
    // project every unique node once (adjacent footprints share nodes)
    eprintln!("  buildings: projecting {} node ids…", ids.len());
    let coord: Vec<(f64, f64)> = coord
        .par_iter()
        .map(|c| {
            if c[0].is_finite() {
                let (e, n) = bng(c[0] as f64, c[1] as f64);
                (e / 1000.0, n / 1000.0)
            } else {
                (f64::NAN, f64::NAN)
            }
        })
        .collect();

    // clip, bin — walls, earcut roofs, and the named-building click rows
    let (mut recs, mut roofs, names): (Vec<(u32, [u16; 4], u8, u8)>, Vec<(u32, [u16; 6], u8)>, Vec<(u32, String)>) = ways
        .par_iter()
        .fold(
            || (Vec::new(), Vec::new(), Vec::new()),
            |(mut recs, mut roofs, mut names), w| {
                let pts: Vec<(f64, f64)> = w
                    .refs
                    .iter()
                    .filter_map(|r| {
                        let p = coord[ids.binary_search(r).ok()?];
                        p.0.is_finite().then_some(p)
                    })
                    .collect();
                let inside = |p: &(f64, f64)| {
                    p.0 >= g.minx && p.0 < g.minx + g.nc as f64 * g.cell && p.1 >= g.miny && p.1 < g.miny + g.nr as f64 * g.cell
                };
                if pts.len() < 2 || !pts.iter().any(inside) {
                    return (recs, roofs, names);
                }
                for w2 in pts.windows(2) {
                    g.clip(w2[0], w2[1], |c, q| recs.push((c, q, w.h, w.mh)));
                }
                if w.roof && pts.len() > 3 && pts.first() == pts.last() {
                    let ring = &pts[..pts.len() - 1];
                    let flat: Vec<f64> = ring.iter().flat_map(|p| [p.0, p.1]).collect();
                    for t in earcutr::earcut(&flat, &[], 2).unwrap_or_default().chunks(3) {
                        clip_tri(&g, [ring[t[0]], ring[t[1]], ring[t[2]]], |c, q| roofs.push((c, q, w.h)));
                    }
                }
                if let Some(name) = &w.name {
                    let (cx, cy) = (pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64,
                                    pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64);
                    let cl = |v: f64, o: f64, n: usize| (((v - o) / g.cell) as i64).clamp(0, n as i64 - 1) as u32;
                    let c = cl(cx, g.minx, g.nc) + g.nc as u32 * cl(cy, g.miny, g.nr);
                    let name = name.replace(['\t', '\n', '\r'], " ");
                    names.push((c, format!("{cx:.3}\t{cy:.3}\t{}\n", name.trim())));
                }
                (recs, roofs, names)
            },
        )
        .reduce(
            || (Vec::new(), Vec::new(), Vec::new()),
            |(mut a, mut b, mut c), (mut d, mut e, mut f)| {
                a.append(&mut d);
                b.append(&mut e);
                c.append(&mut f);
                (a, b, c)
            },
        );

    write_layer(&m.dir, "bldg", &mut recs, g.nc * g.nr);
    let path = |f: &str| format!("{}/{}", m.dir, f);

    // roofs: same sorted-blob + per-cell count contract, 16 B records
    roofs.par_sort_unstable_by_key(|e| e.0);
    let mut counts = vec![0u32; g.nc * g.nr];
    let mut w = BufWriter::new(File::create(path("roof.bin")).unwrap());
    for (c, q, h) in &roofs {
        counts[*c as usize] += 1;
        for v in q {
            w.write_all(&v.to_le_bytes()).unwrap();
        }
        w.write_all(&(*c as u16).to_le_bytes()).unwrap();
        w.write_all(&[*h, 0]).unwrap();
    }
    w.flush().unwrap();
    write_idx(&path("roof.idx"), &counts);

    // the click sidecar: rows sorted by cell + a byte offset per cell boundary
    let mut names = names;
    names.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut tofs = vec![0u32; g.nc * g.nr + 1];
    let mut tsv = File::create(path("bldg.tsv")).unwrap();
    let mut at = 0u32;
    let mut it = names.iter().peekable();
    for c in 0..g.nc as u32 * g.nr as u32 {
        tofs[c as usize] = at;
        while let Some((_, row)) = it.next_if(|(rc, _)| *rc == c) {
            tsv.write_all(row.as_bytes()).unwrap();
            at += row.len() as u32;
        }
    }
    tofs[g.nc * g.nr] = at;
    write_idx(&path("bldg.tofs"), &tofs);
    eprintln!("  buildings: {} edges, {} roof tris, {} named  ->  {}/bldg.* roof.*", recs.len(), roofs.len(), names.len(), m.dir);
}
