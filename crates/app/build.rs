//! Makes sure the embedded phone page exists. The real one comes from
//! `cargo xtask web`; without it a stub page explains how to build it.

use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("web-dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if fs::metadata(dist.join("pkg/web_bg.wasm")).is_ok_and(|m| m.len() > 0) {
        return;
    }
    println!("cargo:warning=phone page not built; run `cargo xtask web` (embedding a stub page)");
    fs::create_dir_all(dist.join("pkg")).unwrap();
    fs::write(
        dist.join("index.html"),
        "<!doctype html><title>iPhone Webcam</title><p>The phone page wasn't built into this binary. \
         Run <code>cargo xtask web</code> and rebuild.</p>",
    )
    .unwrap();
    fs::write(dist.join("pkg/web.js"), "").unwrap();
    fs::write(dist.join("pkg/web_bg.wasm"), "").unwrap();
}
