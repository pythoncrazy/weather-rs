use chrono::{FixedOffset, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

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
    pub elevation_m: Option<f64>,
    #[serde(default)]
    pub is_current_location: bool,
}

impl Location {
    pub fn make_id(lat: f64, lon: f64) -> String {
        format!("{lat:.3}_{lon:.3}")
    }
    pub fn new(
        name: impl Into<String>,
        region: &str,
        country: &str,
        cc: &str,
        lat: f64,
        lon: f64,
        tz: &str,
    ) -> Self {
        Self {
            id: Self::make_id(lat, lon),
            name: name.into(),
            region: region.into(),
            country: country.into(),
            country_code: cc.into(),
            lat,
            lon,
            timezone: tz.into(),
            elevation_m: None,
            is_current_location: false,
        }
    }
    pub fn is_same_place(&self, o: &Self) -> bool {
        if self.id == o.id {
            return true;
        }
        let dlat = (self.lat - o.lat).abs();
        let mut dlon = (self.lon - o.lon).abs();
        if dlon > 180.0 {
            dlon = 360.0 - dlon;
        }
        if dlat < 0.02 && dlon < 0.02 {
            return true;
        }
        !self.name.is_empty()
            && self.name.eq_ignore_ascii_case(&o.name)
            && dlat < 0.15
            && dlon < 0.15
    }
    pub fn subtitle(&self) -> String {
        match (self.region.is_empty(), self.country.is_empty()) {
            (false, false) if self.region != self.name => {
                format!("{}, {}", self.region, self.country)
            }
            (true, false) => self.country.clone(),
            (false, true) => self.region.clone(),
            _ => self.country_code.clone(),
        }
    }
    fn local_dt(&self, ts: i64) -> Option<chrono::DateTime<FixedOffset>> {
        let dt = Utc.timestamp_opt(ts, 0).single()?;
        if let Ok(tz) = self.timezone.parse::<Tz>() {
            return Some(dt.with_timezone(&tz).fixed_offset());
        }
        let off = FixedOffset::east_opt(((self.lon / 15.0).round() as i32) * 3600)
            .unwrap_or_else(|| FixedOffset::east_opt(0).expect("utc0"));
        Some(dt.with_timezone(&off))
    }
    pub fn format_local_time(&self, ts: i64, h24: bool) -> String {
        let fmt = if h24 { "%H:%M" } else { "%-I:%M %p" };
        self.local_dt(ts)
            .map_or_else(|| "--:--".into(), |d| d.format(fmt).to_string())
    }
    pub fn format_local_hour(&self, ts: i64, h24: bool) -> String {
        let fmt = if h24 { "%H:00" } else { "%-I %p" };
        self.local_dt(ts)
            .map_or_else(|| "--".into(), |d| d.format(fmt).to_string())
    }
    pub fn local_date(&self, ts: i64) -> NaiveDate {
        self.local_dt(ts)
            .map_or_else(|| Utc::now().date_naive(), |d| d.date_naive())
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
    pub fn severity_rank(self) -> u8 {
        self as u8
    }
    pub fn from_wmo(code: i32) -> Self {
        match code {
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
            _ => Self::PartlyCloudy,
        }
    }
    pub fn from_met_symbol(sym: &str) -> Self {
        let b = sym.split('_').next().unwrap_or(sym).to_ascii_lowercase();
        const M: &[(&str, WeatherCondition)] = &[
            ("thunder", WeatherCondition::Thunderstorm),
            ("heavysnow", WeatherCondition::HeavySnow),
            ("snow", WeatherCondition::Snow),
            ("sleet", WeatherCondition::Sleet),
            ("heavyrain", WeatherCondition::HeavyRain),
            ("lightrain", WeatherCondition::Drizzle),
            ("drizzle", WeatherCondition::Drizzle),
            ("rain", WeatherCondition::Rain),
            ("fog", WeatherCondition::Fog),
            ("cloudy", WeatherCondition::Overcast),
            ("fair", WeatherCondition::MostlyClear),
            ("clearsky", WeatherCondition::Clear),
        ];
        M.iter()
            .find(|&&(k, _)| b.contains(k))
            .map_or(Self::PartlyCloudy, |&(_, c)| c)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentWeather {
    pub temp_c: f64,
    pub feels_like_c: f64,
    pub condition: WeatherCondition,
    pub is_day: bool,
    pub humidity_pct: f64,
    pub dew_point_c: f64,
    pub pressure_hpa: f64,
    pub wind_speed_mps: f64,
    pub wind_gust_mps: Option<f64>,
    pub wind_dir_deg: f64,
    pub visibility_m: Option<f64>,
    pub uv_index: f64,
    pub cloud_cover_pct: f64,
    pub precip_mm: f64,
    pub updated_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinutelySample {
    pub timestamp_unix: i64,
    pub precip_mm_hr: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyForecast {
    pub timestamp_unix: i64,
    pub temp_c: f64,
    pub feels_like_c: f64,
    pub condition: WeatherCondition,
    pub is_day: bool,
    pub precip_prob_pct: f64,
    pub precip_mm: f64,
    pub wind_speed_mps: f64,
    pub wind_dir_deg: f64,
    pub humidity_pct: f64,
    pub uv_index: f64,
    pub pressure_hpa: f64,
    pub cloud_cover_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyForecast {
    pub date: String,
    pub timestamp_unix: i64,
    pub temp_min_c: f64,
    pub temp_max_c: f64,
    pub condition: WeatherCondition,
    pub precip_prob_pct: f64,
    pub precip_sum_mm: f64,
    pub wind_max_mps: f64,
    pub uv_max: f64,
    pub sunrise_unix: Option<i64>,
    pub sunset_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirQuality {
    pub us_aqi: i32,
    pub european_aqi: Option<i32>,
    pub pm2_5: f64,
    pub pm10: f64,
    pub ozone: Option<f64>,
    pub no2: Option<f64>,
}

impl AirQuality {
    pub fn info(&self) -> (&'static str, &'static str) {
        match self.us_aqi {
            ..=50 => (
                "Good",
                "Air quality is clean and ideal for outdoor activity.",
            ),
            51..=100 => ("Moderate", "Air quality is acceptable for most people."),
            101..=150 => (
                "Sensitive",
                "Sensitive groups should limit prolonged exertion.",
            ),
            151..=200 => ("Unhealthy", "Reduce prolonged or heavy outdoor exertion."),
            201..=300 => (
                "Very Unhealthy",
                "Avoid outdoor exertion due to poor air quality.",
            ),
            _ => (
                "Hazardous",
                "Avoid outdoor exertion due to poor air quality.",
            ),
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
    pub sender: String,
    pub expires: String,
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
    pub fetched_unix: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TempUnit {
    Celsius,
    Fahrenheit,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WindUnit {
    KmH,
    Mph,
    Ms,
    Knots,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PressureUnit {
    HPa,
    InHg,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PrecipUnit {
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
        match self {
            Self::Auto => "Auto (Open-Meteo Best Match + NOAA NWS + MET Norway)",
            Self::MetNorway => "MET Norway (ECMWF — Nordic Official)",
            Self::OpenMeteo => "Open-Meteo (HRRR / ICON / AROME / ECMWF)",
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
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
    pub saved_locations: Vec<Location>,
    pub selected_location_id: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let saved = default_locations();
        let selected_location_id = saved.first().map(|l| l.id.clone());
        Self {
            temp_unit: TempUnit::Celsius,
            wind_unit: WindUnit::KmH,
            pressure_unit: PressureUnit::HPa,
            precip_unit: PrecipUnit::Mm,
            use_24h_time: false,
            provider: ProviderChoice::Auto,
            theme_mode: ThemeMode::DynamicSky,
            animate_weather: true,
            saved_locations: saved,
            selected_location_id,
        }
    }
}

impl Settings {
    pub fn deduplicate_locations(&mut self) {
        let mut seen_current = false;
        let mut out: Vec<Location> = Vec::with_capacity(self.saved_locations.len());
        for loc in self.saved_locations.drain(..) {
            if loc.is_current_location {
                if !seen_current {
                    seen_current = true;
                    out.insert(0, loc);
                }
            } else if !out
                .iter()
                .any(|e| !e.is_current_location && e.is_same_place(&loc))
            {
                out.push(loc);
            }
        }
        self.saved_locations = out;
        if self
            .selected_location_id
            .as_ref()
            .is_none_or(|id| !self.saved_locations.iter().any(|l| &l.id == id))
        {
            self.selected_location_id = self.saved_locations.first().map(|l| l.id.clone());
        }
    }
    pub fn convert_temp(&self, c: f64) -> f64 {
        if self.temp_unit == TempUnit::Fahrenheit {
            c * 9.0 / 5.0 + 32.0
        } else {
            c
        }
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
    pub fn format_visibility(&self, m: f64) -> String {
        let (div, u) = if self.wind_unit == WindUnit::Mph {
            (1609.34, "mi")
        } else {
            (1000.0, "km")
        };
        let v = (m.max(0.0) / div).clamp(0.1, 99.0);
        if v < 1.0 {
            format!("{v:.1} {u}")
        } else {
            format!("{v:.0} {u}")
        }
    }
}

pub fn compass_direction(deg: f64) -> &'static str {
    const D: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    D[(((deg.rem_euclid(360.0)) / 22.5).round() as usize) % 16]
}
pub fn uv_category(uv: f64) -> &'static str {
    match uv {
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
pub fn apparent_temperature_c(t: f64, rh: f64, ws: f64) -> f64 {
    let (rh, ws) = (rh.clamp(1.0, 100.0), ws.max(0.0));
    let kmh = ws * 3.6;
    if t <= 10.0 && kmh > 4.8 {
        // NOAA / Environment Canada Wind Chill index.
        13.12 + 0.6215 * t - 11.37 * kmh.powf(0.16) + 0.3965 * t * kmh.powf(0.16)
    } else if t >= 26.7 && rh >= 40.0 {
        // Rothfusz NOAA Heat Index (computed in °F, returned in °C).
        let tf = t * 9.0 / 5.0 + 32.0;
        let hi_f = -42.379 + 2.049_015_23 * tf + 10.143_331_27 * rh
            - 0.224_755_41 * tf * rh
            - 6.837_83e-3 * tf * tf
            - 5.481_717e-2 * rh * rh
            + 1.228_74e-3 * tf * tf * rh
            + 8.528_2e-4 * tf * rh * rh
            - 1.99e-6 * tf * tf * rh * rh;
        (hi_f - 32.0) * 5.0 / 9.0
    } else {
        // Mild conditions: gentle breeze cooling without Steadman's -4°C shade offset.
        t - (ws - 2.0).max(0.0) * 0.28
    }
}

pub fn default_locations() -> Vec<Location> {
    [
        (
            "New York",
            "New York",
            "United States",
            "US",
            40.7128,
            -74.0060,
            "America/New_York",
            10.0,
        ),
        (
            "London",
            "England",
            "United Kingdom",
            "GB",
            51.5074,
            -0.1278,
            "Europe/London",
            11.0,
        ),
        (
            "Tokyo",
            "Tokyo",
            "Japan",
            "JP",
            35.6762,
            139.6503,
            "Asia/Tokyo",
            40.0,
        ),
        (
            "Oslo",
            "Oslo",
            "Norway",
            "NO",
            59.9139,
            10.7522,
            "Europe/Oslo",
            23.0,
        ),
    ]
    .into_iter()
    .map(|(n, r, c, cc, la, lo, tz, el)| Location {
        elevation_m: Some(el),
        ..Location::new(n, r, c, cc, la, lo, tz)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_units_symbols_and_compass() {
        let mut s = Settings {
            temp_unit: TempUnit::Fahrenheit,
            ..Default::default()
        };
        assert_eq!(
            (s.format_temp(0.0), s.format_temp(100.0)),
            ("32°".into(), "212°".into())
        );
        s.temp_unit = TempUnit::Celsius;
        assert_eq!(
            (s.format_temp(21.4), s.format_precip(2.5)),
            ("21°".into(), "3 mm".into())
        );
        assert_eq!(
            WeatherCondition::from_met_symbol("heavyrainandthunder_day"),
            WeatherCondition::Thunderstorm
        );
        assert_eq!(
            WeatherCondition::from_met_symbol("clearsky_night"),
            WeatherCondition::Clear
        );
        assert_eq!(
            (
                WeatherCondition::from_wmo(95),
                WeatherCondition::from_wmo(75)
            ),
            (WeatherCondition::Thunderstorm, WeatherCondition::HeavySnow)
        );
        assert_eq!(
            [0.0, 90.0, 225.0, 359.0].map(compass_direction),
            ["N", "E", "SW", "N"]
        );
        // Distinct neighboring cities (Oakland vs Berkeley) must NOT collapse together.
        let brk = Location::new("Berkeley", "CA", "US", "US", 37.872, -122.273, "");
        let oak = Location::new("Oakland", "CA", "US", "US", 37.804, -122.271, "");
        assert!(!brk.is_same_place(&oak));
        // Same city with slightly offset coordinates (e.g. IP geoloc vs geocoder) DOES match.
        let brk2 = Location::new("Berkeley", "CA", "US", "US", 37.868, -122.269, "");
        assert!(brk.is_same_place(&brk2));
        // Mild 53°F (11.7°C) with 5 mph (2.2 m/s) breeze must feel near actual temp (~11.6°C).
        assert!((apparent_temperature_c(11.7, 55.0, 2.2) - 11.7).abs() < 0.5);
        // Dense fog visibility must not format as "0 km" or "0 mi".
        assert_eq!(s.format_visibility(200.0), "0.2 km");
        s.wind_unit = WindUnit::Mph;
        assert_eq!(s.format_visibility(200.0), "0.1 mi");
        // No negative zero in temp formatting.
        assert_eq!(s.format_temp(-0.2), "0°");
        assert_eq!(s.format_temp_bare(-0.2), "0");
    }
}
