use super::{WorkerEvent, eccc, lat_lon_to_world_px};
use crate::icons::ramp;
use crate::providers::{EcccModel, WeatherClient};
use crate::storage::{delete_cached_tile, is_shutting_down, load_cached_tile, save_cached_tile};
use iced::advanced::image as adv_image;
use iced::widget::canvas::Frame;
use iced::{Rectangle, Size};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{
    Arc, Condvar, LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Fast non-cryptographic 64-bit FxHash for hot internal integer/TileKey maps (2 instructions vs 40+ SipHash rounds).
#[derive(Default)]
pub(super) struct FxHasher(u64);
impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        while bytes.len() >= 8 {
            let v = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
            self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(K);
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let v = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as u64;
            self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(K);
            bytes = &bytes[4..];
        }
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(K);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}
pub(super) type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub(super) type FxHashSet<K> = HashSet<K, BuildHasherDefault<FxHasher>>;

/// Compact 16-byte `Copy` tile key (zero heap/Arc refcount traffic on hot render lookups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey(pub u32, pub u8, pub u32, pub u32);
pub(super) type TileCoord = (u8, u32, u32);
/// Priority levels (higher pops first; strict FIFO within each level across BOTH `disk_q` and `net_q` — zero LIFO inversion!):
/// `5` = active frame `cur` & visible basemap/labels, `4` = `cur+1`, `3` = `cur+2`, `2` = `cur+3`, `1` = rest of loop, `0` = bg.
const PRIO_LEVELS: usize = 24;
pub(super) const PRIO_MAX: u8 = (PRIO_LEVELS - 1) as u8;
/// Per-origin HTTP concurrency lane so fast parallel pipelining across LibreWXR, ECCC GeoMet, IEM HRRR, and ESRI
/// basemaps NEVER trips single-host burst throttling (`api.librewxr.net` drops connections >12 concurrent!) and NEVER
/// head-of-line blocks other origins while one host is at its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostLane {
    LibreWxr = 0,
    Eccc = 1,
    Iem = 2,
    Base = 3,
}
/// Per-origin tile network accounting, indexed by [`HostLane`]: `[requests, bytes, total ms, slowest ms, failures]`.
static LANE_STATS: [[AtomicU64; 5]; 4] = [const { [const { AtomicU64::new(0) }; 5] }; 4];
/// Records one finished tile HTTP request against its origin lane.
pub(super) fn record_fetch(lane: HostLane, bytes: usize, elapsed: Duration, ok: bool) {
    let (s, ms) = (&LANE_STATS[lane as usize], elapsed.as_millis() as u64);
    s[0].fetch_add(1, Ordering::Relaxed);
    s[1].fetch_add(bytes as u64, Ordering::Relaxed);
    s[2].fetch_add(ms, Ordering::Relaxed);
    s[3].fetch_max(ms, Ordering::Relaxed);
    s[4].fetch_add(u64::from(!ok), Ordering::Relaxed);
}
/// Returns and clears the per-lane counters as `(lane name, [requests, bytes, total ms, slowest ms, failures])`.
#[cfg(test)]
pub(super) fn take_lane_stats() -> [(&'static str, [u64; 5]); 4] {
    let names = ["librewxr", "eccc", "iem", "basemap"];
    std::array::from_fn(|i| (names[i], std::array::from_fn(|k| LANE_STATS[i][k].swap(0, Ordering::Relaxed))))
}
impl HostLane {
    #[inline]
    fn from_ns(ns: &str) -> Self {
        if ns.starts_with("lwxr") || ns.starts_with("rv256_") || ns.starts_with("rv512_") {
            Self::LibreWxr
        } else if ns.starts_with("eccc_") {
            Self::Eccc
        } else if ns.starts_with("hrrr_") || ns.starts_with("iem_") {
            Self::Iem
        } else {
            Self::Base
        }
    }
    #[inline]
    const fn cap(self) -> usize {
        match self {
            // Per-origin concurrency caps. LibreWXR rate-limits a cold edge above 12 parallel requests. ECCC GeoMet
            // throttles above ~15 requests/s per IP and its usage policy asks clients not to pull tiles in bulk: at
            // ~0.6 s per raster, 8 parallel requests stay under that ceiling (18 ran at ~30 req/s and drew multi-second
            // stalls and timeouts).
            Self::LibreWxr => 12,
            Self::Eccc => 8,
            Self::Iem => 24,
            Self::Base => 12,
        }
    }
    #[inline]
    const fn timeout_ms(self) -> (u64, u64) {
        match self {
            Self::Eccc => (4000, 4500),
            Self::LibreWxr | Self::Iem | Self::Base => (2200, 2800),
        }
    }
}
type TileJob = (TileKey, Arc<str>, String, Duration, u8, bool, u64);
struct PrioQueueState {
    buckets: [VecDeque<TileJob>; PRIO_LEVELS],
    /// O(1) side-index from queued `TileKey -> bucket` so `promote()` skips non-queued / already-high tiles in ~5 ns!
    index: FxHashMap<TileKey, u8>,
    active_lanes: [usize; 4],
    total: usize,
}
impl PrioQueueState {
    fn new() -> Self {
        Self { buckets: std::array::from_fn(|_| VecDeque::new()), index: FxHashMap::default(), active_lanes: [0; 4], total: 0 }
    }
}
struct LanePermit<'a> {
    q: &'a (Mutex<PrioQueueState>, Condvar),
    lane: usize,
}
impl Drop for LanePermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.q.0.lock() {
            g.active_lanes[self.lane] = g.active_lanes[self.lane].saturating_sub(1);
            self.q.1.notify_all();
        }
    }
}
/// Counting gate bounding how many worker threads may run CPU-heavy tile decode / stylize / interpolation at once.
pub(super) struct DecGate {
    active: Mutex<usize>,
    cv: Condvar,
    max: usize,
}
pub(super) struct DecPermit<'a>(&'a DecGate);
impl DecGate {
    pub(super) fn new(max: usize) -> Self {
        Self { active: Mutex::new(0), cv: Condvar::new(), max: max.max(1) }
    }
    /// Blocks until a slot is free; the slot is released when the returned permit drops.
    pub(super) fn enter(&self) -> DecPermit<'_> {
        if let Ok(n) = self.active.lock()
            && let Ok(mut n) = self.cv.wait_while(n, |cnt| *cnt >= self.max)
        {
            *n += 1;
        }
        DecPermit(self)
    }
}
impl Drop for DecPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut n) = self.0.active.lock() {
            *n = n.saturating_sub(1);
            self.0.cv.notify_one();
        }
    }
}
pub(super) type MapVp = (Size, f64, f64, f64);

pub(super) const FLOW_THUMB_N: usize = 32;
pub(super) type AlphaThumb = Arc<[u8; FLOW_THUMB_N * FLOW_THUMB_N]>;

pub(super) enum TileEvent {
    Loaded(TileKey, Option<adv_image::Handle>, Option<AlphaThumb>),
    Failed(TileKey),
    Cancelled(TileKey),
    /// A derived tile job that cannot be built until the listed source rasters have been downloaded.
    NeedSources(TileJob, Vec<eccc::SourceReq>),
}
pub(super) enum TileSlot {
    /// Decoded tile handle + optional 24×24 precipitation alpha thumbnail (for optical-flow advection) + arrival time + CLOCK bit.
    Ready(Option<adv_image::Handle>, Option<AlphaThumb>, Instant, AtomicBool),
    Failed(Instant),
}

const TILE_FADE_SECS: f32 = 0.095;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TileLayerKind {
    Base,
    Weather,
    Label,
}

/// Dual-lane tile worker pool with strict 6-bucket FIFO priority queues (zero double-LIFO inversion between disk & HTTP!):
/// - 6 dedicated fast disk-cache workers serve cached tiles in <1 ms (never blocked behind slow HTTP!).
/// - 32 concurrent HTTP workers pipeline uncached network fetches across LibreWXR, HRRR, and ECCC GeoMet in parallel.
pub(super) struct TileWorkers {
    disk_q: Arc<(Mutex<PrioQueueState>, Condvar)>,
    net_q: Arc<(Mutex<PrioQueueState>, Condvar)>,
    epoch: Arc<AtomicU64>,
}
impl TileWorkers {
    fn pop_job(q: &(Mutex<PrioQueueState>, Condvar)) -> Option<TileJob> {
        q.0.lock().ok().and_then(|mut g| {
            while !is_shutting_down() {
                for b in g.buckets.iter_mut().rev() {
                    if let Some(x) = b.pop_front() {
                        g.index.remove(&x.0);
                        g.total = g.total.saturating_sub(1);
                        return Some(x);
                    }
                }
                g = q.1.wait_timeout(g, Duration::from_millis(250)).ok()?.0;
            }
            None
        })
    }
    /// Host-aware priority pop for HTTP workers: always picks the highest-priority FIFO job whose origin lane
    /// is below its concurrency cap, avoiding both single-host burst throttling and cross-host head-of-line blocking!
    fn pop_net_job(q: &(Mutex<PrioQueueState>, Condvar)) -> Option<(TileJob, HostLane, LanePermit<'_>)> {
        q.0.lock().ok().and_then(|mut g| {
            while !is_shutting_down() {
                let state = &mut *g;
                for b in state.buckets.iter_mut().rev() {
                    if let Some(pos) = b.iter().position(|j| {
                        let lane = HostLane::from_ns(&j.1);
                        state.active_lanes[lane as usize] < lane.cap()
                    }) && let Some(x) = b.remove(pos)
                    {
                        let lane = HostLane::from_ns(&x.1);
                        state.active_lanes[lane as usize] += 1;
                        state.index.remove(&x.0);
                        state.total = state.total.saturating_sub(1);
                        return Some((x, lane, LanePermit { q, lane: lane as usize }));
                    }
                }
                g = q.1.wait_timeout(g, Duration::from_millis(250)).ok()?.0;
            }
            None
        })
    }
    fn push_queue(q: &(Mutex<PrioQueueState>, Condvar), job: TileJob, cap: usize) -> Option<TileJob> {
        q.0.lock().ok().and_then(|mut g| {
            let p = (job.4 as usize).min(PRIO_LEVELS - 1);
            let dropped = if g.total >= cap {
                // Evict the oldest/furthest job from the lowest priority bucket <= `p`; never drop higher-priority work!
                let mut ev = None;
                for b in g.buckets.iter_mut().take(p + 1) {
                    if let Some(old) = b.pop_back() {
                        ev = Some(old);
                        break;
                    }
                }
                if let Some(old) = ev {
                    g.index.remove(&old.0);
                    g.index.insert(job.0, p as u8);
                    g.buckets[p].push_back(job);
                    Some(old)
                } else {
                    Some(job)
                }
            } else {
                g.index.insert(job.0, p as u8);
                g.buckets[p].push_back(job);
                g.total += 1;
                None
            };
            q.1.notify_one();
            dropped
        })
    }
    pub fn spawn(client: WeatherClient, tx: mpsc::Sender<TileEvent>, wake: Arc<Notify>) -> Self {
        let disk_q = Arc::new((Mutex::new(PrioQueueState::new()), Condvar::new()));
        let net_q = Arc::new((Mutex::new(PrioQueueState::new()), Condvar::new()));
        let epoch = Arc::new(AtomicU64::new(1));
        // Cap simultaneous CPU image decode/stylize work to half the CPU cores (2..=6) so 40 parallel HTTP fetches
        // NEVER saturate all cores and starve the 60fps UI/WGPU render thread during precipitation tile bursts!
        let max_dec = std::thread::available_parallelism().map_or(3, |n| (n.get() / 2).clamp(2, 6));
        let dec_gate = Arc::new(DecGate::new(max_dec));
        let decode_gated = |gate: &DecGate, ns: &str, c: TileCoord, b: &[u8]| {
            let _permit = gate.enter();
            decode_tile(ns, c, b)
        };
        // 6 instant disk-cache readers (promote cache hits directly to GPU with zero network head-of-line blocking!)
        for _ in 0..6 {
            let (dq, nq, ep, tx, wk, dg) =
                (Arc::clone(&disk_q), Arc::clone(&net_q), Arc::clone(&epoch), tx.clone(), Arc::clone(&wake), Arc::clone(&dec_gate));
            std::thread::spawn(move || {
                while let Some(job) = Self::pop_job(&dq) {
                    let (k, ref ns, ref url, ttl, _, cancel, job_ep) = job;
                    if cancel && job_ep != ep.load(Ordering::Acquire) {
                        let _ = tx.send(TileEvent::Cancelled(k));
                        wk.notify_one();
                        continue;
                    }
                    if eccc::is_job_spec(url) {
                        // ECCC frames are built here, off-network, from hourly rasters in memory / on disk. Rasters that
                        // are still missing go back to the pool, which downloads each one exactly once as a normal tile.
                        let ev = match eccc::build(&dg, ttl, ns, url, (k.1, k.2, k.3)) {
                            eccc::Built::Done(h, th) => TileEvent::Loaded(k, h, th),
                            eccc::Built::Need(reqs) => TileEvent::NeedSources(job, reqs),
                            eccc::Built::Failed => TileEvent::Failed(k),
                        };
                        let _ = tx.send(ev);
                        wk.notify_one();
                        continue;
                    }
                    if eccc::is_source_ns(ns) {
                        let cached = load_cached_tile(ns, k.1, k.2, k.3, ttl);
                        if cached.is_some_and(|b| eccc::ingest_source(&dg, ns, (k.1, k.2, k.3), &b).is_some()) {
                            let _ = tx.send(TileEvent::Loaded(k, None, None));
                            wk.notify_one();
                        } else if let Some(dropped) = Self::push_queue(&nq, job, 1400) {
                            let _ = tx.send(TileEvent::Cancelled(dropped.0));
                            wk.notify_one();
                        }
                        continue;
                    }
                    if let Some(b) = load_cached_tile(ns, k.1, k.2, k.3, ttl) {
                        if let Some((h, th)) = decode_gated(&dg, ns, (k.1, k.2, k.3), &b) {
                            let _ = tx.send(TileEvent::Loaded(k, h, th));
                            wk.notify_one();
                            continue;
                        }
                        delete_cached_tile(ns, k.1, k.2, k.3);
                    }
                    if let Some(dropped) = Self::push_queue(&nq, job, 1400) {
                        let _ = tx.send(TileEvent::Cancelled(dropped.0));
                        wk.notify_one();
                    }
                }
            });
        }
        // Dedicated background disk-cache writer so HTTP workers NEVER stall on filesystem mkdir/rename between tiles!
        let (save_tx, save_rx) = mpsc::sync_channel::<(Arc<str>, u8, u32, u32, Vec<u8>)>(512);
        std::thread::spawn(move || {
            while let Ok((ns, z, x, y, bytes)) = save_rx.recv() {
                if is_shutting_down() {
                    break;
                }
                save_cached_tile(&ns, z, x, y, &bytes);
            }
        });
        // 48 parallel HTTP workers with measured per-origin concurrency lanes (`LibreWxr<=12`, `Eccc<=18`, `Iem<=24`, `Base<=12`)
        for _ in 0..48 {
            let (nq, ep, c, tx, wk, stx, dg) = (
                Arc::clone(&net_q),
                Arc::clone(&epoch),
                client.clone(),
                tx.clone(),
                Arc::clone(&wake),
                save_tx.clone(),
                Arc::clone(&dec_gate),
            );
            std::thread::spawn(move || {
                while let Some(((k, ns, url, _, _, cancel, job_ep), lane, _permit)) = Self::pop_net_job(&nq) {
                    if cancel && job_ep != ep.load(Ordering::Acquire) {
                        let _ = tx.send(TileEvent::Cancelled(k));
                        wk.notify_one();
                        continue;
                    }
                    let (t1, t2) = lane.timeout_ms();
                    // 1x fast automatic retry with bounded lane timeout so a rare hung CDN/origin request recovers quickly!
                    let t_req = Instant::now();
                    let fetched = match c.fetch_bytes_timeout(&url, t1) {
                        Ok(b) => Ok(b),
                        Err(_) if !is_shutting_down() && (!cancel || job_ep == ep.load(Ordering::Acquire)) => {
                            std::thread::sleep(Duration::from_millis(120));
                            if cancel && job_ep != ep.load(Ordering::Acquire) {
                                let _ = tx.send(TileEvent::Cancelled(k));
                                wk.notify_one();
                                continue;
                            }
                            c.fetch_bytes_timeout(&url, t2)
                        }
                        Err(e) => Err(e),
                    };
                    record_fetch(lane, fetched.as_ref().map_or(0, Vec::len), t_req.elapsed(), fetched.is_ok());
                    if eccc::is_source_ns(&ns) {
                        // Hourly ECCC rasters are inputs, not displayable tiles: keep the decoded field for the builds
                        // waiting on it and cache the raw bytes (or a 1-byte sentinel when it holds no precipitation).
                        let ingested = fetched.ok().and_then(|b| eccc::ingest_source(&dg, &ns, (k.1, k.2, k.3), &b).map(|wet| (b, wet)));
                        let _ = tx.send(if ingested.is_some() { TileEvent::Loaded(k, None, None) } else { TileEvent::Failed(k) });
                        wk.notify_one();
                        if let Some((b, wet)) = ingested
                            && !is_shutting_down()
                        {
                            let _ = stx.try_send((ns, k.1, k.2, k.3, if wet { b } else { vec![0] }));
                        }
                        continue;
                    }
                    if let Ok(b) = fetched
                        && let Some((h, th)) = decode_gated(&dg, &ns, (k.1, k.2, k.3), &b)
                    {
                        let non_empty = h.is_some();
                        // Deliver decoded tile to UI/GPU FIRST, then hand off disk write asynchronously (0 us HTTP worker stall!)
                        let _ = tx.send(TileEvent::Loaded(k, h, th));
                        wk.notify_one();
                        if !is_shutting_down() {
                            // Empty/transparent radar tiles store a 1-byte sentinel [0] so reloads skip PNG decode completely!
                            let _ = stx.try_send((ns, k.1, k.2, k.3, if non_empty { b } else { vec![0] }));
                        }
                    } else {
                        let _ = tx.send(TileEvent::Failed(k));
                        wk.notify_one();
                    }
                }
            });
        }
        Self { disk_q, net_q, epoch }
    }
    fn push(&self, job: TileJob, tx: &mpsc::Sender<TileEvent>) {
        if let Some(d) = Self::push_queue(&self.disk_q, job, 1400) {
            let _ = tx.send(TileEvent::Cancelled(d.0));
        }
    }
    /// Promotes an already-queued lower-priority tile into bucket `new_prio` across `net_q` and `disk_q`
    /// using the O(1) side-index so already-active or already-high tiles return in ~5 ns!
    fn promote(&self, key: TileKey, new_prio: u8) {
        let np = (new_prio as usize).min(PRIO_LEVELS - 1);
        for q in [&self.net_q, &self.disk_q] {
            if let Ok(mut g) = q.0.lock()
                && let Some(&old_p_u8) = g.index.get(&key)
            {
                let old_p = old_p_u8 as usize;
                if old_p < np
                    && let Some(pos) = g.buckets[old_p].iter().position(|j| j.0 == key)
                    && let Some(mut job) = g.buckets[old_p].remove(pos)
                {
                    job.4 = np as u8;
                    g.buckets[np].push_back(job);
                    g.index.insert(key, np as u8);
                    q.1.notify_one();
                }
                return;
            }
        }
    }
    pub fn bump_epoch(&self, in_flight: &mut FxHashSet<TileKey>) {
        let ep = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        for q in [&self.disk_q, &self.net_q] {
            if let Ok(mut g) = q.0.lock() {
                let state = &mut *g;
                let mut total = 0_usize;
                for b in &mut state.buckets {
                    b.retain(|j| {
                        let ok = !j.5 || j.6 == ep;
                        if !ok {
                            in_flight.remove(&j.0);
                            state.index.remove(&j.0);
                        }
                        ok
                    });
                    total += b.len();
                }
                state.total = total;
            }
        }
    }
    /// Drops only queued basemap/label jobs (never weather tiles!) when switching basemap style (`Dark`/`Light`/`Satellite`).
    pub fn cancel_base_only(&self, in_flight: &mut FxHashSet<TileKey>) {
        for q in [&self.disk_q, &self.net_q] {
            if let Ok(mut g) = q.0.lock() {
                let state = &mut *g;
                let mut total = 0_usize;
                for b in &mut state.buckets {
                    b.retain(|j| {
                        let drop_it = j.5 && HostLane::from_ns(&j.1) == HostLane::Base;
                        if drop_it {
                            in_flight.remove(&j.0);
                            state.index.remove(&j.0);
                        }
                        !drop_it
                    });
                    total += b.len();
                }
                state.total = total;
            }
        }
    }
}

