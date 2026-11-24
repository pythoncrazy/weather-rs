use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use std::f64::consts::PI;

#[derive(Debug, Clone, Copy)]
pub struct SunTimes {
    pub sunrise_unix: Option<i64>,
    pub sunset_unix: Option<i64>,
    pub daylight_seconds: i64,
}
#[derive(Debug, Clone, Copy)]
pub struct MoonInfo {
    pub phase: f64,
    pub illumination_pct: f64,
    pub phase_name: &'static str,
    pub age_days: f64,
}

pub fn compute_sun_times(lat: f64, lon: f64, date: NaiveDate) -> SunTimes {
    let g = 2.0 * PI / 365.0 * (date.ordinal() as f64 - 1.0);
    let (s1, c1, s2, c2) = (g.sin(), g.cos(), (2.0 * g).sin(), (2.0 * g).cos());
    let eq = 229.18 * (0.000075 + 0.001868 * c1 - 0.032077 * s1 - 0.014615 * c2 - 0.040849 * s2);
    let decl =
        0.006918 - 0.399912 * c1 + 0.070257 * s1 - 0.006758 * c2 + 0.000907 * s2 - 0.002697 * (3.0 * g).cos() + 0.00148 * (3.0 * g).sin();
    let lr = lat.clamp(-89.8, 89.8).to_radians();
    let cos_ha = (90.833_f64.to_radians().cos() / (lr.cos() * decl.cos())) - lr.tan() * decl.tan();
    if cos_ha.abs() > 1.0 {
        return SunTimes { sunrise_unix: None, sunset_unix: None, daylight_seconds: if cos_ha < -1.0 { 86_400 } else { 0 } };
    }
    let mid = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight")).timestamp();
    let (noon, ha) = (720.0 - 4.0 * lon - eq, cos_ha.acos().to_degrees() * 4.0);
    let (r, s) = (mid + ((noon - ha) * 60.0).round() as i64, mid + ((noon + ha) * 60.0).round() as i64);
    SunTimes { sunrise_unix: Some(r), sunset_unix: Some(s), daylight_seconds: (s - r).max(0) }
}

/// Returns the astronomical subsolar point `(lat_deg, lon_deg)` where the sun is directly overhead at `ts`.
pub fn subsolar_point(ts: i64) -> (f64, f64) {
    let Some(dt) = Utc.timestamp_opt(ts, 0).single() else { return (0.0, 0.0) };
    let frac_day = (ts.rem_euclid(86_400) as f64) / 86_400.0;
    let g = 2.0 * PI / 365.0 * (dt.ordinal() as f64 - 1.0 + frac_day);
    let (s1, c1, s2, c2) = (g.sin(), g.cos(), (2.0 * g).sin(), (2.0 * g).cos());
    let eq = 229.18 * (0.000075 + 0.001868 * c1 - 0.032077 * s1 - 0.014615 * c2 - 0.040849 * s2);
    let decl =
        0.006918 - 0.399912 * c1 + 0.070257 * s1 - 0.006758 * c2 + 0.000907 * s2 - 0.002697 * (3.0 * g).cos() + 0.00148 * (3.0 * g).sin();
    let utc_mins = frac_day * 1440.0;
    let lon = (((720.0 - utc_mins - eq) / 4.0) + 180.0).rem_euclid(360.0) - 180.0;
    (decl.to_degrees(), lon)
}

pub fn is_daytime_at(lat: f64, lon: f64, ts: i64) -> bool {
    let Some(dt) = Utc.timestamp_opt(ts + (lon * 240.0).round() as i64, 0).single() else { return true };
    let sun = compute_sun_times(lat, lon, dt.date_naive());
    match (sun.sunrise_unix, sun.sunset_unix) {
        (Some(r), Some(s)) if r <= s => ts >= r && ts <= s,
        (Some(r), Some(s)) => ts >= r || ts <= s,
        _ => sun.daylight_seconds > 0,
    }
}

pub fn compute_moon_info(ts: i64) -> MoonInfo {
    const SYNODIC: f64 = 29.530588853;
    let age_days = ((ts as f64 - 947_182_440.0) / 86_400.0).rem_euclid(SYNODIC);
    let phase = (age_days / SYNODIC).clamp(0.0, 1.0);
    let illumination_pct = (0.5 * (1.0 - (2.0 * PI * phase).cos()) * 100.0).clamp(0.0, 100.0);
    let phase_name = match phase {
        p if !(0.03..=0.97).contains(&p) => "New Moon",
        p if p < 0.22 => "Waxing Crescent",
        p if p < 0.28 => "First Quarter",
        p if p < 0.47 => "Waxing Gibbous",
        p if p < 0.53 => "Full Moon",
        p if p < 0.72 => "Waning Gibbous",
        p if p < 0.78 => "Last Quarter",
        _ => "Waning Crescent",
    };
    MoonInfo { phase, illumination_pct, phase_name, age_days }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sun_and_moon_calculations() {
        let sun = compute_sun_times(47.6062, -122.3321, NaiveDate::from_ymd_opt(2026, 3, 20).expect("d"));
        let (r, s) = (sun.sunrise_unix.expect("r"), sun.sunset_unix.expect("s"));
        let m = compute_moon_info(1_795_077_600);
        let ts = |d, h, mn| Utc.with_ymd_and_hms(2026, 11, d, h, mn, 0).single().expect("dt").timestamp();
        assert!(s > r && (11.8..=12.6).contains(&((s - r) as f64 / 3600.0)));
        assert!((0.0..=1.0).contains(&m.phase) && (0.0..=100.0).contains(&m.illumination_pct) && !m.phase_name.is_empty());
        assert!(is_daytime_at(37.872, -122.273, ts(21, 0, 30)) && is_daytime_at(-33.8688, 151.2093, ts(20, 22, 0)));
        assert!(!is_daytime_at(-33.8688, 151.2093, ts(20, 14, 0)));
        let (slat, slon) = subsolar_point(ts(21, 12, 0));
        assert!((-24.0..-18.0).contains(&slat) && slon.abs() < 6.0);
    }
}
