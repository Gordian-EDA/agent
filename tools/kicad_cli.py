#!/usr/bin/env python3
"""Resolve and verify the KiCad 10 command-line executable for repository tools."""

from functools import lru_cache
import os
from pathlib import Path
import subprocess
import tomllib


@lru_cache(maxsize=1)
def configured_kicad_cli() -> str:
    """Return `KICAD_CLI` or `kicad.cliPath` after verifying KiCad 10 or newer."""
    configured = os.environ.get("KICAD_CLI")
    config_root = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    config_path = config_root / "gordian" / "config.toml"
    if configured:
        cli = configured
    else:
        try:
            config = tomllib.loads(config_path.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as error:
            raise RuntimeError(
                f"cannot read Gordian configuration from {config_path}: {error}"
            ) from error
        cli = config.get("kicad", {}).get("cliPath")
        if not cli:
            raise RuntimeError(f"set kicad.cliPath to KiCad 10 in {config_path}")

    try:
        result = subprocess.run(
            [str(cli), "version"],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError(f"kicad.cliPath {cli} failed `kicad-cli version`: {error}") from error
    version = result.stdout.strip()
    try:
        major = int(version.split(".", 1)[0])
    except ValueError as error:
        raise RuntimeError(f"kicad.cliPath {cli} reported invalid version {version!r}") from error
    if major < 10:
        raise RuntimeError(
            f"KiCad 10 or newer is required, but kicad.cliPath {cli} reports {version}; "
            "set kicad.cliPath, kicad.symbolDir, and kicad.footprintDir"
        )
    return str(cli)


if __name__ == "__main__":
    try:
        print(configured_kicad_cli())
    except RuntimeError as error:
        raise SystemExit(str(error)) from error
