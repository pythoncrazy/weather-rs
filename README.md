# weather-rs

A fast, native cross-platform desktop weather and interactive radar map application written in pure Rust with [iced](https://iced.rs) and GPU-accelerated rendering (`wgpu`).

## Features

- **Dynamic sky UI & responsive bento dashboard** — Adaptive sky palettes driven by live conditions and solar elevation, with full-width and split-map layouts.
- **Multi-provider weather engine with automatic failover** — Combines Open-Meteo (`best_match`), live NOAA NWS station observations and severe weather alerts (US), MET Norway (`Locationforecast` & Nordic `Nowcast`), and CAMS Air Quality Index.
- **Interactive 60fps weather map**:
  - **Precipitation & radar timeline** — Seamless past + future playback combining RainViewer / IEM NEXRAD / GOES IR with NOAA HRRR (`REFP`/`REFD`) and LibreWXR forecast radar, with smooth cross-fading and background frame preloading.
  - **Atmospheric overlays** — Animated wind streamlines, temperature, precipitation, wind speed, and sea-level pressure IDW heatmaps.
  - **Point inspector & base styles** — Click anywhere on the map to inspect local atmospheric conditions; switch between Standard, Dark, Light, and Satellite basemaps.
- **Rich forecast cards** — Minutely precipitation, 24-hour hourly strip, 10-day forecast with temperature range bars, UV index, AQI spectrum, wind compass, sunrise/sunset arc, moon phase & illumination, barometric pressure dial, humidity/dew point, visibility, and historical averages.
- **Offline resilience & preferences** — Local disk caching for forecasts and map tiles, location search & saved locations, customizable units (°C/°F, km/h/mph/m/s/kn, mm/in, hPa/inHg/mmHg), 12h/24h clock, and theme presets.
- **Linux desktop integration** — Live Wayland socket detection, X11 DRI3 probe with automatic software GL fallback, native GTK3 Wayland CSD titlebars, `weather-rs` `app_id`/`WM_CLASS`, and freedesktop `.desktop` entry using the GNOME Weather icon (`org.gnome.Weather`).

## Building & Running

Requires a recent stable Rust toolchain (Edition 2024).

```bash
cargo run --release
```

### Linux Desktop Installation

To build the release binary, install it to `~/.local/bin/weather-rs`, and register the application launcher entry (`~/.local/share/applications/weather-rs.desktop`, using the `org.gnome.Weather` icon):

```bash
./packaging/install.sh
```

To uninstall:

```bash
./packaging/install.sh --uninstall
```

## Testing & Quality Checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
ICED_TEST_BACKEND=wgpu cargo test
```

## Architecture & Vendored Patches

`src/` is organized into focused modules:

- `main.rs` — Startup display probing (Wayland/X11 DRI3), window configuration, and `application_id`.
- `app.rs` — `iced` application state, async task orchestration, split/full bento layout, search, and preferences.
- `cards.rs` — Forecast and atmospheric metric cards.
- `map/` — Mercator tile engine (`tiles.rs`), radar/IDW/wind overlay rendering (`overlays.rs`), map HUD/timeline (`hud.rs`), and interaction controller (`mod.rs`).
- `providers.rs` — Multi-source weather, geocoding, air quality, NWS alerts, and radar manifest clients.
- `model.rs`, `astro.rs`, `icons.rs`, `style.rs`, `storage.rs` — Domain types, solar/lunar ephemeris, vector icons/canvas widgets, palettes, and persistence.

Two upstream crates are temporarily patched in `vendor/` via `[patch.crates-io]`:

- `vendor/iced_tiny_skia` (`0.14.1`) — Fixes missing quad-shadow and per-image `Frame::with_clip` clipping in `tiny-skia` software fallback and optimizes damage diffing. See `vendor/iced_tiny_skia/README`.
- `vendor/sctk-adwaita` (`0.10.1`) — Renders Wayland client-side decorations with the system GTK3 theme (`gtk-decoration-layout`, native headerbar colors, and titlebar actions) with built-in fallback. See `vendor/sctk-adwaita/README`.

Both directories can be removed once upstream releases include these fixes.

## Data Sources & Attribution

- Weather & air quality data: [Open-Meteo](https://open-meteo.com/), [NOAA National Weather Service](https://www.weather.gov/), [MET Norway](https://api.met.no/)
- Forecast radar & satellite/radar imagery: [LibreWXR](https://librewxr.org/) (CC BY 4.0), [Iowa Environmental Mesonet (NEXRAD / HRRR)](https://mesonet.agron.iastate.edu/), [RainViewer](https://www.rainviewer.com/)
- Map tiles: [CARTO](https://carto.com/), [Esri World Imagery](https://www.esri.com/), [OpenStreetMap](https://www.openstreetmap.org/copyright)
- Bundled UI font: [Inter](assets/fonts/INTER-LICENSE.txt)

## License

MIT — see [LICENSE](LICENSE).
