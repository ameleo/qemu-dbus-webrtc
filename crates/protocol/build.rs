// SPDX-License-Identifier: MPL-2.0

//! Generate the Rust types of `proto/qdw.proto`, and with the `broker` feature the gRPC service
//! of `proto/broker.proto`. protox compiles the files in Rust, so building needs no `protoc` on
//! the system.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let descriptors = protox::compile(["qdw.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    // The broker service, with its gRPC client (the host's) and server (the broker's).
    #[cfg(feature = "broker")]
    tonic_prost_build::configure().compile_fds(protox::compile(["broker.proto"], ["proto"])?)?;
    Ok(())
}
