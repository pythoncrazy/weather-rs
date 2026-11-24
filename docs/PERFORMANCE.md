# Performance metrics

Every number here comes from a benchmark in the repository. Run the benchmarks again after any change that touches networking, tiles, or rendering, and update the tables.

## How to measure

Always set `ICED_TEST_BACKEND=wgpu`. Without it the headless harness can fall back to the software renderer, which is slower and draws differently from the real app.

| Benchmark | Command | What it reports |
| --- | --- | --- |
| Timeline load, cold cache | `XDG_CACHE_HOME=$(mktemp -d) ICED_TEST_BACKEND=wgpu cargo test --release bench_live_radar_replay_buffering -- --ignored --nocapture` | Time until the first and until every timeline frame is ready, playback stalls, and requests, bytes and latency per upstream host. |
| App benchmark | `ICED_TEST_BACKEND=wgpu cargo test --release perf_benchmark -- --ignored --nocapture` | Startup cost, weather fetch latency and bytes, CPU hot paths, and frame times. |

The timeline benchmark uses live upstream servers, so latency varies between runs by a factor of two or more. Request counts and bytes are stable; compare those first.

## Precipitation timeline, cold cache

Scenario: Berkeley, 1360×860 window, zoom 5.6, press Play immediately after opening the map (35 frames, -50 min to +12 h).

| Metric | Before (2026-11-24) | Now |
| --- | --- | --- |
| Requests, total | 441 | 387 |
| Bytes, total | 11.3 MB | 6.0 MB |
| LibreWXR radar | 56 requests, 4.7 MB | 54 requests, 0.86 MB |
| ECCC GeoMet | 143 requests, 6.1 MB | 91 requests, 4.6 MB |
| IEM HRRR | 234 requests, 0.48 MB | 234 requests, 0.47 MB |
| Time until every frame is ready | more than 12 s (never finished in the window) | 4.3 s to 8.1 s |
| Playback stall after pressing Play | 3.4 s | 0.8 s to 2.1 s |

What changed:

- **LibreWXR tiles are requested unsmoothed** (`0_1` instead of `1_1`) and smoothed locally. The unsmoothed PNG is palettized and about five times smaller.
- **RDPS and GDPS tiles share one parent-zoom raster** per four tiles. These models are coarser than the tile grid, so no model detail is lost.
- **Hourly ECCC rasters are ordinary tile jobs.** Each raster is requested once and all of them download in parallel. Before, every derived frame held a network slot while it waited for a raster that another frame was already fetching, so forecast hours loaded one after another.
- **A model tier's tiles are requested only where that tier is drawn** inside the viewport.
- **Frames more than about 3 hours ahead wait 350 ms after the view stops moving**, unless playback is running. A pause in the middle of a pan no longer downloads 12 hours of tiles.

## App benchmark (2026-11-24)

| Metric | Value |
| --- | --- |
| Weather, cold | 679 ms, first paint after 75 ms, 4 requests, 4.5 kB on the wire |
| Weather, warm | 30 ms, 1 request, 1.4 kB on the wire |
| Sidebar, 3 cities | 28 ms, 1 request, 0.7 kB on the wire |
| Radar manifest | 198 ms, 2 requests, 2.3 kB |
| Map grid, 20 points | 96 ms, 1 request, 3.5 kB on the wire |
| Bento view, full frame | 3.8 ms |
| Radar playback, full frame | 4.1 ms |
| Map pan, full frame | 3.9 ms |
| Scalar field build | 0.4 ms |
| Isobar build | 0.12 ms |