pub(super) struct SharedTilePool {
    pub slots: FxHashMap<TileKey, TileSlot>,
    ns_ids: FxHashMap<String, u32>,
    ns_names: Vec<Arc<str>>,
    ns_kinds: Vec<TileLayerKind>,
    pub lru: VecDeque<TileKey>,
    pub in_flight: FxHashSet<TileKey>,
    wanted_base: FxHashSet<TileKey>,
    wanted_weather: FxHashSet<TileKey>,
    wanted_label: FxHashSet<TileKey>,
    /// Namespace IDs of the currently displayed/crossfading weather frame(s) so arriving/failing tiles ALWAYS
    /// invalidate `weather_cache` even if they were enqueued via prewarm or `wanted_weather` rotated.
    active_weather_ns: [Option<u32>; 8],
    base_fade_until: Instant,
    label_fade_until: Instant,
    pub base_rev: u64,
    pub weather_rev: u64,
    pub label_rev: u64,
    /// GPU VRAM pinning queue + Second-Chance CLOCK cache (`iced::widget::image::Allocation`):
    /// prevents `iced_wgpu::image::raster::Cache::trim()` from evicting active radar loop frames & basemap tiles!
    pending_gpu_pins: VecDeque<(TileKey, adv_image::Handle)>,
    gpu_pin_requested: FxHashSet<TileKey>,
    gpu_pins: FxHashMap<TileKey, iced_runtime::image::Allocation>,
    gpu_pin_lru: VecDeque<TileKey>,
    /// Derived tile jobs parked until the source raster they are keyed by has been downloaded.
    waiting: FxHashMap<TileKey, Vec<TileJob>>,
    /// `(derived tile, source raster)` pairs whose build was re-run once because the raster had just arrived.
    source_retries: FxHashSet<(TileKey, TileKey)>,
    pub tx: mpsc::Sender<WorkerEvent>,
    pub tile_tx: mpsc::Sender<TileEvent>,
    pub tile_rx: mpsc::Receiver<TileEvent>,
    pub workers: TileWorkers,
}
impl SharedTilePool {
    pub fn new(client: WeatherClient, tx: mpsc::Sender<WorkerEvent>, wake: Arc<Notify>) -> Self {
        let (tile_tx, tile_rx) = mpsc::channel();
        let workers = TileWorkers::spawn(client, tile_tx.clone(), wake);
        let now = Instant::now();
        Self {
            slots: FxHashMap::default(),
            ns_ids: FxHashMap::default(),
            ns_names: Vec::new(),
            ns_kinds: Vec::new(),
            lru: VecDeque::new(),
            in_flight: FxHashSet::default(),
            wanted_base: FxHashSet::default(),
            wanted_weather: FxHashSet::default(),
            wanted_label: FxHashSet::default(),
            active_weather_ns: [None; 8],
            base_fade_until: now,
            label_fade_until: now,
            base_rev: 1,
            weather_rev: 1,
            label_rev: 1,
            pending_gpu_pins: VecDeque::new(),
            gpu_pin_requested: FxHashSet::default(),
            gpu_pins: FxHashMap::default(),
            gpu_pin_lru: VecDeque::new(),
            waiting: FxHashMap::default(),
            source_retries: FxHashSet::default(),
            tx,
            tile_tx,
            tile_rx,
            workers,
        }
    }
    #[inline]
    pub fn has_pending_gpu_pins(&self) -> bool {
        !self.pending_gpu_pins.is_empty()
    }
    #[inline]
    pub fn is_gpu_pin_requested(&self, k: TileKey) -> bool {
        self.gpu_pin_requested.contains(&k)
    }
    /// Queues a tile handle for zero-cost in-place `iced_wgpu` shared-atlas pinning ONLY AFTER `draw()` has emitted it!
    /// Never pin in `drain_ready()` before `draw()`: calling `image::allocate(h)` before `upload_raster()` forces
    /// `iced_wgpu`'s async worker to create a standalone GPU texture + `BindGroup` + blocking `device.poll(Wait)` +
    /// `Shell::invalidate_layout` per tile and breaks GPU instancing across the entire map!
    #[inline]
    pub fn queue_pin_after_draw(&mut self, k: TileKey, handle: &adv_image::Handle) {
        if self.gpu_pin_requested.insert(k) {
            if self.pending_gpu_pins.len() >= 1024
                && let Some((dropped, _)) = self.pending_gpu_pins.pop_front()
            {
                self.gpu_pin_requested.remove(&dropped);
            }
            self.pending_gpu_pins.push_back((k, handle.clone()));
        }
    }
    pub fn take_pending_gpu_pins(&mut self, max: usize) -> Vec<(TileKey, adv_image::Handle)> {
        let n = self.pending_gpu_pins.len().min(max);
        self.pending_gpu_pins.drain(..n).collect()
    }
    pub fn store_gpu_pins(&mut self, pins: Vec<(TileKey, Option<iced_runtime::image::Allocation>)>) {
        for (k, alloc) in pins {
            let Some(a) = alloc else {
                self.gpu_pin_requested.remove(&k);
                continue;
            };
            if self.gpu_pins.insert(k, a).is_none() {
                self.gpu_pin_lru.push_back(k);
            }
        }
        // Second-Chance CLOCK eviction: give recently painted tiles a second chance so supersampled basemaps,
        // labels, all 23 radar loop frames, and all 12 satellite IR frames stay 100% pinned in VRAM on 1440p/4K!
        let mut spins = self.gpu_pin_lru.len();
        while self.gpu_pin_lru.len() > 1024 && spins > 0 {
            spins -= 1;
            if let Some(cand) = self.gpu_pin_lru.pop_front() {
                if let Some(TileSlot::Ready(_, _, _, rf)) = self.slots.get(&cand)
                    && rf.swap(false, Ordering::Relaxed)
                {
                    self.gpu_pin_lru.push_back(cand);
                    continue;
                }
                self.gpu_pins.remove(&cand);
                self.gpu_pin_requested.remove(&cand);
            }
        }
    }
    pub fn is_base_fading(&self) -> bool {
        Instant::now() < self.base_fade_until
    }
    pub fn is_label_fading(&self) -> bool {
        Instant::now() < self.label_fade_until
    }
    pub fn is_fading(&self) -> bool {
        let now = Instant::now();
        now < self.base_fade_until || now < self.label_fade_until
    }
    pub fn set_active_weather_ns(&mut self, ns: [Option<u32>; 8]) {
        self.active_weather_ns = ns;
    }
    pub fn mark_wanted(&mut self, kind: TileLayerKind, key: TileKey) {
        let set = match kind {
            TileLayerKind::Base => &mut self.wanted_base,
            TileLayerKind::Weather => &mut self.wanted_weather,
            TileLayerKind::Label => &mut self.wanted_label,
        };
        if set.len() > 1024 {
            set.clear();
        }
        set.insert(key);
    }
    #[inline]
    pub fn lookup_ns(&self, ns: &str) -> Option<u32> {
        self.ns_ids.get(ns).copied()
    }
    pub fn intern(&mut self, ns: &str) -> u32 {
        if let Some(&id) = self.ns_ids.get(ns) {
            return id;
        }
        let id = self.ns_names.len() as u32;
        let kind = if ns.ends_with("_labels") {
            TileLayerKind::Label
        } else if ns.starts_with("esri_") || ns.starts_with("osm_") || ns.starts_with("carto_") {
            TileLayerKind::Base
        } else {
            TileLayerKind::Weather
        };
        self.ns_names.push(Arc::from(ns));
        self.ns_kinds.push(kind);
        self.ns_ids.insert(ns.into(), id);
        id
    }
    pub fn drain_ready(&mut self) -> bool {
        let mut any = false;
        let now = Instant::now();
        while let Ok(ev) = self.tile_rx.try_recv() {
            match ev {
                TileEvent::NeedSources(job, reqs) => {
                    // Park the job on its first missing raster; when that arrives the build re-runs and, if another
                    // raster is still missing, parks on that one. Every raster is requested here exactly once.
                    let mut park_on = None;
                    for r in reqs {
                        let id = self.intern(&r.ns);
                        let sk = TileKey(id, r.coord.0, r.coord.1, r.coord.2);
                        if matches!(self.slots.get(&sk), Some(TileSlot::Ready(..))) {
                            // The raster finished downloading while this build was reporting it missing: simply build
                            // again. Only if a second attempt still cannot read it back (evicted from memory before
                            // its disk write landed) is it downloaded afresh.
                            if self.source_retries.len() > 4096 {
                                self.source_retries.clear();
                            }
                            if self.source_retries.insert((job.0, sk)) {
                                park_on = None;
                                self.workers.push(job.clone(), &self.tile_tx);
                                break;
                            }
                            self.slots.remove(&sk);
                        }
                        self.source_retries.remove(&(job.0, sk));
                        let _ = self.ensure_id_prio(id, r.coord, || r.url, job.3, job.4, job.5);
                        if self.in_flight.contains(&sk) {
                            park_on.get_or_insert(sk);
                        } else {
                            // The raster is in failure cooldown or was refused by back-pressure: fail this tile for now.
                            park_on = None;
                            self.in_flight.remove(&job.0);
                            self.wanted_weather.remove(&job.0);
                            self.slots.insert(job.0, TileSlot::Failed(now + Duration::from_secs(3)));
                            self.weather_rev = self.weather_rev.wrapping_add(1);
                            any = true;
                            break;
                        }
                    }
                    if let Some(sk) = park_on {
                        self.waiting.entry(sk).or_default().push(job);
                    }
                }
                TileEvent::Loaded(k, h, th) => {
                    self.in_flight.remove(&k);
                    self.release_waiting(k, Some(true), now);
                    let fresh = !matches!(self.slots.get(&k), Some(TileSlot::Ready(..)));
                    let _ = (fresh && k.1 > 2).then(|| self.lru.push_back(k));
                    let has_img = h.is_some();
                    self.slots.insert(k, TileSlot::Ready(h, th, now, AtomicBool::new(true)));
                    // Always bump the layer's exact revision by namespace kind so prewarmed basemap/label/weather tiles
                    // NEVER fail to invalidate their GPU cache (zero blank mini-maps, zero missing labels, zero stuck zoom!).
                    // Fade-in ticks remain gated on `wanted_*` so background prewarms don't spin 60fps redraws unnecessarily.
                    let kind = self.ns_kinds.get(k.0 as usize).copied().unwrap_or(TileLayerKind::Weather);
                    let wb = self.wanted_base.remove(&k);
                    let ww = self.wanted_weather.remove(&k);
                    let wl = self.wanted_label.remove(&k);
                    match kind {
                        TileLayerKind::Base => {
                            if has_img {
                                any = true;
                                self.base_rev = self.base_rev.wrapping_add(1);
                                if wb {
                                    self.base_fade_until = now + Duration::from_millis(105);
                                }
                            } else if wb {
                                any = true;
                            }
                        }
                        TileLayerKind::Label => {
                            if has_img {
                                any = true;
                                self.label_rev = self.label_rev.wrapping_add(1);
                                if wl {
                                    self.label_fade_until = now + Duration::from_millis(105);
                                }
                            } else if wl {
                                any = true;
                            }
                        }
                        TileLayerKind::Weather => {
                            // Always invalidate `weather_cache` if the tile belongs to the currently active/crossfading frame(s)
                            // or was explicitly wanted — background prewarming of distant frames never thrashes the GPU cache!
                            if ww || self.active_weather_ns.contains(&Some(k.0)) {
                                self.weather_rev = self.weather_rev.wrapping_add(1);
                                any = true;
                            }
                        }
                    }
                    // Second-Chance CLOCK eviction preserves hot active basemap + loop tiles in RAM!
                    let mut spins = self.lru.len();
                    while self.lru.len() > 3600 && spins > 0 {
                        spins -= 1;
                        if let Some(o) = self.lru.pop_front() {
                            if let Some(TileSlot::Ready(_, _, _, rf)) = self.slots.get(&o)
                                && rf.swap(false, Ordering::Relaxed)
                            {
                                self.lru.push_back(o);
                                continue;
                            }
                            self.slots.remove(&o);
                        }
                    }
                }
                TileEvent::Failed(k) => {
                    self.in_flight.remove(&k);
                    any |= self.release_waiting(k, Some(false), now);
                    let wb = self.wanted_base.remove(&k);
                    let ww = self.wanted_weather.remove(&k);
                    let wl = self.wanted_label.remove(&k);
                    // Short 3s failure cooldown so transient CDN/WMS glitches self-heal rapidly!
                    self.slots.insert(k, TileSlot::Failed(now + Duration::from_secs(3)));
                    if ww || self.active_weather_ns.contains(&Some(k.0)) {
                        self.weather_rev = self.weather_rev.wrapping_add(1);
                        any = true;
                    } else if wb || wl {
                        any = true;
                    }
                }
                TileEvent::Cancelled(k) => {
                    self.in_flight.remove(&k);
                    any |= self.release_waiting(k, None, now);
                    let wb = self.wanted_base.remove(&k);
                    let ww = self.wanted_weather.remove(&k);
                    let wl = self.wanted_label.remove(&k);
                    if ww || self.active_weather_ns.contains(&Some(k.0)) {
                        self.weather_rev = self.weather_rev.wrapping_add(1);
                        any = true;
                    } else if wb {
                        self.base_rev = self.base_rev.wrapping_add(1);
                        any = true;
                    } else if wl {
                        self.label_rev = self.label_rev.wrapping_add(1);
                        any = true;
                    }
                }
            }
        }
        any
    }
    /// Returns true once a tile has either loaded (image or empty) or entered a temporary failure cooldown,
    /// so playback buffering waits for in-flight tiles without stalling forever on a 404/offline tile.
    #[inline]
    pub fn is_settled_id(&self, ns_id: u32, c: TileCoord) -> bool {
        match self.slots.get(&TileKey(ns_id, c.0, c.1, c.2)) {
            Some(TileSlot::Ready(_, _, _, rf)) => {
                rf.store(true, Ordering::Relaxed);
                true
            }
            Some(TileSlot::Failed(until)) => Instant::now() < *until,
            None => false,
        }
    }
    /// True if tile `(tz, wx, wy)` is already visually covered by an adjacent-zoom parent (`tz-1..=tz-2`) or all 4 children (`tz+1`)
    /// in RAM, so zooming in/out on radar keeps rendering seamlessly without blinking or stalling!
    #[inline]
    pub fn has_zoom_fallback_id(&self, ns_id: u32, (tz, wx, wy): TileCoord, max_z: u8) -> bool {
        if tz < max_z {
            let cz = tz + 1;
            let all_children = (0..2_u32).all(|dy| {
                (0..2_u32).all(|dx| matches!(self.slots.get(&TileKey(ns_id, cz, wx * 2 + dx, wy * 2 + dy)), Some(TileSlot::Ready(..))))
            });
            if all_children {
                return true;
            }
        }
        (1..=2_u8)
            .take_while(|&dz| tz >= dz + 2)
            .any(|dz| matches!(self.slots.get(&TileKey(ns_id, tz - dz, wx >> dz, wy >> dz)), Some(TileSlot::Ready(..))))
    }
    #[cfg(test)]
    pub fn insert_ready_for_test(&mut self, k: TileKey) {
        self.slots.insert(k, TileSlot::Ready(None, None, Instant::now(), AtomicBool::new(true)));
    }
    #[cfg(test)]
    pub fn insert_thumb_for_test(&mut self, k: TileKey, thumb: AlphaThumb) {
        self.slots.insert(k, TileSlot::Ready(None, Some(thumb), Instant::now(), AtomicBool::new(true)));
    }
    #[inline]
    pub fn get_ready_id(&self, ns_id: u32, c: TileCoord) -> Option<&Option<adv_image::Handle>> {
        match self.slots.get(&TileKey(ns_id, c.0, c.1, c.2))? {
            TileSlot::Ready(h, _, _, rf) => {
                rf.store(true, Ordering::Relaxed);
                Some(h)
            }
            TileSlot::Failed(_) => None,
        }
    }
    #[inline]
    pub fn get_thumb_id(&self, ns_id: u32, c: TileCoord) -> Option<&AlphaThumb> {
        match self.slots.get(&TileKey(ns_id, c.0, c.1, c.2))? {
            TileSlot::Ready(_, Some(th), _, _) => Some(th),
            _ => None,
        }
    }
    #[inline]
    pub fn get_ready_timed(&self, ns_id: u32, c: TileCoord) -> Option<(&Option<adv_image::Handle>, Instant)> {
        match self.slots.get(&TileKey(ns_id, c.0, c.1, c.2))? {
            TileSlot::Ready(h, _, t, rf) => {
                rf.store(true, Ordering::Relaxed);
                Some((h, *t))
            }
            TileSlot::Failed(_) => None,
        }
    }
    pub fn bump_epoch(&mut self) {
        self.workers.bump_epoch(&mut self.in_flight);
        // Parked cancellable jobs belong to the old epoch too: forget them so their tiles can be requested afresh.
        let in_flight = &mut self.in_flight;
        self.waiting.retain(|_, jobs| {
            jobs.retain(|j| {
                if j.5 {
                    in_flight.remove(&j.0);
                }
                !j.5
            });
            !jobs.is_empty()
        });
    }
    /// Resolves every derived job parked on source tile `src`: re-runs them once the raster has arrived
    /// (`Some(true)`), puts their tiles into failure cooldown when its download failed (`Some(false)`), or just
    /// forgets them when it was cancelled (`None`) so they can be requested afresh. Returns whether tiles changed state.
    fn release_waiting(&mut self, src: TileKey, outcome: Option<bool>, now: Instant) -> bool {
        let Some(jobs) = self.waiting.remove(&src) else { return false };
        for job in jobs {
            if outcome == Some(true) {
                self.workers.push(job, &self.tile_tx);
                continue;
            }
            self.in_flight.remove(&job.0);
            self.wanted_weather.remove(&job.0);
            if outcome == Some(false) {
                self.slots.insert(job.0, TileSlot::Failed(now + Duration::from_secs(3)));
            }
        }
        if outcome != Some(true) {
            self.weather_rev = self.weather_rev.wrapping_add(1);
        }
        outcome != Some(true)
    }
    pub fn cancel_base_tiles(&mut self) {
        self.workers.cancel_base_only(&mut self.in_flight);
    }
    pub fn ensure(&mut self, ns: &str, c: TileCoord, url_fn: impl FnOnce() -> String, ttl: Duration, high: bool, cancel: bool) {
        let id = self.intern(ns);
        let _ = self.ensure_id_prio(id, c, url_fn, ttl, if high { PRIO_MAX } else { 0 }, cancel);
    }
    /// Enqueues or promotes `(id, c)` into bucket `prio`. Returns `true` if the tile is already ready, in-flight, or enqueued;
    /// returns `false` only if skipped due to low-priority backpressure so prewarmers know to revisit later!
    pub fn ensure_id_prio(
        &mut self, id: u32, c: TileCoord, url_fn: impl FnOnce() -> String, ttl: Duration, prio: u8, cancel: bool,
    ) -> bool {
        let key = TileKey(id, c.0, c.1, c.2);
        if is_shutting_down() {
            return true;
        }
        if self.in_flight.contains(&key) {
            // If this tile is already queued at lower priority and now became urgent (`cur` / `cur+1`),
            // promote it straight into bucket `prio` across `net_q` and `disk_q`!
            if prio >= 2 {
                self.workers.promote(key, prio);
            }
            return true;
        }
        match self.slots.get(&key) {
            Some(TileSlot::Ready(..)) => return true,
            Some(TileSlot::Failed(u)) if Instant::now() < *u => return true,
            _ => {}
        }
        if prio <= 1 && cancel && self.in_flight.len() >= 900 {
            return false;
        }
        let url = url_fn();
        if url.is_empty() {
            return true;
        }
        self.in_flight.insert(key);
        let ep = self.workers.epoch.load(Ordering::Acquire);
        self.workers.push((key, Arc::clone(&self.ns_names[id as usize]), url, ttl, prio, cancel, ep), &self.tile_tx);
        true
    }
}

