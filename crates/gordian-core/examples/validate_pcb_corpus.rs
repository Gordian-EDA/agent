//! Validate representative PCB seed boards through placement + routing.
//!
//! Default run:
//! `cargo run -p gordian-core --example validate_pcb_corpus --quiet`
//!
//! Use `--required` for the bounded acceptance set, `--all` to run every
//! `examples/pcb_circuits/*.json`, pass board names such as `power-buck
//! bga25-route`, or add `--router mesh-global` / `--router mesh-detail
//! --inspect-failed-nets` to isolate dense routing failures.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use drc_lint::lint::lint;
use gordian_core::tools_pcb::apply_direct_rescue_fallback;
use gordian_core::tools_pcb::corpus::{load_corpus_board, route_problem_for_placement};
use kicad_env::KicadEnv;
use kicad_footprint::FootprintCatalog;
use negotiated_mesh::crossing::{
    AssignedCrossing, AssignmentFailure, CellJob, CrossingAssignment, TerminalKind,
    assign_crossings,
};
use negotiated_mesh::detail::{self, DetailPassDiagnostic};
use negotiated_mesh::mesh::CapacityMesh;
use negotiated_mesh::pathing::global_route_with_mesh;
use negotiated_mesh::pipeline::{
    RouteAutoRun, RouteEngineAttempt, route_auto_with_diagnostics, route_detailed_with_global,
    route_mesh_with_diagnostics, route_sequential_with_diagnostics,
};
use negotiated_mesh::problem::{Point2, RouteProblem, RouteQuality, RouteResult, Router};
use negotiated_mesh::router::{GridAStarRouter, geometry_violations};

const DEFAULT_BOARDS: &[&str] = &[
    "rc-divider",
    "transistor-led-driver",
    "keepout-route",
    "rc-lowpass-chain",
];
const REQUIRED_BOARDS: &[&str] = &[
    "rc-divider",
    "transistor-led-driver",
    "keepout-route",
    "rc-lowpass-chain",
    "power-buck",
    "led-array",
    "bga25-route",
];

