fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "proto/ninference.proto";
    let proto_dir = "proto";

    println!("cargo:rerun-if-changed={proto_file}");

    // Server stubs are only needed by embedded mode (the background worker
    // hosts a loopback gRPC server so connection backends stay thin clients).
    let build_server = std::env::var_os("CARGO_FEATURE_EMBEDDED").is_some();

    tonic_build::configure()
        .build_server(build_server)
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
