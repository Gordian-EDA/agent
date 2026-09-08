//! Headless Freerouting: write the board as a DSN, run the router, read the session back.
//!
//! Ported from `pcbagent.route.freerouting.route`, cut down to a whole-board run: no net
//! filter, no routing window, no kept copper and no post-route tidy pass.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kicad::KicadInstallation;

use crate::dsn::write_dsn_scoped;
use crate::model::{Board, Rules};
use crate::ses::apply_ses;

/// Diminishing returns beyond ~10 passes once the board is connected.
pub const DEFAULT_PASSES: u32 = 10;
/// More threads means more candidate boards held in memory, not more speed.
pub const DEFAULT_THREADS: u32 = 4;
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

const BUNDLED_JAR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/freerouting.jar");

#[derive(Debug, Clone)]
pub struct RouteOptions {
    pub passes: u32,
    pub threads: u32,
    pub timeout_s: u64,
    /// Per-net track widths; each distinct width becomes a Specctra class.
    /// Empty means `rules::router_net_widths` decides.
    pub net_widths: BTreeMap<String, f64>,
    /// Routing rules the DSN is written to.
    pub rules: Rules,
    /// Route only these nets, protecting every other net's copper; empty routes everything.
    pub only_nets: Vec<String>,
}

impl Default for RouteOptions {
    fn default() -> Self {
        RouteOptions {
            passes: DEFAULT_PASSES,
            threads: DEFAULT_THREADS,
            timeout_s: DEFAULT_TIMEOUT_SECS,
            net_widths: BTreeMap::new(),
            rules: Rules::default(),
            only_nets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RouteResult {
    /// Nets written into the DSN's `(network)`.
    pub nets_requested: usize,
    /// Nets the session file carried copper for.
    pub routed_nets: usize,
    pub tracks_added: usize,
    pub vias_added: usize,
    /// Connections the router says it could not make; `None` if the log did not say.
    pub unrouted: Option<usize>,
    pub seconds: f64,
    pub log_tail: String,
    pub warnings: Vec<String>,
}

impl RouteResult {
    pub fn summary(&self) -> String {
        let unr = self.unrouted.map(|u| u.to_string()).unwrap_or("?".into());
        format!(
            "freerouting: routed {}/{} nets, {} tracks, {} vias, {unr} unrouted connections, {:.0}s",
            self.routed_nets, self.nets_requested, self.tracks_added, self.vias_added, self.seconds
        )
    }
}

/// `PCB_AUTO_FREEROUTING_JAR`, else `<workspace>/vendor/freerouting.jar`.
pub fn find_jar() -> anyhow::Result<PathBuf> {
    if let Some(env) = std::env::var_os("PCB_AUTO_FREEROUTING_JAR") {
        return Ok(PathBuf::from(env));
    }
    let jar = PathBuf::from(BUNDLED_JAR);
    anyhow::ensure!(
        jar.exists(),
        "freerouting.jar not found at {}; set PCB_AUTO_FREEROUTING_JAR",
        jar.display()
    );
    Ok(jar)
}

/// `(N unrouted and M violations)` from the router's log, last occurrence wins.
fn scan_unrouted(log: &str) -> Option<usize> {
    let mut found = None;
    let mut rest = log;
    while let Some(i) = rest.find(" unrouted and ") {
        let head = &rest[..i];
        let digits: String = head
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        // the count has to be the whole token after an opening bracket
        if let (true, Ok(n)) = (
            head[..head.len() - digits.len()].ends_with('('),
            digits.parse::<usize>(),
        ) {
            found = Some(n);
        }
        rest = &rest[i + " unrouted and ".len()..];
    }
    if found.is_none() && log.contains(" for 0 unrouted") {
        return Some(0);
    }
    found
}

/// Run the router, killing the whole process group if it overruns its budget.
fn run(cmd: &mut Command, timeout: Duration) -> anyhow::Result<(String, bool)> {
    // its own session, so a timeout kills the JVM and anything it started, not just `java`
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let pid = child.id() as libc::pid_t;
    // read both pipes on threads so a chatty router cannot fill a pipe and deadlock
    fn drain<R: std::io::Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                r.read_to_string(&mut s).ok();
            }
            s
        })
    }
    let ho = drain(child.stdout.take());
    let he = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                unsafe { libc::killpg(pid, libc::SIGKILL) };
                timed_out = true;
                child.wait()?;
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let log = format!(
        "{}{}",
        ho.join().unwrap_or_default(),
        he.join().unwrap_or_default()
    );
    Ok((log, timed_out))
}

