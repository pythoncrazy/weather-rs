use super::{WorkerEvent, lat_lon_to_world_px};
use crate::icons::ramp;
use crate::providers::WeatherClient;
use crate::storage::{delete_cached_tile, is_shutting_down, load_cached_tile, save_cached_tile};
use iced::advanced::image as adv_image;
use iced::widget::canvas::Frame;
use iced::{Rectangle, Size};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

thread_local! {
    static SMOOTH_TMP: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static BLEED_TMP: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct TileKey(pub String, pub u8, pub u32, pub u32);

pub(super) enum TileEvent {
    Loaded(TileKey, Option<adv_image::Handle>),
    Failed(TileKey),
    Cancelled(TileKey),
}

type TileCoord = (u8, u32, u32);
type TileMap = HashMap<TileCoord, Option<adv_image::Handle>>;

struct TileJob {
    key: TileKey,
    url: String,
    ttl: Duration,
    high: bool,
    cancellable: bool,
    epoch: u64,
}

#[derive(Default)]
struct JobQueues {
    high: VecDeque<TileJob>,
    low: VecDeque<TileJob>,
}

impl JobQueues {
    fn push(&mut self, job: TileJob) -> Option<TileJob> {
        if job.high {
            self.high.push_front(job);
            (self.high.len() > 96)
                .then(|| self.high.pop_back())
                .flatten()
        } else if self.low.len() >= 192 {
            Some(job)
        } else {
            self.low.push_back(job);
            None
        }
    }
    fn pop(&mut self) -> Option<TileJob> {
        self.high.pop_front().or_else(|| self.low.pop_front())
    }
}

struct PriorityChan {
    q: Mutex<JobQueues>,
    cv: Condvar,
}

impl PriorityChan {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            q: Mutex::new(JobQueues::default()),
            cv: Condvar::new(),
        })
    }
    fn push(&self, job: TileJob, tx: &mpsc::Sender<TileEvent>) {
        let dropped = self.q.lock().ok().and_then(|mut q| {
            let d = q.push(job);
            self.cv.notify_one();
            d
        });
        if let Some(d) = dropped {
            let _ = tx.send(TileEvent::Cancelled(d.key));
        }
    }
    fn pop_blocking(&self) -> Option<TileJob> {
        let Ok(mut q) = self.q.lock() else {
            return None;
        };
        loop {
            if is_shutting_down() {
                return None;
            }
            if let Some(job) = q.pop() {
                return Some(job);
            }
            let Ok((guard, _)) = self.cv.wait_timeout(q, Duration::from_millis(250)) else {
                return None;
            };
            q = guard;
        }
    }
    fn purge_stale(&self, epoch: u64, removed: &mut Vec<TileKey>) {
        if let Ok(mut q) = self.q.lock() {
            let mut keep_fn = |j: &TileJob| {
                let keep = !j.cancellable || j.epoch == epoch;
                if !keep {
                    removed.push(j.key.clone());
                }
                keep
            };
            q.high.retain(&mut keep_fn);
            q.low.retain(&mut keep_fn);
        }
    }
}

pub(super) struct TileWorkers {
    disk: Arc<PriorityChan>,
    net: Arc<PriorityChan>,
    epoch: Arc<AtomicU64>,
}

impl TileWorkers {
    pub fn spawn(client: WeatherClient, tx: mpsc::Sender<TileEvent>, wake: Arc<Notify>) -> Self {
        let disk = PriorityChan::new();
        let net = PriorityChan::new();
        let epoch = Arc::new(AtomicU64::new(1));

        // 2 fast local disk-cache decode threads (never blocked by HTTP I/O).
        for _ in 0..2 {
            let (d, n, ep, tx, wk) = (
                Arc::clone(&disk),
                Arc::clone(&net),
                Arc::clone(&epoch),
                tx.clone(),
                Arc::clone(&wake),
            );
            std::thread::spawn(move || {
                while let Some(job) = d.pop_blocking() {
                    if job.cancellable && job.epoch != ep.load(Ordering::Acquire) {
                        let _ = tx.send(TileEvent::Cancelled(job.key));
                        continue;
                    }
                    if let Some(cached) =
                        load_cached_tile(&job.key.0, job.key.1, job.key.2, job.key.3, job.ttl)
                    {
                        if let Some(h) = decode_tile(&job.key.0, &cached) {
                            let _ = tx.send(TileEvent::Loaded(job.key, h));
                            wk.notify_one();
                            continue;
                        }
                        delete_cached_tile(&job.key.0, job.key.1, job.key.2, job.key.3);
                    }
                    n.push(job, &tx);
                }
            });
        }

        // 8 dedicated HTTP I/O threads (saturated without excess idle threads).
        for _ in 0..8 {
            let (n, ep, c, tx, wk) = (
                Arc::clone(&net),
                Arc::clone(&epoch),
                client.clone(),
                tx.clone(),
                Arc::clone(&wake),
            );
            std::thread::spawn(move || {
                while let Some(job) = n.pop_blocking() {
                    if job.cancellable && job.epoch != ep.load(Ordering::Acquire) {
                        let _ = tx.send(TileEvent::Cancelled(job.key));
                        continue;
                    }
                    if let Ok(bytes) = c.fetch_bytes(&job.url)
                        && let Some(h) = decode_tile(&job.key.0, &bytes)
                    {
                        let dk = job.key.clone();
                        let _ = tx.send(TileEvent::Loaded(job.key, h));
                        wk.notify_one();
                        if !is_shutting_down() {
                            save_cached_tile(&dk.0, dk.1, dk.2, dk.3, &bytes);
                        }
                    } else {
                        let _ = tx.send(TileEvent::Failed(job.key));
                        wk.notify_one();
                    }
                }
            });
        }

        Self { disk, net, epoch }
    }

