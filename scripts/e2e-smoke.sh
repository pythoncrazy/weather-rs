#!/usr/bin/env bash
# End-to-end smoke test: launch the real release binary on a private virtual X display with an empty cache and
# config, capture a screenshot every half second, and fail if the app exits or logs a panic.
#
# Requires `xvfb-run` and ImageMagick's `import`. Usage: scripts/e2e-smoke.sh [output-dir]
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-$(mktemp -d)}"
mkdir -p "$out"/xdg/{cache,config,data}

if [[ -z "${E2E_INNER:-}" ]]; then
    cargo build --release --locked --manifest-path "$repo/Cargo.toml"
    bin="$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/weather-rs"
    exec env E2E_INNER=1 E2E_BIN="$bin" xvfb-run -a -s "-screen 0 1440x900x24" "$0" "$out"
fi

export XDG_CACHE_HOME="$out/xdg/cache" XDG_CONFIG_HOME="$out/xdg/config" XDG_DATA_HOME="$out/xdg/data"
unset WAYLAND_DISPLAY
"$E2E_BIN" >"$out/app.log" 2>&1 &
pid=$!
start=$(date +%s%N)
for i in 1 2 3 4 5 6 7 8; do
    sleep 0.5
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "FAIL: the app exited after $(((($(date +%s%N) - start)) / 1000000)) ms" >&2
        cat "$out/app.log" >&2
        exit 1
    fi
    import -window root "$out/frame_$i.png"
    echo "frame $i captured at $(((($(date +%s%N) - start)) / 1000000)) ms"
done
ps -o rss= -p "$pid" | awk '{printf "resident memory: %.0f MB\n", $1 / 1024}'
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
if grep -qi "panicked" "$out/app.log"; then
    echo "FAIL: the app logged a panic" >&2
    cat "$out/app.log" >&2
    exit 1
fi
echo "OK: screenshots and log are in $out"