fn decode_png_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let out_sz = reader.output_buffer_size()?;
    let mut buf = vec![0_u8; out_sz];
    let out = reader.next_frame(&mut buf).ok()?;
    let n = (w as usize) * (h as usize);
    let rgba = match out.color_type {
        png::ColorType::Rgba => {
            buf.truncate(n * 4);
            buf
        }
        png::ColorType::GrayscaleAlpha => {
            let data = &buf[..out.buffer_size()];
            let mut dst = vec![0_u8; n * 4];
            for (s, d) in data.as_chunks::<2>().0.iter().zip(dst.as_chunks_mut::<4>().0.iter_mut()) {
                *d = [s[0], s[0], s[0], s[1]];
            }
            dst
        }
        _ => return None,
    };
    Some((w, h, rgba))
}

fn decode_jpeg_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let opts = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGBA);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), opts);
    let rgba = dec.decode().ok()?;
    let (w, h) = dec.dimensions()?;
    if w == 0 || h == 0 || w > 512 || h > 512 || rgba.len() != w * h * 4 {
        return None;
    }
    Some((w as u32, h as u32, rgba))
}

pub(super) fn build_alpha_thumb(rgba: &[u8], w: u32, h: u32) -> AlphaThumb {
    let mut out = [0_u8; FLOW_THUMB_N * FLOW_THUMB_N];
    let (wu, hu) = (w as usize, h as usize);
    if wu == 0 || hu == 0 || rgba.len() < wu * hu * 4 {
        return Arc::new(out);
    }
    let rb = wu * 4;
    // Fast power-of-two/regular tile path (256x256, 512x512): precompute cell bounds once, skip empty rows with
    // `row_has_alpha`, and accumulate with a single reciprocal multiply instead of 1,024 integer divisions!
    let mut x_bounds = [(0_usize, 0_usize, 1_usize); FLOW_THUMB_N];
    for (gx, slot) in x_bounds.iter_mut().enumerate() {
        let x0 = (gx * wu) / FLOW_THUMB_N;
        let x1 = (((gx + 1) * wu) / FLOW_THUMB_N).max(x0 + 1).min(wu);
        let sx = ((x1 - x0) / 3).max(1);
        *slot = (x0, x1, sx);
    }
    for gy in 0..FLOW_THUMB_N {
        let y0 = (gy * hu) / FLOW_THUMB_N;
        let y1 = (((gy + 1) * hu) / FLOW_THUMB_N).max(y0 + 1).min(hu);
        let step_y = ((y1 - y0) / 3).max(1);
        let mut sums = [0_u32; FLOW_THUMB_N];
        let mut rows_hit = 0_u32;
        let mut y = y0;
        while y < y1 {
            let row = &rgba[y * rb..(y + 1) * rb];
            rows_hit += 1;
            if row_has_alpha(row) {
                for (gx, &(x0, x1, sx)) in x_bounds.iter().enumerate() {
                    let mut s = 0_u32;
                    let mut x = x0;
                    while x < x1 {
                        s += row[x * 4 + 3] as u32;
                        x += sx;
                    }
                    sums[gx] += s;
                }
            }
            y += step_y;
        }
        let dst = &mut out[gy * FLOW_THUMB_N..(gy + 1) * FLOW_THUMB_N];
        for (gx, &(x0, x1, sx)) in x_bounds.iter().enumerate() {
            let s = sums[gx];
            if s > 0 {
                let cols = ((x1 - x0).div_ceil(sx) as u32).max(1);
                dst[gx] = (s / (rows_hit * cols).max(1)).min(255) as u8;
            }
        }
    }
    Arc::new(out)
}

pub(super) fn decode_tile(ns: &str, coord: TileCoord, bytes: &[u8]) -> Option<(Option<adv_image::Handle>, Option<AlphaThumb>)> {
    if bytes == [0] {
        return Some((None, None));
    }
    let (w, h, mut raw) = if bytes.starts_with(b"\x89PNG") {
        decode_png_rgba(bytes)?
    } else if bytes.starts_with(b"\xff\xd8") {
        decode_jpeg_rgba(bytes)?
    } else {
        let img = if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            ::image::load_from_memory_with_format(bytes, ::image::ImageFormat::WebP).ok()?
        } else {
            ::image::load_from_memory(bytes).ok()?
        };
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 || w > 512 || h > 512 {
            return None;
        }
        (w, h, rgba.into_raw())
    };
    let wx_tile = ns.starts_with("hrrr_")
        || ns.starts_with("eccc_")
        || ns.starts_with("lwxr8_")
        || ns.starts_with("lwxr512_")
        || ns.starts_with("lwxr256_")
        || ns.starts_with("rv256_")
        || ns.starts_with("rv512_")
        || ns == "iem_nexrad"
        || ns.starts_with("iem_goes_ir");
    let opaque = matches!(
        ns,
        "carto_voyager_2x"
            | "carto_dark_2x"
            | "esri_light_base"
            | "esri_dark_base"
            | "esri_world_imagery"
            | "esri_world_topo"
            | "osm_standard"
    );
    if wx_tile && !stylize_radar_tile(ns, coord, &mut raw, w, h) {
        return Some((None, None));
    }
    let thumb = (wx_tile && !ns.starts_with("iem_goes_ir")).then(|| build_alpha_thumb(&raw, w, h));
    if ns == "esri_light_base" {
        stylize_light_basemap(&mut raw);
    }
    if !opaque && !ns.starts_with("iem_goes_ir") {
        bleed_transparent_edges(&mut raw, w, h);
    }
    Some((Some(adv_image::Handle::from_rgba(w, h, raw)), thumb))
}

