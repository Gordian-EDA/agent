//! The visual critic against the live gateway: a rendered sheet comes back with
//! a score anchored to the embedded human reference. Skipped unless
//! `GORDIAN_TEST_SCH` names a sheet to grade.

use std::path::PathBuf;

use gordian_core::{critic, render};

fn sheet_under_test() -> Option<(PathBuf, PathBuf)> {
    let sch = PathBuf::from(std::env::var("GORDIAN_TEST_SCH").ok()?);
    let config = gordian_core::platform::load_config().ok()?;
    let cli = config.kicad.cli_path?;
    (sch.is_file() && cli.is_file()).then_some((cli, sch))
}

#[tokio::test]
async fn a_rendered_sheet_is_graded_against_the_anchor() {
    let Some((cli, sch)) = sheet_under_test() else {
        return;
    };
    gordian_runtime::logging::init_stderr_only();
    let config = gordian_core::platform::load_config().unwrap();
    let client = gordian_core::GenaiProvider::from_config(&config.llm).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let rendered = render::sheet(
        &cli,
        &sch,
        &dir.path().join("s.png"),
        &dir.path().join("s_grid.png"),
    )
    .unwrap();

    let review = critic::review(&client, &rendered.clean, "U1=NE555, R1=1k", true)
        .await
        .unwrap()
        .expect("the critic returns a verdict");

    println!("{}", review.event());
    println!("{}", review.text());
    assert_eq!(review.samples.len(), critic::SAMPLES);
    assert!((1.0..=10.0).contains(&review.mean));
}
