use super::tiles::MapVp;
use super::{BaseMapStyle, MapEngine, WeatherOverlay};
use crate::astro::subsolar_point;
use crate::icons::{pressure_to_color, ramp, temp_to_color};
use crate::model::{AlertSeverity, Location, Settings, WeatherBundle, compass_direction};
use crate::providers::{EcccModel, GridWeatherPoint};
use crate::style::rgba8;
use iced::advanced::image as adv_image;
use iced::{Color, Point, Rectangle, Size};
use std::{collections::HashMap, f64::consts::PI, sync::LazyLock};

const AQI_STOPS: [(f64, [u8; 3]); 7] = [
    (0.0, [52, 199, 89]),
    (42.0, [132, 212, 72]),
    (72.0, [255, 204, 0]),
    (120.0, [255, 149, 0]),
    (175.0, [255, 59, 48]),
    (250.0, [175, 82, 222]),
    (350.0, [142, 28, 62]),
];

fn build_scalar_lut(ov: WeatherOverlay, lo: f32, hi: f32) -> [[u8; 4]; 256] {
    std::array::from_fn(|i| {
        let v = lo as f64 + (i as f64 / 255.0) * (hi - lo) as f64;
        let col = match ov {
            WeatherOverlay::Temperature => temp_to_color(v, 0.58),
            WeatherOverlay::WindFlow => {
                let b = (v / 26.0).clamp(0.0, 1.0);
                rgba8((12.0 + b * 34.0) as u8, (88.0 + b * 58.0) as u8, (156.0 + b * 52.0) as u8, 0.64)
            }
            WeatherOverlay::Pressure => pressure_to_color(v, 0.56),
            WeatherOverlay::AirQuality => ramp(v, &AQI_STOPS, 0.50),
            _ => Color::TRANSPARENT,
        };
        [(col.r * 255.0) as u8, (col.g * 255.0) as u8, (col.b * 255.0) as u8, (col.a * 255.0) as u8]
    })
}

static SCALAR_TEMP_LUT: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| build_scalar_lut(WeatherOverlay::Temperature, -30.0, 50.0));
static SCALAR_WIND_LUT: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| build_scalar_lut(WeatherOverlay::WindFlow, 0.0, 26.0));
static SCALAR_PRESSURE_LUT: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| build_scalar_lut(WeatherOverlay::Pressure, 975.0, 1045.0));
static SCALAR_AQI_LUT: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| build_scalar_lut(WeatherOverlay::AirQuality, 0.0, 350.0));

fn build_terminator_lut(max_night_alpha: f32) -> [[u8; 4]; 256] {
    // Maps `sin_alt` from `0.045` (index 0: transparent twilight edge) down to `-0.235` (index 255: full midnight).
    std::array::from_fn(|i| {
        let sin_alt = 0.045 - (i as f32 / 255.0) * 0.280;
        let (r, g, b, a) = if sin_alt >= -0.045 {
            let u = ((0.045 - sin_alt) / 0.09).clamp(0.0, 1.0);
            let s = u * u * (3.0 - 2.0 * u);
            (
                (36.0 * (1.0 - s) + 14.0 * s) as u8,
                (24.0 * (1.0 - s) + 24.0 * s) as u8,
                (56.0 * (1.0 - s) + 64.0 * s) as u8,
                s * max_night_alpha * 0.42,
            )
        } else {
            let u = ((-0.045 - sin_alt) / 0.19).clamp(0.0, 1.0);
            let s = u * u * (3.0 - 2.0 * u);
            (
                (14.0 * (1.0 - s) + 6.0 * s) as u8,
                (24.0 * (1.0 - s) + 14.0 * s) as u8,
                (64.0 * (1.0 - s) + 38.0 * s) as u8,
                max_night_alpha * (0.42 + 0.58 * s),
            )
        };
        [r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8]
    })
}

static TERM_LUT_DARK: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| build_terminator_lut(0.34));
static TERM_LUT_LIGHT: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| build_terminator_lut(0.25));

pub(super) const WIND_TRAIL_LEN: usize = 6;
#[derive(Debug, Clone, Copy)]
pub(super) struct WindParticle {
    pub u: f32,
    pub v: f32,
    pub dx: f32,
    pub dy: f32,
    pub spd: f32,
    pub age: f32,
    pub max_age: f32,
    pub trail: [(f32, f32); WIND_TRAIL_LEN],
    pub tlen: u8,
}
#[derive(Clone)]
pub(super) struct ScalarCache {
    meta_sig: u64,
    zoom: f64,
    wx0: f64,
    wy0: f64,
    wx1: f64,
    wy1: f64,
    handle: adv_image::Handle,
}

#[derive(Clone, Default)]
pub(super) struct IsobarData {
    pub segs: Vec<(Point, Point)>,
    pub labels: Vec<(Point, String)>,
    /// `(screen_pos, is_high, pressure_hpa)`
    pub centers: Vec<(Point, bool, f32)>,
}

#[derive(Clone)]
pub(super) struct IsobarCache {
    sig: u64,
    data: IsobarData,
}

#[derive(Clone)]
pub(super) struct CityBadge {
    pub title: String,
    pub lat: f64,
    pub lon: f64,
    pub val: String,
    pub sub: String,
    pub col: Color,
    pub active: bool,
    pub is_current_location: bool,
    pub wind_deg: Option<f64>,
    pub alert_severity: Option<AlertSeverity>,
}

pub(super) fn init_wind_particles() -> Vec<WindParticle> {
    (0..220)
        .map(|i| {
            let h = |k: f32, m: f32| ((i as f32 * k).sin() * m).fract().abs();
            let (s1, s2, s3) = (h(97.13, 43_758.547), h(241.79, 28_411.91), h(53.41, 11_937.33));
            let max_age = 2.4 + s3 * 2.8;
            WindParticle {
                u: s1,
                v: s2,
                dx: 0.8,
                dy: -0.2,
                spd: 3.0,
                age: s3 * max_age,
                max_age,
                trail: [(s1, s2); WIND_TRAIL_LEN],
                tlen: 1,
            }
        })
        .collect()
}

fn grid_coords((gw, gh): (usize, usize), (bounds, lat, lon, zoom): MapVp, centered: bool) -> (Vec<f32>, Vec<f32>) {
    let (w, h, ws) = (bounds.width.max(64.0) as f64, bounds.height.max(64.0) as f64, 256.0 * 2.0_f64.powf(zoom));
    let (cwx, cwy) = lat_lon_to_world_px(lat, lon, zoom);
    let (off, dx, dy) = if centered { (0.5, gw as f64, gh as f64) } else { (0.0, (gw - 1).max(1) as f64, (gh - 1).max(1) as f64) };
    let lons = (0..gw).map(|gx| wrap_lon(((cwx + (gx as f64 + off) / dx * w - w * 0.5) / ws) * 360.0 - 180.0) as f32).collect();
    let lats = (0..gh)
        .map(|gy| (PI - (2.0 * PI * (cwy + (gy as f64 + off) / dy * h - h * 0.5).clamp(0.0, ws)) / ws).sinh().atan().to_degrees() as f32)
        .collect();
    (lats, lons)
}
fn hash_u64(vals: &[u64]) -> u64 {
    vals.iter().fold(0_u64, |a, &v| a.wrapping_mul(31).wrapping_add(v))
}

impl MapEngine {
    pub(super) fn shift_wind_particles(&mut self, dx_px: f32, dy_px: f32) {
        if self.overlay != WeatherOverlay::WindFlow {
            return;
        }
        let vp = self.viewport_size();
        let (du, dv) = (dx_px / vp.width.max(320.0), dy_px / vp.height.max(240.0));
        for p in &mut self.particles {
            let (nu, nv) = (p.u + du, p.v + dv);
            if !(0.0..=1.0).contains(&nu) || !(0.0..=1.0).contains(&nv) {
                (p.u, p.v) = (nu.rem_euclid(1.0), nv.rem_euclid(1.0));
                (p.trail, p.tlen) = ([(p.u, p.v); WIND_TRAIL_LEN], 1);
            } else {
                (p.u, p.v) = (nu, nv);
                for k in 0..(p.tlen as usize).min(WIND_TRAIL_LEN) {
                    p.trail[k] = (p.trail[k].0 + du, p.trail[k].1 + dv);
                }
            }
        }
    }

