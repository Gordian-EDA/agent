//! KiCAD session lifecycle compatibility layer.
//!
//! This module still launches a headless KiCAD on a board, connects over IPC,
//! and tears it down for existing callers. Keep launch/headless policy isolated
//! here; the crate-level IPC boundary should remain a socket/protocol adapter.
//!
//! - **Cloud / headless:** `Xvfb` + `pcbnew <board>` on supported KiCAD 9/10.
//! - **Local:** [`Session::connect_running`] attaches to the user's open GUI.
//!
//! A managed session owns only the child processes and socket it created. It
//! never kills an unrelated `pcbnew`, removes another process' board lock, or
//! rewrites KiCAD preferences unless the caller explicitly opts in.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{Error, Kicad};

/// Default IPC socket file (the `ipc://` transport is `ipc://<this>`).
const SOCKET_FILE: &str = "/tmp/kicad/api.sock";

/// A running KiCAD instance plus the connected client.
pub struct Session {
    child: Option<Child>,
    xvfb: Option<Child>,
    owned_socket: Option<SocketIdentity>,
    owned_board_lock: Option<OwnedBoardLock>,
    kicad: Kicad,
}

struct OwnedBoardLock {
    path: PathBuf,
    last_identity: Option<SocketIdentity>,
}

impl OwnedBoardLock {
    fn refresh(&mut self, child: &mut Child) {
        if matches!(child.try_wait(), Ok(None))
            && let Some(identity) = socket_identity(&self.path)
        {
            self.last_identity = Some(identity);
        }
    }
}

/// The single KiCAD board session owner for an agent process.
///
/// Callers ask for a board session. Attached mode only connects to an existing
/// KiCad process; managed mode launches pcbnew for an explicit live workflow.
pub struct SessionManager {
    session: Mutex<Option<ManagedSession>>,
    attach_running: bool,
    pcbnew_path: PathBuf,
    expected_major: Option<u32>,
    enable_api_config: bool,
}

struct ManagedSession {
    board: PathBuf,
    session: Session,
}

impl SessionManager {
    pub fn with_installation(
        pcbnew_path: PathBuf,
        expected_major: Option<u32>,
        attach_running: bool,
        enable_api_config: bool,
    ) -> Self {
        Self {
            session: Mutex::new(None),
            attach_running,
            pcbnew_path,
            expected_major,
            enable_api_config,
        }
    }

    pub fn is_open(&self) -> bool {
        self.session.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    pub fn open(&self, board: &Path) -> Result<(), Error> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Spawn("KiCAD session manager mutex poisoned".to_string()))?;
        self.ensure_bound(&mut session, board)?;
        Ok(())
    }

    pub fn with_session<T>(
        &self,
        board: &Path,
        f: impl FnOnce(&mut Session) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Spawn("KiCAD session manager mutex poisoned".to_string()))?;
        self.ensure_bound(&mut session, board)?;
        let session = &mut session.as_mut().expect("session initialized").session;
        session.refresh_owned_board_lock();
        f(session)
    }

    /// Bind the manager to `board`, relaunching when the managed server process
    /// has died (a crashed pcbnew leaves a socket that only times out).
    fn ensure_bound(
        &self,
        session: &mut Option<ManagedSession>,
        board: &Path,
    ) -> Result<(), Error> {
        if let Some(existing) = session.as_mut()
            && same_board(&existing.board, board)
            && existing.session.process_exited()
        {
            *session = None;
        }
        match session.as_ref() {
            Some(existing) if same_board(&existing.board, board) => {}
            Some(existing) => {
                return Err(Error::Spawn(format!(
                    "KiCAD session is already bound to {}; refusing to reuse it for {}",
                    existing.board.display(),
                    board.display()
                )));
            }
            None => {
                *session = Some(ManagedSession {
                    board: board.to_path_buf(),
                    session: self.attach_or_launch(board)?,
                });
            }
        }
        Ok(())
    }

    pub fn save_if_open(&self) -> Result<bool, Error> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Spawn("KiCAD session manager mutex poisoned".to_string()))?;
        let Some(session) = session.as_mut() else {
            return Ok(false);
        };
        session.session.kicad().save()?;
        Ok(true)
    }

    pub fn close(&self) {
        if let Ok(mut session) = self.session.lock() {
            *session = None;
        }
    }

    fn attach_or_launch(&self, board: &Path) -> Result<Session, Error> {
        if self.attach_running {
            let mut session = Session::connect_running_board(board)?;
            session.ensure_major(self.expected_major)?;
            return Ok(session);
        }
        let mut session = Session::launch_headless_with_major(
            &self.pcbnew_path,
            board,
            self.expected_major,
            self.enable_api_config,
        )?;
        session.ensure_major(self.expected_major)?;
        Ok(session)
    }
}

