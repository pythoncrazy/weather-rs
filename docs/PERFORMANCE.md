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
| Bytes, total | 11.3 MB | 5.4 MB |
| LibreWXR radar | 56 requests, 4.7 MB | 54 requests, 0.9 MB |
| ECCC GeoMet | 143 requests, 6.1 MB | 91 requests, 3.9 MB |
| IEM HRRR | 234 requests, 0.48 MB | 234 requests, 0.49 MB |
| Failed or timed-out requests | 3 | 0 |
| Time until every frame is ready | more than 12 s (never finished in the window) | 7.0 s |
| Playback stall after pressing Play | 3.4 s | 0.5 s |

What changed:

- **LibreWXR tiles are requested unsmoothed** (`0_1` instead of `1_1`) and smoothed locally. The unsmoothed PNG is palettized and about five times smaller.
- **RDPS and GDPS tiles share one parent-zoom raster** per four tiles. These models are coarser than the tile grid, so no model detail is lost.
- **Hourly ECCC rasters are ordinary tile jobs.** Each raster is requested once and all of them download in parallel. Before, every derived frame held a network slot while it waited for a raster that another frame was already fetching, so forecast hours loaded one after another.
- **A model tier's tiles are requested only where that tier is drawn** inside the viewport.
- **RDPS and GDPS rasters carry a 32-pixel margin** instead of 64. At their coarser resolution that still covers an hour of storm travel.
- **ECCC is limited to 8 parallel requests** (was 18). ECCC throttles above about 15 requests per second, and 18 parallel requests ran at about 30 per second, which caused multi-second stalls and timeouts. With 8, loading all 12 hours takes longer in the best case, but no request fails, and the hours nearest the playhead still load first.
- **Frames more than about 3 hours ahead wait 350 ms after the view stops moving**, unless playback is running. A pause in the middle of a pan no longer downloads 12 hours of tiles.

## App benchmark (2026-11-24)

Frame times vary between runs with whatever else the desktop is drawing. The values below are from a quiet run.

| Metric | Value |
| --- | --- |
| Weather, cold | 668 ms, first paint after 71 ms, 4 requests, 4.4 kB on the wire |
| Weather, warm | 31 ms, 1 request, 1.3 kB on the wire |
| Sidebar, 3 cities | 27 ms, 1 request, 0.8 kB on the wire |
| Radar manifest | 192 ms, 2 requests, 2.3 kB |
| Map grid, 20 points | 36 ms, 1 request, 3.4 kB on the wire |
| Bento view, full frame | 3.3 ms |
| Radar playback, full frame | 4.1 ms |
| Map pan, full frame | 4.0 ms |
| Scalar field build | 0.39 ms |
| Isobar build | 0.11 ms |
| Hover readout | 5 µs |