    pub(super) fn zoom_wind_particles(&mut self, scale: f32, anchor: (f32, f32)) {
        if self.overlay != WeatherOverlay::WindFlow || (scale - 1.0).abs() < 1e-4 {
            return;
        }
        let (au, av) = anchor;
        for p in &mut self.particles {
            let (nu, nv) = (au + (p.u - au) * scale, av + (p.v - av) * scale);
            if !(0.0..=1.0).contains(&nu) || !(0.0..=1.0).contains(&nv) {
                (p.u, p.v) = (nu.rem_euclid(1.0), nv.rem_euclid(1.0));
                (p.trail, p.tlen) = ([(p.u, p.v); WIND_TRAIL_LEN], 1);
            } else {
                (p.u, p.v) = (nu, nv);
                for k in 0..(p.tlen as usize).min(WIND_TRAIL_LEN) {
                    p.trail[k] = (au + (p.trail[k].0 - au) * scale, av + (p.trail[k].1 - av) * scale);
                }
            }
        }
    }

    pub(super) fn step_wind_particles(&mut self, bundles: &HashMap<String, WeatherBundle>, dt_sec: f32) {
        let vp = self.viewport_size();
        let (dt, ts, vw, vh, gw, gh) =
            (dt_sec.clamp(0.004, 0.06), self.selected_forecast_timestamp(), vp.width.max(320.0), vp.height.max(240.0), 16, 12);
        let sig = hash_u64(&[
            (self.center_lat * 100.0).round() as i64 as u64,
            (self.center_lon * 100.0).round() as i64 as u64,
            (self.zoom * 100.0).round() as u64,
            (vw.round() as u64) ^ ((vh.round() as u64) << 16),
            self.grid_gen,
            bundles.len() as u64,
            ts.map_or(0, |t| (t / 180) as u64),
        ]);
        let grid = if let Some((s, g)) = self.wind_grid
            && s == sig
        {
            g
        } else {
            let pts = self.all_weather_points(bundles, ts);
            if pts.is_empty() {
                return;
            }
            let (lats, lons) = grid_coords((gw, gh), (Size::new(vw, vh), self.center_lat, self.center_lon, self.zoom), false);
            let mut g = [(0.0_f32, 0.0_f32, 2.0_f32); 192];
            for (gy, &lat) in lats.iter().enumerate() {
                for (gx, &lon) in lons.iter().enumerate() {
                    let (wu, wv, spd) = idw_wind(&pts, lat as f64, lon as f64);
                    let d = spd as f32 + 0.35;
                    g[gy * gw + gx] = (wu as f32 / d, -(wv as f32 / d), spd as f32);
                }
            }
            self.wind_grid = Some((sig, g));
            g
        };
        let samp = |u: f32, v: f32| {
            let (gx, gy) = (u.clamp(0.0, 1.0) * (gw - 1) as f32, v.clamp(0.0, 1.0) * (gh - 1) as f32);
            let (x0, y0) = (gx.floor() as usize, gy.floor() as usize);
            let (x1, y1, tx, ty) = ((x0 + 1).min(gw - 1), (y0 + 1).min(gh - 1), gx - x0 as f32, gy - y0 as f32);
            let (c00, c10, c01, c11) = (grid[y0 * gw + x0], grid[y0 * gw + x1], grid[y1 * gw + x0], grid[y1 * gw + x1]);
            let bi = |a: f32, b: f32, c: f32, d: f32| (a * (1.0 - tx) + b * tx) * (1.0 - ty) + (c * (1.0 - tx) + d * tx) * ty;
            let (dx, dy, spd) = (bi(c00.0, c10.0, c01.0, c11.0), bi(c00.1, c10.1, c01.1, c11.1), bi(c00.2, c10.2, c01.2, c11.2));
            let n = dx.hypot(dy).max(1e-3);
            (dx / n, dy / n, spd)
        };
        for (idx, p) in self.particles.iter_mut().enumerate() {
            let (dx, dy, spd) = samp(p.u, p.v);
            let step = (26.0 + spd * 15.5) * dt;
            (p.dx, p.dy, p.spd, p.u, p.v, p.age) = (dx, dy, spd, p.u + dx * step / vw, p.v + dy * step / vh, p.age + dt);
            if p.age >= p.max_age || !(0.0..=1.0).contains(&p.u) || !(0.0..=1.0).contains(&p.v) {
                let seed = idx as f32 * 1.17 + p.max_age * 13.3 + p.age * 7.1;
                (p.u, p.v, p.age) =
                    (((seed * 73.7).sin() * 43_758.547).fract().abs(), ((seed * 191.3).cos() * 19_421.13).fract().abs(), 0.0);
                (p.dx, p.dy, p.spd) = samp(p.u, p.v);
                (p.trail, p.tlen) = ([(p.u, p.v); WIND_TRAIL_LEN], 1);
            } else if ((p.u - p.trail[0].0) * vw).powi(2) + ((p.v - p.trail[0].1) * vh).powi(2) >= 12.0 || p.tlen < 2 {
                p.trail.copy_within(0..WIND_TRAIL_LEN - 1, 1);
                (p.trail[0], p.tlen) = ((p.u, p.v), (p.tlen + 1).min(WIND_TRAIL_LEN as u8));
            }
        }
    }
}

/// True IEM `hrrr::REFP` / `REFD` coverage longitude bounds `(west_lon, east_lon)` at latitude `la`.
/// IEM reprojects NOAA HRRR CONUS onto a `3050×1340` EPSG:4326 grid cropped strictly to `[-126.0°, -65.0°]` × `[23.2°, 50.0°]`,
/// while HRRR's Lambert Conformal edges narrow slightly further south near Baja (`-123.4°` at 23.2°N) and Florida (`-71.5°` at 23.2°N).
#[inline]
pub(super) fn hrrr_lon_bounds(la: f64) -> (f64, f64) {
    let dl = (la - 21.14).max(0.0);
    ((-122.72 - dl * 0.355).max(-126.0), (-72.29 + dl * 0.355).min(-65.0))
}

/// Geographic bounding box `(lat_min, lat_max, lon_min, lon_max)` of Web Mercator tile `(z, x, y)`.
#[inline]
pub(super) fn tile_geo_bounds(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = (1_u64 << z) as f64;
    let lon_min = (x as f64 / n) * 360.0 - 180.0;
    let lon_max = ((x as f64 + 1.0) / n) * 360.0 - 180.0;
    let lat_max = (PI * (1.0 - 2.0 * y as f64 / n)).sinh().atan().to_degrees();
    let lat_min = (PI * (1.0 - 2.0 * (y as f64 + 1.0) / n)).sinh().atan().to_degrees();
    (lat_min, lat_max, lon_min, lon_max)
}

/// True if Web Mercator tile `(z, x, y)` intersects IEM NOAA HRRR's CONUS domain.
#[inline]
pub(super) fn tile_intersects_hrrr(z: u8, x: u32, y: u32) -> bool {
    let (la0, la1, lo0, lo1) = tile_geo_bounds(z, x, y);
    if la1 < 23.0 || la0 > 50.2 {
        return false;
    }
    let (w_top, e_top) = hrrr_lon_bounds(la1.clamp(23.2, 50.0));
    let (w_bot, e_bot) = hrrr_lon_bounds(la0.clamp(23.2, 50.0));
    lo1 >= w_top.min(w_bot) - 0.25 && lo0 <= e_top.max(e_bot) + 0.25
}

/// Smooth Hermite cross-fade width (in degrees) along model domain perimeters (`HRRR`, `HRDPS`, `RDPS`)
/// so all 4 real NWP tiers (`HRRR 3 km`, `HRDPS 2.5 km`, `RDPS 10 km`, `GDPS 15 km`) form an exact partition of unity (`Σ = 1.0`).
pub(super) const HRRR_ECCC_BLEND_DEG: f64 = 2.0;

#[inline]
fn smooth_in(inside_deg: f64) -> f32 {
    if inside_deg <= 0.0 {
        0.0
    } else if inside_deg >= HRRR_ECCC_BLEND_DEG {
        1.0
    } else {
        let u = (inside_deg / HRRR_ECCC_BLEND_DEG) as f32;
        u * u * (3.0 - 2.0 * u)
    }
}

/// Smooth membership weight `[0, 1]` inside IEM NOAA HRRR 3 km CONUS domain.
#[inline]
pub(super) fn hrrr_weight(la: f64, lo: f64) -> f32 {
    let lat_d = (la - 23.2).min(50.0 - la);
    if lat_d <= 0.0 {
        return 0.0;
    }
    let (w, e) = hrrr_lon_bounds(la);
    smooth_in(lat_d.min(lo - w).min(e - lo))
}

