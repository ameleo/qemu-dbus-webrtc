// SPDX-License-Identifier: MPL-2.0

//! Generates the bindings to libva from the headers installed on the build host, so that the
//! layout of every structure is the one of the library the program links with.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    let va = pkg_config::Config::new()
        .atleast_version("1.12")
        .probe("libva")
        .expect("libva development files (libva-devel, libva-dev)");
    pkg_config::probe_library("libva-drm").expect("libva-drm development files");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(
            va.include_paths
                .iter()
                .map(|path| format!("-I{}", path.display())),
        )
        .allowlist_function("va[A-Z].*")
        .allowlist_type("_?VA[A-Z].*")
        .allowlist_var("VA_.*")
        .prepend_enum_name(false)
        .derive_default(true)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .wrap_unsafe_ops(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindings to libva");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out.join("va.rs")).unwrap();
}
