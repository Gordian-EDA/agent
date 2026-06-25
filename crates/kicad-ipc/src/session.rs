//! KiCAD session lifecycle — launch a headless KiCAD on a board, connect over
//! IPC, and tear it down. This is the reusable handle the interactive PCB flow
//! (agent tools, the deterministic e2e harness) builds on.
//!
//! - **Cloud / headless:** `xvfb-run pcbnew <board>` on KiCAD 9 (this box), or
//!   `kicad-cli api-server` on KiCAD 11+ (no display).
//! - **Local:** [`Session::connect_running`] attaches to the user's open GUI.
//!
//! The launched process is killed and the stale socket removed on drop.

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
    owned_board: Option<PathBuf>,
    kicad: Kicad,
}

/// The single KiCAD board session owner for an agent process.
///
/// Callers ask for a board session; the manager attaches to an existing KiCAD
/// IPC server if possible, otherwise launches a managed headless `pcbnew`.
pub struct SessionManager {
    session: Mutex<Option<ManagedSession>>,
}

struct ManagedSession {
    board: PathBuf,
    session: Session,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    pub fn is_open(&self) -> bool {
        self.session.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    pub fn open(&self, board: &Path) -> Result<(), Error> {
        remove_board_lock(board);
        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Spawn("KiCAD session manager mutex poisoned".to_string()))?;
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
                    session: Self::attach_or_launch(board)?,
                });
            }
        }
        Ok(())
    }

    pub fn with_session<T>(
        &self,
        board: &Path,
        f: impl FnOnce(&mut Session) -> Result<T, Error>,
    ) -> Result<T, Error> {
        remove_board_lock(board);
        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Spawn("KiCAD session manager mutex poisoned".to_string()))?;
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
                    session: Self::attach_or_launch(board)?,
                });
            }
        }
        f(&mut session.as_mut().expect("session initialized").session)
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
            if let Some(existing) = session.as_ref() {
                remove_board_lock(&existing.board);
            }
            *session = None;
        }
    }

    fn attach_or_launch(board: &Path) -> Result<Session, Error> {
        if std::env::var_os("GORDIAN_ATTACH_RUNNING_KICAD").is_some() {
            match Session::connect_running_board(board) {
                Ok(session) => return Ok(session),
                Err(_) => {}
            }
        }
        Session::launch_headless(board)
    }
}

impl Session {
    /// Launch a headless KiCAD (`Xvfb` + `pcbnew <board>`), wait for the IPC
    /// socket, connect, and open the board. The board file must exist.
    pub fn launch_headless(board: &Path) -> Result<Self, Error> {
        if !board.exists() {
            return Err(Error::Spawn(format!(
                "board not found: {}",
                board.display()
            )));
        }
        kill_board_pcbnew(board);
        remove_board_lock(board);
        ensure_api_enabled();
        // A killed prior instance can leave a stale socket → ConnectionRefused.
        let _ = std::fs::remove_file(SOCKET_FILE);

        let launch_cwd = board.parent().unwrap_or_else(|| Path::new("/tmp"));
        let (display, xvfb) = launch_xvfb()?;
        let child = Command::new("pcbnew")
            .arg(board)
            .current_dir(launch_cwd)
            .env("DISPLAY", &display)
            .env_remove("WAYLAND_DISPLAY")
            .env("GDK_BACKEND", "x11")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Spawn(format!("launch pcbnew: {e}")))?;

        Self::await_socket(Duration::from_secs(90))?;
        // Let the server finish binding before the first request.
        std::thread::sleep(Duration::from_millis(500));

        let mut kicad = Kicad::connect()?;
        kicad.open_board_path(board)?;
        std::thread::sleep(Duration::from_millis(1_500));
        Ok(Self {
            child: Some(child),
            xvfb: Some(xvfb),
            owned_board: Some(board.to_path_buf()),
            kicad,
        })
    }

    /// Attach to an already-running KiCAD (the local GUI, or a `kicad-cli
    /// api-server`). Does not own the process.
    pub fn connect_running() -> Result<Self, Error> {
        let mut kicad = Kicad::connect()?;
        kicad.open_board()?;
        Ok(Self {
            child: None,
            xvfb: None,
            owned_board: None,
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
            owned_board: None,
            kicad,
        })
    }

    /// The connected client (board read/edit ops).
    pub fn kicad(&mut self) -> &mut Kicad {
        &mut self.kicad
    }

    fn await_socket(timeout: Duration) -> Result<(), Error> {
        let deadline = Instant::now() + timeout;
        while !Path::new(SOCKET_FILE).exists() {
            if Instant::now() > deadline {
                return Err(Error::LaunchTimeout);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
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
        if let Some(board) = self.owned_board.as_deref() {
            kill_board_pcbnew(board);
            remove_board_lock(board);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = self.xvfb.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(SOCKET_FILE);
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

fn kill_board_pcbnew(board: &Path) {
    let needles = board_needles(board);
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let cmdline = entry.path().join("cmdline");
        let Ok(bytes) = std::fs::read(&cmdline) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
        if text.contains("pcbnew") && needles.iter().any(|needle| text.contains(needle)) {
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        return;
    }
    kill_pids("TERM", &pids);
    std::thread::sleep(Duration::from_millis(500));
    kill_pids("KILL", &pids);
}

fn board_needles(board: &Path) -> Vec<String> {
    let mut needles = vec![board.display().to_string()];
    if let Ok(canonical) = board.canonicalize() {
        let canonical = canonical.display().to_string();
        if !needles.contains(&canonical) {
            needles.push(canonical);
        }
    }
    needles
}

fn kill_pids(signal: &str, pids: &[u32]) {
    let mut cmd = Command::new("kill");
    cmd.arg(format!("-{signal}"));
    for pid in pids {
        cmd.arg(pid.to_string());
    }
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn remove_board_lock(board: &Path) {
    let Some(name) = board.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let lock = board.with_file_name(format!("~{name}.lck"));
    if lock.exists() {
        let _ = std::fs::remove_file(lock);
    }
}

/// Best-effort: flip `api.enable_server` to true in the KiCAD config so the
/// server actually binds. Idempotent; silent on any failure (the connect step
/// surfaces a clear error if the server never comes up).
fn ensure_api_enabled() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let cfg_root = PathBuf::from(home).join(".config/kicad");
    let Ok(versions) = std::fs::read_dir(&cfg_root) else {
        return;
    };
    for v in versions.flatten() {
        let cfg = v.path().join("kicad_common.json");
        let Ok(text) = std::fs::read_to_string(&cfg) else {
            continue;
        };
        if text.contains("\"enable_server\": false") {
            let patched = text.replace("\"enable_server\": false", "\"enable_server\": true");
            let _ = std::fs::write(&cfg, patched);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionManager;

    #[test]
    fn manager_starts_without_open_session() {
        let manager = SessionManager::new();

        assert!(!manager.is_open());
    }
}