/// Safe interior latitude bounds `(south_lat, north_lat)` of ECCC `HRDPS.CONTINENTAL_RT` (2.5 km) at longitude `lo`.
#[inline]
pub(super) fn hrdps_lat_bounds(lo: f64) -> (f64, f64) {
    let south = if lo < -130.0 {
        41.5 + (-130.0 - lo) * 0.35
    } else if lo > -84.0 {
        // East of -84°W, HRDPS 2.5 km extends down to ~36.5°N along the Atlantic seaboard!
        (41.5 - (lo + 84.0) * 0.45).max(37.0)
    } else {
        41.5
    };
    let north = if lo > -68.0 { 63.0 - (lo + 68.0) * 0.42 } else { 66.5 };
    (south, north)
}

/// Smooth membership weight `[0, 1]` inside ECCC `HRDPS.CONTINENTAL_RT` 2.5 km domain (Canada + Pacific NW + NE Atlantic).
#[inline]
pub(super) fn hrdps_weight(la: f64, lo: f64) -> f32 {
    let lon_d = (lo - (-134.0)).min(-58.0 - lo);
    if lon_d <= 0.0 {
        return 0.0;
    }
    let (s, n) = hrdps_lat_bounds(lo);
    smooth_in(lon_d.min(la - s).min(n - la))
}

const RDPS_BLEND_DEG: f64 = 6.0;

/// Smooth membership weight `[0, 1]` inside ECCC `RDPS_10km_PrecipRate` 10 km safe North America / Pacific / Atlantic box.
#[inline]
pub(super) fn rdps_weight(la: f64, lo: f64) -> f32 {
    let d = (la - 23.0).min(82.0 - la).min(lo - (-148.0)).min(-45.0 - lo);
    if d <= 0.0 {
        0.0
    } else if d >= RDPS_BLEND_DEG {
        1.0
    } else {
        let u = (d / RDPS_BLEND_DEG) as f32;
        u * u * (3.0 - 2.0 * u)
    }
}

/// True if Web Mercator tile `(z, x, y)` lies strictly in the `weight == 1.0` interior core of HRRR.
#[inline]
pub(super) fn tile_fully_inside_hrrr_core(z: u8, x: u32, y: u32) -> bool {
    let (la0, la1, lo0, lo1) = tile_geo_bounds(z, x, y);
    let b = HRRR_ECCC_BLEND_DEG + 0.05;
    let inside_core = |la: f64, lo: f64| {
        if !(23.2 + b..=50.0 - b).contains(&la) {
            return false;
        }
        let (w, e) = hrrr_lon_bounds(la);
        lo >= w + b && lo <= e - b
    };
    inside_core(la0, lo0) && inside_core(la0, lo1) && inside_core(la1, lo0) && inside_core(la1, lo1)
}

/// True if any part of Web Mercator tile `(z, x, y)` lies outside (or within the blend rim of) IEM HRRR's CONUS domain.
#[inline]
pub(super) fn tile_extends_outside_hrrr(z: u8, x: u32, y: u32) -> bool {
    !tile_fully_inside_hrrr_core(z, x, y)
}

/// True if tile `(z, x, y)` needs ECCC `HRDPS 2.5 km` (`(1 - w_hrrr) * w_hrdps > 0` somewhere in tile).
#[inline]
pub(super) fn tile_needs_hrdps(z: u8, x: u32, y: u32, with_hrrr: bool) -> bool {
    if with_hrrr && tile_fully_inside_hrrr_core(z, x, y) {
        return false;
    }
    let (la0, la1, lo0, lo1) = tile_geo_bounds(z, x, y);
    if lo1 < -134.1 || lo0 > -57.9 || la1 < 36.9 || la0 > 66.6 {
        return false;
    }
    let (s0, n0) = hrdps_lat_bounds(lo0.clamp(-134.0, -58.0));
    let (s1, n1) = hrdps_lat_bounds(lo1.clamp(-134.0, -58.0));
    la1 >= s0.min(s1) - 0.1 && la0 <= n0.max(n1) + 0.1
}

#[inline]
fn tile_fully_inside_hrdps_core(z: u8, x: u32, y: u32) -> bool {
    let (la0, la1, lo0, lo1) = tile_geo_bounds(z, x, y);
    let b = HRRR_ECCC_BLEND_DEG + 0.05;
    if lo0 < -134.0 + b || lo1 > -58.0 - b {
        return false;
    }
    let (s0, n0) = hrdps_lat_bounds(lo0);
    let (s1, n1) = hrdps_lat_bounds(lo1);
    la0 >= s0.max(s1) + b && la1 <= n0.min(n1) - b
}

/// True if tile `(z, x, y)` needs ECCC `RDPS 10 km` (`(1 - w_hrrr) * (1 - w_hrdps) * w_rdps > 0` somewhere in tile).
#[inline]
pub(super) fn tile_needs_rdps(z: u8, x: u32, y: u32, with_hrrr: bool) -> bool {
    if (with_hrrr && tile_fully_inside_hrrr_core(z, x, y)) || tile_fully_inside_hrdps_core(z, x, y) {
        return false;
    }
    let (la0, la1, lo0, lo1) = tile_geo_bounds(z, x, y);
    la1 >= 22.9 && la0 <= 82.1 && lo1 >= -148.1 && lo0 <= -44.9
}

#[inline]
fn tile_fully_inside_rdps_core(z: u8, x: u32, y: u32) -> bool {
    let (la0, la1, lo0, lo1) = tile_geo_bounds(z, x, y);
    let b = RDPS_BLEND_DEG + 0.05;
    la0 >= 23.0 + b && la1 <= 82.0 - b && lo0 >= -148.0 + b && lo1 <= -45.0 - b
}

/// True if tile `(z, x, y)` needs ECCC `GDPS 15 km` (`(1 - w_hrrr) * (1 - w_hrdps) * (1 - w_rdps) > 0` somewhere in tile).
#[inline]
pub(super) fn tile_needs_gdps(z: u8, x: u32, y: u32, with_hrrr: bool) -> bool {
    !((with_hrrr && tile_fully_inside_hrrr_core(z, x, y)) || tile_fully_inside_hrdps_core(z, x, y) || tile_fully_inside_rdps_core(z, x, y))
}

/// Display weight `[0, 1]` of ECCC tier `model` at a point: the exact partition of unity used to composite the tiers
/// (`HRRR + HRDPS + RDPS + GDPS == 1` everywhere; `outside_hrrr == false` drops HRRR from the partition).
#[inline]
pub(super) fn eccc_tier_weight(model: EcccModel, outside_hrrr: bool, la: f64, lo: f64) -> f32 {
    let base = if outside_hrrr { 1.0 - hrrr_weight(la, lo) } else { 1.0 };
    match model {
        EcccModel::Hrdps => base * hrdps_weight(la, lo),
        EcccModel::Rdps => base * (1.0 - hrdps_weight(la, lo)) * rdps_weight(la, lo),
        EcccModel::Gdps => base * (1.0 - hrdps_weight(la, lo)) * (1.0 - rdps_weight(la, lo)),
    }
}

/// Geographic window `(lat_min, lat_max, lon_min, lon_max)` covering viewport `vp` plus a pan margin, snapped outward
/// to whole degrees so it stays stable (cacheable) during small pans. `None` when the view wraps the antimeridian.
pub(super) fn viewport_geo_window((bounds, lat, lon, zoom): MapVp) -> Option<(f64, f64, f64, f64)> {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    let (cwx, cwy) = lat_lon_to_world_px(lat, lon, zoom);
    let (hw, hh) = (bounds.width as f64 * 0.62, bounds.height as f64 * 0.62);
    if cwx - hw < 0.0 || cwx + hw > ws {
        return None;
    }
    let (la1, lo0) = world_px_to_lat_lon(cwx - hw, cwy - hh, zoom);
    let (la0, lo1) = world_px_to_lat_lon(cwx + hw, cwy + hh, zoom);
    Some((la0.floor(), la1.ceil(), lo0.floor(), lo1.ceil()))
}