/// Estimates coherent screen-space storm displacement `(dx_px, dy_px)` from Frame `A` to Frame `B` across the
/// composited visible precipitation layers using 2-stage coarse-to-fine normalized cross-correlation + parabolic
/// sub-pixel peak interpolation on the cached `24×24` alpha thumbnails (~80 µs ONCE per frame pair!).
pub(super) fn estimate_flow_px(
    pool: &SharedTilePool, layers_a: &[(u32, u8, f64)], layers_b: &[(u32, u8, f64)], (bounds, lat, lon, zoom): MapVp,
) -> (f32, f32) {
    let taper = ((zoom - 4.2) / 0.9).clamp(0.0, 1.0) as f32;
    if taper <= 0.01 || bounds.width < 64.0 || bounds.height < 64.0 || layers_a.is_empty() || layers_b.is_empty() {
        return (0.0, 0.0);
    }
    // Coarse screen-space grid (~12 px per cell, clamped to 96×72 for ~80 µs correlation time).
    const CELL_PX: f64 = 12.0;
    let gw = ((bounds.width as f64 / CELL_PX).round() as usize).clamp(16, 96);
    let gh = ((bounds.height as f64 / CELL_PX).round() as usize).clamp(16, 72);
    let sx_step = bounds.width as f64 / gw as f64;
    let sy_step = bounds.height as f64 / gh as f64;
    let half = (bounds.width as f64 * 0.5, bounds.height as f64 * 0.5);
    let c_world = lat_lon_to_world_px(lat, lon, zoom);

    let rasterize = |layers: &[(u32, u8, f64)], grid: &mut [f32]| -> f32 {
        grid.fill(0.0);
        for &(ns_id, max_z, bias) in layers {
            let tz = effective_tile_zoom_biased(zoom, max_z, bias);
            let n = 1_i64 << tz;
            let tpx = 256.0 * 2.0_f64.powf(zoom - tz as f64);
            let inv_tpx = 1.0 / tpx;
            for gy in 0..gh {
                let wy_px = c_world.1 - half.1 + (gy as f64 + 0.5) * sy_step;
                let ty_f = wy_px * inv_tpx;
                let ty = ty_f.floor() as i64;
                if ty < 0 || ty >= n {
                    continue;
                }
                let py = (((ty_f - ty as f64) * FLOW_THUMB_N as f64) as usize).min(FLOW_THUMB_N - 1);
                let row_off = gy * gw;
                for gx in 0..gw {
                    let wx_px = c_world.0 - half.0 + (gx as f64 + 0.5) * sx_step;
                    let tx_f = wx_px * inv_tpx;
                    let tx = tx_f.floor() as i64;
                    let wx = tx.rem_euclid(n) as u32;
                    let Some(th) = pool.get_thumb_id(ns_id, (tz, wx, ty as u32)) else { continue };
                    let px = (((tx_f - tx as f64) * FLOW_THUMB_N as f64) as usize).min(FLOW_THUMB_N - 1);
                    let v = th[py * FLOW_THUMB_N + px] as f32;
                    if v > 0.0 {
                        grid[row_off + gx] = (grid[row_off + gx] + v).min(255.0);
                    }
                }
            }
        }
        grid.iter().map(|&v| v * v).sum::<f32>()
    };

    thread_local! {
        static FLOW_GRIDS: std::cell::RefCell<(Vec<f32>, Vec<f32>)> = const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    }
    FLOW_GRIDS.with(|cell| {
        let mut guard = cell.borrow_mut();
        let (ga_v, gb_v) = &mut *guard;
        let len = gw * gh;
        if ga_v.len() < len {
            ga_v.resize(len, 0.0);
        }
        if gb_v.len() < len {
            gb_v.resize(len, 0.0);
        }
        let (ga, gb) = (&mut ga_v[..len], &mut gb_v[..len]);
        let ea = rasterize(layers_a, ga);
        if ea < 6_000.0 {
            return (0.0, 0.0);
        }
        let eb = rasterize(layers_b, gb);
        if eb < 6_000.0 {
            return (0.0, 0.0);
        }
        let max_s = (52.0 / sx_step.min(sy_step)).ceil() as isize;
        let max_s = max_s.clamp(2, 7).min((gw as isize / 4).min(gh as isize / 4));
        let corr_at = |dx: isize, dy: isize| -> f32 {
            let x0 = (-dx).max(0) as usize;
            let x1 = (gw as isize - dx).min(gw as isize).max(0) as usize;
            let y0 = (-dy).max(0) as usize;
            let y1 = (gh as isize - dy).min(gh as isize).max(0) as usize;
            if x1 <= x0 + 4 || y1 <= y0 + 4 {
                return 0.0;
            }
            let (mut dot, mut sa, mut sb) = (0.0_f32, 0.0_f32, 0.0_f32);
            for y in y0..y1 {
                let ra = &ga[y * gw..y * gw + gw];
                let rb = &gb[((y as isize + dy) as usize) * gw..((y as isize + dy) as usize) * gw + gw];
                for x in x0..x1 {
                    let a = ra[x];
                    let b = rb[(x as isize + dx) as usize];
                    dot += a * b;
                    sa += a * a;
                    sb += b * b;
                }
            }
            let denom = (sa * sb).sqrt();
            if denom < 4_000.0 { 0.0 } else { dot / denom }
        };
        let c_zero = corr_at(0, 0);
        let (mut best_c, mut best_x, mut best_y) = (c_zero, 0_isize, 0_isize);
        // Coarse pass (step 2)
        let mut dy = -max_s;
        while dy <= max_s {
            let mut dx = -max_s;
            while dx <= max_s {
                let c = corr_at(dx, dy);
                if c > best_c {
                    (best_c, best_x, best_y) = (c, dx, dy);
                }
                dx += 2;
            }
            dy += 2;
        }
        // Fine pass (±2 around coarse peak, step 1)
        let (cx, cy) = (best_x, best_y);
        for fy in (cy - 2).max(-max_s)..=(cy + 2).min(max_s) {
            for fx in (cx - 2).max(-max_s)..=(cx + 2).min(max_s) {
                let c = corr_at(fx, fy);
                if c > best_c {
                    (best_c, best_x, best_y) = (c, fx, fy);
                }
            }
        }
        if best_c < 0.44 || best_c <= c_zero + 0.012 {
            return (0.0, 0.0);
        }
        // Parabolic sub-pixel peak refinement along X and Y
        let sub_peak = |cl: f32, c0: f32, cr: f32| -> f32 {
            let d = cl - 2.0 * c0 + cr;
            if d < -1e-5 { (0.5 * (cl - cr) / d).clamp(-0.5, 0.5) } else { 0.0 }
        };
        let sub_x = if best_x > -max_s && best_x < max_s {
            sub_peak(corr_at(best_x - 1, best_y), best_c, corr_at(best_x + 1, best_y))
        } else {
            0.0
        };
        let sub_y = if best_y > -max_s && best_y < max_s {
            sub_peak(corr_at(best_x, best_y - 1), best_c, corr_at(best_x, best_y + 1))
        } else {
            0.0
        };
        let mut dx_px = (best_x as f32 + sub_x) * sx_step as f32 * taper;
        let mut dy_px = (best_y as f32 + sub_y) * sy_step as f32 * taper;
        let mag = dx_px.hypot(dy_px);
        if mag > 52.0 {
            let s = 52.0 / mag;
            dx_px *= s;
            dy_px *= s;
        }
        (dx_px, dy_px)
    })
}

/// Precomputed 23-bit fixed-point reciprocals for `wa in 1..=4080` (`255 * 16`).
/// Pure 32-bit multiply-shift `(r * inv + (1 << 22)) >> 23` NEVER overflows `u32` (max `255 * 4080 * 2056 + 2^22 < 2^31`)
/// and eliminates all integer divisions & 64-bit widens in both Gaussian and crisp radar filters!
static RECIP23: LazyLock<[u32; 4081]> = LazyLock::new(|| {
    let mut t = [0_u32; 4081];
    for (i, slot) in t.iter_mut().enumerate().skip(1) {
        *slot = (1_u32 << 23) / i as u32;
    }
    t
});

thread_local! {
    static FILTER_SCRATCH: std::cell::RefCell<(Vec<u8>, Vec<bool>)> = const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    static SCALAR_SCRATCH: std::cell::RefCell<(Vec<u16>, Vec<u16>, Vec<bool>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
}

/// Zero-alloc, division-free separable 5-tap Gaussian `[1, 4, 6, 4, 1] >> 4` on a 1-channel `u16` scalar precipitation field.
/// Smoothing BEFORE color-mapping melts 10–15 km nearest-neighbor NWP grid squares into crisp, organic Apple Weather contours
/// with true `RAIN_LUT` color transitions (zero muddy RGBA cross-blending!) at ~4× the speed of RGBA Gaussian!
fn smooth_scalar_u16(field: &mut [u16], tmp: &mut [u16], row_any: &mut [bool], w: usize, h: usize, strides: &[isize]) {
    if w < 3 || h < 3 || field.len() != w * h || tmp.len() != w * h || row_any.len() != h {
        return;
    }
    for &step in strides {
        let su = step as usize;
        let s2 = su * 2;
        let pad = s2.min(w);
        for (y, has) in row_any.iter_mut().enumerate() {
            let off = y * w;
            let src = &field[off..off + w];
            let dst = &mut tmp[off..off + w];
            let non_empty = src.iter().any(|&v| v != 0);
            *has = non_empty;
            if !non_empty {
                dst.fill(0);
                continue;
            }
            let eval_x = |x: usize| -> u16 {
                let x0 = (x as isize - 2 * step).clamp(0, (w - 1) as isize) as usize;
                let x1 = (x as isize - step).clamp(0, (w - 1) as isize) as usize;
                let x3 = (x as isize + step).clamp(0, (w - 1) as isize) as usize;
                let x4 = (x as isize + 2 * step).clamp(0, (w - 1) as isize) as usize;
                ((src[x0] as u32 + ((src[x1] as u32) << 2) + src[x] as u32 * 6 + ((src[x3] as u32) << 2) + src[x4] as u32 + 8) >> 4) as u16
            };
            for (x, d) in dst.iter_mut().enumerate().take(pad) {
                *d = eval_x(x);
            }
            for x in pad..w.saturating_sub(pad) {
                dst[x] = ((src[x - s2] as u32
                    + ((src[x - su] as u32) << 2)
                    + src[x] as u32 * 6
                    + ((src[x + su] as u32) << 2)
                    + src[x + s2] as u32
                    + 8)
                    >> 4) as u16;
            }
            for (x, d) in dst.iter_mut().enumerate().skip(w.saturating_sub(pad).max(pad)) {
                *d = eval_x(x);
            }
        }
        for y in 0..h {
            let sy0 = (y as isize - 2 * step).clamp(0, (h - 1) as isize) as usize;
            let sy1 = (y as isize - step).clamp(0, (h - 1) as isize) as usize;
            let sy3 = (y as isize + step).clamp(0, (h - 1) as isize) as usize;
            let sy4 = (y as isize + 2 * step).clamp(0, (h - 1) as isize) as usize;
            let dst = &mut field[y * w..(y + 1) * w];
            if !(row_any[sy0] || row_any[sy1] || row_any[y] || row_any[sy3] || row_any[sy4]) {
                dst.fill(0);
                continue;
            }
            let (r0, r1, r2, r3, r4) = (
                &tmp[sy0 * w..(sy0 + 1) * w],
                &tmp[sy1 * w..(sy1 + 1) * w],
                &tmp[y * w..(y + 1) * w],
                &tmp[sy3 * w..(sy3 + 1) * w],
                &tmp[sy4 * w..(sy4 + 1) * w],
            );
            for x in 0..w {
                dst[x] = ((r0[x] as u32 + ((r1[x] as u32) << 2) + r2[x] as u32 * 6 + ((r3[x] as u32) << 2) + r4[x] as u32 + 8) >> 4) as u16;
            }
        }
    }
}

/// Zero-alloc separable 3-tap `[1, 6, 1] >> 3` scalar micro-polish for already-smooth 2.5 km HRDPS precipitation fields.
fn smooth_scalar_crisp_u16(field: &mut [u16], tmp: &mut [u16], row_any: &mut [bool], w: usize, h: usize) {
    if w < 3 || h < 3 || field.len() != w * h || tmp.len() != w * h || row_any.len() != h {
        return;
    }
    for (y, has) in row_any.iter_mut().enumerate() {
        let off = y * w;
        let src = &field[off..off + w];
        let dst = &mut tmp[off..off + w];
        let non_empty = src.iter().any(|&v| v != 0);
        *has = non_empty;
        if !non_empty {
            dst.fill(0);
            continue;
        }
        dst[0] = ((src[0] as u32 * 7 + src[1] as u32 + 4) >> 3) as u16;
        for x in 1..w - 1 {
            dst[x] = ((src[x - 1] as u32 + src[x] as u32 * 6 + src[x + 1] as u32 + 4) >> 3) as u16;
        }
        dst[w - 1] = ((src[w - 2] as u32 + src[w - 1] as u32 * 7 + 4) >> 3) as u16;
    }
    for y in 0..h {
        let sy0 = y.saturating_sub(1);
        let sy2 = (y + 1).min(h - 1);
        let dst = &mut field[y * w..(y + 1) * w];
        if !(row_any[sy0] || row_any[y] || row_any[sy2]) {
            dst.fill(0);
            continue;
        }
        let (r0, r1, r2) = (&tmp[sy0 * w..(sy0 + 1) * w], &tmp[y * w..(y + 1) * w], &tmp[sy2 * w..(sy2 + 1) * w]);
        for x in 0..w {
            dst[x] = ((r0[x] as u32 + r1[x] as u32 * 6 + r2[x] as u32 + 4) >> 3) as u16;
        }
    }
}

#[inline]
fn row_has_alpha(row_bytes: &[u8]) -> bool {
    let (chunks, rem) = row_bytes.as_chunks::<8>();
    let mut acc = 0_u64;
    for &c in chunks {
        acc |= u64::from_le_bytes(c);
    }
    (acc & 0xFF00_0000_FF00_0000) != 0 || rem.as_chunks::<4>().0.iter().any(|p| p[3] != 0)
}

/// Separable 5-tap premultiplied-alpha Gaussian filter `[1, 4, 6, 4, 1] / 16` with zero-alloc scratch & 32-bit `RECIP23`.
fn smooth_rgba_gaussian(rgba: &mut [u8], w: usize, h: usize, strides: &[isize]) {
    if w < 3 || h < 3 || rgba.len() != w * h * 4 {
        return;
    }
    const K: [u32; 5] = [1, 4, 6, 4, 1];
    const HALF: u32 = 1 << 22;
    let recip = &*RECIP23;
    let rb = w * 4;
    FILTER_SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let (tmp_v, row_v) = &mut *guard;
        if tmp_v.len() < rgba.len() {
            tmp_v.resize(rgba.len(), 0);
        }
        if row_v.len() < h {
            row_v.resize(h, false);
        }
        let tmp = &mut tmp_v[..rgba.len()];
        let row_any = &mut row_v[..h];
        for &step in strides {
            let su = step as usize;
            let s1 = su * 4;
            let s2 = su * 8;
            let pad = (su * 2).min(w);
            // Horizontal pass (rgba -> tmp), skipping completely transparent rows & unclamped interior!
            for (y, has) in row_any.iter_mut().enumerate() {
                let off = y * rb;
                let src = &rgba[off..off + rb];
                let dst = &mut tmp[off..off + rb];
                let non_empty = row_has_alpha(src);
                *has = non_empty;
                if !non_empty {
                    dst.fill(0);
                    continue;
                }
                let eval_edge = |x: usize, dst: &mut [u8]| {
                    let (mut r, mut g, mut b, mut wa) = (0_u32, 0, 0, 0);
                    for (k, &wt) in K.iter().enumerate() {
                        let sx = (x as isize + (k as isize - 2) * step).clamp(0, (w - 1) as isize) as usize * 4;
                        let a0 = src[sx + 3] as u32;
                        if a0 > 0 {
                            let a = a0 * wt;
                            r += src[sx] as u32 * a;
                            g += src[sx + 1] as u32 * a;
                            b += src[sx + 2] as u32 * a;
                            wa += a;
                        }
                    }
                    let o = x * 4;
                    if wa > 0 {
                        let inv = recip[wa as usize];
                        dst[o..o + 4].copy_from_slice(&[
                            ((r * inv + HALF) >> 23).min(255) as u8,
                            ((g * inv + HALF) >> 23).min(255) as u8,
                            ((b * inv + HALF) >> 23).min(255) as u8,
                            ((wa + 8) >> 4).min(255) as u8,
                        ]);
                    } else {
                        dst[o..o + 4].copy_from_slice(&[0, 0, 0, 0]);
                    }
                };
                for x in 0..pad {
                    eval_edge(x, dst);
                }
                for x in pad..w.saturating_sub(pad) {
                    let c = x * 4;
                    let (i0, i1, i2, i3, i4) = (c - s2, c - s1, c, c + s1, c + s2);
                    let (a0, a1, a2, a3, a4) =
                        (src[i0 + 3] as u32, src[i1 + 3] as u32, src[i2 + 3] as u32, src[i3 + 3] as u32, src[i4 + 3] as u32);
                    if (a0 | a1 | a2 | a3 | a4) == 0 {
                        dst[c..c + 4].copy_from_slice(&[0, 0, 0, 0]);
                        continue;
                    }
                    let (w1, w2, w3) = (a1 << 2, a2 * 6, a3 << 2);
                    let wa = a0 + w1 + w2 + w3 + a4;
                    let r = src[i0] as u32 * a0 + src[i1] as u32 * w1 + src[i2] as u32 * w2 + src[i3] as u32 * w3 + src[i4] as u32 * a4;
                    let g = src[i0 + 1] as u32 * a0
                        + src[i1 + 1] as u32 * w1
                        + src[i2 + 1] as u32 * w2
                        + src[i3 + 1] as u32 * w3
                        + src[i4 + 1] as u32 * a4;
                    let b = src[i0 + 2] as u32 * a0
                        + src[i1 + 2] as u32 * w1
                        + src[i2 + 2] as u32 * w2
                        + src[i3 + 2] as u32 * w3
                        + src[i4 + 2] as u32 * a4;
                    let inv = recip[wa as usize];
                    dst[c..c + 4].copy_from_slice(&[
                        ((r * inv + HALF) >> 23).min(255) as u8,
                        ((g * inv + HALF) >> 23).min(255) as u8,
                        ((b * inv + HALF) >> 23).min(255) as u8,
                        ((wa + 8) >> 4).min(255) as u8,
                    ]);
                }
                for x in w.saturating_sub(pad).max(pad)..w {
                    eval_edge(x, dst);
                }
            }
            // Vertical pass (tmp -> rgba), hoisting all 5 source row slices outside the x loop!
            for y in 0..h {
                let sy0 = (y as isize - 2 * step).clamp(0, (h - 1) as isize) as usize;
                let sy1 = (y as isize - step).clamp(0, (h - 1) as isize) as usize;
                let sy2 = y;
                let sy3 = (y as isize + step).clamp(0, (h - 1) as isize) as usize;
                let sy4 = (y as isize + 2 * step).clamp(0, (h - 1) as isize) as usize;
                let dst = &mut rgba[y * rb..(y + 1) * rb];
                if !(row_any[sy0] || row_any[sy1] || row_any[sy2] || row_any[sy3] || row_any[sy4]) {
                    dst.fill(0);
                    continue;
                }
                let (r0, r1, r2, r3, r4) = (
                    &tmp[sy0 * rb..sy0 * rb + rb],
                    &tmp[sy1 * rb..sy1 * rb + rb],
                    &tmp[sy2 * rb..sy2 * rb + rb],
                    &tmp[sy3 * rb..sy3 * rb + rb],
                    &tmp[sy4 * rb..sy4 * rb + rb],
                );
                for x in 0..w {
                    let c = x * 4;
                    let (a0, a1, a2, a3, a4) = (r0[c + 3] as u32, r1[c + 3] as u32, r2[c + 3] as u32, r3[c + 3] as u32, r4[c + 3] as u32);
                    if (a0 | a1 | a2 | a3 | a4) == 0 {
                        dst[c..c + 4].copy_from_slice(&[0, 0, 0, 0]);
                        continue;
                    }
                    let (w1, w2, w3) = (a1 << 2, a2 * 6, a3 << 2);
                    let wa = a0 + w1 + w2 + w3 + a4;
                    let r = r0[c] as u32 * a0 + r1[c] as u32 * w1 + r2[c] as u32 * w2 + r3[c] as u32 * w3 + r4[c] as u32 * a4;
                    let g = r0[c + 1] as u32 * a0
                        + r1[c + 1] as u32 * w1
                        + r2[c + 1] as u32 * w2
                        + r3[c + 1] as u32 * w3
                        + r4[c + 1] as u32 * a4;
                    let b = r0[c + 2] as u32 * a0
                        + r1[c + 2] as u32 * w1
                        + r2[c + 2] as u32 * w2
                        + r3[c + 2] as u32 * w3
                        + r4[c + 2] as u32 * a4;
                    let inv = recip[wa as usize];
                    dst[c..c + 4].copy_from_slice(&[
                        ((r * inv + HALF) >> 23).min(255) as u8,
                        ((g * inv + HALF) >> 23).min(255) as u8,
                        ((b * inv + HALF) >> 23).min(255) as u8,
                        ((wa + 8) >> 4).min(255) as u8,
                    ]);
                }
            }
        }
    });
}

