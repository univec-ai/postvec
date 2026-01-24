// Compile the CANONICAL wire contract at the repository root — not a copy.
//
// postvec vendors the same file (postvec/proto/ninference.proto) and its
// proto-drift test keeps the two byte-identical, so the extension, the test
// fixture and this server always speak one protocol. Vendoring a fourth copy
// here would create a drift surface with no test behind it.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "../proto/ninference.proto";
    let proto_dir = "../proto";

    println!("cargo:rerun-if-changed={proto_file}");

    // The client stub is generated too. It is not used by the server, but it
    // is what lets the wire-contract tests dial this process the way postvec
    // does — the alternative is asserting on handler internals, which would
    // pass while the service was mounted wrong.
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