/// True if ECCC tier `model` has nonzero display weight anywhere in the part of tile `(z, x, y)` that lies inside
/// `window` (the whole tile when `None`).
///
/// Tiles are only worth downloading where their tier is actually drawn: a 45°-wide GDPS tile that pokes 2° into the
/// viewport is skipped when RDPS owns those 2°. The tier masks are sampled on a ≤ 0.5° lattice (their blend ramps are
/// ≥ 2° wide), and results are memoized because the draw loop asks every frame.
pub(super) fn eccc_tier_needed_in(
    model: EcccModel, outside_hrrr: bool, z: u8, x: u32, y: u32, window: Option<(f64, f64, f64, f64)>,
) -> bool {
    type Key = (u8, bool, u8, u32, u32, Option<[i32; 4]>);
    thread_local! {
        static MEMO: std::cell::RefCell<HashMap<Key, bool>> = std::cell::RefCell::new(HashMap::new());
    }
    let key: Key = (model as u8, outside_hrrr, z, x, y, window.map(|w| [w.0 as i32, w.1 as i32, w.2 as i32, w.3 as i32]));
    if let Some(v) = MEMO.with(|m| m.borrow().get(&key).copied()) {
        return v;
    }
    let coarse = match model {
        EcccModel::Hrdps => tile_needs_hrdps(z, x, y, outside_hrrr),
        EcccModel::Rdps => tile_needs_rdps(z, x, y, outside_hrrr),
        EcccModel::Gdps => tile_needs_gdps(z, x, y, outside_hrrr),
    };
    let needed = coarse && {
        let (mut la0, mut la1, mut lo0, mut lo1) = tile_geo_bounds(z, x, y);
        if let Some((wa0, wa1, wo0, wo1)) = window {
            (la0, la1, lo0, lo1) = (la0.max(wa0), la1.min(wa1), lo0.max(wo0), lo1.min(wo1));
        }
        la0 <= la1 && lo0 <= lo1 && {
            let (ny, nx) = ((((la1 - la0) / 0.5).ceil() as usize).clamp(1, 96), (((lo1 - lo0) / 0.5).ceil() as usize).clamp(1, 96));
            (0..=ny).any(|iy| {
                let la = la0 + (la1 - la0) * iy as f64 / ny as f64;
                (0..=nx).any(|ix| eccc_tier_weight(model, outside_hrrr, la, lo0 + (lo1 - lo0) * ix as f64 / nx as f64) > 0.002)
            })
        }
    };
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 8192 {
            m.clear();
        }
        m.insert(key, needed);
    });
    needed
}

impl MapEngine {
    pub(super) fn build_scalar_overlay_handle(
        &self, vp: MapVp, ov: WeatherOverlay, bundles: &HashMap<String, WeatherBundle>, ts: Option<i64>,
    ) -> Option<(adv_image::Handle, Rectangle)> {
        if !matches!(ov, WeatherOverlay::Temperature | WeatherOverlay::AirQuality | WeatherOverlay::WindFlow | WeatherOverlay::Pressure) {
            return None;
        }
        let (bounds, lat, lon, zoom) = vp;
        let meta_sig = hash_u64(&[ov as u64, self.grid_gen, bundles.len() as u64, ts.map_or(0, |t| (t / 180) as u64)]);
        let (cwx, cwy) = lat_lon_to_world_px(lat, lon, zoom);
        let ws = 256.0 * 2.0_f64.powf(zoom);
        if let Ok(g) = self.scalar_tex.lock()
            && let Some(c) = g.as_ref()
            && c.meta_sig == meta_sig
            && (zoom - c.zoom).abs() <= 0.48
        {
            let sc = 2.0_f64.powf(zoom - c.zoom);
            let (mut x0, mut x1, y0, y1) = (c.wx0 * sc, c.wx1 * sc, c.wy0 * sc, c.wy1 * sc);
            let mid = (x0 + x1) * 0.5;
            if mid - cwx > ws * 0.5 {
                (x0, x1) = (x0 - ws, x1 - ws);
            } else if cwx - mid > ws * 0.5 {
                (x0, x1) = (x0 + ws, x1 + ws);
            }
            let (sx0, sy0) = ((bounds.width as f64 * 0.5 + x0 - cwx) as f32, (bounds.height as f64 * 0.5 + y0 - cwy) as f32);
            let (sw, sh) = (((x1 - x0) as f32).max(1.0), ((y1 - y0) as f32).max(1.0));
            if sx0 <= 1.0 && sy0 <= 1.0 && sx0 + sw >= bounds.width - 1.0 && sy0 + sh >= bounds.height - 1.0 {
                return Some((c.handle.clone(), Rectangle { x: sx0, y: sy0, width: sw, height: sh }));
            }
        }
        let (fw, fh) = (160_usize, 120_usize);
        let (ov_w, ov_h) = (bounds.width.max(64.0) as f64 * 1.75, bounds.height.max(64.0) as f64 * 1.75);
        let (wx0, wx1, wy0, wy1) = (cwx - ov_w * 0.5, cwx + ov_w * 0.5, cwy - ov_h * 0.5, cwy + ov_h * 0.5);
        let ov_vp: MapVp = (Size::new(ov_w as f32, ov_h as f32), lat, lon, zoom);
        let (lats, lons) = grid_coords((fw, fh), ov_vp, true);
        let raw: Vec<(f32, f32, f32)> = if ov == WeatherOverlay::AirQuality {
            self.all_aqi_points(bundles).into_iter().map(|(la, lo, v)| (la as f32, lo as f32, v as f32)).collect()
        } else {
            let idx = match ov {
                WeatherOverlay::Temperature => 0,
                WeatherOverlay::WindFlow => 4,
                _ => 5,
            };
            self.all_weather_points(bundles, ts).into_iter().map(|pt| (pt.lat as f32, pt.lon as f32, pt.v[idx])).collect()
        };
        let lat_span = (lats[0] - lats[fh - 1]).abs().max(0.5);
        let (clat_f, clon_f, lat_h) = (lat as f32, lon as f32, lat_span * 0.55 + 14.0);
        let lon_h = ((360.0 / 2.0_f32.powf((zoom - 1.5).max(0.0) as f32)).min(180.0) * 1.15 + 18.0).min(180.0);
        let mut pts: Vec<_> = raw
            .iter()
            .copied()
            .filter(|&(la, lo, _)| (la - clat_f).abs() <= lat_h && (lo - clon_f).abs().min(360.0 - (lo - clon_f).abs()) <= lon_h)
            .collect();
        if pts.len() < 8 && raw.len() > pts.len() {
            pts = raw;
        }
        // Never invent a uniform fallback blanket when no real observations/samples exist yet!
        if pts.is_empty() {
            return None;
        }
        let npts = pts.len();
        let vis_span = lat_span / 1.75;
        let (lut_lo, lut_hi, lut): (f32, f32, &[[u8; 4]; 256]) = match ov {
            WeatherOverlay::Temperature => (-30.0, 50.0, &SCALAR_TEMP_LUT),
            WeatherOverlay::WindFlow => (0.0, 26.0, &SCALAR_WIND_LUT),
            WeatherOverlay::Pressure => (975.0, 1045.0, &SCALAR_PRESSURE_LUT),
            WeatherOverlay::AirQuality => (0.0, 350.0, &SCALAR_AQI_LUT),
            _ => return None,
        };
        let lut_scale = 255.0 / (lut_hi - lut_lo);
        const BAYER4: [f32; 16] = [-0.44, 0.06, -0.31, 0.19, 0.31, -0.19, 0.44, -0.06, -0.25, 0.25, -0.38, 0.12, 0.50, 0.00, 0.38, -0.12];
        let mut rgba = vec![0_u8; fw * fh * 4];
        let analysis = SampleAnalysis::new(&pts, zoom);
        let lon_tbl = analysis.lon_table(&lons);
        let (mut vals, mut near2) = (vec![0.0_f32; fw], vec![0.0_f32; fw]);
        // Before the 20-point CAMS AQI regional grid arrives, show a refined radial aura around known stations
        // instead of smearing one city's AQI uniformly across thousands of miles of ocean.
        let aqi_sparse_fade = ov == WeatherOverlay::AirQuality && npts < 4;
        let r_in = (vis_span * 0.16).clamp(0.8, 3.2);
        let r_out = (r_in * 2.2).max(r_in + 1.0);
        for (gy, &la) in lats.iter().enumerate() {
            analysis.eval_row(la, &lon_tbl, &mut vals, &mut near2);
            let bayer_row = (gy & 3) << 2;
            for (gx, (&v, &d2)) in vals.iter().zip(&near2).enumerate() {
                let idx = ((v - lut_lo) * lut_scale + BAYER4[bayer_row | (gx & 3)]).clamp(0.0, 255.0) as usize;
                let mut px = lut[idx];
                if aqi_sparse_fade {
                    let u = ((r_out - d2.sqrt()) / (r_out - r_in)).clamp(0.0, 1.0);
                    px[3] = (px[3] as f32 * u * u * (3.0 - 2.0 * u)).round() as u8;
                }
                let o = (gy * fw + gx) * 4;
                rgba[o..o + 4].copy_from_slice(&px);
            }
        }
        let handle = adv_image::Handle::from_rgba(fw as u32, fh as u32, rgba);
        let _ = self.scalar_tex.lock().map(|mut g| {
            *g = Some(ScalarCache { meta_sig, zoom, wx0, wy0, wx1, wy1, handle: handle.clone() });
        });
        let dst = Rectangle {
            x: (bounds.width as f64 * 0.5 - ov_w * 0.5) as f32,
            y: (bounds.height as f64 * 0.5 - ov_h * 0.5) as f32,
            width: ov_w as f32,
            height: ov_h as f32,
        };
        Some((handle, dst))
    }
}

