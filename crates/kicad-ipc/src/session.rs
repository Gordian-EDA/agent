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
use std::time::{Duration, Instant};

use crate::{Error, Kicad};

/// Default IPC socket file (the `ipc://` transport is `ipc://<this>`).
const SOCKET_FILE: &str = "/tmp/kicad/api.sock";

/// A running KiCAD instance plus the connected client.
pub struct Session {
    child: Option<Child>,
    kicad: Kicad,
}

impl Session {
    /// Launch a headless KiCAD (`xvfb-run pcbnew <board>`), wait for the IPC
    /// socket, connect, and open the board. The board file must exist.
    pub fn launch_headless(board: &Path) -> Result<Self, Error> {
        if !board.exists() {
            return Err(Error::Spawn(format!("board not found: {}", board.display())));
        }
        ensure_api_enabled();
        // A killed prior instance can leave a stale socket → ConnectionRefused.
        let _ = std::fs::remove_file(SOCKET_FILE);

        let child = Command::new("xvfb-run")
            .arg("-a")
            .arg("pcbnew")
            .arg(board)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Spawn(format!("launch xvfb-run pcbnew: {e}")))?;

        Self::await_socket(Duration::from_secs(90))?;
        // Let the server finish binding before the first request.
        std::thread::sleep(Duration::from_millis(500));

        let mut kicad = Kicad::connect()?;
        kicad.open_board()?;
        Ok(Self { child: Some(child), kicad })
    }

    /// Attach to an already-running KiCAD (the local GUI, or a `kicad-cli
    /// api-server`). Does not own the process.
    pub fn connect_running() -> Result<Self, Error> {
        let mut kicad = Kicad::connect()?;
        kicad.open_board()?;
        Ok(Self { child: None, kicad })
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

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(SOCKET_FILE);
        }
    }
}

/// Best-effort: flip `api.enable_server` to true in the KiCAD config so the
/// server actually binds. Idempotent; silent on any failure (the connect step
/// surfaces a clear error if the server never comes up).
fn ensure_api_enabled() {
    let Some(home) = std::env::var_os("HOME") else { return };
    let cfg_root = PathBuf::from(home).join(".config/kicad");
    let Ok(versions) = std::fs::read_dir(&cfg_root) else { return };
    for v in versions.flatten() {
        let cfg = v.path().join("kicad_common.json");
        let Ok(text) = std::fs::read_to_string(&cfg) else { continue };
        if text.contains("\"enable_server\": false") {
            let patched = text.replace("\"enable_server\": false", "\"enable_server\": true");
            let _ = std::fs::write(&cfg, patched);
        }
    }
}
