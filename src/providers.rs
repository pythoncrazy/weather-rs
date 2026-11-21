use crate::astro::{compute_sun_times, is_daytime_at};
use crate::model::{
    AirQuality, AlertSeverity, CurrentWeather, DailyForecast, HourlyForecast, Location,
    MinutelySample, ProviderChoice, WeatherAlert, WeatherBundle, WeatherCondition,
    apparent_temperature_c, dew_point_from_rh,
};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use ureq::Agent;

static NWS_STATIONS: LazyLock<Mutex<HashMap<(i32, i32), String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Circuit-breaker: Unix timestamp until which `api.open-meteo.com` is skipped after a connect/timeout stall.
static OPEN_METEO_BACKOFF_UNTIL: AtomicI64 = AtomicI64::new(0);

fn open_meteo_healthy() -> bool {
    Utc::now().timestamp() >= OPEN_METEO_BACKOFF_UNTIL.load(Ordering::Relaxed)
}
fn note_open_meteo_fail() {
    OPEN_METEO_BACKOFF_UNTIL.store(Utc::now().timestamp() + 45, Ordering::Relaxed);
}

const UA: &str = "weather-rs/0.2.0 (https://github.com/pythoncrazy/weather-rs)";

/// Packed weather sample: `[temp_c, precip_mm_hr, wind_u, wind_v, wind_spd, pressure_hpa]`.
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
        Self {
            lat,
            lon,
            v,
            hourly: [(0, [0.0; 6]); 14],
            hourly_len: 0,
        }
    }
    pub fn at_timestamp(&self, target: Option<i64>) -> Self {
        let Some(ts) = target else { return *self };
        let n = (self.hourly_len as usize).min(14);
        if n == 0 {
            return *self;
        }
        let s = &self.hourly[..n];
        if ts <= s[0].0 {
            return Self { v: s[0].1, ..*self };
        }
        for &[a, b] in s.array_windows::<2>() {
            if ts <= b.0 {
                let t = ((ts - a.0) as f32 / (b.0 - a.0).max(1) as f32).clamp(0.0, 1.0);
                let mut v = std::array::from_fn(|i| a.1[i] + (b.1[i] - a.1[i]) * t);
                v[1] = v[1].max(0.0);
                v[4] = v[4].max(v[2].hypot(v[3]));
                return Self { v, ..*self };
            }
        }
        Self {
            v: s[n - 1].1,
            ..*self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RadarFrameSource {
    LibreWxr(String),
    RainViewer(String),
    NoaaHrrr {
        init: String,
        lead: u32,
        refp: bool,
    },
    #[default]
    GlobalNwp,
}

#[derive(Debug, Clone, Default)]
pub struct RainViewerFrame {
    pub time_unix: i64,
    pub is_nowcast: bool,
    pub source: RadarFrameSource,
}

impl RainViewerFrame {
    pub fn tile_namespace(&self) -> Option<String> {
        match &self.source {
            RadarFrameSource::LibreWxr(_) => Some(format!("lwxr256_{}", self.time_unix)),
            RadarFrameSource::RainViewer(_) => Some(format!("rv256_{}", self.time_unix)),
            RadarFrameSource::NoaaHrrr { init, lead, refp } => Some(format!(
                "hrrr_{}_{init}_{lead:04}",
                if *refp { "refp" } else { "refd" }
            )),
            RadarFrameSource::GlobalNwp => None,
        }
    }
    pub fn max_native_zoom(&self) -> u8 {
        match &self.source {
            RadarFrameSource::LibreWxr(_) | RadarFrameSource::NoaaHrrr { .. } => 8,
            _ => 7,
        }
    }
    pub fn tile_url(&self, host: &str, z: u8, x: u32, y: u32) -> Option<String> {
        match &self.source {
            RadarFrameSource::LibreWxr(p) | RadarFrameSource::RainViewer(p) => {
                Some(format!("{host}{p}/256/{z}/{x}/{y}/2/1_1.png"))
            }
            RadarFrameSource::NoaaHrrr { init, lead, refp } => {
                let p = if *refp { "REFP" } else { "REFD" };
                Some(format!(
                    "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/hrrr::{p}-F{lead:04}-{init}/{z}/{x}/{y}.png"
                ))
            }
            RadarFrameSource::GlobalNwp => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RainViewerManifest {
    pub host: String,
    pub frames: Vec<RainViewerFrame>,
}

#[derive(Clone)]
pub struct WeatherClient {
    std_agent: Agent,
    fast_agent: Agent,
}
impl Default for WeatherClient {
    fn default() -> Self {
        Self::new()
    }
}

fn build_agent(ms: u64) -> Agent {
    Agent::config_builder()
        .timeout_connect(Some(Duration::from_millis(1_100)))
        .timeout_recv_response(Some(Duration::from_millis(ms.min(2_500))))
        .timeout_global(Some(Duration::from_millis(ms)))
        .max_idle_connections_per_host(24)
        .max_idle_connections(64)
        .user_agent(UA)
        .build()
        .into()
}
fn get_val(a: &Agent, url: &str) -> Result<Value, String> {
    let mut r = a.get(url).call().map_err(|e| e.to_string())?;
    r.body_mut().read_json::<Value>().map_err(|e| e.to_string())
}
fn get_nws(a: &Agent, url: &str) -> Option<Value> {
    a.get(url)
        .header("Accept", "application/geo+json")
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()
}
fn js<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    v.pointer(p).and_then(Value::as_str)
}
fn jf(v: &Value, p: &str) -> Option<f64> {
    v.pointer(p).and_then(Value::as_f64)
}
fn ji(v: &Value, p: &str) -> Option<i64> {
    v.pointer(p).and_then(Value::as_i64)
}
fn ja<'a>(v: &'a Value, p: &str) -> &'a [Value] {
    v.pointer(p)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}
fn parse_ts(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        let d = NaiveDateTime::parse_from_str(v.as_str()?, "%Y-%m-%dT%H:%M").ok()?;
        Some(Utc.from_utc_datetime(&d).timestamp())
    })
}
fn rfc3339(v: &Value, p: &str) -> Option<i64> {
    Some(DateTime::parse_from_rfc3339(js(v, p)?).ok()?.timestamp())
}
fn at_f(v: &Value, k: &str, i: usize, d: f64) -> f64 {
    v.get(k)
        .and_then(|a| a.get(i))
        .and_then(Value::as_f64)
        .unwrap_or(d)
}
fn at_i(v: &Value, k: &str, i: usize, d: i64) -> i64 {
    v.get(k)
        .and_then(|a| a.get(i))
        .and_then(parse_ts)
        .unwrap_or(d)
}
fn clamp_lat(la: f64) -> f64 {
    la.clamp(-85.0, 85.0)
}
fn wrap_lon(lo: f64) -> f64 {
    ((lo + 180.0).rem_euclid(360.0)) - 180.0
}
fn coords_csv(c: &[(f64, f64)]) -> (String, String) {
    let lats: Vec<_> = c
        .iter()
        .map(|&(la, _)| format!("{:.2}", clamp_lat(la)))
        .collect();
    let lons: Vec<_> = c
        .iter()
        .map(|&(_, lo)| format!("{:.2}", wrap_lon(lo)))
        .collect();
    (lats.join(","), lons.join(","))
}
fn as_items(v: &Value) -> Vec<&Value> {
    v.as_array().map_or_else(|| vec![v], |a| a.iter().collect())
}
fn mk_bundle(
    loc: Location,
    cur: CurrentWeather,
    min: Vec<MinutelySample>,
    hr: Vec<HourlyForecast>,
    day: Vec<DailyForecast>,
    src: &str,
    now: i64,
) -> WeatherBundle {
    WeatherBundle {
        location: loc,
        current: cur,
        minutely: min,
        hourly: hr,
        daily: day,
        air_quality: None,
        alerts: Vec::new(),
        source_name: src.into(),
        fetched_unix: now,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WifiAp {
    mac: String,
    signal_dbm: i32,
    freq_mhz: Option<u32>,
    locally_administered: bool,
}

fn parse_nmcli_wifi_aps(raw: &str) -> Vec<WifiAp> {
    let mut best: BTreeMap<String, WifiAp> = BTreeMap::new();
    for line in raw.lines() {
        let mut parts = line.rsplitn(3, ':');
        let (Some(freq_s), Some(sig_s), Some(bssid_s)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let mac = bssid_s.replace("\\:", ":").trim().to_ascii_lowercase();
        let bytes: Vec<u8> = mac
            .split(':')
            .filter_map(|h| {
                (h.len() == 2)
                    .then(|| u8::from_str_radix(h, 16).ok())
                    .flatten()
            })
            .collect();
        if bytes.len() != 6
            || bytes.iter().all(|&b| b == 0)
            || bytes.iter().all(|&b| b == 0xff)
            || (bytes[0] & 0x01) != 0
        {
            continue;
        }
        let sig_pct: i32 = sig_s.trim().parse().unwrap_or(0).clamp(0, 100);
        let signal_dbm = -100 + sig_pct / 2;
        let freq_mhz = freq_s
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&f| f > 0);
        let ap = WifiAp {
            mac: mac.clone(),
            signal_dbm,
            freq_mhz,
            locally_administered: (bytes[0] & 0x02) != 0,
        };
        match best.get_mut(&mac) {
            Some(prev) if prev.signal_dbm >= ap.signal_dbm => {}
            _ => {
                best.insert(mac, ap);
            }
        }
    }
    let mut aps: Vec<WifiAp> = best.into_values().collect();
    // Prioritize globally-unique OUI MACs (indexed in BeaconDB) over virtual/locally-administered ones.
    aps.sort_by(|a, b| {
        a.locally_administered
            .cmp(&b.locally_administered)
            .then_with(|| b.signal_dbm.cmp(&a.signal_dbm))
            .then_with(|| a.mac.cmp(&b.mac))
    });
    aps.truncate(80);
    aps
}

fn parse_beacondb_response(v: &Value) -> Option<(f64, f64)> {
    if v.get("fallback").is_some() {
        return None;
    }
    let (lat, lon) = (jf(v, "/location/lat")?, jf(v, "/location/lng")?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    if jf(v, "/accuracy").is_some_and(|a| a > 5_000.0) {
        return None;
    }
    Some((lat, lon))
}

#[cfg(target_os = "linux")]
fn scan_wifi_aps() -> Vec<WifiAp> {
    std::process::Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "BSSID,SIGNAL,FREQ",
            "dev",
            "wifi",
            "list",
            "--rescan",
            "no",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_nmcli_wifi_aps(&s))
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn trigger_wifi_rescan_async() {
    let _ = std::process::Command::new("nmcli")
        .args(["dev", "wifi", "rescan"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "linux"))]
fn scan_wifi_aps() -> Vec<WifiAp> {
    Vec::new()
}

#[cfg(not(target_os = "linux"))]
fn trigger_wifi_rescan_async() {}

fn query_beacondb(agent: &Agent, aps: &[WifiAp]) -> Option<(f64, f64)> {
    if aps.len() < 2 {
        return None;
    }
    let body = serde_json::json!({
        "wifiAccessPoints": aps.iter().map(|ap| {
            let mut m = serde_json::Map::new();
            m.insert("macAddress".into(), Value::String(ap.mac.clone()));
            m.insert("signalStrength".into(), Value::from(ap.signal_dbm));
            if let Some(f) = ap.freq_mhz {
                m.insert("frequency".into(), Value::from(f));
            }
            Value::Object(m)
        }).collect::<Vec<_>>(),
        "considerIp": false,
    });
    let mut resp = agent
        .post("https://api.beacondb.net/v1/geolocate")
        .header("Content-Type", "application/json")
        .send_json(&body)
        .ok()?;
    let v = resp.body_mut().read_json::<Value>().ok()?;
    parse_beacondb_response(&v)
}

impl WeatherClient {
    pub fn new() -> Self {
        Self {
            std_agent: build_agent(6000),
            fast_agent: build_agent(1800),
        }
    }

    pub fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut r = self.std_agent.get(url).call().map_err(|e| e.to_string())?;
        r.body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| e.to_string())
    }

    fn detect_wifi_location(&self, agent: &Agent) -> Option<Location> {
        let cached = scan_wifi_aps();
        let Some((lat, lon)) = query_beacondb(agent, &cached) else {
            // Refresh NM's AP cache asynchronously without blocking the caller for ~7s.
            trigger_wifi_rescan_async();
            return None;
        };
        let mut loc = self.reverse_geocode_with_agent(agent, lat, lon);
        (loc.id, loc.is_current_location) = ("current_location".into(), true);
        Some(loc)
    }

    pub fn detect_current_location(&self) -> Option<(Location, bool)> {
        // Use a fresh short-timeout agent so Wi-Fi/VPN switches never stall on stale pooled sockets.
        let agent: Agent = Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(2_000)))
            .timeout_global(Some(Duration::from_millis(4_000)))
            .max_idle_connections(0)
            .user_agent(UA)
            .build()
            .into();
        // Prefer ~50m Wi-Fi BSSID triangulation (works even behind VPN), fall back to IP geolocation.
        if let Some(loc) = self.detect_wifi_location(&agent) {
            return Some((loc, true));
        }
        const ENDPOINTS: [(&str, &str); 3] = [
            ("https://get.geojs.io/v1/ip/geo.json", "/timezone"),
            ("https://ipwho.is/", "/timezone/id"),
            ("https://ipinfo.io/json", "/timezone"),
        ];
        for (url, tz_p) in ENDPOINTS {
            let Ok(mut resp) = agent.get(url).header("Cache-Control", "no-cache").call() else {
                continue;
            };
            let Ok(v) = resp.body_mut().read_json::<Value>() else {
                continue;
            };
            if v.get("success").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let (lat, lon) = jf(&v, "/latitude")
                .or_else(|| js(&v, "/latitude")?.parse().ok())
                .zip(jf(&v, "/longitude").or_else(|| js(&v, "/longitude")?.parse().ok()))
                .or_else(|| {
                    let (a, b) = js(&v, "/loc")?.split_once(',')?;
                    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
                })
                .unzip();
            if let (Some(la), Some(lo)) = (lat, lon)
                && (-90.0..=90.0).contains(&la)
                && (-180.0..=180.0).contains(&lo)
            {
                let name = js(&v, "/city")
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Current Location");
                let mut l = Location::new(
                    name,
                    js(&v, "/region").unwrap_or(""),
                    js(&v, "/country").unwrap_or(""),
                    js(&v, "/country_code")
                        .or_else(|| js(&v, "/country").filter(|s| s.len() == 2))
                        .unwrap_or(""),
                    la,
                    lo,
                    js(&v, tz_p).unwrap_or(""),
                );
                (l.id, l.is_current_location) = ("current_location".into(), true);
                return Some((l, false));
            }
        }
        None
    }

    pub fn search_locations(&self, query: &str) -> Vec<Location> {
        let q = query.trim();
        if q.len() < 2 {
            return Vec::new();
        }
        let enc = utf8_percent_encode(q, NON_ALPHANUMERIC).to_string();
        let om = format!(
            "https://geocoding-api.open-meteo.com/v1/search?name={enc}&count=8&language=en&format=json"
        );
        if let Ok(val) = get_val(&self.std_agent, &om) {
            let res: Vec<_> = ja(&val, "/results")
                .iter()
                .filter_map(|i| {
                    let mut l = Location::new(
                        js(i, "/name")?,
                        js(i, "/admin1").unwrap_or(""),
                        js(i, "/country").unwrap_or(""),
                        js(i, "/country_code").unwrap_or(""),
                        jf(i, "/latitude")?,
                        jf(i, "/longitude")?,
                        js(i, "/timezone").unwrap_or(""),
                    );
                    l.elevation_m = jf(i, "/elevation");
                    Some(l)
                })
                .collect();
            if !res.is_empty() {
                return res;
            }
        }
        let nom = format!(
            "https://nominatim.openstreetmap.org/search?q={enc}&format=jsonv2&limit=6&addressdetails=1"
        );
        let Ok(val) = get_val(&self.std_agent, &nom) else {
            return Vec::new();
        };
        val.as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let name = js(i, "/name")
                    .filter(|s| !s.is_empty())
                    .or_else(|| js(i, "/display_name").and_then(|d| d.split(',').next()))?;
                let reg = js(i, "/address/state")
                    .or_else(|| js(i, "/address/county"))
                    .unwrap_or("");
                let cc = js(i, "/address/country_code")
                    .unwrap_or("")
                    .to_ascii_uppercase();
                Some(Location::new(
                    name,
                    reg,
                    js(i, "/address/country").unwrap_or(""),
                    &cc,
                    js(i, "/lat")?.parse().ok()?,
                    js(i, "/lon")?.parse().ok()?,
                    "",
                ))
            })
            .collect()
    }

    pub fn reverse_geocode(&self, lat: f64, lon: f64) -> Location {
        self.reverse_geocode_with_agent(&self.fast_agent, lat, lon)
    }

    fn reverse_geocode_with_agent(&self, agent: &Agent, lat: f64, lon: f64) -> Location {
        let url = format!(
            "https://nominatim.openstreetmap.org/reverse?format=jsonv2&lat={lat:.5}&lon={lon:.5}&zoom=12&addressdetails=1"
        );
        if let Ok(v) = get_val(agent, &url)
            && let Some(name) = [
                "/address/city",
                "/address/town",
                "/address/village",
                "/address/municipality",
                "/address/suburb",
                "/address/hamlet",
                "/address/county",
                "/address/state",
                "/name",
            ]
            .iter()
            .find_map(|p| js(&v, p).filter(|s| !s.is_empty()))
        {
            let reg = js(&v, "/address/state")
                .or_else(|| js(&v, "/address/region"))
                .unwrap_or("");
            let cc = js(&v, "/address/country_code")
                .unwrap_or("")
                .to_ascii_uppercase();
            return Location::new(
                name,
                reg,
                js(&v, "/address/country").unwrap_or(""),
                &cc,
                lat,
                lon,
                "",
            );
        }
        // Fast fallback if Nominatim is rate-limited or unreachable.
        let bdc = format!(
            "https://api.bigdatacloud.net/data/reverse-geocode-client?latitude={lat:.5}&longitude={lon:.5}&localityLanguage=en"
        );
        if let Ok(v) = get_val(agent, &bdc)
            && let Some(name) = ["/locality", "/city", "/principalSubdivision"]
                .iter()
                .find_map(|p| js(&v, p).filter(|s| !s.is_empty()))
        {
            return Location::new(
                name,
                js(&v, "/principalSubdivision").unwrap_or(""),
                js(&v, "/countryName").unwrap_or(""),
                &js(&v, "/countryCode").unwrap_or("").to_ascii_uppercase(),
                lat,
                lon,
                "",
            );
        }
        Location::new(format!("{lat:.2}°, {lon:.2}°"), "", "", "", lat, lon, "")
    }

    fn probe_hrrr(&self, init_u: i64, prod: &str, refp: bool) -> Option<(i64, String, bool)> {
        let stamp = DateTime::from_timestamp(init_u, 0)?
            .format("%Y%m%d%H00")
            .to_string();
        let url = format!(
            "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/hrrr::{prod}-F0120-{stamp}/5/5/12.png"
        );
        self.fast_agent
            .head(&url)
            .call()
            .is_ok_and(|r| r.status() == 200)
            .then_some((init_u, stamp, refp))
    }

    fn detect_latest_hrrr_init(&self) -> Option<(i64, String, bool)> {
        let h = (Utc::now().timestamp() / 3600) * 3600;
        let candidates = [
            (h - 3600, "REFP", true),
            (h - 7200, "REFP", true),
            (h - 10800, "REFP", true),
            (h - 3600, "REFD", false),
        ];
        std::thread::scope(|s| {
            let handles: Vec<_> = candidates
                .iter()
                .map(|&(t, p, r)| s.spawn(move || self.probe_hrrr(t, p, r)))
                .collect();
            handles.into_iter().find_map(|h| h.join().ok().flatten())
        })
    }

    fn build_radar_manifest(
        primary: Option<&Value>,
        is_lwxr: bool,
        hrrr: Option<(i64, String, bool)>,
    ) -> Option<RainViewerManifest> {
        let mut host = if is_lwxr {
            "https://api.librewxr.net"
        } else {
            "https://tilecache.rainviewer.com"
        }
        .to_string();
        let (mut frames, now) = (Vec::new(), Utc::now().timestamp());
        let mk_src = |p: &str| {
            if is_lwxr {
                RadarFrameSource::LibreWxr(p.into())
            } else {
                RadarFrameSource::RainViewer(p.into())
            }
        };
        if let Some(v) = primary {
            if let Some(h) = js(v, "/host") {
                host = h.into();
            }
            let push_list = |frames: &mut Vec<RainViewerFrame>, items: &[Value], nowcast: bool| {
                for item in items {
                    if let (Some(t), Some(p)) = (ji(item, "/time"), js(item, "/path")) {
                        frames.push(RainViewerFrame {
                            time_unix: t,
                            is_nowcast: nowcast && t > now + 120,
                            source: mk_src(p),
                        });
                    }
                }
            };
            let past = ja(v, "/radar/past");
            push_list(&mut frames, &past[past.len().saturating_sub(6)..], false);
            push_list(&mut frames, ja(v, "/radar/nowcast"), true);
        }
        let live_ts = frames
            .iter()
            .rfind(|f| !f.is_nowcast)
            .map_or(now, |f| f.time_unix)
            .max(now - 600);
        let offs: &[i64] = if frames.iter().any(|f| f.is_nowcast) {
            &[60, 90, 120, 180, 240, 300, 360, 480, 600, 720]
        } else {
            &[15, 30, 45, 60, 90, 120, 180, 240, 300, 360, 480, 600, 720]
        };
        let mut last_ts = frames.last().map_or(live_ts, |f| f.time_unix);
        if let Some((iu, init, refp)) = hrrr {
            let mut last_l = 0_u32;
            for &o in offs {
                let st = if o <= 90 { 15_i64 } else { 60 };
                let lead = (((((live_ts + o * 60 - iu).max(900) + st * 30) / (st * 60)) * st)
                    as u32)
                    .max(last_l + 15);
                if lead > 1080 {
                    break;
                }
                let vt = iu + lead as i64 * 60;
                if vt > last_ts + 420 {
                    (last_ts, last_l) = (vt, lead);
                    frames.push(RainViewerFrame {
                        time_unix: vt,
                        is_nowcast: true,
                        source: RadarFrameSource::NoaaHrrr {
                            init: init.clone(),
                            lead,
                            refp,
                        },
                    });
                }
            }
        } else if !frames.is_empty() {
            for &o in offs {
                let st = if o <= 90 { 900 } else { 3600 };
                let vt = ((live_ts + o * 60 + st / 2) / st) * st;
                if vt > last_ts + 420 {
                    last_ts = vt;
                    frames.push(RainViewerFrame {
                        time_unix: vt,
                        is_nowcast: true,
                        source: RadarFrameSource::GlobalNwp,
                    });
                }
            }
        }
        (!frames.is_empty()).then_some(RainViewerManifest { host, frames })
    }

    /// Streams the live radar manifest immediately (~160ms), then upgrades with HRRR forecast frames.
    pub fn fetch_radar_manifest_streaming(&self, mut emit: impl FnMut(RainViewerManifest)) {
        std::thread::scope(|s| {
            let hrrr_h = s.spawn(|| self.detect_latest_hrrr_init());
            let (primary, is_lwxr) = match get_val(
                &self.fast_agent,
                "https://api.librewxr.net/public/weather-maps.json",
            ) {
                Ok(v) => (Some(v), true),
                Err(_) => (
                    get_val(
                        &self.std_agent,
                        "https://api.rainviewer.com/public/weather-maps.json",
                    )
                    .ok(),
                    false,
                ),
            };
            if let Some(m) = Self::build_radar_manifest(primary.as_ref(), is_lwxr, None) {
                emit(m);
            }
            if let Ok(Some(hrrr)) = hrrr_h.join()
                && let Some(m) = Self::build_radar_manifest(primary.as_ref(), is_lwxr, Some(hrrr))
            {
                emit(m);
            }
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
        let (lats, lons) = coords_csv(coords);
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lats}&longitude={lons}&current=temperature_2m,precipitation,wind_speed_10m,wind_direction_10m,pressure_msl&hourly=temperature_2m,precipitation,wind_speed_10m,wind_direction_10m,pressure_msl&forecast_hours=14&timeformat=unixtime&wind_speed_unit=ms"
        );
        if open_meteo_healthy() {
            if let Ok(val) = get_val(&self.std_agent, &url) {
                let pts: Vec<_> = as_items(&val)
                    .into_iter()
                    .zip(coords)
                    .filter_map(|(it, &(la, lo))| {
                        let (tc, ws, wd, pr) = (
                            jf(it, "/current/temperature_2m")?,
                            jf(it, "/current/wind_speed_10m").unwrap_or(0.0),
                            jf(it, "/current/wind_direction_10m").unwrap_or(0.0),
                            jf(it, "/current/pressure_msl").unwrap_or(1013.25),
                        );
                        let (u, v) = wind_uv(ws, wd);
                        let (mut hourly, mut hl) = ([(0_i64, [0.0_f32; 6]); 14], 0_usize);
                        if let Some(h) = it.get("hourly") {
                            for (i, tv) in ja(h, "/time").iter().take(14).enumerate() {
                                let Some(ts) = tv.as_i64() else { continue };
                                let hs = at_f(h, "wind_speed_10m", i, ws);
                                let (hu, hv) = wind_uv(hs, at_f(h, "wind_direction_10m", i, wd));
                                hourly[hl] = (
                                    ts,
                                    [
                                        at_f(h, "temperature_2m", i, tc) as f32,
                                        at_f(h, "precipitation", i, 0.0) as f32,
                                        hu,
                                        hv,
                                        hs as f32,
                                        at_f(h, "pressure_msl", i, pr) as f32,
                                    ],
                                );
                                hl += 1;
                            }
                        }
                        let p0 = jf(it, "/current/precipitation").unwrap_or(0.0) as f32;
                        Some(GridWeatherPoint {
                            lat: clamp_lat(la),
                            lon: wrap_lon(lo),
                            v: [tc as f32, p0, u, v, ws as f32, pr as f32],
                            hourly,
                            hourly_len: hl as u8,
                        })
                    })
                    .collect();
                if !pts.is_empty() {
                    return pts;
                }
            } else {
                note_open_meteo_fail();
            }
        }
        std::thread::scope(|s| {
            let hs: Vec<_> = coords
                .iter()
                .take(8)
                .map(|&(la, lo)| {
                    s.spawn(move || {
                        let (la, lo) = (clamp_lat(la), wrap_lon(lo));
                        let url = format!(
                            "https://api.met.no/weatherapi/locationforecast/2.0/compact?lat={la:.3}&lon={lo:.3}"
                        );
                        let v = get_val(&self.fast_agent, &url).ok()?;
                        let d = v.pointer("/properties/timeseries/0/data/instant/details")?;
                        let ws = jf(d, "/wind_speed").unwrap_or(0.0);
                        let (wu, wv) = wind_uv(ws, jf(d, "/wind_from_direction").unwrap_or(0.0));
                        Some(GridWeatherPoint::stationary(
                            la,
                            lo,
                            [
                                jf(d, "/air_temperature")? as f32,
                                0.0,
                                wu,
                                wv,
                                ws as f32,
                                jf(d, "/air_pressure_at_sea_level").unwrap_or(1013.25) as f32,
                            ],
                        ))
                    })
                })
                .collect();
            hs.into_iter()
                .filter_map(|h| h.join().ok().flatten())
                .collect()
        })
    }

    pub fn fetch_aqi_grid_batch(&self, coords: &[(f64, f64)]) -> Vec<(f64, f64, f64)> {
        if coords.is_empty() {
            return Vec::new();
        }
        let (lats, lons) = coords_csv(coords);
        let url = format!(
            "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lats}&longitude={lons}&current=us_aqi"
        );
        let Ok(val) = get_val(&self.std_agent, &url) else {
            return Vec::new();
        };
        as_items(&val)
            .into_iter()
            .zip(coords)
            .filter_map(|(i, &(la, lo))| {
                Some((clamp_lat(la), wrap_lon(lo), jf(i, "/current/us_aqi")?))
            })
            .collect()
    }

    fn fetch_nws_obs(&self, lat: f64, lon: f64) -> Option<Value> {
        if !(17.0..=72.0).contains(&lat) || !(-179.0..=-64.0).contains(&lon) {
            return None;
        }
        let key = ((lat * 20.0).round() as i32, (lon * 20.0).round() as i32);
        let id = if let Some(cached) = NWS_STATIONS.lock().ok().and_then(|m| m.get(&key).cloned()) {
            cached
        } else {
            let pts = get_nws(
                &self.fast_agent,
                &format!("https://api.weather.gov/points/{lat:.4},{lon:.4}"),
            )?;
            let st = js(&pts, "/properties/observationStations")
                .and_then(|u| get_nws(&self.fast_agent, u))?;
            let id = js(&st, "/features/0/properties/stationIdentifier")?.to_string();
            if let Ok(mut m) = NWS_STATIONS.lock() {
                m.insert(key, id.clone());
            }
            id
        };
        get_nws(
            &self.fast_agent,
            &format!("https://api.weather.gov/stations/{id}/observations/latest"),
        )
    }

    fn apply_nws_obs(cur: &mut CurrentWeather, obs: &Value) -> bool {
        let Some(p) = obs.get("properties") else {
            return false;
        };
        if rfc3339(p, "/timestamp").is_some_and(|t| (Utc::now().timestamp() - t).abs() > 4500) {
            return false;
        }
        let val = |k: &str| p.get(k)?.get("value")?.as_f64();
        let w = |k: &str| {
            val(k).map(|v| {
                if p.get(k)
                    .and_then(|o| js(o, "/unitCode"))
                    .unwrap_or("")
                    .contains("km_h-1")
                {
                    v / 3.6
                } else {
                    v
                }
            })
        };
        let mut merged = false;
        if let Some(t) = val("temperature").filter(|t| (-65.0..=60.0).contains(t)) {
            cur.temp_c = t;
            merged = true;
        }
        if let Some(v) = val("dewpoint") {
            cur.dew_point_c = v;
        }
        if let Some(v) = val("relativeHumidity") {
            cur.humidity_pct = v.clamp(0.0, 100.0);
        }
        if let Some(v) = w("windSpeed").filter(|w| (0.0..=90.0).contains(w)) {
            cur.wind_speed_mps = v;
            merged = true;
        }
        if let Some(v) = w("windGust").filter(|w| (0.0..=110.0).contains(w)) {
            cur.wind_gust_mps = Some(v);
        }
        if let Some(v) = val("windDirection") {
            cur.wind_dir_deg = v;
        }
        if let Some(v) = val("seaLevelPressure")
            .or_else(|| val("barometricPressure"))
            .map(|pa| if pa > 2000.0 { pa / 100.0 } else { pa })
            .filter(|p| (870.0..=1085.0).contains(p))
        {
            cur.pressure_hpa = v;
        }
        if let Some(v) = val("visibility").filter(|v| *v > 50.0) {
            cur.visibility_m = Some(v);
        }
        if merged {
            cur.feels_like_c = val("windChill")
                .or_else(|| val("heatIndex"))
                .filter(|t| (-75.0..=75.0).contains(t))
                .unwrap_or_else(|| {
                    apparent_temperature_c(cur.temp_c, cur.humidity_pct, cur.wind_speed_mps)
                });
        }
        merged
    }

    pub fn fetch_weather(
        &self,
        loc: &Location,
        provider: ProviderChoice,
    ) -> Result<WeatherBundle, String> {
        self.fetch_weather_inner(loc, provider, true, None)
    }

    pub fn fetch_weather_streaming(
        &self,
        loc: &Location,
        provider: ProviderChoice,
        mut on_primary: impl FnMut(WeatherBundle),
    ) -> Result<WeatherBundle, String> {
        self.fetch_weather_inner(loc, provider, true, Some(&mut on_primary))
    }

    pub fn fetch_weather_quick(
        &self,
        loc: &Location,
        provider: ProviderChoice,
    ) -> Result<WeatherBundle, String> {
        self.fetch_weather_inner(loc, provider, false, None)
    }

    fn fetch_weather_inner(
        &self,
        loc: &Location,
        provider: ProviderChoice,
        enrich: bool,
        on_primary: Option<&mut dyn FnMut(WeatherBundle)>,
    ) -> Result<WeatherBundle, String> {
        let nordic = (55.0..=72.0).contains(&loc.lat) && (3.0..=32.0).contains(&loc.lon);
        std::thread::scope(|s| {
            let aqi_h = enrich.then(|| s.spawn(|| self.fetch_air_quality(loc.lat, loc.lon)));
            let alerts_h = enrich.then(|| s.spawn(|| self.fetch_nws_alerts(loc.lat, loc.lon)));
            let nws_h = (provider != ProviderChoice::MetNorway)
                .then(|| s.spawn(|| self.fetch_nws_obs(loc.lat, loc.lon)));
            let mut b = match provider {
                ProviderChoice::MetNorway => self.fetch_met_norway(loc),
                ProviderChoice::Auto if nordic || !open_meteo_healthy() => self
                    .fetch_met_norway(loc)
                    .or_else(|_| self.fetch_open_meteo(loc)),
                _ => self
                    .fetch_open_meteo(loc)
                    .inspect_err(|_| note_open_meteo_fail())
                    .or_else(|_| self.fetch_met_norway(loc)),
            }?;
            if let Some(cb) = on_primary {
                cb(b.clone());
            }
            if let Some(obs) = nws_h.and_then(|h| h.join().ok().flatten())
                && Self::apply_nws_obs(&mut b.current, &obs)
            {
                append_source(&mut b.source_name, "NOAA NWS");
            }
            if let Some((aqi, tz)) = aqi_h.and_then(|h| h.join().ok()) {
                if b.location.timezone.is_empty()
                    && let Some(tz) = tz
                {
                    b.location.timezone = tz;
                }
                if let Some(aqi) = aqi {
                    b.air_quality = Some(aqi);
                    append_source(&mut b.source_name, "CAMS AQI");
                }
            }
            if let Some(alerts) = alerts_h.and_then(|h| h.join().ok())
                && !alerts.is_empty()
            {
                b.alerts = alerts;
                append_source(&mut b.source_name, "NOAA NWS");
            }
            Ok(b)
        })
    }

    fn fetch_air_quality(&self, lat: f64, lon: f64) -> (Option<AirQuality>, Option<String>) {
        let url = format!(
            "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lat:.4}&longitude={lon:.4}&timezone=auto&current=us_aqi,european_aqi,pm10,pm2_5,ozone,nitrogen_dioxide"
        );
        let Ok(v) = get_val(&self.fast_agent, &url) else {
            return (None, None);
        };
        let tz = js(&v, "/timezone")
            .filter(|s| !s.is_empty() && *s != "GMT")
            .map(str::to_string);
        let aqi = ji(&v, "/current/us_aqi").map(|u| AirQuality {
            us_aqi: u as i32,
            european_aqi: ji(&v, "/current/european_aqi").map(|x| x as i32),
            pm2_5: jf(&v, "/current/pm2_5").unwrap_or(0.0),
            pm10: jf(&v, "/current/pm10").unwrap_or(0.0),
            ozone: jf(&v, "/current/ozone"),
            no2: jf(&v, "/current/nitrogen_dioxide"),
        });
        (aqi, tz)
    }

    fn fetch_nws_alerts(&self, lat: f64, lon: f64) -> Vec<WeatherAlert> {
        if !(15.0..=72.0).contains(&lat) || !(-179.0..=-60.0).contains(&lon) {
            return Vec::new();
        }
        let Some(v) = get_nws(
            &self.fast_agent,
            &format!("https://api.weather.gov/alerts/active?point={lat:.4},{lon:.4}"),
        ) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut alerts: Vec<_> = ja(&v, "/features")
            .iter()
            .filter_map(|f| {
                let p = f.get("properties")?;
                let event = js(p, "/event")?.to_string();
                if !seen.insert(event.clone()) {
                    return None;
                }
                let severity = match js(p, "/severity").unwrap_or("") {
                    "Extreme" => AlertSeverity::Extreme,
                    "Severe" => AlertSeverity::Severe,
                    "Moderate" => AlertSeverity::Moderate,
                    _ => AlertSeverity::Minor,
                };
                Some(WeatherAlert {
                    event,
                    headline: js(p, "/headline").unwrap_or("").into(),
                    description: js(p, "/description").unwrap_or("").into(),
                    severity,
                    sender: js(p, "/senderName").unwrap_or("NWS").into(),
                    expires: js(p, "/expires").unwrap_or("").into(),
                })
            })
            .collect();
        alerts.sort_by_key(|a| a.severity as u8);
        alerts
    }

    fn fetch_met_norway(&self, loc: &Location) -> Result<WeatherBundle, String> {
        let url = format!(
            "https://api.met.no/weatherapi/locationforecast/2.0/complete?lat={:.4}&lon={:.4}",
            loc.lat, loc.lon
        );
        let root = get_val(&self.std_agent, &url).map_err(|e| format!("MET Norway error: {e}"))?;
        let series = ja(&root, "/properties/timeseries");
        if series.is_empty() {
            return Err("MET Norway returned empty timeseries".into());
        }
        let (now, mut hourly) = (Utc::now().timestamp(), Vec::with_capacity(72));
        #[derive(Default)]
        struct Acc {
            ts: i64,
            lo: f64,
            hi: f64,
            cond: Option<WeatherCondition>,
            psum: f64,
            pprob: f64,
            wmax: f64,
            uv: f64,
        }
        let mut days: BTreeMap<NaiveDate, Acc> = BTreeMap::new();
        let mut first_hourly_idx: Option<usize> = None;
        for (idx, e) in series.iter().enumerate() {
            let (Some(ts), Some(data), Some(d)) = (
                rfc3339(e, "/time"),
                e.get("data"),
                e.pointer("/data/instant/details"),
            ) else {
                continue;
            };
            let (tc, rh, ws, wd) = (
                jf(d, "/air_temperature").unwrap_or(15.0),
                jf(d, "/relative_humidity").unwrap_or(50.0),
                jf(d, "/wind_speed").unwrap_or(0.0),
                jf(d, "/wind_from_direction").unwrap_or(0.0),
            );
            let feels = jf(d, "/apparent_air_temperature")
                .unwrap_or_else(|| apparent_temperature_c(tc, rh, ws));
            let (pr, cld, uv) = (
                jf(d, "/air_pressure_at_sea_level").unwrap_or(1013.25),
                jf(d, "/cloud_area_fraction").unwrap_or(0.0),
                jf(d, "/ultraviolet_index_clear_sky").unwrap_or(0.0),
            );
            let (nb, bh) = [
                ("next_1_hours", 1_i64),
                ("next_6_hours", 6),
                ("next_12_hours", 12),
            ]
            .iter()
            .find_map(|&(k, h)| data.get(k).map(|b| (Some(b), h)))
            .unwrap_or((None, 1));
            let sym = nb
                .and_then(|b| js(b, "/summary/symbol_code"))
                .unwrap_or("partlycloudy_day");
            let cond = WeatherCondition::from_met_symbol(sym);
            let is_day = if sym.ends_with("_day") {
                true
            } else if sym.ends_with("_night") {
                false
            } else {
                is_daytime_at(loc.lat, loc.lon, ts)
            };
            let nd = nb.and_then(|b| b.get("details"));
            let pmm = nd
                .and_then(|x| jf(x, "/precipitation_amount"))
                .unwrap_or(0.0);
            let pprob = nd
                .and_then(|x| jf(x, "/probability_of_precipitation"))
                .unwrap_or(if pmm > 2.0 {
                    85.0
                } else if pmm > 0.2 {
                    55.0
                } else if pmm > 0.0 {
                    25.0
                } else {
                    0.0
                });
            let sh = series
                .get(idx + 1)
                .and_then(|n| rfc3339(n, "/time"))
                .map_or(bh, |nt| ((nt - ts) / 3600).clamp(1, 12));
            if ts >= now - 3500 && hourly.len() < 72 {
                if first_hourly_idx.is_none() {
                    first_hourly_idx = Some(idx);
                }
                hourly.push(HourlyForecast {
                    timestamp_unix: ts,
                    temp_c: tc,
                    feels_like_c: feels,
                    condition: cond,
                    is_day,
                    precip_prob_pct: pprob,
                    precip_mm: pmm / bh as f64,
                    wind_speed_mps: ws,
                    wind_dir_deg: wd,
                    humidity_pct: rh,
                    uv_index: if is_day { uv } else { 0.0 },
                    pressure_hpa: pr,
                    cloud_cover_pct: cld,
                });
            }
            let a = days.entry(loc.local_date(ts)).or_insert_with(|| Acc {
                ts,
                lo: tc,
                hi: tc,
                ..Default::default()
            });
            a.lo =
                a.lo.min(tc)
                    .min(nd.and_then(|x| jf(x, "/air_temperature_min")).unwrap_or(tc));
            a.hi =
                a.hi.max(tc)
                    .max(nd.and_then(|x| jf(x, "/air_temperature_max")).unwrap_or(tc));
            a.psum += pmm * (sh as f64 / bh as f64).min(1.0);
            a.pprob = a.pprob.max(pprob);
            a.wmax = a.wmax.max(ws);
            if is_day {
                a.uv = a.uv.max(uv);
            }
            if a.cond
                .is_none_or(|p| p.severity_rank() < cond.severity_rank())
            {
                a.cond = Some(cond);
            }
        }
        let fd = series[first_hourly_idx.unwrap_or(0)]
            .pointer("/data/instant/details")
            .unwrap_or(&Value::Null);
        let fh = hourly.first().cloned().ok_or("No hourly forecast points")?;
        let fog = jf(fd, "/fog_area_fraction").unwrap_or(0.0);
        let vis = if fog > 50.0 {
            800.0
        } else if fog > 15.0 {
            4000.0
        } else if fh.precip_mm > 4.0 {
            6000.0
        } else if fh.humidity_pct > 93.0 {
            9500.0
        } else {
            16000.0
        };
        let cur = CurrentWeather {
            temp_c: fh.temp_c,
            feels_like_c: fh.feels_like_c,
            condition: fh.condition,
            is_day: fh.is_day,
            humidity_pct: fh.humidity_pct,
            dew_point_c: jf(fd, "/dew_point_temperature")
                .unwrap_or_else(|| dew_point_from_rh(fh.temp_c, fh.humidity_pct)),
            pressure_hpa: fh.pressure_hpa,
            wind_speed_mps: fh.wind_speed_mps,
            wind_gust_mps: jf(fd, "/wind_speed_of_gust"),
            wind_dir_deg: fh.wind_dir_deg,
            visibility_m: Some(vis),
            uv_index: fh.uv_index,
            cloud_cover_pct: fh.cloud_cover_pct,
            precip_mm: fh.precip_mm,
            updated_unix: now,
        };
        let min = self
            .fetch_met_nowcast(loc.lat, loc.lon)
            .unwrap_or_else(|| synth_minutely(&hourly, now));
        let daily = days
            .into_iter()
            .filter(|(d, _)| *d >= loc.local_date(now))
            .take(10)
            .map(|(date, a)| {
                let sun = compute_sun_times(loc.lat, loc.lon, date);
                DailyForecast {
                    date: date.format("%Y-%m-%d").to_string(),
                    timestamp_unix: a.ts,
                    temp_min_c: a.lo,
                    temp_max_c: a.hi,
                    condition: a.cond.unwrap_or(WeatherCondition::PartlyCloudy),
                    precip_prob_pct: a.pprob,
                    precip_sum_mm: a.psum,
                    wind_max_mps: a.wmax,
                    uv_max: a.uv,
                    sunrise_unix: sun.sunrise_unix,
                    sunset_unix: sun.sunset_unix,
                }
            })
            .collect();
        Ok(mk_bundle(
            loc.clone(),
            cur,
            min,
            hourly,
            daily,
            "ECMWF (MET Norway)",
            now,
        ))
    }

    fn fetch_met_nowcast(&self, lat: f64, lon: f64) -> Option<Vec<MinutelySample>> {
        if !(55.0..=72.0).contains(&lat) || !(3.0..=32.0).contains(&lon) {
            return None;
        }
        let url =
            format!("https://api.met.no/weatherapi/nowcast/2.0/complete?lat={lat:.4}&lon={lon:.4}");
        let v = get_val(&self.fast_agent, &url).ok()?;
        let s: Vec<_> = ja(&v, "/properties/timeseries")
            .iter()
            .filter_map(|e| {
                Some(MinutelySample {
                    timestamp_unix: rfc3339(e, "/time")?,
                    precip_mm_hr: jf(e, "/data/instant/details/precipitation_rate")?,
                })
            })
            .collect();
        (!s.is_empty()).then_some(s)
    }

    fn fetch_open_meteo(&self, location: &Location) -> Result<WeatherBundle, String> {
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={:.4}&longitude={:.4}&models=best_match&current=temperature_2m,relative_humidity_2m,dew_point_2m,apparent_temperature,is_day,precipitation,weather_code,cloud_cover,pressure_msl,wind_speed_10m,wind_direction_10m,wind_gusts_10m,visibility,uv_index&minutely_15=precipitation&forecast_minutely_15=6&hourly=temperature_2m,apparent_temperature,precipitation_probability,precipitation,weather_code,pressure_msl,cloud_cover,visibility,wind_speed_10m,wind_direction_10m,uv_index,relative_humidity_2m,is_day&forecast_hours=72&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_sum,precipitation_probability_max,wind_speed_10m_max&wind_speed_unit=ms&timeformat=unixtime&timezone=auto&forecast_days=10",
            location.lat, location.lon
        );
        let val = get_val(&self.std_agent, &url).map_err(|e| format!("Open-Meteo error: {e}"))?;
        let mut loc = location.clone();
        if loc.timezone.is_empty()
            && let Some(tz) = js(&val, "/timezone")
        {
            loc.timezone = tz.into();
        }
        let (now, cur) = (
            Utc::now().timestamp(),
            val.get("current").ok_or("Open-Meteo missing current")?,
        );
        let (tc, rh) = (
            jf(cur, "/temperature_2m").unwrap_or(15.0),
            jf(cur, "/relative_humidity_2m").unwrap_or(50.0),
        );
        let h = val.get("hourly").unwrap_or(&Value::Null);
        let (mut hourly, mut cvis, mut cuv) = (Vec::new(), None, 0.0);
        for (i, tv) in ja(h, "/time").iter().enumerate() {
            let Some(ts) = parse_ts(tv) else { continue };
            if ts >= now - 3500 && hourly.len() < 72 {
                if cvis.is_none() {
                    cvis = h
                        .get("visibility")
                        .and_then(|a| a.get(i))
                        .and_then(Value::as_f64);
                    cuv = at_f(h, "uv_index", i, 0.0);
                }
                hourly.push(HourlyForecast {
                    timestamp_unix: ts,
                    temp_c: at_f(h, "temperature_2m", i, tc),
                    feels_like_c: at_f(h, "apparent_temperature", i, tc),
                    condition: WeatherCondition::from_wmo(at_i(h, "weather_code", i, 0) as i32),
                    is_day: at_i(h, "is_day", i, 1) == 1,
                    precip_prob_pct: at_f(h, "precipitation_probability", i, 0.0),
                    precip_mm: at_f(h, "precipitation", i, 0.0),
                    wind_speed_mps: at_f(h, "wind_speed_10m", i, 0.0),
                    wind_dir_deg: at_f(h, "wind_direction_10m", i, 0.0),
                    humidity_pct: at_f(h, "relative_humidity_2m", i, rh),
                    uv_index: at_f(h, "uv_index", i, 0.0),
                    pressure_hpa: at_f(h, "pressure_msl", i, 1013.25),
                    cloud_cover_pct: at_f(h, "cloud_cover", i, 0.0),
                });
            }
        }
        let current = CurrentWeather {
            temp_c: tc,
            feels_like_c: jf(cur, "/apparent_temperature").unwrap_or(tc),
            condition: WeatherCondition::from_wmo(ji(cur, "/weather_code").unwrap_or(0) as i32),
            is_day: ji(cur, "/is_day").unwrap_or(1) == 1,
            humidity_pct: rh,
            dew_point_c: jf(cur, "/dew_point_2m").unwrap_or_else(|| dew_point_from_rh(tc, rh)),
            pressure_hpa: jf(cur, "/pressure_msl").unwrap_or(1013.25),
            wind_speed_mps: jf(cur, "/wind_speed_10m").unwrap_or(0.0),
            wind_gust_mps: jf(cur, "/wind_gusts_10m"),
            wind_dir_deg: jf(cur, "/wind_direction_10m").unwrap_or(0.0),
            visibility_m: jf(cur, "/visibility").or(cvis),
            uv_index: jf(cur, "/uv_index").unwrap_or(cuv),
            cloud_cover_pct: jf(cur, "/cloud_cover").unwrap_or(0.0),
            precip_mm: jf(cur, "/precipitation").unwrap_or(0.0),
            updated_unix: now,
        };
        let (m, d) = (
            val.get("minutely_15").unwrap_or(&Value::Null),
            val.get("daily").unwrap_or(&Value::Null),
        );
        let min = ja(m, "/time")
            .iter()
            .enumerate()
            .filter_map(|(i, tv)| {
                let ts = parse_ts(tv)?;
                (ts >= now - 900 && ts <= now + 5400).then(|| MinutelySample {
                    timestamp_unix: ts,
                    precip_mm_hr: at_f(m, "precipitation", i, 0.0) * 4.0,
                })
            })
            .collect();
        let daily = ja(d, "/time")
            .iter()
            .enumerate()
            .filter_map(|(i, tv)| {
                let ts = parse_ts(tv)?;
                Some(DailyForecast {
                    date: loc.local_date(ts).format("%Y-%m-%d").to_string(),
                    timestamp_unix: ts,
                    temp_min_c: at_f(d, "temperature_2m_min", i, tc - 3.0),
                    temp_max_c: at_f(d, "temperature_2m_max", i, tc + 3.0),
                    condition: WeatherCondition::from_wmo(at_i(d, "weather_code", i, 0) as i32),
                    precip_prob_pct: at_f(d, "precipitation_probability_max", i, 0.0),
                    precip_sum_mm: at_f(d, "precipitation_sum", i, 0.0),
                    wind_max_mps: at_f(d, "wind_speed_10m_max", i, 0.0),
                    uv_max: at_f(d, "uv_index_max", i, 0.0),
                    sunrise_unix: d.get("sunrise").and_then(|a| a.get(i)).and_then(parse_ts),
                    sunset_unix: d.get("sunset").and_then(|a| a.get(i)).and_then(parse_ts),
                })
            })
            .collect();
        Ok(mk_bundle(
            loc,
            current,
            min,
            hourly,
            daily,
            "Open-Meteo (NOAA / ECMWF / DWD)",
            now,
        ))
    }
}