    pub fn bump_epoch(&self, in_flight: &mut HashSet<TileKey>) {
        let ep = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let mut removed = Vec::new();
        self.disk.purge_stale(ep, &mut removed);
        self.net.purge_stale(ep, &mut removed);
        for k in removed {
            in_flight.remove(&k);
        }
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

pub(super) struct SharedTilePool {
    pub tiles: HashMap<String, TileMap>,
    pub lru: VecDeque<TileKey>,
    pub in_flight: HashSet<TileKey>,
    pub failed_until: HashMap<TileKey, Instant>,
    pub tx: mpsc::Sender<WorkerEvent>,
    pub tile_tx: mpsc::Sender<TileEvent>,
    pub tile_rx: mpsc::Receiver<TileEvent>,
    pub workers: TileWorkers,
}

impl SharedTilePool {
    pub fn drain_ready(&mut self) -> bool {
        let mut any = false;
        while let Ok(ev) = self.tile_rx.try_recv() {
            any = true;
            match ev {
                TileEvent::Loaded(k, h) => {
                    self.in_flight.remove(&k);
                    self.insert(k, h);
                }
                TileEvent::Failed(k) => {
                    self.in_flight.remove(&k);
                    self.failed_until
                        .insert(k, Instant::now() + Duration::from_secs(18));
                }
                TileEvent::Cancelled(k) => {
                    self.in_flight.remove(&k);
                }
            }
        }
        any
    }

    pub fn has_tile(&self, ns: &str, c: TileCoord) -> bool {
        self.tiles.get(ns).is_some_and(|m| m.contains_key(&c))
    }

    pub fn bump_epoch(&mut self) {
        self.workers.bump_epoch(&mut self.in_flight);
    }

    pub fn ensure(
        &mut self,
        ns: &str,
        c: TileCoord,
        url_fn: impl FnOnce() -> String,
        ttl: Duration,
        high: bool,
        cancellable: bool,
    ) {
        if self.has_tile(ns, c)
            || is_shutting_down()
            || (!high && cancellable && self.in_flight.len() >= 192)
        {
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
        let epoch = self.workers.current_epoch();
        self.workers.disk.push(
            TileJob {
                key,
                url,
                ttl,
                high,
                cancellable,
                epoch,
            },
            &self.tile_tx,
        );
    }

    pub fn insert(&mut self, key: TileKey, handle: Option<adv_image::Handle>) {
        let coord = (key.1, key.2, key.3);
        let ns_map = self.tiles.entry(key.0.clone()).or_default();
        if !ns_map.contains_key(&coord) && key.1 > 2 {
            self.lru.push_back(key.clone());
        }
        ns_map.insert(coord, handle);
        while self.lru.len() > 1536 {
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

fn is_radar_ns(ns: &str) -> bool {
    ns.starts_with("hrrr_") || ns.starts_with("lwxr256_") || ns.starts_with("rv256_")
}
fn is_opaque_base_ns(ns: &str) -> bool {
    matches!(
        ns,
        "esri_dark_base" | "esri_sat" | "esri_topo" | "carto_voyager"
    )
}

fn decode_tile(ns: &str, bytes: &[u8]) -> Option<Option<adv_image::Handle>> {
    let rgba = ::image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let mut raw = rgba.into_raw();
    if is_radar_ns(ns) {
        if !stylize_radar_tile(ns, &mut raw, w, h) {
            return Some(None);
        }
        bleed_transparent_edges(&mut raw, w, h);
    } else if !is_opaque_base_ns(ns) {
        bleed_transparent_edges(&mut raw, w, h);
    }
    Some(Some(adv_image::Handle::from_rgba(w, h, raw)))
}

fn smooth_radar_alpha(rgba: &mut [u8], w: usize, h: usize) {
    if w < 3 || h < 3 || rgba.len() != w * h * 4 {
        return;
    }
    SMOOTH_TMP.with(|cell| {
        let mut tmp = cell.borrow_mut();
        tmp.resize(rgba.len(), 0);
        let mut row_any = [false; 512];
        for (y, has_any) in row_any.iter_mut().enumerate().take(h) {
            let row = y * w;
            let mut any = false;
            for x in 0..w {
                let (i0, i1, i2) = (
                    (row + x.saturating_sub(1)) * 4,
                    (row + x) * 4,
                    (row + (x + 1).min(w - 1)) * 4,
                );
                let (a0, a1, a2) = (
                    rgba[i0 + 3] as u32,
                    rgba[i1 + 3] as u32,
                    rgba[i2 + 3] as u32,
                );
                let asum = a0 + a1 * 2 + a2;
                if asum == 0 {
                    tmp[i1 + 3] = 0;
                    continue;
                }
                any = true;
                for c in 0..3 {
                    tmp[i1 + c] = ((rgba[i0 + c] as u32 * a0
                        + rgba[i1 + c] as u32 * a1 * 2
                        + rgba[i2 + c] as u32 * a2)
                        >> 2) as u16;
                }
                tmp[i1 + 3] = (asum >> 2) as u16;
            }
            *has_any = any;
        }
        for y in 0..h {
            let (ya, yb) = (y.saturating_sub(1), (y + 1).min(h - 1));
            if !row_any[ya] && !row_any[y] && !row_any[yb] {
                continue;
            }
            let (y0, yc, y1) = (ya * w, y * w, yb * w);
            for x in 0..w {
                let (i0, i1, i2) = ((y0 + x) * 4, (yc + x) * 4, (y1 + x) * 4);
                let asum = tmp[i0 + 3] as u32 + tmp[i1 + 3] as u32 * 2 + tmp[i2 + 3] as u32;
                if let Some(nz) = NonZeroU32::new(asum) {
                    for c in 0..3 {
                        rgba[i1 + c] =
                            ((tmp[i0 + c] as u32 + tmp[i1 + c] as u32 * 2 + tmp[i2 + c] as u32)
                                / nz)
                                .min(255) as u8;
                    }
                    rgba[i1 + 3] = ((asum + 2) >> 2).min(255) as u8;
                } else {
                    rgba[i1 + 3] = 0;
                }
            }
        }
    });
}

/// Stylizes radar RGBA tiles in-place. Returns `false` if the tile is completely empty/transparent.
pub(super) fn stylize_radar_tile(ns: &str, rgba: &mut [u8], w: u32, h: u32) -> bool {
    let (refp, refd) = (ns.starts_with("hrrr_refp_"), ns.starts_with("hrrr_refd_"));
    if !refp && !refd && !ns.starts_with("lwxr256_") && !ns.starts_with("rv256_") {
        return true;
    }
    const RAIN_STOPS: [(f64, [u8; 3]); 5] = [
        (0.0, [110, 198, 255]),
        (0.35, [56, 160, 255]),
        (0.65, [38, 102, 245]),
        (0.85, [162, 68, 230]),
        (1.0, [244, 171, 55]),
    ];
    let mut any_visible = false;
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
            let out_a = (na * (a as f32 / 255.0)).clamp(0.0, 255.0) as u8;
            if out_a > 0 {
                any_visible = true;
            }
            px.copy_from_slice(&[
                nr.clamp(0.0, 255.0) as u8,
                ng.clamp(0.0, 255.0) as u8,
                nb.clamp(0.0, 255.0) as u8,
                out_a,
            ]);
        }
    } else {
        any_visible = rgba.as_chunks::<4>().0.iter().any(|p| p[3] >= 10);
    }
    if !any_visible {
        return false;
    }
    smooth_radar_alpha(rgba, w as usize, h as usize);
    true
}

/// 1-pass RGB color bleed into adjacent transparent pixels to eliminate dark halos under GPU bilinear filtering.
pub(super) fn bleed_transparent_edges(rgba: &mut [u8], w: u32, h: u32) {
    let (w, h) = (w as usize, h as usize);
    if w < 2 || h < 2 || rgba.len() != w * h * 4 {
        return;
    }
    let row_bytes = w * 4;
    let mut row_has_opaque = [false; 512];
    let mut row_has_trans = [false; 512];
    let (mut any_op, mut any_tr) = (false, false);
    for y in 0..h {
        let mut op = false;
        let mut tr = false;
        for p in rgba[y * row_bytes..(y + 1) * row_bytes].as_chunks::<4>().0 {
            if p[3] == 0 {
                tr = true;
            } else {
                op = true;
            }
            if op && tr {
                break;
            }
        }
        row_has_opaque[y] = op;
        row_has_trans[y] = tr;
        any_op |= op;
        any_tr |= tr;
    }
    if !any_op || !any_tr {
        return;
    }
    BLEED_TMP.with(|cell| {
        let mut src = cell.borrow_mut();
        src.clear();
        src.extend_from_slice(rgba);
        for (y, &has_tr) in row_has_trans.iter().enumerate().take(h) {
            if !has_tr {
                continue;
            }
            let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
            if !row_has_opaque[y0..=y1].iter().any(|&v| v) {
                continue;
            }
            for x in 0..w {
                let i = (y * w + x) * 4;
                if src[i + 3] != 0 {
                    continue;
                }
                let (mut r, mut g, mut b, mut wa) = (0_u32, 0_u32, 0_u32, 0_u32);
                let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
                for (ny, &has_op) in row_has_opaque.iter().enumerate().take(y1 + 1).skip(y0) {
                    if !has_op {
                        continue;
                    }
                    let nr = ny * w;
                    for nx in x0..=x1 {
                        let ni = (nr + nx) * 4;
                        let na = src[ni + 3] as u32;
                        if na > 0 {
                            r += src[ni] as u32 * na;
                            g += src[ni + 1] as u32 * na;
                            b += src[ni + 2] as u32 * na;
                            wa += na;
                        }
                    }
                }
                if let Some(nz) = NonZeroU32::new(wa) {
                    rgba[i..i + 3].copy_from_slice(&[
                        (r / nz) as u8,
                        (g / nz) as u8,
                        (b / nz) as u8,
                    ]);
                }
            }
        }
    });
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
}

pub(super) struct TileLayer<'a> {
    pub ns: &'a str,
    pub max_z: u8,
    pub opacity: f32,
    pub opaque_base: bool,
    pub fallback: bool,
    pub allow_fetch: bool,
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
            allow_fetch: true,
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
            .and_then(Option::as_ref)
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
    pool.drain_ready();
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
    if layer.allow_fetch {
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
                    true,
                );
            }
        }
    }
    let Some(ns) = pool.tiles.get(layer.ns) else {
        return;
    };
    if layer.opaque_base && layer.fallback {
        let mut drawn: Vec<(u8, i64, u32)> = Vec::new();
        for ty in min_y..=max_y {
            for tx in min_x..=max_x {
                let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
                if ns.contains_key(&(tz, wx, wy)) {
                    continue;
                }
                if let Some((dz, az, awx, awy, ah)) = find_ancestor(ns, tz, wx, wy, 5) {
                    let gax = tx.div_euclid(n) * (1_i64 << az) + awx as i64;
                    let key = (az, gax, awy);
                    if !drawn.contains(&key) {
                        drawn.push(key);
                        let r =
                            tile_rect((gax, awy as i64), px * (1_u32 << dz) as f64, c, half, true);
                        if r.intersects(&vp_rect) {
                            draw_img(frame, r, ah, layer.opacity, 0.0);
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
                                if let Some(ch) = ns
                                    .get(&(cz, (wx << dz) + sx, (wy << dz) + sy))
                                    .and_then(Option::as_ref)
                                    .cloned()
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
                                        draw_img(frame, r, ch, layer.opacity, 0.0);
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
            match ns.get(&(tz, wx, wy)) {
                Some(Some(h)) => draw_img(frame, dst, h.clone(), layer.opacity, 0.0),
                Some(None) => {}
                None if !layer.opaque_base && layer.fallback => {
                    if let Some((dz, az, awx, awy, ah)) = find_ancestor(ns, tz, wx, wy, 2)
                        && let Some(clipped) = dst.intersection(&vp_rect)
                    {
                        let gax = tx.div_euclid(n) * (1_i64 << az) + awx as i64;
                        let r =
                            tile_rect((gax, awy as i64), px * (1_u32 << dz) as f64, c, half, false);
                        frame.with_clip(clipped, |sub| draw_img(sub, r, ah, layer.opacity, 0.0));
                    }
                }
                None => {}
            }
        }
    }
}
