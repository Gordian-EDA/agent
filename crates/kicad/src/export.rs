use std::io;
use std::path::{Path, PathBuf};

/// Fail with stderr (or a generic message) when a `kicad-cli` invocation exits
/// nonzero. Plain export wrappers have no JSON report to key success on.
pub(crate) fn check_status(output: &std::process::Output, what: &str) -> io::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        format!("{what} failed")
    } else {
        format!("{what} failed: {stderr}")
    };
    Err(io::Error::new(io::ErrorKind::InvalidData, detail))
}

/// The sorted paths of files in `dir` whose extension equals `ext`.
pub(crate) fn files_with_ext(dir: &Path, ext: &str) -> io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| x.eq_ignore_ascii_case(ext))
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    Ok(out)
}

/// `dir` with a guaranteed trailing path separator.
pub(crate) fn with_trailing_sep(dir: &Path) -> PathBuf {
    let mut s = dir.as_os_str().to_os_string();
    s.push(std::path::MAIN_SEPARATOR_STR);
    PathBuf::from(s)
}
