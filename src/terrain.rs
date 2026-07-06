// terrain artefacts (gdn-gis --terrain). two tiers against the same 2 km grid,
// one encoding: u16 = elevation decimetres + 1000 (fens sit below sea level),
// rows south→north.
//
//   terr0.bin  resident coarse relief: posts at 200 m over the whole grid
//              (map.json t0.nx × t0.ny), from os terrain 50 (data/terr50.zip,
//              fetch-terrain.sh).
//   terr1.bin  paged detail: 251×251 posts at 8 m per populated cell, from the
//              ea 1 m composite dtm crawl (data/terr1/<E>_<N>, cell sw corner
//              in km — grid-independent names, fetch-lidar.py). fixed-size
//              records in ascending cell order; nodata holes filled from the
//              os 50 m grid so the client never sees a sentinel. the ea wcs is
//              england-only: scotland cells fall back to the coarse tier.
//   terr1.idx  presence bitmap (lsb-first), 1 bit per cell; the client rank-sums
//              it — offset = rank(cell) × 251×251×2. absent = flat-at-coarse.

use std::fs::File;
use std::io::{BufWriter, Read, Write};

use crate::map::MapCfg;

// the one home of the terrain layout constants — map.rs writes them into map.json
pub(crate) const P: usize = 251; // detail posts per cell edge (8 m, edges shared)
pub(crate) const STEP: f64 = 0.2; // coarse post spacing, km

/// tier-0 post counts (x, y) over the map grid.
pub(crate) fn t0_dims(m: &MapCfg) -> (usize, usize) {
    let k = (m.cell_km / STEP).round() as usize;
    (m.ncols * k + 1, m.nrows * k + 1)
}

/// os terrain 50 as one 50 m national post grid over the map extent.
struct Master {
    g: Vec<u16>,
    nx: usize,
    ny: usize,
    minx: f64, // metres
    miny: f64,
}
impl Master {
    fn load(m: &MapCfg) -> Master {
        let (minx, miny) = (m.origin[0] * 1000.0, m.origin[1] * 1000.0);
        let post = |n: usize| (n as f64 * m.cell_km * 1000.0 / 50.0) as usize + 1;
        let (nx, ny) = (post(m.ncols), post(m.nrows));
        let mut g = vec![1000u16; nx * ny]; // default sea level
        let f = File::open("data/terr50.zip").expect("data/terr50.zip — run scripts/fetch-terrain.sh");
        let mut arch = zip::ZipArchive::new(std::io::BufReader::new(f)).unwrap();
        for i in 0..arch.len() {
            let mut e = arch.by_index(i).unwrap();
            if !e.name().ends_with(".zip") {
                continue;
            }
            let mut buf = Vec::with_capacity(e.size() as usize);
            e.read_to_end(&mut buf).unwrap();
            let mut inner = zip::ZipArchive::new(std::io::Cursor::new(buf)).unwrap();
            let Some(j) = (0..inner.len()).find(|&j| inner.name_for_index(j).is_some_and(|n| n.ends_with(".asc"))) else { continue };
            let mut txt = String::new();
            inner.by_index(j).unwrap().read_to_string(&mut txt).unwrap();
            let mut it = txt.split_ascii_whitespace();
            let mut hdr: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
            while let Some(k) = it.next() {
                if k.parse::<f64>().is_ok() {
                    // first bare number = first grid value; headers are done
                    let (nc, nr) = (hdr["ncols"] as usize, hdr["nrows"] as usize);
                    let (xll, yll, cs) = (hdr["xllcorner"], hdr["yllcorner"], hdr["cellsize"]);
                    if xll + nc as f64 * cs <= minx || xll >= minx + nx as f64 * 50.0
                        || yll + nr as f64 * cs <= miny || yll >= miny + ny as f64 * 50.0 {
                        break;
                    }
                    let mut vals = vec![k.parse::<f64>().unwrap()];
                    vals.extend(it.by_ref().filter_map(|v| v.parse::<f64>().ok()));
                    // asc rows run north→south; sample the asc cell each 50 m post falls in
                    for r in 0..nr {
                        let y = yll + (nr - 1 - r) as f64 * cs;
                        let gy: f64 = ((y - miny) / 50.0).round();
                        if gy < 0.0 || gy >= ny as f64 {
                            continue;
                        }
                        for c in 0..nc {
                            let x = xll + c as f64 * cs;
                            let gx: f64 = ((x - minx) / 50.0).round();
                            if gx < 0.0 || gx >= nx as f64 {
                                continue;
                            }
                            let v = vals[r * nc + c];
                            if v > -100.0 && v < 3000.0 {
                                g[gy as usize * nx + gx as usize] = (v * 10.0 + 1000.0).round() as u16;
                            }
                        }
                    }
                    break;
                }
                hdr.insert(k.to_lowercase(), it.next().unwrap().parse::<f64>().unwrap());
            }
        }
        Master { g, nx, ny, minx, miny }
    }
    /// bilinear elevation (u16 dm+1000) at bng metres.
    fn at(&self, x: f64, y: f64) -> u16 {
        let (gx, gy) = (((x - self.minx) / 50.0).clamp(0.0, (self.nx - 1) as f64 - 1e-9),
                        ((y - self.miny) / 50.0).clamp(0.0, (self.ny - 1) as f64 - 1e-9));
        let (x0, y0) = (gx as usize, gy as usize);
        let (dx, dy) = (gx - x0 as f64, gy - y0 as f64);
        let v = |i: usize, j: usize| self.g[(y0 + j).min(self.ny - 1) * self.nx + (x0 + i).min(self.nx - 1)] as f64;
        ((v(0, 0) * (1.0 - dx) + v(1, 0) * dx) * (1.0 - dy) + (v(0, 1) * (1.0 - dx) + v(1, 1) * dx) * dy)
            .round() as u16
    }
}

