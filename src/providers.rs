use crate::astro::{compute_sun_times, is_daytime_at};
use crate::model::{
    AirQuality, AlertSeverity, BUNDLE_SCHEMA, CurrentWeather, DailyForecast, HourlyForecast, Location, MinutelySample, ProviderChoice,
    WeatherAlert, WeatherBundle, WeatherCondition, apparent_temperature_c, dew_point_from_rh, rh_from_dew_point,
};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use ureq::Agent;

static NET_REQS: AtomicU64 = AtomicU64::new(0);
static NET_BYTES_RX: AtomicU64 = AtomicU64::new(0);
static NET_JSON_REQS: AtomicU64 = AtomicU64::new(0);
static NET_JSON_BYTES: AtomicU64 = AtomicU64::new(0);
static NET_TILE_REQS: AtomicU64 = AtomicU64::new(0);
static NET_TILE_BYTES: AtomicU64 = AtomicU64::new(0);

static NET_WIRE_BYTES_RX: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub fn reset_net_metrics() {
    for c in [&NET_REQS, &NET_BYTES_RX, &NET_WIRE_BYTES_RX, &NET_JSON_REQS, &NET_JSON_BYTES, &NET_TILE_REQS, &NET_TILE_BYTES] {
        c.store(0, Ordering::Relaxed);
    }
}
#[cfg(test)]
pub fn snapshot_net_metrics() -> (u64, u64, u64, u64, u64, u64) {
    (
        NET_REQS.load(Ordering::Relaxed),
        NET_BYTES_RX.load(Ordering::Relaxed),
        NET_JSON_REQS.load(Ordering::Relaxed),
        NET_JSON_BYTES.load(Ordering::Relaxed),
        NET_TILE_REQS.load(Ordering::Relaxed),
        NET_TILE_BYTES.load(Ordering::Relaxed),
    )
}
#[cfg(test)]
pub fn snapshot_wire_bytes() -> u64 {
    NET_WIRE_BYTES_RX.load(Ordering::Relaxed)
}

type StationCache = Mutex<HashMap<(i32, i32), Vec<String>>>;
type AqiEntry = crate::storage::AqiDiskEntry;
type AlertsEntry = crate::storage::AlertsDiskEntry;
type NwsObsEntry = crate::storage::NwsObsDiskEntry;
static NWS_STATIONS: LazyLock<StationCache> = LazyLock::new(|| Mutex::new(crate::storage::load_nws_station_cache()));
static NWS_OBS_CACHE: LazyLock<Mutex<HashMap<String, NwsObsEntry>>> =
    LazyLock::new(|| Mutex::new(crate::storage::load_nws_obs_cache(3600)));
static AQI_CACHE: LazyLock<Mutex<HashMap<(i32, i32), AqiEntry>>> = LazyLock::new(|| Mutex::new(crate::storage::load_aqi_cache(1800)));
static ALERTS_CACHE: LazyLock<Mutex<HashMap<(i32, i32), AlertsEntry>>> =
    LazyLock::new(|| Mutex::new(crate::storage::load_alerts_cache(3600)));
static LAST_HRRR: Mutex<Option<(i64, String, bool, std::time::Instant)>> = Mutex::new(None);
/// Raw MET Norway response, its `Last-Modified` and the time it was fetched / last revalidated. The response is
/// re-parsed on every use so "current" is always the sample nearest to the present hour.
type MetNoCacheEntry = (Value, Option<String>, i64);
/// Observation stations farther than this (≈ 40 km) do not describe the selected place.
const OBS_MAX_DIST_DEG: f64 = 0.36;
/// ...and neither do stations whose elevation differs from the forecast point's by more than this.
const OBS_MAX_ELEV_DIFF_M: f64 = 150.0;
/// Result of one alert lookup: `(alerts, source tag, time the feed answered)`; `None` = the feed did not answer.
type AlertsFetch = Option<(Vec<WeatherAlert>, String, i64)>;
/// One alert-feed response: `Some((alerts, ETag))`, or `None` for an HTTP 304 (unchanged).
type AlertsResponse = Option<(Vec<WeatherAlert>, Option<String>)>;
static METNO_CACHE: LazyLock<Mutex<HashMap<(i32, i32), MetNoCacheEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static OPEN_METEO_DOWN_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static OPEN_METEO_FAILS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn open_meteo_healthy() -> bool {
    OPEN_METEO_DOWN_UNTIL.lock().ok().and_then(|g| *g).is_none_or(|until| std::time::Instant::now() >= until)
}
fn mark_open_meteo_ok() {
    OPEN_METEO_FAILS.store(0, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut g) = OPEN_METEO_DOWN_UNTIL.lock() {
        *g = None;
    }
}
fn mark_open_meteo_down() {
    if OPEN_METEO_FAILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 >= 2
        && let Ok(mut g) = OPEN_METEO_DOWN_UNTIL.lock()
    {
        *g = Some(std::time::Instant::now() + Duration::from_secs(15));
    }
}
const UA: &str = "weather-rs/0.2.0 (https://github.com/pythoncrazy/weather-rs)";

#[derive(Debug, Clone, Copy)]
pub struct GridWeatherPoint {
    pub lat: f64,
    pub lon: f64,
    /// `[temperature °C, (unused), wind u m/s, wind v m/s, wind speed m/s, sea-level pressure hPa]`.
    pub v: [f32; 6],
    pub hourly: [(i64, [f32; 6]); 14],
    pub hourly_len: u8,
}
pub fn wind_uv(speed: f64, from_deg: f64) -> (f32, f32) {
    let r = (from_deg + 180.0).to_radians();
    ((speed * r.sin()) as f32, (speed * r.cos()) as f32)
}
impl GridWeatherPoint {
    pub fn stationary(lat: f64, lon: f64, v: [f32; 6]) -> Self {
        Self { lat, lon, v, hourly: [(0, [0.0; 6]); 14], hourly_len: 0 }
    }
    pub fn at_timestamp(&self, target: Option<i64>) -> Self {
        let (Some(ts), n) = (target, (self.hourly_len as usize).min(14)) else { return *self };
        if n == 0 {
            return *self;
        }
        let s = &self.hourly[..n];
        let v = if ts <= s[0].0 {
            s[0].1
        } else if let Some(&[a, b]) = s.array_windows::<2>().find(|&&[_, b]| ts <= b.0) {
            let t = ((ts - a.0) as f32 / (b.0 - a.0).max(1) as f32).clamp(0.0, 1.0);
            let mut m = std::array::from_fn(|i| a.1[i] + (b.1[i] - a.1[i]) * t);
            (m[1], m[4]) = (m[1].max(0.0), m[4].max(m[2].hypot(m[3])));
            m
        } else {
            s[n - 1].1
        };
        Self { v, ..*self }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RadarFrameSource {
    Tile(String, bool),
    NoaaHrrr(String, u32, bool),
    #[default]
    #[serde(alias = "GlobalNwp")]
    EcccNwp,
}
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RainViewerFrame {
    pub time_unix: i64,
    pub is_nowcast: bool,
    pub source: RadarFrameSource,
}
/// One ECCC interpolation slot: `(UTC hour, index within the hour)`.
pub type EcccSlot = (i64, u8);
/// Motion-interpolated ECCC slots per hour (7.5 minutes apart, so every quarter-hour timeline frame lands on a slot).
pub const ECCC_SUBSTEPS: u8 = 8;
/// Whether forecast-map frames between hourly ECCC model outputs are motion-interpolated (the user's
/// "Forecast Map: Smooth / Model Frames" setting).
static FORECAST_INTERPOLATION: AtomicBool = AtomicBool::new(true);
/// Applies the forecast-map interpolation setting; call before (re)building the map's timeline namespaces.
pub fn set_forecast_interpolation(on: bool) {
    FORECAST_INTERPOLATION.store(on, Ordering::Relaxed);
}
/// Current forecast-map interpolation setting.
pub fn forecast_interpolation() -> bool {
    FORECAST_INTERPOLATION.load(Ordering::Relaxed)
}
/// Environment Canada (ECCC GeoMet WMS) multi-tier NWP precipitation rate models, ordered finest to coarsest:
/// - `Hrdps`: 2.5 km Continental High-Resolution Deterministic Prediction System (Canada + US PNW/Northern US/NE Atlantic, `z=6`).
/// - `Rdps`: 10 km Regional Deterministic Prediction System (all North America + 2,500 km offshore Pacific/Atlantic, `z=5`).
/// - `Gdps`: 15 km Global Deterministic Prediction System (worldwide fallback outside HRRR/HRDPS/RDPS, `z=5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EcccModel {
    Hrdps,
    Rdps,
    Gdps,
}

impl EcccModel {
    pub const ALL: [Self; 3] = [Self::Hrdps, Self::Rdps, Self::Gdps];
    #[inline]
    pub const fn wms_layer(self) -> &'static str {
        match self {
            Self::Hrdps => "HRDPS.CONTINENTAL_RT",
            Self::Rdps => "RDPS_10km_PrecipRate",
            Self::Gdps => "GDPS_15km_PrecipRate",
        }
    }
    #[inline]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Hrdps => "hrdps",
            Self::Rdps => "rdps",
            Self::Gdps => "gdps",
        }
    }
    #[inline]
    pub const fn max_native_zoom(self) -> u8 {
        match self {
            // 512×512 WMS tiles carry 2× linear pixel density (== z+1 at 256px) with 4× fewer HTTP round trips!
            // Cap each model at its meteorological grid limit while unlocking progressive ultra-hi-res zoom when zoomed in:
            // - HRDPS 2.5 km: z=5 @ 512×512 at regional zooms (`bias=-1.25`), unlocking z=6 (~0.85 km/px) when zoomed into cities (`z >= 7.25`)!
            Self::Hrdps => 6,
            // - RDPS 10 km: z=4 @ 512×512 (~3.8 km/px at 38°N — 2.6× finer than 10 km native grid!)
            Self::Rdps => 4,
            // - GDPS 15 km: z=3 @ 512×512 (~6.1–7.7 km/px — 2×–2.5× finer than 15 km native grid, 3× fewer WMS calls!)
            Self::Gdps => 3,
        }
    }
    /// Margin of real neighboring data (px) requested around every hourly raster of this model (see `map::eccc`).
    /// It must cover the farthest a storm can travel in an hour at the raster's resolution: 64 px for 2.5 km HRDPS;
    /// RDPS / GDPS rasters are fetched one zoom level coarser (≥ 7 km per pixel), where 32 px is ample.
    #[inline]
    pub const fn gutter_px(self) -> u32 {
        match self {
            Self::Hrdps => 64,
            Self::Rdps | Self::Gdps => 32,
        }
    }
    /// Per-tier 512×512 zoom bias matched to each model's native grid spacing so 1440p/4K displays stay under
    /// ECCC GeoMet's ~15 req/s per-IP server ceiling while preserving full meteorological detail!
    #[inline]
    pub const fn zoom_bias(self) -> f64 {
        match self {
            Self::Hrdps | Self::Rdps => -1.25,
            Self::Gdps => -1.65,
        }
    }
}

impl RainViewerFrame {
    /// ECCC slots to draw at an arbitrary playback time: the slot at or before it, and — when the time falls between
    /// two slots — the next slot with its blend weight `0..1`.
    ///
    /// Interpolated mode walks the `ECCC_SUBSTEPS` motion-interpolated slots of each hour, so consecutive slots are
    /// only a few minutes of storm travel apart. Model-frames mode holds each issued hour and cross-fades to the next
    /// over the ten minutes around the half hour.
    pub fn eccc_blend_at(time_unix: i64, interpolate: bool) -> (EcccSlot, Option<(EcccSlot, f32)>) {
        let snap = |first: (i64, u8), second: (i64, u8), frac: f32| {
            if frac <= 0.02 {
                (first, None)
            } else if frac >= 0.98 {
                (second, None)
            } else {
                (first, Some((second, frac)))
            }
        };
        let h0 = time_unix.div_euclid(3600) * 3600;
        if !interpolate {
            let u = ((time_unix - h0 - 1500) as f32 / 600.0).clamp(0.0, 1.0);
            return snap((h0, 0), (h0 + 3600, 0), u * u * (3.0 - 2.0 * u));
        }
        let step = 3600 / ECCC_SUBSTEPS as i64;
        let k = (time_unix - h0) / step;
        let slot = |k: i64| if k >= ECCC_SUBSTEPS as i64 { (h0 + 3600, 0) } else { (h0, k as u8) };
        snap(slot(k), slot(k + 1), (time_unix - h0 - k * step) as f32 / step as f32)
    }
    /// Tile namespaces (`[HRDPS, RDPS, GDPS]`) of one ECCC slot, composited as an exact partition of unity outside
    /// NOAA HRRR (`outside_hrrr`, for `NoaaHrrr` frames) or over the whole globe (for `EcccNwp` frames).
    pub fn eccc_slot_namespaces(outside_hrrr: bool, (h0, k): (i64, u8)) -> [String; 3] {
        let mode = if outside_hrrr { "out" } else { "full" };
        EcccModel::ALL.map(|m| format!("eccc_{}_{mode}_v8_{h0}_{k}", m.tag()))
    }
    /// Official Environment and Climate Change Canada (ECCC GeoMet WMS) real NWP precipitation rate tile URL
    /// (`HRDPS.CONTINENTAL_RT` 2.5 km / `RDPS_10km_PrecipRate` 10 km / `GDPS_15km_PrecipRate` 15 km, all 100% hourly `+1h..+12h`).
    /// Requests `512×512` tiles so each HTTP call delivers 4× the area at identical native resolution (`4.5×–7×` faster loop load!).
    #[cfg(test)]
    pub fn eccc_wms_url_for(model: EcccModel, hour_unix: i64, z: u8, x: u32, y: u32) -> String {
        Self::eccc_wms_request(model, hour_unix, z, x, y, 0).map_or_else(String::new, |r| r.0)
    }
    /// Builds the ECCC GeoMet WMS `GetMap` request for tile `(z, x, y)` at UTC hour `hour_unix`, padded by a `gutter`-pixel
    /// margin of real neighboring data on every side that has a neighbor (never across the ±180° antimeridian or the
    /// Web Mercator poles). Returns `(url, [left, top, width, height])`: the tile itself occupies the `512×512` block at
    /// pixel offset `(left, top)` inside the returned `width×height` raster.
    ///
    /// The margin lets [`crate::map`] smooth and motion-interpolate hourly rasters without tile-edge artifacts.
    pub fn eccc_wms_request(model: EcccModel, hour_unix: i64, z: u8, x: u32, y: u32, gutter: u32) -> Option<(String, [u32; 4])> {
        const R: f64 = 20_037_508.342_789_244;
        let n_u = 1_u32 << z;
        let n = n_u as f64;
        let w = 2.0 * R / n;
        let res = w / 512.0;
        let mut minx = -R + x as f64 * w;
        let mut maxx = minx + w;
        // ECCC GeoMet MapServer falls back from bilinear to coarse 4x4/8x8 nearest-neighbor blocks when an EPSG:3857
        // BBOX touches within 1 pixel (~16-42 km) of the ±180° antimeridian seam (`x == 0` or `x == n - 1`).
        // Insetting only the antimeridian edge by ~1 pixel restores 100% silky full-res bilinear output with zero interior seam!
        let anti_inset = (w / 240.0).max(16_000.0);
        let (mut gl, mut gr) = (gutter, gutter);
        if x == 0 {
            minx += anti_inset;
            gl = 0;
        }
        if x + 1 == n_u {
            maxx -= anti_inset;
            gr = 0;
        }
        let (gt, gb) = (if y == 0 { 0 } else { gutter }, if y + 1 >= n_u { 0 } else { gutter });
        minx -= gl as f64 * res;
        maxx += gr as f64 * res;
        let maxy = R - y as f64 * w + gt as f64 * res;
        let miny = R - (y + 1) as f64 * w - gb as f64 * res;
        let iso = DateTime::from_timestamp(hour_unix, 0)?.format("%Y-%m-%dT%H:00:00Z").to_string();
        let layer = model.wms_layer();
        let (pw, ph) = (512 + gl + gr, 512 + gt + gb);
        Some((
            format!(
                "https://geo.weather.gc.ca/geomet?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={layer}&STYLES=PRECIPPRTMMH-LINEAR&CRS=EPSG:3857&BBOX={minx:.2},{miny:.2},{maxx:.2},{maxy:.2}&WIDTH={pw}&HEIGHT={ph}&FORMAT=image/webp&TRANSPARENT=TRUE&TIME={iso}"
            ),
            [gl, gt, pw, ph],
        ))
    }
    #[cfg(test)]
    pub fn eccc_wms_url(hour_unix: i64, z: u8, x: u32, y: u32) -> String {
        Self::eccc_wms_url_for(EcccModel::Gdps, hour_unix, z, x, y)
    }
    pub fn tile_namespace(&self) -> Option<String> {
        match &self.source {
            RadarFrameSource::Tile(_, lwxr) => Some(format!("{}_{}", if *lwxr { "lwxr8_raw512" } else { "rv512" }, self.time_unix)),
            RadarFrameSource::NoaaHrrr(init, lead, refp) => Some(format!("hrrr_{}_{init}_{lead:04}", if *refp { "refp" } else { "refd" })),
            RadarFrameSource::EcccNwp => None,
        }
    }
    pub fn max_native_zoom(&self) -> u8 {
        match self.source {
            // NOAA HRRR is a 3 km grid. A z=6 tile already draws every model cell at least 2 px wide (~1.9 km/px at
            // 38°N); IEM's z=7 tiles repeat the very same cells at 4 px each — measured identical data — for four
            // times the requests, so z=6 is the deepest level worth fetching.
            RadarFrameSource::NoaaHrrr(..) => 6,
            // LibreWXR / RainViewer 512×512 tiles at z=6 deliver identical ~0.96 km/px density to z=7 @ 256px
            // with 3.6× fewer HTTP requests and 3.6× fewer tile slots on 1440p/4K displays!
            RadarFrameSource::Tile(..) => 6,
            RadarFrameSource::EcccNwp => EcccModel::Gdps.max_native_zoom(),
        }
    }
    /// Tile zoom bias matched to each source's pixel size & native model grid spacing (zero oversampling!).
    #[inline]
    pub const fn primary_zoom_bias(&self) -> f64 {
        match self.source {
            // 512×512 live radar tiles carry 2× linear pixel density (`-0.95` == `256px` at `+0.05`, 3.6× fewer tiles!).
            RadarFrameSource::Tile(..) => -0.95,
            // 256×256 3 km HRRR tiles: z=5 (~3.8 km/px) at regional zoom, upgrading to z=6 (~1.9 km/px) when zoomed in.
            RadarFrameSource::NoaaHrrr(..) => -0.35,
            RadarFrameSource::EcccNwp => -1.25,
        }
    }
    pub fn tile_url(&self, host: &str, z: u8, x: u32, y: u32) -> Option<String> {
        match &self.source {
            RadarFrameSource::Tile(p, lwxr) => {
                // 512×512 radar tiles fetched at `z - 1` (`primary_zoom_bias = -0.95`) deliver identical screen pixel
                // density to 256×256 at `z` with 3.6× fewer HTTP round trips & tile slots on 1440p/4K screens!
                // LibreWXR is asked for unsmoothed tiles (`0_1`: its exact palettized radar values, ~5× smaller than the
                // server-blurred `1_1` PNGs) and smoothed locally; the RainViewer fallback keeps its pre-smoothed scheme.
                let (scheme, opts) = if *lwxr { (8, "0_1") } else { (2, "1_1") };
                Some(format!("{host}{p}/512/{z}/{x}/{y}/{scheme}/{opts}.png"))
            }
            RadarFrameSource::NoaaHrrr(init, lead, refp) => Some(format!(
                "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/hrrr::{}-F{lead:04}-{init}/{z}/{x}/{y}.png",
                if *refp { "REFP" } else { "REFD" }
            )),
            RadarFrameSource::EcccNwp => None,
        }
    }
}
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SatelliteFrame {
    pub time_unix: i64,
    pub path: String,
}
impl SatelliteFrame {
    pub fn tile_namespace(&self) -> String {
        format!("lwxr_sat_{}", self.time_unix)
    }
    pub fn tile_url(&self, host: &str, z: u8, x: u32, y: u32) -> String {
        format!("{host}{}/512/{z}/{x}/{y}/0/0_0.png", self.path)
    }
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RainViewerManifest {
    pub host: String,
    pub frames: Vec<RainViewerFrame>,
    #[serde(default)]
    pub satellite: Vec<SatelliteFrame>,
}

#[derive(Clone)]
pub struct WeatherClient(Agent);
impl Default for WeatherClient {
    fn default() -> Self {
        Self::new()
    }
}

struct CountingReader<R> {
    inner: R,
    count: u64,
}
impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        Ok(n)
    }
}