fn append_source(s: &mut String, tag: &str) {
    if !s.contains(tag) {
        s.push_str(" + ");
        s.push_str(tag);
    }
}

fn synth_minutely(hourly: &[HourlyForecast], now: i64) -> Vec<MinutelySample> {
    if hourly.len() < 2 {
        return Vec::new();
    }
    let (h0, h1, h2) = (
        hourly[0].precip_mm,
        hourly[1].precip_mm,
        hourly.get(2).map_or(hourly[1].precip_mm, |h| h.precip_mm),
    );
    if h0 <= 0.02 && h1 <= 0.02 && h2 <= 0.02 {
        return Vec::new();
    }
    (0..12)
        .map(|step| {
            let f = step as f64 / 11.0;
            let w = (step as f64 * 0.7).sin() * 0.08 * (h0 + h1);
            let rate = if f <= 0.5 {
                h0 + (h1 - h0) * f * 2.0 + w
            } else {
                h1 + (h2 - h1) * (f - 0.5) * 2.0 + w
            };
            MinutelySample {
                timestamp_unix: now + step * 300,
                precip_mm_hr: rate.max(0.0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::default_locations;

    #[test]
    fn live_fetch_weather_and_radar_manifest() {
        let c = WeatherClient::new();
        let b = c
            .fetch_weather(&default_locations()[0], ProviderChoice::Auto)
            .expect("live weather");
        assert!(
            (-60.0..=60.0).contains(&b.current.temp_c)
                && b.hourly.len() >= 24
                && b.daily.len() >= 7
                && b.air_quality.is_some()
        );
        assert!(
            (b.source_name.contains("Open-Meteo") || b.source_name.contains("MET Norway"))
                && b.hourly
                    .iter()
                    .all(|h| (0.0..=100.0).contains(&h.precip_prob_pct))
        );
        assert!(
            c.search_locations("Seattle")
                .iter()
                .any(|r| r.name.contains("Seattle"))
        );
        let m = c.fetch_rainviewer_manifest().expect("manifest");
        let now = Utc::now().timestamp();
        let fut: Vec<_> = m
            .frames
            .iter()
            .filter(|f| f.is_nowcast && f.time_unix > now)
            .collect();
        assert!(m.frames.iter().filter(|f| !f.is_nowcast).count() >= 6 && fut.len() >= 8);
        assert!(fut.iter().map(|f| f.time_unix).max().unwrap_or(0) >= now + 6 * 3600);
    }

    #[test]
    fn wifi_ap_parsing_and_beacondb_validation() {
        let sample = "\
4E\\:D7\\:4A\\:7D\\:84\\:71:90:5180 MHz
14\\:22\\:3B\\:4F\\:C0\\:7A:60:2462 MHz
14\\:22\\:3B\\:4F\\:C0\\:7A:80:2462 MHz
AC\\:39\\:3D\\:19\\:CE\\:03:48:2437 MHz
00\\:00\\:00\\:00\\:00\\:00:99:2412 MHz
FF\\:FF\\:FF\\:FF\\:FF\\:FF:99:2412 MHz
01\\:00\\:5E\\:00\\:00\\:01:80:2412 MHz
bad:line
";
        let aps = parse_nmcli_wifi_aps(sample);
        assert_eq!(aps.len(), 3);
        // Globally-unique OUI MACs sorted before locally-administered (0x4e has bit 1 set), strongest first.
        assert_eq!(aps[0].mac, "14:22:3b:4f:c0:7a");
        assert_eq!(aps[0].signal_dbm, -60); // dedup kept 80% (-60 dBm) over 60% (-70 dBm)
        assert_eq!(aps[0].freq_mhz, Some(2462));
        assert!(!aps[0].locally_administered);
        assert_eq!(aps[1].mac, "ac:39:3d:19:ce:03");
        assert!(!aps[1].locally_administered);
        assert_eq!(aps[2].mac, "4e:d7:4a:7d:84:71");
        assert!(aps[2].locally_administered);

        let good: Value = serde_json::json!({
            "location": { "lat": 37.878076, "lng": -122.266842 },
            "accuracy": 49.0
        });
        assert_eq!(
            parse_beacondb_response(&good),
            Some((37.878076, -122.266842))
        );

        let ip_fallback: Value = serde_json::json!({
            "location": { "lat": 37.89, "lng": -122.26 },
            "accuracy": 25000.0,
            "fallback": "ipf"
        });
        assert_eq!(parse_beacondb_response(&ip_fallback), None);

        let coarse: Value = serde_json::json!({
            "location": { "lat": 37.89, "lng": -122.26 },
            "accuracy": 12000.0
        });
        assert_eq!(parse_beacondb_response(&coarse), None);
    }
}