/// Route the whole board in place with Freerouting.
///
/// Writes the board as a Specctra DSN into a temp directory, runs the bundled router there,
/// then applies the session file it produces. `kicad` is used to refill the pours before the
/// export, so a poured net goes out as a plane instead of being re-laid as tracks.
pub fn route(
    board: &mut Board,
    opts: &RouteOptions,
    kicad: &KicadInstallation,
) -> anyhow::Result<RouteResult> {
    let only: HashSet<String> = opts.only_nets.iter().cloned().collect();
    if !only.is_empty() {
        // A retry re-lays these nets from scratch: their own copper goes, everything else
        // stays and is handed to the router as protected wiring.
        let ids: HashSet<i64> = board
            .nets()
            .into_iter()
            .filter(|n| n.id != 0 && only.contains(&n.name))
            .map(|n| n.id)
            .collect();
        if !ids.is_empty() {
            board.remove_copper(Some(&ids));
        }
    }
    let doc = write_dsn_scoped(board, &opts.net_widths, &opts.rules, Some(kicad), &only)?;
    let dir = tempfile::Builder::new().prefix("pcb-auto-route-").tempdir()?;
    let (dsn_path, ses_path) = (dir.path().join("board.dsn"), dir.path().join("board.ses"));
    std::fs::write(&dsn_path, &doc.text)?;

    let mut cmd = Command::new("java");
    cmd.current_dir(dir.path())
        .arg("-Xmx1g")
        .arg("-XX:+UseSerialGC")
        .arg("-Djava.awt.headless=true")
        .arg("-jar")
        .arg(find_jar()?)
        .arg("-de")
        .arg(&dsn_path)
        .arg("-do")
        .arg(&ses_path)
        .arg("-mp")
        .arg(opts.passes.to_string())
        .arg("-mt")
        .arg(opts.threads.to_string());

    let t0 = Instant::now();
    let (log, timed_out) = run(&mut cmd, Duration::from_secs(opts.timeout_s))?;
    let seconds = t0.elapsed().as_secs_f64();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.contains("Analytics") && !l.contains("SegmentClient"))
        .collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    if timed_out {
        anyhow::bail!(
            "freerouting exceeded its {}s budget on {} nets and was stopped\n{tail}",
            opts.timeout_s,
            doc.nets.len()
        );
    }
    anyhow::ensure!(
        ses_path.exists(),
        "freerouting produced no session file\n{tail}"
    );

    let applied = apply_ses(board, &std::fs::read_to_string(&ses_path)?, &doc.layers, &only)?;
    let mut warnings: Vec<String> = lines
        .iter()
        .filter(|l| l.contains("WARN") && !l.contains("screen resolution"))
        .take(10)
        .map(|l| l.trim().to_string())
        .collect();
    if !applied.unknown_nets.is_empty() {
        warnings.push(format!(
            "session named nets the board does not have: {:?}",
            &applied.unknown_nets[..applied.unknown_nets.len().min(5)]
        ));
    }
    Ok(RouteResult {
        nets_requested: doc.nets.len(),
        routed_nets: applied.nets,
        tracks_added: applied.tracks,
        vias_added: applied.vias,
        unrouted: scan_unrouted(&log),
        seconds,
        log_tail: tail,
        warnings,
    })
}
