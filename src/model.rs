use chrono::{FixedOffset, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::HashMap};

/// Fast thread-local IANA timezone lookup cache (avoids scanning chrono-tz table dozens of times per frame).
fn parse_tz_cached(name: &str) -> Option<Tz> {
    if name.is_empty() {
        return None;
    }
    thread_local! {
        static TZ_CACHE: RefCell<HashMap<String, Option<Tz>>> = RefCell::new(HashMap::new());
    }
    TZ_CACHE.with(|c| {
        if let Some(&v) = c.borrow().get(name) {
            return v;
        }
        let parsed = name.parse::<Tz>().ok();
        let mut m = c.borrow_mut();
        if m.len() > 64 {
            m.clear();
        }
        m.insert(name.to_string(), parsed);
        parsed
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Location {
    pub id: String,
    pub name: String,
    pub region: String,
    pub country: String,
    pub country_code: String,
    pub lat: f64,
    pub lon: f64,
    pub timezone: String,
    #[serde(default)]
    pub is_current_location: bool,
}

impl Location {
    pub fn make_id(lat: f64, lon: f64) -> String {
        format!("{lat:.3}_{lon:.3}")
    }
    pub fn new(name: impl Into<String>, region: &str, country: &str, cc: &str, lat: f64, lon: f64, tz: &str) -> Self {
        Self {
            id: Self::make_id(lat, lon),
            name: name.into(),
            region: region.into(),
            country: country.into(),
            country_code: cc.into(),
            lat,
            lon,
            timezone: tz.into(),
            is_current_location: false,
        }
    }
    pub fn is_same_place(&self, o: &Self) -> bool {
        let (dlat, dlon) = ((self.lat - o.lat).abs(), (self.lon - o.lon).abs());
        let dlon = dlon.min(360.0 - dlon);
        self.id == o.id
            || (dlat < 0.02 && dlon < 0.02)
            || (!self.name.is_empty() && self.name.eq_ignore_ascii_case(&o.name) && dlat < 0.15 && dlon < 0.15)
    }
    pub fn subtitle(&self) -> String {
        match (self.region.is_empty(), self.country.is_empty()) {
            (false, false) if self.region != self.name => format!("{}, {}", self.region, self.country),
            (true, false) => self.country.clone(),
            (false, true) => self.region.clone(),
            _ => self.country_code.clone(),
        }
    }
    fn local_dt(&self, ts: i64) -> Option<chrono::DateTime<FixedOffset>> {
        let dt = Utc.timestamp_opt(ts, 0).single()?;
        if let Some(tz) = parse_tz_cached(&self.timezone) {
            return Some(dt.with_timezone(&tz).fixed_offset());
        }
        let off =
            FixedOffset::east_opt(((self.lon / 15.0).round() as i32) * 3600).unwrap_or_else(|| FixedOffset::east_opt(0).expect("utc0"));
        Some(dt.with_timezone(&off))
    }
    pub fn format_local_time(&self, ts: i64, h24: bool) -> String {
        self.local_dt(ts).map_or_else(|| "—".into(), |d| d.format(if h24 { "%H:%M" } else { "%-I:%M %p" }).to_string())
    }
    pub fn format_local_hour(&self, ts: i64, h24: bool) -> String {
        self.local_dt(ts).map_or_else(|| "—".into(), |d| d.format(if h24 { "%H:00" } else { "%-I %p" }).to_string())
    }
    pub fn local_date(&self, ts: i64) -> NaiveDate {
        self.local_dt(ts).map_or_else(|| Utc::now().date_naive(), |d| d.date_naive())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WeatherCondition {
    Clear,
    MostlyClear,
    PartlyCloudy,
    Overcast,
    Fog,
    Drizzle,
    Rain,
    HeavyRain,
    FreezingRain,
    Sleet,
    Snow,
    HeavySnow,
    Thunderstorm,
}

impl WeatherCondition {
    pub fn label(self) -> &'static str {
        const L: [&str; 13] = [
            "Clear",
            "Mostly Clear",
            "Partly Cloudy",
            "Cloudy",
            "Fog",
            "Drizzle",
            "Rain",
            "Heavy Rain",
            "Freezing Rain",
            "Sleet",
            "Snow",
            "Heavy Snow",
            "Thunderstorms",
        ];
        L[self as usize]
    }
    /// Maps a WMO 4677 weather code as published by Open-Meteo; an unknown code is `None`, never a guessed condition.
    pub fn from_wmo(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Clear,
            1 => Self::MostlyClear,
            2 => Self::PartlyCloudy,
            3 => Self::Overcast,
            45 | 48 => Self::Fog,
            51 | 53 | 55 => Self::Drizzle,
            56 | 57 | 66 | 67 => Self::FreezingRain,
            61 | 63 | 80 | 81 => Self::Rain,
            65 | 82 => Self::HeavyRain,
            71 | 73 | 77 | 85 => Self::Snow,
            75 | 86 => Self::HeavySnow,
            95 | 96 | 99 => Self::Thunderstorm,
            _ => return None,
        })
    }
    /// Variants are declared from least to most significant weather, so a day's condition is the maximum over its
    /// periods (the same "most severe condition of the day" rule Open-Meteo applies to its daily `weather_code`).
    pub fn most_significant(self, other: Self) -> Self {
        if (other as u8) > (self as u8) { other } else { self }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Every `Option` in a bundle is `None` when the upstream source did not report the value: the UI then shows "—".
/// Nothing is ever defaulted, extrapolated or filled with a typical value.
pub struct CurrentWeather {
    pub temp_c: f64,
    /// Reported by the source, or computed (`feels_like_computed`) from temperature, humidity and wind of that same source.
    pub feels_like_c: Option<f64>,
    #[serde(default)]
    pub feels_like_computed: bool,
    pub condition: WeatherCondition,
    pub is_day: bool,
    pub humidity_pct: f64,
    pub dew_point_c: f64,
    pub pressure_hpa: Option<f64>,
    pub wind_speed_mps: f64,
    pub wind_gust_mps: Option<f64>,
    /// `None` for calm or variable ("VRB") wind and when the source reports no direction.
    pub wind_dir_deg: Option<f64>,
    pub visibility_m: Option<f64>,
    /// The station reported visibility as a lower bound (METAR "6+" / "10+" statute miles), not as a measured distance.
    #[serde(default)]
    pub visibility_at_least: bool,
    pub uv_index: Option<f64>,
    pub precip_mm: Option<f64>,
    pub updated_unix: i64,
}
/// Length of the period each `MinutelySample` covers; Open-Meteo stamps a sample with the END of its 15 minutes.
pub const MINUTELY_STEP_SECS: i64 = 900;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinutelySample {
    pub timestamp_unix: i64,
    pub precip_mm_hr: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyForecast {
    pub timestamp_unix: i64,
    pub temp_c: f64,
    pub condition: WeatherCondition,
    pub is_day: bool,
    pub precip_prob_pct: Option<f64>,
    pub precip_mm: f64,
    pub wind_speed_mps: f64,
    pub wind_dir_deg: f64,
    pub pressure_hpa: Option<f64>,
    #[serde(default)]
    pub uv_index: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyForecast {
    pub date: String,
    pub temp_min_c: f64,
    pub temp_max_c: f64,
    pub condition: WeatherCondition,
    pub precip_prob_pct: Option<f64>,
    pub precip_sum_mm: Option<f64>,
    pub wind_max_mps: Option<f64>,
    pub uv_max: Option<f64>,
    pub sunrise_unix: Option<i64>,
    pub sunset_unix: Option<i64>,
    /// The source only covers the REST of this day (MET Norway has no data for hours already past), so the
    /// low / high / precipitation describe the remaining hours and must not be presented as the whole day's values.
    #[serde(default)]
    pub partial: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirQuality {
    /// Upstream US AQI exactly as published; the app never recomputes or adjusts it.
    pub us_aqi: i32,
    pub pm2_5: Option<f64>,
}
impl AirQuality {
    /// US EPA AQI category and the EPA's own description of it.
    pub fn info(&self) -> (&'static str, &'static str) {
        match self.us_aqi {
            ..=50 => ("Good", "EPA: air pollution poses little or no risk."),
            51..=100 => ("Moderate", "EPA: acceptable; may be a risk for unusually sensitive people."),
            101..=150 => ("Sensitive", "EPA: sensitive groups may experience health effects."),
            151..=200 => ("Unhealthy", "EPA: some of the general public may experience health effects."),
            201..=300 => ("Very Unhealthy", "EPA health alert: increased risk of health effects for everyone."),
            _ => ("Hazardous", "EPA health warning of emergency conditions."),
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertSeverity {
    Extreme,
    Severe,
    Moderate,
    Minor,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherAlert {
    pub event: String,
    pub headline: String,
    pub description: String,
    pub severity: AlertSeverity,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherBundle {
    pub location: Location,
    pub current: CurrentWeather,
    pub minutely: Vec<MinutelySample>,
    pub hourly: Vec<HourlyForecast>,
    pub daily: Vec<DailyForecast>,
    pub air_quality: Option<AirQuality>,
    pub alerts: Vec<WeatherAlert>,
    pub source_name: String,
    /// Wall-clock time the current conditions were fetched (`0` = unknown).
    #[serde(default)]
    pub fetched_unix: i64,
    /// Wall-clock time of the last FULL fetch (hourly / minutely / 10-day / air quality / alerts). `None` = never.
    #[serde(default)]
    pub full_fetched_unix: Option<i64>,
    /// Wall-clock time the alert feed last answered; `None` when it has not answered (so `alerts` says nothing).
    #[serde(default)]
    pub alerts_fetched_unix: Option<i64>,
    /// Current conditions come from the lean sidebar batch: forecast detail, air quality and alerts are whatever the
    /// last full fetch (`full_fetched_unix`) returned, so the bundle is refreshed in full as soon as it is selected.
    #[serde(default)]
    pub lean: bool,
    /// Terrain elevation of the forecast point as reported by the forecast provider.
    #[serde(default)]
    pub elevation_m: Option<f64>,
    /// UV values in this bundle are MET Norway's clear-sky UV index rather than a cloud-adjusted UV index.
    #[serde(default)]
    pub uv_clear_sky: bool,
    /// Precipitation the forecast provider reports for the 24 whole hours before the current hour and for the 24
    /// hours after it, in mm. `None` unless the provider returned all 24 hourly values of that window.
    #[serde(default)]
    pub precip_last_24h_mm: Option<f64>,
    #[serde(default)]
    pub precip_next_24h_mm: Option<f64>,
    /// `BUNDLE_SCHEMA` of the build that wrote the bundle; older cached bundles may hold defaulted values and are discarded.
    #[serde(default)]
    pub schema: u8,
}
/// Bumped whenever the meaning of stored values changes (2: no defaulted values, upstream AQI, per-source observation merge).
pub const BUNDLE_SCHEMA: u8 = 2;
/// Normal refresh cadence: data older than this is refreshed and, if that fails, its age is shown.
pub const REFRESH_INTERVAL_SECS: i64 = 600;
/// A 3-hourly pressure change smaller than this is called "Steady".
pub const PRESSURE_STEADY_HPA: f64 = 0.8;

impl WeatherBundle {
    /// Fetch time of the OLDEST part on display: the current block, or the detail kept from the last full fetch.
    pub fn data_time_unix(&self) -> i64 {
        self.full_fetched_unix.map_or(self.fetched_unix, |f| f.min(self.fetched_unix))
    }
    /// True when selecting this location must trigger a full fetch (lean bundle, or full data older than the refresh interval).
    pub fn needs_full_refresh(&self, now: i64) -> bool {
        self.lean || self.full_fetched_unix.is_none_or(|t| (now - t).abs() > REFRESH_INTERVAL_SECS)
    }
    /// Folds a lean sidebar update (`self`) onto the previous bundle. Current conditions come from the update; the
    /// hourly / minutely / daily detail, air quality and alerts stay as the last full fetch returned them, together
    /// with the time of that fetch. Nothing is synthesized: a value neither bundle has stays `None`.
    pub fn merge_lean_onto(mut self, prev: &Self) -> Self {
        self.lean = true;
        self.full_fetched_unix = prev.full_fetched_unix;
        self.air_quality = prev.air_quality.clone();
        (self.alerts, self.alerts_fetched_unix) = (prev.alerts.clone(), prev.alerts_fetched_unix);
        self.elevation_m = self.elevation_m.or(prev.elevation_m);
        let (same_uv, c, pc) = (self.uv_clear_sky == prev.uv_clear_sky, &mut self.current, &prev.current);
        // If the lean source omitted a value, keep the previous one only while it is still current (one refresh interval)
        // and cannot contradict the new reading (a kept gust is never below the new mean wind).
        if prev.fetched_unix > 0 && (self.fetched_unix - prev.fetched_unix).abs() <= REFRESH_INTERVAL_SECS {
            if c.uv_index.is_none() && same_uv {
                c.uv_index = pc.uv_index;
            }
            if c.visibility_m.is_none() {
                (c.visibility_m, c.visibility_at_least) = (pc.visibility_m, pc.visibility_at_least);
            }
            if c.wind_gust_mps.is_none() {
                c.wind_gust_mps = pc.wind_gust_mps.filter(|g| *g >= c.wind_speed_mps);
            }
        }
        let own_detail = !self.hourly.is_empty();
        if !own_detail {
            (self.hourly, self.minutely) = (prev.hourly.clone(), prev.minutely.clone());
            let mut daily = prev.daily.clone();
            if !same_uv {
                self.hourly.iter_mut().for_each(|h| h.uv_index = None);
                daily.iter_mut().for_each(|d| d.uv_max = None);
            }
            if let Some(d0) = self.daily.first() {
                daily.retain(|d| d.date >= d0.date);
                match daily.first_mut().filter(|d| d.date == d0.date) {
                    Some(slot) if slot.partial == d0.partial => {
                        (slot.temp_max_c, slot.temp_min_c, slot.condition) = (d0.temp_max_c, d0.temp_min_c, d0.condition);
                    }
                    Some(slot) => *slot = d0.clone(),
                    None => daily.insert(0, d0.clone()),
                }
            }
            self.daily = daily;
        }
        // Attribution names exactly the sources whose data is in the merged bundle.
        for (i, tag) in prev.source_name.split(" + ").enumerate() {
            let used = match tag {
                _ if i == 0 => !own_detail,
                t if t.ends_with("obs") => false,
                t if t.ends_with("alerts") => !self.alerts.is_empty(),
                t if t.ends_with("AQI") => self.air_quality.is_some(),
                _ => true,
            };
            if used && !tag.is_empty() && !self.source_name.split(" + ").any(|t| t == tag) {
                self.source_name = format!("{} + {tag}", self.source_name);
            }
        }
        self
    }
    /// When the alert feed did not answer for this bundle (`alerts_fetched_unix == None`), a failed request must not
    /// read as "no alerts": keep the alerts last known for this location, with the time they were fetched.
    pub fn keep_known_alerts(&mut self, prev: &Self) {
        if self.alerts_fetched_unix.is_some() {
            return;
        }
        (self.alerts, self.alerts_fetched_unix) = (prev.alerts.clone(), prev.alerts_fetched_unix);
        for tag in prev.source_name.split(" + ").filter(|t| t.ends_with("alerts")) {
            if !self.alerts.is_empty() && !self.source_name.split(" + ").any(|t| t == tag) {
                self.source_name = format!("{} + {tag}", self.source_name);
            }
        }
    }
    /// Highest real UV value left in the location's local day: the current value plus every later hourly sample of
    /// today. `None` when the hourly series has no UV or stops before the end of the day (the peak is then unknown).
    pub fn uv_peak_rest_of_day(&self, loc: &Location, now: i64) -> Option<(i64, f64)> {
        let today = loc.local_date(now);
        if !self.hourly.iter().any(|h| h.uv_index.is_some() && loc.local_date(h.timestamp_unix) > today) {
            return None;
        }
        let later = self.hourly.iter().filter(|h| h.timestamp_unix > now && loc.local_date(h.timestamp_unix) == today);
        later
            .filter_map(|h| Some((h.timestamp_unix, h.uv_index?)))
            .chain(self.current.uv_index.map(|u| (now, u)))
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
    }
    /// Pressure change over the next 3 hours taken from the hourly model series alone (never model minus observation).
    pub fn pressure_trend_3h(&self, now: i64) -> Option<f64> {
        let h0 = self.hourly.iter().min_by_key(|h| (h.timestamp_unix - now).abs()).filter(|h| (h.timestamp_unix - now).abs() <= 1800)?;
        let h3 = self.hourly.iter().find(|h| h.timestamp_unix == h0.timestamp_unix + 10_800)?;
        Some(h3.pressure_hpa? - h0.pressure_hpa?)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum TempUnit {
    #[default]
    Celsius,
    Fahrenheit,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum WindUnit {
    #[default]
    KmH,
    Mph,
    Ms,
    Knots,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum PressureUnit {
    #[default]
    HPa,
    InHg,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum PrecipUnit {
    #[default]
    Mm,
    Inches,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ProviderChoice {
    #[default]
    Auto,
    MetNorway,
    OpenMeteo,
}
impl ProviderChoice {
    pub fn label(self) -> &'static str {
        const L: [&str; 3] = [
            "Auto (Open-Meteo Best Match + NOAA NWS + MET Norway)",
            "MET Norway (ECMWF — Nordic Official)",
            "Open-Meteo (HRRR / ICON / AROME / ECMWF)",
        ];
        L[self as usize]
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum ThemeMode {
    #[default]
    DynamicSky,
    Dark,
    Light,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub temp_unit: TempUnit,
    pub wind_unit: WindUnit,
    pub pressure_unit: PressureUnit,
    pub precip_unit: PrecipUnit,
    pub use_24h_time: bool,
    #[serde(default)]
    pub provider: ProviderChoice,
    #[serde(default)]
    pub theme_mode: ThemeMode,
    pub animate_weather: bool,
    /// Forecast map: when `true`, the frames between hourly ECCC model outputs are motion-interpolated by the app
    /// (smooth playback, labelled "Interpolated Forecast"); when `false`, only frames the models actually issued are
    /// shown and the map steps from hour to hour.
    #[serde(default = "default_true")]
    pub interpolate_forecast: bool,
    pub saved_locations: Vec<Location>,
    pub selected_location_id: Option<String>,
}
const fn default_true() -> bool {
    true
}
impl Default for Settings {
    fn default() -> Self {
        let saved_locations = default_locations();
        let selected_location_id = saved_locations.first().map(|l| l.id.clone());
        Self {
            temp_unit: TempUnit::default(),
            wind_unit: WindUnit::default(),
            pressure_unit: PressureUnit::default(),
            precip_unit: PrecipUnit::default(),
            use_24h_time: false,
            provider: ProviderChoice::default(),
            theme_mode: ThemeMode::default(),
            animate_weather: true,
            interpolate_forecast: true,
            saved_locations,
            selected_location_id,
        }
    }
}
impl Settings {
    pub fn deduplicate_locations(&mut self) {
        let (mut seen, mut out) = (false, Vec::with_capacity(self.saved_locations.len()));
        for loc in self.saved_locations.drain(..) {
            if loc.is_current_location {
                if !seen {
                    seen = true;
                    out.insert(0, loc);
                }
            } else if !out.iter().any(|e| !e.is_current_location && e.is_same_place(&loc)) {
                out.push(loc);
            }
        }
        self.saved_locations = out;
        if self.selected_location_id.as_ref().is_none_or(|id| !self.saved_locations.iter().any(|l| &l.id == id)) {
            self.selected_location_id = self.saved_locations.first().map(|l| l.id.clone());
        }
    }
    pub fn convert_temp(&self, c: f64) -> f64 {
        if self.temp_unit == TempUnit::Fahrenheit { c * 9.0 / 5.0 + 32.0 } else { c }
    }
    pub fn format_temp(&self, c: f64) -> String {
        format!("{}°", self.format_temp_bare(c))
    }
    pub fn format_temp_bare(&self, c: f64) -> String {
        let r = self.convert_temp(c).round();
        format!("{:.0}", if r.abs() < 0.5 { 0.0 } else { r })
    }
    pub fn format_wind(&self, mps: f64) -> String {
        let (v, u) = self.wind_value_and_unit(mps);
        format!("{v} {u}")
    }
    pub fn wind_value_and_unit(&self, mps: f64) -> (String, &'static str) {
        match self.wind_unit {
            WindUnit::KmH => (format!("{:.0}", mps * 3.6), "km/h"),
            WindUnit::Mph => (format!("{:.0}", mps * 2.23694), "mph"),
            WindUnit::Ms => (format!("{mps:.1}"), "m/s"),
            WindUnit::Knots => (format!("{:.0}", mps * 1.94384), "kt"),
        }
    }
    pub fn format_pressure(&self, hpa: f64) -> String {
        match self.pressure_unit {
            PressureUnit::HPa => format!("{hpa:.0} hPa"),
            PressureUnit::InHg => format!("{:.2} inHg", hpa * 0.02953),
        }
    }
    pub fn format_precip(&self, mm: f64) -> String {
        let mm = mm.max(0.0);
        match self.precip_unit {
            PrecipUnit::Mm if (0.05..0.95).contains(&mm) => format!("{mm:.1} mm"),
            PrecipUnit::Mm => format!("{:.0} mm", mm.round()),
            PrecipUnit::Inches => format!("{:.2}\"", mm / 25.4),
        }
    }
    /// `at_least` renders a reported lower bound such as METAR "6+" SM as "10+ km" / "6+ mi", never as an exact distance.
    pub fn format_visibility(&self, m: f64, at_least: bool) -> String {
        let (m, plus) = (m.max(0.0), if at_least { "+" } else { "" });
        let (v, u, small) = if self.wind_unit == WindUnit::Mph {
            (m / 1609.344, "mi", format!("{:.0}{plus} ft", m * 3.280_84))
        } else {
            (m / 1000.0, "km", format!("{m:.0}{plus} m"))
        };
        if v < 0.05 {
            small
        } else if v < 0.95 {
            format!("{v:.1}{plus} {u}")
        } else {
            format!("{:.0}{plus} {u}", v.round())
        }
    }
}

pub fn compass_direction(deg: f64) -> &'static str {
    const D: [&str; 16] = ["N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW"];
    D[(((deg.rem_euclid(360.0)) / 22.5).round() as usize) % 16]
}
/// The UV index as displayed: rounded half-up to a whole number (WHO reports and categorises whole index values).
pub fn uv_display(uv: f64) -> String {
    format!("{:.0}", uv.max(0.0).round())
}
/// WHO exposure category of the ROUNDED index, so the label always agrees with the number shown next to it.
pub fn uv_category(uv: f64) -> &'static str {
    match uv.max(0.0).round() {
        u if u < 3.0 => "Low",
        u if u < 6.0 => "Moderate",
        u if u < 8.0 => "High",
        u if u < 11.0 => "Very High",
        _ => "Extreme",
    }
}
pub fn dew_point_from_rh(t: f64, rh: f64) -> f64 {
    let a = (rh.clamp(1.0, 100.0) / 100.0).ln() + (17.625 * t) / (243.04 + t);
    (243.04 * a) / (17.625 - a)
}
/// Relative humidity (%) from temperature and dew point by the Magnus formula (same constants as `dew_point_from_rh`).
pub fn rh_from_dew_point(t: f64, td: f64) -> f64 {
    (100.0 * ((17.625 * td) / (243.04 + td) - (17.625 * t) / (243.04 + t)).exp()).clamp(0.0, 100.0)
}
/// US NWS apparent temperature: the JAG/TI wind-chill index at or below 10 °C with wind above 4.8 km/h, the Rothfusz
/// heat index at or above 26.7 °C with humidity of 40 % or more, and otherwise the air temperature itself.
pub fn apparent_temperature_c(t: f64, rh: f64, ws: f64) -> f64 {
    let (rh, kmh) = (rh.clamp(1.0, 100.0), ws.max(0.0) * 3.6);
    if t <= 10.0 && kmh > 4.8 {
        13.12 + 0.6215 * t - 11.37 * kmh.powf(0.16) + 0.3965 * t * kmh.powf(0.16)
    } else if t >= 26.7 && rh >= 40.0 {
        let tf = t * 9.0 / 5.0 + 32.0;
        let hi = -42.379 + 2.049_015_23 * tf + 10.143_331_27 * rh - 0.224_755_41 * tf * rh - 6.837_83e-3 * tf * tf - 5.481_717e-2 * rh * rh
            + 1.228_74e-3 * tf * tf * rh
            + 8.528_2e-4 * tf * rh * rh
            - 1.99e-6 * tf * tf * rh * rh;
        (hi - 32.0) * 5.0 / 9.0
    } else {
        t
    }
}

pub fn default_locations() -> Vec<Location> {
    [
        ("New York", "New York", "United States", "US", 40.7128, -74.0060, "America/New_York"),
        ("London", "England", "United Kingdom", "GB", 51.5074, -0.1278, "Europe/London"),
        ("Tokyo", "Tokyo", "Japan", "JP", 35.6762, 139.6503, "Asia/Tokyo"),
        ("Oslo", "Oslo", "Norway", "NO", 59.9139, 10.7522, "Europe/Oslo"),
    ]
    .into_iter()
    .map(|(n, r, c, cc, la, lo, tz)| Location::new(n, r, c, cc, la, lo, tz))
    .collect()
}

/// A minimal, fully real-valued bundle for unit tests (no hourly / daily detail, no air quality, no alerts).
#[cfg(test)]
pub(crate) fn test_bundle(fetched: i64) -> WeatherBundle {
    let current = CurrentWeather {
        temp_c: 15.0,
        feels_like_c: Some(15.0),
        feels_like_computed: false,
        condition: WeatherCondition::Overcast,
        is_day: true,
        humidity_pct: 50.0,
        dew_point_c: 4.7,
        pressure_hpa: Some(1015.0),
        wind_speed_mps: 3.0,
        wind_gust_mps: Some(6.0),
        wind_dir_deg: Some(270.0),
        visibility_m: Some(20_000.0),
        visibility_at_least: false,
        uv_index: Some(5.0),
        precip_mm: Some(0.0),
        updated_unix: fetched,
    };
    WeatherBundle {
        location: Location::new("Berkeley", "CA", "United States", "US", 37.87, -122.27, "America/Los_Angeles"),
        current,
        minutely: Vec::new(),
        hourly: Vec::new(),
        daily: Vec::new(),
        air_quality: None,
        alerts: Vec::new(),
        source_name: "Open-Meteo (best-match model)".into(),
        fetched_unix: fetched,
        full_fetched_unix: None,
        alerts_fetched_unix: None,
        lean: false,
        elevation_m: Some(60.0),
        uv_clear_sky: false,
        precip_last_24h_mm: None,
        precip_next_24h_mm: None,
        schema: BUNDLE_SCHEMA,
    }
}
#[cfg(test)]
pub(crate) fn test_hour(ts: i64, uv: Option<f64>, pressure: Option<f64>) -> HourlyForecast {
    HourlyForecast {
        timestamp_unix: ts,
        temp_c: 14.0,
        condition: WeatherCondition::Clear,
        is_day: true,
        precip_prob_pct: None,
        precip_mm: 0.0,
        wind_speed_mps: 2.0,
        wind_dir_deg: 270.0,
        pressure_hpa: pressure,
        uv_index: uv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_units_symbols_and_compass() {
        let mut s = Settings { temp_unit: TempUnit::Fahrenheit, ..Default::default() };
        assert_eq!((s.format_temp(0.0), s.format_temp(100.0)), ("32°".into(), "212°".into()));
        s.temp_unit = TempUnit::Celsius;
        assert_eq!(
            (s.format_temp(21.4), s.format_precip(2.5), s.format_visibility(200.0, false)),
            ("21°".into(), "3 mm".into(), "0.2 km".into())
        );
        assert_eq!(
            (WeatherCondition::from_wmo(0), WeatherCondition::from_wmo(95), WeatherCondition::from_wmo(75), WeatherCondition::from_wmo(4)),
            (Some(WeatherCondition::Clear), Some(WeatherCondition::Thunderstorm), Some(WeatherCondition::HeavySnow), None),
        );
        assert_eq!(WeatherCondition::Overcast.most_significant(WeatherCondition::HeavyRain), WeatherCondition::HeavyRain);
        assert_eq!(WeatherCondition::Snow.most_significant(WeatherCondition::Fog), WeatherCondition::Snow);
        assert_eq!([0.0, 90.0, 225.0, 359.0].map(compass_direction), ["N", "E", "SW", "N"]);
        let brk = Location::new("Berkeley", "CA", "US", "US", 37.872, -122.273, "");
        assert!(!brk.is_same_place(&Location::new("Oakland", "CA", "US", "US", 37.804, -122.271, "")));
        assert!(brk.is_same_place(&Location::new("Berkeley", "CA", "US", "US", 37.868, -122.269, "")));
        // NWS apparent temperature: the air temperature itself between the wind-chill and heat-index regimes.
        assert_eq!(apparent_temperature_c(11.7, 55.0, 2.2), 11.7);
        assert!(apparent_temperature_c(5.0, 80.0, 7.2) < 3.0 && apparent_temperature_c(32.0, 70.0, 1.0) > 36.0);
        assert!(
            (rh_from_dew_point(13.5, 11.0) - 85.0).abs() < 2.0
                && (dew_point_from_rh(13.5, rh_from_dew_point(13.5, 11.0)) - 11.0).abs() < 0.01
        );
        // A reported lower bound ("6+" / "10+" statute miles) is shown as a bound, in either unit; tiny values are not clamped.
        let (six_sm, ten_sm) = (6.0 * 1609.344, 10.0 * 1609.344);
        assert_eq!((s.format_visibility(six_sm, true), s.format_visibility(ten_sm, true)), ("10+ km".into(), "16+ km".into()));
        assert_eq!((s.format_visibility(120_000.0, false), s.format_visibility(40.0, false)), ("120 km".into(), "40 m".into()));
        s.wind_unit = WindUnit::Mph;
        assert_eq!((s.format_visibility(six_sm, true), s.format_visibility(ten_sm, true)), ("6+ mi".into(), "10+ mi".into()));
        assert_eq!(
            (s.format_visibility(200.0, false), s.format_temp(-0.2), s.format_temp_bare(-0.2)),
            ("0.1 mi".into(), "0°".into(), "0".into())
        );
    }

    #[test]
    fn uv_category_matches_the_rounded_value_on_display() {
        // 7.65 is shown as "8", so it is "Very High" (not "High"); 2.95 is shown as "3", so it is "Moderate" (not "Low").
        let shown = |uv: f64| (uv_display(uv), uv_category(uv));
        assert_eq!(shown(7.65), ("8".into(), "Very High"));
        assert_eq!(shown(2.95), ("3".into(), "Moderate"));
        assert_eq!(shown(2.5), ("3".into(), "Moderate"));
        assert_eq!(shown(2.49), ("2".into(), "Low"));
        assert_eq!(shown(5.5), ("6".into(), "High"));
        assert_eq!(shown(10.5), ("11".into(), "Extreme"));
        assert_eq!(shown(0.0), ("0".into(), "Low"));
    }

    #[test]
    fn uv_peak_uses_only_real_remaining_hourly_values_of_the_local_day() {
        // 2026-11-24 10:30 PST (18:30 UTC). Hourly samples every hour from 10:00 PST for 26 hours.
        let (loc, now) = (Location::new("Berkeley", "CA", "US", "US", 37.87, -122.27, "America/Los_Angeles"), 1_795_545_000);
        assert_eq!(loc.format_local_time(now, true), "10:30");
        let h0 = now - 1800;
        let uv_at = |i: i64| match i {
            2 => 7.65, // 12:00 today: the remaining peak
            1 | 3 => 4.0,
            26 => 9.0, // 12:00 TOMORROW: must not count for today
            _ => 0.4,
        };
        let mut b = test_bundle(now);
        b.current.uv_index = Some(2.95);
        b.hourly = (0..=26).map(|i| test_hour(h0 + i * 3600, Some(uv_at(i)), None)).collect();
        assert_eq!(b.uv_peak_rest_of_day(&loc, now), Some((h0 + 2 * 3600, 7.65)));
        // Late afternoon: the peak hours are in the past and do not count any more.
        assert_eq!(b.uv_peak_rest_of_day(&loc, now + 5 * 3600), Some((now + 5 * 3600, 2.95)));
        // The current value is itself part of "the rest of the day".
        b.current.uv_index = Some(8.2);
        assert_eq!(b.uv_peak_rest_of_day(&loc, now), Some((now, 8.2)));
        // No hourly UV from the provider, or a series that stops before midnight: the peak is unknown, not "Low".
        b.hourly.iter_mut().for_each(|h| h.uv_index = None);
        assert_eq!(b.uv_peak_rest_of_day(&loc, now), None);
        b.hourly = (0..=5).map(|i| test_hour(h0 + i * 3600, Some(1.0), None)).collect();
        assert_eq!(b.uv_peak_rest_of_day(&loc, now), None);
    }

    #[test]
    fn pressure_trend_comes_from_the_hourly_model_series_only() {
        let now = 1_795_545_000;
        let mut b = test_bundle(now);
        // An observed current pressure far above the model must not turn a rising forecast into "Falling" (or vice versa).
        b.current.pressure_hpa = Some(1021.4);
        b.hourly = [1012.0, 1012.4, 1013.1, 1014.0, 1014.2]
            .iter()
            .enumerate()
            .map(|(i, p)| test_hour(now - 600 + i as i64 * 3600, None, Some(*p)))
            .collect();
        assert!((b.pressure_trend_3h(now).expect("trend") - 2.0).abs() < 1e-9);
        // Missing pressure at either end, or no hourly sample near "now": no trend at all.
        b.hourly[3].pressure_hpa = None;
        assert_eq!(b.pressure_trend_3h(now), None);
        b.hourly[3].pressure_hpa = Some(1014.0);
        assert_eq!(b.pressure_trend_3h(now + 7 * 3600), None);
        b.hourly.clear();
        assert_eq!(b.pressure_trend_3h(now), None);
    }

    fn day(date: &str, lo: f64, hi: f64) -> DailyForecast {
        DailyForecast {
            date: date.into(),
            temp_min_c: lo,
            temp_max_c: hi,
            condition: WeatherCondition::Rain,
            precip_prob_pct: Some(60.0),
            precip_sum_mm: Some(4.2),
            wind_max_mps: Some(7.0),
            uv_max: Some(6.0),
            sunrise_unix: None,
            sunset_unix: None,
            partial: false,
        }
    }

    #[test]
    fn lean_update_keeps_full_detail_and_never_drops_or_invents_values() {
        let t0 = 1_795_545_000;
        let mut full = test_bundle(t0);
        (full.full_fetched_unix, full.alerts_fetched_unix) = (Some(t0), Some(t0));
        full.hourly = (0..30).map(|i| test_hour(t0 + i * 3600, Some(1.0), Some(1010.0))).collect();
        full.air_quality = Some(AirQuality { us_aqi: 73, pm2_5: Some(30.6) });
        full.alerts = vec![WeatherAlert {
            event: "Wind Advisory".into(),
            headline: "Wind Advisory until 6 PM".into(),
            description: String::new(),
            severity: AlertSeverity::Minor,
        }];
        full.daily = vec![day("2026-11-23", 8.0, 14.0), day("2026-11-24", 9.0, 15.0), day("2026-11-25", 7.0, 13.0)];
        full.source_name = "Open-Meteo (best-match model) + NOAA METAR KOAK obs + NOAA NWS alerts + CAMS AQI".into();
        assert!(!full.needs_full_refresh(t0 + 60) && full.needs_full_refresh(t0 + REFRESH_INTERVAL_SECS + 1));

        // Lean sidebar update five minutes later: its own UV, but no gust / visibility in the response.
        let mut lean = test_bundle(t0 + 300);
        lean.lean = true;
        lean.current = CurrentWeather {
            temp_c: 17.0,
            wind_speed_mps: 4.0,
            wind_gust_mps: None,
            visibility_m: None,
            uv_index: Some(2.0),
            ..lean.current
        };
        lean.daily = vec![DailyForecast {
            precip_sum_mm: None,
            precip_prob_pct: None,
            wind_max_mps: None,
            uv_max: None,
            ..day("2026-11-24", 9.5, 16.5)
        }];
        let m = lean.clone().merge_lean_onto(&full);
        // Current conditions are the update's; the detail is the last full fetch, labelled with ITS time.
        assert!(m.lean && m.needs_full_refresh(t0 + 301));
        assert_eq!((m.fetched_unix, m.full_fetched_unix, m.alerts_fetched_unix, m.data_time_unix()), (t0 + 300, Some(t0), Some(t0), t0));
        assert_eq!((m.current.temp_c, m.hourly.len(), m.alerts.len()), (17.0, 30, 1));
        assert_eq!(m.air_quality.as_ref().map(|a| a.us_aqi), Some(73));
        // UV is the update's own value; gust / visibility the update lacks are kept from the still-current full bundle.
        assert_eq!((m.current.uv_index, m.current.wind_gust_mps, m.current.visibility_m), (Some(2.0), Some(6.0), Some(20_000.0)));
        // Today's row takes the fresh high / low and keeps its forecast detail; yesterday's row is gone.
        assert_eq!(m.daily.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(), ["2026-11-24", "2026-11-25"]);
        assert_eq!((m.daily[0].temp_min_c, m.daily[0].temp_max_c, m.daily[0].precip_sum_mm), (9.5, 16.5, Some(4.2)));
        // Attribution: the station observation applied to the OLD current block and is no longer part of this bundle.
        assert_eq!(m.source_name, "Open-Meteo (best-match model) + NOAA NWS alerts + CAMS AQI");

        // An hour later the previous gust / visibility are no longer current: they become missing, never 0 or a guess.
        let mut late = lean.clone();
        (late.fetched_unix, late.current.uv_index) = (t0 + 3600, None);
        let m2 = late.merge_lean_onto(&full);
        assert_eq!((m2.current.uv_index, m2.current.wind_gust_mps, m2.current.visibility_m), (None, None, None));
        // A kept gust may never contradict the new mean wind.
        let mut windy = lean.clone();
        windy.current.wind_speed_mps = 8.0;
        assert_eq!(windy.merge_lean_onto(&full).current.wind_gust_mps, None);
        // A daily series that ended before today is dropped, not relabelled.
        let mut old = full.clone();
        old.daily = vec![day("2026-11-10", 1.0, 2.0)];
        assert_eq!(lean.merge_lean_onto(&old).daily.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(), ["2026-11-24"]);
    }

    #[test]
    fn failed_alert_fetch_keeps_the_alerts_already_known() {
        let t0 = 1_795_545_000;
        let mut known = test_bundle(t0);
        (known.full_fetched_unix, known.alerts_fetched_unix) = (Some(t0), Some(t0));
        known.source_name = "Open-Meteo (best-match model) + ECCC alerts".into();
        known.alerts = vec![WeatherAlert {
            event: "Snowfall warning".into(),
            headline: "Snowfall warning — Goderich".into(),
            description: "15 cm".into(),
            severity: AlertSeverity::Severe,
        }];
        // Full refresh whose alert request failed: `alerts_fetched_unix` is `None`, so nothing is known from this fetch.
        let mut failed = test_bundle(t0 + 600);
        failed.keep_known_alerts(&known);
        assert_eq!((failed.alerts.len(), failed.alerts_fetched_unix), (1, Some(t0)));
        assert_eq!(failed.source_name, "Open-Meteo (best-match model) + ECCC alerts");
        // The feed answered "no alerts": that answer stands and the old alert goes away.
        let mut answered = test_bundle(t0 + 600);
        answered.alerts_fetched_unix = Some(t0 + 600);
        answered.keep_known_alerts(&known);
        assert!(answered.alerts.is_empty() && answered.alerts_fetched_unix == Some(t0 + 600));
    }
}
