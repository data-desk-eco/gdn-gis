// clean geoparquet from the gdn's open gpi (gas pipe infrastructure) release —
// the public counterpart of the maps-viewer extraction. normalises the vocabulary
// to match cadent_gas_network.parquet (diameter in mm, material labels, host main
// of an insertion, pressure code + label) and writes proper geoparquet metadata
// (wkb multilinestrings, ogc:crs84, bbox) for local duckdb analysis.

use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, BooleanArray, Float64Array, StringArray};
use arrow::datatypes::Float64Type;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct PipesCfg {
    file: String, // the gpi_open parquet as published (wkb geometry, crs84)
    #[serde(default = "d_out")]
    out: String,
}
fn d_out() -> String {
    "dist/pipes.parquet".into()
}

/// fold every vertex of a little-endian wkb (multi)linestring into lo/hi.
fn scan(w: &[u8], lo: &mut [f64; 2], hi: &mut [f64; 2]) {
    let u = |o: usize| u32::from_le_bytes(w[o..o + 4].try_into().unwrap()) as usize;
    let mut pts = |o: usize| {
        let n = u(o);
        for c in w[o + 4..o + 4 + 16 * n].chunks_exact(16) {
            let (x, y) = (f64::from_le_bytes(c[..8].try_into().unwrap()), f64::from_le_bytes(c[8..].try_into().unwrap()));
            (lo[0], lo[1], hi[0], hi[1]) = (lo[0].min(x), lo[1].min(y), hi[0].max(x), hi[1].max(y));
        }
        o + 4 + 16 * n
    };
    match (w.first(), u(1)) {
        (Some(1), 2) => {
            pts(5);
        }
        (Some(1), 5) => {
            let mut o = 9;
            for _ in 0..u(5) {
                o = pts(o + 5); // skip each member's byte-order + type tag
            }
        }
        _ => {}
    }
}

/// diameter in mm from a gpi value + unit code (I = inches; UN/absent falls back
/// on magnitude, mirroring dia_mm's convention for the maps-viewer screentips).
fn mm(v: Option<f64>, u: Option<&str>) -> Option<f64> {
    let v = v?;
    match u {
        Some("I") => Some((v * 25.4 * 10.0).round() / 10.0),
        Some("MM") => Some(v),
        _ if v >= 100.0 => Some(v),
        _ => None,
    }
}

pub fn write(p: &PipesCfg, mats: &HashMap<String, String>) {
    let rd = ParquetRecordBatchReaderBuilder::try_new(File::open(&p.file).expect("open [pipes] file")).unwrap().build().unwrap();
    let mut batches = Vec::new();
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    let mut n = 0usize;
    let mat = |c: Option<&str>| c.map(|c| mats.get(c).cloned().unwrap_or_else(|| c.to_lowercase()));
    for b in rd {
        let b = b.unwrap();
        let k = b.num_rows();
        n += k;
        let col = |name: &str| b.column_by_name(name).unwrap_or_else(|| panic!("no column {name}"));
        let s = |name: &str| col(name).as_string::<i32>().clone();
        let f = |name: &str| col(name).as_primitive::<Float64Type>().clone();
        let sv = |a: &StringArray, i: usize| a.is_valid(i).then(|| a.value(i).to_string());
        let (pr, ma, di, du, cm, cd, cu, ag) = (
            s("pressure"), s("material"), f("diameter"), s("diam_unit"), s("carr_mat"), f("carr_dia"), s("carr_di_un"), s("ag_ind"),
        );
        let geo = col("geo_shape").as_binary::<i32>();
        let (mut pcode, mut plabel) = (Vec::with_capacity(k), Vec::with_capacity(k));
        let (mut dia, mut hdia) = (Vec::with_capacity(k), Vec::with_capacity(k));
        let (mut mlab, mut hmat, mut ins, mut ab) =
            (Vec::with_capacity(k), Vec::with_capacity(k), Vec::with_capacity(k), Vec::with_capacity(k));
        for i in 0..k {
            let pc = sv(&pr, i).unwrap_or_default().to_lowercase();
            plabel.push(match pc.as_str() {
                "lp" => "low pressure".to_string(),
                "mp" => "medium pressure".to_string(),
                c => c.to_string(),
            });
            pcode.push(pc);
            mlab.push(mat(ma.is_valid(i).then(|| ma.value(i))));
            dia.push(mm(di.is_valid(i).then(|| di.value(i)), du.is_valid(i).then(|| du.value(i))));
            let hm = cm.is_valid(i).then(|| cm.value(i));
            ins.push(Some(hm.is_some()));
            hmat.push(mat(hm));
            hdia.push(mm(cd.is_valid(i).then(|| cd.value(i)), cu.is_valid(i).then(|| cu.value(i))));
            ab.push(Some(sv(&ag, i).as_deref() == Some("True")));
            scan(geo.value(i), &mut lo, &mut hi);
        }
        batches.push(vec![
            ("asset_id".to_string(), col("asset_id").clone()),
            ("type".into(), col("type").clone()),
            ("pressure_code".into(), Arc::new(StringArray::from(pcode)) as ArrayRef),
            ("pressure".into(), Arc::new(StringArray::from(plabel)) as _),
            ("diameter_mm".into(), Arc::new(Float64Array::from(dia)) as _),
            ("material".into(), Arc::new(StringArray::from(mlab)) as _),
            ("host_diameter_mm".into(), Arc::new(Float64Array::from(hdia)) as _),
            ("host_material".into(), Arc::new(StringArray::from(hmat)) as _),
            ("inserted".into(), Arc::new(BooleanArray::from(ins)) as _),
            ("above_ground".into(), Arc::new(BooleanArray::from(ab)) as _),
            ("depth".into(), col("depth").clone()),
            ("inst_date".into(), col("inst_date").clone()),
            ("geometry".into(), col("geo_shape").clone()),
        ]);
    }
    // crs84 (the release's native crs); null crs means ogc:crs84 per the geoparquet spec
    crate::geoparquet(&p.out, batches, r#""LineString","MultiLineString""#, "null", [lo[0], lo[1], hi[0], hi[1]]);
    eprintln!("  pipes: {} features  ->  {}", n, p.out);
}
