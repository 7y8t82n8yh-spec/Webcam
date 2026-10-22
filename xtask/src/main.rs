//! Build tasks, run with `cargo xtask <task>`.
//!
//! * `web` - compile the phone page (crates/web) to WebAssembly and put it, with its
//!   HTML, into crates/app/web-dist/ where the app embeds it. Needs the
//!   `wasm32-unknown-unknown` target and `wasm-bindgen-cli` at the same version as the
//!   `wasm-bindgen` crate (see Cargo.lock).

use std::path::{Path, PathBuf};
use std::process::{exit, Command};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run(cmd: &mut Command) {
    eprintln!("> {cmd:?}");
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("failed: {status}");
            exit(1);
        }
        Err(e) => {
            eprintln!("could not run {:?}: {e}", cmd.get_program());
            exit(1);
        }
    }
}

fn web() {
    let root = root();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(Command::new(&cargo).current_dir(&root).args([
        "build",
        "--release",
        "-p",
        "web",
        "--target",
        "wasm32-unknown-unknown",
    ]));
    let dist = root.join("crates/app/web-dist");
    std::fs::create_dir_all(dist.join("pkg")).unwrap();
    run(Command::new("wasm-bindgen")
        .arg(root.join("target/wasm32-unknown-unknown/release/web.wasm"))
        .args(["--target", "web", "--no-typescript", "--out-name", "web", "--out-dir"])
        .arg(dist.join("pkg")));
    std::fs::copy(root.join("crates/web/static/index.html"), dist.join("index.html")).unwrap();
    let size = std::fs::metadata(dist.join("pkg/web_bg.wasm")).map(|m| m.len()).unwrap_or(0);
    eprintln!("phone page built into {} ({} KB of WebAssembly)", dist.display(), size / 1024);
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("web") => web(),
        _ => {
            eprintln!("usage: cargo xtask web");
            exit(2);
        }
    }
}
