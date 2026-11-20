use super::{WorkerEvent, lat_lon_to_world_px};
use crate::icons::ramp;
use crate::providers::WeatherClient;
use crate::storage::{delete_cached_tile, is_shutting_down, load_cached_tile, save_cached_tile};
use iced::advanced::image as adv_image;
use iced::widget::canvas::Frame;
use iced::{Rectangle, Size};
use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct TileKey(pub String, pub u8, pub u32, pub u32);

type TileCoord = (u8, u32, u32);
type TileMap = HashMap<TileCoord, adv_image::Handle>;

pub(super) struct SharedTilePool {
    pub tiles: HashMap<String, TileMap>,
    pub lru: VecDeque<TileKey>,
    pub in_flight: HashSet<TileKey>,
    pub failed_until: HashMap<TileKey, Instant>,
    pub tx: mpsc::Sender<WorkerEvent>,
    pub client: WeatherClient,
}

impl SharedTilePool {
    pub fn has_tile(&self, ns: &str, c: TileCoord) -> bool {
        self.tiles.get(ns).is_some_and(|m| m.contains_key(&c))
    }

    pub fn ensure(
        &mut self,
        ns: &str,
        c: TileCoord,
        url_fn: impl FnOnce() -> String,
        ttl: Duration,
        high: bool,
    ) {
        if self.has_tile(ns, c) || is_shutting_down() || (!high && self.in_flight.len() >= 128) {
            return;
        }
        let key = TileKey(ns.into(), c.0, c.1, c.2);
        if self.in_flight.contains(&key)
            || self
                .failed_until
                .get(&key)
                .is_some_and(|u| Instant::now() < *u)
        {
            return;
        }
        let url = url_fn();
        if url.is_empty() {
            return;
        }
        self.in_flight.insert(key.clone());
        let (tx, client) = (self.tx.clone(), self.client.clone());
        let task = move || {
            if is_shutting_down() {
                return;
            }
            if let Some(cached) = load_cached_tile(&key.0, key.1, key.2, key.3, ttl) {
                if let Some(h) = decode_tile(&key.0, &cached) {
                    let _ = tx.send(WorkerEvent::TileLoaded(key, h));
                    return;
                }
                delete_cached_tile(&key.0, key.1, key.2, key.3);
            }
            if is_shutting_down() {
                return;
            }
            if let Ok(bytes) = client.fetch_bytes(&url)
                && let Some(h) = decode_tile(&key.0, &bytes)
            {
                let dk = key.clone();
                let _ = tx.send(WorkerEvent::TileLoaded(key, h));
                if !is_shutting_down() {
                    save_cached_tile(&dk.0, dk.1, dk.2, dk.3, &bytes);
                }
                return;
            }
            let _ = tx.send(WorkerEvent::TileFailed(key));
        };
        if high {
            rayon::spawn(task);
        } else {
            rayon::spawn_fifo(task);
        }
    }

    pub fn insert(&mut self, key: TileKey, handle: adv_image::Handle) {
        let coord = (key.1, key.2, key.3);
        let ns_map = self.tiles.entry(key.0.clone()).or_default();
        if !ns_map.contains_key(&coord) {
            self.lru.push_back(key.clone());
        }
        ns_map.insert(coord, handle);
        while self.lru.len() > 1024 {
            if let Some(old) = self.lru.pop_front()
                && let Some(m) = self.tiles.get_mut(&old.0)
            {
                m.remove(&(old.1, old.2, old.3));
                if m.is_empty() {
                    self.tiles.remove(&old.0);
                }
            }
        }
    }
}

fn decode_tile(ns: &str, bytes: &[u8]) -> Option<adv_image::Handle> {
    let rgba = ::image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let mut raw = rgba.into_raw();
    stylize_radar_tile(ns, &mut raw, w, h);
    bleed_transparent_edges(&mut raw, w, h);
    Some(adv_image::Handle::from_rgba(w, h, raw))
}