fn decode_json_response(resp: &mut ureq::http::Response<ureq::Body>) -> Result<(Value, u64, u64), String> {
    let is_gzip = resp.headers().get("content-encoding").and_then(|v| v.to_str().ok()).is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
    let wire = resp.body_mut().with_config().limit(8 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())?;
    let wire_len = wire.len() as u64;
    let (val, dec_len) = if is_gzip {
        // Stream-inflate gzip directly into serde_json via zlib-rs (zero intermediate Vec<u8> copy + exact wire bytes!).
        let mut cr = CountingReader { inner: flate2::read::GzDecoder::new(wire.as_slice()), count: 0 };
        let v: Value = serde_json::from_reader(&mut cr).map_err(|e| e.to_string())?;
        (v, cr.count)
    } else {
        let v: Value = serde_json::from_slice(&wire).map_err(|e| e.to_string())?;
        (v, wire_len)
    };
    Ok((val, wire_len, dec_len))
}

fn build_agent(ms: u64, idle: usize) -> Agent {
    Agent::config_builder()
        .timeout_connect(Some(Duration::from_millis(3_500)))
        .timeout_recv_response(Some(Duration::from_millis(ms.min(4_500))))
        .timeout_global(Some(Duration::from_millis(ms)))
        .max_idle_connections_per_host(idle.min(32))
        .max_idle_connections(idle)
        .user_agent(UA)
        .build()
        .into()
}
fn get_json_t(a: &Agent, u: &str, geo: bool, timeout_ms: u64) -> Result<Value, String> {
    let mut r = a.get(u).config().timeout_global(Some(Duration::from_millis(timeout_ms))).build().header("Accept-Encoding", "gzip");
    if geo {
        r = r.header("Accept", "application/geo+json");
    }
    let mut resp = r.call().map_err(|e| e.to_string())?;
    let (val, wire_len, dec_len) = decode_json_response(&mut resp)?;
    NET_REQS.fetch_add(1, Ordering::Relaxed);
    NET_JSON_REQS.fetch_add(1, Ordering::Relaxed);
    NET_BYTES_RX.fetch_add(dec_len, Ordering::Relaxed);
    NET_WIRE_BYTES_RX.fetch_add(wire_len, Ordering::Relaxed);
    NET_JSON_BYTES.fetch_add(dec_len, Ordering::Relaxed);
    Ok(val)
}
type ConditionalJson = Option<(Value, Option<String>, Option<String>)>;

/// Conditional HTTP GET supporting `If-Modified-Since` and `If-None-Match` (ETag) -> `304 Not Modified` (0 bytes transferred & 0 JSON parsed!).
fn get_json_conditional(
    a: &Agent, u: &str, geo: bool, timeout_ms: u64, if_modified_since: Option<&str>, if_none_match: Option<&str>,
) -> Result<ConditionalJson, String> {
    let mut r = a
        .get(u)
        .config()
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
        .http_status_as_error(false)
        .build()
        .header("Accept-Encoding", "gzip");
    if geo {
        r = r.header("Accept", "application/geo+json");
    }
    if let Some(ims) = if_modified_since {
        r = r.header("If-Modified-Since", ims);
    }
    if let Some(inm) = if_none_match {
        r = r.header("If-None-Match", inm);
    }
    let mut resp = r.call().map_err(|e| e.to_string())?;
    let st = resp.status().as_u16();
    NET_REQS.fetch_add(1, Ordering::Relaxed);
    NET_JSON_REQS.fetch_add(1, Ordering::Relaxed);
    if st == 304 {
        return Ok(None);
    }
    if !(200..300).contains(&st) {
        return Err(format!("HTTP {st}"));
    }
    let lm = resp.headers().get("last-modified").and_then(|v| v.to_str().ok()).map(str::to_string);
    let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
    let (val, wire_len, dec_len) = decode_json_response(&mut resp)?;
    NET_BYTES_RX.fetch_add(dec_len, Ordering::Relaxed);
    NET_WIRE_BYTES_RX.fetch_add(wire_len, Ordering::Relaxed);
    NET_JSON_BYTES.fetch_add(dec_len, Ordering::Relaxed);
    Ok(Some((val, lm, etag)))
}
fn get_json(a: &Agent, u: &str, geo: bool) -> Result<Value, String> {
    get_json_t(a, u, geo, 6000)
}
fn js<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    v.pointer(p).and_then(Value::as_str)
}
fn jf(v: &Value, p: &str) -> Option<f64> {
    v.pointer(p).and_then(Value::as_f64)
}
fn ja<'a>(v: &'a Value, p: &str) -> &'a [Value] {
    v.pointer(p).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}
fn parse_ts(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| Some(Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(v.as_str()?, "%Y-%m-%dT%H:%M").ok()?).timestamp()))
}
fn rfc3339(v: &Value, p: &str) -> Option<i64> {
    Some(DateTime::parse_from_rfc3339(js(v, p)?).ok()?.timestamp())
}
/// Finite number at JSON pointer `p`; `None` for a missing / null / non-finite value (callers never substitute a default).
fn fin(v: &Value, p: &str) -> Option<f64> {
    jf(v, p).filter(|x| x.is_finite())
}
fn wrap_lon(lo: f64) -> f64 {
    ((lo + 180.0).rem_euclid(360.0)) - 180.0
}
fn coords_csv(c: &[(f64, f64)]) -> (String, String) {
    let j = |f: fn(&(f64, f64)) -> String| c.iter().map(f).collect::<Vec<_>>().join(",");
    (j(|&(la, _)| format!("{:.2}", la.clamp(-85.0, 85.0))), j(|&(_, lo)| format!("{:.2}", wrap_lon(lo))))
}

/// Converts an observed wind value to m/s; a value in an unrecognised unit is `None` rather than assumed to be m/s.
fn nws_wind_to_mps(v: f64, u: &str) -> Option<f64> {
    if u.contains("km_h-1") {
        Some(v / 3.6)
    } else if u.contains("kt") || u.contains("kn") {
        Some(v * 0.514_444)
    } else if u.contains("mi_h-1") {
        Some(v * 0.447_04)
    } else if u.contains("m_s-1") {
        Some(v)
    } else {
        None
    }
}
/// Folds one alert-feed response into the cache: `Ok(Some)` is a fresh answer, `Ok(None)` an HTTP 304, `Err` a failed
/// request. A failure never becomes "no alerts": the alerts last known for the point are returned with the time they
/// were fetched and nothing is cached, so the next refresh asks again; with nothing known the result is `None` and the
/// caller keeps whatever it already shows. Returns `(result, cache entry to store)`.
fn settle_alerts(
    res: Result<AlertsResponse, String>, cached: Option<AlertsEntry>, src: &str, now: i64,
) -> (AlertsFetch, Option<AlertsEntry>) {
    match (res, cached) {
        (Ok(Some((alerts, etag))), _) => (Some((alerts.clone(), src.to_string(), now)), Some((alerts, src.to_string(), etag, now))),
        (Ok(None), Some((al, csrc, etag, _))) => (Some((al.clone(), csrc.clone(), now)), Some((al, csrc, etag, now))),
        (Err(_), Some((al, csrc, _, ts))) => (Some((al, csrc, ts)), None),
        (Ok(None) | Err(_), None) => (None, None),
    }
}
/// ECCC `weather-alerts` features → alerts in effect. Ended and expired alerts are skipped, an alert published for
/// several forecast zones (one feature each, identical code + text) is kept once, and only then is the list capped.
/// `None` when the payload is not a feature collection.
fn parse_eccc_alerts(val: &Value, now: i64) -> Option<Vec<WeatherAlert>> {
    let mut seen = HashSet::new();
    let mut alerts: Vec<_> = val
        .get("features")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let p = item.get("properties")?;
            if js(p, "/status_en").is_some_and(|s| s.trim().eq_ignore_ascii_case("ended"))
                || rfc3339(p, "/expiration_datetime").is_some_and(|exp| exp <= now)
            {
                return None;
            }
            let raw_ev = js(p, "/alert_name_en").or_else(|| js(p, "/alert_short_name_en"))?.trim();
            let mut chars = raw_ev.chars();
            let event = format!("{}{}", chars.next()?.to_uppercase(), chars.as_str());
            let severity = match js(p, "/alert_type").unwrap_or("").to_ascii_lowercase().as_str() {
                "warning" => AlertSeverity::Severe,
                "watch" => AlertSeverity::Moderate,
                _ => AlertSeverity::Minor,
            };
            let description = js(p, "/alert_text_en").unwrap_or("").trim();
            if !seen.insert((js(p, "/alert_code").unwrap_or(raw_ev).to_string(), description.to_string())) {
                return None;
            }
            let area = js(p, "/feature_name_en").unwrap_or("").trim();
            let headline = if area.is_empty() { event.clone() } else { format!("{event} — {area}") };
            Some(WeatherAlert { event, severity, headline, description: description.into() })
        })
        .collect();
    alerts.sort_by_key(|a| a.severity as u8);
    alerts.truncate(4);
    Some(alerts)
}
/// CAP-style alert feeds (NOAA NWS, DWD via Bright Sky, MET Norway): `arr` points at the alert array, `prop` says the
/// fields sit under `properties`, `ks` lists the `[event, headline, description]` keys. `None` for an unexpected payload.
fn parse_feed_alerts(val: &Value, arr: &str, prop: bool, ks: [&[&str]; 3]) -> Option<Vec<WeatherAlert>> {
    let mut seen = HashSet::new();
    let mut alerts: Vec<_> = val
        .pointer(arr)?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let p = if prop { item.get("properties")? } else { item };
            let pick = |k: &[&str]| k.iter().find_map(|s| js(p, s));
            let event = pick(ks[0])?.to_string();
            let severity = match js(p, "/severity").unwrap_or("").to_ascii_lowercase().as_str() {
                "extreme" => AlertSeverity::Extreme,
                "severe" => AlertSeverity::Severe,
                "moderate" => AlertSeverity::Moderate,
                _ => AlertSeverity::Minor,
            };
            let headline = pick(ks[1]).unwrap_or(&event).to_string();
            seen.insert((event.clone(), headline.clone())).then(|| WeatherAlert {
                event,
                severity,
                headline,
                description: pick(ks[2]).unwrap_or("").trim().into(),
            })
        })
        .collect();
    alerts.sort_by_key(|a| a.severity as u8);
    alerts.truncate(4);
    Some(alerts)
}
fn nws_desc_to_condition(desc: &str) -> Option<WeatherCondition> {
    use WeatherCondition as W;
    let d = desc.trim().to_ascii_lowercase();
    // "Freezing Fog" is fog, not freezing rain; mist / haze / smoke are not fog, so they fall through to the sky cover.
    const M: &[(&[&str], W)] = &[
        (&["thunder", "t-storm"], W::Thunderstorm),
        (&["freezing rain", "freezing drizzle"], W::FreezingRain),
        (&["sleet", "ice pellet", "mix"], W::Sleet),
        (&["heavy snow", "blizzard"], W::HeavySnow),
        (&["snow", "flurr"], W::Snow),
        (&["heavy rain"], W::HeavyRain),
        (&["rain", "shower"], W::Rain),
        (&["drizzle"], W::Drizzle),
        (&["fog"], W::Fog),
        (&["overcast"], W::Overcast),
        (&["mostly cloudy", "partly cloudy", "broken"], W::PartlyCloudy),
        (&["few clouds", "mostly clear", "fair"], W::MostlyClear),
        (&["clear", "sunny"], W::Clear),
    ];
    (!d.is_empty()).then(|| M.iter().find(|&&(ks, _)| ks.iter().any(|k| d.contains(k))).map(|&(_, c)| c)).flatten()
}
/// NWS-style text for a METAR: present weather first, otherwise the densest reported sky cover. A clear-sky report
/// (`CLR` / `SKC` / `CAVOK`) carries an empty `clouds` array and states its cover only in the top-level `cover` field.
fn metar_wx_desc(wx: Option<&str>, cover: Option<&str>, clouds: Option<&Vec<Value>>) -> &'static str {
    // Phenomena reported "in the vicinity" (VC…) are not at the station and say nothing about its weather.
    let here: Vec<&str> =
        wx.into_iter().flat_map(str::split_whitespace).filter(|t| !t.trim_start_matches(['+', '-']).starts_with("VC")).collect();
    if !here.is_empty() {
        let w = here.join(" ");
        if w.contains("TS") {
            return "Thunderstorm";
        }
        if w.contains("FZRA") || w.contains("FZDZ") {
            return "Freezing Rain";
        }
        if w.contains("PL") || w.contains("GR") || w.contains("GS") {
            return "Sleet";
        }
        if w.contains("+SN") {
            return "Heavy Snow";
        }
        if w.contains("SN") || w.contains("SG") {
            return "Snow";
        }
        if w.contains("+RA") || w.contains("+SHRA") {
            return "Heavy Rain";
        }
        if w.contains("RA") || w.contains("SH") {
            return "Rain";
        }
        if w.contains("DZ") {
            return "Drizzle";
        }
        // Fog proper only. Shallow / patchy / partial fog (MIFG, BCFG, PRFG) and mist, haze or smoke (BR, HZ, FU) are not
        // "Fog": the reported sky cover and visibility describe those conditions.
        if here.iter().any(|t| matches!(*t, "FG" | "FZFG")) {
            return "Fog";
        }
    }
    let rank = |c: &str| match c {
        "OVC" | "VV" | "OVX" => 4,
        "BKN" => 3,
        "SCT" => 2,
        "FEW" => 1,
        "CLR" | "SKC" | "CAVOK" => 0,
        _ => -1,
    };
    let layers = clouds.into_iter().flatten().filter_map(|o| o.get("cover").and_then(Value::as_str));
    match layers.chain(cover).map(rank).max().unwrap_or(-1) {
        4 => "Overcast",
        3 => "Mostly Cloudy",
        2 => "Partly Cloudy",
        1 => "Few Clouds",
        0 => "Clear",
        _ => "",
    }
}