fn main() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let env = KicadEnv::detect().ok_or_else(|| {
        anyhow!(
            "KiCad libraries not found; set KICAD_SYMBOL_DIR/KICAD_FOOTPRINT_DIR or install KiCad"
        )
    })?;
    let catalog = FootprintCatalog::from_env(&env).context("loading footprint catalog")?;
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/pcb_circuits");
    let boards = resolve_boards(&corpus_dir, &args)?;

    println!(
        "board,parts,nets,layers,place_ms,route_ms,failed,lints,vias,wirelength_mm,attempts,slowest_engine,slowest_ms,status"
    );
    let mut failures = 0usize;
    for path in boards {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let started = Instant::now();
        let board = match load_corpus_board(&path, &catalog) {
            Ok(board) => board,
            Err(err) => {
                failures += 1;
                println!("{name},0,0,0,0,0,0,0,0,0.00,0,,0,LOAD_ERROR:{err}");
                continue;
            }
        };

        let placed = pcb_place::placement::place_board(&board.problem, &board.hints);
        let place_ms = started.elapsed().as_millis();
        if !placed.legal {
            failures += 1;
            println!(
                "{name},{},0,{},{},0,0,0,0,0.00,0,,0,PLACE_ILLEGAL",
                board.problem.parts.len(),
                board.problem.layer_count,
                place_ms
            );
            continue;
        }
        if args.place_only {
            println!(
                "{name},{},0,{},{},0,0,0,0,0.00,0,,0,PLACE_OK",
                board.problem.parts.len(),
                board.problem.layer_count,
                place_ms
            );
            continue;
        }

        let rp = route_problem_for_placement(&board, &placed.placements);
        if args.router == RouterMode::MeshGlobal || args.router == RouterMode::MeshAssign {
            let route_started = Instant::now();
            let mesh = CapacityMesh::build(&rp);
            let global = global_route_with_mesh(&rp, &mesh);
            let route_ms = route_started.elapsed().as_millis();
            if args.router == RouterMode::MeshAssign && global.is_feasible() {
                let assign_started = Instant::now();
                let assignment = assign_crossings(&rp, &mesh, &global.plan);
                let assign_ms = assign_started.elapsed().as_millis();
                let failed = assignment.failures.len();
                let status = if failed == 0 {
                    "ASSIGN_OK"
                } else {
                    failures += 1;
                    "ASSIGN_FAULT"
                };
                println!(
                    "{name},{},{},{},{},{},{},{},{},{:.2},{},mesh-assign,{},{}",
                    board.problem.parts.len(),
                    rp.connections.len(),
                    rp.layer_count,
                    place_ms,
                    route_ms + assign_ms,
                    failed,
                    0,
                    0,
                    0.0,
                    1,
                    assign_ms,
                    status
                );
                if args.verbose {
                    let max_terms = assignment
                        .jobs
                        .iter()
                        .map(|job| job.terminals.len())
                        .max()
                        .unwrap_or(0);
                    eprintln!(
                        "  {name}: assign jobs={} crossings={} failures={} max_terms={} global_ms={} assign_ms={}",
                        assignment.jobs.len(),
                        assignment.crossings.len(),
                        assignment.failures.len(),
                        max_terms,
                        route_ms,
                        assign_ms
                    );
                    for failure in assignment.failures.iter().take(12) {
                        eprintln!("  {name}: assign-failure={failure:?}");
                        if let AssignmentFailure::ViaSite { connection, leaf } = failure {
                            let rect = &mesh.leaves[*leaf].rect;
                            eprintln!(
                                "  {name}: assign-viasite leaf={} rect=({:.3},{:.3})-({:.3},{:.3}) connection={}",
                                leaf, rect.min_x, rect.min_y, rect.max_x, rect.max_y, connection
                            );
                            if let Some(conn) =
                                rp.connections.iter().find(|conn| conn.name == *connection)
                            {
                                let points = conn
                                    .points_to_connect
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, pt)| {
                                        format!("{idx}:({:.3},{:.3},{})", pt.x, pt.y, pt.layer.0)
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                eprintln!("  {name}: assign-viasite points={points}");
                            }
                        }
                    }
                }
                let inspect_nets = inspect_nets_with_optional_failures(
                    &args.inspect_nets,
                    args.inspect_failed_nets,
                    assignment
                        .failures
                        .iter()
                        .map(assignment_failure_connection)
                        .filter(|name| !name.is_empty()),
                );
                if args.inspect_detail_jobs {
                    dump_detail_job_inspection(name, &rp, &mesh, &assignment, &inspect_nets);
                }
                dump_route_inspection(name, &rp, None, &inspect_nets);
                continue;
            }
            let overflow_fault = usize::from(global.report.final_overflow > 0);
            let failed = global.report.unrouted.len() + overflow_fault;
            let status = if global.is_feasible() {
                "GLOBAL_OK"
            } else {
                failures += 1;
                "GLOBAL_FAULT"
            };
            println!(
                "{name},{},{},{},{},{},{},{},{},{:.2},{},{},{},{}",
                board.problem.parts.len(),
                rp.connections.len(),
                rp.layer_count,
                place_ms,
                route_ms,
                failed,
                0,
                0,
                0.0,
                1,
                if args.router == RouterMode::MeshAssign {
                    "mesh-assign"
                } else {
                    "mesh-global"
                },
                route_ms,
                status
            );
            if args.verbose {
                eprintln!(
                    "  {name}: global iterations={} overflow={} unrouted={}",
                    global.report.iterations,
                    global.report.final_overflow,
                    global.report.unrouted.len()
                );
                for hotspot in global.report.edge_hotspots.iter().take(8) {
                    eprintln!(
                        "  {name}: hotspot edge={} layer={} load={:.2} usage={} capacity={}",
                        hotspot.edge, hotspot.layer, hotspot.load, hotspot.usage, hotspot.capacity
                    );
                }
            }
            let inspect_nets = inspect_nets_with_optional_failures(
                &args.inspect_nets,
                args.inspect_failed_nets,
                global
                    .report
                    .unrouted
                    .iter()
                    .map(|failed| failed.connection.clone()),
            );
            if args.inspect_detail_jobs {
                dump_detail_job_inspection_for_problem(name, &rp, &inspect_nets);
            }
            dump_route_inspection(name, &rp, None, &inspect_nets);
            continue;
        }

        let route_started = Instant::now();
        let mut routed = route_with_mode(&rp, args.router);
        let direct_rescue_applied = args.router != RouterMode::MeshDetail
            && apply_direct_rescue_fallback(&rp, &mut routed.result);
        if direct_rescue_applied {
            routed
                .attempts
                .push(attempt_summary(&rp, &routed.result, 0));
        }
        let route_ms = route_started.elapsed().as_millis();
        let findings = lint(&rp, &routed.result.solution);
        let metrics = routed.result.solution.metrics();
        let inspect_nets = inspect_nets_with_optional_failures(
            &args.inspect_nets,
            args.inspect_failed_nets,
            routed
                .result
                .failed
                .iter()
                .map(|failed| failed.connection.clone()),
        );
        if args.inspect_detail_jobs {
            dump_detail_job_inspection_for_problem(name, &rp, &inspect_nets);
        }
        dump_route_inspection(name, &rp, Some(&routed.result), &inspect_nets);
        let failed = routed.result.failed.len();
        let (slowest_engine, slowest_ms) = routed
            .attempts
            .iter()
            .max_by_key(|attempt| attempt.elapsed_ms)
            .map(|attempt| (attempt.engine.as_str(), attempt.elapsed_ms))
            .unwrap_or(("", 0));
        let status = if failed == 0 && findings.is_empty() {
            "OK"
        } else {
            failures += 1;
            "ROUTE_FAULT"
        };
        println!(
            "{name},{},{},{},{},{},{},{},{},{:.2},{},{},{},{}",
            board.problem.parts.len(),
            rp.connections.len(),
            rp.layer_count,
            place_ms,
            route_ms,
            failed,
            findings.len(),
            metrics.via_count,
            metrics.wirelength,
            routed.attempts.len(),
            slowest_engine,
            slowest_ms,
            status
        );
        if args.verbose && status != "OK" {
            eprintln!("  {name}: failed={:?}", routed.result.failed);
            for failed in &routed.result.failed {
                if let Some(conn) = rp
                    .connections
                    .iter()
                    .find(|conn| conn.name == failed.connection)
                {
                    let points = conn
                        .points_to_connect
                        .iter()
                        .enumerate()
                        .map(|(idx, pt)| format!("{idx}:({:.3},{:.3},{})", pt.x, pt.y, pt.layer.0))
                        .collect::<Vec<_>>()
                        .join(" ");
                    eprintln!("  {name}: failed-net {} points={points}", conn.name);
                }
            }
            for finding in findings.iter().take(12) {
                eprintln!("  {name}: lint={finding:?}");
            }
            for attempt in &routed.attempts {
                let failed_names = attempt
                    .failed
                    .iter()
                    .map(|f| f.connection.as_str())
                    .collect::<Vec<_>>()
                    .join("|");
                eprintln!(
                    "  {name}: attempt engine={} elapsed_ms={} faults={} geom={} failed_nets={} vias={} wirelength={:.2} failed={}",
                    attempt.engine,
                    attempt.elapsed_ms,
                    attempt.fault_weight,
                    attempt.geometry_violations,
                    attempt.failed_nets,
                    attempt.vias,
                    attempt.wirelength,
                    failed_names
                );
            }
        }
    }

    if failures > 0 {
        Err(anyhow!("{failures} corpus board(s) failed validation"))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
struct Args {
    all: bool,
    required: bool,
    place_only: bool,
    verbose: bool,
    inspect_detail_jobs: bool,
    inspect_failed_nets: bool,
    router: RouterMode,
    inspect_nets: Vec<String>,
    names: Vec<String>,
}

impl Args {
    fn parse(values: impl Iterator<Item = String>) -> Result<Self> {
        let mut args = Args {
            all: false,
            required: false,
            place_only: false,
            verbose: false,
            inspect_detail_jobs: false,
            inspect_failed_nets: false,
            router: RouterMode::Auto,
            inspect_nets: Vec::new(),
            names: Vec::new(),
        };
        let mut values = values.peekable();
        while let Some(arg) = values.next() {
            match arg.as_str() {
                "--all" => args.all = true,
                "--required" => args.required = true,
                "--place-only" => args.place_only = true,
                "-v" | "--verbose" => args.verbose = true,
                "--inspect-detail-jobs" => args.inspect_detail_jobs = true,
                "--inspect-failed-nets" => args.inspect_failed_nets = true,
                "--router" => {
                    let value = values.next().ok_or_else(|| {
                        anyhow!(
                            "--router requires auto, mesh, mesh-detail, mesh-global, mesh-assign, sequential, grid, or astar"
                        )
                    })?;
                    args.router = RouterMode::parse(&value)?;
                }
                "--inspect-net" => {
                    let value = values.next().ok_or_else(|| {
                        anyhow!("--inspect-net requires a net name or comma list")
                    })?;
                    args.inspect_nets
                        .extend(value.split(',').filter_map(inspect_net_name));
                }
                _ if arg.starts_with("--inspect-net=") => {
                    let value = arg
                        .strip_prefix("--inspect-net=")
                        .expect("prefix was checked");
                    args.inspect_nets
                        .extend(value.split(',').filter_map(inspect_net_name));
                }
                "-h" | "--help" => {
                    println!(
                        "usage: validate_pcb_corpus [--required|--all] [--place-only] [--router auto|mesh|mesh-detail|mesh-global|mesh-assign|sequential|grid|astar] [--inspect-net NET[,NET...]] [--inspect-failed-nets] [--inspect-detail-jobs] [-v] [board ...]"
                    );
                    std::process::exit(0);
                }
                _ if arg.starts_with('-') => return Err(anyhow!("unknown argument `{arg}`")),
                _ => args.names.push(arg),
            }
        }
        if args.all && args.required {
            return Err(anyhow!("--required and --all are mutually exclusive"));
        }
        if args.required && !args.names.is_empty() {
            return Err(anyhow!(
                "--required cannot be combined with explicit board names"
            ));
        }
        Ok(args)
    }
}

fn inspect_net_name(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn inspect_nets_with_optional_failures<I>(
    explicit: &[String],
    include_failed: bool,
    failed: I,
) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut names = explicit.to_vec();
    if include_failed {
        for name in failed {
            if !names.iter().any(|existing| existing == &name) {
                names.push(name);
            }
        }
    }
    names
}

fn assignment_failure_connection(failure: &AssignmentFailure) -> String {
    match failure {
        AssignmentFailure::Overflow { .. } => String::new(),
        AssignmentFailure::ViaSite { connection, .. } => connection.clone(),
    }
}

fn dump_detail_job_inspection_for_problem(
    board_name: &str,
    problem: &RouteProblem,
    net_names: &[String],
) {
    let started = Instant::now();
    let mesh = CapacityMesh::build(problem);
    let global = global_route_with_mesh(problem, &mesh);
    let global_ms = started.elapsed().as_millis();
    if !global.is_feasible() {
        eprintln!(
            "  {board_name}: detail-jobs global infeasible iterations={} overflow={} unrouted={} global_ms={global_ms}",
            global.report.iterations,
            global.report.final_overflow,
            global.report.unrouted.len()
        );
        return;
    }
    let assign_started = Instant::now();
    let assignment = assign_crossings(problem, &mesh, &global.plan);
    eprintln!(
        "  {board_name}: detail-jobs global_ms={global_ms} assign_ms={} assignment_failures={}",
        assign_started.elapsed().as_millis(),
        assignment.failures.len()
    );
    dump_detail_job_inspection(board_name, problem, &mesh, &assignment, net_names);
}

fn dump_detail_job_inspection(
    board_name: &str,
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    assignment: &CrossingAssignment,
    net_names: &[String],
) {
    let max_terms = assignment
        .jobs
        .iter()
        .map(|job| job.terminals.len())
        .max()
        .unwrap_or(0);
    let via_jobs = assignment
        .jobs
        .iter()
        .filter(|job| job.terminals.iter().any(|t| t.kind == TerminalKind::Via))
        .count();
    let total_terms: usize = assignment.jobs.iter().map(|job| job.terminals.len()).sum();
    let avg_terms = if assignment.jobs.is_empty() {
        0.0
    } else {
        total_terms as f64 / assignment.jobs.len() as f64
    };
    eprintln!(
        "  {board_name}: detail-jobs jobs={} crossings={} failures={} max_terms={} avg_terms={avg_terms:.2} via_jobs={via_jobs}",
        assignment.jobs.len(),
        assignment.crossings.len(),
        assignment.failures.len(),
        max_terms
    );
    let (_, diagnostics) = detail::route_cells_with_diagnostics(problem, mesh, assignment);
    dump_detail_pass_diagnostics(board_name, &diagnostics);

    let selected_nets = selected_detail_job_nets(problem, assignment, net_names);
    for net_name in selected_nets {
        let jobs = assignment
            .jobs
            .iter()
            .filter(|job| job.connection == net_name)
            .collect::<Vec<_>>();
        let crossings = assignment
            .crossings
            .iter()
            .filter(|crossing| crossing.connection == net_name)
            .collect::<Vec<_>>();
        if jobs.is_empty() && crossings.is_empty() {
            eprintln!("  {board_name}: detail-jobs net={net_name} no assigned jobs/crossings");
            continue;
        }
        dump_detail_net_jobs(board_name, mesh, &net_name, &jobs, &crossings);
    }
}

fn dump_detail_pass_diagnostics(board_name: &str, diagnostics: &[DetailPassDiagnostic]) {
    for diagnostic in diagnostics {
        let failed = if diagnostic.failed.is_empty() {
            "-".to_owned()
        } else {
            diagnostic.failed.join("|")
        };
        eprintln!(
            "  {board_name}: detail-pass idx={} selected={} geom={} failed={} failed_pad_weight={} failed_nets={} order={}",
            diagnostic.index,
            diagnostic.selected,
            diagnostic.geometry,
            diagnostic.fail_count,
            diagnostic.failed_pad_weight,
            failed,
            diagnostic.net_order.join("|")
        );
    }
}

fn selected_detail_job_nets(
    problem: &RouteProblem,
    assignment: &CrossingAssignment,
    net_names: &[String],
) -> Vec<String> {
    if !net_names.is_empty() {
        return net_names.to_vec();
    }
    let mut metrics: Vec<_> = problem
        .connections
        .iter()
        .map(|conn| {
            let job_count = assignment
                .jobs
                .iter()
                .filter(|job| job.connection == conn.name)
                .count();
            let term_count = assignment
                .jobs
                .iter()
                .filter(|job| job.connection == conn.name)
                .map(|job| job.terminals.len())
                .sum::<usize>();
            let crossing_count = assignment
                .crossings
                .iter()
                .filter(|crossing| crossing.connection == conn.name)
                .count();
            (
                std::cmp::Reverse(term_count + crossing_count),
                job_count,
                conn.name.clone(),
            )
        })
        .collect();
    metrics.sort();
    metrics
        .into_iter()
        .take(INSPECT_LIMIT)
        .map(|(_, _, name)| name)
        .collect()
}

fn dump_detail_net_jobs(
    board_name: &str,
    mesh: &CapacityMesh,
    net_name: &str,
    jobs: &[&CellJob],
    crossings: &[&AssignedCrossing],
) {
    let term_count: usize = jobs.iter().map(|job| job.terminals.len()).sum();
    let pad_count = count_terms(jobs, TerminalKind::Pad);
    let entry_count = count_terms(jobs, TerminalKind::Entry);
    let exit_count = count_terms(jobs, TerminalKind::Exit);
    let via_count = count_terms(jobs, TerminalKind::Via);
    eprintln!(
        "  {board_name}: detail-jobs net={net_name} jobs={} terms={} pads={} entries={} exits={} vias={} crossings={}",
        jobs.len(),
        term_count,
        pad_count,
        entry_count,
        exit_count,
        via_count,
        crossings.len()
    );
    for job in jobs.iter().take(INSPECT_LIMIT) {
        let rect = &mesh.leaves[job.leaf].rect;
        let terms = job
            .terminals
            .iter()
            .map(|terminal| {
                format!(
                    "{:?}@({:.3},{:.3},{})",
                    terminal.kind, terminal.at.x, terminal.at.y, terminal.layer.0
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "  {board_name}: detail-jobs net={net_name} leaf={} rect=({:.3},{:.3})-({:.3},{:.3}) terms={}",
            job.leaf, rect.min_x, rect.min_y, rect.max_x, rect.max_y, terms
        );
    }
    for crossing in crossings.iter().take(INSPECT_LIMIT) {
        eprintln!(
            "  {board_name}: detail-jobs net={net_name} crossing edge={} layer={} at=({:.3},{:.3}) path={} step={}",
            crossing.edge,
            crossing.layer,
            crossing.at.x,
            crossing.at.y,
            crossing.path,
            crossing.step
        );
    }
}

fn count_terms(jobs: &[&CellJob], kind: TerminalKind) -> usize {
    jobs.iter()
        .flat_map(|job| job.terminals.iter())
        .filter(|terminal| terminal.kind == kind)
        .count()
}

const INSPECT_RADIUS_MM: f64 = 2.0;
const INSPECT_LIMIT: usize = 12;

fn dump_route_inspection(
    board_name: &str,
    problem: &RouteProblem,
    result: Option<&RouteResult>,
    net_names: &[String],
) {
    if net_names.is_empty() {
        return;
    }
    for net_name in net_names {
        let Some(conn) = problem
            .connections
            .iter()
            .find(|conn| conn.name == *net_name)
        else {
            eprintln!("{board_name}: inspect-net {net_name}: missing from route problem");
            continue;
        };
        dump_connection_geometry(board_name, problem, conn);
        if let Some(result) = result {
            dump_connection_route_context(board_name, problem, result, conn);
        }
    }
}

fn dump_connection_geometry(
    board_name: &str,
    problem: &RouteProblem,
    conn: &pcb_model::Connection,
) {
    let points = conn
        .points_to_connect
        .iter()
        .enumerate()
        .map(|(idx, pt)| format!("{idx}:({:.3},{:.3},{})", pt.x, pt.y, pt.layer.0))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "  {board_name}: inspect-net {} terminals={} hpwl={:.3} points={points}",
        conn.name,
        conn.points_to_connect.len(),
        conn.half_perimeter()
    );

    let terminal_points = conn
        .points_to_connect
        .iter()
        .map(|pt| pt.point())
        .collect::<Vec<_>>();
    let mut nearby = problem
        .obstacles
        .iter()
        .enumerate()
        .map(|(idx, obstacle)| {
            let distance = terminal_points
                .iter()
                .map(|point| point_rect_distance(*point, obstacle_rect(obstacle)))
                .fold(f64::INFINITY, f64::min);
            (distance, idx, obstacle)
        })
        .filter(|(distance, _, _)| *distance <= INSPECT_RADIUS_MM)
        .collect::<Vec<_>>();
    nearby.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.connected_to.cmp(&b.2.connected_to))
    });

    eprintln!(
        "  {board_name}: inspect-net {} nearby_obstacles={} radius_mm={:.1}",
        conn.name,
        nearby.len(),
        INSPECT_RADIUS_MM
    );
    for (distance, idx, obstacle) in nearby.into_iter().take(INSPECT_LIMIT) {
        let owner = obstacle_owner(&conn.name, &obstacle.connected_to);
        eprintln!(
            "  {board_name}: inspect-net {} obstacle#{idx} d={:.3} owner={} layers={} center=({:.3},{:.3}) size=({:.3},{:.3}) type={}",
            conn.name,
            distance,
            owner,
            layer_list(&obstacle.layers),
            obstacle.center.x,
            obstacle.center.y,
            obstacle.width,
            obstacle.height,
            obstacle.kind
        );
    }
}

