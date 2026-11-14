// SPDX-License-Identifier: MPL-2.0

//! Generates the bindings to libva and libvpx from the headers installed on the build host,
//! so that the layout of every structure is the one of the library the program links with.

use std::env;
use std::path::{Path, PathBuf};

/// The bindings of `header` to `out`, with the include paths of `library`.
fn generate(header: &str, library: &pkg_config::Library, allow: &[&str], out: &Path) {
    println!("cargo:rerun-if-changed={header}");
    let mut builder = bindgen::Builder::default()
        .header(header)
        .clang_args(
            library
                .include_paths
                .iter()
                .map(|path| format!("-I{}", path.display())),
        )
        .prepend_enum_name(false)
        .derive_default(true)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .wrap_unsafe_ops(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for pattern in allow {
        builder = builder
            .allowlist_function(pattern)
            .allowlist_type(pattern)
            .allowlist_var(pattern);
    }
    builder
        .generate()
        .unwrap_or_else(|error| panic!("bindings of {header}: {error}"))
        .write_to_file(out)
        .unwrap();
}

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let va = pkg_config::Config::new()
        .atleast_version("1.12")
        .probe("libva")
        .expect("libva development files (libva-devel, libva-dev)");
    pkg_config::probe_library("libva-drm").expect("libva-drm development files");
    generate(
        "wrapper.h",
        &va,
        &["va[A-Z].*", "_?VA[A-Z].*", "VA_.*"],
        &out.join("va.rs"),
    );
    // VP9 on the CPU: libvpx 1.8 brought row-based multithreading in real time.
    let vpx = pkg_config::Config::new()
        .atleast_version("1.8")
        .probe("vpx")
        .expect("libvpx development files (libvpx-devel, libvpx-dev)");
    generate(
        "vpx-wrapper.h",
        &vpx,
        &["vpx_.*", "VPX_.*", "VP8.*", "VP9.*", "vp8.*", "vp9.*"],
        &out.join("vpx.rs"),
    );
}
