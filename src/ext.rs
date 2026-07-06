// external open-data networks merged into the map artefacts beside the cadent
// extract. sgn's per-authority mains (scripts/fetch-sgn.sh) and the nts pipe
// corridors (scripts/fetch-nts.sh) arrive as one tsv row per feature —
// `wkt \t pressure \t material-code \t inst_date`. the nts corridors are
// constant-width buffer polygons, collapsed to their centreline (see
// `centreline`) so they draw as hairlines; other rings read as closed lines.
// pressure NTS marks the transmission network: map.rs tones it 9 (grey),
// always visible on the timeline.
// nts site boundaries become dist/sites.{f32,tsv}: centroid markers appended
// to the works instance buffer (flag 2) with a lazy click card.

use std::io::Write;

use serde::Deserialize;

use crate::{Geom, Row};

#[derive(Deserialize)]
pub struct ExtCfg {
    pipes: Vec<String>,
    sites: Option<String>,
}

/// every coordinate ring in a wkt body — linestrings and polygon rings alike:
/// the innermost paren groups are the point lists.
fn rings(w: &str) -> Vec<Vec<(f64, f64)>> {
    w.split('(')
        .filter_map(|g| {
            let pts: Vec<(f64, f64)> = g
                .split(')')
                .next()?
                .split(',')
                .filter_map(|p| {
                    let mut it = p.split_ascii_whitespace();
                    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
                })
                .collect();
            (pts.len() >= 2).then_some(pts)
        })
        .collect()
}

/// collapse a buffered corridor polygon (the nts shapes: a constant ~24 m
/// round-capped buffer of an unpublished centreline) back to that centreline.
/// each boundary vertex pairs with the nearest vertex on the *opposite* wall —
/// the global nearest excluding its own ring within a 90 m arc (so a vertex
/// looks across the corridor, not along it; sibling rings let loops pair an
/// outer wall with a hole). the pair midpoint is one axis point; walking the
/// vertices in ring order traces the axis, and keeping only pairs where the
/// vertex index precedes its partner's drops the mirror trace from the far
/// wall. a chain breaks only where successive axis points jump — a junction,
/// a cap, or a gap between corridors in a multipolygon.
fn centreline(g: Vec<Vec<(f64, f64)>>) -> Vec<Vec<(f64, f64)>> {
    fn d(a: (f64, f64), b: (f64, f64)) -> f64 {
        (a.0 - b.0).hypot(a.1 - b.1)
    }
    let rs: Vec<Vec<(f64, f64)>> = g
        .into_iter()
        .map(|mut r| {
            if r.len() > 1 && r.first() == r.last() {
                r.pop();
            }
            r
        })
        .collect();
    // cumulative arc length per ring, for the same-wall neighbour exclusion
    let arcs: Vec<Vec<f64>> = rs
        .iter()
        .map(|r| {
            let mut s = 0.0;
            (0..r.len())
                .map(|i| {
                    let a = s;
                    s += d(r[i], r[(i + 1) % r.len()]);
                    a
                })
                .collect()
        })
        .collect();
    let perim: Vec<f64> = rs.iter().zip(&arcs).map(|(r, a)| a.last().copied().unwrap_or(0.0) + if r.len() > 1 { d(r[r.len() - 1], r[0]) } else { 0.0 }).collect();
    let arc = |q: usize, a: usize, b: usize| {
        let g = (arcs[q][a] - arcs[q][b]).abs();
        g.min(perim[q] - g)
    };
    let mut out = Vec::new();
    for (ri, r) in rs.iter().enumerate() {
        let mut chain: Vec<(f64, f64)> = Vec::new();
        let mut close = |c: &mut Vec<(f64, f64)>| {
            if c.len() > 1 {
                out.push(std::mem::take(c));
            } else {
                c.clear();
            }
        };
        for i in 0..r.len() {
            let mut best: Option<(usize, usize, f64)> = None;
            for (qi, q) in rs.iter().enumerate() {
                for j in 0..q.len() {
                    if qi == ri && arc(ri, i, j) < 90.0 {
                        continue;
                    }
                    let dd = d(r[i], q[j]);
                    if dd < best.map_or(120.0, |b| b.2) {
                        best = Some((qi, j, dd));
                    }
                }
            }
            match best.filter(|&(qi, j, _)| (ri, i) < (qi, j)) {
                Some((qi, j, _)) => {
                    let mid = ((r[i].0 + rs[qi][j].0) / 2.0, (r[i].1 + rs[qi][j].1) / 2.0);
                    // break where the axis leaps — cap, junction, or corridor gap
                    if chain.last().is_some_and(|&p| d(p, mid) > 400.0) {
                        close(&mut chain);
                    }
                    chain.push(mid);
                }
                None => close(&mut chain),
            }
        }
        close(&mut chain);
    }
    out
}

