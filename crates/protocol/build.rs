// SPDX-License-Identifier: MPL-2.0

//! Generate the Rust types of `proto/qdw.proto`. protox compiles the file in Rust, so building
//! needs no `protoc` on the system.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let descriptors = protox::compile(["qdw.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    Ok(())
}