fn smooth_radar_alpha(rgba: &mut [u8], w: usize, h: usize) {
    if w < 3 || h < 3 || rgba.len() != w * h * 4 {
        return;
    }
    let mut tmp = vec![0_u16; rgba.len()];
    for y in 0..h {
        for x in 0..w {
            let (i0, i1, i2) = (
                (y * w + x.saturating_sub(1)) * 4,
                (y * w + x) * 4,
                (y * w + (x + 1).min(w - 1)) * 4,
            );
            let (a0, a1, a2) = (
                rgba[i0 + 3] as u32,
                rgba[i1 + 3] as u32,
                rgba[i2 + 3] as u32,
            );
            let asum = a0 + a1 * 2 + a2;
            if asum == 0 {
                continue;
            }
            for c in 0..3 {
                tmp[i1 + c] = ((rgba[i0 + c] as u32 * a0
                    + rgba[i1 + c] as u32 * a1 * 2
                    + rgba[i2 + c] as u32 * a2)
                    >> 2) as u16;
            }
            tmp[i1 + 3] = (asum >> 2) as u16;
        }
    }
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
        for x in 0..w {
            let (i0, i1, i2) = ((y0 * w + x) * 4, (y * w + x) * 4, (y1 * w + x) * 4);
            let asum = tmp[i0 + 3] as u32 + tmp[i1 + 3] as u32 * 2 + tmp[i2 + 3] as u32;
            if let Some(nz) = NonZeroU32::new(asum) {
                for c in 0..3 {
                    rgba[i1 + c] =
                        ((tmp[i0 + c] as u32 + tmp[i1 + c] as u32 * 2 + tmp[i2 + c] as u32) / nz)
                            .min(255) as u8;
                }
                rgba[i1 + 3] = ((asum + 2) >> 2).min(255) as u8;
            } else {
                rgba[i1 + 3] = 0;
            }
        }
    }
}

pub(super) fn stylize_radar_tile(ns: &str, rgba: &mut [u8], w: u32, h: u32) {
    let (refp, refd) = (ns.starts_with("hrrr_refp_"), ns.starts_with("hrrr_refd_"));
    if !refp && !refd && !ns.starts_with("lwxr256_") && !ns.starts_with("rv256_") {
        return;
    }
    const RAIN_STOPS: [(f64, [u8; 3]); 5] = [
        (0.0, [110, 198, 255]),
        (0.35, [56, 160, 255]),
        (0.65, [38, 102, 245]),
        (0.85, [162, 68, 230]),
        (1.0, [244, 171, 55]),
    ];
    if refp || refd {
        for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
            let a = px[3];
            if a < 10 {
                px[3] = 0;
                continue;
            }
            let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
            if refd && (r.max(g).max(b) - r.min(g).min(b)) < 28.0 && r > 110.0 && g > 110.0 {
                px[3] = 0;
                continue;
            }
            let (nr, ng, nb, na) = if refp && b > r + 14.0 && b >= g - 4.0 {
                let t = ((225.0 - r) / 215.0).clamp(0.0, 1.0);
                (
                    140.0 - t * 92.0,
                    224.0 - t * 56.0,
                    255.0 - t * 12.0,
                    125.0 + t * 105.0,
                )
            } else if refp && r > g + 18.0 && b > g + 12.0 {
                let t = ((230.0 - g) / 210.0).clamp(0.0, 1.0);
                (
                    212.0 + t * 30.0,
                    132.0 - t * 68.0,
                    248.0 - t * 52.0,
                    145.0 + t * 95.0,
                )
            } else {
                let inten = if r > 205.0 && g < 110.0 && b < 110.0 {
                    0.92 + ((255.0 - g) / 255.0) * 0.08
                } else if r > 195.0 && g > 110.0 && b < 105.0 {
                    0.68 + ((255.0 - g) / 160.0).clamp(0.0, 1.0) * 0.24
                } else {
                    ((232.0 - (0.299 * r + 0.587 * g + 0.114 * b)) / 145.0).clamp(0.05, 0.68)
                };
                let c = ramp(inten as f64, &RAIN_STOPS, 1.0);
                (
                    c.r * 255.0,
                    c.g * 255.0,
                    c.b * 255.0,
                    (115.0 + inten * 145.0).min(245.0),
                )
            };
            px.copy_from_slice(&[
                nr.clamp(0.0, 255.0) as u8,
                ng.clamp(0.0, 255.0) as u8,
                nb.clamp(0.0, 255.0) as u8,
                (na * (a as f32 / 255.0)).clamp(0.0, 255.0) as u8,
            ]);
        }
    }
    smooth_radar_alpha(rgba, w as usize, h as usize);
}