/// Precomputed 256-entry Dark Sky / Apple Weather multi-hue precipitation RGB LUT anchored directly to live
/// `LibreWXR` scheme-8 radar hues so observed radar (`-50m..Now`) and NWP forecast models (`HRRR`/`ECCC`) share
/// one seamless, honest color scale with zero palette/brightness jump across the live-to-forecast transition.
static RAIN_LUT: LazyLock<[[u8; 3]; 256]> = LazyLock::new(|| {
    const RAIN: [(f64, [u8; 3]); 7] = [
        (0.00, [12, 98, 198]),
        (0.36, [36, 98, 202]),
        (0.54, [118, 78, 182]),
        (0.68, [192, 82, 160]),
        (0.82, [242, 96, 134]),
        (0.93, [246, 128, 76]),
        (1.00, [250, 208, 76]),
    ];
    std::array::from_fn(|i| {
        let c = ramp(i as f64 / 255.0, &RAIN, 1.0);
        [(c.r * 255.0) as u8, (c.g * 255.0) as u8, (c.b * 255.0) as u8]
    })
});

/// Soft-alpha knee LUT (`oa < 24 => oa*oa/24`, else identity) — zero division in inner loops.
static SOFT_ALPHA: LazyLock<[u8; 256]> = LazyLock::new(|| std::array::from_fn(|i| if i < 24 { ((i * i) / 24) as u8 } else { i as u8 }));

/// Precomputed GOES IR smoothstep LUT `(r, g, b, alpha_base)` for `cloud in 0..=255`.
static GOES_LUT: LazyLock<[[u8; 4]; 256]> = LazyLock::new(|| {
    std::array::from_fn(|i| {
        let c = i as f32 / 255.0;
        let s = c * c * (3.0 - 2.0 * c);
        [(182.0 + s * 73.0) as u8, (204.0 + s * 51.0) as u8, (232.0 + s * 23.0) as u8, (s * 224.0) as u8]
    })
});

/// Gently upgrades ESRI World Light Gray Base from washed-out gray-on-gray (`#D0D4D8` ocean on `#EFEFEF` land)
/// into authentic Apple Maps Light contrast (crisp soft powder-blue ocean `#BDD8EE` vs warm off-white land `#F4F3EF`).
fn stylize_light_basemap(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        let lum = (r + g * 2 + b) >> 2;
        let sat = r.max(g).max(b) - r.min(g).min(b);
        // ESRI World_Light_Gray_Base ocean sits right at lum 198..=222 with near-zero saturation;
        // land sits at lum 234..=255; coastlines/borders sit between 165..197.
        if sat <= 14 && (196..=224).contains(&lum) {
            let t = ((224 - lum).min(10) as f32 * 0.1).min((lum - 196).min(10) as f32 * 0.1).clamp(0.0, 1.0);
            px[0] = (r as f32 * (1.0 - 0.10 * t)).clamp(0.0, 255.0) as u8;
            px[1] = (g as f32 + 9.0 * t).clamp(0.0, 255.0) as u8;
            px[2] = (b as f32 + 26.0 * t).clamp(0.0, 255.0) as u8;
        } else if sat <= 8 && lum >= 234 {
            px[0] = (r + 3).min(248) as u8;
            px[1] = (g + 2).min(247) as u8;
            px[2] = (b - 1).clamp(0, 244) as u8;
        }
    }
}

/// Removes isolated 1-pixel radar salt-and-pepper speckles (surrounded by >=7 empty neighbors) without blurring real rain cells.
fn despeckle_radar_alpha(rgba: &mut [u8], w: usize, h: usize) {
    if w < 3 || h < 3 || rgba.len() != w * h * 4 {
        return;
    }
    let rb = w * 4;
    for y in 1..h - 1 {
        let row = y * rb;
        if !row_has_alpha(&rgba[row..row + rb]) {
            continue;
        }
        for x in 1..w - 1 {
            let i = row + x * 4;
            let a = rgba[i + 3];
            if a == 0 || a > 195 {
                continue;
            }
            let nb = u8::from(rgba[i - 1] > 14)
                + u8::from(rgba[i + 7] > 14)
                + u8::from(rgba[i - rb + 3] > 14)
                + u8::from(rgba[i + rb + 3] > 14)
                + u8::from(rgba[i - rb - 1] > 14)
                + u8::from(rgba[i - rb + 7] > 14)
                + u8::from(rgba[i + rb - 1] > 14)
                + u8::from(rgba[i + rb + 7] > 14);
            if nb <= 1 {
                rgba[i + 3] = 0;
            }
        }
    }
}

/// Which ECCC GeoMet tier a precipitation tile belongs to, and whether NOAA HRRR owns the CONUS interior (`outside_hrrr`).
#[derive(Clone, Copy)]
pub(super) enum EcccMask {
    Hrdps { outside_hrrr: bool },
    Rdps { outside_hrrr: bool },
    Gdps { outside_hrrr: bool },
    Legacy { outside_hrrr: bool },
}
impl EcccMask {
    pub(super) fn from_ns(ns: &str) -> Option<Self> {
        if ns.starts_with("eccc_hrdps_out_") {
            Some(Self::Hrdps { outside_hrrr: true })
        } else if ns.starts_with("eccc_hrdps_full_") {
            Some(Self::Hrdps { outside_hrrr: false })
        } else if ns.starts_with("eccc_rdps_out_") {
            Some(Self::Rdps { outside_hrrr: true })
        } else if ns.starts_with("eccc_rdps_full_") {
            Some(Self::Rdps { outside_hrrr: false })
        } else if ns.starts_with("eccc_gdps_out_") {
            Some(Self::Gdps { outside_hrrr: true })
        } else if ns.starts_with("eccc_gdps_full_") {
            Some(Self::Gdps { outside_hrrr: false })
        } else if ns.starts_with("eccc_out_") {
            Some(Self::Legacy { outside_hrrr: true })
        } else if ns.starts_with("eccc_nwp_") {
            Some(Self::Legacy { outside_hrrr: false })
        } else {
            None
        }
    }
}

/// Reverse-maps an ECCC GeoMet `PRECIPPRTMMH-LINEAR` (MATLAB jet) RGBA raster into a continuous `u16` scalar intensity
/// field (`t * alpha`, `0..=65535`). Returns `false` when the raster holds no precipitation at all.
pub(super) fn eccc_extract_scalar(rgba: &[u8], wu: usize, hu: usize, field: &mut [u16]) -> bool {
    let mut any_raw = false;
    for py in 0..hu {
        let row = &rgba[py * wu * 4..(py + 1) * wu * 4];
        let frow = &mut field[py * wu..(py + 1) * wu];
        if !row_has_alpha(row) {
            frow.fill(0);
            continue;
        }
        for (px, dst) in row.as_chunks::<4>().0.iter().zip(frow.iter_mut()) {
            let a = px[3];
            if a < 12 {
                *dst = 0;
                continue;
            }
            let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
            let t = if b >= r && b >= g {
                if g < 24.0 && r < 24.0 { ((b - 110.0) / 145.0) * 0.12 } else { 0.12 + (g / 255.0) * 0.25 }
            } else if g >= r && g >= b {
                0.37 + ((r - b + 255.0) / 510.0) * 0.25
            } else if g > 16.0 || b > 16.0 {
                0.62 + ((255.0 - g) / 255.0) * 0.24
            } else {
                0.86 + ((255.0 - r) / 130.0) * 0.14
            }
            .clamp(0.0, 1.0);
            if t < 0.04 {
                *dst = 0;
                continue;
            }
            let scaled = (t * (a as f32 * (65535.0 / 255.0))).round().clamp(0.0, 65535.0) as u16;
            any_raw |= scaled > 0;
            *dst = scaled;
        }
    }
    any_raw
}

/// Smooths an extracted ECCC scalar field BEFORE color-mapping so 10–15 km NWP cells melt into organic contours:
/// light 3-tap polish on HRDPS (2.5 km), one 5-tap Gaussian pass on RDPS (10 km) / GDPS (15 km).
pub(super) fn eccc_smooth_scalar(em: EcccMask, field: &mut [u16], tmp: &mut [u16], row_any: &mut [bool], wu: usize, hu: usize) {
    match em {
        EcccMask::Hrdps { .. } => smooth_scalar_crisp_u16(field, tmp, row_any, wu, hu),
        EcccMask::Rdps { .. } | EcccMask::Gdps { .. } | EcccMask::Legacy { .. } => smooth_scalar_u16(field, tmp, row_any, wu, hu, &[1]),
    }
}

/// Color-maps a smoothed ECCC scalar field covering exactly tile `coord` through `RAIN_LUT` and applies the exact
/// partition-of-unity domain mask (`HRRR + HRDPS + RDPS + GDPS == 1`). Returns `true` if any pixel is visible.
pub(super) fn eccc_colorize(field: &[u16], em: EcccMask, coord: TileCoord, rgba: &mut [u8], wu: usize, hu: usize) -> bool {
    let rain_lut = &*RAIN_LUT;
    let (tz, tx, ty) = coord;
    let n_tiles = (1_u64 << tz) as f64;
    let lon_0 = (tx as f64 / n_tiles) * 360.0 - 180.0;
    let lon_step = 360.0 / (n_tiles * wu.max(1) as f64);
    let mut vis = false;
    // Calibrate ECCC `PRECIPPRTMMH-LINEAR` (0.01..50 mm/h) directly to Marshall-Palmer radar reflectivity (`dBZ`)
    // matching live `LibreWXR` scheme 8:
    // - Suppress sub-radar maritime trace drizzle `< ~0.18 mm/h` (`t < 0.29`, `< 15 dBZ`) so ocean static vanishes!
    // - Map 0.5 mm/h (~20 dBZ) -> rich blue, 4–10 mm/h (~35–42 dBZ) -> purple/rose-magenta, 25+ mm/h -> coral/amber!
    const T_MIN: f32 = 0.29;
    const T_MAX: f32 = 0.94;
    const T_MIN_U16: u16 = 19005;
    for py in 0..hu {
        let frow = &field[py * wu..(py + 1) * wu];
        let row = &mut rgba[py * wu * 4..(py + 1) * wu * 4];
        if !frow.iter().any(|&v| v >= T_MIN_U16) {
            row.fill(0);
            continue;
        }
        let lat_row =
            (std::f64::consts::PI * (1.0 - 2.0 * (ty as f64 + (py as f64 + 0.5) / hu.max(1) as f64) / n_tiles)).sinh().atan().to_degrees();
        for (px_i, (val, px)) in frow.iter().zip(row.as_chunks_mut::<4>().0.iter_mut()).enumerate() {
            if *val < T_MIN_U16 {
                *px = [0, 0, 0, 0];
                continue;
            }
            let lo = lon_0 + (px_i as f64 + 0.5) * lon_step;
            let mask = match em {
                EcccMask::Hrdps { outside_hrrr } => super::overlays::eccc_tier_weight(EcccModel::Hrdps, outside_hrrr, lat_row, lo),
                EcccMask::Rdps { outside_hrrr } => super::overlays::eccc_tier_weight(EcccModel::Rdps, outside_hrrr, lat_row, lo),
                EcccMask::Gdps { outside_hrrr } => super::overlays::eccc_tier_weight(EcccModel::Gdps, outside_hrrr, lat_row, lo),
                EcccMask::Legacy { outside_hrrr } => {
                    if outside_hrrr {
                        1.0 - super::overlays::hrrr_weight(lat_row, lo)
                    } else {
                        1.0
                    }
                }
            };
            if mask <= 0.002 {
                *px = [0, 0, 0, 0];
                continue;
            }
            let t = *val as f32 * (1.0 / 65535.0);
            let raw_i = ((t - T_MIN) * (1.0 / (T_MAX - T_MIN))).clamp(0.0, 1.0);
            let inten = raw_i.powf(1.12);
            let [cr, cg, cb] = rain_lut[(inten * 255.0) as usize];
            let mut na = (118.0 + inten * 130.0).min(248.0);
            if t < 0.41 {
                let u = (t - T_MIN) * (1.0 / (0.41 - T_MIN));
                na *= u * u * (3.0 - 2.0 * u);
            }
            let oa = (na * mask).round().clamp(0.0, 248.0) as u8;
            if oa < 3 {
                *px = [0, 0, 0, 0];
                continue;
            }
            vis = true;
            *px = [cr, cg, cb, oa];
        }
    }
    vis
}

