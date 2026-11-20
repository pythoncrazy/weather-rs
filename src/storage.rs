use crate::model::{Settings, WeatherBundle};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
static PENDING_SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}
pub fn is_shutting_down() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

fn app_dir(base: Option<PathBuf>) -> PathBuf {
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("weather-rs")
}

fn atomic_write(dir: &Path, name: &str, bytes: &[u8]) {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()));
    let res = fs::write(&tmp, bytes).or_else(|e| {
        if e.kind() == ErrorKind::NotFound {
            fs::create_dir_all(dir)?;
            fs::write(&tmp, bytes)
        } else {
            Err(e)
        }
    });
    if res.is_ok() {
        let _ = fs::rename(&tmp, dir.join(name));
    } else {
        let _ = fs::remove_file(&tmp);
    }
}

pub fn load_settings() -> (Settings, bool) {
    let path = app_dir(dirs::config_dir()).join("settings.json");
    if let Ok(b) = fs::read(path)
        && let Ok(mut s) = serde_json::from_slice::<Settings>(&b)
    {
        s.deduplicate_locations();
        return (s, false);
    }
    (Settings::default(), true)
}

fn write_settings_sync(s: &Settings) {
    if let Ok(_g) = SETTINGS_LOCK.lock()
        && let Ok(json) = serde_json::to_vec_pretty(s)
    {
        atomic_write(&app_dir(dirs::config_dir()), "settings.json", &json);
    }
}

pub fn save_settings(s: &Settings) {
    if is_shutting_down() {
        write_settings_sync(s);
        return;
    }
    if let Ok(mut slot) = PENDING_SETTINGS.lock() {
        *slot = Some(s.clone());
    }
    rayon::spawn(flush_pending_settings);
}

pub fn flush_pending_settings() {
    if let Some(s) = PENDING_SETTINGS.lock().ok().and_then(|mut g| g.take()) {
        write_settings_sync(&s);
    }
}

fn safe_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn load_cached_bundle(id: &str) -> Option<WeatherBundle> {
    let p = app_dir(dirs::cache_dir())
        .join("forecasts")
        .join(format!("{}.json", safe_name(id)));
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

pub fn save_cached_bundle(b: &WeatherBundle) {
    if let Ok(bytes) = serde_json::to_vec(b) {
        atomic_write(
            &app_dir(dirs::cache_dir()).join("forecasts"),
            &format!("{}.json", safe_name(&b.location.id)),
            &bytes,
        );
    }
}

fn tile_dir(ns: &str, z: u8) -> PathBuf {
    app_dir(dirs::cache_dir())
        .join("tiles")
        .join(safe_name(ns))
        .join(z.to_string())
}

pub fn load_cached_tile(ns: &str, z: u8, x: u32, y: u32, max_age: Duration) -> Option<Vec<u8>> {
    let p = tile_dir(ns, z).join(format!("{x}_{y}.bin"));
    let age = SystemTime::now()
        .duration_since(fs::metadata(&p).ok()?.modified().ok()?)
        .unwrap_or(Duration::ZERO);
    (age <= max_age).then(|| fs::read(p).ok()).flatten()
}

pub fn save_cached_tile(ns: &str, z: u8, x: u32, y: u32, bytes: &[u8]) {
    atomic_write(&tile_dir(ns, z), &format!("{x}_{y}.bin"), bytes);
}

pub fn delete_cached_tile(ns: &str, z: u8, x: u32, y: u32) {
    let _ = fs::remove_file(tile_dir(ns, z).join(format!("{x}_{y}.bin")));
}

pub fn spawn_background_cache_prune() {
    std::thread::spawn(|| {
        for _ in 0..30 {
            if is_shutting_down() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let Ok(entries) = fs::read_dir(app_dir(dirs::cache_dir()).join("tiles")) else {
            return;
        };
        let (now, ttl) = (SystemTime::now(), Duration::from_secs(3 * 3600));
        for e in entries.flatten() {
            if is_shutting_down() {
                return;
            }
            let n = e.file_name();
            let s = n.to_string_lossy();
            let is_radar =
                s.starts_with("lwxr256_") || s.starts_with("rv256_") || s.starts_with("hrrr_");
            if is_radar
                && e.path().is_dir()
                && e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .is_some_and(|m| now.duration_since(m).unwrap_or(Duration::ZERO) > ttl)
            {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    });
}
