// SPDX-License-Identifier: MPL-2.0

//! Generates the bindings to libopus from the headers installed on the build host.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=opus-wrapper.h");
    let opus = pkg_config::Config::new()
        .atleast_version("1.3")
        .probe("opus")
        .expect("libopus development files (opus-devel, libopus-dev)");
    let bindings = bindgen::Builder::default()
        .header("opus-wrapper.h")
        .clang_args(
            opus.include_paths
                .iter()
                .map(|path| format!("-I{}", path.display())),
        )
        .allowlist_function("opus_.*")
        .allowlist_type("Opus.*")
        .allowlist_var("OPUS_.*")
        .rust_edition(bindgen::RustEdition::Edition2024)
        .wrap_unsafe_ops(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindings to libopus");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out.join("opus.rs")).unwrap();
}
