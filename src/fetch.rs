// source-data fetching. the obscure sources this tool ingests each need a bespoke
// download flow — the aes-zipped maps viewer bundle behind a dnv veracity (azure b2c)
// login, and the dft street manager permit archive on an open s3 bucket. both are
// network/shell shaped (cookie jars, streaming unzip), so they live as small posix
// scripts under scripts/ that the proven unix stack (curl, bsdtar, jq) drives; this
// module just wires config + credentials into them so `gdn-gis fetch-*` runs the
// whole flow with nothing downloaded by hand.

use std::process::Command;

use crate::Config;

fn script(f: &str) -> String {
    format!("{}/scripts/{}", env!("CARGO_MANIFEST_DIR"), f)
}

/// `fetch-maps` / `fetch-works`: run the matching script with paths + creds resolved
/// from the config, cli flags and environment.
pub fn run(cmd: &str, args: &[String], cfg: &Config) {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let st = match cmd {
        "fetch-maps" => {
            let user = flag("-u").or_else(|| std::env::var("VERACITY_USER").ok()).expect("veracity user: -u or $VERACITY_USER");
            let pass = flag("-p").or_else(|| std::env::var("VERACITY_PASS").ok()).expect("veracity pass: -p or $VERACITY_PASS");
            let out = flag("-o").or_else(|| cfg.zip.clone()).unwrap_or_else(|| "data/maps-viewer.zip".into());
            Command::new("sh").arg(script("fetch-maps.sh")).args([out, user, pass]).status()
        }
        _ => {
            let w = cfg.works.as_ref().expect("no [works] in config");
            Command::new("sh").arg(script("fetch-works.sh")).args([w.dir.clone(), w.bucket.clone(), w.fetch_match.clone()]).status()
        }
    };
    if !st.expect("run fetch script").success() {
        std::process::exit(1);
    }
}
