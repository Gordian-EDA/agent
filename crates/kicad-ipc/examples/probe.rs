//! Smoke test: connect to a running KiCAD IPC server (native Rust, no Python) and
//! print its version. Needs KiCAD running with the API enabled (locally, or
//! `xvfb-run -a pcbnew <board>` headless).
//!
//! ```text
//! cargo run -p kicad-ipc --example probe
//! ```

fn main() {
    let mut k = match kicad_ipc::Kicad::connect() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("could not connect to KiCAD IPC: {e}");
            eprintln!(
                "(is KiCAD running with api.enable_server=true? socket: $KICAD_API_SOCKET or /tmp/kicad/api.sock)"
            );
            std::process::exit(1);
        }
    };
    match k.version() {
        Ok((maj, min, pat, full)) => println!("connected to KiCAD {maj}.{min}.{pat}  ({full})"),
        Err(e) => {
            eprintln!("version() failed: {e}");
            std::process::exit(1);
        }
    }
}
