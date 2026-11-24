use crate::model::{Settings, WeatherBundle};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
static PENDING_SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
static CONFIG_DIR: LazyLock<PathBuf> = LazyLock::new(|| dirs::config_dir().unwrap_or_else(|| ".".into()).join("weather-rs"));
static CACHE_DIR: LazyLock<PathBuf> = LazyLock::new(|| dirs::cache_dir().unwrap_or_else(|| ".".into()).join("weather-rs"));

pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}
pub fn is_shutting_down() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

fn atomic_write(dir: &Path, name: &str, bytes: &[u8]) {
    let tmp = dir.join(format!("{name}.{}.{}.tmp", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let dst = dir.join(name);
    let res = fs::write(&tmp, bytes).or_else(|e| {
        if e.kind() == ErrorKind::NotFound {
            fs::create_dir_all(dir)?;
            fs::write(&tmp, bytes)
        } else {
            Err(e)
        }
    });
    let moved = res.and_then(|()| {
        fs::rename(&tmp, &dst).or_else(|e| {
            // Windows Defender / indexer or pre-existing read-only target can reject direct rename; retry once after unlink.
            if matches!(e.kind(), ErrorKind::AlreadyExists | ErrorKind::PermissionDenied) {
                let _ = fs::remove_file(&dst);
                fs::rename(&tmp, &dst)
            } else {
                Err(e)
            }
        })
    });
    if moved.is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

pub fn load_settings() -> (Settings, bool) {
    let parsed = fs::read(CONFIG_DIR.join("settings.json")).ok().and_then(|b| serde_json::from_slice::<Settings>(&b).ok());
    parsed.map_or_else(
        || (Settings::default(), true),
        |mut s| {
            s.deduplicate_locations();
            (s, false)
        },
    )
}
fn write_settings_sync(s: &Settings) {
    if let Ok(_g) = SETTINGS_LOCK.lock()
        && let Ok(j) = serde_json::to_vec_pretty(s)
    {
        atomic_write(&CONFIG_DIR, "settings.json", &j);
    }
}
pub fn save_settings(s: &Settings) {
    if cfg!(test) {
        return;
    }
    if is_shutting_down() {
        return write_settings_sync(s);
    }
    let _ = PENDING_SETTINGS.lock().map(|mut slot| *slot = Some(s.clone()));
    std::thread::spawn(flush_pending_settings);
}
pub fn flush_pending_settings() {
    let _ = PENDING_SETTINGS.lock().ok().and_then(|mut g| g.take()).map(|s| write_settings_sync(&s));
}

fn safe_name(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' })
        .collect::<String>()
        .trim_end_matches('.')
        .to_string();
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if s.is_empty()
        || matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
    {
        format!("_{s}")
    } else {
        s
    }
}
fn tile_dir(ns: &str, z: u8) -> PathBuf {
    CACHE_DIR.join("tiles").join(safe_name(ns)).join(z.to_string())
}

pub fn load_cached_bundle(id: &str) -> Option<WeatherBundle> {
    serde_json::from_slice(&fs::read(CACHE_DIR.join("forecasts").join(format!("{}.json", safe_name(id)))).ok()?).ok()
}
pub fn save_cached_bundle(b: &WeatherBundle) {
    if !cfg!(test)
        && let Ok(bytes) = serde_json::to_vec(b)
    {
        atomic_write(&CACHE_DIR.join("forecasts"), &format!("{}.json", safe_name(&b.location.id)), &bytes);
    }
}

pub fn load_cached_tile(ns: &str, z: u8, x: u32, y: u32, max_age: Duration) -> Option<Vec<u8>> {
    let mut f = File::open(tile_dir(ns, z).join(format!("{x}_{y}.bin"))).ok()?;
    let meta = f.metadata().ok()?;
    (SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or(Duration::ZERO) <= max_age).then(|| {
        let mut buf = Vec::with_capacity(meta.len().min(512 * 1024) as usize);
        f.read_to_end(&mut buf).ok().map(|_| buf)
    })?
}
pub fn save_cached_tile(ns: &str, z: u8, x: u32, y: u32, bytes: &[u8]) {
    atomic_write(&tile_dir(ns, z), &format!("{x}_{y}.bin"), bytes);
}
pub fn delete_cached_tile(ns: &str, z: u8, x: u32, y: u32) {
    let _ = fs::remove_file(tile_dir(ns, z).join(format!("{x}_{y}.bin")));
}

pub fn load_nws_station_cache() -> std::collections::HashMap<(i32, i32), Vec<String>> {
    let Ok(bytes) = fs::read(CACHE_DIR.join("nws_stations.json")) else { return Default::default() };
    let Ok(raw) = serde_json::from_slice::<std::collections::HashMap<String, Vec<String>>>(&bytes) else { return Default::default() };
    raw.into_iter()
        .filter_map(|(k, v)| {
            let (a, b) = k.split_once(',')?;
            Some(((a.parse().ok()?, b.parse().ok()?), v))
        })
        .collect()
}
pub fn save_nws_station_cache(map: &std::collections::HashMap<(i32, i32), Vec<String>>) {
    if cfg!(test) {
        return;
    }
    let flat: std::collections::HashMap<String, &Vec<String>> = map.iter().map(|((a, b), v)| (format!("{a},{b}"), v)).collect();
    if let Ok(bytes) = serde_json::to_vec(&flat) {
        atomic_write(&CACHE_DIR, "nws_stations.json", &bytes);
    }
}

pub type AqiDiskEntry = (Option<crate::model::AirQuality>, Option<String>, i64);
pub fn load_aqi_cache(max_age_secs: i64) -> std::collections::HashMap<(i32, i32), AqiDiskEntry> {
    if cfg!(test) {
        return Default::default();
    }
    let Ok(bytes) = fs::read(CACHE_DIR.join("aqi_cache.json")) else { return Default::default() };
    let Ok(raw) = serde_json::from_slice::<std::collections::HashMap<String, AqiDiskEntry>>(&bytes) else { return Default::default() };
    let now = chrono::Utc::now().timestamp();
    raw.into_iter()
        .filter_map(|(k, v)| {
            ((now - v.2).abs() <= max_age_secs).then(|| {
                let (a, b) = k.split_once(',')?;
                Some(((a.parse().ok()?, b.parse().ok()?), v))
            })?
        })
        .collect()
}
pub fn save_aqi_cache(map: &std::collections::HashMap<(i32, i32), AqiDiskEntry>) {
    if cfg!(test) {
        return;
    }
    let flat: std::collections::HashMap<String, &AqiDiskEntry> = map.iter().map(|((a, b), v)| (format!("{a},{b}"), v)).collect();
    if let Ok(bytes) = serde_json::to_vec(&flat) {
        atomic_write(&CACHE_DIR, "aqi_cache.json", &bytes);
    }
}

pub type AlertsDiskEntry = (Vec<crate::model::WeatherAlert>, String, Option<String>, i64);
pub fn load_alerts_cache(max_age_secs: i64) -> std::collections::HashMap<(i32, i32), AlertsDiskEntry> {
    if cfg!(test) {
        return Default::default();
    }
    let Ok(bytes) = fs::read(CACHE_DIR.join("alerts_cache.json")) else { return Default::default() };
    let Ok(raw) = serde_json::from_slice::<std::collections::HashMap<String, AlertsDiskEntry>>(&bytes) else { return Default::default() };
    let now = chrono::Utc::now().timestamp();
    raw.into_iter()
        .filter_map(|(k, v)| {
            ((now - v.3).abs() <= max_age_secs).then(|| {
                let (a, b) = k.split_once(',')?;
                Some(((a.parse().ok()?, b.parse().ok()?), v))
            })?
        })
        .collect()
}
pub fn save_alerts_cache(map: &std::collections::HashMap<(i32, i32), AlertsDiskEntry>) {
    if cfg!(test) {
        return;
    }
    let flat: std::collections::HashMap<String, &AlertsDiskEntry> = map.iter().map(|((a, b), v)| (format!("{a},{b}"), v)).collect();
    if let Ok(bytes) = serde_json::to_vec(&flat) {
        atomic_write(&CACHE_DIR, "alerts_cache.json", &bytes);
    }
}

pub type NwsObsDiskEntry = (serde_json::Value, Option<String>, i64);
pub fn load_nws_obs_cache(max_age_secs: i64) -> std::collections::HashMap<String, NwsObsDiskEntry> {
    if cfg!(test) {
        return Default::default();
    }
    let Ok(bytes) = fs::read(CACHE_DIR.join("nws_obs_cache.json")) else { return Default::default() };
    let Ok(raw) = serde_json::from_slice::<std::collections::HashMap<String, NwsObsDiskEntry>>(&bytes) else { return Default::default() };
    let now = chrono::Utc::now().timestamp();
    raw.into_iter().filter(|(_, v)| (now - v.2).abs() <= max_age_secs).collect()
}
pub fn save_nws_obs_cache(map: &std::collections::HashMap<String, NwsObsDiskEntry>) {
    if cfg!(test) {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(map) {
        atomic_write(&CACHE_DIR, "nws_obs_cache.json", &bytes);
    }
}

pub fn load_cached_manifest(max_age: Duration) -> Option<crate::providers::RainViewerManifest> {
    let p = CACHE_DIR.join("radar_manifest.json");
    let meta = fs::metadata(&p).ok()?;
    let age = SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or(Duration::ZERO);
    (age <= max_age).then(|| serde_json::from_slice(&fs::read(&p).ok()?).ok()).flatten()
}
pub fn save_cached_manifest(m: &crate::providers::RainViewerManifest) {
    if !cfg!(test)
        && let Ok(bytes) = serde_json::to_vec(m)
    {
        atomic_write(&CACHE_DIR, "radar_manifest.json", &bytes);
    }
}

pub fn load_cached_geo_ip(route_ip: &str, max_age: Duration) -> Option<(crate::model::Location, bool)> {
    let p = CACHE_DIR.join("geo_ip.json");
    let meta = fs::metadata(&p).ok()?;
    let age = SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or(Duration::ZERO);
    if age > max_age {
        return None;
    }
    let (cached_ip, loc, precise): (String, crate::model::Location, bool) = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
    (cached_ip == route_ip).then_some((loc, precise))
}
pub fn save_cached_geo_ip(route_ip: &str, loc: &crate::model::Location, precise: bool) {
    if !cfg!(test)
        && let Ok(bytes) = serde_json::to_vec(&(route_ip, loc, precise))
    {
        atomic_write(&CACHE_DIR, "geo_ip.json", &bytes);
    }
}

pub fn spawn_background_cache_prune() {
    std::thread::spawn(|| {
        for _ in 0..30 {
            if is_shutting_down() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let Ok(entries) = fs::read_dir(CACHE_DIR.join("tiles")) else { return };
        let (now, ttl) = (SystemTime::now(), Duration::from_secs(3 * 3600));
        for e in entries.flatten() {
            if is_shutting_down() {
                return;
            }
            let s = e.file_name().to_string_lossy().into_owned();
            let old = e.metadata().and_then(|m| m.modified()).ok().is_some_and(|m| now.duration_since(m).unwrap_or(Duration::ZERO) > ttl);
            if ["lwxr8_", "lwxr512_", "lwxr256_", "rv256_", "hrrr_"].iter().any(|p| s.starts_with(p)) && e.path().is_dir() && old {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn safe_name_cross_platform_sanitization() {
        assert_eq!(safe_name("loc:37.77,-122.42"), "loc_37.77_-122.42");
        assert_eq!(safe_name("foo/bar\\baz?*\"<>|"), "foo_bar_baz______");
        assert_eq!(safe_name("trailing..."), "trailing");
        assert_eq!(safe_name("..."), "_");
        assert_eq!(safe_name("con"), "_con");
        assert_eq!(safe_name("NUL.json"), "_NUL.json");
        assert_eq!(safe_name("COM9"), "_COM9");
        assert_eq!(safe_name("seattle_wa"), "seattle_wa");
    }
}
