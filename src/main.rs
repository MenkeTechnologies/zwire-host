//! Thin entry point — all logic lives in the library so other Rust apps can
//! embed the host directly. See [`zwire_host`] for the protocol and transports.
fn main() {
    zwire_host::run(std::env::args().skip(1).collect());
}