impl Session {
    /// Launch the selected PCB editor headlessly and bind it to `board`.
    pub fn launch_headless_with(pcbnew_path: &Path, board: &Path) -> Result<Self, Error> {
        Self::launch_headless_with_major(pcbnew_path, board, None, false)
    }

    fn launch_headless_with_major(
        pcbnew_path: &Path,
        board: &Path,
        expected_major: Option<u32>,
        enable_api_config: bool,
    ) -> Result<Self, Error> {
        if !board.exists() {
            return Err(Error::Spawn(format!(
                "board not found: {}",
                board.display()
            )));
        }
        if Path::new(SOCKET_FILE).exists() {
            return Err(Error::Spawn(format!(
                "KiCAD IPC socket {SOCKET_FILE} is already present; attach to the running KiCAD instance or stop its owner before launching a managed session"
            )));
        }
        if board_lock_path(board).is_some_and(|lock| lock.exists()) {
            return Err(Error::Spawn(format!(
                "board {} is locked by another KiCAD process; refusing to remove its lock or launch a competing editor",
                board.display()
            )));
        }
        if enable_api_config {
            enable_api_for_major(expected_major.ok_or_else(|| {
                Error::Spawn(
                    "enable_api_config requires a selected KiCAD major version".to_string(),
                )
            })?);
        }

        let launch_cwd = board.parent().unwrap_or_else(|| Path::new("/tmp"));
        let (display, xvfb) = launch_xvfb()?;
        let mut child = Command::new(pcbnew_path)
            .arg(board)
            .current_dir(launch_cwd)
            .env("DISPLAY", &display)
            .env_remove("WAYLAND_DISPLAY")
            .env("GDK_BACKEND", "x11")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                Error::Spawn(format!("launch pcbnew at {}: {e}", pcbnew_path.display()))
            })?;
        let mut owned_board_lock = board_lock_path(board).map(|path| OwnedBoardLock {
            last_identity: socket_identity(&path),
            path,
        });

        if let Err(err) =
            Self::await_socket(&mut child, &mut owned_board_lock, Duration::from_secs(90))
        {
            cleanup_launch(&mut child, xvfb, owned_board_lock, None);
            return Err(match api_server_disabled(expected_major) {
                Some(cfg) => Error::Spawn(format!(
                    "{err}: `api.enable_server` is false in {}. KiCAD rewrites that file on exit, \
                     so set `kicad.enableApiConfig = true` in the Gordian config rather than \
                     editing it by hand",
                    cfg.display()
                )),
                None => err,
            });
        }
        let owned_socket = socket_identity(Path::new(SOCKET_FILE));
        // Let the server finish binding before the first request.
        std::thread::sleep(Duration::from_millis(500));

        let mut kicad = match Kicad::connect_launch_probe() {
            Ok(kicad) => kicad,
            Err(err) => {
                cleanup_launch(&mut child, xvfb, owned_board_lock, owned_socket);
                return Err(err);
            }
        };
        // The API socket becomes reachable before pcbnew necessarily registers
        // the requested document. Poll that second readiness boundary instead
        // of turning a normal startup race into a spurious `NoBoard` failure.
        let document_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match kicad.open_board_path(board) {
                Ok(()) => break,
                Err(err)
                    if matches!(err, Error::NoBoard)
                        || err.is_transient_api_ready_error()
                        || err.is_transport_timeout() =>
                {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            cleanup_launch(&mut child, xvfb, owned_board_lock, owned_socket);
                            return Err(Error::Spawn(format!(
                                "pcbnew exited with {status} before opening {}",
                                board.display()
                            )));
                        }
                        Ok(None) => {}
                        Err(wait_err) => {
                            cleanup_launch(&mut child, xvfb, owned_board_lock, owned_socket);
                            return Err(Error::Spawn(format!(
                                "checking pcbnew readiness: {wait_err}"
                            )));
                        }
                    }
                    if Instant::now() >= document_deadline {
                        cleanup_launch(&mut child, xvfb, owned_board_lock, owned_socket);
                        return Err(Error::Spawn(format!(
                            "timed out waiting for pcbnew to open {}: {err}",
                            board.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    cleanup_launch(&mut child, xvfb, owned_board_lock, owned_socket);
                    return Err(err);
                }
            }
        }
        kicad.use_default_timeouts();
        std::thread::sleep(Duration::from_millis(1_500));
        // Remember the only lock path this child may create. We capture its
        // identity immediately before terminating the still-running child in
        // `Drop`: KiCAD can create the lock slightly after the API reports the
        // board open, so sampling it here alone would leave a late lock stale.
        if let Some(owned) = owned_board_lock.as_mut() {
            owned.refresh(&mut child);
        }
        Ok(Self {
            child: Some(child),
            xvfb: Some(xvfb),
            owned_socket,
            owned_board_lock,
            kicad,
        })
    }

    /// Attach to an already-running KiCAD (the local GUI, or a `kicad
    /// api-server`). Does not own the process.
    pub fn connect_running() -> Result<Self, Error> {
        let mut kicad = Kicad::connect()?;
        kicad.open_board()?;
        Ok(Self {
            child: None,
            xvfb: None,
            owned_socket: None,
            owned_board_lock: None,
            kicad,
        })
    }

    /// Attach to an already-running KiCAD only if it has `board` open.
    pub fn connect_running_board(board: &Path) -> Result<Self, Error> {
        let mut kicad = Kicad::connect()?;
        kicad.open_board_path(board)?;
        Ok(Self {
            child: None,
            xvfb: None,
            owned_socket: None,
            owned_board_lock: None,
            kicad,
        })
    }

    /// The connected client (board read/edit ops).
    pub fn kicad(&mut self) -> &mut Kicad {
        &mut self.kicad
    }

    fn ensure_major(&mut self, expected: Option<u32>) -> Result<(), Error> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let (actual, _, _, full) = self.kicad.version()?;
        if actual != expected {
            return Err(Error::Unsupported(format!(
                "configured kicad is KiCAD {expected}, but the selected/running PCB editor is KiCAD {full}"
            )));
        }
        Ok(())
    }

    /// Whether the managed server process has exited. Attached sessions own no
    /// process and are assumed alive.
    pub fn process_exited(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
    }

    fn refresh_owned_board_lock(&mut self) {
        if let Some(child) = self.child.as_mut()
            && let Some(owned) = self.owned_board_lock.as_mut()
        {
            owned.refresh(child);
        }
    }

    fn await_socket(
        child: &mut Child,
        owned_board_lock: &mut Option<OwnedBoardLock>,
        timeout: Duration,
    ) -> Result<(), Error> {
        let deadline = Instant::now() + timeout;
        while !Path::new(SOCKET_FILE).exists() {
            if let Some(owned) = owned_board_lock.as_mut() {
                owned.refresh(child);
            }
            if Instant::now() > deadline {
                return Err(Error::LaunchTimeout);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
    }
}

fn cleanup_launch(
    child: &mut Child,
    mut xvfb: Child,
    mut owned_board_lock: Option<OwnedBoardLock>,
    owned_socket: Option<SocketIdentity>,
) {
    if let Some(owned) = owned_board_lock.as_mut() {
        owned.refresh(child);
    }
    let owned_board_lock = owned_board_lock
        .and_then(|owned| owned.last_identity.map(|identity| (owned.path, identity)));
    let _ = child.kill();
    let _ = child.wait();
    let _ = xvfb.kill();
    let _ = xvfb.wait();
    remove_if_same_identity(Path::new(SOCKET_FILE), owned_socket);
    if let Some((path, identity)) = owned_board_lock {
        remove_if_same_identity(&path, Some(identity));
    }
}

fn same_board(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    a == b
}

impl Drop for Session {
    fn drop(&mut self) {
        // The lock was absent before this managed child launched. While that
        // child is still alive, a lock at this exact board path belongs to its
        // session. Capture the identity before termination, then remove only
        // that same file afterward so a newly acquired lock is never touched.
        self.refresh_owned_board_lock();
        let owned_board_lock = self
            .owned_board_lock
            .take()
            .and_then(|owned| owned.last_identity.map(|identity| (owned.path, identity)));
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = self.xvfb.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        remove_if_same_identity(Path::new(SOCKET_FILE), self.owned_socket);
        if let Some((path, owned)) = owned_board_lock {
            remove_if_same_identity(&path, Some(owned));
        }
    }
}

fn launch_xvfb() -> Result<(String, Child), Error> {
    for display_num in 120..220 {
        let display = format!(":{display_num}");
        let mut child = Command::new("Xvfb")
            .arg(&display)
            .arg("-screen")
            .arg("0")
            .arg("1024x768x24")
            .arg("-nolisten")
            .arg("tcp")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Spawn(format!("launch Xvfb: {e}")))?;
        std::thread::sleep(Duration::from_millis(250));
        match child.try_wait() {
            Ok(None) => return Ok((display, child)),
            Ok(Some(_)) => {
                let _ = child.wait();
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    Err(Error::Spawn(
        "could not start Xvfb on displays :120..:219".to_string(),
    ))
}

fn board_lock_path(board: &Path) -> Option<PathBuf> {
    let name = board.file_name().and_then(|s| s.to_str())?;
    Some(board.with_file_name(format!("~{name}.lck")))
}

/// Explicitly enable the API server for one selected KiCAD major version.
///
/// This is called only when the frontend opted into `enableApiConfig`; ordinary
/// IPC connection and launch paths never rewrite user preferences.
/// The selected KiCAD's preferences file when it has the API server switched
/// off - the usual reason the IPC socket never appears.
fn api_server_disabled(major: Option<u32>) -> Option<PathBuf> {
    let cfg = kicad_common_config(major?)?;
    std::fs::read_to_string(&cfg)
        .ok()?
        .contains("\"enable_server\": false")
        .then_some(cfg)
}

fn kicad_common_config(major: u32) -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(
        base.config_dir()
            .join("kicad")
            .join(format!("{major}.0/kicad_common.json")),
    )
}

fn enable_api_for_major(major: u32) {
    let Some(cfg) = kicad_common_config(major) else {
        return;
    };
    if cfg.exists() {
        enable_api_in_config(&cfg);
    } else {
        let _ = std::fs::create_dir_all(cfg.parent().expect("version config has parent"));
        let _ = std::fs::write(
            &cfg,
            "{\n  \"api\": {\n    \"enable_server\": true\n  }\n}\n",
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> Option<SocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(SocketIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn socket_identity(_path: &Path) -> Option<SocketIdentity> {
    None
}

fn remove_if_same_identity(path: &Path, owned: Option<SocketIdentity>) {
    if owned.is_some_and(|owned| socket_identity(path) == Some(owned)) {
        let _ = std::fs::remove_file(path);
    }
}

fn enable_api_in_config(cfg: &Path) {
    let Ok(text) = std::fs::read_to_string(cfg) else {
        return;
    };
    if text.contains("\"enable_server\": false") {
        let patched = text.replace("\"enable_server\": false", "\"enable_server\": true");
        let _ = std::fs::write(cfg, patched);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OwnedBoardLock, SessionManager, SocketIdentity, cleanup_launch, enable_api_in_config,
        remove_if_same_identity, socket_identity,
    };
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    #[test]
    fn manager_starts_without_open_session() {
        let manager =
            SessionManager::with_installation(PathBuf::from("pcbnew"), None, false, false);

        assert!(!manager.is_open());
    }

    #[test]
    fn manager_retains_the_selected_installation() {
        let manager = SessionManager::with_installation(
            PathBuf::from("/opt/kicad10/bin/pcbnew"),
            Some(10),
            false,
            false,
        );

        assert_eq!(
            manager.pcbnew_path,
            PathBuf::from("/opt/kicad10/bin/pcbnew")
        );
        assert_eq!(manager.expected_major, Some(10));
    }

    #[test]
    fn enables_api_in_existing_kicad_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("kicad_common.json");
        std::fs::write(
            &config,
            "{\n  \"api\": {\n    \"enable_server\": false\n  }\n}\n",
        )
        .unwrap();

        enable_api_in_config(&config);

        let updated = std::fs::read_to_string(config).unwrap();
        assert!(updated.contains("\"enable_server\": true"));
        assert!(!updated.contains("\"enable_server\": false"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_managed_launch_removes_only_its_observed_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("~board.kicad_pcb.lck");
        std::fs::write(&lock, "owned").unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let xvfb = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();

        cleanup_launch(
            &mut child,
            xvfb,
            Some(OwnedBoardLock {
                path: lock.clone(),
                last_identity: None,
            }),
            None,
        );

        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn identity_cleanup_preserves_a_replacement_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("~board.kicad_pcb.lck");
        std::fs::write(&lock, "old").unwrap();
        let old = socket_identity(&lock).unwrap();
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, "replacement").unwrap();

        remove_if_same_identity(&lock, Some(old));

        assert_eq!(std::fs::read_to_string(lock).unwrap(), "replacement");
    }

    #[test]
    fn identity_cleanup_requires_an_owned_identity() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("~board.kicad_pcb.lck");
        std::fs::write(&lock, "unowned").unwrap();

        remove_if_same_identity(&lock, None::<SocketIdentity>);

        assert!(lock.exists());
    }
}