pub(super) fn declutter_points(pts: &[(f64, f64, bool)], clat: f64, clon: f64, zoom: f64, rect: Rectangle) -> Vec<(usize, Point, Point)> {
    let mut out = Vec::with_capacity(pts.len());
    for act in [true, false] {
        for (i, &(lat, lon, active)) in pts.iter().enumerate().filter(|(_, p)| p.2 == act) {
            let pos = project_lat_lon_to_screen(lat, lon, clat, clon, zoom, rect);
            if pos.x < -36.0 || pos.x > rect.width + 36.0 || pos.y < -36.0 || pos.y > rect.height + 36.0 {
                continue;
            }
            let dc = Point::new(pos.x, if active { pos.y - 31.0 } else { pos.y });
            if !active && out.iter().any(|&(_, _, o): &(usize, Point, Point)| (o.x - dc.x).powi(2) + (o.y - dc.y).powi(2) < 2704.0) {
                continue;
            }
            out.push((i, pos, dc));
        }
    }
    out
}

pub fn lat_lon_to_world_px(lat: f64, lon: f64, zoom: f64) -> (f64, f64) {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    let merc = (PI * 0.25 + lat.clamp(-85.05112878, 85.05112878).to_radians() * 0.5).tan().ln();
    (((lon + 180.0) / 360.0) * ws, (0.5 - merc / (2.0 * PI)) * ws)
}
pub fn world_px_to_lat_lon(wx: f64, wy: f64, zoom: f64) -> (f64, f64) {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    ((PI - (2.0 * PI * wy.clamp(0.0, ws)) / ws).sinh().atan().to_degrees(), wrap_lon((wx / ws) * 360.0 - 180.0))
}
pub(super) fn project_lat_lon_to_screen(lat: f64, lon: f64, clat: f64, clon: f64, zoom: f64, rect: Rectangle) -> Point {
    let (ws, (cwx, cwy), (mut wx, wy)) =
        (256.0 * 2.0_f64.powf(zoom), lat_lon_to_world_px(clat, clon, zoom), lat_lon_to_world_px(lat, lon, zoom));
    if wx - cwx > ws * 0.5 {
        wx -= ws;
    } else if cwx - wx > ws * 0.5 {
        wx += ws;
    }
    Point::new(rect.width * 0.5 + (wx - cwx) as f32, rect.height * 0.5 + (wy - cwy) as f32)
}
pub(super) fn project_screen_to_lat_lon(pos: Point, clat: f64, clon: f64, zoom: f64, rect: Rectangle) -> (f64, f64) {
    let (cwx, cwy) = lat_lon_to_world_px(clat, clon, zoom);
    world_px_to_lat_lon(cwx + (pos.x - rect.width * 0.5) as f64, cwy + (pos.y - rect.height * 0.5) as f64, zoom)
}
pub(super) fn wrap_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}
/// Extent in degrees of the 4×5 model sample lattice requested around the map center at `zoom`
/// (`span × 1.15` of latitude over 4 rows, `span × 1.5` of longitude over 5 columns).
pub(super) fn grid_sample_span_deg(zoom: f64) -> f64 {
    (135.0 / 2.0_f64.powf(zoom - 2.0)).clamp(1.2, 55.0)
}
/// Two-scale objective analysis of sparse model samples `(lat, lon, value)` into a continuous field — the single
/// interpolator behind the temperature / wind / pressure / air-quality color fields and the pressure isobars.
///
/// A wide inverse-distance kernel (softening radius = the sample lattice's row spacing) gives a smooth first guess;
/// two successive-correction passes with a kernel half as wide then add back what the first guess smoothed away at
/// the samples (the classic Barnes scheme). Against the model's own pressure field this is ~1.3 hPa RMS; the previous
/// single wide kernel flattened real gradients to ~4 hPa RMS (up to 13 hPa), putting isobars and colors in the wrong
/// place, while a single narrow kernel is accurate at the samples but draws lumpy bullseyes between them.
pub(super) struct SampleAnalysis {
    lat: Vec<f32>,
    lon: Vec<f32>,
    /// Sample values, zero-padded to a multiple of 4 lanes.
    val: Vec<f32>,
    /// Summed correction residual per sample, zero-padded like `val`.
    resid: Vec<f32>,
    s2_wide: f32,
    s2_narrow: f32,
}

impl SampleAnalysis {
    pub(super) fn new(pts: &[(f32, f32, f32)], zoom: f64) -> Self {
        let spacing = (grid_sample_span_deg(zoom) * 1.15 / 3.0) as f32;
        let pad_n = (pts.len() + 3) & !3;
        let mut a = Self {
            lat: pts.iter().map(|p| p.0).collect(),
            lon: pts.iter().map(|p| p.1).collect(),
            val: pts.iter().map(|p| p.2).chain(std::iter::repeat(0.0)).take(pad_n).collect(),
            resid: vec![0.0; pad_n],
            s2_wide: spacing * spacing,
            s2_narrow: (0.5 * spacing) * (0.5 * spacing),
        };
        for _ in 0..2 {
            let corr: Vec<f32> = pts.iter().map(|&(la, lo, v)| v - a.eval_point(la, lo)).collect();
            for (r, c) in a.resid.iter_mut().zip(corr) {
                *r += c;
            }
        }
        a
    }

    /// Latitude-compressed squared distance weights: `1 / (Δlat² + Δlon²·cos²(lat) + s²)²`.
    #[inline]
    fn cos2(la: f32) -> f32 {
        let c = la.to_radians().cos().abs().max(0.22);
        c * c
    }

    /// Analysed value at one point (used to form the correction residuals at the samples themselves).
    fn eval_point(&self, la: f32, lo: f32) -> f32 {
        let cos2 = Self::cos2(la);
        let (mut ww, mut aw, mut wn, mut an) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        for (k, (&pla, &plo)) in self.lat.iter().zip(&self.lon).enumerate() {
            let dl = (plo - lo).abs();
            let dlo = dl.min(360.0 - dl);
            let d2 = (pla - la) * (pla - la) + dlo * dlo * cos2;
            let (w, n) = (1.0 / ((d2 + self.s2_wide) * (d2 + self.s2_wide)), 1.0 / ((d2 + self.s2_narrow) * (d2 + self.s2_narrow)));
            ww += w;
            aw += self.val[k] * w;
            wn += n;
            an += self.resid[k] * n;
        }
        aw / ww.max(1e-12) + an / wn.max(1e-12)
    }

    /// Squared longitude differences between every output column and every sample (`lons.len() × padded samples`).
    pub(super) fn lon_table(&self, lons: &[f32]) -> Vec<f32> {
        let pad_n = self.val.len();
        let mut tbl = vec![0.0_f32; lons.len() * pad_n];
        for (row, &lo) in tbl.chunks_exact_mut(pad_n.max(1)).zip(lons) {
            for (slot, &plo) in row.iter_mut().zip(&self.lon) {
                let dl = (plo - lo).abs();
                let dlo = dl.min(360.0 - dl);
                *slot = dlo * dlo;
            }
        }
        tbl
    }

