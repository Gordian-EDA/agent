#!/usr/bin/env bash
# Fetch the Freerouting autorouter JAR. v1.9.0 is the last release that runs on
# Java 21 (2.2+ needs Java 25). Run headless in batch mode under xvfb-run:
#   xvfb-run -a java -jar freerouting.jar -de board.dsn -do board.ses -mp 100
set -euo pipefail
ver="${1:-1.9.0}"
dest="$(dirname "$0")/freerouting.jar"
url="https://github.com/freerouting/freerouting/releases/download/v${ver}/freerouting-${ver}.jar"
echo "fetching freerouting v${ver} -> ${dest}"
curl -sL "$url" -o "$dest"
echo "${ver}" > "$(dirname "$0")/freerouting.version"
ls -la "$dest"