pub fn rows(cfg: &ExtCfg) -> Vec<Row> {
    fn layer(p: &str) -> &str {
        match p {
            "LP" => "Low Pressure Mains & Plant",
            "MP" => "Medium Pressure Mains & Plant",
            "IP" => "Intermediate Pressure Mains & Plant",
            "HP" | "LHP" => "Local High Pressure Mains & Plant",
            other => other, // NTS (and anything new) keeps its own name
        }
    }
    let mut out = Vec::new();
    for f in &cfg.pipes {
        let Ok(txt) = std::fs::read_to_string(f) else {
            eprintln!("  ext: {f} missing — run scripts/fetch-sgn.sh / fetch-nts.sh");
            continue;
        };
        let n0 = out.len();
        out.extend(txt.lines().filter_map(|ln| {
            let mut c = ln.split('\t');
            let (w, p, m) = (c.next()?, c.next().unwrap_or(""), c.next().unwrap_or(""));
            let year = c.next().unwrap_or("").get(..4).and_then(|y| y.parse().ok()).unwrap_or(0);
            // nts corridors are constant-width buffer polygons — collapse them
            // to their centreline so they draw as hairlines like every other main
            let mut g = if p == "NTS" { centreline(rings(w)) } else { rings(w) };
            (!g.is_empty()).then(|| Row {
                fid: None,
                layer: layer(p).into(),
                // recreate a screentip so map.rs's one material vocabulary applies
                tip: (!m.is_empty()).then(|| format!("0MM {m}")),
                square: String::new(),
                facet: String::new(),
                src: None,
                date: None,
                year: Some(year),
                geom: if g.len() == 1 { Geom::Line(g.pop().unwrap()) } else { Geom::Multi(g) },
            })
        }));
        eprintln!("  ext: {} features from {f}", out.len() - n0);
    }
    out
}

/// nts sites: boundary centroid + `location \t facility` click card, ordered
/// south→north so the artefact is stable across rebuilds.
pub fn sites(cfg: &ExtCfg, dir: &str) {
    let Some(f) = &cfg.sites else { return };
    let Ok(txt) = std::fs::read_to_string(f) else {
        eprintln!("  ext: {f} missing — run scripts/fetch-nts.sh");
        return;
    };
    let mut rows: Vec<(f64, f64, String)> = txt
        .lines()
        .filter_map(|ln| {
            let mut c = ln.split('\t');
            let (w, fac, loc) = (c.next()?, c.next().unwrap_or(""), c.next().unwrap_or(""));
            let r = rings(w).into_iter().next()?;
            let n = r.len() as f64;
            let (x, y) = (r.iter().map(|p| p.0).sum::<f64>() / n / 1000.0, r.iter().map(|p| p.1).sum::<f64>() / n / 1000.0);
            // "Blanchland_4219" -> "Blanchland"; facility codes -> words
            let loc = loc.replace('_', " ");
            let loc = loc.trim_end_matches(|c: char| c.is_ascii_digit()).trim();
            let fac = match fac.trim() {
                "AGI" => "above-ground installation",
                "COMP" => "compressor station",
                "TCSITE" => "terminal",
                f => f,
            };
            Some((x, y, format!("{loc}\t{fac}")))
        })
        .collect();
    rows.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
    let mut wf = std::io::BufWriter::new(std::fs::File::create(format!("{dir}/sites.f32")).unwrap());
    let mut wt = std::io::BufWriter::new(std::fs::File::create(format!("{dir}/sites.tsv")).unwrap());
    for (x, y, det) in &rows {
        for v in [*x as f32, *y as f32, 0.0, 2.0] {
            wf.write_all(&v.to_le_bytes()).unwrap();
        }
        writeln!(wt, "{det}").unwrap();
    }
    wf.flush().unwrap();
    wt.flush().unwrap();
    eprintln!("  ext: {} sites  ->  {dir}/sites.*", rows.len());
}