type WifiAp = (String, i32, Option<u32>, bool);
fn valid_bssid(raw: &str) -> Option<(String, bool)> {
    let mac = raw.replace("\\:", ":").replace('-', ":").trim().to_ascii_lowercase();
    let b: Vec<u8> = mac.split(':').filter_map(|h| (h.len() == 2).then(|| u8::from_str_radix(h, 16).ok()).flatten()).collect();
    if b.len() != 6 || b.iter().all(|&x| x == 0) || b.iter().all(|&x| x == 0xff) || (b[0] & 0x01) != 0 {
        return None;
    }
    Some((mac, (b[0] & 0x02) != 0))
}
fn wifi_channel_to_mhz(ch: u32) -> Option<u32> {
    match ch {
        1..=13 => Some(2407 + ch * 5),
        14 => Some(2484),
        32..=177 => Some(5000 + ch * 5),
        _ => None,
    }
}
fn finalize_wifi_aps(best: BTreeMap<String, WifiAp>) -> Vec<WifiAp> {
    let mut aps: Vec<_> = best.into_values().collect();
    aps.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| b.1.cmp(&a.1)).then_with(|| a.0.cmp(&b.0)));
    aps.truncate(80);
    aps
}
fn parse_nmcli_wifi_aps(raw: &str) -> Vec<WifiAp> {
    let mut best: BTreeMap<String, WifiAp> = BTreeMap::new();
    for line in raw.lines() {
        let mut p = line.rsplitn(3, ':');
        let (Some(fs), Some(ss), Some(bs)) = (p.next(), p.next(), p.next()) else { continue };
        let Some((mac, local)) = valid_bssid(bs) else { continue };
        let dbm = -100 + ss.trim().parse::<i32>().unwrap_or(0).clamp(0, 100) / 2;
        let freq = fs.split_whitespace().next().and_then(|s| s.parse::<u32>().ok()).filter(|&f| f > 0);
        let _ = best.get(&mac).is_none_or(|prev| prev.1 < dbm).then(|| best.insert(mac.clone(), (mac, dbm, freq, local)));
    }
    finalize_wifi_aps(best)
}
/// Parses Windows `netsh wlan show networks mode=bssid` output (locale-agnostic: matches BSSID hex + `%` signal + channel number).
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn parse_netsh_wifi_aps(raw: &str) -> Vec<WifiAp> {
    let mut best: BTreeMap<String, WifiAp> = BTreeMap::new();
    let mut cur: Option<(String, bool, i32, Option<u32>)> = None;
    let flush = |best: &mut BTreeMap<String, WifiAp>, cur: &mut Option<(String, bool, i32, Option<u32>)>| {
        if let Some((mac, local, dbm, freq)) = cur.take() {
            let _ = best.get(&mac).is_none_or(|prev| prev.1 < dbm).then(|| best.insert(mac.clone(), (mac, dbm, freq, local)));
        }
    };
    for line in raw.lines() {
        let Some((lhs, rhs)) = line.split_once(':') else { continue };
        let (l, r) = (lhs.trim().to_ascii_lowercase(), rhs.trim());
        if l.starts_with("bssid") {
            flush(&mut best, &mut cur);
            if let Some((mac, local)) = valid_bssid(r) {
                cur = Some((mac, local, -85, None));
            }
        } else if let Some((_, _, ref mut dbm, ref mut freq)) = cur {
            if let Some(pct) = r.strip_suffix('%').and_then(|s| s.trim().parse::<i32>().ok()) {
                *dbm = -100 + pct.clamp(0, 100) / 2;
            } else if l.contains("channel") || l.contains("canal") || l.contains("kanal") {
                *freq = r.split_whitespace().next().and_then(|s| s.parse::<u32>().ok()).and_then(wifi_channel_to_mhz);
            }
        }
    }
    flush(&mut best, &mut cur);
    finalize_wifi_aps(best)
}
/// Parses macOS `airport -s` output (`SSID  aa:bb:cc:dd:ee:ff  -54  36,+1  ...`).
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_macos_airport_aps(raw: &str) -> Vec<WifiAp> {
    let mut best: BTreeMap<String, WifiAp> = BTreeMap::new();
    for line in raw.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        for (i, tok) in toks.iter().enumerate() {
            if tok.len() == 17
                && let Some((mac, local)) = valid_bssid(tok)
            {
                let dbm = toks.get(i + 1).and_then(|s| s.parse::<i32>().ok()).filter(|&d| (-110..=-10).contains(&d)).unwrap_or(-80);
                let freq =
                    toks.get(i + 2).and_then(|s| s.split(',').next()).and_then(|s| s.parse::<u32>().ok()).and_then(wifi_channel_to_mhz);
                let _ = best.get(&mac).is_none_or(|prev| prev.1 < dbm).then(|| best.insert(mac.clone(), (mac, dbm, freq, local)));
                break;
            }
        }
    }
    finalize_wifi_aps(best)
}
fn parse_beacondb_response(v: &Value) -> Option<(f64, f64)> {
    let (lat, lon) = (v.get("fallback").is_none().then(|| jf(v, "/location/lat"))??, jf(v, "/location/lng")?);
    (!jf(v, "/accuracy").is_some_and(|a| a > 5_000.0) && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then_some((lat, lon))
}

fn geo_dist_deg(a: (f64, f64), b: (f64, f64)) -> f64 {
    let cos = ((a.0 + b.0) * 0.5).to_radians().cos().abs().max(0.1);
    (a.0 - b.0).hypot(wrap_lon(a.1 - b.1) * cos)
}
fn geo_medoid_idx(pts: &[(f64, f64)]) -> Option<usize> {
    let n = pts.len();
    if n == 0 {
        return None;
    }
    let k = (n / 2).max(1).min(n - 1);
    (0..n).min_by(|&i, &j| {
        let score = |idx: usize| {
            let mut ds: Vec<f64> = (0..n).filter(|&m| m != idx).map(|m| geo_dist_deg(pts[idx], pts[m])).collect();
            ds.sort_by(f64::total_cmp);
            ds.into_iter().take(k).sum::<f64>()
        };
        score(i).total_cmp(&score(j))
    })
}
fn geo_cluster_centroid(pts: &[(f64, f64, f64)], cluster_deg: f64) -> Option<(f64, f64)> {
    let coords: Vec<_> = pts.iter().map(|&(la, lo, _)| (la, lo)).collect();
    let med = coords[geo_medoid_idx(&coords)?];
    let (mut sw, mut sla, mut slo) = (0.0, 0.0, 0.0);
    for &(la, lo, w) in pts {
        if geo_dist_deg((la, lo), med) <= cluster_deg {
            (sw, sla, slo) = (sw + w, sla + la * w, slo + wrap_lon(lo - med.1) * w);
        }
    }
    if sw > 0.0 { Some((sla / sw, wrap_lon(med.1 + slo / sw))) } else { Some(med) }
}

fn pb_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let (mut v, mut sh) = (0_u64, 0_u32);
    while *i < buf.len() && sh < 64 {
        let b = buf[*i];
        *i += 1;
        v |= ((b & 0x7f) as u64) << sh;
        if b & 0x80 == 0 {
            return Some(v);
        }
        sh += 7;
    }
    None
}
fn pb_walk<'a>(buf: &'a [u8], mut f: impl FnMut(u64, u64, &'a [u8])) {
    let mut i = 0;
    while i < buf.len() {
        let Some(tag) = pb_varint(buf, &mut i) else { break };
        match tag & 7 {
            0 => {
                let Some(v) = pb_varint(buf, &mut i) else { break };
                f(tag >> 3, v, &[]);
            }
            2 => {
                let Some(n) = pb_varint(buf, &mut i).map(|n| n as usize) else { break };
                if i + n > buf.len() {
                    break;
                }
                f(tag >> 3, 0, &buf[i..i + n]);
                i += n;
            }
            5 => i = i.saturating_add(4),
            1 => i = i.saturating_add(8),
            _ => break,
        }
    }
}
fn encode_apple_wloc(aps: &[WifiAp]) -> Vec<u8> {
    let mut by_sig: Vec<&WifiAp> = aps.iter().collect();
    by_sig.sort_by_key(|a| std::cmp::Reverse(a.1));
    let mut pb = Vec::new();
    for ap in by_sig.into_iter().take(12) {
        let mb = ap.0.as_bytes();
        pb.extend_from_slice(&[0x12, (mb.len() + 2) as u8, 0x0a, mb.len() as u8]);
        pb.extend_from_slice(mb);
    }
    pb.extend_from_slice(&[0x18, 0x00, 0x20, 0x01]);
    let mut req = b"\x00\x01\x00\x05en_US\x00\x13com.apple.locationd\x00\x0a8.1.12B411\x00\x00\x00\x01\x00\x00".to_vec();
    req.extend_from_slice(&(pb.len() as u16).to_be_bytes());
    req.extend_from_slice(&pb);
    req
}
fn decode_apple_wloc(data: &[u8], sig_map: &HashMap<String, i32>) -> Option<(f64, f64)> {
    if data.len() <= 10 {
        return None;
    }
    let mut hits = Vec::new();
    pb_walk(&data[10..], |fn1, _, s1| {
        if fn1 != 2 || s1.is_empty() {
            return;
        }
        let (mut mac, mut lat, mut lon, mut acc) = (String::new(), None, None, 100.0_f64);
        pb_walk(s1, |fn2, _, s2| match fn2 {
            1 => {
                mac =
                    String::from_utf8_lossy(s2).split(':').map(|o| format!("{:0>2}", o.to_ascii_lowercase())).collect::<Vec<_>>().join(":")
            }
            2 => pb_walk(s2, |fn3, v3, _| match fn3 {
                1 => lat = Some((v3 as i64) as f64 * 1e-8),
                2 => lon = Some((v3 as i64) as f64 * 1e-8),
                3 => acc = (v3 as i64).clamp(5, 100_000) as f64,
                _ => {}
            }),
            _ => {}
        });
        if let (Some(la), Some(lo)) = (lat, lon)
            && (-90.0..=90.0).contains(&la)
            && (-180.0..=180.0).contains(&lo)
            && (la + 180.0).abs() > 1.0
            && acc <= 1_500.0
        {
            let dbm = sig_map.get(&mac).copied().unwrap_or(-85) as f64;
            let w = 10.0_f64.powf((dbm + 100.0) / 20.0) / acc.max(10.0);
            hits.push((la, lo, w));
        }
    });
    geo_cluster_centroid(&hits, 0.008)
}

