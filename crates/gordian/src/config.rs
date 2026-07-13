//! Interface-owned config loading and persistence.
//!
//! gordian-core defines the typed model; this binary crate decides where TOML
//! lives and when to create it.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gordian_core::GordianConfig;
use kicad_env::KicadEnv;

const CONFIG_FILE: &str = "config.toml";

pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: GordianConfig,
}

pub fn load_or_create() -> Result<LoadedConfig> {
    let path = default_config_path()?;
    if let Some(text) =
        read_existing_config(&path).with_context(|| format!("opening config {}", path.display()))?
    {
        let config: GordianConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .map_err(|e| anyhow!("invalid config in {}: {e}", path.display()))?;
        return Ok(LoadedConfig { path, config });
    }

    let config = GordianConfig::default();
    write_default_config(&path, &config)?;
    Ok(LoadedConfig { path, config })
}

pub fn detect_kicad(config: &GordianConfig) -> Option<KicadEnv> {
    KicadEnv::detect_with(
        config.kicad.symbol_dir.as_deref(),
        config.kicad.footprint_dir.as_deref(),
        config.kicad.cli_path.as_deref(),
    )
}

fn default_config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "Gordian", "gordian")
        .ok_or_else(|| anyhow!("could not determine platform config directory"))?;
    Ok(dirs.config_dir().join(CONFIG_FILE))
}

fn write_default_config(path: &Path, config: &GordianConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }

    let body = format!(
        "# Gordian config. Persistence is owned by the CLI/TUI; gordian-core only consumes this typed model.\n\
         # Fill in llm.adapter, llm.model, and llm.apiKey for hosted providers. Local/no-auth endpoints may omit apiKey.\n\
         # Example:\n\
         # [llm]\n\
         # adapter = \"openai\"\n\
         # model = \"gpt-4o\"\n\
         # apiKey = \"sk-...\"\n\
         # endpoint = \"https://your-openai-compatible-gateway/v1\"\n\n{}",
        toml::to_string_pretty(config).context("serializing default config")?
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", path.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary config in {}", parent.display()))?;
    temp.write_all(body.as_bytes())
        .with_context(|| format!("writing temporary config for {}", path.display()))?;
    set_owner_only_permissions(temp.as_file())
        .with_context(|| format!("securing temporary config for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary config for {}", path.display()))?;
    temp.persist_noclobber(path)
        .map_err(|err| err.error)
        .with_context(|| format!("installing new config {}", path.display()))?;
    sync_dir(parent).with_context(|| format!("syncing config dir {}", parent.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

/// Open, secure, and read an existing config without following a leaf symlink.
/// `None` means the leaf is genuinely absent; all other unsafe or unreadable
/// file types are reported to the caller.
#[cfg(unix)]
fn read_existing_config(path: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path is not a regular file: {}", path.display()),
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        // O_NONBLOCK prevents a path-swap to a FIFO from hanging the process;
        // O_NOFOLLOW makes the kernel reject a leaf symlink during open.
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    // If the checked leaf disappeared before open, report the race instead of
    // creating a replacement in the same operation.
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    let leaf = std::fs::symlink_metadata(path)?;
    if !opened.is_file()
        || leaf.file_type().is_symlink()
        || opened.dev() != leaf.dev()
        || opened.ino() != leaf.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path is not a regular file: {}", path.display()),
        ));
    }
    set_owner_only_permissions(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

#[cfg(not(unix))]
fn read_existing_config(path: &Path) -> io::Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path is not a regular file: {}", path.display()),
        ));
    }
    let mut file = std::fs::File::open(path)?;
    set_owner_only_permissions(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_toml_roundtrips() {
        let cfg = GordianConfig::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: GordianConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[cfg(unix)]
    #[test]
    fn config_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();

        assert_eq!(
            read_existing_config(&path).unwrap().as_deref(),
            Some("secret")
        );

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn config_symlinks_are_rejected_without_touching_the_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("shared.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&target, "shared").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, &link).unwrap();

        let err = read_existing_config(&link).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "shared");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn config_directories_are_rejected_without_changing_their_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = read_existing_config(&path).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn default_config_creation_never_clobbers_an_existing_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "keep me").unwrap();

        let err = write_default_config(&path, &GordianConfig::default()).unwrap_err();

        assert!(err.to_string().contains("installing new config"), "{err:#}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep me");
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_config_is_complete_and_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        write_default_config(&path, &GordianConfig::default()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        toml::from_str::<GordianConfig>(&text).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