    /// Evaluates the analysed field along one latitude row. `out[gx]` receives the value at column `gx` of the
    /// `lon_table`; `near2[gx]` receives the squared distance (degrees²) to the nearest sample.
    pub(super) fn eval_row(&self, la: f32, lon_tbl: &[f32], out: &mut [f32], near2: &mut [f32]) {
        let (n, pad_n) = (self.lat.len(), self.val.len());
        if n == 0 {
            out.fill(0.0);
            near2.fill(f32::INFINITY);
            return;
        }
        let cos2 = Self::cos2(la);
        // Padding lanes sit infinitely far away so they contribute nothing to either kernel.
        let mut dlat2 = vec![1e18_f32; pad_n];
        for (d, &pla) in dlat2.iter_mut().zip(&self.lat) {
            *d = (pla - la) * (pla - la);
        }
        let (dlat_c, val_c, res_c) = (dlat2.as_chunks::<4>().0, self.val.as_chunks::<4>().0, self.resid.as_chunks::<4>().0);
        for ((o, nr), dlo_row) in out.iter_mut().zip(near2.iter_mut()).zip(lon_tbl.chunks_exact(pad_n)) {
            // 4-wide lanes auto-vectorize cleanly (AVX2 / NEON).
            let (mut ww, mut aw, mut wn, mut an, mut md) = ([0.0_f32; 4], [0.0_f32; 4], [0.0_f32; 4], [0.0_f32; 4], [f32::INFINITY; 4]);
            for (((dl, dlo), pv), pr) in dlat_c.iter().zip(dlo_row.as_chunks::<4>().0).zip(val_c).zip(res_c) {
                for l in 0..4 {
                    let d2 = dl[l] + dlo[l] * cos2;
                    let (dw, dn) = (d2 + self.s2_wide, d2 + self.s2_narrow);
                    let (w, nw) = (1.0 / (dw * dw), 1.0 / (dn * dn));
                    ww[l] += w;
                    aw[l] += pv[l] * w;
                    wn[l] += nw;
                    an[l] += pr[l] * nw;
                    md[l] = md[l].min(d2);
                }
            }
            let sum = |a: [f32; 4]| (a[0] + a[1]) + (a[2] + a[3]);
            *o = sum(aw) / sum(ww).max(1e-12) + sum(an) / sum(wn).max(1e-12);
            *nr = md[0].min(md[1]).min(md[2].min(md[3]));
        }
    }
}
pub(super) fn quantize_coord(lat: f64, lon: f64) -> (i32, i32) {
    ((lat * 10.0).round() as i32, (wrap_lon(lon) * 10.0).round() as i32)
}
pub(super) fn idw_wind(pts: &[GridWeatherPoint], lat: f64, lon: f64) -> (f64, f64, f64) {
    let (mut ws, mut us, mut vs, mut ss) = (0.0, 0.0, 0.0, 0.0);
    for pt in pts {
        let (dl, dlat) = ((pt.lon - lon).abs(), pt.lat - lat);
        let w = 1.0 / (dlat * dlat + dl.min(360.0 - dl).powi(2) + 0.15);
        (ws, us, vs, ss) = (ws + w, us + pt.v[2] as f64 * w, vs + pt.v[3] as f64 * w, ss + pt.v[4] as f64 * w);
    }
    if ws > 0.0 { (us / ws, vs / ws, ss / ws) } else { (0.0, 0.0, 0.0) }
}

/// Computes Apple Maps latitude-corrected distance scale bar `(width_px, label)`.
pub(super) fn scale_bar_spec(lat: f64, zoom: f64, imperial: bool) -> (f32, String) {
    let mpp = 156_543.033_92 * lat.to_radians().cos().abs().max(0.06) / 2.0_f64.powf(zoom);
    let target_m = mpp * 84.0;
    const STEPS: [f64; 13] = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0];
    let pick = |val: f64| STEPS.iter().copied().rev().find(|&s| s <= val).unwrap_or(1.0);
    let (dist_m, lbl) = if imperial {
        let mi = target_m / 1609.344;
        if mi >= 0.8 {
            let s = pick(mi);
            (s * 1609.344, format!("{s:.0} mi"))
        } else {
            let s = pick(target_m * 3.28084);
            (s / 3.28084, format!("{s:.0} ft"))
        }
    } else if target_m >= 900.0 {
        let s = pick(target_m / 1000.0);
        (s * 1000.0, format!("{s:.0} km"))
    } else {
        let s = pick(target_m);
        (s, format!("{s:.0} m"))
    };
    ((dist_m / mpp).clamp(36.0, 130.0) as f32, lbl)
}

impl MapEngine {
    /// Value under the cursor, read from the same analysed field the overlay is colored from.
    ///
    /// Everything here is an estimate between sparse model samples, so every readout is prefixed with `≈`; the exact
    /// model value for a point comes from "Inspect Weather Here", which fetches it.
    pub(super) fn sample_probe_label(
        &self, lat: f64, lon: f64, ov: WeatherOverlay, bundles: &HashMap<String, WeatherBundle>, settings: &Settings,
    ) -> String {
        let ts = self.selected_forecast_timestamp();
        let analysed =
            |pts: &[(f32, f32, f32)]| (!pts.is_empty()).then(|| SampleAnalysis::new(pts, self.zoom).eval_point(lat as f32, lon as f32));
        match ov {
            WeatherOverlay::Temperature | WeatherOverlay::Pressure => {
                let idx = if ov == WeatherOverlay::Temperature { 0 } else { 5 };
                let pts: Vec<(f32, f32, f32)> =
                    self.all_weather_points(bundles, ts).into_iter().map(|p| (p.lat as f32, p.lon as f32, p.v[idx])).collect();
                analysed(&pts).map_or_else(String::new, |v| {
                    format!("≈ {}", if idx == 0 { settings.format_temp(v as f64) } else { settings.format_pressure(v as f64) })
                })
            }
            WeatherOverlay::WindFlow => {
                // Zero-allocation IDW wind interpolation over grid + bundles!
                let (mut ws, mut us, mut vs, mut ss) = (0.0, 0.0, 0.0, 0.0);
                let mut add_w = |la: f64, lo: f64, wu: f64, wv: f64, wspd: f64| {
                    let (dl, dlat) = ((lo - lon).abs(), la - lat);
                    let w = 1.0 / (dlat * dlat + dl.min(360.0 - dl).powi(2) + 0.15);
                    ws += w;
                    us += wu * w;
                    vs += wv * w;
                    ss += wspd * w;
                };
                for p in self.grid.data.values() {
                    let pt = p.at_timestamp(ts);
                    add_w(pt.lat, pt.lon, pt.v[2] as f64, pt.v[3] as f64, pt.v[4] as f64);
                }
                for b in bundles.values() {
                    let (wspd, wdir) = ts
                        .and_then(|t| b.hourly.iter().min_by_key(|h| (h.timestamp_unix - t).abs()))
                        .map_or((b.current.wind_speed_mps, b.current.wind_dir_deg), |h| (h.wind_speed_mps, Some(h.wind_dir_deg)));
                    // Calm wind has no direction and a zero vector; wind of unknown direction cannot be drawn.
                    let Some(wdir) = wdir.or((wspd == 0.0).then_some(0.0)) else { continue };
                    let (wu, wv) = crate::providers::wind_uv(wspd, wdir);
                    add_w(b.location.lat, b.location.lon, wu as f64, wv as f64, wspd);
                }
                if ws <= 0.0 {
                    return String::new();
                }
                let (u, v, spd) = (us / ws, vs / ws, ss / ws);
                let from_deg = ((-u).atan2(-v).to_degrees() + 360.0) % 360.0;
                format!("≈ {} {}", settings.format_wind(spd), compass_direction(from_deg))
            }
            WeatherOverlay::AirQuality => {
                let pts: Vec<(f32, f32, f32)> =
                    self.all_aqi_points(bundles).into_iter().map(|(la, lo, v)| (la as f32, lo as f32, v as f32)).collect();
                analysed(&pts).map_or_else(String::new, |v| format!("≈ AQI {:.0}", v.clamp(0.0, 500.0)))
            }
            _ => {
                let (ns, ew) = (if lat >= 0.0 { "N" } else { "S" }, if lon >= 0.0 { "E" } else { "W" });
                format!("{:.2}° {ns}, {:.2}° {ew}", lat.abs(), lon.abs())
            }
        }
    }