pub(super) fn stylize_radar_tile(ns: &str, coord: TileCoord, rgba: &mut [u8], w: u32, h: u32) -> bool {
    let (refp, refd) = (ns.starts_with("hrrr_refp_"), ns.starts_with("hrrr_refd_"));
    let eccc_mode = EcccMask::from_ns(ns);
    let (nexrad, goes_ir) = (ns == "iem_nexrad", ns.starts_with("iem_goes_ir"));
    let lwxr_s8 = ns.starts_with("lwxr8_");
    let lwxr_sat = ns.starts_with("lwxr_sat_");
    let rv_ub = ns.starts_with("rv256_") || ns.starts_with("rv512_") || ns.starts_with("lwxr512_") || ns.starts_with("lwxr256_");
    if !refp && !refd && eccc_mode.is_none() && !nexrad && !goes_ir && !lwxr_s8 && !lwxr_sat && !rv_ub {
        return true;
    }
    let mut vis = false;
    if let Some(em) = eccc_mode {
        // Environment Canada (ECCC GeoMet) HRDPS 2.5 km / RDPS 10 km / GDPS 15 km `PRECIPPRTMMH-LINEAR` (MATLAB jet: 0.01..50 mm/h):
        // extract a continuous scalar field, smooth it BEFORE color-mapping, then apply `RAIN_LUT` + the domain mask.
        let (wu, hu) = (w as usize, h as usize);
        let npix = wu * hu;
        return SCALAR_SCRATCH.with(|cell| {
            let mut guard = cell.borrow_mut();
            let (f_v, tmp_v, row_v) = &mut *guard;
            if f_v.len() < npix {
                f_v.resize(npix, 0);
            }
            if tmp_v.len() < npix {
                tmp_v.resize(npix, 0);
            }
            if row_v.len() < hu {
                row_v.resize(hu, false);
            }
            let (field, tmp, row_any) = (&mut f_v[..npix], &mut tmp_v[..npix], &mut row_v[..hu]);
            if !eccc_extract_scalar(rgba, wu, hu, field) {
                rgba.fill(0);
                return false;
            }
            eccc_smooth_scalar(em, field, tmp, row_any, wu, hu);
            eccc_colorize(field, em, coord, rgba, wu, hu)
        });
    } else if lwxr_s8 {
        // LibreWXR scheme 8 PNGs are straight (non-premultiplied) RGBA in the Dark Sky / Apple Weather palette!
        // Never un-premultiply straight RGBA (that corrupted ~57% of medium-dark rain pixels into bright spotty patches!).
        let soft = &*SOFT_ALPHA;
        for pair in rgba.as_chunks_mut::<8>().0.iter_mut() {
            if u64::from_ne_bytes(*pair) == 0 {
                continue;
            }
            for px in pair.as_chunks_mut::<4>().0.iter_mut() {
                let a = px[3];
                if a < 10 {
                    px[3] = 0;
                    continue;
                }
                let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
                let a32 = a as u32;
                let is_snow = r > 85 && r < 175 && g > 190 && b > 210;
                let oa = soft[if is_snow { (a32 * 189) >> 8 } else { a32 } as usize];
                vis |= oa > 0;
                px[3] = oa;
            }
        }
        if vis {
            despeckle_radar_alpha(rgba, w as usize, h as usize);
            // Tiles are requested unsmoothed (the provider's exact palettized values, ~5× fewer bytes on the wire than its
            // server-side smoothed PNGs). The light `[1, 6, 1]` premultiplied-alpha pass is the closest match to that
            // server smoothing (measured against the same tile), without the dark fringes it leaves around snow.
            smooth_rgba_crisp(rgba, w as usize, h as usize);
        }
        return vis;
    } else if refp || refd {
        let rain_lut = &*RAIN_LUT;
        let (wu, hu) = (w as usize, h as usize);
        let (tz, tx, ty) = coord;
        let feather_rim = super::overlays::tile_extends_outside_hrrr(tz, tx, ty);
        for py in 0..hu {
            let row = &mut rgba[py * wu * 4..(py + 1) * wu * 4];
            if row.as_chunks::<8>().0.iter().all(|pair| u64::from_ne_bytes(*pair) == 0) {
                continue;
            }
            for px in row.as_chunks_mut::<4>().0.iter_mut() {
                let a = px[3];
                if a < 18 {
                    *px = [0, 0, 0, 0];
                    continue;
                }
                let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
                let sat = r.max(g).max(b) - r.min(g).min(b);
                if (refd && sat < 28 && r > 110 && g > 110) || (sat < 16 && r > 205 && g > 205 && b > 205) {
                    *px = [0, 0, 0, 0];
                    continue;
                }
                let (nr, ng, nb, na) = if refp && b > r + 14 && b >= g - 4 {
                    // Match live LibreWXR scheme-8 crisp ice-cyan snow `[96..118, 204..228, 248..255]` (never murky slate!).
                    let t = ((225 - r).clamp(0, 215) as f32) * (1.0 / 215.0);
                    ((116.0 - t * 24.0) as u8, (226.0 - t * 24.0) as u8, (255.0 - t * 6.0) as u8, (132.0 + t * 82.0) as u32)
                } else if refp && r > g + 18 && b > g + 12 {
                    let t = ((230 - g).clamp(0, 210) as f32) * (1.0 / 210.0);
                    ((216.0 + t * 28.0) as u8, (118.0 - t * 50.0) as u8, (242.0 - t * 48.0) as u8, (148.0 + t * 90.0) as u32)
                } else {
                    let inten = if r > 165 && b > 165 && g < 130 {
                        0.98_f32
                    } else if r > 195 && g < 120 && b < 110 {
                        0.84 + ((120 - g).clamp(0, 120) as f32) * (0.14 / 120.0)
                    } else if r > 195 && g >= 120 && b < 110 {
                        0.68 + ((245 - g).clamp(0, 135) as f32) * (0.16 / 135.0)
                    } else {
                        let lum = (77 * r + 150 * g + 29 * b) >> 8;
                        ((228 - lum) as f32 * (1.0 / 180.0)).clamp(0.06, 0.68)
                    };
                    let [cr, cg, cb] = rain_lut[(inten * 255.0) as usize];
                    let mut a_f = (108.0 + inten * 140.0).min(246.0);
                    if inten < 0.18 {
                        let u = (inten / 0.18).clamp(0.0, 1.0);
                        a_f *= u * u * (3.0 - 2.0 * u);
                    }
                    (cr, cg, cb, a_f as u32)
                };
                let oa = ((na * a as u32) >> 8).min(244) as u8;
                vis |= oa > 0;
                *px = [nr, ng, nb, oa];
            }
        }
        if vis {
            despeckle_radar_alpha(rgba, wu, hu);
            // At `tz <= 5`, 3 km HRRR cells are already sub-pixel — light 3×3 crisp anti-aliasing keeps storm cores
            // razor-sharp & cuts decode time ~40%, reserving 5×5 Gaussian smoothing for deep zoom `tz >= 6`!
            if tz >= 6 {
                smooth_rgba_gaussian(rgba, wu, hu, &[1]);
            } else {
                smooth_rgba_crisp(rgba, wu, hu);
            }
            let soft = &*SOFT_ALPHA;
            if feather_rim {
                vis = false;
                let n_tiles = (1_u64 << tz) as f64;
                let lon_0 = (tx as f64 / n_tiles) * 360.0 - 180.0;
                let lon_step = 360.0 / (n_tiles * wu.max(1) as f64);
                for py in 0..hu {
                    let row = &mut rgba[py * wu * 4..(py + 1) * wu * 4];
                    if !row_has_alpha(row) {
                        continue;
                    }
                    let lat_row = (std::f64::consts::PI * (1.0 - 2.0 * (ty as f64 + (py as f64 + 0.5) / hu.max(1) as f64) / n_tiles))
                        .sinh()
                        .atan()
                        .to_degrees();
                    for (px_i, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        if px[3] == 0 {
                            continue;
                        }
                        if px[3] < 24 {
                            px[3] = soft[px[3] as usize];
                        }
                        let lo = lon_0 + (px_i as f64 + 0.5) * lon_step;
                        let m = super::overlays::hrrr_weight(lat_row, lo);
                        let oa = (px[3] as f32 * m).round().clamp(0.0, 244.0) as u8;
                        if oa == 0 {
                            *px = [0, 0, 0, 0];
                        } else {
                            px[3] = oa;
                            vis = true;
                        }
                    }
                }
            } else {
                for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
                    if px[3] < 24 {
                        px[3] = soft[px[3] as usize];
                    }
                }
            }
        }
        return vis;
    } else if nexrad {
        let rain_lut = &*RAIN_LUT;
        let (wu, hu) = (w as usize, h as usize);
        let (tz, tx, ty) = coord;
        let (la0, la1, lo0, lo1) = super::overlays::tile_geo_bounds(tz, tx, ty);
        let feather_rim = la0 < 25.6 || la1 > 48.4 || lo0 < -124.4 || lo1 > -67.6;
        let n_tiles = (1_u64 << tz) as f64;
        let lon_0 = (tx as f64 / n_tiles) * 360.0 - 180.0;
        let lon_step = 360.0 / (n_tiles * wu.max(1) as f64);
        const NEX_BLEND: f64 = 1.6;
        for py in 0..hu {
            let row = &mut rgba[py * wu * 4..(py + 1) * wu * 4];
            if !row_has_alpha(row) {
                continue;
            }
            let lat_d = if feather_rim {
                let lat_row = (std::f64::consts::PI * (1.0 - 2.0 * (ty as f64 + (py as f64 + 0.5) / hu.max(1) as f64) / n_tiles))
                    .sinh()
                    .atan()
                    .to_degrees();
                (lat_row - 24.0).min(50.0 - lat_row)
            } else {
                NEX_BLEND + 1.0
            };
            if feather_rim && lat_d <= 0.0 {
                row.fill(0);
                continue;
            }
            for (px_i, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let a = px[3];
                if a < 14 {
                    px[3] = 0;
                    continue;
                }
                let rim_mask = if feather_rim {
                    let lo = lon_0 + (px_i as f64 + 0.5) * lon_step;
                    let inside_d = lat_d.min(lo - (-126.0)).min(-66.0 - lo);
                    if inside_d <= 0.0 {
                        *px = [0, 0, 0, 0];
                        continue;
                    }
                    if inside_d >= NEX_BLEND {
                        1.0_f32
                    } else {
                        let u = (inside_d / NEX_BLEND) as f32;
                        u * u * (3.0 - 2.0 * u)
                    }
                } else {
                    1.0_f32
                };
                let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
                let sat = r.max(g).max(b) - r.min(g).min(b);
                // Suppress < 15 dBZ biological/ground clutter speckles on US NEXRAD HD while preserving real light rain.
                if sat < 30 || (r > 95 && g > 105 && b > 130 && (b - r) < 68) || (g <= r + 18 && r < 145 && b < 175) {
                    px[3] = 0;
                    continue;
                }
                let inten = if r > 170 && b > 155 && g < 130 {
                    0.98_f32
                } else if r > 190 && g < 95 {
                    0.84 + ((95 - g).clamp(0, 95) as f32) * (0.12 / 95.0)
                } else if r > 185 && g >= 95 && b < 90 {
                    0.60 + ((240 - g).clamp(0, 150) as f32) * (0.24 / 150.0)
                } else if g > r + 20 {
                    0.18 + ((230 - g).clamp(0, 180) as f32) * (0.40 / 180.0)
                } else {
                    0.06 + ((b - r).max(0) as f32) * (0.14 / 255.0)
                };
                let [cr, cg, cb] = rain_lut[(inten * 255.0) as usize];
                let na = ((95.0 + inten * 148.0) as u32).min(242);
                let oa = ((((na * a as u32) >> 8) as f32) * rim_mask).round().clamp(0.0, 242.0) as u8;
                vis |= oa > 0;
                *px = [cr, cg, cb, oa];
            }
        }
    } else if goes_ir {
        // Decode IEM GOES-16/18 Ch13 IR: continuous grayscale clouds (106..=255) smoothly merged with ALL color-enhanced
        // cold cloud-top bands (purple/violet -> blue/cyan -> green -> yellow/red) — zero black cracks or binary cliffs!
        let gl = &*GOES_LUT;
        for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
            let a = px[3];
            if a < 8 {
                px[3] = 0;
                continue;
            }
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            let maxc = r.max(g).max(b);
            let minc = r.min(g).min(b);
            let sat = maxc - minc;
            let ci = if sat <= 20 {
                // Grayscale IR brightness: <=106 is warm clear ocean/ground; 107..=255 ramps smoothly from thin wispy clouds to bright overcast.
                let lum = (r + g * 2 + b) >> 2;
                if lum <= 106 {
                    px[3] = 0;
                    continue;
                }
                (((lum - 106) * 204) / 149).clamp(0, 204)
            } else if r >= 165 && b < 120 {
                // Yellow / orange / red / white-core extreme convective tops
                240 + (((255 - g) * 15) >> 8).clamp(0, 15)
            } else if g >= 115 && g >= r && b < 160 {
                // Cold green cloud tops
                228 + ((r * 12) / 200).clamp(0, 12)
            } else if b >= 120 && r > 55 && g < 115 && (b - g) > 40 {
                // Purple / violet / indigo enhanced IR band (transitions cleanly from bright white 204 -> cold blue 216!)
                204 + (((255 - r.min(255)) * 12) / 200).clamp(0, 12)
            } else if b > r + 20 {
                // Cold blue / cyan band
                216 + (((g.max(0)) * 12) / 200).clamp(0, 12)
            } else {
                // Any remaining warm/mixed enhanced pixel: use max channel so no cloud interior ever drops to black!
                192 + (((maxc - 100) * 32) / 155).clamp(0, 32)
            };
            let [nr, ng, nb, ba] = gl[ci as usize];
            let oa = ((ba as u32 * a as u32) >> 8) as u8;
            if oa < 3 {
                px[3] = 0;
                continue;
            }
            vis = true;
            *px = [nr, ng, nb, oa];
        }
    } else if lwxr_sat {
        // LibreWXR global 512×512 IR satellite PNGs are clean straight RGBA clouds (`alpha 0..197`, cool silver RGB
        // `116..183`). Lift luminance into crisp luminous white cloud tops while preserving wispy translucent edges!
        let soft = &*SOFT_ALPHA;
        for pair in rgba.as_chunks_mut::<8>().0.iter_mut() {
            if u64::from_ne_bytes(*pair) == 0 {
                continue;
            }
            for px in pair.as_chunks_mut::<4>().0.iter_mut() {
                let a = px[3];
                if a < 8 {
                    *px = [0, 0, 0, 0];
                    continue;
                }
                let oa = soft[((a as u32 * 316) >> 8).min(234) as usize];
                if oa < 3 {
                    *px = [0, 0, 0, 0];
                    continue;
                }
                let r = ((px[0] as u32 * 336) >> 8).min(248) as u8;
                let g = ((px[1] as u32 * 340) >> 8).min(252) as u8;
                let b = ((px[2] as u32 * 348) >> 8).min(255) as u8;
                vis = true;
                *px = [r, g, b, oa];
            }
        }
        return vis;
    } else {
        // Harmonize RainViewer Universal Blue fallback across the full multi-hue Dark Sky / Apple Weather ramp (straight RGBA!).
        let rain_lut = &*RAIN_LUT;
        for pair in rgba.as_chunks_mut::<8>().0.iter_mut() {
            if u64::from_ne_bytes(*pair) == 0 {
                continue;
            }
            for px in pair.as_chunks_mut::<4>().0.iter_mut() {
                let a = px[3];
                if a < 10 {
                    px[3] = 0;
                    continue;
                }
                let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
                let (nr, ng, nb, na) = if r > 175 && b > 175 && g < 165 {
                    let [cr, cg, cb] = rain_lut[250];
                    (cr, cg, cb, 240_u32)
                } else if r > 175 && g < 145 {
                    let inten = 0.80 + ((145 - g) as f32) * (0.16 / 145.0);
                    let [cr, cg, cb] = rain_lut[(inten * 255.0) as usize];
                    (cr, cg, cb, (212.0 + inten * 30.0) as u32)
                } else if r > 180 && g > 145 && b < 110 {
                    let inten = 0.60 + ((245_u32.saturating_sub(g)).min(130) as f32) * (0.20 / 130.0);
                    let [cr, cg, cb] = rain_lut[(inten * 255.0) as usize];
                    (cr, cg, cb, (186.0 + inten * 42.0) as u32)
                } else if r > 165 && g > 205 && b > 225 {
                    (126, 224, 255, 152)
                } else {
                    let lum = (77 * r + 150 * g + 29 * b) as f32 * (1.0 / 65280.0);
                    let inten = ((0.88 - lum) * (1.0 / 0.72)).clamp(0.04, 0.56);
                    let [cr, cg, cb] = rain_lut[(inten * 255.0) as usize];
                    (cr, cg, cb, (96.0 + inten * 124.0) as u32)
                };
                let oa = ((na * a as u32) >> 8).min(245) as u8;
                vis |= oa > 0;
                *px = [nr, ng, nb, oa];
            }
        }
    }
    if vis {
        if goes_ir {
            // Fast 2-pass separable [1, 2, 1]/4 alpha-only smooth for monochromatic silver-white GOES clouds.
            smooth_goes_clouds(rgba, w as usize, h as usize);
        } else {
            despeckle_radar_alpha(rgba, w as usize, h as usize);
            // NEXRAD / RainViewer fallback: 1-pass 5×5 Gaussian melts grid steps smoothly while preserving front edges.
            smooth_rgba_gaussian(rgba, w as usize, h as usize, &[1]);
            let soft = &*SOFT_ALPHA;
            for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
                if px[3] < 24 {
                    px[3] = soft[px[3] as usize];
                }
            }
        }
    }
    vis
}

