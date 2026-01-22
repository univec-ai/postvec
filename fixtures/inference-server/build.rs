// Compile the CANONICAL wire contract at the repository root — not a copy.
// postvec vendors the same file (postvec/proto/ninference.proto) and its
// proto-drift test keeps the two byte-identical, so the fixture and the
// extension always speak the same protocol.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "../../proto/ninference.proto";
    let proto_dir = "../../proto";

    println!("cargo:rerun-if-changed={proto_file}");

    tonic_build::configure()
        .build_client(false)
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