    /// Computes cached marching-squares pressure isobar contours (every 4 hPa) + synoptic H/L centers.
    pub(super) fn build_pressure_isobars(&self, vp: MapVp, bundles: &HashMap<String, WeatherBundle>, ts: Option<i64>) -> IsobarData {
        let (bounds, lat, lon, zoom) = vp;
        let (cwx, cwy) = lat_lon_to_world_px(lat, lon, zoom);
        let sig = hash_u64(&[
            bounds.width.round() as u64,
            bounds.height.round() as u64,
            (cwx * 0.25).round() as i64 as u64,
            (cwy * 0.25).round() as i64 as u64,
            (zoom * 50.0).round() as u64,
            self.grid_gen,
            bundles.len() as u64,
            ts.map_or(0, |t| (t / 180) as u64),
        ]);
        if let Ok(g) = self.isobar_cache.lock()
            && let Some(c) = g.as_ref()
            && c.sig == sig
        {
            return c.data.clone();
        }
        let pts: Vec<(f32, f32, f32)> =
            self.all_weather_points(bundles, ts).into_iter().map(|p| (p.lat as f32, p.lon as f32, p.v[5])).collect();
        if pts.len() < 3 {
            return IsobarData::default();
        }
        let ((gw, gh), (lats, lons)) = ((64_usize, 48_usize), grid_coords((64, 48), vp, false));
        let analysis = SampleAnalysis::new(&pts, zoom);
        let lon_tbl = analysis.lon_table(&lons);
        let mut field = vec![0.0_f32; gw * gh];
        let mut near2 = vec![0.0_f32; gw];
        for (gy, &la) in lats.iter().enumerate() {
            analysis.eval_row(la, &lon_tbl, &mut field[gy * gw..(gy + 1) * gw], &mut near2);
        }
        // One separable [1, 2, 1] pass over the contouring grid: a single node that pokes a fraction of a hectopascal
        // above a contour level would otherwise draw a stray one-cell diamond.
        let raw = field.clone();
        for gy in 0..gh {
            for gx in 0..gw {
                let (x0, x1, y0, y1) = (gx.saturating_sub(1), (gx + 1).min(gw - 1), gy.saturating_sub(1), (gy + 1).min(gh - 1));
                let row = |y: usize| raw[y * gw + x0] + 2.0 * raw[y * gw + gx] + raw[y * gw + x1];
                field[gy * gw + gx] = (row(y0) + 2.0 * row(gy) + row(y1)) / 16.0;
            }
        }
        let (vmin, vmax) = field.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        let (dx, dy) = (bounds.width / (gw - 1) as f32, bounds.height / (gh - 1) as f32);
        let mut data = IsobarData::default();
        let l0 = ((vmin / 4.0).floor() as i32) * 4;
        let l1 = ((vmax / 4.0).ceil() as i32) * 4;
        for lv_i in (l0..=l1).step_by(4) {
            let lv = lv_i as f32;
            let mut lbl_pt: Option<Point> = None;
            let mut best_center_d = f32::INFINITY;
            for gy in 0..gh - 1 {
                for gx in 0..gw - 1 {
                    let (v0, v1, v2, v3) =
                        (field[gy * gw + gx], field[gy * gw + gx + 1], field[(gy + 1) * gw + gx + 1], field[(gy + 1) * gw + gx]);
                    let m =
                        usize::from(v0 >= lv) | (usize::from(v1 >= lv) << 1) | (usize::from(v2 >= lv) << 2) | (usize::from(v3 >= lv) << 3);
                    if m == 0 || m == 15 {
                        continue;
                    }
                    let (x0, y0) = (gx as f32 * dx, gy as f32 * dy);
                    let interp = |a: f32, b: f32| ((lv - a) / (b - a)).clamp(0.05, 0.95);
                    let e = |edge: u8| match edge {
                        0 => Point::new(x0 + interp(v0, v1) * dx, y0),
                        1 => Point::new(x0 + dx, y0 + interp(v1, v2) * dy),
                        2 => Point::new(x0 + interp(v3, v2) * dx, y0 + dy),
                        _ => Point::new(x0, y0 + interp(v0, v3) * dy),
                    };
                    let pairs: &[(u8, u8)] = match m {
                        1 | 14 => &[(3, 0)],
                        2 | 13 => &[(0, 1)],
                        3 | 12 => &[(3, 1)],
                        4 | 11 => &[(1, 2)],
                        5 => &[(3, 2), (0, 1)],
                        6 | 9 => &[(0, 2)],
                        7 | 8 => &[(3, 2)],
                        10 => &[(3, 0), (1, 2)],
                        _ => &[],
                    };
                    for &(ea, eb) in pairs {
                        let (pa, pb) = (e(ea), e(eb));
                        data.segs.push((pa, pb));
                        let mp = Point::new((pa.x + pb.x) * 0.5, (pa.y + pb.y) * 0.5);
                        if mp.x > 130.0 && mp.x < bounds.width - 90.0 && mp.y > 70.0 && mp.y < bounds.height - 110.0 {
                            let cd = (mp.x - bounds.width * 0.5).hypot(mp.y - bounds.height * 0.5);
                            if cd < best_center_d {
                                best_center_d = cd;
                                lbl_pt = Some(mp);
                            }
                        }
                    }
                }
            }
            if let Some(lp) = lbl_pt
                && data.labels.iter().all(|(o, _)| (o.x - lp.x).hypot(o.y - lp.y) >= 42.0)
            {
                data.labels.push((lp, format!("{lv_i}")));
            }
        }
        // Detect crisp local synoptic High (H) and Low (L) pressure centers (decluttered >= 170px apart).
        for gy in 2..gh - 2 {
            for gx in 2..gw - 2 {
                let v = field[gy * gw + gx];
                let (mut is_max, mut is_min) = (v >= 1016.0, v <= 1009.5);
                if !is_max && !is_min {
                    continue;
                }
                for ny in gy - 2..=gy + 2 {
                    for nx in gx - 2..=gx + 2 {
                        if nx == gx && ny == gy {
                            continue;
                        }
                        let nv = field[ny * gw + nx];
                        if nv >= v {
                            is_max = false;
                        }
                        if nv <= v {
                            is_min = false;
                        }
                    }
                }
                if is_max || is_min {
                    let pt = Point::new(gx as f32 * dx, gy as f32 * dy);
                    if pt.x > 130.0
                        && pt.x < bounds.width - 80.0
                        && pt.y > 65.0
                        && pt.y < bounds.height - 105.0
                        && data.centers.iter().all(|(o, _, _)| (o.x - pt.x).hypot(o.y - pt.y) >= 170.0)
                    {
                        data.centers.push((pt, is_max, v));
                    }
                }
            }
        }
        let _ = self.isobar_cache.lock().map(|mut g| *g = Some(IsobarCache { sig, data: data.clone() }));
        data
    }

