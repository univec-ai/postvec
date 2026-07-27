// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

// Compile the repo-root proto, not a vendored copy. postvec vendors the same
// file and a drift test keeps them byte-identical.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "../proto/ninference.proto";
    let proto_dir = "../proto";

    println!("cargo:rerun-if-changed={proto_file}");

    // Generate the client stub too so wire tests can dial this process.
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
