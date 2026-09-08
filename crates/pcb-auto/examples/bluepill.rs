//! End-to-end auto-layout of a Blue Pill: schematic (or a prebuilt staging board) in, placed,
//! routed, poured and DRC-clean `.kicad_pcb` plus front/back renders out.
//!
//! ```text
//! cargo run -p pcb-auto --example bluepill -- [--sch <file.kicad_sch> | --board <file.kicad_pcb>] [--out <dir>]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use kicad::KicadInstallation;
use pcb_auto::pipeline::{auto_layout, AutoOptions, Outline};
use pcb_auto::render::{render, Side};
use pcb_auto::schematic::board_from_schematic;

fn main() -> anyhow::Result<()> {
    let mut sch: Option<PathBuf> = None;
    let mut board: Option<PathBuf> = None;
    let mut out = PathBuf::from("target/bluepill");
    let mut timeout = 90u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sch" => sch = args.next().map(PathBuf::from),
            "--board" => board = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from).unwrap_or(out),
            "--timeout" => timeout = args.next().and_then(|v| v.parse().ok()).unwrap_or(timeout),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let kicad = KicadInstallation::detect()
        .ok_or_else(|| anyhow::anyhow!("no KiCad 10 installation found"))?;
    std::fs::create_dir_all(&out)?;
    let pcb = out.join("bluepill.kicad_pcb");

    let t0 = Instant::now();
    if let Some(sch) = sch {
        let parts = board_from_schematic(&kicad, &sch, &pcb)?;
        println!("staged {parts} parts from {} in {:.1}s", sch.display(), t0.elapsed().as_secs_f64());
    } else if let Some(src) = board {
        std::fs::copy(&src, &pcb)?;
        let pro = src.with_extension("kicad_pro");
        if pro.is_file() {
            let _ = std::fs::copy(&pro, pcb.with_extension("kicad_pro"));
        }
    } else {
        anyhow::bail!("pass --sch <schematic> or --board <staging board>");
    }

    let opts = AutoOptions {
        outline: Outline::Suggest,
        holes: 0,
        layers: 2,
        edge_for: BTreeMap::from([
            ("J2".into(), "left".into()),
            ("J3".into(), "right".into()),
            ("J1".into(), "top".into()),
        ]),
        gnd_zone: true,
        timeout_s: timeout,
    };
    let t1 = Instant::now();
    let report = auto_layout(&kicad, &pcb, &opts)?;
    println!(
        "auto_layout: ok={} completion={:.3} unrouted={} drc_errors={} drc_warnings={} outline={:.1}x{:.1} mm in {:.1}s (layout {:.1}s)",
        report.ok,
        report.completion,
        report.unrouted,
        report.drc_errors,
        report.drc_warnings,
        report.outline_mm.0,
        report.outline_mm.1,
        report.seconds,
        t1.elapsed().as_secs_f64(),
    );
    for n in &report.notes {
        println!("  - {n}");
    }
    for (side, name) in [(Side::Front, "front"), (Side::Back, "back")] {
        let png = out.join(format!("bluepill-{name}.png"));
        render(&kicad, &pcb, &png, side)?;
        println!("render: {}", png.display());
    }
    println!("board: {}", pcb.display());
    Ok(())
}