    /// Builds a geo-anchored 2.0x overscanned astronomical day/night twilight terminator texture (like Apple Maps).
    pub(super) fn build_terminator_overlay_handle(
        &self, vp: MapVp, base_style: BaseMapStyle, ts_unix: i64,
    ) -> Option<(adv_image::Handle, Rectangle, f32)> {
        let (bounds, lat, lon, zoom) = vp;
        if !self.show_terminator || zoom > 7.8 {
            return None;
        }
        let zoom_fade = ((7.8 - zoom) / 1.2).clamp(0.0, 1.0) as f32;
        let dark_base = matches!(base_style, BaseMapStyle::AppleDark | BaseMapStyle::Satellite);
        let bucket = (ts_unix / 1800) as u64;
        let meta_sig = hash_u64(&[bucket, u64::from(dark_base)]);
        let (cwx, cwy) = lat_lon_to_world_px(lat, lon, zoom);
        let ws = 256.0 * 2.0_f64.powf(zoom);
        if let Ok(g) = self.terminator_tex.lock()
            && let Some(c) = g.as_ref()
            && c.meta_sig == meta_sig
            && (zoom - c.zoom).abs() <= 0.55
        {
            let sc = 2.0_f64.powf(zoom - c.zoom);
            let (mut x0, mut x1, y0, y1) = (c.wx0 * sc, c.wx1 * sc, c.wy0 * sc, c.wy1 * sc);
            let mid = (x0 + x1) * 0.5;
            if mid - cwx > ws * 0.5 {
                (x0, x1) = (x0 - ws, x1 - ws);
            } else if cwx - mid > ws * 0.5 {
                (x0, x1) = (x0 + ws, x1 + ws);
            }
            let (sx0, sy0) = ((bounds.width as f64 * 0.5 + x0 - cwx) as f32, (bounds.height as f64 * 0.5 + y0 - cwy) as f32);
            let (sw, sh) = (((x1 - x0) as f32).max(1.0), ((y1 - y0) as f32).max(1.0));
            if sx0 <= 1.0 && sy0 <= 1.0 && sx0 + sw >= bounds.width - 1.0 && sy0 + sh >= bounds.height - 1.0 {
                return Some((c.handle.clone(), Rectangle { x: sx0, y: sy0, width: sw, height: sh }, zoom_fade));
            }
        }
        let (fw, fh) = (192_usize, 144_usize);
        let (ov_w, ov_h) = (bounds.width.max(64.0) as f64 * 1.85, bounds.height.max(64.0) as f64 * 1.85);
        let (wx0, wx1, wy0, wy1) = (cwx - ov_w * 0.5, cwx + ov_w * 0.5, cwy - ov_h * 0.5, cwy + ov_h * 0.5);
        let (lats, lons) = grid_coords((fw, fh), (Size::new(ov_w as f32, ov_h as f32), lat, lon, zoom), true);
        let (slat_deg, slon_deg) = subsolar_point(ts_unix);
        let (slat_sin, slat_cos) = slat_deg.to_radians().sin_cos();
        let lut: &[[u8; 4]; 256] = if dark_base { &TERM_LUT_DARK } else { &TERM_LUT_LIGHT };
        // Precompute cos(dlon) once per column (192 cos calls instead of 27,648!).
        let cos_dlon: Vec<f32> = lons.iter().map(|&lo| ((lo as f64 - slon_deg).to_radians().cos()) as f32).collect();
        // Pre-fill transparent daytime pixels with twilight edge RGB [36, 24, 56, 0] so GPU linear filtering has zero dark halo
        // without needing a post-pass `bleed_transparent_edges`!
        let mut rgba = vec![0_u8; fw * fh * 4];
        const INV_SPAN: f32 = 255.0 / 0.280;
        for (gy, &la) in lats.iter().enumerate() {
            let (ls, lc) = (la as f64).to_radians().sin_cos();
            let (a_term, b_term) = ((ls * slat_sin) as f32, (lc * slat_cos) as f32);
            let row = &mut rgba[gy * fw * 4..(gy + 1) * fw * 4];
            for (px, &cd) in row.as_chunks_mut::<4>().0.iter_mut().zip(&cos_dlon) {
                let sin_alt = a_term + b_term * cd;
                if sin_alt >= 0.045 {
                    *px = [36, 24, 56, 0];
                } else {
                    let idx = ((0.045 - sin_alt) * INV_SPAN).clamp(0.0, 255.0) as usize;
                    *px = lut[idx];
                }
            }
        }
        let handle = adv_image::Handle::from_rgba(fw as u32, fh as u32, rgba);
        let _ = self.terminator_tex.lock().map(|mut g| {
            *g = Some(ScalarCache { meta_sig, zoom, wx0, wy0, wx1, wy1, handle: handle.clone() });
        });
        let dst = Rectangle {
            x: (bounds.width as f64 * 0.5 - ov_w * 0.5) as f32,
            y: (bounds.height as f64 * 0.5 - ov_h * 0.5) as f32,
            width: ov_w as f32,
            height: ov_h as f32,
        };
        Some((handle, dst, zoom_fade))
    }
}

/// Computes an Apple Maps overview `(center_lat, center_lon, zoom)` framing all saved cities with HUD margin.
pub(super) fn compute_overview_camera(locs: &[Location], vp: Size) -> Option<(f64, f64, f64)> {
    if locs.is_empty() {
        return None;
    }
    if locs.len() == 1 {
        return Some((locs[0].lat.clamp(-82.0, 82.0), wrap_lon(locs[0].lon), 6.8));
    }
    let mut lons: Vec<f64> = locs.iter().map(|l| wrap_lon(l.lon)).collect();
    lons.sort_by(f64::total_cmp);
    // Find the largest longitudinal gap so framing works seamlessly across the Pacific/antimeridian!
    let mut best_gap = (lons[0] + 360.0) - lons[lons.len() - 1];
    let mut gap_end_idx = 0_usize;
    for i in 1..lons.len() {
        let g = lons[i] - lons[i - 1];
        if g > best_gap {
            best_gap = g;
            gap_end_idx = i;
        }
    }
    let lon_span = (360.0 - best_gap).max(0.4);
    let min_lon = lons[gap_end_idx];
    let center_lon = wrap_lon(min_lon + lon_span * 0.5);
    let (mut min_wy, mut max_wy) = (f64::INFINITY, f64::NEG_INFINITY);
    for l in locs {
        let (_, wy) = lat_lon_to_world_px(l.lat.clamp(-80.0, 80.0), l.lon, 0.0);
        min_wy = min_wy.min(wy);
        max_wy = max_wy.max(wy);
    }
    let (center_lat, _) = world_px_to_lat_lon(128.0, (min_wy + max_wy) * 0.5, 0.0);
    let usable_w = (vp.width.max(420.0) as f64) * 0.68;
    let usable_h = (vp.height.max(300.0) as f64) * 0.62;
    let wx_span0 = (lon_span / 360.0 * 256.0).max(0.5);
    let wy_span0 = (max_wy - min_wy).max(0.5);
    let zx = (usable_w / wx_span0).log2();
    let zy = (usable_h / wy_span0).log2();
    Some((center_lat.clamp(-80.0, 80.0), center_lon, zx.min(zy).clamp(2.3, 10.5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 4×5 lattice `poll_grid_samples` requests around `(lat, lon)` at `zoom`, sampled from `f`.
    fn lattice(lat: f32, lon: f32, zoom: f64, f: impl Fn(f32, f32) -> f32) -> Vec<(f32, f32, f32)> {
        let span = grid_sample_span_deg(zoom) as f32;
        let mut pts = Vec::new();
        for gy in 0..4 {
            for gx in 0..5 {
                let (la, lo) = (lat + ((gy as f32 - 1.5) / 3.0) * span * 1.15, lon + ((gx as f32 - 2.0) / 4.0) * span * 1.5);
                pts.push((la, lo, f(la, lo)));
            }
        }
        pts
    }

    #[test]
    fn sample_analysis_honors_samples_and_keeps_real_gradients() {
        // A deep low west of the viewport and a ridge inland: ~35 hPa across the lattice, like a Pacific winter storm.
        let truth = |la: f32, lo: f32| 1024.0 - 40.0 * (-((la - 44.0).powi(2) + ((lo + 134.0) * 0.75).powi(2)) / 90.0).exp();
        let pts = lattice(37.87, -122.27, 5.0, truth);
        let a = SampleAnalysis::new(&pts, 5.0);
        // 1. The field passes (nearly) through every sample: a pin's value and the contours around it agree.
        let worst_at_samples = pts.iter().map(|&(la, lo, v)| (a.eval_point(la, lo) - v).abs()).fold(0.0_f32, f32::max);
        assert!(worst_at_samples < 1.0, "analysis misses its own samples by {worst_at_samples:.2} hPa");
        // 2. Between samples it tracks the true field far better than one contour interval (4 hPa) on average.
        let (lats, lons): (Vec<f32>, Vec<f32>) =
            ((0..24).map(|i| 29.0 + i as f32 * 0.75).collect(), (0..32).map(|i| -134.0 + i as f32 * 0.75).collect());
        let tbl = a.lon_table(&lons);
        let (mut row, mut near2) = (vec![0.0_f32; lons.len()], vec![0.0_f32; lons.len()]);
        let (mut se, mut n) = (0.0_f64, 0_u32);
        for &la in &lats {
            a.eval_row(la, &tbl, &mut row, &mut near2);
            for (&v, &lo) in row.iter().zip(&lons) {
                // Row evaluation and point evaluation are the same analysis.
                assert!((v - a.eval_point(la, lo)).abs() < 0.02);
                se += ((v - truth(la, lo)) as f64).powi(2);
                n += 1;
            }
        }
        let rms = (se / n as f64).sqrt();
        assert!(rms < 2.0, "analysis rms error {rms:.2} hPa");
        // 3. A single isolated sample yields a constant field, and its distance aura is exact.
        let one = SampleAnalysis::new(&[(10.0, 20.0, 42.0)], 5.0);
        let tbl = one.lon_table(&[20.0, 23.0]);
        one.eval_row(10.0, &tbl, &mut row[..2], &mut near2[..2]);
        assert!((row[0] - 42.0).abs() < 1e-3 && (row[1] - 42.0).abs() < 1e-3 && near2[0] < 1e-6);
        assert!((near2[1].sqrt() - 3.0 * 10.0_f32.to_radians().cos()).abs() < 1e-3);
    }
}
