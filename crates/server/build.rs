//! The web UI is embedded from `webui/dist`. Rust code must also build before
//! `npm run build` has ever run, so a stub page stands in for it.

use std::fs;
use std::path::Path;

const STUB: &str = "<!doctype html><title>jlinkd</title>\
<p>The web UI was not built. Run <code>npm install &amp;&amp; npm run build</code> in <code>webui/</code> \
and rebuild jlinkd.</p>";

fn main() {
    let dist = Path::new("../../webui/dist");
    if !dist.join("index.html").exists() {
        fs::create_dir_all(dist).expect("creating webui/dist");
        fs::write(dist.join("index.html"), STUB).expect("writing the stub page");
    }
    println!("cargo::rerun-if-changed=../../webui/dist");
    println!("cargo::rerun-if-changed=build.rs");
}