fn dump_connection_route_context(
    board_name: &str,
    problem: &RouteProblem,
    result: &RouteResult,
    conn: &pcb_model::Connection,
) {
    if let Some(failed) = result
        .failed
        .iter()
        .find(|failed| failed.connection == conn.name)
    {
        eprintln!(
            "  {board_name}: inspect-net {} failed_reason={}",
            conn.name, failed.reason
        );
    }

    let own_traces = result
        .solution
        .traces
        .iter()
        .filter(|trace| trace.connection == conn.name)
        .count();
    let own_vias = result
        .solution
        .vias
        .iter()
        .filter(|via| via.connection == conn.name)
        .count();
    eprintln!(
        "  {board_name}: inspect-net {} own_copper traces={} vias={}",
        conn.name, own_traces, own_vias
    );

    let terminal_points = conn
        .points_to_connect
        .iter()
        .map(|pt| pt.point())
        .collect::<Vec<_>>();
    let mut traces = result
        .solution
        .traces
        .iter()
        .enumerate()
        .filter(|(_, trace)| trace.connection != conn.name)
        .filter_map(|(idx, trace)| {
            let distance = trace
                .path
                .windows(2)
                .flat_map(|segment| {
                    terminal_points
                        .iter()
                        .map(move |point| point_segment_distance(*point, segment[0], segment[1]))
                })
                .fold(f64::INFINITY, f64::min);
            (distance <= INSPECT_RADIUS_MM).then_some((distance, idx, trace))
        })
        .collect::<Vec<_>>();
    traces.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.2.connection.cmp(&b.2.connection))
            .then_with(|| a.1.cmp(&b.1))
    });
    eprintln!(
        "  {board_name}: inspect-net {} nearby_traces={} radius_mm={:.1}",
        conn.name,
        traces.len(),
        INSPECT_RADIUS_MM
    );
    for (distance, idx, trace) in traces.into_iter().take(INSPECT_LIMIT) {
        let (start, end) = trace_endpoints(trace);
        eprintln!(
            "  {board_name}: inspect-net {} trace#{idx} d={:.3} net={} layer={} width={:.3} endpoints=({:.3},{:.3})->({:.3},{:.3})",
            conn.name,
            distance,
            trace.connection,
            trace.layer.0,
            trace.width,
            start.x,
            start.y,
            end.x,
            end.y
        );
    }

    let mut vias = result
        .solution
        .vias
        .iter()
        .enumerate()
        .filter(|(_, via)| via.connection != conn.name)
        .filter_map(|(idx, via)| {
            let distance = terminal_points
                .iter()
                .map(|point| point.dist(via.at))
                .fold(f64::INFINITY, f64::min);
            (distance <= INSPECT_RADIUS_MM).then_some((distance, idx, via))
        })
        .collect::<Vec<_>>();
    vias.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.2.connection.cmp(&b.2.connection))
            .then_with(|| a.1.cmp(&b.1))
    });
    eprintln!(
        "  {board_name}: inspect-net {} nearby_vias={} radius_mm={:.1}",
        conn.name,
        vias.len(),
        INSPECT_RADIUS_MM
    );
    for (distance, idx, via) in vias.into_iter().take(INSPECT_LIMIT) {
        eprintln!(
            "  {board_name}: inspect-net {} via#{idx} d={:.3} net={} at=({:.3},{:.3}) diameter={:.3}",
            conn.name, distance, via.connection, via.at.x, via.at.y, via.diameter
        );
    }

    let lint_count = geometry_violations(problem, &result.solution);
    eprintln!(
        "  {board_name}: inspect-net {} solution_geometry_violations={lint_count}",
        conn.name
    );
}