/// Zero-alloc, division-free alpha-weighted 3-tap `[1, 6, 1] >> 3` separable micro-antialias for high-res radar & NWP
/// (LibreWXR, NOAA HRRR 3 km, ECCC HRDPS 2.5 km): softens 1px quantization noise while preserving sharp rainband filaments.
fn smooth_rgba_crisp(rgba: &mut [u8], w: usize, h: usize) {
    if w < 3 || h < 3 || rgba.len() != w * h * 4 {
        return;
    }
    const HALF: u32 = 1 << 22;
    let recip = &*RECIP23;
    let rb = w * 4;
    FILTER_SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let (tmp_v, row_v) = &mut *guard;
        if tmp_v.len() < rgba.len() {
            tmp_v.resize(rgba.len(), 0);
        }
        if row_v.len() < h {
            row_v.resize(h, false);
        }
        let tmp = &mut tmp_v[..rgba.len()];
        let row_any = &mut row_v[..h];
        for (y, has) in row_any.iter_mut().enumerate() {
            let off = y * rb;
            let src = &rgba[off..off + rb];
            let dst = &mut tmp[off..off + rb];
            let non_empty = row_has_alpha(src);
            *has = non_empty;
            if !non_empty {
                dst.fill(0);
                continue;
            }
            for x in [0, w - 1] {
                let (i0, i1, i2) = (x.saturating_sub(1) * 4, x * 4, (x + 1).min(w - 1) * 4);
                let (a0, a1, a2) = (src[i0 + 3] as u32, src[i1 + 3] as u32, src[i2 + 3] as u32);
                let w1 = a1 * 6;
                let sa = a0 + w1 + a2;
                if sa < 8 {
                    dst[i1..i1 + 4].copy_from_slice(&[0, 0, 0, 0]);
                } else {
                    let inv = recip[sa as usize];
                    let r = src[i0] as u32 * a0 + src[i1] as u32 * w1 + src[i2] as u32 * a2;
                    let g = src[i0 + 1] as u32 * a0 + src[i1 + 1] as u32 * w1 + src[i2 + 1] as u32 * a2;
                    let b = src[i0 + 2] as u32 * a0 + src[i1 + 2] as u32 * w1 + src[i2 + 2] as u32 * a2;
                    dst[i1..i1 + 4].copy_from_slice(&[
                        ((r * inv + HALF) >> 23).min(255) as u8,
                        ((g * inv + HALF) >> 23).min(255) as u8,
                        ((b * inv + HALF) >> 23).min(255) as u8,
                        ((sa + 4) >> 3).min(255) as u8,
                    ]);
                }
            }
            for x in 1..w - 1 {
                let c = x * 4;
                let (a0, a1, a2) = (src[c - 1] as u32, src[c + 3] as u32, src[c + 7] as u32);
                if (a0 | a1 | a2) == 0 {
                    dst[c..c + 4].copy_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let w1 = a1 * 6;
                let sa = a0 + w1 + a2;
                if sa < 8 {
                    dst[c..c + 4].copy_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let inv = recip[sa as usize];
                let r = src[c - 4] as u32 * a0 + src[c] as u32 * w1 + src[c + 4] as u32 * a2;
                let g = src[c - 3] as u32 * a0 + src[c + 1] as u32 * w1 + src[c + 5] as u32 * a2;
                let b = src[c - 2] as u32 * a0 + src[c + 2] as u32 * w1 + src[c + 6] as u32 * a2;
                dst[c..c + 4].copy_from_slice(&[
                    ((r * inv + HALF) >> 23).min(255) as u8,
                    ((g * inv + HALF) >> 23).min(255) as u8,
                    ((b * inv + HALF) >> 23).min(255) as u8,
                    ((sa + 4) >> 3).min(255) as u8,
                ]);
            }
        }
        for y in 0..h {
            let (sy0, sy1, sy2) = (y.saturating_sub(1), y, (y + 1).min(h - 1));
            let dst = &mut rgba[y * rb..(y + 1) * rb];
            if !(row_any[sy0] || row_any[sy1] || row_any[sy2]) {
                dst.fill(0);
                continue;
            }
            let (r0, r1, r2) = (&tmp[sy0 * rb..sy0 * rb + rb], &tmp[sy1 * rb..sy1 * rb + rb], &tmp[sy2 * rb..sy2 * rb + rb]);
            for x in 0..w {
                let c = x * 4;
                let (a0, a1, a2) = (r0[c + 3] as u32, r1[c + 3] as u32, r2[c + 3] as u32);
                if (a0 | a1 | a2) == 0 {
                    dst[c..c + 4].copy_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let w1 = a1 * 6;
                let sa = a0 + w1 + a2;
                if sa < 8 {
                    dst[c..c + 4].copy_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let inv = recip[sa as usize];
                let r = r0[c] as u32 * a0 + r1[c] as u32 * w1 + r2[c] as u32 * a2;
                let g = r0[c + 1] as u32 * a0 + r1[c + 1] as u32 * w1 + r2[c + 1] as u32 * a2;
                let b = r0[c + 2] as u32 * a0 + r1[c + 2] as u32 * w1 + r2[c + 2] as u32 * a2;
                dst[c..c + 4].copy_from_slice(&[
                    ((r * inv + HALF) >> 23).min(255) as u8,
                    ((g * inv + HALF) >> 23).min(255) as u8,
                    ((b * inv + HALF) >> 23).min(255) as u8,
                    ((sa + 4) >> 3).min(255) as u8,
                ]);
            }
        }
    });
}

/// Ultra-fast 3-tap `[1, 2, 1] >> 2` separable blur tailored for GOES-IR silver-white clouds (zero divisions, zero RGB weighting!).
fn smooth_goes_clouds(rgba: &mut [u8], w: usize, h: usize) {
    if w < 3 || h < 3 || rgba.len() != w * h * 4 {
        return;
    }
    thread_local! {
        static GOES_A: std::cell::RefCell<(Vec<u8>, Vec<u8>)> = const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    }
    let gl = &*GOES_LUT;
    GOES_A.with(|cell| {
        let mut g = cell.borrow_mut();
        let (a0, a1) = &mut *g;
        let n = w * h;
        a0.resize(n, 0);
        a1.resize(n, 0);
        for (dst, px) in a0[..n].iter_mut().zip(rgba.as_chunks::<4>().0.iter()) {
            *dst = px[3];
        }
        // 2-pass separable [1, 2, 1] >> 2 alpha feathering for silky, atmospheric GOES satellite clouds!
        for pass in 0..2 {
            // Horizontal
            for y in 0..h {
                let row = y * w;
                let s = &a0[row..row + w];
                let d = &mut a1[row..row + w];
                d[0] = ((s[0] as u16 * 3 + s[1] as u16 + 2) >> 2) as u8;
                for x in 1..w - 1 {
                    d[x] = ((s[x - 1] as u16 + (s[x] as u16) * 2 + s[x + 1] as u16 + 2) >> 2) as u8;
                }
                d[w - 1] = ((s[w - 2] as u16 + s[w - 1] as u16 * 3 + 2) >> 2) as u8;
            }
            if pass == 0 {
                // Vertical into a0 for second pass
                for y in 0..h {
                    let r0 = &a1[y.saturating_sub(1) * w..y.saturating_sub(1) * w + w];
                    let r1 = &a1[y * w..y * w + w];
                    let r2 = &a1[(y + 1).min(h - 1) * w..(y + 1).min(h - 1) * w + w];
                    let dst = &mut a0[y * w..(y + 1) * w];
                    for x in 0..w {
                        dst[x] = ((r0[x] as u16 + (r1[x] as u16) * 2 + r2[x] as u16 + 2) >> 2) as u8;
                    }
                }
            } else {
                // Final vertical pass + map back to silky silver-white cloud tint
                for y in 0..h {
                    let r0 = &a1[y.saturating_sub(1) * w..y.saturating_sub(1) * w + w];
                    let r1 = &a1[y * w..y * w + w];
                    let r2 = &a1[(y + 1).min(h - 1) * w..(y + 1).min(h - 1) * w + w];
                    let out_row = &mut rgba[y * w * 4..(y + 1) * w * 4];
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let a = ((r0[x] as u16 + (r1[x] as u16) * 2 + r2[x] as u16 + 2) >> 2) as u8;
                        if a < 3 {
                            // Pre-fill transparent pixels with silver-white cloud edge RGB so GPU linear filtering has zero dark halos!
                            *px = [182, 204, 232, 0];
                        } else {
                            let [cr, cg, cb, _] = gl[((a as usize * 255) / 224).min(255)];
                            *px = [cr, cg, cb, a];
                        }
                    }
                }
            }
        }
    });
}

/// Zero-allocation boundary-push edge bleed with fast word-skip for empty rows and bounds-check-free interior pass.
pub(super) fn bleed_transparent_edges(rgba: &mut [u8], w: u32, h: u32) {
    let (w, h) = (w as usize, h as usize);
    if w < 2 || h < 2 || rgba.len() != w * h * 4 {
        return;
    }
    let rb = w * 4;
    let bleed_px = |buf: &mut [u8], x: usize, y: usize, rgb: [u8; 3]| {
        let y0 = y.saturating_sub(1);
        let y1 = (y + 1).min(h - 1);
        let x0 = x.saturating_sub(1);
        let x1 = (x + 1).min(w - 1);
        for ny in y0..=y1 {
            let nr = ny * rb;
            for nx in x0..=x1 {
                let ni = nr + nx * 4;
                if buf[ni + 3] == 0 {
                    buf[ni] = rgb[0];
                    buf[ni + 1] = rgb[1];
                    buf[ni + 2] = rgb[2];
                }
            }
        }
    };
    for y in 0..h {
        let row = y * rb;
        if !row_has_alpha(&rgba[row..row + rb]) {
            continue;
        }
        let interior_y = y > 0 && y + 1 < h;
        if interior_y {
            // Left/right border pixels of this row
            for x in [0, w - 1] {
                let i = row + x * 4;
                if rgba[i + 3] != 0 {
                    let rgb = [rgba[i], rgba[i + 1], rgba[i + 2]];
                    bleed_px(rgba, x, y, rgb);
                }
            }
            // Fast interior x in 1..w-1: 8-pixel (4×u64) zero-alpha word skip + branchless cardinal-alpha interior skip!
            const ALPHA_PAIR_MASK: u64 = 0xFF00_0000_FF00_0000;
            let mut x = 1_usize;
            while x + 8 < w {
                let i = row + x * 4;
                let s = &rgba[i..i + 32];
                let a_or = (u64::from_le_bytes(s[0..8].try_into().unwrap())
                    | u64::from_le_bytes(s[8..16].try_into().unwrap())
                    | u64::from_le_bytes(s[16..24].try_into().unwrap())
                    | u64::from_le_bytes(s[24..32].try_into().unwrap()))
                    & ALPHA_PAIR_MASK;
                if a_or == 0 {
                    x += 8;
                    continue;
                }
                for dx in 0..8 {
                    let ii = i + dx * 4;
                    if rgba[ii + 3] == 0 {
                        continue;
                    }
                    if rgba[ii - 1].min(rgba[ii + 7]).min(rgba[ii - rb + 3]).min(rgba[ii + rb + 3]) != 0 {
                        continue;
                    }
                    let rgb = [rgba[ii], rgba[ii + 1], rgba[ii + 2]];
                    let cx = x + dx;
                    for dy in [row - rb, row, row + rb] {
                        for ni in [dy + (cx - 1) * 4, dy + cx * 4, dy + (cx + 1) * 4] {
                            if rgba[ni + 3] == 0 {
                                rgba[ni] = rgb[0];
                                rgba[ni + 1] = rgb[1];
                                rgba[ni + 2] = rgb[2];
                            }
                        }
                    }
                }
                x += 8;
            }
            while x < w - 1 {
                let i = row + x * 4;
                if rgba[i + 3] != 0 && rgba[i - 1].min(rgba[i + 7]).min(rgba[i - rb + 3]).min(rgba[i + rb + 3]) == 0 {
                    let rgb = [rgba[i], rgba[i + 1], rgba[i + 2]];
                    for dy in [row - rb, row, row + rb] {
                        for ni in [dy + (x - 1) * 4, dy + x * 4, dy + (x + 1) * 4] {
                            if rgba[ni + 3] == 0 {
                                rgba[ni] = rgb[0];
                                rgba[ni + 1] = rgb[1];
                                rgba[ni + 2] = rgb[2];
                            }
                        }
                    }
                }
                x += 1;
            }
        } else {
            for x in 0..w {
                let i = row + x * 4;
                if rgba[i + 3] == 0 {
                    continue;
                }
                let b_l = x > 0 && rgba[i - 1] == 0;
                let b_r = x + 1 < w && rgba[i + 7] == 0;
                let b_u = y > 0 && rgba[i - rb + 3] == 0;
                let b_d = y + 1 < h && rgba[i + rb + 3] == 0;
                if b_l || b_r || b_u || b_d {
                    let rgb = [rgba[i], rgba[i + 1], rgba[i + 2]];
                    bleed_px(rgba, x, y, rgb);
                }
            }
        }
    }
}

/// Hi-DPI supersampling bias for 256×256 Esri basemaps and satellite imagery:
/// switches to `z+1` at fractional zoom `z >= k + 0.42` (e.g. `5.5 -> z=6`), rendering coastlines, terrain, and
/// satellite detail supersampled (~0.71×–1.34×) instead of blurry `1.41×–1.56×` upscaling on 1440p/4K displays!
pub(super) const BASE_ZOOM_BIAS: f64 = 0.58;
/// Crisp 1:1 readability bias for rasterized Esri city/place label overlays so pre-baked text strokes never shrink
/// below ~0.78× or become thin/aliased at fractional zooms.
pub(super) const LABEL_ZOOM_BIAS: f64 = 0.35;

pub(super) fn effective_tile_zoom(zoom: f64, max_z: u8) -> u8 {
    effective_tile_zoom_biased(zoom, max_z, BASE_ZOOM_BIAS)
}
pub(super) fn effective_tile_zoom_biased(zoom: f64, max_z: u8, bias: f64) -> u8 {
    ((zoom + bias).floor() as i32).clamp(2, max_z as i32) as u8
}
pub(super) fn tile_coord_at(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let (n, (wx, wy)) = (1_i64 << z, lat_lon_to_world_px(lat, lon, z as f64));
    ((wx / 256.0).floor().rem_euclid(n as f64) as u32, (wy / 256.0).floor().clamp(0.0, (n - 1) as f64) as u32)
}

pub(super) struct TileLayer<'a> {
    pub ns: &'a str,
    pub kind: TileLayerKind,
    pub max_z: u8,
    pub zoom_bias: f64,
    pub opacity: f32,
    pub opaque_base: bool,
    pub fallback: bool,
    pub fade: bool,
    pub allow_fetch: bool,
    pub ttl: Duration,
    pub temporal_fallbacks: [Option<u32>; 2],
    /// Optional sub-frame screen-pixel translation `(dx_px, dy_px)` for seamless optical-flow advection between frames.
    pub shift_px: (f32, f32),
}
impl<'a> TileLayer<'a> {
    pub fn overlay(ns: &'a str, kind: TileLayerKind, max_z: u8, opacity: f32, ttl: Duration, allow_fetch: bool, fade: bool) -> Self {
        Self {
            ns,
            kind,
            max_z,
            zoom_bias: BASE_ZOOM_BIAS,
            opacity,
            opaque_base: false,
            fallback: true,
            fade,
            allow_fetch,
            ttl,
            temporal_fallbacks: [None; 2],
            shift_px: (0.0, 0.0),
        }
    }
    pub fn with_zoom_bias(mut self, bias: f64) -> Self {
        self.zoom_bias = bias;
        self
    }
    pub fn with_temporal_fallbacks(mut self, fallbacks: [Option<u32>; 2]) -> Self {
        self.temporal_fallbacks = fallbacks;
        self
    }
    pub fn with_shift_px(mut self, shift_px: (f32, f32)) -> Self {
        self.shift_px = shift_px;
        self
    }
}

