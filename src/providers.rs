use crate::astro::{compute_sun_times, is_daytime_at};
use crate::model::{
    AirQuality, AlertSeverity, CurrentWeather, DailyForecast, HourlyForecast, Location, MinutelySample, ProviderChoice, WeatherAlert,
    WeatherBundle, WeatherCondition, apparent_temperature_c, dew_point_from_rh,
};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
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
type MetNoCacheEntry = (WeatherBundle, Option<String>, i64);
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
/// Margin of real neighboring data (px) requested around every hourly ECCC GeoMet tile (see `map::eccc`).
pub const ECCC_GUTTER_PX: u32 = 64;
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
    /// ECCC GeoMet interpolation slot `(h0, q)` for this frame: the frame sits `q` quarter-hours after UTC hour `h0`
    /// (`q == 0` is the hourly model raster itself; `q` in `1..=3` is synthesized between `h0` and `h0 + 1h`).
    #[inline]
    pub fn eccc_slot(&self) -> (i64, u8) {
        let h0 = self.time_unix.div_euclid(3600) * 3600;
        let q = (self.time_unix - h0 + 450) / 900;
        if q >= 4 { (h0 + 3600, 0) } else { (h0, q as u8) }
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
    /// Multi-tier ECCC GeoMet tile namespaces (`[HRDPS 2.5 km, RDPS 10 km, GDPS 15 km]`) composited as an exact
    /// partition of unity outside NOAA HRRR (`_out_`) for `NoaaHrrr` frames, or globally (`_full_`) for `EcccNwp` frames.
    /// Each namespace names one interpolation slot (see [`Self::eccc_slot`]).
    pub fn eccc_tier_namespaces(&self) -> Option<[String; 3]> {
        let mode = match self.source {
            RadarFrameSource::NoaaHrrr(..) => "out",
            RadarFrameSource::EcccNwp => "full",
            RadarFrameSource::Tile(..) => return None,
        };
        let (h0, q) = self.eccc_slot();
        Some(EcccModel::ALL.map(|m| format!("eccc_{}_{mode}_v7_{h0}_{q}", m.tag())))
    }
    pub fn max_native_zoom(&self) -> u8 {
        match self.source {
            // NOAA HRRR 3.0 km CONUS model: regional zooms (`z <= 7.3`) stay at z=5/6 via `primary_zoom_bias = -0.35`,
            // while deep city zooms (`z >= 7.35`) progressively unlock z=7 (`~0.96 km/px`, 2× sharper!) with instant z=6 fallback.
            RadarFrameSource::NoaaHrrr(..) => 7,
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
fn at_f(v: &Value, k: &str, i: usize, d: f64) -> f64 {
    v.get(k).and_then(|a| a.get(i)).and_then(Value::as_f64).unwrap_or(d)
}
fn wrap_lon(lo: f64) -> f64 {
    ((lo + 180.0).rem_euclid(360.0)) - 180.0
}
fn coords_csv(c: &[(f64, f64)]) -> (String, String) {
    let j = |f: fn(&(f64, f64)) -> String| c.iter().map(f).collect::<Vec<_>>().join(",");
    (j(|&(la, _)| format!("{:.2}", la.clamp(-85.0, 85.0))), j(|&(_, lo)| format!("{:.2}", wrap_lon(lo))))
}

fn epa_pm25_aqi(pm25: f64) -> i32 {
    if !pm25.is_finite() || pm25 <= 0.0 {
        return 0;
    }
    let c = (pm25 * 10.0).floor() / 10.0;
    const BP: [(f64, f64, f64, f64); 6] = [
        (0.0, 9.0, 0.0, 50.0),
        (9.1, 35.4, 51.0, 100.0),
        (35.5, 55.4, 101.0, 150.0),
        (55.5, 125.4, 151.0, 200.0),
        (125.5, 225.4, 201.0, 300.0),
        (225.5, 325.4, 301.0, 500.0),
    ];
    BP.iter()
        .find(|&&(_, hi, _, _)| c <= hi)
        .map_or(500, |&(lo, hi, ilo, ihi)| (((ihi - ilo) / (hi - lo)) * (c - lo).max(0.0) + ilo).round() as i32)
}
fn nws_wind_to_mps(v: f64, u: &str) -> f64 {
    if u.contains("km_h-1") {
        v / 3.6
    } else if u.contains("kt") || u.contains("kn") {
        v * 0.514_444
    } else if u.contains("mi_h-1") {
        v * 0.447_04
    } else {
        v
    }
}
fn nws_desc_to_condition(desc: &str) -> Option<WeatherCondition> {
    use WeatherCondition as W;
    let d = desc.trim().to_ascii_lowercase();
    const M: &[(&[&str], W)] = &[
        (&["thunder", "t-storm"], W::Thunderstorm),
        (&["freezing"], W::FreezingRain),
        (&["sleet", "ice pellet", "mix"], W::Sleet),
        (&["heavy snow", "blizzard"], W::HeavySnow),
        (&["snow", "flurr"], W::Snow),
        (&["heavy rain"], W::HeavyRain),
        (&["rain", "shower"], W::Rain),
        (&["drizzle"], W::Drizzle),
        (&["fog", "mist", "haze", "smoke"], W::Fog),
        (&["overcast"], W::Overcast),
        (&["mostly cloudy", "partly cloudy", "broken"], W::PartlyCloudy),
        (&["few clouds", "mostly clear", "fair"], W::MostlyClear),
        (&["clear", "sunny"], W::Clear),
    ];
    (!d.is_empty()).then(|| M.iter().find(|&&(ks, _)| ks.iter().any(|k| d.contains(k))).map(|&(_, c)| c)).flatten()
}
fn metar_wx_desc(wx: Option<&str>, clouds: Option<&Vec<Value>>) -> &'static str {
    if let Some(w) = wx.map(str::trim).filter(|s| !s.is_empty()) {
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
        if w.contains("FG") || w.contains("BR") || w.contains("HZ") || w.contains("FU") {
            return "Fog";
        }
    }
    let Some(cs) = clouds else { return "" };
    let rank = |c: &str| match c {
        "OVC" | "VV" => 4,
        "BKN" => 3,
        "SCT" => 2,
        "FEW" => 1,
        "CLR" | "SKC" | "CAVOK" => 0,
        _ => -1,
    };
    match cs.iter().filter_map(|o| o.get("cover").and_then(Value::as_str)).map(rank).max().unwrap_or(-1) {
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
        // Model forecast frames on a quarter-hour UTC grid: every 15 min through +2h, then every 30 min through +12h.
        // HRRR is natively 15-minute; hourly ECCC rasters are motion-interpolated onto the same grid (`map::eccc`), so
        // consecutive frames never differ by more than 30 minutes of storm travel.
        let first_off: i64 = if frames.iter().any(|f| f.is_nowcast) { 45 } else { 15 };
        let mut last_ts = frames.last().map_or(live_ts, |f| f.time_unix);
        let mut off = first_off;
        while off <= 720 {
            let grid = if off <= 120 { 900 } else { 1800 };
            let vt = ((live_ts + off * 60 + grid / 2) / grid) * grid;
            off += if off < 120 { 15 } else { 30 };
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
                "https://api.open-meteo.com/v1/forecast?latitude={lats}&longitude={lons}&hourly=temperature_2m,precipitation,wind_speed_10m,wind_direction_10m,pressure_msl&forecast_hours=13&timeformat=unixtime&wind_speed_unit=ms"
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
                            let (Some(ts), Some(tc), Some(pr)) = (tv.as_i64(), opt_f("temperature_2m", i), opt_f("pressure_msl", i)) else {
                                continue;
                            };
                            let hs = opt_f("wind_speed_10m", i).unwrap_or(0.0);
                            let (hu, hv) = wind_uv(hs, opt_f("wind_direction_10m", i).unwrap_or(0.0));
                            hourly[hl] = (ts, [tc as f32, opt_f("precipitation", i).unwrap_or(0.0) as f32, hu, hv, hs as f32, pr as f32]);
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
                            let (Some(tc), Some(pr)) = (
                                jf(det, "/air_temperature").filter(|x| x.is_finite()),
                                jf(det, "/air_pressure_at_sea_level").filter(|x| x.is_finite()),
                            ) else {
                                continue;
                            };
                            let ws = jf(det, "/wind_speed").unwrap_or(0.0);
                            let wd = jf(det, "/wind_from_direction").unwrap_or(0.0);
                            let pmm = entry
                                .pointer("/data/next_1_hours/details/precipitation_amount")
                                .and_then(Value::as_f64)
                                .or_else(|| {
                                    entry
                                        .pointer("/data/next_6_hours/details/precipitation_amount")
                                        .and_then(Value::as_f64)
                                        .map(|x| x / 6.0)
                                })
                                .unwrap_or(0.0) as f32;
                            let (hu, hv) = wind_uv(ws, wd);
                            hourly[hl] = (ts, [tc as f32, pmm, hu, hv, ws as f32, pr as f32]);
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
        let url = format!("https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lats}&longitude={lons}&current=us_aqi,pm2_5");
        let Ok(val) = get_json(&self.0, &url, false) else { return Vec::new() };
        val.as_array()
            .map_or_else(|| vec![&val], |a| a.iter().collect())
            .into_iter()
            .zip(coords)
            .filter_map(|(i, &(la, lo))| {
                let aqi = jf(i, "/current/us_aqi")?.max(jf(i, "/current/pm2_5").map_or(0.0, |p| epa_pm25_aqi(p) as f64));
                Some((la.clamp(-85.0, 85.0), wrap_lon(lo), aqi))
            })
            .collect()
    }

    /// Worldwide 1-request official NOAA Aviation Weather Center METAR observation (`aviationweather.gov/api/data/metar`).
    /// Replaces 3 serial `api.weather.gov` round trips (~600 ms, US-only) with 1 gzipped call (~135 ms, ~800 B wire, global),
    /// normalizing the nearest fresh ICAO station into the NWS `properties` schema with automatic `api.weather.gov` US fallback.
    fn fetch_metar_obs(&self, lat: f64, lon: f64) -> Option<Value> {
        let now = Utc::now().timestamp();
        let ck = format!("metar_{}_{}", (lat * 8.0).round() as i32, (lon * 8.0).round() as i32);
        if let Some((obs, _, ts)) = NWS_OBS_CACHE.lock().ok().and_then(|g| g.get(&ck).cloned())
            && (now - ts).abs() < 300
        {
            return Some(obs);
        }
        let dlon = (0.45 / lat.to_radians().cos().abs().max(0.25)).min(1.5);
        let url = format!(
            "https://aviationweather.gov/api/data/metar?bbox={:.3},{:.3},{:.3},{:.3}&format=json",
            (lat - 0.45).max(-85.0),
            (lon - dlon).max(-180.0),
            (lat + 0.45).min(85.0),
            (lon + dlon).min(180.0),
        );
        let arr = get_json_t(&self.0, &url, false, 1800).ok()?;
        let obs = Self::parse_metar_array(&arr, lat, lon, now)?;
        if let Ok(mut g) = NWS_OBS_CACHE.lock() {
            if g.len() > 128 {
                g.clear();
            }
            g.insert(ck, (obs.clone(), None, now));
            crate::storage::save_nws_obs_cache(&g);
        }
        Some(obs)
    }

    fn parse_metar_array(arr: &Value, lat: f64, lon: f64, now: i64) -> Option<Value> {
        let cos = lat.to_radians().cos().abs().max(0.15);
        let best = arr
            .as_array()?
            .iter()
            .filter_map(|m| {
                let ot = m.get("obsTime").and_then(Value::as_i64)?;
                let temp = m.get("temp").and_then(Value::as_f64).filter(|t| (-65.0..=60.0).contains(t))?;
                if (now - ot).abs() > 4500 {
                    return None;
                }
                let (slat, slon) = (m.get("lat").and_then(Value::as_f64)?, m.get("lon").and_then(Value::as_f64)?);
                let dist = (slat - lat).hypot((slon - lon) * cos);
                (dist <= 0.65).then_some((dist, m, ot, temp))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        let (_, m, ot, temp) = best;
        let dewp = m.get("dewp").and_then(Value::as_f64).filter(|d| (-80.0..=50.0).contains(d));
        let rh = dewp.map(|td| {
            let e = ((17.625 * td) / (243.04 + td) - (17.625 * temp) / (243.04 + temp)).exp();
            (100.0 * e).clamp(0.0, 100.0)
        });
        let wspd = m.get("wspd").and_then(Value::as_f64).filter(|w| (0.0..=180.0).contains(w)).map(|kt| kt * 0.514_444);
        let wgst = m.get("wgst").and_then(Value::as_f64).filter(|w| (0.0..=220.0).contains(w)).map(|kt| kt * 0.514_444);
        let wdir = m.get("wdir").and_then(Value::as_f64).filter(|d| (0.0..=360.0).contains(d));
        let slp = m.get("slp").and_then(Value::as_f64).filter(|p| (870.0..=1085.0).contains(p)).map(|p| p * 100.0);
        let altim = m.get("altim").and_then(Value::as_f64).filter(|p| (870.0..=1085.0).contains(p)).map(|p| p * 100.0);
        let vis_m = m
            .get("visib")
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim_end_matches('+').parse::<f64>().ok()))
            .filter(|v| *v > 0.0)
            .map(|sm| sm * 1609.344);
        let desc = metar_wx_desc(m.get("wxString").and_then(Value::as_str), m.get("clouds").and_then(Value::as_array));
        let ts = DateTime::from_timestamp(ot, 0)?.to_rfc3339();
        let val = |x: Option<f64>| serde_json::json!({ "value": x });
        let mps = |x: Option<f64>| serde_json::json!({ "value": x, "unitCode": "wmoUnit:m_s-1" });
        Some(serde_json::json!({
            "properties": {
                "timestamp": ts,
                "temperature": val(Some(temp)),
                "dewpoint": val(dewp),
                "relativeHumidity": val(rh),
                "windSpeed": mps(wspd),
                "windGust": mps(wgst),
                "windDirection": val(wdir),
                "seaLevelPressure": val(slp),
                "barometricPressure": val(altim),
                "visibility": val(vis_m),
                "textDescription": desc,
            }
        }))
    }

    fn fetch_nws_obs(&self, lat: f64, lon: f64) -> Option<Value> {
        if let Some(obs) = self.fetch_metar_obs(lat, lon) {
            return Some(obs);
        }
        if !(17.0..=72.0).contains(&lat) || !(-179.0..=-64.0).contains(&lon) {
            return None;
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
        })?;
        let now = Utc::now().timestamp();
        for (idx, id) in ids.iter().enumerate() {
            let cached = NWS_OBS_CACHE.lock().ok().and_then(|g| g.get(id).cloned());
            if let Some((ref obs, _, ts)) = cached
                && (now - ts).abs() < 300
            {
                return Some(obs.clone());
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
                        return Some(old);
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
            let fresh = rfc3339(p, "/timestamp").is_none_or(|t| (now - t).abs() <= 4500);
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
                return Some(obs);
            }
        }
        None
    }

    fn apply_nws_obs(b: &mut WeatherBundle, obs: &Value) -> bool {
        let Some(p) = obs.get("properties").filter(|p| rfc3339(p, "/timestamp").is_none_or(|t| (Utc::now().timestamp() - t).abs() <= 4500))
        else {
            return false;
        };
        let val = |k: &str| p.get(k)?.get("value")?.as_f64();
        let wind = |k: &str| val(k).map(|v| nws_wind_to_mps(v, p.get(k).and_then(|o| js(o, "/unitCode")).unwrap_or("")));
        let (cur, mut merged) = (&mut b.current, false);
        let _ = val("temperature").filter(|t| (-65.0..=60.0).contains(t)).map(|t| (cur.temp_c, merged) = (t, true));
        let _ = wind("windSpeed").filter(|w| (0.0..=90.0).contains(w)).map(|v| (cur.wind_speed_mps, merged) = (v, true));
        let _ = val("dewpoint").map(|v| cur.dew_point_c = v);
        let _ = val("relativeHumidity").map(|v| cur.humidity_pct = v.clamp(0.0, 100.0));
        let _ = wind("windGust").filter(|w| (0.0..=110.0).contains(w)).map(|v| cur.wind_gust_mps = Some(v));
        let _ = val("windDirection").map(|v| cur.wind_dir_deg = v);
        let prs = val("seaLevelPressure").or_else(|| val("barometricPressure")).map(|pa| if pa > 2000.0 { pa / 100.0 } else { pa });
        let _ = prs.filter(|pr| (870.0..=1085.0).contains(pr)).map(|v| cur.pressure_hpa = v);
        let _ = val("visibility").filter(|v| *v > 50.0).map(|v| cur.visibility_m = Some(v));
        let _ = js(p, "/textDescription").and_then(nws_desc_to_condition).map(|c| cur.condition = c);
        if merged {
            cur.feels_like_c = val("windChill")
                .or_else(|| val("heatIndex"))
                .filter(|t| (-75.0..=75.0).contains(t))
                .unwrap_or_else(|| apparent_temperature_c(cur.temp_c, cur.humidity_pct, cur.wind_speed_mps));
            let today = b.location.local_date(cur.updated_unix).format("%Y-%m-%d").to_string();
            let di = b.daily.iter().position(|d| d.date >= today).unwrap_or(0);
            let _ = b
                .daily
                .get_mut(di)
                .map(|d0| (d0.temp_max_c, d0.temp_min_c) = (d0.temp_max_c.max(cur.temp_c), d0.temp_min_c.min(cur.temp_c)));
            let _ = b.hourly.first_mut().filter(|h0| (h0.timestamp_unix - cur.updated_unix).abs() <= 2700).map(|h0| h0.temp_c = cur.temp_c);
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
            if let Some(cb) = on_primary.as_deref_mut() {
                cb(b.clone());
            }
            let add = |dst: &mut String, tag: &str| {
                if !tag.is_empty() && !dst.contains(tag) {
                    dst.push_str(" + ");
                    dst.push_str(tag);
                }
            };
            let mut updated_mid = false;
            if let Some(obs) = nws_h.and_then(|h| h.join().ok().flatten())
                && Self::apply_nws_obs(&mut b, &obs)
            {
                add(&mut b.source_name, "NOAA NWS");
                updated_mid = true;
            }
            if let Some((alerts, src)) = alerts_h.and_then(|h| h.join().ok())
                && !alerts.is_empty()
            {
                b.alerts = alerts;
                add(&mut b.source_name, &src);
                updated_mid = true;
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
            Ok(b)
        })
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
        let pm2_5 = jf(&v, "/current/pm2_5").unwrap_or(0.0);
        let aq =
            v.pointer("/current/us_aqi").and_then(Value::as_i64).map(|u| AirQuality { us_aqi: (u as i32).max(epa_pm25_aqi(pm2_5)), pm2_5 });
        if let Ok(mut g) = AQI_CACHE.lock() {
            if g.len() > 128 {
                g.clear();
            }
            g.insert(key, (aq.clone(), tz.clone(), now));
            crate::storage::save_aqi_cache(&g);
        }
        (aq, tz)
    }

    fn fetch_alerts(&self, lat: f64, lon: f64, country_code: &str) -> (Vec<WeatherAlert>, String) {
        let key = ((lat * 4.0).round() as i32, (lon * 4.0).round() as i32);
        let now = Utc::now().timestamp();
        let cached = ALERTS_CACHE.lock().ok().and_then(|g| g.get(&key).cloned());
        if let Some((ref al, ref src, _, ts)) = cached
            && (now - ts).abs() < 300
        {
            return (al.clone(), src.clone());
        }
        let inm = cached.as_ref().and_then(|(_, _, et, _)| et.as_deref());
        let cc = country_code.trim();
        let is_ca = cc.eq_ignore_ascii_case("CA") || (cc.is_empty() && (49.0..=72.0).contains(&lat) && (-141.0..=-52.0).contains(&lon));
        if is_ca {
            let url = format!(
                "https://api.weather.gc.ca/collections/weather-alerts/items?f=json&limit=4&bbox={:.4},{:.4},{:.4},{:.4}",
                lon - 0.08,
                lat - 0.08,
                lon + 0.08,
                lat + 0.08
            );
            let (val, etag) = match get_json_conditional(&self.0, &url, false, 2200, None, inm) {
                Ok(None) => {
                    if let Some((al, csrc, et, _)) = cached {
                        if let Ok(mut g) = ALERTS_CACHE.lock() {
                            g.insert(key, (al.clone(), csrc.clone(), et, now));
                            crate::storage::save_alerts_cache(&g);
                        }
                        return (al, csrc);
                    }
                    (None, None)
                }
                Ok(Some((v, _, et))) => (Some(v), et),
                Err(_) => (None, None),
            };
            let mut seen = HashSet::new();
            let mut alerts: Vec<_> = val
                .iter()
                .flat_map(|r| ja(r, "/features"))
                .filter_map(|item| {
                    let p = item.get("properties")?;
                    if let Some(exp) = rfc3339(p, "/expiration_datetime")
                        && exp <= now
                    {
                        return None;
                    }
                    let raw_ev = js(p, "/alert_name_en").or_else(|| js(p, "/alert_short_name_en"))?.trim();
                    if raw_ev.is_empty() {
                        return None;
                    }
                    let mut chars = raw_ev.chars();
                    let event = match chars.next() {
                        Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                        None => return None,
                    };
                    let severity = match js(p, "/alert_type").unwrap_or("").to_ascii_lowercase().as_str() {
                        "warning" => AlertSeverity::Severe,
                        "watch" => AlertSeverity::Moderate,
                        _ => AlertSeverity::Minor,
                    };
                    let area = js(p, "/feature_name_en").unwrap_or("").trim();
                    let headline = if area.is_empty() { event.clone() } else { format!("{event} — {area}") };
                    seen.insert((event.clone(), headline.clone())).then(|| WeatherAlert {
                        event,
                        severity,
                        headline,
                        description: js(p, "/alert_text_en").unwrap_or("").trim().into(),
                    })
                })
                .collect();
            alerts.sort_by_key(|a| a.severity as u8);
            alerts.truncate(4);
            let src_s = "ECCC".to_string();
            if let Ok(mut g) = ALERTS_CACHE.lock() {
                if g.len() > 128 {
                    g.clear();
                }
                g.insert(key, (alerts.clone(), src_s.clone(), etag, now));
                crate::storage::save_alerts_cache(&g);
            }
            return (alerts, src_s);
        }
        let (url, geo, arr, prop, ks, src): (String, bool, &str, bool, [&[&str]; 3], &str) =
            if (cc.eq_ignore_ascii_case("US") || cc.is_empty()) && (15.0..=72.0).contains(&lat) && (-179.0..=-60.0).contains(&lon) {
                (
                    format!("https://api.weather.gov/alerts/active?point={lat:.4},{lon:.4}"),
                    true,
                    "/features",
                    true,
                    [&["/event"], &["/headline"], &["/description"]],
                    "NOAA NWS",
                )
            } else if (47.0..=55.5).contains(&lat) && (5.5..=15.5).contains(&lon) {
                (
                    format!("https://api.brightsky.dev/alerts?lat={lat:.4}&lon={lon:.4}"),
                    false,
                    "/alerts",
                    false,
                    [&["/event_en", "/event_de"], &["/headline_en", "/headline_de"], &["/description_en", "/description_de"]],
                    "DWD",
                )
            } else if (57.0..=72.0).contains(&lat) && (4.0..=32.0).contains(&lon) {
                (
                    format!("https://api.met.no/weatherapi/metalerts/2.0/current.json?lat={lat:.4}&lon={lon:.4}"),
                    false,
                    "/features",
                    true,
                    [&["/eventAwarenessName", "/event"], &["/title"], &["/description"]],
                    "MET Norway",
                )
            } else {
                return (Vec::new(), String::new());
            };
        let (val, etag) = match get_json_conditional(&self.0, &url, geo, 2200, None, inm) {
            Ok(None) => {
                // 304 Not Modified via ETag: zero bytes transferred, zero JSON parsed!
                if let Some((al, csrc, et, _)) = cached {
                    if let Ok(mut g) = ALERTS_CACHE.lock() {
                        g.insert(key, (al.clone(), csrc.clone(), et, now));
                        crate::storage::save_alerts_cache(&g);
                    }
                    return (al, csrc);
                }
                (None, None)
            }
            Ok(Some((v, _, et))) => (Some(v), et),
            Err(_) => (None, None),
        };
        let mut seen = HashSet::new();
        let mut alerts: Vec<_> = val
            .iter()
            .flat_map(|r| ja(r, arr))
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
        let src_s = src.to_string();
        if let Ok(mut g) = ALERTS_CACHE.lock() {
            if g.len() > 128 {
                g.clear();
            }
            g.insert(key, (alerts.clone(), src_s.clone(), etag, now));
            crate::storage::save_alerts_cache(&g);
        }
        (alerts, src_s)
    }

    /// Fetches lean current + today H/L summary for all background sidebar cities in a SINGLE multi-coordinate Open-Meteo request (-75% payload!),
    /// with parallel MET Norway direct fallback if Open-Meteo is degraded.
    pub fn fetch_sidebar_batch(&self, locs: &[Location], provider: ProviderChoice) -> Vec<(String, WeatherBundle)> {
        if locs.is_empty() {
            return Vec::new();
        }
        if open_meteo_healthy() {
            let metno = provider == ProviderChoice::MetNorway;
            let m = if metno { "&models=metno_seamless" } else { "" };
            let lats = locs.iter().map(|l| format!("{:.4}", l.lat)).collect::<Vec<_>>().join(",");
            let lons = locs.iter().map(|l| format!("{:.4}", l.lon)).collect::<Vec<_>>().join(",");
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lats}&longitude={lons}&timezone=auto&timeformat=unixtime&wind_speed_unit=ms{m}&current=temperature_2m,relative_humidity_2m,apparent_temperature,is_day,precipitation,weather_code,pressure_msl,wind_speed_10m,wind_direction_10m&daily=weather_code,temperature_2m_max,temperature_2m_min&forecast_days=1"
            );
            if let Ok(root) = get_json_t(&self.0, &url, false, 2400) {
                mark_open_meteo_ok();
                let items: Vec<&Value> = root.as_array().map_or_else(|| vec![&root], |a| a.iter().collect());
                return locs
                    .iter()
                    .zip(items)
                    .filter_map(|(loc, v)| Self::parse_open_meteo_root(v, loc, metno).ok().map(|b| (loc.id.clone(), b)))
                    .collect();
            }
            mark_open_meteo_down();
        }
        std::thread::scope(|s| {
            let hs: Vec<_> =
                locs.iter().map(|l| s.spawn(move || self.fetch_met_norway_direct(l).ok().map(|b| (l.id.clone(), b)))).collect();
            hs.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
        })
    }

    fn fetch_open_meteo(&self, location: &Location, metno: bool) -> Result<WeatherBundle, String> {
        let m = if metno { "&models=metno_seamless" } else { "" };
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={:.4}&longitude={:.4}&timezone=auto&timeformat=unixtime&wind_speed_unit=ms{m}&current=temperature_2m,relative_humidity_2m,dew_point_2m,apparent_temperature,is_day,precipitation,weather_code,pressure_msl,wind_speed_10m,wind_direction_10m,wind_gusts_10m&minutely_15=precipitation&forecast_minutely_15=8&hourly=temperature_2m,precipitation_probability,precipitation,weather_code,pressure_msl,visibility,wind_speed_10m,wind_direction_10m,uv_index&past_hours=1&forecast_hours=26&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_sum,precipitation_probability_max,wind_speed_10m_max&forecast_days=10",
            location.lat, location.lon
        );
        let root = get_json(&self.0, &url, false)?;
        Self::parse_open_meteo_root(&root, location, metno)
    }

    fn parse_open_meteo_root(root: &Value, location: &Location, metno: bool) -> Result<WeatherBundle, String> {
        let cur = root.get("current").ok_or("Missing Open-Meteo current block")?;
        let mut loc = location.clone();
        let _ = js(root, "/timezone").filter(|s| !s.is_empty() && *s != "GMT").map(|tz| loc.timezone = tz.into());
        let now_ts = cur.get("time").and_then(parse_ts).unwrap_or_else(|| Utc::now().timestamp());
        let tc = jf(cur, "/temperature_2m").filter(|x| x.is_finite()).ok_or("Missing Open-Meteo current temperature")?;
        let rh = jf(cur, "/relative_humidity_2m").filter(|x| x.is_finite()).ok_or("Missing Open-Meteo current humidity")?;
        let cur_pressure = jf(cur, "/pressure_msl").filter(|x| x.is_finite());
        let dp_now = jf(cur, "/dew_point_2m");
        let (mut hourly, mut uv_now, mut vis_now, mut best_dt, mut last_pr) = (Vec::new(), 0.0, None, i64::MAX, cur_pressure);
        if let Some(h) = root.get("hourly") {
            let opt_f = |k: &str, i: usize| h.get(k).and_then(|a| a.get(i)).and_then(Value::as_f64).filter(|x| x.is_finite());
            for (i, tv) in ja(h, "/time").iter().enumerate() {
                let Some(ts) = parse_ts(tv) else { continue };
                let (u, v) = (opt_f("uv_index", i).unwrap_or(0.0), opt_f("visibility", i));
                let _ = ((ts - now_ts).abs() < best_dt).then(|| (best_dt, uv_now, vis_now) = ((ts - now_ts).abs(), u, v));
                if let Some(p) = opt_f("pressure_msl", i) {
                    last_pr = Some(p);
                }
                if ts >= now_ts - 3600 && hourly.len() < 36 {
                    let Some(htc) = opt_f("temperature_2m", i) else { continue };
                    hourly.push(HourlyForecast {
                        timestamp_unix: ts,
                        temp_c: htc,
                        condition: WeatherCondition::from_wmo(opt_f("weather_code", i).unwrap_or(3.0) as i32),
                        is_day: is_daytime_at(loc.lat, loc.lon, ts),
                        precip_prob_pct: opt_f("precipitation_probability", i).unwrap_or(0.0),
                        precip_mm: opt_f("precipitation", i).unwrap_or(0.0),
                        wind_speed_mps: opt_f("wind_speed_10m", i).unwrap_or(0.0),
                        wind_dir_deg: opt_f("wind_direction_10m", i).unwrap_or(0.0),
                        pressure_hpa: opt_f("pressure_msl", i).or(last_pr).unwrap_or(1013.25),
                    });
                }
            }
        }
        let current = CurrentWeather {
            temp_c: tc,
            feels_like_c: jf(cur, "/apparent_temperature").unwrap_or(tc),
            condition: WeatherCondition::from_wmo(jf(cur, "/weather_code").unwrap_or(3.0) as i32),
            is_day: jf(cur, "/is_day").map_or_else(|| is_daytime_at(loc.lat, loc.lon, now_ts), |x| x >= 0.5),
            wind_speed_mps: jf(cur, "/wind_speed_10m").unwrap_or(0.0),
            wind_gust_mps: jf(cur, "/wind_gusts_10m"),
            wind_dir_deg: jf(cur, "/wind_direction_10m").unwrap_or(0.0),
            humidity_pct: rh,
            dew_point_c: dp_now.unwrap_or_else(|| dew_point_from_rh(tc, rh)),
            pressure_hpa: cur_pressure.or(last_pr).unwrap_or(1013.25),
            visibility_m: vis_now,
            uv_index: uv_now,
            precip_mm: jf(cur, "/precipitation").unwrap_or(0.0),
            updated_unix: now_ts,
        };
        let minutely = root.get("minutely_15").map_or_else(Vec::new, |m| {
            let pts: Vec<_> = ja(m, "/time")
                .iter()
                .enumerate()
                .filter_map(|(i, tv)| Some((parse_ts(tv)?, at_f(m, "precipitation", i, 0.0) * 4.0)))
                .collect();
            if pts.len() < 2 || !pts.iter().any(|(_, r)| *r > 0.02) {
                return Vec::new();
            }
            (0..12)
                .map(|step| {
                    let ts = now_ts + step * 300;
                    let rate = pts.array_windows::<2>().find(|&&[_, b]| ts <= b.0).map_or(pts[pts.len() - 1].1, |&[a, b]| {
                        a.1 + (b.1 - a.1) * ((ts - a.0) as f64 / (b.0 - a.0).max(1) as f64).clamp(0.0, 1.0)
                    });
                    MinutelySample { timestamp_unix: ts, precip_mm_hr: rate.max(0.0) }
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
                    let (tmin, tmax) = (opt_d("temperature_2m_min", i)?, opt_d("temperature_2m_max", i)?);
                    let date = tv.as_str().map(str::to_string).unwrap_or_else(|| {
                        loc.local_date(parse_ts(tv).unwrap_or(now_ts + i as i64 * 86_400)).format("%Y-%m-%d").to_string()
                    });
                    let sun = NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok().map(|nd| compute_sun_times(loc.lat, loc.lon, nd));
                    let ts_opt = |k: &str| d.get(k).and_then(|a| a.get(i)).and_then(parse_ts).filter(|&x| x > 0);
                    Some(DailyForecast {
                        sunrise_unix: ts_opt("sunrise").or_else(|| sun.and_then(|s| s.sunrise_unix)),
                        sunset_unix: ts_opt("sunset").or_else(|| sun.and_then(|s| s.sunset_unix)),
                        date,
                        temp_min_c: tmin,
                        temp_max_c: tmax,
                        condition: WeatherCondition::from_wmo(opt_d("weather_code", i).unwrap_or(3.0) as i32),
                        precip_prob_pct: opt_d("precipitation_probability_max", i).unwrap_or(0.0),
                        precip_sum_mm: opt_d("precipitation_sum", i).unwrap_or(0.0),
                        wind_max_mps: opt_d("wind_speed_10m_max", i).unwrap_or(0.0),
                        uv_max: opt_d("uv_index_max", i).unwrap_or(0.0),
                    })
                })
                .collect()
        });
        let source_name = if metno { "MET Norway (Open-Meteo)" } else { "Open-Meteo (NOAA / ECMWF / DWD)" }.into();
        Ok(WeatherBundle { location: loc, current, minutely, hourly, daily, air_quality: None, alerts: Vec::new(), source_name })
    }

    /// Direct MET Norway Locationforecast 2.0 (ECMWF) automatic resilience fallback with `If-Modified-Since` 304 support.
    fn fetch_met_norway_direct(&self, orig: &Location) -> Result<WeatherBundle, String> {
        let key = ((orig.lat * 20.0).round() as i32, (orig.lon * 20.0).round() as i32);
        let now = Utc::now().timestamp();
        let cached = METNO_CACHE.lock().ok().and_then(|g| g.get(&key).cloned());
        if let Some((b, _, ts)) = &cached
            && now - *ts < 300
        {
            let mut out = b.clone();
            out.location = orig.clone();
            return Ok(out);
        }
        let ims = cached.as_ref().and_then(|(_, lm, _)| lm.as_deref());
        // Use MET Norway /complete (like Breezy Weather's MetNoService) for real UV index, dew point, apparent temp,
        // wind gust, and probability of precipitation instead of guessing missing fields!
        let u = format!("https://api.met.no/weatherapi/locationforecast/2.0/complete?lat={:.4}&lon={:.4}", orig.lat, orig.lon);
        let (root, last_mod) = match get_json_conditional(&self.0, &u, false, 4500, ims, None) {
            Ok(None) => {
                if let Some((mut b, lm, _)) = cached {
                    if let Ok(mut g) = METNO_CACHE.lock() {
                        g.insert(key, (b.clone(), lm, now));
                    }
                    b.location = orig.clone();
                    return Ok(b);
                }
                return Err("met.no 304 without cache".into());
            }
            Ok(Some((v, lm, _))) => (v, lm),
            Err(e) => return Err(e),
        };
        let ts_arr = ja(&root, "/properties/timeseries");
        if ts_arr.is_empty() {
            return Err("empty met.no timeseries".into());
        }
        let map_sym = |s: &str| -> WeatherCondition {
            use WeatherCondition as W;
            let base = s.split('_').next().unwrap_or("");
            if base.contains("thunder") {
                W::Thunderstorm
            } else if base.contains("heavysnow") {
                W::HeavySnow
            } else if base.contains("snow") {
                W::Snow
            } else if base.contains("sleet") {
                W::Sleet
            } else if base.contains("heavyrain") {
                W::HeavyRain
            } else if base.contains("lightrain") || base.contains("drizzle") {
                W::Drizzle
            } else if base.contains("rain") {
                W::Rain
            } else if base.contains("fog") {
                W::Fog
            } else if base.contains("cloudy") && !base.contains("partly") {
                W::Overcast
            } else if base.contains("partlycloudy") {
                W::PartlyCloudy
            } else if base.contains("fair") {
                W::MostlyClear
            } else {
                W::Clear
            }
        };
        let loc = orig.clone();
        let now_ts = Utc::now().timestamp();
        let mut hourly = Vec::with_capacity(48);
        struct MetNoDayAgg {
            date: String,
            lo: f64,
            hi: f64,
            cond: WeatherCondition,
            psum: f64,
            wmax: f64,
            pmax: f64,
            uvmax: f64,
        }
        let mut by_day: Vec<MetNoDayAgg> = Vec::new();
        for entry in ts_arr {
            let Some(t) = rfc3339(entry, "/time") else { continue };
            let det = entry.pointer("/data/instant/details");
            let Some(tc) = det.and_then(|d| jf(d, "/air_temperature")).filter(|x| x.is_finite()) else { continue };
            let ws = det.and_then(|d| jf(d, "/wind_speed")).unwrap_or(0.0);
            let wd = det.and_then(|d| jf(d, "/wind_from_direction")).unwrap_or(0.0);
            let pr = det.and_then(|d| jf(d, "/air_pressure_at_sea_level")).unwrap_or(1013.25);
            let uv = det.and_then(|d| jf(d, "/ultraviolet_index_clear_sky")).unwrap_or(0.0).max(0.0);
            let sym = entry
                .pointer("/data/next_1_hours/summary/symbol_code")
                .or_else(|| entry.pointer("/data/next_6_hours/summary/symbol_code"))
                .and_then(Value::as_str)
                .unwrap_or("cloudy");
            let pmm = entry
                .pointer("/data/next_1_hours/details/precipitation_amount")
                .or_else(|| entry.pointer("/data/next_6_hours/details/precipitation_amount").map(|_| entry))
                .and_then(|v| v.as_f64().or_else(|| jf(v, "/data/next_6_hours/details/precipitation_amount").map(|x| x / 6.0)))
                .unwrap_or(0.0);
            let cond = map_sym(sym);
            let is_day = is_daytime_at(loc.lat, loc.lon, t);
            // Only use real reported probability_of_precipitation (never invent fake percentages!).
            let pprob = entry
                .pointer("/data/next_1_hours/details/probability_of_precipitation")
                .or_else(|| entry.pointer("/data/next_6_hours/details/probability_of_precipitation"))
                .and_then(Value::as_f64)
                .map_or(0.0, |p| p.clamp(0.0, 100.0));
            if t >= now_ts - 3600 && hourly.len() < 48 {
                hourly.push(HourlyForecast {
                    timestamp_unix: t,
                    temp_c: tc,
                    condition: cond,
                    is_day,
                    precip_prob_pct: pprob,
                    precip_mm: pmm,
                    wind_speed_mps: ws,
                    wind_dir_deg: wd,
                    pressure_hpa: pr,
                });
            }
            let dkey = loc.local_date(t).format("%Y-%m-%d").to_string();
            let day_uv = if is_day { uv } else { 0.0 };
            if let Some(slot) = by_day.iter_mut().find(|x| x.date == dkey) {
                (slot.lo, slot.hi, slot.psum, slot.wmax, slot.pmax, slot.uvmax) =
                    (slot.lo.min(tc), slot.hi.max(tc), slot.psum + pmm, slot.wmax.max(ws), slot.pmax.max(pprob), slot.uvmax.max(day_uv));
            } else if by_day.len() < 10 {
                by_day.push(MetNoDayAgg { date: dkey, lo: tc, hi: tc, cond, psum: pmm, wmax: ws, pmax: pprob, uvmax: day_uv });
            }
        }
        let first = &ts_arr[0];
        let fdet = first.pointer("/data/instant/details");
        let tc = fdet.and_then(|d| jf(d, "/air_temperature")).filter(|x| x.is_finite()).ok_or("missing met.no air_temperature")?;
        let rh = fdet.and_then(|d| jf(d, "/relative_humidity")).filter(|x| x.is_finite()).ok_or("missing met.no relative_humidity")?;
        let ws = fdet.and_then(|d| jf(d, "/wind_speed")).unwrap_or(0.0);
        let cond = map_sym(
            first
                .pointer("/data/next_1_hours/summary/symbol_code")
                .or_else(|| first.pointer("/data/next_6_hours/summary/symbol_code"))
                .and_then(Value::as_str)
                .unwrap_or("cloudy"),
        );
        let is_day = is_daytime_at(loc.lat, loc.lon, now_ts);
        let dew = fdet.and_then(|d| jf(d, "/dew_point_temperature")).unwrap_or_else(|| dew_point_from_rh(tc, rh));
        let feels = fdet.and_then(|d| jf(d, "/apparent_air_temperature")).unwrap_or_else(|| apparent_temperature_c(tc, rh, ws));
        let uv_now = if is_day { fdet.and_then(|d| jf(d, "/ultraviolet_index_clear_sky")).unwrap_or(0.0).max(0.0) } else { 0.0 };
        let current = CurrentWeather {
            temp_c: tc,
            feels_like_c: feels,
            dew_point_c: dew,
            humidity_pct: rh,
            wind_speed_mps: ws,
            wind_gust_mps: fdet.and_then(|d| jf(d, "/wind_speed_of_gust")),
            wind_dir_deg: fdet.and_then(|d| jf(d, "/wind_from_direction")).unwrap_or(0.0),
            pressure_hpa: fdet.and_then(|d| jf(d, "/air_pressure_at_sea_level")).unwrap_or(1013.25),
            visibility_m: None,
            uv_index: uv_now,
            precip_mm: first.pointer("/data/next_1_hours/details/precipitation_amount").and_then(Value::as_f64).unwrap_or(0.0),
            condition: cond,
            is_day,
            updated_unix: now_ts,
        };
        let daily: Vec<DailyForecast> = by_day
            .into_iter()
            .map(|d| {
                let sun = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok().map(|nd| compute_sun_times(loc.lat, loc.lon, nd));
                DailyForecast {
                    sunrise_unix: sun.and_then(|s| s.sunrise_unix),
                    sunset_unix: sun.and_then(|s| s.sunset_unix),
                    date: d.date,
                    temp_min_c: d.lo,
                    temp_max_c: d.hi,
                    condition: d.cond,
                    precip_prob_pct: d.pmax,
                    precip_sum_mm: d.psum,
                    wind_max_mps: d.wmax,
                    uv_max: d.uvmax,
                }
            })
            .collect();
        let bundle = WeatherBundle {
            location: loc,
            current,
            minutely: Vec::new(),
            hourly,
            daily,
            air_quality: None,
            alerts: Vec::new(),
            source_name: "MET Norway (ECMWF)".into(),
        };
        if let Ok(mut g) = METNO_CACHE.lock() {
            if g.len() > 64 {
                g.clear();
            }
            g.insert(key, (bundle.clone(), last_mod, now));
        }
        Ok(bundle)
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
                && b.hourly.iter().all(|h| (0.0..=100.0).contains(&h.precip_prob_pct))
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
        assert_eq!((epa_pm25_aqi(0.0), epa_pm25_aqi(9.0), epa_pm25_aqi(35.4), epa_pm25_aqi(500.0)), (0, 50, 100, 500));
        assert!(
            (65..=72).contains(&epa_pm25_aqi(19.3))
                && (nws_wind_to_mps(10.0, "wmoUnit:kt") - 5.14444).abs() < 1e-3
                && (nws_wind_to_mps(36.0, "wmoUnit:km_h-1") - 10.0).abs() < 1e-3
        );
        assert_eq!(nws_desc_to_condition("Light Rain and Fog"), Some(WeatherCondition::Rain));
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
        let obs = WeatherClient::parse_metar_array(&metars, 37.80, -122.25, now).expect("nearest KOAK metar");
        assert!((jf(&obs, "/properties/temperature/value").unwrap() - 13.5).abs() < 1e-6);
        assert!((jf(&obs, "/properties/windSpeed/value").unwrap() - 6.0 * 0.514_444).abs() < 1e-4);
        assert!(jf(&obs, "/properties/windDirection/value").is_none());
        assert!((jf(&obs, "/properties/seaLevelPressure/value").unwrap() - 101_920.0).abs() < 1e-3);
        assert!((jf(&obs, "/properties/visibility/value").unwrap() - 8.0 * 1609.344).abs() < 1.0);
        let rh = jf(&obs, "/properties/relativeHumidity/value").unwrap();
        assert!((83.0..=87.0).contains(&rh), "unexpected Magnus RH {rh}");
        assert_eq!(js(&obs, "/properties/textDescription"), Some("Rain"));
        assert_eq!(metar_wx_desc(Some("+TSRA"), None), "Thunderstorm");
        assert_eq!(metar_wx_desc(None, Some(&vec![serde_json::json!({"cover": "OVC"})])), "Overcast");
    }
}
