# Performance metrics

Every number here comes from a benchmark in the repository. Run the benchmarks again after any change that touches networking, tiles, or rendering, and update the tables.

## How to measure

Always set `ICED_TEST_BACKEND=wgpu`. Without it the headless harness can fall back to the software renderer, which is slower and draws differently from the real app.

| Benchmark | Command | What it reports |
| --- | --- | --- |
| Timeline load, cold cache | `XDG_CACHE_HOME=$(mktemp -d) ICED_TEST_BACKEND=wgpu cargo test --release bench_live_radar_replay_buffering -- --ignored --nocapture` | Time until the first and until every timeline frame is ready, playback stalls, and requests, bytes and latency per upstream host. |
| Playback smoothness | `WEATHER_RS_PLAYBACK=<dir> ICED_TEST_BACKEND=wgpu cargo test --release audit_precip_live_playback -- --ignored --nocapture` | Plays the precipitation loop in real time and saves a frame about every 55 ms, named with the playhead position and `_WAIT` while playback waits. Prints the total time spent waiting. Add `XDG_CACHE_HOME=$(mktemp -d)` for a cold cache, `WEATHER_RS_PLAYBACK_VIEW=lat,lon,zoom` for another view, `WEATHER_RS_PLAYBACK_SIZE=WxH` and `WEATHER_RS_PLAYBACK_2X=1` for a heavier load, `WEATHER_RS_PLAYBACK_THEME=light` for the light map, and `WEATHER_RS_PLAYBACK_MODEL_FRAMES=1` for the Model Frames setting. |
| App benchmark | `ICED_TEST_BACKEND=wgpu cargo test --release perf_benchmark -- --ignored --nocapture` | Startup cost, weather fetch latency and bytes, CPU hot paths, and frame times. |
| End-to-end smoke test | `scripts/e2e-smoke.sh` | Launches the real binary on a virtual display with an empty cache. Reports when each screenshot was taken and the resident memory. |

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

## Precipitation playback (2026-11-24)

Scenario: Pacific Northwest during a frontal passage, 1360×860 window, zoom 6.6, one full loop at 1× (-50 min to +12 h).

| Metric | Before | Now |
| --- | --- | --- |
| Wait after pressing Play, warm cache | 0 s | 0 s |
| Wait after pressing Play, cold cache | 0.5 s, then a further pause at every frame that was not loaded yet | 0.6 s to 2.9 s once (limit 3 s) |
| Pauses after playback starts | one per unloaded frame | 0 |
| Whole-picture jumps per loop, not counting the restart | 24 (one every 30 forecast minutes) | 0 |
| Forecast speed | 15 min per 0.25 s to 0.9 s, changing along the loop | constant 60 forecast minutes per second |
| Interpolated ECCC frames per hour | 3 (every 15 min) | 7 (every 7.5 min) |
| Timeline frames | 35 | 55 (HRRR every 15 min for all 12 hours) |

The loop now starts by itself when the precipitation map opens, as in Apple Weather. With an empty cache it began 1.5 s after the map opened; with a warm cache it begins at once. The "wait after pressing Play" rows apply only when the user pauses and presses Play again on frames that are not loaded.

Stress check: at 2560×1440 and 2× speed (`WEATHER_RS_PLAYBACK_SIZE=2560x1440 WEATHER_RS_PLAYBACK_2X=1`), with a warm cache, the loop also ran with no wait, no missing tiles and no jump other than the restart.

What changed:

- **Playback waits once, before it starts, and never again.** Pressing Play waits until the next 2 seconds of playback are loaded (at most 3 seconds). After that the playhead always moves. Frames load several times faster than they are shown, and a tile that is still late is replaced by the same tile from the adjacent moment until it arrives.
- **Interpolated ECCC frames are built just ahead of the playhead and dropped behind it.** Only the hourly model rasters are downloaded. The loop keeps about one hour of interpolated frames in memory, so 7.5-minute steps cost no extra network and no extra memory.
- **ECCC is drawn at the exact playback time**, blended between the two nearest 7.5-minute frames, instead of holding one frame per timeline step.
- **Day/night shading is off by default** (Apple Weather draws none; the layers menu turns it on), and when on it slides continuously. Before, it moved in 30-minute steps, which changed the brightness of the whole map twice a second during playback.
- **No dead zones in the cross-dissolve.** Before, each frame was held alone for the first and last 2% of its interval, which made the next frame pop in.