pub(super) fn draw_tile_layer(
    frame: &mut Frame, pool: &mut SharedTilePool, vp: MapVp, l: TileLayer<'_>, url_fn: impl Fn(u8, u32, u32) -> String,
) {
    let ns_id = pool.intern(l.ns);
    draw_tile_layer_filtered(frame, pool, ns_id, vp, l, |_, _, _| true, url_fn);
}

pub(super) fn draw_tile_layer_filtered(
    frame: &mut Frame, pool: &mut SharedTilePool, ns_id: u32, (bounds, lat, lon, zoom): MapVp, l: TileLayer<'_>,
    filter: impl Fn(u8, u32, u32) -> bool, url_fn: impl Fn(u8, u32, u32) -> String,
) {
    let (vp_rect, tz, now) = (Rectangle::with_size(bounds), effective_tile_zoom_biased(zoom, l.max_z, l.zoom_bias), Instant::now());
    let c_raw = lat_lon_to_world_px(lat, lon, zoom);
    let (px, n, c) = (256.0 * 2.0_f64.powf(zoom - tz as f64), 1_i64 << tz, (c_raw.0 - l.shift_px.0 as f64, c_raw.1 - l.shift_px.1 as f64));
    let half = (bounds.width as f64 * 0.5, bounds.height as f64 * 0.5);
    let (min_x, max_x) = (((c.0 - half.0) / px).floor() as i64, ((c.0 + half.0) / px).ceil() as i64 - 1);
    let (min_y, max_y) = ((((c.1 - half.1) / px).floor() as i64).max(0), ((((c.1 + half.1) / px).ceil() as i64) - 1).min(n - 1));
    if min_y > max_y || min_x > max_x {
        return;
    }
    // Exact subpixel positioning (zero staircase shimmer during pan/zoom); tiny 0.35px overlap on opaque basemaps prevents hairline seams.
    let trect = |(gx, gy): (i64, i64), p: f64, bleed: bool| {
        let (x0, x1) = ((half.0 + gx as f64 * p - c.0) as f32, (half.0 + (gx + 1) as f64 * p - c.0) as f32);
        let (y0, y1) = ((half.1 + gy as f64 * p - c.1) as f32, (half.1 + (gy + 1) as f64 * p - c.1) as f32);
        let pad = if bleed { 0.35_f32 } else { 0.0 };
        Rectangle { x: x0 - pad, y: y0 - pad, width: (x1 - x0 + pad * 2.0).max(1.0), height: (y1 - y0 + pad * 2.0).max(1.0) }
    };
    let dimg = |f: &mut Frame, r: Rectangle, h: adv_image::Handle, alpha: f32| {
        if alpha > 0.01 {
            f.draw_image(r, adv_image::Image::new(h).filter_method(adv_image::FilterMethod::Linear).opacity(alpha));
        }
    };
    let fade_of = |t: Instant| {
        if !l.fade {
            return 1.0;
        }
        let u = (now.saturating_duration_since(t).as_secs_f32() / TILE_FADE_SECS).clamp(0.0, 1.0);
        1.0 - (1.0 - u) * (1.0 - u)
    };
    let (f_min_x, f_max_x) = (((c_raw.0 - half.0) / px).floor() as i64, ((c_raw.0 + half.0) / px).ceil() as i64 - 1);
    let (f_min_y, f_max_y) =
        ((((c_raw.1 - half.1) / px).floor() as i64).max(0), ((((c_raw.1 + half.1) / px).ceil() as i64) - 1).min(n - 1));
    if l.allow_fetch && f_min_y <= f_max_y && f_min_x <= f_max_x {
        // Fetch strictly visible unshifted filtered tiles (zero phantom fetches from optical-flow advection shift!).
        let (cx_f, cy_f) = (c_raw.0 / px - 0.5, c_raw.1 / px - 0.5);
        thread_local! {
            static TILE_REQS: std::cell::RefCell<Vec<(i64, i64, f64)>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        TILE_REQS.with(|cell| {
            let mut reqs = cell.borrow_mut();
            reqs.clear();
            for ty in f_min_y..=f_max_y {
                for tx in f_min_x..=f_max_x {
                    let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
                    if !filter(tz, wx, wy) {
                        continue;
                    }
                    if pool.get_ready_id(ns_id, (tz, wx, wy)).is_none() {
                        pool.mark_wanted(l.kind, TileKey(ns_id, tz, wx, wy));
                        let d2 = (tx as f64 - cx_f).powi(2) + (ty as f64 - cy_f).powi(2);
                        reqs.push((tx, ty, d2));
                    }
                }
            }
            // Strict FIFO priority queue: sort nearest viewport-center tiles first!
            reqs.sort_by(|a, b| a.2.total_cmp(&b.2));
            for &(tx, ty, _) in reqs.iter() {
                let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
                let _ = pool.ensure_id_prio(ns_id, (tz, wx, wy), || url_fn(tz, wx, wy), l.ttl, PRIO_MAX, true);
            }
        });
    } else if f_min_y <= f_max_y && f_min_x <= f_max_x {
        for ty in f_min_y..=f_max_y {
            for tx in f_min_x..=f_max_x {
                let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
                if filter(tz, wx, wy) && pool.get_ready_id(ns_id, (tz, wx, wy)).is_none() {
                    pool.mark_wanted(l.kind, TileKey(ns_id, tz, wx, wy));
                }
            }
        }
    }
    let anc = |pool: &SharedTilePool, wx: u32, wy: u32, md: u8| {
        (1..=md).take_while(|&dz| tz >= dz + 2).find_map(|dz| {
            let (az, awx, awy) = (tz - dz, wx >> dz, wy >> dz);
            pool.get_ready_id(ns_id, (az, awx, awy)).and_then(Option::as_ref).cloned().map(|h| (dz, az, awx, awy, h))
        })
    };
    // For text label overlays, avoid mixing clipped stretched ancestor text beside crisp loaded tiles (zero sliced letters!).
    let is_labels = l.ns.ends_with("_labels");
    let any_tz_ready = is_labels
        && (min_y..=max_y).any(|ty| (min_x..=max_x).any(|tx| pool.get_ready_id(ns_id, (tz, tx.rem_euclid(n) as u32, ty as u32)).is_some()));
    // Pass 1 (opaque basemaps): draw ancestor OR crisp zoom-out child tiles underneath any missing or fading tile!
    // Reuse thread-local scratch buffers so 60fps multi-layer draws do 0 heap allocations and skip Handle clones for pinned tiles.
    type DrawScratch = (FxHashSet<(u8, i64, u32)>, Vec<(TileKey, adv_image::Handle)>);
    thread_local! {
        static DRAW_SCRATCH: std::cell::RefCell<DrawScratch> = std::cell::RefCell::new((FxHashSet::default(), Vec::new()));
    }
    DRAW_SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        let (drawn_anc, to_pin) = &mut *scratch;
        drawn_anc.clear();
        to_pin.clear();
        for pass in [true, false] {
            if pass && !(l.opaque_base && l.fallback) {
                continue;
            }
            for ty in min_y..=max_y {
                for tx in min_x..=max_x {
                    let (wx, wy) = (tx.rem_euclid(n) as u32, ty as u32);
                    if !filter(tz, wx, wy) {
                        continue;
                    }
                    let slot = pool.get_ready_timed(ns_id, (tz, wx, wy));
                    let fully_opaque = slot.is_some_and(|(h, t)| h.is_none() || fade_of(t) >= 0.995);
                    if pass {
                        if fully_opaque {
                            continue;
                        }
                        // Try higher-res child tiles first (instant crisp zoom-out), then ancestor fallback (zoom-in).
                        let mut drew_child = false;
                        if tz < l.max_z {
                            let (cz, cpx) = (tz + 1, px * 0.5);
                            for cdy in 0..2_u32 {
                                for cdx in 0..2_u32 {
                                    let (cwx, cwy) = (wx * 2 + cdx, wy * 2 + cdy);
                                    if let Some(Some(ch)) = pool.get_ready_id(ns_id, (cz, cwx, cwy)) {
                                        let r = trect((tx * 2 + cdx as i64, ty * 2 + cdy as i64), cpx, true);
                                        if r.intersects(&vp_rect) {
                                            let pk = TileKey(ns_id, cz, cwx, cwy);
                                            if !pool.is_gpu_pin_requested(pk) {
                                                to_pin.push((pk, ch.clone()));
                                            }
                                            dimg(frame, r, ch.clone(), l.opacity);
                                            drew_child = true;
                                        }
                                    }
                                }
                            }
                        }
                        if !drew_child && let Some((dz, az, awx, awy, ah)) = anc(pool, wx, wy, 5) {
                            let gax = tx.div_euclid(n) * (1_i64 << az) + awx as i64;
                            if drawn_anc.insert((az, gax, awy)) {
                                let r = trect((gax, awy as i64), px * (1_u32 << dz) as f64, true);
                                if r.intersects(&vp_rect) {
                                    let pk = TileKey(ns_id, az, awx, awy);
                                    if !pool.is_gpu_pin_requested(pk) {
                                        to_pin.push((pk, ah.clone()));
                                    }
                                    dimg(frame, r, ah, l.opacity);
                                }
                            }
                        }
                        continue;
                    }
                    let dst = trect((tx, ty), px, l.opaque_base);
                    if !dst.intersects(&vp_rect) {
                        continue;
                    }
                    match slot {
                        Some((Some(h), arrived)) => {
                            let f = if l.fallback { fade_of(arrived) } else { 1.0 };
                            if !l.opaque_base
                                && l.fallback
                                && !is_labels
                                && f < 0.995
                                && let Some((dz, az, awx, awy, ah)) = anc(pool, wx, wy, 2)
                                && let Some(clip) = dst.intersection(&vp_rect)
                            {
                                let r =
                                    trect((tx.div_euclid(n) * (1_i64 << az) + awx as i64, awy as i64), px * (1_u32 << dz) as f64, false);
                                let pk = TileKey(ns_id, az, awx, awy);
                                if !pool.is_gpu_pin_requested(pk) {
                                    to_pin.push((pk, ah.clone()));
                                }
                                frame.with_clip(clip, |sub| dimg(sub, r, ah, l.opacity * (1.0 - f)));
                            }
                            let pk = TileKey(ns_id, tz, wx, wy);
                            if !pool.is_gpu_pin_requested(pk) {
                                to_pin.push((pk, h.clone()));
                            }
                            dimg(frame, dst, h.clone(), l.opacity * f);
                        }
                        None if !l.opaque_base && l.fallback && (!is_labels || !any_tz_ready) => {
                            // Seamless zoom-out (crisp 2x2 child tiles) AND zoom-in (clipped ancestor tile) fallback for weather overlays!
                            // When only some of the 4 child quadrants are cached, draw the ready children AND clip the ancestor into
                            // the remaining missing quadrants so there is never a blank corner OR double-drawn overlap!
                            let max_dz = if is_labels { 1 } else { 3 };
                            let mut all_children_settled = false;
                            let mut any_child_settled = false;
                            if !is_labels && tz < l.max_z {
                                let (cz, cpx) = (tz + 1, px * 0.5);
                                let mut settled_cnt = 0_u8;
                                for cdy in 0..2_u32 {
                                    for cdx in 0..2_u32 {
                                        if pool.get_ready_id(ns_id, (cz, wx * 2 + cdx, wy * 2 + cdy)).is_some() {
                                            settled_cnt += 1;
                                        }
                                    }
                                }
                                any_child_settled = settled_cnt > 0;
                                all_children_settled = settled_cnt == 4;
                                if any_child_settled {
                                    let anc_fb = if all_children_settled { None } else { anc(pool, wx, wy, max_dz) };
                                    for cdy in 0..2_u32 {
                                        for cdx in 0..2_u32 {
                                            let r = trect((tx * 2 + cdx as i64, ty * 2 + cdy as i64), cpx, false);
                                            if !r.intersects(&vp_rect) {
                                                continue;
                                            }
                                            let (cwx, cwy) = (wx * 2 + cdx, wy * 2 + cdy);
                                            if let Some(ch_opt) = pool.get_ready_id(ns_id, (cz, cwx, cwy)) {
                                                if let Some(ch) = ch_opt {
                                                    let pk = TileKey(ns_id, cz, cwx, cwy);
                                                    if !pool.is_gpu_pin_requested(pk) {
                                                        to_pin.push((pk, ch.clone()));
                                                    }
                                                    dimg(frame, r, ch.clone(), l.opacity);
                                                }
                                            } else if let Some((dz, az, awx, awy, ref ah)) = anc_fb
                                                && let Some(clip) = r.intersection(&vp_rect)
                                            {
                                                let ar = trect(
                                                    (tx.div_euclid(n) * (1_i64 << az) + awx as i64, awy as i64),
                                                    px * (1_u32 << dz) as f64,
                                                    false,
                                                );
                                                let pk = TileKey(ns_id, az, awx, awy);
                                                if !pool.is_gpu_pin_requested(pk) {
                                                    to_pin.push((pk, ah.clone()));
                                                }
                                                frame.with_clip(clip, |sub| dimg(sub, ar, ah.clone(), l.opacity));
                                            }
                                        }
                                    }
                                }
                            }
                            if !any_child_settled && !all_children_settled {
                                // Never upscale raster text labels more than 1 zoom level (or >1.35x) — avoids giant blurry place names while zooming!
                                if let Some((dz, az, awx, awy, ah)) = anc(pool, wx, wy, max_dz)
                                    && (!is_labels || zoom - az as f64 <= 1.35)
                                    && let Some(clip) = dst.intersection(&vp_rect)
                                {
                                    let r = trect(
                                        (tx.div_euclid(n) * (1_i64 << az) + awx as i64, awy as i64),
                                        px * (1_u32 << dz) as f64,
                                        false,
                                    );
                                    let alpha = if is_labels { l.opacity * 0.72 } else { l.opacity };
                                    let pk = TileKey(ns_id, az, awx, awy);
                                    if !pool.is_gpu_pin_requested(pk) {
                                        to_pin.push((pk, ah.clone()));
                                    }
                                    frame.with_clip(clip, |sub| dimg(sub, r, ah, alpha));
                                } else if !is_labels
                                    && let Some((fid, th)) = l.temporal_fallbacks.iter().flatten().find_map(|&fid| {
                                        pool.get_ready_id(fid, (tz, wx, wy)).and_then(Option::as_ref).map(|h| (fid, h.clone()))
                                    })
                                {
                                    // Seamless temporal bridge when a single tile hits a transient network error/timeout:
                                    // paint the adjacent timeline frame's tile instead of leaving a sharp rectangular hole!
                                    let pk = TileKey(fid, tz, wx, wy);
                                    if !pool.is_gpu_pin_requested(pk) {
                                        to_pin.push((pk, th.clone()));
                                    }
                                    dimg(frame, dst, th, l.opacity);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        for (k, h) in to_pin.drain(..) {
            pool.queue_pin_after_draw(k, &h);
        }
    });
}
