#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${HOME}/.local/bin"
APP_DIR="${HOME}/.local/share/applications"
BIN_DEST="${BIN_DIR}/weather-rs"
DESKTOP_DEST="${APP_DIR}/weather-rs.desktop"

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "${BIN_DEST}" "${DESKTOP_DEST}"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "${APP_DIR}" || true
    fi
    echo "Uninstalled weather-rs from ${BIN_DEST} and ${DESKTOP_DEST}"
    exit 0
fi

echo "Building weather-rs (release)..."
cargo build --release --manifest-path "${REPO_DIR}/Cargo.toml"

install -Dm755 "${REPO_DIR}/target/release/weather-rs" "${BIN_DEST}"
mkdir -p "${APP_DIR}"
sed "s|^Exec=weather-rs$|Exec=${BIN_DEST}|" \
    "${REPO_DIR}/packaging/weather-rs.desktop" > "${DESKTOP_DEST}"
chmod 644 "${DESKTOP_DEST}"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "${DESKTOP_DEST}"
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${APP_DIR}" || true
fi

echo "Installed binary:        ${BIN_DEST}"
echo "Installed desktop entry: ${DESKTOP_DEST}"
