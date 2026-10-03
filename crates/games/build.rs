//! Embeds games/*.yaml, so programs built from this crate (the probe, the
//! relay) work without a source checkout.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let games = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../games");
    println!("cargo:rerun-if-changed={}", games.display());

    let mut files: Vec<_> = fs::read_dir(&games)
        .expect("games/ exists")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .collect();
    files.sort();

    let mut out = String::from("/// Game definitions from games/, embedded at build time.\n");
    out.push_str("pub static EMBEDDED_GAMES: &[(&str, &str)] = &[\n");
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("UTF-8 file name");
        out.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");
    let dest = Path::new(&env::var("OUT_DIR").expect("OUT_DIR is set")).join("embedded_games.rs");
    fs::write(dest, out).expect("can write to OUT_DIR");
}