fn obstacle_owner(net_name: &str, owners: &[String]) -> String {
    if owners.is_empty() {
        "keepout".to_owned()
    } else if owners.iter().any(|owner| owner == net_name) {
        format!("own({})", owners.join("|"))
    } else {
        format!("foreign({})", owners.join("|"))
    }
}

fn layer_list(layers: &[negotiated_mesh::problem::LayerRef]) -> String {
    layers
        .iter()
        .map(|layer| layer.0.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

fn trace_endpoints(trace: &pcb_model::Trace) -> (Point2, Point2) {
    let start = trace
        .path
        .first()
        .copied()
        .unwrap_or(Point2 { x: 0.0, y: 0.0 });
    let end = trace.path.last().copied().unwrap_or(start);
    (start, end)
}

fn obstacle_rect(obstacle: &pcb_model::Obstacle) -> (f64, f64, f64, f64) {
    let hw = obstacle.width / 2.0;
    let hh = obstacle.height / 2.0;
    (
        obstacle.center.x - hw,
        obstacle.center.y - hh,
        obstacle.center.x + hw,
        obstacle.center.y + hh,
    )
}

fn point_rect_distance(point: Point2, rect: (f64, f64, f64, f64)) -> f64 {
    let (min_x, min_y, max_x, max_y) = rect;
    let dx = if point.x < min_x {
        min_x - point.x
    } else if point.x > max_x {
        point.x - max_x
    } else {
        0.0
    };
    let dy = if point.y < min_y {
        min_y - point.y
    } else if point.y > max_y {
        point.y - max_y
    } else {
        0.0
    };
    dx.hypot(dy)
}

fn point_segment_distance(point: Point2, a: Point2, b: Point2) -> f64 {
    let vx = b.x - a.x;
    let vy = b.y - a.y;
    let wx = point.x - a.x;
    let wy = point.y - a.y;
    let len_sq = vx * vx + vy * vy;
    if len_sq <= f64::EPSILON {
        return point.dist(a);
    }
    let t = ((wx * vx + wy * vy) / len_sq).clamp(0.0, 1.0);
    point.dist(Point2 {
        x: a.x + t * vx,
        y: a.y + t * vy,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouterMode {
    Auto,
    Mesh,
    MeshDetail,
    MeshGlobal,
    MeshAssign,
    Sequential,
    Grid,
}

impl RouterMode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "mesh" => Ok(Self::Mesh),
            "mesh-detail" => Ok(Self::MeshDetail),
            "mesh-global" => Ok(Self::MeshGlobal),
            "mesh-assign" => Ok(Self::MeshAssign),
            "sequential" | "sequential-grid" | "seq" => Ok(Self::Sequential),
            "grid" | "astar" | "a-star" => Ok(Self::Grid),
            _ => Err(anyhow!(
                "unknown router `{value}`; expected auto, mesh, mesh-detail, mesh-global, mesh-assign, sequential, grid, or astar"
            )),
        }
    }
}

fn route_with_mode(
    problem: &negotiated_mesh::problem::RouteProblem,
    mode: RouterMode,
) -> RouteAutoRun {
    match mode {
        RouterMode::Auto => route_auto_with_diagnostics(problem),
        RouterMode::Mesh => route_mesh_with_diagnostics(problem),
        RouterMode::MeshDetail => route_mesh_detail_with_diagnostics(problem),
        RouterMode::MeshGlobal | RouterMode::MeshAssign => {
            unreachable!("mesh diagnostics are handled before copper routing")
        }
        RouterMode::Sequential => route_sequential_with_diagnostics(problem),
        RouterMode::Grid => route_grid_with_diagnostics(problem),
    }
}

fn route_mesh_detail_with_diagnostics(
    problem: &negotiated_mesh::problem::RouteProblem,
) -> RouteAutoRun {
    let started = Instant::now();
    let (result, global) = route_detailed_with_global(problem);
    let elapsed_ms = started.elapsed().as_millis();
    let attempts = vec![attempt_summary(problem, &result, elapsed_ms)];
    RouteAutoRun {
        result,
        attempts,
        global: Some(global),
    }
}

fn route_grid_with_diagnostics(problem: &negotiated_mesh::problem::RouteProblem) -> RouteAutoRun {
    let grid = GridAStarRouter;
    let started = Instant::now();
    let result = grid.route(problem);
    let elapsed_ms = started.elapsed().as_millis();
    let attempts = vec![attempt_summary(problem, &result, elapsed_ms)];
    RouteAutoRun {
        result,
        attempts,
        global: None,
    }
}

fn attempt_summary(
    problem: &negotiated_mesh::problem::RouteProblem,
    result: &RouteResult,
    elapsed_ms: u128,
) -> RouteEngineAttempt {
    let geometry_violations = geometry_violations(problem, &result.solution);
    let quality = RouteQuality::of(problem, result, geometry_violations);
    RouteEngineAttempt {
        engine: result.engine.clone(),
        failed: result.failed.clone(),
        elapsed_ms,
        fault_weight: quality.fault_weight,
        geometry_violations,
        failed_nets: quality.failed_nets,
        vias: quality.via_count,
        wirelength: quality.wirelength,
    }
}

fn resolve_boards(corpus_dir: &Path, args: &Args) -> Result<Vec<PathBuf>> {
    if args.all {
        let mut boards = std::fs::read_dir(corpus_dir)
            .with_context(|| format!("reading {}", corpus_dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        boards.sort();
        return Ok(boards);
    }
    let names = if args.required {
        REQUIRED_BOARDS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    } else if args.names.is_empty() {
        DEFAULT_BOARDS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    } else {
        args.names.clone()
    };
    names
        .into_iter()
        .map(|name| {
            let path = if name.ends_with(".json") {
                corpus_dir.join(name)
            } else {
                corpus_dir.join(format!("{name}.json"))
            };
            if path.is_file() {
                Ok(path)
            } else {
                Err(anyhow!("corpus board not found: {}", path.display()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_failed_net_inspection_flag() {
        let args = Args::parse(
            [
                "--router",
                "mesh-detail",
                "--inspect-failed-nets",
                "--inspect-net",
                "S1,S2",
                "bga25-route",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("args parse");

        assert_eq!(args.router, RouterMode::MeshDetail);
        assert!(args.inspect_failed_nets);
        assert_eq!(args.inspect_nets, vec!["S1".to_owned(), "S2".to_owned()]);
        assert_eq!(args.names, vec!["bga25-route".to_owned()]);
    }

    #[test]
    fn args_parse_required_corpus_flag() {
        let args = Args::parse(["--required"].into_iter().map(str::to_owned)).expect("args parse");

        assert!(args.required);
        assert!(!args.all);
        assert!(args.names.is_empty());
    }

    #[test]
    fn args_reject_required_with_all_or_names() {
        let all = Args::parse(["--required", "--all"].into_iter().map(str::to_owned));
        assert!(all.is_err());

        let names = Args::parse(["--required", "bga25-route"].into_iter().map(str::to_owned));
        assert!(names.is_err());
    }

    #[test]
    fn resolve_required_boards_uses_exact_acceptance_set() {
        let dir = tempfile::tempdir().expect("temp corpus");
        for name in REQUIRED_BOARDS {
            std::fs::write(dir.path().join(format!("{name}.json")), "{}").expect("board file");
        }
        let args = Args::parse(["--required"].into_iter().map(str::to_owned)).expect("args parse");

        let boards = resolve_boards(dir.path(), &args).expect("required boards resolve");
        let names = boards
            .iter()
            .map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect::<Vec<_>>();

        assert_eq!(names, REQUIRED_BOARDS);
    }

    #[test]
    fn args_parse_sequential_router_alias() {
        let args = Args::parse(
            ["--router", "seq", "board-a"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("args parse");

        assert_eq!(args.router, RouterMode::Sequential);
        assert_eq!(args.names, vec!["board-a".to_owned()]);
    }

    #[test]
    fn args_parse_astar_router_alias() {
        let args = Args::parse(
            ["--router", "astar", "board-a"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("args parse");

        assert_eq!(args.router, RouterMode::Grid);
        assert_eq!(args.names, vec!["board-a".to_owned()]);
    }

    #[test]
    fn args_parse_inspect_net_equals_form_and_trim_empty_names() {
        let args = Args::parse(
            ["--inspect-net=S1,, S2 ", "--inspect-failed-nets", "board-a"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("args parse");

        assert!(args.inspect_failed_nets);
        assert_eq!(args.inspect_nets, vec!["S1".to_owned(), "S2".to_owned()]);
        assert_eq!(args.names, vec!["board-a".to_owned()]);
    }

    #[test]
    fn inspect_failed_nets_appends_unique_failed_names_after_explicit_names() {
        let explicit = vec!["S1".to_owned(), "S2".to_owned()];
        let merged = inspect_nets_with_optional_failures(
            &explicit,
            true,
            ["S2".to_owned(), "GND".to_owned(), "S1".to_owned()],
        );

        assert_eq!(
            merged,
            vec!["S1".to_owned(), "S2".to_owned(), "GND".to_owned()]
        );
        assert_eq!(
            inspect_nets_with_optional_failures(&explicit, false, ["GND".to_owned()]),
            explicit
        );
    }

    #[test]
    fn assignment_failure_connection_is_empty_for_non_net_specific_overflow() {
        assert_eq!(
            assignment_failure_connection(&AssignmentFailure::ViaSite {
                connection: "VCC".to_owned(),
                leaf: 42,
            }),
            "VCC"
        );
        assert_eq!(
            assignment_failure_connection(&AssignmentFailure::Overflow {
                edge: 1,
                layer: 0,
                needed: 2,
                available: 1,
            }),
            ""
        );
    }
}
