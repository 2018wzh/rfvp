use shinki_emu_build::{BuildAsset, CoreBuildManifest, write_core_build_manifest};
use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing CARGO_MANIFEST_DIR")?)
        .join("../..")
        .canonicalize()?;
    let mut manifest = CoreBuildManifest::new("rfvp", "rust-cdylib")?;
    for path in [
        "LICENSE",
        "crates/shinki-emu-rfvp/THIRD_PARTY_NOTICES.md",
        "crates/shinki-emu-rfvp/MODIFICATIONS.md",
    ] {
        manifest.licenses.push(BuildAsset {
            source: root.join(path),
            dest: format!("licenses/rfvp/{path}"),
        });
    }
    write_core_build_manifest(&manifest)?;
    Ok(())
}