pub fn write(m: &MapCfg) {
    let master = Master::load(m);
    let path = |f: &str| format!("{}/{}", m.dir, f);
    std::fs::create_dir_all(&m.dir).ok();

    // tier 0: 200 m posts = every 4th 50 m post
    let (t0x, t0y) = t0_dims(m);
    let mut w = BufWriter::new(File::create(path("terr0.bin")).unwrap());
    for j in 0..t0y {
        for i in 0..t0x {
            w.write_all(&master.g[(j * 4).min(master.ny - 1) * master.nx + (i * 4).min(master.nx - 1)].to_le_bytes()).unwrap();
        }
    }
    w.flush().unwrap();

    // tier 1: ea lidar cells, holes filled from the 50 m grid
    let ncell = m.ncols * m.nrows;
    let mut bits = vec![0u8; ncell.div_ceil(8)];
    let mut w = BufWriter::new(File::create(path("terr1.bin")).unwrap());
    let (mut ncells, mut nfill) = (0usize, 0usize);
    for cid in 0..ncell {
        let (e0, n0) = (master.minx + (cid % m.ncols) as f64 * 2000.0, master.miny + (cid / m.ncols) as f64 * 2000.0);
        let Ok(raw) = std::fs::read(format!("data/terr1/{}_{}", e0 as i64 / 1000, n0 as i64 / 1000)) else { continue };
        if raw.len() != P * P * 2 {
            continue; // zero-byte marker: no ea coverage
        }
        let mut raw = raw;
        for k in 0..P * P {
            if raw[k * 2] == 0xff && raw[k * 2 + 1] == 0xff {
                // nodata → patch in place from the os 50 m grid
                nfill += 1;
                let v = master.at(e0 + (k % P) as f64 * 8.0, n0 + (k / P) as f64 * 8.0);
                raw[k * 2..k * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        w.write_all(&raw).unwrap();
        bits[cid / 8] |= 1 << (cid % 8);
        ncells += 1;
    }
    w.flush().unwrap();
    File::create(path("terr1.idx")).unwrap().write_all(&bits).unwrap();
    eprintln!("  terrain: terr0 {t0x}×{t0y} posts, terr1 {ncells} cells ({nfill} posts backfilled)  ->  {}/terr*", m.dir);
}