- **HRRR tiles stop at zoom 6** (was 7). HRRR is a 3 km grid. A zoom-6 tile already draws each model cell at least 2 pixels wide, and a zoom-7 tile from IEM repeats the same cells at 4 pixels each (checked by comparing one zoom-6 tile with its four zoom-7 children). Views zoomed past 7.35 now need a quarter of the tiles: the New York scenario (1920×1080, zoom 7.5) went from 1104 IEM requests and 368 kB to 368 requests and 128 kB.

- **No lines or squares at tile edges.** Rain tiles are drawn up to 3× their native size. Each tile used to be filtered on its own, so the last half texel at its edge went flat and the next tile started with a step: a line along every tile edge and a flat square where four tiles meet. Each weather tile now gets a one-texel border copied from its neighbours once they are loaded, and is drawn one texel larger and clipped back to its own rectangle, so filtering crosses the edge as if the map were one texture. Radar playback still renders a full frame in 4.7 ms, and the 2560×1440, 2× stress check still ran with no wait.

- **HRRR to ECCC hand-off.** IEM publishes HRRR only inside 126°W–50°N (its world file). Within 2° of that edge HRRR fades out and ECCC fades in. Where the two models disagree, this band shows both at partial strength. That is real model data, not a rendering fault.

- **A model tier is fetched only where the viewport itself needs it.** The 12% pan margin around the viewport is gone. In the Berkeley scenario it pulled in one extra HRDPS raster for each of 13 forecast hours.

Cold-cache load for the same Berkeley scenario as above, with 55 frames instead of 35: 554 requests, 5.6 MB, 0 failed, every frame ready after 5.9 s, 1.8 s wait after pressing Play immediately. The extra requests are IEM HRRR tiles (414 requests, 1.2 MB, about 3 kB each) for the 20 additional model frames. Bytes per timeline frame fell from 154 kB to 102 kB. ECCC rasters are already requested as WebP.

Not done, on purpose: fetching one viewport-sized ECCC raster per model and hour instead of grid tiles. It would cut this benchmark to about 39 ECCC requests, but every pan would then download all 13 hours again. With grid tiles, four RDPS rasters cover the whole north-east Pacific and western North America and are reused across pans and zooms.

## App benchmark (2026-11-24)

Frame times vary between runs with whatever else the desktop is drawing. The values below are from a quiet run.

| Metric | Value |
| --- | --- |
| Weather, cold | 668 ms, first paint after 71 ms, 4 requests, 4.4 kB on the wire. Rerun 2026-11-25 after adding the past 24 hours: 695 ms, 4 requests, 5.7 kB (another location and day, so part of the difference is the data). |
| Forecast request, past 24 h of hourly data | +271 B on the wire (1,338 B → 1,609 B). Open-Meteo has no per-variable past window, so this is the smallest request that gives the Precipitation card its "in last 24h" total. |
| Weather, warm | 31 ms, 1 request, 1.3 kB on the wire |
| Sidebar, 3 cities | 27 ms, 1 request, 0.8 kB on the wire |
| Radar manifest | 192 ms, 2 requests, 2.3 kB |
| Map grid, 20 points | 36 ms, 1 request, 3.4 kB on the wire |
| Bento view, full frame | 3.3 ms |
| Radar playback, full frame | 4.6 ms (4.1 ms before the loop drew two interpolation frames at once); 4.7 ms with stitched tile borders |
| Map pan, full frame | 4.0 ms |
| Scalar field build | 0.39 ms |
| Isobar build | 0.11 ms |
| Hover readout | 5 µs |

## Cold launch of the real binary (2026-11-24)

Measured with `scripts/e2e-smoke.sh`: empty cache and config, software renderer on a virtual display.

| Metric | Value |
| --- | --- |
| Complete dashboard on screen (forecast, sidebar cities, mini-map) | within 0.58 s of launch |
| Resident memory after 5.5 s | 96 MB |