impl WeatherClient {
    pub fn new() -> Self {
        Self(build_agent(6000, 128))
    }
    pub fn fetch_bytes_timeout(&self, url: &str, timeout_ms: u64) -> Result<Vec<u8>, String> {
        let mut r =
            self.0.get(url).config().timeout_global(Some(Duration::from_millis(timeout_ms))).build().call().map_err(|e| e.to_string())?;
        let clen = r.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok());
        let bytes = r.body_mut().with_config().limit(8 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())?;
        NET_REQS.fetch_add(1, Ordering::Relaxed);
        NET_TILE_REQS.fetch_add(1, Ordering::Relaxed);
        NET_BYTES_RX.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        NET_WIRE_BYTES_RX.fetch_add(clen.unwrap_or(bytes.len() as u64), Ordering::Relaxed);
        NET_TILE_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    pub fn detect_current_location(&self) -> Option<(Location, bool)> {
        let agent = &self.0;
        #[cfg(target_os = "linux")]
        let aps = {
            let scan = |mode: &str| {
                std::process::Command::new("nmcli")
                    .args(["-t", "-f", "BSSID,SIGNAL,FREQ", "dev", "wifi", "list", "--rescan", mode])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| parse_nmcli_wifi_aps(&s))
                    .unwrap_or_default()
            };
            let cached = scan("no");
            if cached.len() >= 2 { cached } else { scan("auto") }
        };
        #[cfg(target_os = "windows")]
        let aps = {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            std::process::Command::new("netsh")
                .args(["wlan", "show", "networks", "mode=bssid"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| parse_netsh_wifi_aps(&s))
                .unwrap_or_default()
        };
        #[cfg(target_os = "macos")]
        let aps = {
            std::process::Command::new("/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport")
                .arg("-s")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| parse_macos_airport_aps(&s))
                .unwrap_or_default()
        };
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        let aps: Vec<WifiAp> = Vec::new();
        if !aps.is_empty() {
            let sig_map: HashMap<String, i32> = aps.iter().map(|(m, d, _, _)| (m.clone(), *d)).collect();
            let apple_pos = agent
                .post("https://gs-loc.apple.com/clls/wloc")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", "locationd/1753.17 CFNetwork/889.9 Darwin/17.2.0")
                .send(&encode_apple_wloc(&aps))
                .ok()
                .and_then(|mut r| r.body_mut().read_to_vec().ok())
                .and_then(|b| decode_apple_wloc(&b, &sig_map));
            let pos = apple_pos.or_else(|| {
                if aps.len() < 2 {
                    return None;
                }
                let wifi: Vec<_> = aps
                    .iter()
                    .map(|(mac, dbm, freq, _)| {
                        let mut m = serde_json::Map::from_iter([
                            ("macAddress".into(), Value::String(mac.clone())),
                            ("signalStrength".into(), Value::from(*dbm)),
                        ]);
                        let _ = freq.map(|f| m.insert("frequency".into(), Value::from(f)));
                        Value::Object(m)
                    })
                    .collect();
                agent
                    .post("https://api.beacondb.net/v1/geolocate")
                    .header("Content-Type", "application/json")
                    .send_json(serde_json::json!({ "wifiAccessPoints": wifi, "considerIp": false }))
                    .ok()
                    .and_then(|mut r| r.body_mut().read_json::<Value>().ok())
                    .and_then(|v| parse_beacondb_response(&v))
            });
            if let Some((lat, lon)) = pos {
                let mut loc = self.reverse_geocode(lat, lon);
                (loc.id, loc.is_current_location) = ("current_location".into(), true);
                return Some((loc, true));
            }
        }
        // Race the 2 fastest IP geo providers and return on the FIRST valid result (~100 ms instead of waiting for worst-of-4!).
        const FAST_IP: [(&str, &str, &str, &str, &str, &str); 2] = [
            ("https://get.geojs.io/v1/ip/geo.json", "/city", "/region", "/country", "/country_code", "/timezone"),
            ("https://ipwho.is/", "/city", "/region", "/country", "/country_code", "/timezone/id"),
        ];
        const FALLBACK_IP: [(&str, &str, &str, &str, &str, &str); 2] = [
            ("https://api.ip.sb/geoip", "/city", "/region", "/country", "/country_code", "/timezone"),
            ("https://freeipapi.com/api/json", "/cityName", "/regionName", "/countryName", "/countryCode", "/timeZones/0"),
        ];
        let fetch_ip =
            |ag: Agent, spec: (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str)| -> Option<Location> {
                let (url, ci, rg, co, cc, tz) = spec;
                let v = get_json_t(&ag, url, false, 2200).ok()?;
                let num = |k: &str| jf(&v, k).or_else(|| js(&v, k)?.parse::<f64>().ok());
                let mut l = Location::new(
                    js(&v, ci).unwrap_or("Current Location"),
                    js(&v, rg).unwrap_or(""),
                    js(&v, co).unwrap_or(""),
                    js(&v, cc).unwrap_or(""),
                    num("/latitude")?,
                    num("/longitude")?,
                    js(&v, tz).unwrap_or(""),
                );
                (l.id, l.is_current_location) = ("current_location".into(), true);
                Some(l)
            };
        let race_pair =
            |pair: [(&'static str, &'static str, &'static str, &'static str, &'static str, &'static str); 2]| -> Option<Location> {
                let (tx, rx) = std::sync::mpsc::channel();
                for spec in pair {
                    let (ag, tx) = (self.0.clone(), tx.clone());
                    std::thread::spawn(move || {
                        let _ = tx.send(fetch_ip(ag, spec));
                    });
                }
                drop(tx);
                while let Ok(res) = rx.recv() {
                    if let Some(loc) = res {
                        return Some(loc);
                    }
                }
                None
            };
        race_pair(FAST_IP).or_else(|| race_pair(FALLBACK_IP)).map(|l| (l, false))
    }

    pub fn search_locations(&self, query: &str) -> Vec<Location> {
        let q = query.trim();
        if q.len() < 2 {
            return Vec::new();
        }
        let url = format!(
            "https://geocoding-api.open-meteo.com/v1/search?name={}&count=8&language=en&format=json",
            utf8_percent_encode(q, NON_ALPHANUMERIC)
        );
        let Ok(v) = get_json(&self.0, &url, false) else { return Vec::new() };
        ja(&v, "/results")
            .iter()
            .filter_map(|r| {
                Some(Location::new(
                    js(r, "/name")?,
                    js(r, "/admin1").unwrap_or(""),
                    js(r, "/country").unwrap_or(""),
                    js(r, "/country_code").unwrap_or(""),
                    jf(r, "/latitude")?,
                    jf(r, "/longitude")?,
                    js(r, "/timezone").unwrap_or(""),
                ))
            })
            .collect()
    }

    pub fn reverse_geocode(&self, lat: f64, lon: f64) -> Location {
        let url = format!("https://nominatim.openstreetmap.org/reverse?lat={lat:.4}&lon={lon:.4}&format=jsonv2&zoom=10");
        get_json_t(&self.0, &url, false, 2200)
            .ok()
            .and_then(|v| {
                let name = ["/address/city", "/address/town", "/address/village", "/address/municipality", "/address/county", "/name"]
                    .iter()
                    .find_map(|k| js(&v, k))
                    .filter(|s| !s.is_empty())?;
                let cc = js(&v, "/address/country_code").unwrap_or("").to_uppercase();
                Some(Location::new(
                    name,
                    js(&v, "/address/state").unwrap_or(""),
                    js(&v, "/address/country").unwrap_or(""),
                    &cc,
                    lat,
                    lon,
                    "",
                ))
            })
            .unwrap_or_else(|| Location::new(format!("{lat:.2}°, {lon:.2}°"), "", "", "", lat, lon, ""))
    }

    fn build_radar_manifest(primary: Option<&Value>, is_lwxr: bool, hrrr: Option<(i64, String, bool)>) -> Option<RainViewerManifest> {
        let now = Utc::now().timestamp();
        let host = primary
            .and_then(|v| js(v, "/host"))
            .unwrap_or(if is_lwxr { "https://api.librewxr.net" } else { "https://tilecache.rainviewer.com" })
            .to_string();
        let mut frames = Vec::new();
        let mut satellite = Vec::new();
        if let Some(v) = primary {
            let past = ja(v, "/radar/past");
            let nc_raw = ja(v, "/radar/nowcast");
            // Cap optical-flow nowcast to the first 3 crisp frames (+10m/+20m/+30m) so +45m..+12h uses real
            // meteorological NWP model raster tiles (NOAA HRRR 3 km + ECCC RDPS 10 km / GDPS 15 km) without
            // long-range optical-flow crosshatch/waffle warping artifacts over the Pacific!
            let nc_slice = &nc_raw[..nc_raw.len().min(3)];
            for (items, nc) in [(&past[past.len().saturating_sub(6)..], false), (nc_slice, true)] {
                for it in items {
                    let (Some(t), Some(p)) = (it.pointer("/time").and_then(Value::as_i64), js(it, "/path")) else { continue };
                    frames.push(RainViewerFrame {
                        time_unix: t,
                        is_nowcast: nc && t > now + 120,
                        source: RadarFrameSource::Tile(p.into(), is_lwxr),
                    });
                }
            }
            let sat_ir = ja(v, "/satellite/infrared");
            for it in &sat_ir[sat_ir.len().saturating_sub(12)..] {
                if let (Some(t), Some(p)) = (it.pointer("/time").and_then(Value::as_i64), js(it, "/path")) {
                    satellite.push(SatelliteFrame { time_unix: t, path: p.into() });
                }
            }
        }
        let live_ts = frames.iter().rfind(|f| !f.is_nowcast).map_or(now, |f| f.time_unix).max(now - 600);
        // Model forecast frames every 15 minutes (a quarter-hour UTC grid) through +12h. HRRR is natively 15-minute;
        // hourly ECCC rasters are motion-interpolated between hours while the timeline plays (`map::eccc`).
        let mut last_ts = frames.last().map_or(live_ts, |f| f.time_unix);
        let first_off: i64 = if frames.iter().any(|f| f.is_nowcast) { 45 } else { 15 };
        for off in (first_off..=720).step_by(15) {
            let vt = ((live_ts + off * 60 + 450) / 900) * 900;
            if vt <= last_ts + 420 {
                continue;
            }
            let source = hrrr
                .as_ref()
                .and_then(|(iu, init, refp)| {
                    let lead = (vt - iu) / 60;
                    ((15..=1080).contains(&lead) && lead % 15 == 0).then(|| RadarFrameSource::NoaaHrrr(init.clone(), lead as u32, *refp))
                })
                .unwrap_or(RadarFrameSource::EcccNwp);
            // Without any radar or HRRR there is nothing to anchor a forecast-only timeline to.
            if frames.is_empty() && matches!(source, RadarFrameSource::EcccNwp) {
                break;
            }
            last_ts = vt;
            frames.push(RainViewerFrame { time_unix: vt, is_nowcast: true, source });
        }
        (!frames.is_empty()).then_some(RainViewerManifest { host, frames, satellite })
    }

    pub fn fetch_radar_manifest_streaming(&self, emit: impl FnOnce(RainViewerManifest)) {
        std::thread::scope(|s| {
            let hrrr_h = s.spawn(|| {
                let now_ts = Utc::now().timestamp();
                let h = (now_ts / 3600) * 3600;
                // Reuse recently verified HRRR run (< 25 min old and < 6h old init) to avoid redundant IEM probes!
                if let Ok(g) = LAST_HRRR.lock()
                    && let Some((t, ref st, refp, at)) = *g
                    && at.elapsed() < Duration::from_secs(1500)
                    && now_ts - t < 21_600
                {
                    return Some((t, st.clone(), refp));
                }
                // Primary fast path: query IEM's live metadata JSON for the 18h lead (`refp_1080.json`), which returns the
                // exact latest COMPLETE HRRR cycle in 1 ~190 ms GET — even when NOAA/IEM is 4-6h behind real-time UTC!
                let meta_lookup = |lead_tag: &str, refp: bool| -> Option<(i64, String, bool)> {
                    let pr = if refp { "refp" } else { "refd" };
                    let u = format!("https://mesonet.agron.iastate.edu/data/gis/images/4326/hrrr/{pr}_{lead_tag}.json");
                    let v = get_json_t(&self.0, &u, false, 1800).ok()?;
                    let iso = v["model_init_utc"].as_str()?;
                    let dt = DateTime::parse_from_rfc3339(iso).ok()?;
                    let t = dt.timestamp();
                    if now_ts - t > 25_200 || t > now_ts + 3600 {
                        return None;
                    }
                    let st = dt.format("%Y%m%d%H00").to_string();
                    Some((t, st, refp))
                };
                let probe = |t: i64, pr: &'static str, refp: bool| -> Option<(i64, String, bool)> {
                    let st = DateTime::from_timestamp(t, 0)?.format("%Y%m%d%H00").to_string();
                    let u = format!("https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/hrrr::{pr}-F0120-{st}/5/5/12.png");
                    NET_REQS.fetch_add(1, Ordering::Relaxed);
                    self.0
                        .head(&u)
                        .config()
                        .timeout_global(Some(Duration::from_millis(1400)))
                        .build()
                        .call()
                        .is_ok_and(|r| r.status() == 200)
                        .then_some((t, st, refp))
                };
                let found = meta_lookup("1080", true)
                    .or_else(|| meta_lookup("0720", true))
                    .or_else(|| {
                        std::thread::scope(|ps| {
                            let p1 = ps.spawn(|| probe(h - 3600, "REFP", true));
                            let p2 = ps.spawn(|| probe(h - 7200, "REFP", true));
                            let p3 = ps.spawn(|| probe(h - 14_400, "REFP", true));
                            p1.join().ok().flatten().or_else(|| p2.join().ok().flatten()).or_else(|| p3.join().ok().flatten())
                        })
                    })
                    .or_else(|| probe(h - 10_800, "REFP", true))
                    .or_else(|| probe(h - 18_000, "REFP", true))
                    .or_else(|| meta_lookup("0720", false));
                if let Some((t, ref st, refp)) = found
                    && let Ok(mut g) = LAST_HRRR.lock()
                {
                    *g = Some((t, st.clone(), refp, std::time::Instant::now()));
                }
                found
            });
            let (pri, is_lwxr) = get_json_t(&self.0, "https://api.librewxr.net/public/weather-maps.json", false, 3000)
                .or_else(|_| get_json_t(&self.0, "https://api.librewxr.net/public/weather-maps.json", false, 2500))
                .map(|v| (Some(v), true))
                .unwrap_or_else(|_| (get_json(&self.0, "https://api.rainviewer.com/public/weather-maps.json", false).ok(), false));
            // Wait for the parallel HRRR probe (~200 ms, capped at 1.4 s) before emitting a SINGLE final manifest so we
            // never launch 60 wasted intermediate `eccc_nwp_*` HTTP requests or jump the timeline mid-load!
            let hr = hrrr_h.join().ok().flatten();
            let _ = Self::build_radar_manifest(pri.as_ref(), is_lwxr, hr).map(emit);
        });
    }
    #[cfg(test)]
    pub fn fetch_rainviewer_manifest(&self) -> Option<RainViewerManifest> {
        let mut last = None;
        self.fetch_radar_manifest_streaming(|m| last = Some(m));
        last
    }

    pub fn fetch_grid_batch(&self, coords: &[(f64, f64)]) -> Vec<GridWeatherPoint> {
        if coords.is_empty() {
            return Vec::new();
        }
        if open_meteo_healthy() {
            let (lats, lons) = coords_csv(coords);
            // Fetch hourly only (13h, starting at current UTC hour) — eliminates duplicate `current=` payload (~12% smaller)!
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lats}&longitude={lons}&hourly=temperature_2m,wind_speed_10m,wind_direction_10m,pressure_msl&forecast_hours=13&timeformat=unixtime&wind_speed_unit=ms"
            );
            if let Ok(val) = get_json_t(&self.0, &url, false, 3200) {
                mark_open_meteo_ok();
                let pts: Vec<_> = val
                    .as_array()
                    .map_or_else(|| vec![&val], |a| a.iter().collect())
                    .into_iter()
                    .zip(coords)
                    .filter_map(|(it, &(la, lo))| {
                        let h = it.get("hourly")?;
                        let times = ja(h, "/time");
                        if times.is_empty() {
                            return None;
                        }
                        let (mut hourly, mut hl) = ([(0_i64, [0.0_f32; 6]); 14], 0_usize);
                        let opt_f = |k: &str, i: usize| h.get(k).and_then(|a| a.get(i)).and_then(Value::as_f64).filter(|x| x.is_finite());
                        for (i, tv) in times.iter().take(14).enumerate() {
                            // A sample feeds the temperature, wind and pressure maps together: hours missing any of
                            // those upstream are skipped rather than filled with a default.
                            let (Some(ts), Some(tc), Some(pr), Some(hs), Some(hd)) = (
                                tv.as_i64(),
                                opt_f("temperature_2m", i),
                                opt_f("pressure_msl", i),
                                opt_f("wind_speed_10m", i),
                                opt_f("wind_direction_10m", i),
                            ) else {
                                continue;
                            };
                            let (hu, hv) = wind_uv(hs, hd);
                            hourly[hl] = (ts, [tc as f32, 0.0, hu, hv, hs as f32, pr as f32]);
                            hl += 1;
                        }
                        if hl == 0 {
                            return None;
                        }
                        Some(GridWeatherPoint {
                            lat: la.clamp(-85.0, 85.0),
                            lon: wrap_lon(lo),
                            v: hourly[0].1,
                            hourly,
                            hourly_len: hl as u8,
                        })
                    })
                    .collect();
                if !pts.is_empty() {
                    return pts;
                }
            }
            mark_open_meteo_down();
        }
        // Resilient parallel MET Norway fallback whenever Open-Meteo is slow/unavailable: keeps Temp, Pressure isobars,
        // WindFlow streamlines, and offshore Pacific precipitation 100% live!
        self.fetch_grid_batch_metno(coords)
    }

    fn fetch_grid_batch_metno(&self, coords: &[(f64, f64)]) -> Vec<GridWeatherPoint> {
        let step = (coords.len() / 12).max(1);
        let sampled: Vec<(f64, f64)> = coords.iter().step_by(step).take(12).copied().collect();
        std::thread::scope(|s| {
            let hs: Vec<_> = sampled
                .into_iter()
                .map(|(la, lo)| {
                    s.spawn(move || {
                        let url = format!("https://api.met.no/weatherapi/locationforecast/2.0/compact?lat={la:.3}&lon={lo:.3}");
                        let val = get_json_t(&self.0, &url, false, 3000).ok()?;
                        let arr = ja(&val, "/properties/timeseries");
                        let (mut hourly, mut hl) = ([(0_i64, [0.0_f32; 6]); 14], 0_usize);
                        for entry in arr.iter().take(14) {
                            let Some(ts) = rfc3339(entry, "/time") else { continue };
                            let det = entry.pointer("/data/instant/details")?;
                            let (Some(tc), Some(pr), Some(ws), Some(wd)) = (
                                jf(det, "/air_temperature").filter(|x| x.is_finite()),
                                jf(det, "/air_pressure_at_sea_level").filter(|x| x.is_finite()),
                                jf(det, "/wind_speed").filter(|x| x.is_finite()),
                                jf(det, "/wind_from_direction").filter(|x| x.is_finite()),
                            ) else {
                                continue;
                            };
                            let (hu, hv) = wind_uv(ws, wd);
                            hourly[hl] = (ts, [tc as f32, 0.0, hu, hv, ws as f32, pr as f32]);
                            hl += 1;
                        }
                        if hl == 0 {
                            return None;
                        }
                        Some(GridWeatherPoint {
                            lat: la.clamp(-85.0, 85.0),
                            lon: wrap_lon(lo),
                            v: hourly[0].1,
                            hourly,
                            hourly_len: hl as u8,
                        })
                    })
                })
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
        })
    }

    pub fn fetch_aqi_grid_batch(&self, coords: &[(f64, f64)]) -> Vec<(f64, f64, f64)> {
        if coords.is_empty() {
            return Vec::new();
        }
        let (lats, lons) = coords_csv(coords);
        let url = format!("https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lats}&longitude={lons}&current=us_aqi");
        let Ok(val) = get_json(&self.0, &url, false) else { return Vec::new() };
        val.as_array()
            .map_or_else(|| vec![&val], |a| a.iter().collect())
            .into_iter()
            .zip(coords)
            .filter_map(|(i, &(la, lo))| {
                // The upstream US AQI, unmodified — the same number the air-quality card shows for a location.
                Some((la.clamp(-85.0, 85.0), wrap_lon(lo), jf(i, "/current/us_aqi")?))
            })
            .collect()
    }

    /// Worldwide 1-request official NOAA Aviation Weather Center METAR observation (`aviationweather.gov/api/data/metar`).
    /// Replaces 3 serial `api.weather.gov` round trips (~600 ms, US-only) with 1 gzipped call (~135 ms, ~800 B wire, global),
    /// normalizing every fresh ICAO station near the point into the NWS `properties` schema (nearest first).
    fn fetch_metar_obs(&self, lat: f64, lon: f64) -> Vec<Value> {
        let now = Utc::now().timestamp();
        let ck = format!("metar2_{}_{}", (lat * 8.0).round() as i32, (lon * 8.0).round() as i32);
        if let Some((obs, _, ts)) = NWS_OBS_CACHE.lock().ok().and_then(|g| g.get(&ck).cloned())
            && (now - ts).abs() < 300
        {
            return Self::rank_obs(obs.as_array().cloned().unwrap_or_default(), lat, lon);
        }
        let dlon = (0.45 / lat.to_radians().cos().abs().max(0.25)).min(1.5);
        let url = format!(
            "https://aviationweather.gov/api/data/metar?bbox={:.3},{:.3},{:.3},{:.3}&format=json",
            (lat - 0.45).max(-85.0),
            (lon - dlon).max(-180.0),
            (lat + 0.45).min(85.0),
            (lon + dlon).min(180.0),
        );
        let Ok(arr) = get_json_t(&self.0, &url, false, 1800) else { return Vec::new() };
        // Cache every station of the 0.125° cell's response; each caller ranks them against its own coordinates.
        let all: Vec<Value> = arr.as_array().into_iter().flatten().filter_map(|m| Self::normalize_metar(m, now)).collect();
        if !all.is_empty()
            && let Ok(mut g) = NWS_OBS_CACHE.lock()
        {
            if g.len() > 128 {
                g.clear();
            }
            g.insert(ck, (Value::Array(all.clone()), None, now));
            crate::storage::save_nws_obs_cache(&g);
        }
        Self::rank_obs(all, lat, lon)
    }

    /// Normalizes one aviationweather.gov METAR into the NWS `properties` schema; `None` when it is stale or has no
    /// temperature. A field the station did not report stays `null` — it is never filled in.
    fn normalize_metar(m: &Value, now: i64) -> Option<Value> {
        let num = |k: &str| m.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
        let ot = m.get("obsTime").and_then(Value::as_i64).filter(|ot| (now - ot).abs() <= 4500)?;
        let temp = num("temp").filter(|t| (-65.0..=60.0).contains(t))?;
        let (slat, slon) = (num("lat")?, num("lon")?);
        let dewp = num("dewp").filter(|d| (-80.0..=50.0).contains(d));
        let kt = |k: &str, max: f64| num(k).filter(|w| (0.0..=max).contains(w)).map(|kt| kt * 0.514_444);
        let hpa = |k: &str| num(k).filter(|p| (870.0..=1085.0).contains(p)).map(|p| p * 100.0);
        // `visib` is in statute miles: a number, or text such as "6+" / "10+" meaning "at least" (ICAO 9999, US 10SM).
        // The "+" is carried along as `visibilityAtLeast` so a lower bound is never shown as a measured distance.
        let (vis_sm, vis_min) = match m.get("visib") {
            Some(Value::String(s)) => (s.trim().trim_end_matches('+').parse::<f64>().ok(), s.trim().ends_with('+')),
            v => (v.and_then(Value::as_f64), false),
        };
        let vis_m = vis_sm.filter(|v| v.is_finite() && *v >= 0.0).map(|sm| sm * 1609.344);
        let desc = metar_wx_desc(
            m.get("wxString").and_then(Value::as_str),
            m.get("cover").and_then(Value::as_str),
            m.get("clouds").and_then(Value::as_array),
        );
        let tag = match m.get("icaoId").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => format!("NOAA METAR {id} obs"),
            None => "NOAA METAR obs".into(),
        };
        let val = |x: Option<f64>| serde_json::json!({ "value": x });
        let mps = |x: Option<f64>| serde_json::json!({ "value": x, "unitCode": "wmoUnit:m_s-1" });
        Some(serde_json::json!({
            "geometry": { "type": "Point", "coordinates": [slon, slat] },
            "properties": {
                "sourceTag": tag,
                "timestamp": DateTime::from_timestamp(ot, 0)?.to_rfc3339(),
                "elevation": val(num("elev")),
                "temperature": val(Some(temp)),
                "dewpoint": val(dewp),
                "relativeHumidity": val(dewp.map(|td| rh_from_dew_point(temp, td))),
                "windSpeed": mps(kt("wspd", 180.0)),
                "windGust": mps(kt("wgst", 220.0)),
                "windDirection": val(num("wdir").filter(|d| (0.0..=360.0).contains(d))),
                "seaLevelPressure": val(hpa("slp")),
                "barometricPressure": val(hpa("altim")),
                "visibility": val(vis_m),
                "visibilityAtLeast": vis_min,
                "textDescription": desc,
            }
        }))
    }

    /// Keeps the observations whose station lies within `OBS_MAX_DIST_DEG` (≈ 40 km) of the point, nearest first.
    fn rank_obs(cands: Vec<Value>, lat: f64, lon: f64) -> Vec<Value> {
        let mut near: Vec<(f64, Value)> = cands
            .into_iter()
            .filter_map(|o| {
                let d = geo_dist_deg((fin(&o, "/geometry/coordinates/1")?, fin(&o, "/geometry/coordinates/0")?), (lat, lon));
                (d <= OBS_MAX_DIST_DEG).then_some((d, o))
            })
            .collect();
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        near.into_iter().map(|(_, o)| o).collect()
    }

    /// Stations of an aviationweather.gov response usable for `(lat, lon)`: fresh, within range, nearest first.
    #[cfg(test)]
    fn parse_metar_array(arr: &Value, lat: f64, lon: f64, now: i64) -> Vec<Value> {
        Self::rank_obs(arr.as_array().into_iter().flatten().filter_map(|m| Self::normalize_metar(m, now)).collect(), lat, lon)
    }

    /// Candidate station observations for the point, best first (the caller adopts the first one that passes
    /// `apply_nws_obs`'s distance / elevation checks).
    fn fetch_nws_obs(&self, lat: f64, lon: f64) -> Vec<Value> {
        let metars = self.fetch_metar_obs(lat, lon);
        if !metars.is_empty() || !(17.0..=72.0).contains(&lat) || !(-179.0..=-64.0).contains(&lon) {
            return metars;
        }
        let key = ((lat * 20.0).round() as i32, (lon * 20.0).round() as i32);
        let ids = NWS_STATIONS.lock().ok().and_then(|m| m.get(&key).cloned()).or_else(|| {
            let st = get_json(&self.0, &format!("https://api.weather.gov/points/{lat:.4},{lon:.4}"), true).ok().and_then(|p| {
                let base = js(&p, "/properties/observationStations")?;
                let sep = if base.contains('?') { '&' } else { '?' };
                get_json(&self.0, &format!("{base}{sep}limit=4"), true).ok()
            })?;
            let mut cands: Vec<_> = ja(&st, "/features")
                .iter()
                .take(4)
                .enumerate()
                .filter_map(|(i, f)| {
                    let id = js(f, "/properties/stationIdentifier")?.to_string();
                    Some((!(id.len() == 4 && id.starts_with(['K', 'P', 'T']) && id.bytes().all(|b| b.is_ascii_alphanumeric())), i, id))
                })
                .collect();
            cands.sort_by_key(|&(n, i, _)| (n, i));
            let ids: Vec<_> = cands.into_iter().take(4).map(|(_, _, id)| id).collect();
            (!ids.is_empty()).then(|| {
                if let Ok(mut m) = NWS_STATIONS.lock() {
                    m.insert(key, ids.clone());
                    crate::storage::save_nws_station_cache(&m);
                }
                ids
            })
        });
        let now = Utc::now().timestamp();
        for (idx, id) in ids.iter().flatten().enumerate() {
            let cached = NWS_OBS_CACHE.lock().ok().and_then(|g| g.get(id).cloned());
            if let Some((ref obs, _, ts)) = cached
                && (now - ts).abs() < 300
            {
                return vec![obs.clone()];
            }
            let ims = cached.as_ref().and_then(|(_, lm, _)| lm.as_deref());
            let url = format!("https://api.weather.gov/stations/{id}/observations/latest");
            let obs = match get_json_conditional(&self.0, &url, true, 2200, ims, None) {
                Ok(None) => {
                    // 304 Not Modified: zero bytes transferred, zero JSON parsed!
                    if let Some((old, lm, _)) = cached {
                        if let Ok(mut g) = NWS_OBS_CACHE.lock() {
                            g.insert(id.clone(), (old.clone(), lm, now));
                            crate::storage::save_nws_obs_cache(&g);
                        }
                        return vec![old];
                    }
                    continue;
                }
                Ok(Some((v, lm, _))) => {
                    if let Ok(mut g) = NWS_OBS_CACHE.lock() {
                        if g.len() > 128 {
                            g.clear();
                        }
                        g.insert(id.clone(), (v.clone(), lm, now));
                        crate::storage::save_nws_obs_cache(&g);
                    }
                    v
                }
                Err(_) => continue,
            };
            let Some(p) = obs.get("properties") else { continue };
            let fresh = rfc3339(p, "/timestamp").is_some_and(|t| (now - t).abs() <= 4500);
            if fresh && jf(p, "/temperature/value").is_some_and(|t| (-65.0..=60.0).contains(&t)) {
                // Promote working station to front of cache so future refreshes succeed on the very first HTTP call!
                if idx > 0
                    && let Ok(mut m) = NWS_STATIONS.lock()
                    && let Some(list) = m.get_mut(&key)
                    && let Some(pos) = list.iter().position(|s| s == id)
                {
                    list.swap(0, pos);
                    crate::storage::save_nws_station_cache(&m);
                }
                return vec![obs];
            }
        }
        Vec::new()
    }

    /// Merges one station observation into the bundle's current conditions. Values are adopted in self-consistent
    /// groups so an observation is never combined with a model value into a contradiction: temperature / humidity /
    /// dew point / feels-like together, wind speed / direction / gust together (an observation with wind but no gust
    /// has NO gust — the model's gust is not kept beside an observed wind), and pressure, visibility and sky condition
    /// each on their own. Forecast rows (hourly, daily high / low) stay exactly as the forecast provider gave them.
    /// Returns `false`, changing nothing, for a stale, distant or differently-elevated station.
    fn apply_nws_obs(b: &mut WeatherBundle, obs: &Value, now: i64) -> bool {
        let Some(p) = obs.get("properties").filter(|p| rfc3339(p, "/timestamp").is_some_and(|t| (now - t).abs() <= 4500)) else {
            return false;
        };
        let val = |k: &str| p.get(k)?.get("value")?.as_f64().filter(|x| x.is_finite());
        if let (Some(slon), Some(slat)) = (fin(obs, "/geometry/coordinates/0"), fin(obs, "/geometry/coordinates/1"))
            && geo_dist_deg((slat, slon), (b.location.lat, b.location.lon)) > OBS_MAX_DIST_DEG
        {
            return false;
        }
        if let (Some(station), Some(point)) = (val("elevation"), b.elevation_m)
            && (station - point).abs() > OBS_MAX_ELEV_DIFF_M
        {
            return false;
        }
        let wind = |k: &str| val(k).and_then(|v| nws_wind_to_mps(v, js(p.get(k)?, "/unitCode")?));
        let (cur, mut merged) = (&mut b.current, false);
        let ws = wind("windSpeed").filter(|w| (0.0..=90.0).contains(w));
        if let Some(ws) = ws {
            cur.wind_speed_mps = ws;
            // A calm report carries direction 000 and a variable one "VRB": neither is a direction.
            cur.wind_dir_deg = val("windDirection").filter(|d| ws > 0.0 && (0.0..=360.0).contains(d));
            cur.wind_gust_mps = wind("windGust").filter(|g| (0.0..=110.0).contains(g));
            merged = true;
        }
        let (t, td) = (val("temperature").filter(|t| (-65.0..=60.0).contains(t)), val("dewpoint").filter(|d| (-80.0..=50.0).contains(d)));
        let rh =
            val("relativeHumidity").filter(|h| (0.0..=100.0).contains(h)).or_else(|| t.zip(td).map(|(t, td)| rh_from_dew_point(t, td)));
        if let (Some(t), Some(rh)) = (t, rh) {
            (cur.temp_c, cur.humidity_pct, cur.dew_point_c) = (t, rh, td.unwrap_or_else(|| dew_point_from_rh(t, rh)));
            // Feels-like is the station's own wind chill / heat index, else computed from this observation alone.
            let reported = val("windChill").or_else(|| val("heatIndex")).filter(|f| (-75.0..=75.0).contains(f));
            (cur.feels_like_c, cur.feels_like_computed) = match (reported, ws) {
                (Some(f), _) => (Some(f), false),
                (None, Some(ws)) => (Some(apparent_temperature_c(t, rh, ws)), true),
                (None, None) => (None, false),
            };
            merged = true;
        }
        let prs = val("seaLevelPressure").or_else(|| val("barometricPressure")).map(|pa| if pa > 2000.0 { pa / 100.0 } else { pa });
        if let Some(v) = prs.filter(|pr| (870.0..=1085.0).contains(pr)) {
            (cur.pressure_hpa, merged) = (Some(v), true);
        }
        if let Some(v) = val("visibility").filter(|v| *v >= 0.0) {
            // api.weather.gov has no "at least" flag, but US stations report at most 10 SM, which means "10 SM or more".
            let at_least = p.get("visibilityAtLeast").and_then(Value::as_bool).unwrap_or(v >= 16_093.0);
            (cur.visibility_m, cur.visibility_at_least, merged) = (Some(v), at_least, true);
        }
        if let Some(c) = js(p, "/textDescription").and_then(nws_desc_to_condition) {
            (cur.condition, merged) = (c, true);
        }
        merged
    }

    pub fn fetch_weather(&self, loc: &Location, p: ProviderChoice) -> Result<WeatherBundle, String> {
        self.fetch_weather_inner(loc, p, true, None)
    }
    pub fn fetch_weather_streaming(
        &self, loc: &Location, p: ProviderChoice, mut cb: impl FnMut(WeatherBundle),
    ) -> Result<WeatherBundle, String> {
        self.fetch_weather_inner(loc, p, true, Some(&mut cb))
    }
    pub fn fetch_weather_quick(&self, loc: &Location, p: ProviderChoice) -> Result<WeatherBundle, String> {
        self.fetch_weather_inner(loc, p, false, None)
    }

    fn fetch_primary_hedged(&self, loc: &Location, metno: bool) -> Result<WeatherBundle, String> {
        if !open_meteo_healthy() {
            return self.fetch_met_norway_direct(loc);
        }
        // Launch Open-Meteo; if it takes > 950 ms (edge stall), hedge with direct MET Norway in parallel and return first winner!
        let (tx, rx) = std::sync::mpsc::channel::<Result<WeatherBundle, String>>();
        let c1 = self.clone();
        let l1 = loc.clone();
        let tx1 = tx.clone();
        std::thread::spawn(move || {
            let res = c1.fetch_open_meteo(&l1, metno);
            if res.is_ok() {
                mark_open_meteo_ok();
            } else {
                mark_open_meteo_down();
            }
            let _ = tx1.send(res);
        });
        match rx.recv_timeout(Duration::from_millis(950)) {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => self.fetch_met_norway_direct(loc).map_err(|_| e),
            Err(_) => {
                let c2 = self.clone();
                let l2 = loc.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(c2.fetch_met_norway_direct(&l2));
                });
                match rx.recv() {
                    Ok(Ok(b)) => Ok(b),
                    Ok(Err(e)) => rx.recv().ok().and_then(Result::ok).ok_or(e),
                    Err(_) => Err("weather fetch aborted".into()),
                }
            }
        }
    }

    fn fetch_weather_inner(
        &self, loc: &Location, provider: ProviderChoice, enrich: bool, mut on_primary: Option<&mut dyn FnMut(WeatherBundle)>,
    ) -> Result<WeatherBundle, String> {
        let nordic = (55.0..=72.0).contains(&loc.lat) && (3.0..=32.0).contains(&loc.lon);
        std::thread::scope(|s| {
            let aqi_h = enrich.then(|| s.spawn(|| self.fetch_air_quality(loc.lat, loc.lon)));
            let alerts_h = enrich.then(|| s.spawn(|| self.fetch_alerts(loc.lat, loc.lon, &loc.country_code)));
            let nws_h = (provider != ProviderChoice::MetNorway).then(|| s.spawn(|| self.fetch_nws_obs(loc.lat, loc.lon)));
            let mut b =
                self.fetch_primary_hedged(loc, provider == ProviderChoice::MetNorway || (provider == ProviderChoice::Auto && nordic))?;
            // Without enrichment the bundle has no air quality / alerts: it is flagged lean so selecting it fetches in full.
            b.lean = !enrich;
            if let Some(cb) = on_primary.as_deref_mut() {
                cb(b.clone());
            }
            let add = |dst: &mut String, tag: &str| {
                if !tag.is_empty() && !dst.contains(tag) {
                    dst.push_str(" + ");
                    dst.push_str(tag);
                }
            };
            let (mut updated_mid, now) = (false, Utc::now().timestamp());
            // Nearest station first; one that is too far or at a different elevation is skipped for the next.
            for obs in nws_h.and_then(|h| h.join().ok()).unwrap_or_default() {
                if Self::apply_nws_obs(&mut b, &obs, now) {
                    let tag = js(&obs, "/properties/sourceTag").map(str::to_string).unwrap_or_else(|| {
                        js(&obs, "/properties/stationId").map_or_else(|| "NOAA NWS obs".into(), |id| format!("NOAA NWS {id} obs"))
                    });
                    add(&mut b.source_name, &tag);
                    updated_mid = true;
                    break;
                }
            }
            // `None` = the alert feed did not answer: `alerts_fetched_unix` stays `None` and the app keeps what it knew.
            if let Some((alerts, src, ts)) = alerts_h.and_then(|h| h.join().ok().flatten()) {
                b.alerts_fetched_unix = Some(ts);
                if !alerts.is_empty() {
                    b.alerts = alerts;
                    add(&mut b.source_name, &src);
                    updated_mid = true;
                }
            }
            // Don't hold back fast NOAA NWS obs & alerts while slow (~650ms) CAMS AQI is still in flight!
            if updated_mid
                && aqi_h.as_ref().is_some_and(|h| !h.is_finished())
                && let Some(cb) = on_primary
            {
                cb(b.clone());
            }
            if let Some((aqi, tz)) = aqi_h.and_then(|h| h.join().ok()) {
                let _ = b.location.timezone.is_empty().then(|| tz.map(|t| b.location.timezone = t));
                let _ = aqi.map(|a| {
                    b.air_quality = Some(a);
                    add(&mut b.source_name, "CAMS AQI");
                });
            }
            if enrich {
                b.full_fetched_unix = Some(b.fetched_unix);
            }
            Ok(b)
        })
    }

    /// CAMS air quality exactly as Open-Meteo publishes it: the upstream `us_aqi` is passed through unmodified.
    fn parse_air_quality(v: &Value) -> Option<AirQuality> {
        Some(AirQuality { us_aqi: fin(v, "/current/us_aqi")?.round() as i32, pm2_5: fin(v, "/current/pm2_5") })
    }

    fn fetch_air_quality(&self, lat: f64, lon: f64) -> (Option<AirQuality>, Option<String>) {
        // CAMS AQI updates hourly and is the slowest upstream endpoint (~200–700ms); cache 30 min in RAM + disk per 0.25° cell!
        let key = ((lat * 4.0).round() as i32, (lon * 4.0).round() as i32);
        let now = Utc::now().timestamp();
        if let Ok(g) = AQI_CACHE.lock()
            && let Some((aq, tz, ts)) = g.get(&key)
            && (now - *ts).abs() < 1800
        {
            return (aq.clone(), tz.clone());
        }
        let url = format!(
            "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lat:.4}&longitude={lon:.4}&timezone=auto&current=us_aqi,pm2_5"
        );
        let Ok(v) = get_json_t(&self.0, &url, false, 2200) else { return (None, None) };
        let tz = js(&v, "/timezone").filter(|s| !s.is_empty() && *s != "GMT").map(str::to_string);
        let aq = Self::parse_air_quality(&v);
        if let Ok(mut g) = AQI_CACHE.lock() {
            if g.len() > 128 {
                g.clear();
            }
            g.insert(key, (aq.clone(), tz.clone(), now));
            crate::storage::save_aqi_cache(&g);
        }
        (aq, tz)
    }

    fn fetch_alerts(&self, lat: f64, lon: f64, country_code: &str) -> AlertsFetch {
        // Every feed is queried for the exact point, so answers are cached per ~1 km cell: a coarser cell would hand one
        // place's alerts to a neighbour in a different warning zone.
        let key = ((lat * 100.0).round() as i32, (lon * 100.0).round() as i32);
        let now = Utc::now().timestamp();
        let cached = ALERTS_CACHE.lock().ok().and_then(|g| g.get(&key).cloned());
        if let Some((al, src, _, ts)) = &cached
            && (now - *ts).abs() < 300
        {
            return Some((al.clone(), src.clone(), *ts));
        }
        let inm = cached.as_ref().and_then(|(_, _, et, _)| et.as_deref());
        let cc = country_code.trim();
        let is_ca = cc.eq_ignore_ascii_case("CA") || (cc.is_empty() && (49.0..=72.0).contains(&lat) && (-141.0..=-52.0).contains(&lon));
        // `feed`: `(array pointer, fields under "properties", [event, headline, description] keys)`; `None` = ECCC schema.
        type Feed = Option<(&'static str, bool, [&'static [&'static str]; 3])>;
        let (url, geo, feed, src): (String, bool, Feed, &str) = if is_ca {
            // A point-sized bbox returns only the forecast zone containing the place. `limit` is generous because ended
            // alerts and other zones' copies are filtered out AFTER the query (see `parse_eccc_alerts`).
            (
                format!(
                    "https://api.weather.gc.ca/collections/weather-alerts/items?f=json&limit=50&bbox={lon:.4},{lat:.4},{lon:.4},{lat:.4}"
                ),
                false,
                None,
                "ECCC alerts",
            )
        } else if (cc.eq_ignore_ascii_case("US") || cc.is_empty()) && (15.0..=72.0).contains(&lat) && (-179.0..=-60.0).contains(&lon) {
            (
                format!("https://api.weather.gov/alerts/active?point={lat:.4},{lon:.4}"),
                true,
                Some(("/features", true, [&["/event"], &["/headline"], &["/description"]])),
                "NOAA NWS alerts",
            )
        } else if (47.0..=55.5).contains(&lat) && (5.5..=15.5).contains(&lon) {
            (
                format!("https://api.brightsky.dev/alerts?lat={lat:.4}&lon={lon:.4}"),
                false,
                Some((
                    "/alerts",
                    false,
                    [&["/event_en", "/event_de"], &["/headline_en", "/headline_de"], &["/description_en", "/description_de"]],
                )),
                "DWD alerts",
            )
        } else if (57.0..=72.0).contains(&lat) && (4.0..=32.0).contains(&lon) {
            (
                format!("https://api.met.no/weatherapi/metalerts/2.0/current.json?lat={lat:.4}&lon={lon:.4}"),
                false,
                Some(("/features", true, [&["/eventAwarenessName", "/event"], &["/title"], &["/description"]])),
                "MET Norway alerts",
            )
        } else {
            // No alert feed covers this place: an (empty) answer, not a failure.
            return Some((Vec::new(), String::new(), now));
        };
        let res = get_json_conditional(&self.0, &url, geo, 2200, None, inm).and_then(|r| match r {
            None => Ok(None),
            Some((v, _, etag)) => {
                let parsed = match feed {
                    None => parse_eccc_alerts(&v, now),
                    Some((arr, prop, ks)) => parse_feed_alerts(&v, arr, prop, ks),
                };
                parsed.map(|alerts| Some((alerts, etag))).ok_or_else(|| "unexpected alert payload".to_string())
            }
        });
        let (out, entry) = settle_alerts(res, cached, src, now);
        if let Some(e) = entry
            && let Ok(mut g) = ALERTS_CACHE.lock()
        {
            if g.len() > 128 {
                g.clear();
            }
            g.insert(key, e);
            crate::storage::save_alerts_cache(&g);
        }
        out
    }

    /// Fetches current conditions + today's H/L for all background sidebar cities in a SINGLE multi-coordinate Open-Meteo
    /// request, with parallel MET Norway direct fallback if Open-Meteo is degraded. The bundles are flagged `lean`: they
    /// carry real current values (including gusts, visibility and UV) but no hourly detail, air quality or alerts.
    pub fn fetch_sidebar_batch(&self, locs: &[Location], provider: ProviderChoice) -> Vec<(String, WeatherBundle)> {
        if locs.is_empty() {
            return Vec::new();
        }
        let lean = |mut b: WeatherBundle| {
            b.lean = true;
            b
        };
        if open_meteo_healthy() {
            let metno = provider == ProviderChoice::MetNorway;
            let m = if metno { "&models=metno_seamless" } else { "" };
            let lats = locs.iter().map(|l| format!("{:.4}", l.lat)).collect::<Vec<_>>().join(",");
            let lons = locs.iter().map(|l| format!("{:.4}", l.lon)).collect::<Vec<_>>().join(",");
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lats}&longitude={lons}&timezone=auto&timeformat=unixtime&wind_speed_unit=ms{m}&current=temperature_2m,relative_humidity_2m,dew_point_2m,apparent_temperature,is_day,precipitation,weather_code,pressure_msl,wind_speed_10m,wind_direction_10m,wind_gusts_10m,visibility,uv_index&daily=weather_code,temperature_2m_max,temperature_2m_min&forecast_days=1"
            );
            if let Ok(root) = get_json_t(&self.0, &url, false, 2400) {
                mark_open_meteo_ok();
                let now = Utc::now().timestamp();
                let items: Vec<&Value> = root.as_array().map_or_else(|| vec![&root], |a| a.iter().collect());
                return locs
                    .iter()
                    .zip(items)
                    .filter_map(|(loc, v)| Self::parse_open_meteo_root(v, loc, metno, now).ok().map(|b| (loc.id.clone(), lean(b))))
                    .collect();
            }
            mark_open_meteo_down();
        }
        std::thread::scope(|s| {
            let hs: Vec<_> =
                locs.iter().map(|l| s.spawn(move || self.fetch_met_norway_direct(l).ok().map(|b| (l.id.clone(), lean(b))))).collect();
            hs.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
        })
    }

    fn fetch_open_meteo(&self, location: &Location, metno: bool) -> Result<WeatherBundle, String> {
        let m = if metno { "&models=metno_seamless" } else { "" };
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={:.4}&longitude={:.4}&timezone=auto&timeformat=unixtime&wind_speed_unit=ms{m}&current=temperature_2m,relative_humidity_2m,dew_point_2m,apparent_temperature,is_day,precipitation,weather_code,pressure_msl,wind_speed_10m,wind_direction_10m,wind_gusts_10m,visibility,uv_index&minutely_15=precipitation&forecast_minutely_15=8&hourly=temperature_2m,precipitation_probability,precipitation,weather_code,pressure_msl,wind_speed_10m,wind_direction_10m,uv_index&past_hours=1&forecast_hours=26&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_sum,precipitation_probability_max,wind_speed_10m_max&forecast_days=10",
            location.lat, location.lon
        );
        let root = get_json(&self.0, &url, false)?;
        Self::parse_open_meteo_root(&root, location, metno, Utc::now().timestamp())
    }

    /// Parses one Open-Meteo forecast object. Nothing is defaulted: the bundle is rejected when a core current value
    /// (time, temperature, humidity, wind speed, weather code) is missing, an hourly / daily row is dropped when one of
    /// its core values is missing, and every other missing value stays `None`.
    fn parse_open_meteo_root(root: &Value, location: &Location, metno: bool, fetched: i64) -> Result<WeatherBundle, String> {
        let cur = root.get("current").ok_or("Missing Open-Meteo current block")?;
        let mut loc = location.clone();
        let _ = js(root, "/timezone").filter(|s| !s.is_empty() && *s != "GMT").map(|tz| loc.timezone = tz.into());
        let wmo = |c: Option<f64>| c.and_then(|c| WeatherCondition::from_wmo(c as i32));
        let now_ts = cur.get("time").and_then(parse_ts).ok_or("Missing Open-Meteo current time")?;
        let tc = fin(cur, "/temperature_2m").ok_or("Missing Open-Meteo current temperature")?;
        let rh = fin(cur, "/relative_humidity_2m").ok_or("Missing Open-Meteo current humidity")?;
        let ws = fin(cur, "/wind_speed_10m").ok_or("Missing Open-Meteo current wind speed")?;
        let condition = wmo(fin(cur, "/weather_code")).ok_or("Missing Open-Meteo current weather code")?;
        let mut hourly = Vec::new();
        if let Some(h) = root.get("hourly") {
            let opt_f = |k: &str, i: usize| h.get(k).and_then(|a| a.get(i)).and_then(Value::as_f64).filter(|x| x.is_finite());
            for (i, tv) in ja(h, "/time").iter().enumerate() {
                let Some(ts) = parse_ts(tv).filter(|&ts| ts >= now_ts - 3600 && hourly.len() < 36) else { continue };
                let (Some(temp_c), Some(condition), Some(precip_mm), Some(wind_speed_mps), Some(wind_dir_deg)) = (
                    opt_f("temperature_2m", i),
                    wmo(opt_f("weather_code", i)),
                    opt_f("precipitation", i),
                    opt_f("wind_speed_10m", i),
                    opt_f("wind_direction_10m", i),
                ) else {
                    continue;
                };
                hourly.push(HourlyForecast {
                    timestamp_unix: ts,
                    temp_c,
                    condition,
                    is_day: is_daytime_at(loc.lat, loc.lon, ts),
                    precip_prob_pct: opt_f("precipitation_probability", i),
                    precip_mm,
                    wind_speed_mps,
                    wind_dir_deg,
                    pressure_hpa: opt_f("pressure_msl", i),
                    uv_index: opt_f("uv_index", i),
                });
            }
        }
        // Open-Meteo's own apparent temperature; if absent it is computed from this same model instant and flagged.
        let (feels, feels_like_computed) =
            fin(cur, "/apparent_temperature").map_or_else(|| (apparent_temperature_c(tc, rh, ws), true), |f| (f, false));
        let current = CurrentWeather {
            temp_c: tc,
            feels_like_c: Some(feels),
            feels_like_computed,
            condition,
            is_day: fin(cur, "/is_day").map_or_else(|| is_daytime_at(loc.lat, loc.lon, now_ts), |x| x >= 0.5),
            wind_speed_mps: ws,
            wind_gust_mps: fin(cur, "/wind_gusts_10m"),
            wind_dir_deg: fin(cur, "/wind_direction_10m"),
            humidity_pct: rh,
            dew_point_c: fin(cur, "/dew_point_2m").unwrap_or_else(|| dew_point_from_rh(tc, rh)),
            pressure_hpa: fin(cur, "/pressure_msl"),
            visibility_m: fin(cur, "/visibility"),
            visibility_at_least: false,
            uv_index: fin(cur, "/uv_index"),
            precip_mm: fin(cur, "/precipitation"),
            updated_unix: now_ts,
        };
        // Every 15-minute sample the API returned, as a rate (mm per 15 min × 4); nothing is resampled or extended.
        let minutely = root.get("minutely_15").map_or_else(Vec::new, |m| {
            ja(m, "/time")
                .iter()
                .enumerate()
                .filter_map(|(i, tv)| {
                    let mm = m.get("precipitation")?.get(i)?.as_f64().filter(|x| x.is_finite() && *x >= 0.0)?;
                    Some(MinutelySample { timestamp_unix: parse_ts(tv)?, precip_mm_hr: mm * 4.0 })
                })
                .collect()
        });
        let daily = root.get("daily").map_or_else(Vec::new, |d| {
            let opt_d = |k: &str, i: usize| d.get(k).and_then(|a| a.get(i)).and_then(Value::as_f64).filter(|x| x.is_finite());
            ja(d, "/time")
                .iter()
                .enumerate()
                .take(10)
                .filter_map(|(i, tv)| {
                    let (tmin, tmax, condition) =
                        (opt_d("temperature_2m_min", i)?, opt_d("temperature_2m_max", i)?, wmo(opt_d("weather_code", i))?);
                    // Daily rows are stamped with local midnight in the response's own UTC offset: that offset names the date.
                    let date = match (tv.as_str(), fin(root, "/utc_offset_seconds")) {
                        (Some(s), _) => s.to_string(),
                        (None, Some(off)) => DateTime::from_timestamp(parse_ts(tv)? + off as i64, 0)?.format("%Y-%m-%d").to_string(),
                        (None, None) => loc.local_date(parse_ts(tv)?).format("%Y-%m-%d").to_string(),
                    };
                    let sun = NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok().map(|nd| compute_sun_times(loc.lat, loc.lon, nd));
                    let ts_opt = |k: &str| d.get(k).and_then(|a| a.get(i)).and_then(parse_ts).filter(|&x| x > 0);
                    Some(DailyForecast {
                        sunrise_unix: ts_opt("sunrise").or_else(|| sun.and_then(|s| s.sunrise_unix)),
                        sunset_unix: ts_opt("sunset").or_else(|| sun.and_then(|s| s.sunset_unix)),
                        date,
                        temp_min_c: tmin,
                        temp_max_c: tmax,
                        condition,
                        precip_prob_pct: opt_d("precipitation_probability_max", i),
                        precip_sum_mm: opt_d("precipitation_sum", i),
                        wind_max_mps: opt_d("wind_speed_10m_max", i),
                        uv_max: opt_d("uv_index_max", i),
                        partial: false,
                    })
                })
                .collect()
        });
        Ok(WeatherBundle {
            location: loc,
            current,
            minutely,
            hourly,
            daily,
            air_quality: None,
            alerts: Vec::new(),
            source_name: if metno { "MET Norway (via Open-Meteo)" } else { "Open-Meteo (best-match model)" }.into(),
            fetched_unix: fetched,
            full_fetched_unix: None,
            alerts_fetched_unix: None,
            lean: false,
            elevation_m: fin(root, "/elevation"),
            uv_clear_sky: false,
            schema: BUNDLE_SCHEMA,
        })
    }

    /// Direct MET Norway Locationforecast 2.0 automatic resilience fallback with `If-Modified-Since` 304 support.
    /// Uses `/complete` (like Breezy Weather's MetNoService) for the dew point, apparent temperature and — where MET
    /// publishes them — clear-sky UV, wind gust and probability of precipitation.
    fn fetch_met_norway_direct(&self, orig: &Location) -> Result<WeatherBundle, String> {
        let key = ((orig.lat * 20.0).round() as i32, (orig.lon * 20.0).round() as i32);
        let now = Utc::now().timestamp();
        let cached = METNO_CACHE.lock().ok().and_then(|g| g.get(&key).cloned());
        if let Some((root, _, ts)) = &cached
            && now - *ts < 300
        {
            return Self::parse_met_norway(root, orig, now, *ts);
        }
        let ims = cached.as_ref().and_then(|(_, lm, _)| lm.as_deref());
        let u = format!("https://api.met.no/weatherapi/locationforecast/2.0/complete?lat={:.4}&lon={:.4}", orig.lat, orig.lon);
        let (root, last_mod) = match get_json_conditional(&self.0, &u, false, 4500, ims, None)? {
            Some((v, lm, _)) => (v, lm),
            // 304 Not Modified: MET confirms the cached response is still its latest.
            None => match cached {
                Some((root, lm, _)) => (root, lm),
                None => return Err("met.no 304 without cache".into()),
            },
        };
        let bundle = Self::parse_met_norway(&root, orig, now, now)?;
        if let Ok(mut g) = METNO_CACHE.lock() {
            if g.len() > 64 {
                g.clear();
            }
            g.insert(key, (root, last_mod, now));
        }
        Ok(bundle)
    }

    /// MET Norway `symbol_code` → condition. An unrecognised symbol is `None`, never a guessed condition.
    fn met_symbol_condition(s: &str) -> Option<WeatherCondition> {
        use WeatherCondition as W;
        let base = s.split('_').next().unwrap_or("");
        Some(if base.contains("thunder") {
            W::Thunderstorm
        } else if base.contains("heavysnow") {
            W::HeavySnow
        } else if base.contains("snow") {
            W::Snow
        } else if base.contains("sleet") {
            W::Sleet
        } else if base.contains("heavyrain") {
            W::HeavyRain
        } else if base.contains("rain") {
            W::Rain
        } else {
            match base {
                "fog" => W::Fog,
                "cloudy" => W::Overcast,
                "partlycloudy" => W::PartlyCloudy,
                "fair" => W::MostlyClear,
                "clearsky" => W::Clear,
                _ => return None,
            }
        })
    }

    /// Parses a MET Norway Locationforecast `/complete` response as of `now`.
    ///
    /// MET publishes hourly steps for about 2.5 days and 6-hourly steps after that; every step describes the period
    /// that FOLLOWS it (`next_1_hours`, else `next_6_hours`). The daily rows are aggregated from those periods:
    /// * a period is credited to the local day that holds its midpoint, and its precipitation amount is added in full
    ///   (a 6-hour amount is 6 hours of rain, not one hour's worth);
    /// * low / high use the hourly temperatures and, for 6-hour periods, MET's `air_temperature_min` / `_max`;
    /// * the day's condition is the most significant symbol of its periods;
    /// * probability of precipitation, gusts and UV exist only where MET publishes them (elsewhere they are `None`),
    ///   and wind / UV maxima are given only for days made entirely of hourly steps;
    /// * MET has no data for hours already past, so today's row is flagged `partial` unless it starts at local midnight.
    fn parse_met_norway(root: &Value, orig: &Location, now: i64, fetched: i64) -> Result<WeatherBundle, String> {
        struct Day {
            date: String,
            start: i64,
            secs: i64,
            lo: f64,
            hi: f64,
            cond: Option<WeatherCondition>,
            psum: Option<f64>,
            pmax: Option<f64>,
            wmax: Option<f64>,
            uvmax: Option<f64>,
            uv_n: i64,
            hourly_only: bool,
        }
        let ts_arr = ja(root, "/properties/timeseries");
        if ts_arr.is_empty() {
            return Err("empty met.no timeseries".into());
        }
        let loc = orig.clone();
        let omax = |a: Option<f64>, b: Option<f64>| a.zip(b).map(|(x, y)| x.max(y)).or(a).or(b);
        let (mut hourly, mut by_day): (Vec<HourlyForecast>, Vec<Day>) = (Vec::with_capacity(48), Vec::new());
        for entry in ts_arr {
            let (Some(t), Some(det)) = (rfc3339(entry, "/time"), entry.pointer("/data/instant/details")) else { continue };
            let Some(tc) = fin(det, "/air_temperature") else { continue };
            let Some((per, len)) = entry
                .pointer("/data/next_1_hours")
                .map(|p| (p, 3600_i64))
                .or_else(|| entry.pointer("/data/next_6_hours").map(|p| (p, 21_600)))
            else {
                continue;
            };
            let cond = js(per, "/summary/symbol_code").and_then(Self::met_symbol_condition);
            let pmm = fin(per, "/details/precipitation_amount").filter(|x| *x >= 0.0);
            let pprob = fin(per, "/details/probability_of_precipitation").filter(|p| (0.0..=100.0).contains(p));
            let (ws, wd, uv) = (fin(det, "/wind_speed"), fin(det, "/wind_from_direction"), fin(det, "/ultraviolet_index_clear_sky"));
            if len == 3600
                && t >= now - 3600
                && hourly.len() < 48
                && let (Some(condition), Some(precip_mm), Some(wind_speed_mps), Some(wind_dir_deg)) = (cond, pmm, ws, wd)
            {
                hourly.push(HourlyForecast {
                    timestamp_unix: t,
                    temp_c: tc,
                    condition,
                    is_day: is_daytime_at(loc.lat, loc.lon, t),
                    precip_prob_pct: pprob,
                    precip_mm,
                    wind_speed_mps,
                    wind_dir_deg,
                    pressure_hpa: fin(det, "/air_pressure_at_sea_level"),
                    uv_index: uv,
                });
            }
            let date = loc.local_date(t + len / 2 - 1).format("%Y-%m-%d").to_string();
            let (lo, hi) = if len == 3600 {
                (tc, tc)
            } else {
                (fin(per, "/details/air_temperature_min").unwrap_or(tc), fin(per, "/details/air_temperature_max").unwrap_or(tc))
            };
            let slot = match by_day.iter().position(|d| d.date == date) {
                Some(i) => &mut by_day[i],
                None if by_day.len() < 10 => {
                    let (psum, hourly_only) = (Some(0.0), true);
                    by_day.push(Day {
                        date,
                        start: t,
                        secs: 0,
                        lo,
                        hi,
                        cond: None,
                        psum,
                        pmax: None,
                        wmax: None,
                        uvmax: None,
                        uv_n: 0,
                        hourly_only,
                    });
                    let last = by_day.len() - 1;
                    &mut by_day[last]
                }
                None => continue,
            };
            (slot.lo, slot.hi, slot.secs, slot.hourly_only) =
                (slot.lo.min(lo), slot.hi.max(hi), slot.secs + len, slot.hourly_only && len == 3600);
            slot.cond = slot.cond.zip(cond).map(|(a, c)| a.most_significant(c)).or(slot.cond).or(cond);
            // A period without an amount leaves the day's total unknown rather than silently too low.
            slot.psum = slot.psum.zip(pmm).map(|(a, x)| a + x);
            (slot.pmax, slot.wmax, slot.uvmax, slot.uv_n) =
                (omax(slot.pmax, pprob), omax(slot.wmax, ws), omax(slot.uvmax, uv), slot.uv_n + uv.is_some() as i64);
        }
        // "Current" is the forecast step nearest to the present hour (never an older step relabelled as now).
        let (cur_t, first) = ts_arr
            .iter()
            .filter_map(|e| Some((rfc3339(e, "/time")?, e)))
            .min_by_key(|(t, _)| (t - now).abs())
            .filter(|(t, _)| (t - now).abs() <= 3600)
            .ok_or("met.no has no forecast step for the current hour")?;
        let fdet = first.pointer("/data/instant/details").ok_or("missing met.no instant details")?;
        let tc = fin(fdet, "/air_temperature").ok_or("missing met.no air_temperature")?;
        let rh = fin(fdet, "/relative_humidity").ok_or("missing met.no relative_humidity")?;
        let ws = fin(fdet, "/wind_speed").ok_or("missing met.no wind_speed")?;
        let condition = first
            .pointer("/data/next_1_hours/summary/symbol_code")
            .or_else(|| first.pointer("/data/next_6_hours/summary/symbol_code"))
            .and_then(Value::as_str)
            .and_then(Self::met_symbol_condition)
            .ok_or("missing met.no weather symbol")?;
        let (feels, feels_like_computed) =
            fin(fdet, "/apparent_air_temperature").map_or_else(|| (apparent_temperature_c(tc, rh, ws), true), |f| (f, false));
        let current = CurrentWeather {
            temp_c: tc,
            feels_like_c: Some(feels),
            feels_like_computed,
            dew_point_c: fin(fdet, "/dew_point_temperature").unwrap_or_else(|| dew_point_from_rh(tc, rh)),
            humidity_pct: rh,
            wind_speed_mps: ws,
            wind_gust_mps: fin(fdet, "/wind_speed_of_gust"),
            wind_dir_deg: fin(fdet, "/wind_from_direction"),
            pressure_hpa: fin(fdet, "/air_pressure_at_sea_level"),
            visibility_m: None,
            visibility_at_least: false,
            uv_index: fin(fdet, "/ultraviolet_index_clear_sky"),
            precip_mm: fin(first, "/data/next_1_hours/details/precipitation_amount"),
            condition,
            is_day: is_daytime_at(loc.lat, loc.lon, now),
            updated_unix: cur_t,
        };
        let daily: Vec<DailyForecast> = by_day
            .into_iter()
            .enumerate()
            // Only today may be incomplete; a trailing day the series does not reach the end of is dropped.
            .take_while(|(i, d)| *i == 0 || d.secs >= 18 * 3600)
            .filter_map(|(i, d)| {
                let sun = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok().map(|nd| compute_sun_times(loc.lat, loc.lon, nd));
                // Today is partial when its first period starts an hour or more into the local day.
                let partial = i == 0 && loc.local_date(d.start - 3600).format("%Y-%m-%d").to_string() == d.date;
                Some(DailyForecast {
                    sunrise_unix: sun.and_then(|s| s.sunrise_unix),
                    sunset_unix: sun.and_then(|s| s.sunset_unix),
                    temp_min_c: d.lo,
                    temp_max_c: d.hi,
                    condition: d.cond?,
                    precip_prob_pct: d.pmax,
                    precip_sum_mm: d.psum,
                    wind_max_mps: d.wmax.filter(|_| d.hourly_only),
                    uv_max: d.uvmax.filter(|_| d.hourly_only && d.uv_n * 3600 == d.secs),
                    date: d.date,
                    partial,
                })
            })
            .collect();
        Ok(WeatherBundle {
            location: loc,
            current,
            minutely: Vec::new(),
            hourly,
            daily,
            air_quality: None,
            alerts: Vec::new(),
            source_name: "MET Norway Locationforecast".into(),
            fetched_unix: fetched,
            full_fetched_unix: None,
            alerts_fetched_unix: None,
            lean: false,
            elevation_m: fin(root, "/geometry/coordinates/2"),
            uv_clear_sky: true,
            schema: BUNDLE_SCHEMA,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::default_locations;

    #[test]
    fn live_fetch_weather_and_radar_manifest() {
        let c = WeatherClient::new();
        let (loc, _precise) = c.detect_current_location().expect("live detect_current_location");
        assert!(!loc.name.is_empty() && loc.lat.is_finite() && loc.lon.is_finite());
        let b = c.fetch_weather(&default_locations()[0], ProviderChoice::Auto).expect("live weather");
        assert!((-60.0..=60.0).contains(&b.current.temp_c) && b.hourly.len() >= 24 && b.daily.len() >= 7 && b.air_quality.is_some());
        assert!(
            (b.source_name.contains("Open-Meteo") || b.source_name.contains("MET Norway"))
                && b.hourly.iter().all(|h| h.precip_prob_pct.is_none_or(|p| (0.0..=100.0).contains(&p)))
        );
        // A full fetch is stamped as such and carries nothing defaulted.
        assert!(!b.lean && b.full_fetched_unix == Some(b.fetched_unix) && b.schema == BUNDLE_SCHEMA && b.alerts_fetched_unix.is_some());
        assert!(
            b.current.pressure_hpa.is_none_or(|p| (870.0..=1085.0).contains(&p))
                && b.current.uv_index.is_none_or(|u| (0.0..=20.0).contains(&u))
        );
        assert!(c.search_locations("Seattle").iter().any(|r| r.name.contains("Seattle")));
        let (m, now) = (c.fetch_rainviewer_manifest().expect("manifest"), Utc::now().timestamp());
        let fut: Vec<_> = m.frames.iter().filter(|f| f.is_nowcast && f.time_unix > now).collect();
        assert!(
            m.frames.iter().filter(|f| !f.is_nowcast).count() >= 6
                && fut.len() >= 8
                && fut.iter().map(|f| f.time_unix).max().unwrap_or(0) >= now + 6 * 3600
        );
    }

    #[test]
    fn accuracy_helpers_and_wifi_parsing() {
        assert!(
            (nws_wind_to_mps(10.0, "wmoUnit:kt").unwrap() - 5.14444).abs() < 1e-3
                && (nws_wind_to_mps(36.0, "wmoUnit:km_h-1").unwrap() - 10.0).abs() < 1e-3
                && nws_wind_to_mps(4.0, "wmoUnit:m_s-1") == Some(4.0)
                && nws_wind_to_mps(4.0, "").is_none()
        );
        assert_eq!(nws_desc_to_condition("Light Rain and Fog"), Some(WeatherCondition::Rain));
        // "Freezing Fog" is fog (it used to map to Freezing Rain); mist / haze / smoke are not fog.
        assert_eq!(nws_desc_to_condition("Freezing Fog"), Some(WeatherCondition::Fog));
        assert_eq!(nws_desc_to_condition("Light Freezing Rain"), Some(WeatherCondition::FreezingRain));
        assert_eq!(nws_desc_to_condition("Freezing Drizzle and Fog"), Some(WeatherCondition::FreezingRain));
        assert_eq!((nws_desc_to_condition("Haze"), nws_desc_to_condition("Overcast and Mist")), (None, Some(WeatherCondition::Overcast)));
        let aps = parse_nmcli_wifi_aps(
            "4E\\:D7\\:4A\\:7D\\:84\\:71:90:5180 MHz\n14\\:22\\:3B\\:4F\\:C0\\:7A:60:2462 MHz\n14\\:22\\:3B\\:4F\\:C0\\:7A:80:2462 MHz\nAC\\:39\\:3D\\:19\\:CE\\:03:48:2437 MHz\n00\\:00\\:00\\:00\\:00\\:00:99:2412 MHz\n01\\:00\\:5E\\:00\\:00\\:01:80:2412 MHz\nbad:line\n",
        );
        assert_eq!((aps.len(), aps[0].0.as_str(), aps[0].1, aps[2].3), (3, "14:22:3b:4f:c0:7a", -60, true));
        // Cross-platform Windows netsh & macOS airport Wi-Fi parsers.
        let win_aps = parse_netsh_wifi_aps(
            "SSID 1 : HomeNet\n    BSSID 1                 : 14:22:3b:4f:c0:7a\n         Signal             : 82%\n         Channel            : 36\n    BSSID 2                 : 4e:d7:4a:7d:84:71\n         Signal             : 90%\n         Channel            : 6\n",
        );
        assert_eq!((win_aps.len(), win_aps[0].0.as_str(), win_aps[0].1, win_aps[0].2), (2, "14:22:3b:4f:c0:7a", -59, Some(5180)));
        let mac_aps = parse_macos_airport_aps(
            "                            SSID BSSID             RSSI CHANNEL HT CC SECURITY\n                        CoffeeWiFi 14:22:3b:4f:c0:7a -58  149,+1  Y  US WPA2(PSK)\n                          GuestNet ac:39:3d:19:ce:03 -72  11      Y  US WPA2\n",
        );
        assert_eq!((mac_aps.len(), mac_aps[0].0.as_str(), mac_aps[0].1, mac_aps[0].2), (2, "14:22:3b:4f:c0:7a", -58, Some(5745)));
        assert_eq!(
            parse_beacondb_response(&serde_json::json!({"location":{"lat":37.87,"lng":-122.26},"accuracy":49.0})),
            Some((37.87, -122.26))
        );
        assert_eq!(parse_beacondb_response(&serde_json::json!({"location":{"lat":37.89,"lng":-122.26},"accuracy":12000.0})), None);
        // Medoid rejects outliers in both IP consensus and moved-router Wi-Fi clustering.
        let ip_med = geo_medoid_idx(&[(37.7749, -122.4194), (37.8919, -122.2636), (37.8922, -122.2729), (38.4403, -122.7143)]).unwrap();
        assert!(ip_med == 1 || ip_med == 2);
        let (cla, clo) =
            geo_cluster_centroid(&[(37.8779, -122.2671, 5.0), (37.8778, -122.2672, 4.0), (47.6062, -122.3321, 9.0)], 0.008).unwrap();
        assert!((cla - 37.8779).abs() < 0.001 && (clo + 122.2671).abs() < 0.001);
        assert!(encode_apple_wloc(&aps).len() > 44);
        // Worldwide NOAA Aviation Weather Center METAR parser + Magnus RH + ICAO weather/cloud mapping.
        let now = 1_760_000_000_i64;
        let metars = serde_json::json!([
            {"icaoId": "KSFO", "obsTime": now - 600, "lat": 37.62, "lon": -122.38, "temp": 14.0, "dewp": 10.0, "wdir": 280, "wspd": 12, "wgst": 18, "altim": 1018.4, "visib": "10+", "clouds": [{"cover": "SCT"}]},
            {"icaoId": "KOAK", "obsTime": now - 300, "lat": 37.72, "lon": -122.22, "temp": 13.5, "dewp": 11.0, "wdir": "VRB", "wspd": 6, "slp": 1019.2, "visib": 8.0, "wxString": "-RA", "clouds": [{"cover": "BKN"}]}
        ]);
        let cands = WeatherClient::parse_metar_array(&metars, 37.80, -122.25, now);
        assert_eq!(
            cands.iter().filter_map(|o| js(o, "/properties/sourceTag")).collect::<Vec<_>>(),
            ["NOAA METAR KOAK obs", "NOAA METAR KSFO obs"]
        );
        let obs = cands[0].clone();
        assert!((jf(&obs, "/properties/temperature/value").unwrap() - 13.5).abs() < 1e-6);
        assert!((jf(&obs, "/properties/windSpeed/value").unwrap() - 6.0 * 0.514_444).abs() < 1e-4);
        assert!(jf(&obs, "/properties/windDirection/value").is_none());
        assert!((jf(&obs, "/properties/seaLevelPressure/value").unwrap() - 101_920.0).abs() < 1e-3);
        assert!((jf(&obs, "/properties/visibility/value").unwrap() - 8.0 * 1609.344).abs() < 1.0);
        let rh = jf(&obs, "/properties/relativeHumidity/value").unwrap();
        assert!((83.0..=87.0).contains(&rh), "unexpected Magnus RH {rh}");
        assert_eq!(js(&obs, "/properties/textDescription"), Some("Rain"));
        assert_eq!(metar_wx_desc(Some("+TSRA"), None, None), "Thunderstorm");
        assert_eq!(metar_wx_desc(None, None, Some(&vec![serde_json::json!({"cover": "OVC"})])), "Overcast");
    }

    const NOW: i64 = 1_795_512_900; // 2026-11-24 09:35 UTC

    fn brk() -> Location {
        Location::new("Berkeley", "CA", "United States", "US", 37.87, -122.27, "America/Los_Angeles")
    }

    #[test]
    fn metar_clear_sky_and_at_least_visibility() {
        // Shapes copied from live aviationweather.gov responses: a clear sky is `clouds: []` plus a top-level `cover`.
        let metars = serde_json::json!([
            {"icaoId": "KHWD", "obsTime": NOW - 300, "lat": 37.6589, "lon": -122.1212, "elev": 9, "temp": 8.3, "dewp": 6.7, "wdir": 60, "wspd": 6, "wgst": null, "visib": "10+", "altim": 1022.1, "wxString": null, "cover": "CLR", "clouds": []},
            {"icaoId": "YSSY", "obsTime": NOW - 300, "lat": 37.70, "lon": -122.20, "elev": 3, "temp": 17, "dewp": 16, "wdir": 170, "wspd": 21, "wgst": null, "visib": "6+", "altim": 1022, "wxString": null, "cover": "CAVOK", "clouds": []},
            {"icaoId": "KCCR", "obsTime": NOW - 300, "lat": 37.9916, "lon": -122.0527, "elev": 6, "temp": 5.6, "dewp": 5.6, "wdir": 0, "wspd": 0, "wgst": null, "visib": 5, "altim": 1022.8, "wxString": "BR", "cover": "OVC", "clouds": [{"cover": "OVC", "base": 400}]},
            {"icaoId": "KHAF", "obsTime": NOW - 300, "lat": 37.70, "lon": -122.50, "elev": 11, "temp": 9, "dewp": 8, "wdir": 0, "wspd": 0, "wgst": null, "visib": null, "altim": 1021.4, "wxString": null, "cover": null, "clouds": null},
            {"icaoId": "KSTS", "obsTime": NOW - 300, "lat": 38.51, "lon": -122.81, "elev": 38, "temp": 4, "dewp": 3, "wdir": 0, "wspd": 0, "visib": "10+", "altim": 1022.0, "cover": "CLR", "clouds": []},
            {"icaoId": "KOLD", "obsTime": NOW - 9000, "lat": 37.87, "lon": -122.27, "elev": 3, "temp": 30, "dewp": 3, "wdir": 0, "wspd": 0, "visib": 1, "altim": 1022.0, "cover": "OVC", "clouds": []}
        ]);
        let cands = WeatherClient::parse_metar_array(&metars, 37.87, -122.27, NOW);
        // Nearest first; KSTS (~80 km away) and the 2.5-hour-old report are not candidates at all.
        let by_id = |id: &str| {
            cands.iter().find(|o| js(o, "/properties/sourceTag").is_some_and(|t| t == format!("NOAA METAR {id} obs"))).expect("candidate")
        };
        assert_eq!(cands.len(), 4);
        assert_eq!(js(&cands[0], "/properties/sourceTag"), Some("NOAA METAR YSSY obs"));
        // "10+" / "6+" statute miles are lower bounds, flagged as such — never an exact 10 or 6 miles.
        let (clr, cavok, mist, bare) = (by_id("KHWD"), by_id("YSSY"), by_id("KCCR"), by_id("KHAF"));
        assert_eq!(
            (clr.pointer("/properties/visibilityAtLeast"), cavok.pointer("/properties/visibilityAtLeast")),
            (Some(&Value::Bool(true)), Some(&Value::Bool(true)))
        );
        assert!((jf(cavok, "/properties/visibility/value").unwrap() - 6.0 * 1609.344).abs() < 0.01);
        assert_eq!(mist.pointer("/properties/visibilityAtLeast"), Some(&Value::Bool(false)));
        // CLR / CAVOK give "Clear"; mist under an overcast is "Overcast", not fog; no sky report gives no condition.
        assert_eq!((js(clr, "/properties/textDescription"), js(cavok, "/properties/textDescription")), (Some("Clear"), Some("Clear")));
        assert_eq!((js(mist, "/properties/textDescription"), js(bare, "/properties/textDescription")), (Some("Overcast"), Some("")));
        assert!(jf(bare, "/properties/visibility/value").is_none() && jf(clr, "/properties/windGust/value").is_none());
        assert_eq!(metar_wx_desc(Some("FZFG"), Some("OVX"), None), "Fog");
        // Live KOAK: "10SM MIFG FEW007 ... SCT250" is shallow fog under scattered cloud, not fog; vicinity showers are not rain here.
        assert_eq!(
            (metar_wx_desc(Some("MIFG"), Some("SCT"), None), metar_wx_desc(Some("VCSH"), Some("BKN"), None)),
            ("Partly Cloudy", "Mostly Cloudy")
        );
        assert_eq!(metar_wx_desc(Some("-RA VCTS"), Some("OVC"), None), "Rain");
        assert_eq!((metar_wx_desc(None, Some("CAVOK"), None), metar_wx_desc(None, None, None)), ("Clear", ""));

        // Applied to a model bundle that says "Overcast": the clear-sky observation overrides it, and the visibility
        // is carried as a lower bound that the UI renders as "10+ km" / "6+ mi".
        let mut b = crate::model::test_bundle(NOW);
        b.location = brk();
        assert!(WeatherClient::apply_nws_obs(&mut b, cavok, NOW));
        assert_eq!((b.current.condition, b.current.visibility_at_least), (WeatherCondition::Clear, true));
        let mut s = crate::model::Settings::default();
        assert_eq!(s.format_visibility(b.current.visibility_m.unwrap(), b.current.visibility_at_least), "10+ km");
        s.wind_unit = crate::model::WindUnit::Mph;
        assert_eq!(s.format_visibility(b.current.visibility_m.unwrap(), b.current.visibility_at_least), "6+ mi");
        // A measured value stays exact, and a station that reports no visibility leaves the model's value alone.
        let mut b2 = crate::model::test_bundle(NOW);
        b2.location = brk();
        assert!(WeatherClient::apply_nws_obs(&mut b2, mist, NOW));
        assert_eq!((b2.current.visibility_at_least, b2.current.condition), (false, WeatherCondition::Overcast));
        assert!((b2.current.visibility_m.unwrap() - 5.0 * 1609.344).abs() < 0.01);
        // Calm wind: speed 0, no direction, no gust.
        assert_eq!((b2.current.wind_speed_mps, b2.current.wind_dir_deg, b2.current.wind_gust_mps), (0.0, None, None));
    }

    #[test]
    fn observation_never_mixes_with_model_values() {
        let mk = |id: &str, lat: f64, elev: f64, extra: Value| {
            let mut m = serde_json::json!({"icaoId": id, "obsTime": NOW - 600, "lat": lat, "lon": -122.25, "elev": elev, "temp": 5.0, "dewp": 1.0, "wdir": 250, "wspd": 14, "wgst": null, "visib": 9, "altim": 1016.0, "cover": "FEW", "clouds": [{"cover": "FEW"}]});
            m.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            m
        };
        let model = || {
            let mut b = crate::model::test_bundle(NOW);
            b.location = brk();
            // Model: wind 3 m/s gusting 6.7 m/s (24 km/h), 15 °C, forecast high 16 / low 9, first hourly row 14 °C.
            b.current.wind_gust_mps = Some(6.7);
            b.hourly = vec![crate::model::test_hour(NOW - 300, Some(0.0), Some(1015.0))];
            b.daily = vec![DailyForecast {
                date: "2026-11-24".into(),
                temp_min_c: 9.0,
                temp_max_c: 16.0,
                condition: WeatherCondition::Overcast,
                precip_prob_pct: None,
                precip_sum_mm: Some(0.0),
                wind_max_mps: None,
                uv_max: None,
                sunrise_unix: None,
                sunset_unix: None,
                partial: false,
            }];
            b
        };
        // 1. Wind 14 kt (26 km/h) with NO gust in the observation: the model's 24 km/h gust must not survive beside it.
        let no_gust =
            WeatherClient::parse_metar_array(&serde_json::json!([mk("KOAK", 37.80, 3.0, serde_json::json!({}))]), 37.87, -122.27, NOW);
        let mut b = model();
        assert!(WeatherClient::apply_nws_obs(&mut b, &no_gust[0], NOW));
        assert!((b.current.wind_speed_mps * 3.6 - 25.9).abs() < 0.1 && b.current.wind_gust_mps.is_none());
        assert_eq!(b.current.wind_dir_deg, Some(250.0));
        // Temperature, dew point and humidity come from the observation as one set; feels-like is computed from that
        // same observation (wind chill at 5 °C / 26 km/h) and flagged as computed.
        assert_eq!((b.current.temp_c, b.current.dew_point_c, b.current.feels_like_computed), (5.0, 1.0, true));
        assert!((b.current.humidity_pct - 75.3).abs() < 1.0 && (b.current.feels_like_c.unwrap() - 0.42).abs() < 0.2);
        // The forecast is left exactly as the provider gave it, even though 5 °C is below the forecast low.
        assert_eq!((b.daily[0].temp_min_c, b.daily[0].temp_max_c, b.hourly[0].temp_c), (9.0, 16.0, 14.0));
        // 2. A gust the station does report is used.
        let gusty = WeatherClient::parse_metar_array(
            &serde_json::json!([mk("KOAK", 37.80, 3.0, serde_json::json!({"wgst": 25}))]),
            37.87,
            -122.27,
            NOW,
        );
        let mut b = model();
        assert!(WeatherClient::apply_nws_obs(&mut b, &gusty[0], NOW));
        assert!((b.current.wind_gust_mps.unwrap() - 25.0 * 0.514_444).abs() < 1e-6);
        // 3. Variable wind ("VRB") has a speed but no direction.
        let vrb = WeatherClient::parse_metar_array(
            &serde_json::json!([mk("KOAK", 37.80, 3.0, serde_json::json!({"wdir": "VRB", "wspd": 3}))]),
            37.87,
            -122.27,
            NOW,
        );
        let mut b = model();
        assert!(WeatherClient::apply_nws_obs(&mut b, &vrb[0], NOW));
        assert!(b.current.wind_dir_deg.is_none() && b.current.wind_speed_mps > 1.5);
        // 4. Station selection: a mountain-top station 900 m above the 60 m forecast point is skipped (nothing changes),
        //    and the caller moves on to the next-nearest candidate.
        let two = WeatherClient::parse_metar_array(
            &serde_json::json!([
                mk("KHIL", 37.86, 960.0, serde_json::json!({"temp": -2.0})),
                mk("KOAK", 37.72, 3.0, serde_json::json!({}))
            ]),
            37.87,
            -122.27,
            NOW,
        );
        assert_eq!(two.len(), 2);
        let mut b = model();
        assert!(!WeatherClient::apply_nws_obs(&mut b, &two[0], NOW) && b.current.temp_c == 15.0 && b.current.wind_gust_mps == Some(6.7));
        assert!(WeatherClient::apply_nws_obs(&mut b, &two[1], NOW) && b.current.temp_c == 5.0);
        // Without a known forecast-point elevation the check cannot be made and the station is accepted.
        let mut b = model();
        b.elevation_m = None;
        assert!(WeatherClient::apply_nws_obs(&mut b, &two[0], NOW));
        // A stale observation is never applied.
        let mut b = model();
        assert!(!WeatherClient::apply_nws_obs(&mut b, &two[1], NOW + 3 * 3600));
        // 5. api.weather.gov shape: its own wind chill is used as reported, and exactly 10 SM is a lower bound.
        let nws = serde_json::json!({"geometry": {"type": "Point", "coordinates": [-122.22, 37.73]}, "properties": {
            "timestamp": "2026-11-24T09:25:00+00:00", "elevation": {"unitCode": "wmoUnit:m", "value": 3}, "textDescription": "Mostly Clear",
            "temperature": {"value": 7}, "dewpoint": {"value": 6}, "relativeHumidity": {"value": 93.35},
            "windSpeed": {"unitCode": "wmoUnit:km_h-1", "value": 5.544}, "windGust": {"unitCode": "wmoUnit:km_h-1", "value": null},
            "windDirection": {"value": 80}, "windChill": {"value": 6.18}, "heatIndex": {"value": null},
            "visibility": {"value": 16093.44}, "barometricPressure": {"value": 102201.21}, "seaLevelPressure": {"value": null}}});
        let mut b = model();
        assert!(WeatherClient::apply_nws_obs(&mut b, &nws, NOW));
        assert_eq!((b.current.feels_like_c, b.current.feels_like_computed, b.current.visibility_at_least), (Some(6.18), false, true));
        assert!((b.current.pressure_hpa.unwrap() - 1022.01).abs() < 0.01 && b.current.wind_gust_mps.is_none());
        assert_eq!(b.current.condition, WeatherCondition::MostlyClear);
    }

    #[test]
    fn open_meteo_parse_never_defaults_missing_values() {
        // Shape of a live api.open-meteo.com response (trimmed): nulls stand for values the model does not provide.
        let t0 = 1_795_512_600_i64; // current.time
        let root = serde_json::json!({
            "latitude": 37.87, "longitude": -122.27, "elevation": 60.0, "utc_offset_seconds": -28800, "timezone": "America/Los_Angeles",
            "current": {"time": t0, "interval": 900, "temperature_2m": 8.5, "relative_humidity_2m": 81, "dew_point_2m": 5.4, "apparent_temperature": 5.7,
                "is_day": 0, "precipitation": 0.1, "weather_code": 51, "pressure_msl": null, "wind_speed_10m": 3.03, "wind_direction_10m": 82,
                "wind_gusts_10m": null, "visibility": null, "uv_index": 2.95},
            "minutely_15": {"time": [t0, t0 + 900, t0 + 1800, t0 + 2700, t0 + 3600, t0 + 4500, t0 + 5400, t0 + 6300],
                "precipitation": [0.1, 0.1, 0.1, 0.2, 0.2, null, 0.2, 0.0]},
            "hourly": {"time": [t0 - 1800, t0 + 1800, t0 + 5400, t0 + 9000],
                "temperature_2m": [8.6, 8.2, null, 7.9], "precipitation_probability": [57, null, 20, 10], "precipitation": [0.0, 0.3, 0.1, 0.0],
                "weather_code": [2, 61, 3, 0], "pressure_msl": [1020.8, null, 1020.5, 1020.1], "wind_speed_10m": [3.0, 2.5, 2.0, 1.5],
                "wind_direction_10m": [82, 90, 95, 100], "uv_index": [0.0, null, 0.2, 7.65]},
            "daily": {"time": [1_795_507_200_i64, 1_795_593_600_i64], "weather_code": [53, 3], "temperature_2m_max": [15.0, 14.0], "temperature_2m_min": [8.3, 7.1],
                "sunrise": [1_795_532_400_i64, 1_795_618_800_i64], "sunset": [1_795_568_400_i64, 1_795_654_800_i64], "uv_index_max": [3.0, null],
                "precipitation_sum": [4.2, null], "precipitation_probability_max": [100, null], "wind_speed_10m_max": [null, 6.1]}
        });
        let b = WeatherClient::parse_open_meteo_root(&root, &brk(), false, NOW).expect("bundle");
        // Missing current values are `None` — not 1013.25 hPa, not 0 gusts, not an invented visibility.
        assert_eq!((b.current.pressure_hpa, b.current.wind_gust_mps, b.current.visibility_m), (None, None, None));
        assert_eq!(
            (b.current.uv_index, b.current.precip_mm, b.current.feels_like_c, b.current.feels_like_computed),
            (Some(2.95), Some(0.1), Some(5.7), false)
        );
        assert_eq!((b.fetched_unix, b.full_fetched_unix, b.elevation_m, b.schema), (NOW, None, Some(60.0), BUNDLE_SCHEMA));
        // The hourly row without a temperature is dropped; missing probability / pressure / UV stay missing and real UV is kept.
        assert_eq!(b.hourly.iter().map(|h| h.timestamp_unix - t0).collect::<Vec<_>>(), [-1800, 1800, 9000]);
        assert_eq!(b.hourly.iter().map(|h| h.precip_prob_pct).collect::<Vec<_>>(), [Some(57.0), None, Some(10.0)]);
        assert_eq!(b.hourly.iter().map(|h| h.pressure_hpa).collect::<Vec<_>>(), [Some(1020.8), None, Some(1020.1)]);
        assert_eq!(b.hourly.iter().map(|h| h.uv_index).collect::<Vec<_>>(), [Some(0.0), None, Some(7.65)]);
        // Every real 15-minute sample is kept (as mm/h); the null one is skipped, nothing is resampled or extended.
        assert_eq!(b.minutely.len(), 7);
        assert_eq!(
            b.minutely.iter().map(|s| (s.timestamp_unix - t0, (s.precip_mm_hr * 10.0).round() / 10.0)).next_back(),
            Some((6300, 0.0))
        );
        assert!((b.minutely[3].precip_mm_hr - 0.8).abs() < 1e-9 && b.minutely[5].timestamp_unix == t0 + 5400);
        // Daily: today's precipitation is the provider's own sum; nulls stay missing; dates come from the response's offset.
        assert_eq!(b.daily.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(), ["2026-11-24", "2026-11-25"]);
        assert_eq!(
            (b.daily[0].precip_sum_mm, b.daily[0].precip_prob_pct, b.daily[0].uv_max, b.daily[0].wind_max_mps),
            (Some(4.2), Some(100.0), Some(3.0), None)
        );
        assert_eq!(
            (b.daily[1].precip_sum_mm, b.daily[1].precip_prob_pct, b.daily[1].uv_max, b.daily[1].wind_max_mps),
            (None, None, None, Some(6.1))
        );
        // Lean sidebar shape (no hourly block): still real gust / visibility / UV from `current`, nothing grafted.
        let mut lean = root.clone();
        let obj = lean.as_object_mut().unwrap();
        obj.remove("hourly");
        obj.remove("minutely_15");
        obj["current"]["wind_gusts_10m"] = serde_json::json!(3.5);
        obj["current"]["visibility"] = serde_json::json!(16900.0);
        let l = WeatherClient::parse_open_meteo_root(&lean, &brk(), false, NOW).expect("lean bundle");
        assert!(l.hourly.is_empty() && l.minutely.is_empty());
        assert_eq!((l.current.wind_gust_mps, l.current.visibility_m, l.current.uv_index), (Some(3.5), Some(16900.0), Some(2.95)));
        // A response without a core current value is rejected rather than patched up.
        let mut bad = root.clone();
        bad["current"]["wind_speed_10m"] = Value::Null;
        assert!(WeatherClient::parse_open_meteo_root(&bad, &brk(), false, NOW).is_err());
        bad = root.clone();
        bad["current"]["weather_code"] = serde_json::json!(4);
        assert!(WeatherClient::parse_open_meteo_root(&bad, &brk(), false, NOW).is_err());
    }

    #[test]
    fn air_quality_is_the_upstream_us_aqi_unmodified() {
        // Live Berkeley sample: us_aqi 73 with PM2.5 30.6 µg/m³ (whose instantaneous EPA sub-index would be 91).
        let v = serde_json::json!({"timezone": "America/Los_Angeles", "current": {"time": "2026-11-24T01:00", "interval": 3600, "us_aqi": 73, "pm2_5": 30.6}});
        let aq = WeatherClient::parse_air_quality(&v).expect("aqi");
        assert_eq!((aq.us_aqi, aq.pm2_5), (73, Some(30.6)));
        let no_pm = WeatherClient::parse_air_quality(&serde_json::json!({"current": {"us_aqi": 59, "pm2_5": null}})).expect("aqi");
        assert_eq!((no_pm.us_aqi, no_pm.pm2_5), (59, None));
        assert!(WeatherClient::parse_air_quality(&serde_json::json!({"current": {"us_aqi": null, "pm2_5": 12.0}})).is_none());
    }

    #[test]
    fn eccc_alerts_skip_ended_and_duplicates_before_limiting() {
        let feature = |code: &str, kind: &str, name: &str, text: &str, zone: &str, status: &str, expires: &str| {
            serde_json::json!({"type": "Feature", "geometry": {"type": "Polygon", "coordinates": []}, "properties": {
                "alert_code": code, "alert_type": kind, "alert_name_en": name, "alert_short_name_en": name, "alert_text_en": text,
                "feature_name_en": zone, "province": "ON", "status_en": status, "publication_datetime": "2026-11-24T06:54:25.868Z",
                "expiration_datetime": expires}})
        };
        let (later, past) = ("2026-11-24T18:03:25.868Z", "2026-11-24T08:00:00.000Z");
        let sps = "Winter weather travel statement in effect.";
        // Live shape: four "ended" statements for neighbouring zones come first — with `limit=4` applied before filtering
        // they used to be the whole result. The same statement then appears once per zone, plus one real warning.
        let val = serde_json::json!({"type": "FeatureCollection", "numberMatched": 8, "features": [
            feature("SPS", "statement", "special weather statement", sps, "London - Parkhill", "ended", later),
            feature("SPS", "statement", "special weather statement", sps, "Stratford - Mitchell", "ended", later),
            feature("SPS", "statement", "special weather statement", sps, "Strathroy - Komoka", "ended", later),
            feature("SPS", "statement", "special weather statement", sps, "Sarnia - Petrolia", "Ended", later),
            feature("SPS", "statement", "special weather statement", sps, "Goderich - Bluewater", "continued", later),
            feature("SPS", "statement", "special weather statement", sps, "Watford - Pinery Park", "continued", later),
            feature("SSW", "warning", "snow squall warning", "Snow squalls continue.", "Goderich - Bluewater", "issued", later),
            feature("FRW", "warning", "freezing rain warning", "Freezing rain has ended.", "Goderich - Bluewater", "issued", past),
        ]});
        let alerts = parse_eccc_alerts(&val, NOW).expect("feature collection");
        assert_eq!(
            alerts.iter().map(|a| a.headline.as_str()).collect::<Vec<_>>(),
            ["Snow squall warning — Goderich - Bluewater", "Special weather statement — Goderich - Bluewater"]
        );
        assert_eq!(alerts.iter().map(|a| a.severity).collect::<Vec<_>>(), [AlertSeverity::Severe, AlertSeverity::Minor]);
        // An error document is not "no alerts".
        assert!(parse_eccc_alerts(&serde_json::json!({"code": "InvalidParameterValue", "description": "bbox"}), NOW).is_none());
        assert_eq!(parse_eccc_alerts(&serde_json::json!({"type": "FeatureCollection", "features": []}), NOW).map(|a| a.len()), Some(0));
        // CAP-style feeds keep working through the shared parser.
        let nws = serde_json::json!({"features": [{"properties": {"event": "Wind Advisory", "severity": "Moderate", "headline": "Wind Advisory until 6 PM", "description": " Gusts to 50 mph. "}}]});
        let parsed = parse_feed_alerts(&nws, "/features", true, [&["/event"], &["/headline"], &["/description"]]).expect("alerts");
        assert_eq!((parsed.len(), parsed[0].severity, parsed[0].description.as_str()), (1, AlertSeverity::Moderate, "Gusts to 50 mph."));
        assert!(
            parse_feed_alerts(&serde_json::json!({"title": "error"}), "/features", true, [&["/event"], &["/headline"], &["/description"]])
                .is_none()
        );
    }

    #[test]
    fn failed_alert_request_is_not_cached_as_no_alerts() {
        let alert = WeatherAlert {
            event: "Snow squall warning".into(),
            headline: "Snow squall warning".into(),
            description: String::new(),
            severity: AlertSeverity::Severe,
        };
        let cached: AlertsEntry = (vec![alert.clone()], "ECCC alerts".into(), Some("\"etag-1\"".into()), NOW - 900);
        // Timeout / HTTP error with alerts already known: they are returned with the time they were fetched and the
        // cache is left alone (so the next refresh asks again instead of trusting an empty result for 5 minutes).
        let (out, entry) = settle_alerts(Err("timeout".into()), Some(cached.clone()), "ECCC alerts", NOW);
        let (alerts, src, ts) = out.expect("known alerts are kept");
        assert_eq!((alerts.len(), src.as_str(), ts), (1, "ECCC alerts", NOW - 900));
        assert!(entry.is_none());
        // Failure with nothing known: the result is "unknown" (`None`), which leaves `alerts_fetched_unix` unset so
        // `WeatherBundle::keep_known_alerts` keeps whatever the app already shows.
        let (out, entry) = settle_alerts(Err("HTTP 503".into()), None, "ECCC alerts", NOW);
        assert!(out.is_none() && entry.is_none());
        // A real "no alerts" answer does clear them, and is cached.
        let (out, entry) = settle_alerts(Ok(Some((Vec::new(), None))), Some(cached.clone()), "ECCC alerts", NOW);
        assert_eq!(out.map(|(a, _, ts)| (a.len(), ts)), Some((0, NOW)));
        assert_eq!(entry.map(|(a, _, _, ts)| (a.len(), ts)), Some((0, NOW)));
        // HTTP 304: the cached alerts are confirmed as current.
        let (out, entry) = settle_alerts(Ok(None), Some(cached), "ECCC alerts", NOW);
        assert_eq!(out.map(|(a, _, ts)| (a.len(), ts)), Some((1, NOW)));
        assert_eq!(entry.map(|(_, _, etag, ts)| (etag, ts)), Some((Some("\"etag-1\"".to_string()), NOW)));
    }

    /// MET Norway `/complete` timeseries shaped like the live API: hourly steps, then 6-hourly steps, then a bare instant.
    fn metno_fixture() -> Value {
        let iso = |t: i64| DateTime::from_timestamp(t, 0).unwrap().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let start = 1_795_510_800_i64; // 2026-11-24 09:00 UTC
        let mut ts = Vec::new();
        // 39 hourly steps: 2026-11-24 09:00 .. 2026-11-25 23:00. Temperature follows the hour of day (peak 15:00).
        for i in 0..39_i64 {
            let t = start + i * 3600;
            let hod = (t / 3600) % 24;
            let mut inst = serde_json::json!({"air_pressure_at_sea_level": 1021.8, "air_temperature": 10.0 + 8.0 - (hod - 15).abs() as f64 * 0.5,
                "apparent_air_temperature": 9.0, "dew_point_temperature": 7.0, "relative_humidity": 77.1,
                "ultraviolet_index_clear_sky": if (9..=15).contains(&hod) { 4.5 } else { 0.0 }, "wind_from_direction": 160.0, "wind_speed": 3.0 + hod as f64 * 0.1});
            let mut n1 = serde_json::json!({"summary": {"symbol_code": if hod == 20 { "heavyrain" } else { "lightrain" }}, "details": {"precipitation_amount": 0.2}});
            if i == 0 {
                // Only the Nordic model publishes gusts and a probability of precipitation.
                inst["wind_speed_of_gust"] = serde_json::json!(5.0);
                n1["details"]["probability_of_precipitation"] = serde_json::json!(40.0);
            }
            ts.push(serde_json::json!({"time": iso(t), "data": {"instant": {"details": inst}, "next_1_hours": n1}}));
        }
        // 8 six-hourly steps: 2026-11-26 00:00 .. 2026-11-27 18:00. Instants are a flat 12 °C; the real range is in next_6_hours.
        for j in 0..8_i64 {
            let t = start + 39 * 3600 + j * 21_600;
            let sym = ["cloudy", "rain", "heavyrain", "partlycloudy_night", "fair_day", "cloudy", "cloudy", "clearsky_night"][j as usize];
            ts.push(serde_json::json!({"time": iso(t), "data": {
                "instant": {"details": {"air_pressure_at_sea_level": 1007.2, "air_temperature": 12.0, "relative_humidity": 90.4, "wind_from_direction": 347.7, "wind_speed": 9.0}},
                "next_6_hours": {"summary": {"symbol_code": sym}, "details": {"air_temperature_max": 21.0 - j as f64, "air_temperature_min": 9.0 + j as f64, "precipitation_amount": if j < 4 { 6.8 } else { 0.0 }}}}}));
        }
        ts.push(serde_json::json!({"time": iso(start + 39 * 3600 + 8 * 21_600), "data": {"instant": {"details": {"air_temperature": 11.0, "relative_humidity": 80.0, "wind_speed": 2.0}}}}));
        serde_json::json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": [0.0, 51.5, 25]}, "properties": {"timeseries": ts}})
    }

    #[test]
    fn met_norway_daily_aggregation_uses_real_period_values() {
        let (loc, now) = (Location::new("Greenwich", "England", "United Kingdom", "GB", 51.5, 0.0, "UTC"), 1_795_511_400); // 09:10 UTC
        let b = WeatherClient::parse_met_norway(&metno_fixture(), &loc, now, now).expect("bundle");
        assert_eq!(b.daily.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(), ["2026-11-24", "2026-11-25", "2026-11-26", "2026-11-27"]);
        let d = &b.daily;
        // Today only covers 09:00 onward, so it is flagged as partial instead of posing as the whole day's high / low.
        assert_eq!((d[0].partial, d[1].partial, d[2].partial, d[3].partial), (true, false, false, false));
        assert!((d[0].precip_sum_mm.unwrap() - 15.0 * 0.2).abs() < 1e-9 && (d[1].precip_sum_mm.unwrap() - 24.0 * 0.2).abs() < 1e-9);
        // Hourly days: low / high over the real hourly values; the day's condition is its most significant hour (20:00
        // heavy rain), not the first sample of the day.
        assert_eq!((d[1].temp_min_c, d[1].temp_max_c, d[1].condition), (10.5, 18.0, WeatherCondition::HeavyRain));
        assert!((d[1].wind_max_mps.unwrap() - 5.3).abs() < 1e-9 && d[1].uv_max == Some(4.5));
        // 6-hourly day: four 6.8 mm periods are 27.2 mm (the old code showed 4.5 mm), and the high / low come from
        // next_6_hours air_temperature_max / _min (21 / 9), not from the four flat 12 °C instants.
        assert!((d[2].precip_sum_mm.unwrap() - 27.2).abs() < 1e-9);
        assert_eq!((d[2].temp_min_c, d[2].temp_max_c, d[2].condition), (9.0, 21.0, WeatherCondition::HeavyRain));
        assert_eq!(
            (d[3].precip_sum_mm, d[3].temp_min_c, d[3].temp_max_c, d[3].condition),
            (Some(0.0), 13.0, 17.0, WeatherCondition::Overcast)
        );
        // Four instantaneous samples are not a daily wind / UV maximum.
        assert_eq!((d[2].wind_max_mps, d[2].uv_max, d[3].wind_max_mps), (None, None, None));
        // No probability of precipitation outside the Nordic model: missing, not 0 %. Where MET reports one, it is used.
        assert_eq!((d[0].precip_prob_pct, d[1].precip_prob_pct, d[2].precip_prob_pct), (Some(40.0), None, None));
        assert_eq!((b.hourly[0].precip_prob_pct, b.hourly[1].precip_prob_pct, b.hourly.len()), (Some(40.0), None, 39));
        assert!(b.hourly.iter().all(|h| h.precip_mm == 0.2 && h.pressure_hpa == Some(1021.8)));
        // Current conditions: the 09:00 step (nearest to 09:10), its gust as reported, MET's own apparent temperature.
        assert_eq!(
            (b.current.updated_unix, b.current.wind_gust_mps, b.current.feels_like_c, b.current.feels_like_computed),
            (1_795_510_800, Some(5.0), Some(9.0), false)
        );
        assert_eq!((b.current.visibility_m, b.current.uv_index, b.uv_clear_sky, b.elevation_m), (None, Some(4.5), true, Some(25.0)));
        assert_eq!(
            (b.current.condition, b.current.precip_mm, b.source_name.as_str()),
            (WeatherCondition::Rain, Some(0.2), "MET Norway Locationforecast")
        );
        // An hour later the gust-less 10:00 step is current: no gust, rather than 0 or the previous hour's.
        let later = WeatherClient::parse_met_norway(&metno_fixture(), &loc, now + 3600, now).expect("bundle");
        assert_eq!((later.current.updated_unix, later.current.wind_gust_mps), (1_795_514_400, None));
        // A response too old to contain the present hour is rejected instead of being relabelled as "now".
        assert!(WeatherClient::parse_met_norway(&metno_fixture(), &loc, now + 30 * 86_400, now).is_err());
        assert_eq!(WeatherClient::met_symbol_condition("lightrainshowers_night"), Some(WeatherCondition::Rain));
        assert_eq!(
            (WeatherClient::met_symbol_condition("clearsky_day"), WeatherClient::met_symbol_condition("mystery")),
            (Some(WeatherCondition::Clear), None)
        );
    }
}
