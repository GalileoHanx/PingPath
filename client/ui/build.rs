//! Tauri's build step: reads tauri.conf.json, embeds the frontend, the icon
//! and the version information, and resolves the capabilities.
//!
//! `bundle.resources` ships pingpath-service.exe and wintun.dll from
//! `resources/`, which CI fills before it builds the installer. tauri-build
//! also copies them next to the binary, and fails when they are missing,
//! but `cargo check`, `cargo clippy` and a development run don't need them.
//! So when they aren't there, this build leaves them out of its own view of
//! the config and says so. The bundler reads tauri.conf.json itself, so an
//! installer can never be built without them.

use std::path::Path;

const RESOURCES: [&str; 2] = ["resources/pingpath-service.exe", "resources/wintun.dll"];

fn main() {
    // The folder, not the files: a missing file would rerun this script on
    // every build.
    println!("cargo:rerun-if-changed=resources");
    let missing: Vec<&str> = RESOURCES
        .into_iter()
        .filter(|path| !Path::new(path).is_file())
        .collect();
    // TAURI_CONFIG is set by `tauri build --config ...`: that is a real
    // bundle, which must have its resources.
    if !missing.is_empty() && std::env::var_os("TAURI_CONFIG").is_none() {
        println!(
            "cargo:warning=building without {} (see resources/README.md); fine for checks and development, not for an installer",
            missing.join(" and ")
        );
        set_env("TAURI_CONFIG", r#"{"bundle":{"resources":null}}"#);
    }
    tauri_build::build();
}

#[allow(unsafe_code)]
fn set_env(key: &str, value: &str) {
    // SAFETY: a build script is one thread, and nothing else is reading or
    // changing the environment while this runs; tauri-build reads the
    // variable afterwards on the same thread.
    unsafe { std::env::set_var(key, value) }
}
