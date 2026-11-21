use super::tiles::{MapViewport, bleed_transparent_edges};
use super::{MapEngine, WeatherOverlay};
use crate::icons::{aqi_to_color, pressure_to_color, ramp, temp_to_color};
use crate::model::WeatherBundle;
use crate::providers::GridWeatherPoint;
use crate::style::rgba8;
use iced::advanced::image as adv_image;
use iced::{Color, Point, Rectangle, Size};
use std::collections::HashMap;
use std::f64::consts::PI;

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
}

pub(super) fn init_wind_particles() -> Vec<WindParticle> {
    (0..300)
        .map(|i| {
            let s1 = ((i as f32 * 97.13).sin() * 43_758.547).fract().abs();
            let s2 = ((i as f32 * 241.79).cos() * 28_411.91).fract().abs();
            let s3 = ((i as f32 * 53.41).sin() * 11_937.33).fract().abs();
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

fn grid_coords((gw, gh): (usize, usize), vp: MapViewport, centered: bool) -> (Vec<f32>, Vec<f32>) {
    let (w, h, ws) = (
        vp.bounds.width.max(64.0) as f64,
        vp.bounds.height.max(64.0) as f64,
        256.0 * 2.0_f64.powf(vp.zoom),
    );
    let (cwx, cwy) = lat_lon_to_world_px(vp.lat, vp.lon, vp.zoom);
    let (off, dx, dy) = if centered {
        (0.5, gw as f64, gh as f64)
    } else {
        (0.0, (gw - 1).max(1) as f64, (gh - 1).max(1) as f64)
    };
    let lons = (0..gw)
        .map(|gx| {
            wrap_lon(((cwx + (gx as f64 + off) / dx * w - w * 0.5) / ws) * 360.0 - 180.0) as f32
        })
        .collect();
    let lats = (0..gh)
        .map(|gy| {
            let wy = (cwy + (gy as f64 + off) / dy * h - h * 0.5).clamp(0.0, ws);
            (PI - (2.0 * PI * wy) / ws).sinh().atan().to_degrees() as f32
        })
        .collect();
    (lats, lons)
}

impl MapEngine {
    pub(super) fn step_wind_particles(
        &mut self,
        bundles: &HashMap<String, WeatherBundle>,
        dt_sec: f32,
    ) {
        let (dt, ts) = (
            dt_sec.clamp(0.004, 0.06),
            self.selected_forecast_timestamp(),
        );
        let (vw, vh, gw, gh) = (900.0_f32, 760.0_f32, 16_usize, 12_usize);
        let sig = [
            (self.center_lat * 100.0).round() as i64 as u64,
            (self.center_lon * 100.0).round() as i64 as u64,
            (self.zoom * 100.0).round() as u64,
            self.grid_gen,
            bundles.len() as u64,
            ts.map_or(0, |t| (t / 180) as u64),
        ]
        .into_iter()
        .fold(0_u64, |a, v| a.wrapping_mul(31).wrapping_add(v));
        let grid = if let Some((s, g)) = self.wind_grid
            && s == sig
        {
            g
        } else {
            let pts = self.all_weather_points(bundles, ts);
            if pts.is_empty() {
                return;
            }
            let vp = MapViewport {
                bounds: Size::new(vw, vh),
                lat: self.center_lat,
                lon: self.center_lon,
                zoom: self.zoom,
            };
            let (lats, lons) = grid_coords((gw, gh), vp, false);
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
        for (idx, p) in self.particles.iter_mut().enumerate() {
            let (dx1, dy1, s1) = sample_wind(&grid, gw, gh, p.u, p.v);
            let px1 = 26.0 + s1 * 15.5;
            let (dx2, dy2, s2) = sample_wind(
                &grid,
                gw,
                gh,
                (p.u + dx1 * px1 * dt * 0.5 / vw).clamp(0.0, 1.0),
                (p.v + dy1 * px1 * dt * 0.5 / vh).clamp(0.0, 1.0),
            );
            let px2 = 26.0 + s2 * 15.5;
            (p.dx, p.dy, p.spd) = (dx2, dy2, s2);
            p.u += dx2 * px2 * dt / vw;
            p.v += dy2 * px2 * dt / vh;
            p.age += dt;
            if p.age >= p.max_age || !(0.0..=1.0).contains(&p.u) || !(0.0..=1.0).contains(&p.v) {
                let seed = idx as f32 * 1.17 + p.max_age * 13.3 + p.age * 7.1;
                (p.u, p.v, p.age) = (
                    ((seed * 73.7).sin() * 43_758.547).fract().abs(),
                    ((seed * 191.3).cos() * 19_421.13).fract().abs(),
                    0.0,
                );
                (p.dx, p.dy, p.spd) = sample_wind(&grid, gw, gh, p.u, p.v);
                (p.trail, p.tlen) = ([(p.u, p.v); WIND_TRAIL_LEN], 1);
            } else if ((p.u - p.trail[0].0) * vw).powi(2) + ((p.v - p.trail[0].1) * vh).powi(2)
                >= 12.0
                || p.tlen < 2
            {
                p.trail.copy_within(0..WIND_TRAIL_LEN - 1, 1);
                p.trail[0] = (p.u, p.v);
                p.tlen = (p.tlen + 1).min(WIND_TRAIL_LEN as u8);
            }
        }
    }

    pub(super) fn build_scalar_overlay_handle(
        &self,
        vp: MapViewport,
        ov: WeatherOverlay,
        bundles: &HashMap<String, WeatherBundle>,
        ts: Option<i64>,
        hrrr: bool,
    ) -> Option<adv_image::Handle> {
        let global_p = ov == WeatherOverlay::RadarTimeline
            && ts.is_some()
            && (!hrrr || !viewport_inside_hrrr(vp.lat, vp.lon, vp.zoom));
        if !matches!(
            ov,
            WeatherOverlay::Temperature
                | WeatherOverlay::AirQuality
                | WeatherOverlay::WindFlow
                | WeatherOverlay::Pressure
        ) && !global_p
        {
            return None;
        }
        let (cwx, cwy) = lat_lon_to_world_px(vp.lat, vp.lon, vp.zoom);
        let sig = [
            vp.bounds.width.round() as u64,
            vp.bounds.height.round() as u64,
            cwx.round() as i64 as u64,
            cwy.round() as i64 as u64,
            (vp.zoom * 10_000.0).round() as u64,
            ov as u64,
            self.grid_gen,
            bundles.len() as u64,
            ts.map_or(0, |t| (t / 180) as u64),
            hrrr as u64,
        ]
        .into_iter()
        .fold(0_u64, |a, v| a.wrapping_mul(31).wrapping_add(v));
        if let Ok(g) = self.scalar_tex.lock()
            && let Some((s, h)) = g.as_ref()
            && *s == sig
        {
            return Some(h.clone());
        }
        let (fw, fh) = (96_usize, 64_usize);
        let (lats, lons) = grid_coords((fw, fh), vp, true);
        let raw_samples: Vec<(f32, f32, f32)> = if ov == WeatherOverlay::AirQuality {
            self.all_aqi_points(bundles)
                .into_iter()
                .map(|(la, lo, v)| (la as f32, lo as f32, v as f32))
                .collect()
        } else {
            let idx = match ov {
                WeatherOverlay::Temperature => 0,
                WeatherOverlay::RadarTimeline => 1,
                WeatherOverlay::WindFlow => 4,
                _ => 5,
            };
            self.all_weather_points(bundles, ts)
                .into_iter()
                .map(|pt| (pt.lat as f32, pt.lon as f32, pt.v[idx]))
                .collect()
        };
        // Spatial viewport culling + Structure-of-Arrays for branchless SIMD-friendly IDW.
        let (clat_f, clon_f) = (vp.lat as f32, vp.lon as f32);
        let lat_half = (lats[0] - lats[fh - 1]).abs() * 0.5 + 14.0;
        let lon_span = (360.0 / 2.0_f32.powf((vp.zoom - 1.5).max(0.0) as f32)).min(180.0);
        let lon_half = (lon_span * 0.85 + 18.0).min(180.0);
        let mut plat = Vec::with_capacity(raw_samples.len());
        let mut plon = Vec::with_capacity(raw_samples.len());
        let mut pval = Vec::with_capacity(raw_samples.len());
        for &(la, lo, v) in &raw_samples {
            let dl = (lo - clon_f).abs();
            if (la - clat_f).abs() <= lat_half && dl.min(360.0 - dl) <= lon_half {
                plat.push(la);
                plon.push(lo);
                pval.push(v);
            }
        }
        if plat.len() < 8 && raw_samples.len() > plat.len() {
            plat.clear();
            plon.clear();
            pval.clear();
            for &(la, lo, v) in &raw_samples {
                plat.push(la);
                plon.push(lo);
                pval.push(v);
            }
        }
        const P_STOPS: [(f64, [u8; 3]); 5] = [
            (0.08, [72, 184, 255]),
            (1.2, [54, 160, 255]),
            (4.5, [148, 82, 235]),
            (12.0, [236, 64, 55]),
            (27.0, [250, 224, 67]),
        ];
        let mut rgba = vec![0_u8; fw * fh * 4];
        for (gy, &lat) in lats.iter().enumerate() {
            for (gx, &lon) in lons.iter().enumerate() {
                let col = match ov {
                    WeatherOverlay::RadarTimeline if hrrr && in_hrrr(lat as f64, lon as f64) => {
                        Color::TRANSPARENT
                    }
                    WeatherOverlay::RadarTimeline => {
                        let r = idw_soa((&plat, &plon, &pval), (lat, lon), 0.08, true, 0.0) as f64;
                        if r < 0.08 {
                            Color::TRANSPARENT
                        } else {
                            ramp(
                                r,
                                &P_STOPS,
                                ((0.22 + (r / 4.5).min(1.0) * 0.56) as f32).min(0.80),
                            )
                        }
                    }
                    WeatherOverlay::Temperature => temp_to_color(
                        idw_soa((&plat, &plon, &pval), (lat, lon), 0.12, true, 15.0) as f64,
                        0.58,
                    ),
                    WeatherOverlay::WindFlow => {
                        let b = (idw_soa((&plat, &plon, &pval), (lat, lon), 0.12, true, 3.0)
                            / 26.0)
                            .clamp(0.0, 1.0);
                        rgba8(
                            (12.0 + b * 34.0) as u8,
                            (88.0 + b * 58.0) as u8,
                            (156.0 + b * 52.0) as u8,
                            0.64,
                        )
                    }
                    WeatherOverlay::Pressure => pressure_to_color(
                        idw_soa((&plat, &plon, &pval), (lat, lon), 0.12, true, 1013.25) as f64,
                        0.56,
                    ),
                    WeatherOverlay::AirQuality => Color {
                        a: 0.50,
                        ..aqi_to_color(
                            idw_soa((&plat, &plon, &pval), (lat, lon), 0.25, false, 32.0).round()
                                as i32,
                        )
                    },
                    _ => Color::TRANSPARENT,
                };
                let o = (gy * fw + gx) * 4;
                rgba[o..o + 4].copy_from_slice(&[
                    (col.r.clamp(0.0, 1.0) * 255.0) as u8,
                    (col.g.clamp(0.0, 1.0) * 255.0) as u8,
                    (col.b.clamp(0.0, 1.0) * 255.0) as u8,
                    (col.a.clamp(0.0, 1.0) * 255.0) as u8,
                ]);
            }
        }
        if ov == WeatherOverlay::RadarTimeline {
            bleed_transparent_edges(&mut rgba, fw as u32, fh as u32);
        }
        let h = adv_image::Handle::from_rgba(fw as u32, fh as u32, rgba);
        if let Ok(mut g) = self.scalar_tex.lock() {
            *g = Some((sig, h.clone()));
        }
        Some(h)
    }
}

pub(super) fn declutter_badges<'a>(
    badges: &'a [CityBadge],
    clat: f64,
    clon: f64,
    zoom: f64,
    rect: Rectangle,
) -> Vec<(usize, &'a CityBadge, Point, Point)> {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    let (cwx, cwy) = lat_lon_to_world_px(clat, clon, zoom);
    let mut out: Vec<(usize, &'a CityBadge, Point, Point)> = Vec::with_capacity(badges.len());
    for active_pass in [true, false] {
        for (i, b) in badges.iter().enumerate() {
            if b.active != active_pass {
                continue;
            }
            let (mut wx, wy) = lat_lon_to_world_px(b.lat, b.lon, zoom);
            if wx - cwx > ws * 0.5 {
                wx -= ws;
            } else if cwx - wx > ws * 0.5 {
                wx += ws;
            }
            let pos = Point::new(
                rect.width * 0.5 + (wx - cwx) as f32,
                rect.height * 0.5 + (wy - cwy) as f32,
            );
            if pos.x < -36.0
                || pos.x > rect.width + 36.0
                || pos.y < -36.0
                || pos.y > rect.height + 36.0
            {
                continue;
            }
            let disc = Point::new(pos.x, if b.active { pos.y - 31.0 } else { pos.y });
            if !b.active
                && out
                    .iter()
                    .any(|(_, _, _, dc)| (dc.x - disc.x).powi(2) + (dc.y - disc.y).powi(2) < 2704.0)
            {
                continue;
            }
            out.push((i, b, pos, disc));
        }
    }
    out
}

fn sample_wind(grid: &[(f32, f32, f32)], gw: usize, gh: usize, u: f32, v: f32) -> (f32, f32, f32) {
    let (gx, gy) = (
        u.clamp(0.0, 1.0) * (gw - 1) as f32,
        v.clamp(0.0, 1.0) * (gh - 1) as f32,
    );
    let (x0, y0) = (gx.floor() as usize, gy.floor() as usize);
    let (x1, y1, tx, ty) = (
        (x0 + 1).min(gw - 1),
        (y0 + 1).min(gh - 1),
        gx - x0 as f32,
        gy - y0 as f32,
    );
    let (c00, c10, c01, c11) = (
        grid[y0 * gw + x0],
        grid[y0 * gw + x1],
        grid[y1 * gw + x0],
        grid[y1 * gw + x1],
    );
    let bi = |a: f32, b: f32, c: f32, d: f32| {
        (a * (1.0 - tx) + b * tx) * (1.0 - ty) + (c * (1.0 - tx) + d * tx) * ty
    };
    let (dx, dy, spd) = (
        bi(c00.0, c10.0, c01.0, c11.0),
        bi(c00.1, c10.1, c01.1, c11.1),
        bi(c00.2, c10.2, c01.2, c11.2),
    );
    let n = dx.hypot(dy).max(1e-3);
    (dx / n, dy / n, spd)
}

pub fn lat_lon_to_world_px(lat: f64, lon: f64, zoom: f64) -> (f64, f64) {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    let merc = (PI * 0.25 + lat.clamp(-85.05112878, 85.05112878).to_radians() * 0.5)
        .tan()
        .ln();
    (((lon + 180.0) / 360.0) * ws, (0.5 - merc / (2.0 * PI)) * ws)
}
pub fn world_px_to_lat_lon(wx: f64, wy: f64, zoom: f64) -> (f64, f64) {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    (
        (PI - (2.0 * PI * wy.clamp(0.0, ws)) / ws)
            .sinh()
            .atan()
            .to_degrees(),
        wrap_lon((wx / ws) * 360.0 - 180.0),
    )
}
pub(super) fn project_lat_lon_to_screen(
    lat: f64,
    lon: f64,
    clat: f64,
    clon: f64,
    zoom: f64,
    rect: Rectangle,
) -> Point {
    let ws = 256.0 * 2.0_f64.powf(zoom);
    let ((cwx, cwy), (mut wx, wy)) = (
        lat_lon_to_world_px(clat, clon, zoom),
        lat_lon_to_world_px(lat, lon, zoom),
    );
    if wx - cwx > ws * 0.5 {
        wx -= ws;
    } else if cwx - wx > ws * 0.5 {
        wx += ws;
    }
    Point::new(
        rect.width * 0.5 + (wx - cwx) as f32,
        rect.height * 0.5 + (wy - cwy) as f32,
    )
}
pub(super) fn project_screen_to_lat_lon(
    pos: Point,
    clat: f64,
    clon: f64,
    zoom: f64,
    rect: Rectangle,
) -> (f64, f64) {
    let (cwx, cwy) = lat_lon_to_world_px(clat, clon, zoom);
    world_px_to_lat_lon(
        cwx + (pos.x - rect.width * 0.5) as f64,
        cwy + (pos.y - rect.height * 0.5) as f64,
        zoom,
    )
}
pub(super) fn wrap_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}
pub(super) fn quantize_coord(lat: f64, lon: f64) -> (i32, i32) {
    (
        (lat * 2.0).round() as i32,
        (wrap_lon(lon) * 2.0).round() as i32,
    )
}
fn in_hrrr(lat: f64, lon: f64) -> bool {
    (22.0..=52.0).contains(&lat) && (-133.5..=-61.0).contains(&lon)
}
fn viewport_inside_hrrr(clat: f64, clon: f64, zoom: f64) -> bool {
    let hs = 90.0 / 2.0_f64.powf((zoom - 2.0).max(0.0));
    in_hrrr(clat - hs, clon - hs * 1.3) && in_hrrr(clat + hs, clon + hs * 1.3)
}

fn idw_soa(
    (plat, plon, pval): (&[f32], &[f32], &[f32]),
    (lat, lon): (f32, f32),
    eps: f32,
    quartic: bool,
    fb: f32,
) -> f32 {
    let (mut ws, mut acc) = (0.0_f32, 0.0_f32);
    if quartic {
        for i in 0..plat.len() {
            let dla = plat[i] - lat;
            let dl = (plon[i] - lon).abs();
            let dl = dl.min(360.0 - dl);
            let d2 = dla * dla + dl * dl + eps;
            let w = 1.0 / (d2 * d2);
            ws += w;
            acc += pval[i] * w;
        }
    } else {
        for i in 0..plat.len() {
            let dla = plat[i] - lat;
            let dl = (plon[i] - lon).abs();
            let dl = dl.min(360.0 - dl);
            let w = 1.0 / (dla * dla + dl * dl + eps);
            ws += w;
            acc += pval[i] * w;
        }
    }
    if ws > 0.0 { acc / ws } else { fb }
}

pub(super) fn idw_wind(pts: &[GridWeatherPoint], lat: f64, lon: f64) -> (f64, f64, f64) {
    let (mut ws, mut us, mut vs, mut ss) = (0.0, 0.0, 0.0, 0.0);
    for pt in pts {
        let dl = (pt.lon - lon).abs();
        let dl = dl.min(360.0 - dl);
        let dlat = pt.lat - lat;
        let w = 1.0 / (dlat * dlat + dl * dl + 0.15);
        ws += w;
        us += pt.v[2] as f64 * w;
        vs += pt.v[3] as f64 * w;
        ss += pt.v[4] as f64 * w;
    }
    if ws > 0.0 {
        (us / ws, vs / ws, ss / ws)
    } else {
        (2.0, 0.5, 2.0)
    }
}
