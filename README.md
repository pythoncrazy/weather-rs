# weather-rs

`weather-rs` is a desktop weather application written in Rust.
It uses `iced` and `wgpu` for GPU graphics, with a `tiny-skia` software fallback.

## Features

- **Adaptive dashboard**: The interface adjusts sky colors to live weather and solar elevation. It supports full-width and split-map layouts.
- **Multi-provider weather data**: The app reads forecasts from Open-Meteo (`best_match`) and MET Norway. It merges US station observations from NOAA NWS and global air quality data from CAMS.
- **Severe weather alerts**: The app loads active alerts from NOAA NWS (US), BrightSky DWD (Germany), and MET Norway (Norway).
- **Interactive weather map**:
  - **Radar timeline**: Play past radar and 12-hour forecast frames from LibreWXR, NOAA HRRR (through IEM), and ECCC GeoMet, with RainViewer as the radar fallback. HRRR frames arrive every 15 minutes. ECCC models publish hourly, so the app interpolates the frames between hours along the storm motion and labels them "Interpolated Forecast".
  - **Atmospheric layers**: View animated wind particles and maps for temperature, air quality, wind speed, and sea-level pressure. These maps are interpolated between sparse model samples, so the value under the cursor is shown as an estimate (`≈`). Use the point inspector for the exact model value.
  - **Point inspector and map styles**: Click any point on the map to read local weather. Switch among five map styles: Standard Light, Dark Canvas, Satellite, Topographic, and OpenStreetMap.
- **Forecast cards**: Cards show 60-minute precipitation, 24-hour and 10-day forecasts, UV index, AQI, wind, sun and moon cycles, pressure, humidity, visibility, and climate averages.
- **Offline cache and settings**: The app saves forecasts and map tiles to disk. You can search locations, save cities, choose units, set a 12-hour or 24-hour clock, and select a theme.
- **Linux desktop integration**: On Linux, the app detects Wayland sockets and X11 DRI3 support at startup. On an X11 display without DRI3, such as VNC or Xvfb, it uses the software renderer. It draws Wayland titlebars with GTK3, and sets `weather-rs` as the application ID so the packaged freedesktop launcher entry matches its window.

## Build and run

Install a stable Rust toolchain that supports Edition 2024.
Then run the release build:

```bash
cargo run --release
```

### Install

To install only the binary into `~/.cargo/bin` on any platform, run:

```bash
cargo install --path . --locked
```

To build native installers with a launcher entry and icon, use [`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager):

```bash
cargo install cargo-packager --locked
cargo packager --release
```

This writes the installers for the current platform next to the release binary (`target/release/` by default):

- **Linux**: a `.deb`, an AppImage, and a pacman package (`PKGBUILD` plus tarball; install it with `makepkg -si` in that directory).
- **macOS**: a `.app` bundle and a `.dmg`.
- **Windows**: an NSIS installer.

Pass `--formats` to build a subset, for example `cargo packager --release --formats pacman`.
The packaging settings live under `[package.metadata.packager]` in `Cargo.toml`.

## Test and check code

Run these commands to check code style, lints, and tests:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
ICED_TEST_BACKEND=wgpu cargo test
```

To test the real binary from start to finish, run the smoke test. It needs `xvfb-run` and ImageMagick:

```bash
scripts/e2e-smoke.sh
```

The script launches the release build on a private virtual display with an empty cache. It saves a screenshot every half second and fails if the app exits or logs a panic.

Performance benchmarks and the current numbers are in [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md).

## Source layout

- `src/main.rs` checks the Linux display backend, configures the window, and starts the app.
- `src/app.rs` holds application state, runs background tasks, and builds the main layout, search, and preferences.
- `src/cards.rs` draws forecast cards and metric widgets.
- `src/map/` implements the tile engine (`tiles.rs`), weather overlays (`overlays.rs`), map controls (`hud.rs`), and input logic (`mod.rs`).
- `src/providers.rs` fetches weather, geocoding, air quality, severe alerts, and radar manifests.
- `src/model.rs`, `src/astro.rs`, `src/icons.rs`, `src/style.rs`, and `src/storage.rs` define data models, solar and lunar math, SVG icons, color palettes, and disk storage.

## Vendored patches

`Cargo.toml` applies two local patches in `vendor/`:

- `vendor/iced_tiny_skia` (`0.14.1`) adds quad shadows and per-image `Frame::with_clip` support to the software renderer. It also positions images in exact destination pixels, so adjacent map tiles do not overlap. See `vendor/iced_tiny_skia/README`.
- `vendor/sctk-adwaita` (`0.10.1`) draws Wayland titlebars with the system GTK3 theme and a built-in fallback. See `vendor/sctk-adwaita/README`.

Remove both directories after upstream releases include these fixes.

## Data sources

- Weather and air quality: [Open-Meteo](https://open-meteo.com/), [NOAA National Weather Service](https://www.weather.gov/), [MET Norway](https://api.met.no/), and [BrightSky / DWD](https://brightsky.dev/).
- Radar and satellite imagery: [LibreWXR](https://librewxr.net/) (CC BY 4.0), [Iowa Environmental Mesonet](https://mesonet.agron.iastate.edu/) (NOAA HRRR and NEXRAD), and [RainViewer](https://www.rainviewer.com/).
- Forecast precipitation outside the HRRR domain: HRDPS, RDPS and GDPS model data from [Environment and Climate Change Canada, MSC GeoMet](https://eccc-msc.github.io/open-data/msc-geomet/readme_en/).
- Map tiles: [Esri ArcGIS Online](https://www.esri.com/) and [OpenStreetMap](https://www.openstreetmap.org/copyright).
- UI font: [Inter](assets/fonts/INTER-LICENSE.txt).

## License

This project uses the MIT License.
See [LICENSE](LICENSE) for details.
