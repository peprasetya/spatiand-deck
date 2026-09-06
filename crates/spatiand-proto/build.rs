//! Recompile when the protocol changes.
//!
//! `generate_server_code!` reads the XML at macro-expansion time, and cargo has no way to know
//! that: it watches source files and `Cargo.toml`, not whatever a proc macro happens to open.
//! Without this, editing the protocol and rebuilding produces the *old* bindings, silently —
//! which shows up as a request that is plainly in the XML and plainly not in the generated
//! enum, and sends you looking for a mistake in the XML that is not there.
fn main() {
    println!("cargo:rerun-if-changed=protocol/spatiand-xr-v1.xml");
}
