//! Embeds every file under `assets/` into a `path → bytes` table.
//!
//! Adding an asset file requires no registration edit; this script regenerates
//! the table on every change (`docs/UI.md` §3).

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"));
    let assets_dir = manifest_dir.join("assets");
    println!("cargo::rerun-if-changed={}", assets_dir.display());

    let mut files = Vec::new();
    collect_files(&assets_dir, &assets_dir, &mut files);
    files.sort();

    let mut code = String::from(
        "/// Every embedded asset, as `(relative path, contents)`, sorted by path.\n\
         pub static ASSETS: &[(&str, &[u8])] = &[\n",
    );
    for relative in &files {
        let absolute = assets_dir.join(relative);
        println!("cargo::rerun-if-changed={}", absolute.display());
        code.push_str(&format!(
            "    ({relative:?}, include_bytes!({:?})),\n",
            absolute.display().to_string()
        ));
    }
    code.push_str("];\n");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .join("assets_generated.rs");
    fs::write(&out_path, code).expect("write assets_generated.rs");
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read assets directory") {
        let path = entry.expect("read assets directory entry").path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("asset path is under assets/");
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}