pub(super) fn bleed_transparent_edges(rgba: &mut [u8], w: u32, h: u32) {
    let (w, h) = (w as usize, h as usize);
    if w < 2
        || h < 2
        || rgba.len() != w * h * 4
        || !rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 0)
    {
        return;
    }
    let mut next = rgba.to_vec();
    for _ in 0..2 {
        let mut changed = false;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                if next[i + 3] != 0 {
                    continue;
                }
                let (mut r, mut g, mut b, mut wa) = (0_u32, 0_u32, 0_u32, 0_u32);
                for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                    for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                        let ni = (ny * w + nx) * 4;
                        let na = next[ni + 3] as u32;
                        if na > 0 {
                            r += next[ni] as u32 * na;
                            g += next[ni + 1] as u32 * na;
                            b += next[ni + 2] as u32 * na;
                            wa += na;
                        }
                    }
                }
                if let Some(nz) = NonZeroU32::new(wa) {
                    rgba[i..i + 4].copy_from_slice(&[
                        (r / nz) as u8,
                        (g / nz) as u8,
                        (b / nz) as u8,
                        0,
                    ]);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
        for i in (0..rgba.len()).step_by(4) {
            if next[i + 3] == 0 && (rgba[i] | rgba[i + 1] | rgba[i + 2]) != 0 {
                next[i..i + 3].copy_from_slice(&rgba[i..i + 3]);
                next[i + 3] = 1;
            }
        }
    }
}

pub(super) fn effective_tile_zoom(zoom: f64, max_z: u8) -> u8 {
    ((zoom + 0.35).floor() as i32).clamp(2, max_z as i32) as u8
}
pub(super) fn tile_coord_at(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let (n, (wx, wy)) = (1_i64 << z, lat_lon_to_world_px(lat, lon, z as f64));
    (
        (wx / 256.0).floor().rem_euclid(n as f64) as u32,
        (wy / 256.0).floor().clamp(0.0, (n - 1) as f64) as u32,
    )
}

#[derive(Clone, Copy)]
pub(super) struct MapViewport {
    pub bounds: Size,
    pub lat: f64,
    pub lon: f64,
    pub zoom: f64,
    pub corner_r: f32,
}

pub(super) struct TileLayer<'a> {
    pub ns: &'a str,
    pub max_z: u8,
    pub opacity: f32,
    pub opaque_base: bool,
    pub fallback: bool,
    pub ttl: Duration,
    pub offset: (f64, f64),
}
impl<'a> TileLayer<'a> {
    pub fn overlay(ns: &'a str, max_z: u8, opacity: f32, ttl: Duration) -> Self {
        Self {
            ns,
            max_z,
            opacity,
            opaque_base: false,
            fallback: true,
            ttl,
            offset: (0.0, 0.0),
        }
    }
}

fn tile_rect(
    (gx, gy): (i64, i64),
    px: f64,
    c: (f64, f64),
    half: (f64, f64),
    snap: bool,
) -> Rectangle {
    let (rx0, rx1) = (
        half.0 + gx as f64 * px - c.0,
        half.0 + (gx + 1) as f64 * px - c.0,
    );
    let (ry0, ry1) = (
        half.1 + gy as f64 * px - c.1,
        half.1 + (gy + 1) as f64 * px - c.1,
    );
    let (x0, x1, y0, y1) = if snap {
        (
            rx0.floor() as f32,
            rx1.ceil() as f32,
            ry0.floor() as f32,
            ry1.ceil() as f32,
        )
    } else {
        (rx0 as f32, rx1 as f32, ry0 as f32, ry1 as f32)
    };
    Rectangle {
        x: x0,
        y: y0,
        width: (x1 - x0).max(1.0),
        height: (y1 - y0).max(1.0),
    }
}

fn draw_img(f: &mut Frame, rect: Rectangle, h: adv_image::Handle, opacity: f32, r: f32) {
    let mut img = adv_image::Image::new(h)
        .filter_method(adv_image::FilterMethod::Linear)
        .opacity(opacity);
    img.border_radius = r.into();
    f.draw_image(rect, img);
}

fn find_ancestor(
    ns: &TileMap,
    tz: u8,
    wx: u32,
    wy: u32,
    max_dz: u8,
) -> Option<(u8, u8, u32, u32, adv_image::Handle)> {
    (1..=max_dz).take_while(|&dz| tz >= dz + 2).find_map(|dz| {
        let (az, awx, awy) = (tz - dz, wx >> dz, wy >> dz);
        ns.get(&(az, awx, awy))
            .cloned()
            .map(|h| (dz, az, awx, awy, h))
    })
}

