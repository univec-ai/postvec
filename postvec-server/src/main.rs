//! Thin binary wrapper. Everything lives in the library so the HTTP and
//! admin surfaces can be exercised by integration tests against a real
//! router rather than by asserting on handler internals.

use std::process::ExitCode;

fn main() -> ExitCode {
    postvec_server::run()
}