pub(super) fn draw_tile_layer(
    frame: &mut Frame,
    pool: &mut SharedTilePool,
    vp: MapViewport,
    layer: TileLayer<'_>,
    url_fn: impl Fn(u8, u32, u32) -> String,
) {
    let vp_rect = Rectangle::with_size(vp.bounds);
    let tz = effective_tile_zoom(vp.zoom, layer.max_z);
    let (px, n) = (256.0 * 2.0_f64.powf(vp.zoom - tz as f64), 1_i64 << tz);
    let (cwx, cwy) = lat_lon_to_world_px(vp.lat, vp.lon, vp.zoom);
    let (c, half) = (
        (cwx - layer.offset.0, cwy - layer.offset.1),
        (vp.bounds.width as f64 * 0.5, vp.bounds.height as f64 * 0.5),
    );
    let (min_x, max_x) = (
        ((c.0 - half.0) / px).floor() as i64,
        ((c.0 + half.0) / px).ceil() as i64 - 1,
    );
    let (min_y, max_y) = (
        (((c.1 - half.1) / px).floor() as i64).max(0),
        ((((c.1 + half.1) / px).ceil() as i64) - 1).min(n - 1),
    );
    if min_y > max_y || min_x > max_x {
        return;
    }
    for ty in (min_y - 1).max(0)..=(max_y + 1).min(n - 1) {
        for tx in (min_x - 1)..=(max_x + 1) {
            let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
            let vis = (min_y..=max_y).contains(&ty) && (min_x..=max_x).contains(&tx);
            pool.ensure(
                layer.ns,
                (tz, wx, wy),
                || url_fn(tz, wx, wy),
                layer.ttl,
                vis,
            );
        }
    }
    let Some(ns) = pool.tiles.get(layer.ns) else {
        return;
    };
    if layer.opaque_base && layer.fallback {
        let mut drawn = HashSet::new();
        for ty in min_y..=max_y {
            for tx in min_x..=max_x {
                let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
                if ns.contains_key(&(tz, wx, wy)) {
                    continue;
                }
                if let Some((dz, az, awx, awy, ah)) = find_ancestor(ns, tz, wx, wy, 5) {
                    let gax = tx.div_euclid(n) * (1_i64 << az) + awx as i64;
                    if drawn.insert((az, gax, awy)) {
                        let r =
                            tile_rect((gax, awy as i64), px * (1_u32 << dz) as f64, c, half, true);
                        if r.intersects(&vp_rect) {
                            draw_img(frame, r, ah, layer.opacity, vp.corner_r);
                        }
                    }
                } else {
                    for dz in 1..=2_u8 {
                        let cz = tz + dz;
                        if cz > layer.max_z {
                            break;
                        }
                        let (sub, cpx, mut any) = (1_u32 << dz, px / (1_u32 << dz) as f64, false);
                        for sy in 0..sub {
                            for sx in 0..sub {
                                if let Some(ch) =
                                    ns.get(&(cz, (wx << dz) + sx, (wy << dz) + sy)).cloned()
                                {
                                    any = true;
                                    let r = tile_rect(
                                        (tx * sub as i64 + sx as i64, ty * sub as i64 + sy as i64),
                                        cpx,
                                        c,
                                        half,
                                        true,
                                    );
                                    if r.intersects(&vp_rect) {
                                        draw_img(frame, r, ch, layer.opacity, vp.corner_r);
                                    }
                                }
                            }
                        }
                        if any {
                            break;
                        }
                    }
                }
            }
        }
    }
    for ty in min_y..=max_y {
        for tx in min_x..=max_x {
            let dst = tile_rect((tx, ty), px, c, half, layer.opaque_base);
            if !dst.intersects(&vp_rect) {
                continue;
            }
            let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
            if let Some(h) = ns.get(&(tz, wx, wy)).cloned() {
                draw_img(frame, dst, h, layer.opacity, vp.corner_r);
            } else if !layer.opaque_base
                && layer.fallback
                && let Some((dz, az, awx, awy, ah)) = find_ancestor(ns, tz, wx, wy, 2)
                && let Some(clipped) = dst.intersection(&vp_rect)
            {
                let gax = tx.div_euclid(n) * (1_i64 << az) + awx as i64;
                let r = tile_rect((gax, awy as i64), px * (1_u32 << dz) as f64, c, half, false);
                frame.with_clip(clipped, |sub| draw_img(sub, r, ah, layer.opacity, 0.0));
            }
        }
    }
}
