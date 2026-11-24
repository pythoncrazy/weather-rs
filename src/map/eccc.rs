//! Motion-compensated sub-hourly frames for hourly ECCC GeoMet precipitation-rate rasters.
//!
//! ECCC publishes HRDPS / RDPS / GDPS precipitation rate only once per UTC hour. Cross-dissolving two hourly rasters
//! on the GPU double-exposes every moving rain band (two half-bright copies up to an hour of travel apart), which reads
//! as rhythmic blurring and "breathing" during playback. Instead, every sub-hourly forecast frame is synthesized here
//! on the CPU, in scalar-intensity space, before color-mapping:
//!
//! 1. Both bracketing hourly rasters are downloaded once by the tile pool, with a margin of real neighboring data
//!    ([`EcccModel::gutter_px`]), then reverse-mapped to a scalar intensity field and smoothed ([`ingest_source`]).
//! 2. A dense displacement field between the two hours is estimated by regularized block matching ([`estimate_flow`]).
//! 3. Each quarter-hour frame warps both hours toward its own timestamp along that field and blends the intensities
//!    ([`warp_blend`]), so rain bands travel instead of fading out in one place and fading in at another.
//!
//! Only the hourly source rasters hit the network and the disk cache — as ordinary tile jobs, so every raster is
//! requested once and all of them download in parallel; derived frames are rebuilt from them on demand, off-network.

use super::tiles::{
    AlphaThumb, DecGate, EcccMask, TileCoord, bleed_transparent_edges, build_alpha_thumb, eccc_colorize, eccc_extract_scalar,
    eccc_smooth_scalar,
};
use crate::providers::{EcccModel, RainViewerFrame};
use crate::storage::{delete_cached_tile, load_cached_tile};
use iced::advanced::image as adv_image;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Edge length of the finished tile.
const TILE: usize = 512;
/// Block-matching runs on fields box-downsampled by this factor.
const DS: usize = 8;
/// Spacing of displacement-field nodes, in downsampled pixels.
const NODE: usize = 4;
/// Spacing of displacement-field nodes, in source pixels.
const NODE_PX: usize = DS * NODE;
/// Half-size of the block-matching window, in downsampled pixels (`13×13` cells ≈ `104×104` source px).
const WIN: isize = 6;
/// Fastest storm motion the matcher will consider.
const MAX_SPEED_KMH: f64 = 150.0;
/// Number of interpolated sub-steps per hour (`q` in `0..SUBSTEPS`; `q == 0` is the hourly raster itself).
pub(super) const SUBSTEPS: u8 = 4;

/// A smoothed scalar precipitation-intensity raster (`t * alpha`, `0..=65535`) for one hourly tile plus its margin.
/// The tile itself occupies the `TILE×TILE` block at `(ox, oy)`.
pub(super) struct Field {
    w: usize,
    h: usize,
    ox: usize,
    oy: usize,
    data: Vec<u16>,
}

impl Field {
    /// Extracts and smooths the scalar field of a decoded WMS raster. Returns `None` when it holds no precipitation.
    pub(super) fn from_rgba(model: EcccModel, rgba: &[u8], w: usize, h: usize, ox: usize, oy: usize) -> Option<Self> {
        if w < TILE + ox || h < TILE + oy || rgba.len() != w * h * 4 {
            return None;
        }
        let mut data = vec![0_u16; w * h];
        if !eccc_extract_scalar(rgba, w, h, &mut data) {
            return None;
        }
        let em = match model {
            EcccModel::Hrdps => EcccMask::Hrdps { outside_hrrr: false },
            EcccModel::Rdps => EcccMask::Rdps { outside_hrrr: false },
            EcccModel::Gdps => EcccMask::Gdps { outside_hrrr: false },
        };
        let (mut tmp, mut rows) = (vec![0_u16; w * h], vec![false; h]);
        eccc_smooth_scalar(em, &mut data, &mut tmp, &mut rows, w, h);
        Some(Self { w, h, ox, oy, data })
    }

    /// Bilinear sample at source-pixel position `(x, y)`, clamped to the raster.
    #[inline]
    fn sample(&self, x: f32, y: f32) -> f32 {
        let xc = x.clamp(0.0, (self.w - 1) as f32);
        let yc = y.clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (xc as usize, yc as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (xc - x0 as f32, yc - y0 as f32);
        let (r0, r1) = (y0 * self.w, y1 * self.w);
        let top = self.data[r0 + x0] as f32 * (1.0 - fx) + self.data[r0 + x1] as f32 * fx;
        let bot = self.data[r1 + x0] as f32 * (1.0 - fx) + self.data[r1 + x1] as f32 * fx;
        top * (1.0 - fy) + bot * fy
    }

    /// Box-downsamples by `ds` into a `0..1` float raster. Intensities below the visible drizzle floor are suppressed
    /// so block matching locks onto the rain the user can actually see.
    fn downsample(&self, ds: usize) -> (usize, usize, Vec<f32>) {
        let (cw, ch) = (self.w / ds, self.h / ds);
        let mut out = vec![0.0_f32; cw * ch];
        let norm = 1.0 / ((ds * ds) as f32 * 65535.0);
        for cy in 0..ch {
            for cx in 0..cw {
                let mut s = 0_u32;
                for y in 0..ds {
                    let off = (cy * ds + y) * self.w + cx * ds;
                    s += self.data[off..off + ds].iter().map(|&v| v as u32).sum::<u32>();
                }
                out[cy * cw + cx] = (s as f32 * norm - 0.15).max(0.0);
            }
        }
        (cw, ch, out)
    }
}

/// Dense displacement field between two consecutive hourly rasters: how far (in source pixels) the precipitation at
/// each node travels over the full hour. Nodes sit every [`NODE_PX`] source pixels starting at the raster origin.
pub(super) struct Flow {
    gw: usize,
    gh: usize,
    vx: Vec<f32>,
    vy: Vec<f32>,
}

impl Flow {
    fn zero() -> Self {
        Self { gw: 1, gh: 1, vx: vec![0.0], vy: vec![0.0] }
    }

    /// Bilinearly interpolated displacement at source-pixel position `(x, y)`.
    #[inline]
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        let gx = (x / NODE_PX as f32).clamp(0.0, (self.gw - 1) as f32);
        let gy = (y / NODE_PX as f32).clamp(0.0, (self.gh - 1) as f32);
        let (x0, y0) = (gx as usize, gy as usize);
        let (x1, y1) = ((x0 + 1).min(self.gw - 1), (y0 + 1).min(self.gh - 1));
        let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
        let lerp = |v: &[f32]| {
            let top = v[y0 * self.gw + x0] * (1.0 - fx) + v[y0 * self.gw + x1] * fx;
            let bot = v[y1 * self.gw + x0] * (1.0 - fx) + v[y1 * self.gw + x1] * fx;
            top * (1.0 - fy) + bot * fy
        };
        (lerp(&self.vx), lerp(&self.vy))
    }
}

/// One block-matching pass over every displacement node at downsample factor `ds`.
///
/// Each node compares a `(2·win+1)²` window of `ca` against windows of `cb` shifted by its current displacement
/// ± `search` cells (sum of squared differences, plus a mild penalty `lambda` on straying from the current
/// displacement so ambiguous matches stay put), then refines the best shift to sub-cell precision. Nodes that see
/// rain get a new displacement and a confidence weight (how much rain × how distinct the best match is); nodes that
/// see none keep their displacement at half confidence.
#[allow(clippy::too_many_arguments)]
fn match_pass(
    ca: &[f32], cb: &[f32], (cw, ch): (usize, usize), ds: usize, win: isize, search: isize, lambda: f32, gw: usize, gh: usize,
    vx: &mut [f32], vy: &mut [f32], wt: &mut [f32],
) {
    const E_MIN: f32 = 0.0015;
    const E_REF: f32 = 0.015;
    let (cwi, chi) = (cw as isize, ch as isize);
    let side = (2 * search + 1) as usize;
    let mut scores = vec![f32::INFINITY; side * side];
    let node_cells = (NODE_PX / ds) as isize;
    for ny in 0..gh {
        for nx in 0..gw {
            let i = ny * gw + nx;
            let (cx, cy) = ((nx as isize * node_cells).min(cwi - 1), (ny as isize * node_cells).min(chi - 1));
            let (x0, x1) = ((cx - win).max(0), (cx + win).min(cwi - 1));
            let (y0, y1) = ((cy - win).max(0), (cy + win).min(chi - 1));
            let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f32;
            let (mut ea, mut eb) = (0.0_f32, 0.0_f32);
            for y in y0..=y1 {
                let off = (y * cwi) as usize;
                for x in x0..=x1 {
                    let (va, vb) = (ca[off + x as usize], cb[off + x as usize]);
                    ea += va * va;
                    eb += vb * vb;
                }
            }
            let e_norm = (ea + eb) / (2.0 * area);
            if e_norm < E_MIN || ea <= 0.0 || eb <= 0.0 {
                wt[i] *= 0.5;
                continue;
            }
            let (base_x, base_y) = (vx[i] / ds as f32, vy[i] / ds as f32);
            let (bx0, by0) = (base_x.round() as isize, base_y.round() as isize);
            scores.fill(f32::INFINITY);
            let (mut best, mut best_o, mut sum, mut cnt_valid) = (f32::INFINITY, (0_isize, 0_isize), 0.0_f32, 0_u32);
            for oy in -search..=search {
                let dy = by0 + oy;
                let (py0, py1) = (y0.max(-dy), y1.min(chi - 1 - dy));
                if py1 < py0 {
                    continue;
                }
                for ox in -search..=search {
                    let dx = bx0 + ox;
                    let (px0, px1) = (x0.max(-dx), x1.min(cwi - 1 - dx));
                    if px1 < px0 {
                        continue;
                    }
                    let cnt = ((px1 - px0 + 1) * (py1 - py0 + 1)) as f32;
                    if cnt < area * 0.6 {
                        continue;
                    }
                    let mut ssd = 0.0_f32;
                    for y in py0..=py1 {
                        let (ra, rb) = ((y * cwi) as usize, ((y + dy) * cwi) as usize);
                        let sa = &ca[ra + px0 as usize..=ra + px1 as usize];
                        let sb = &cb[rb + (px0 + dx) as usize..=rb + (px1 + dx) as usize];
                        ssd += sa.iter().zip(sb).map(|(p, q)| (p - q) * (p - q)).sum::<f32>();
                    }
                    let dev = (dx as f32 - base_x).powi(2) + (dy as f32 - base_y).powi(2);
                    let sc = ssd / cnt + lambda * dev * e_norm;
                    scores[((oy + search) as usize) * side + (ox + search) as usize] = sc;
                    sum += sc;
                    cnt_valid += 1;
                    if sc < best {
                        (best, best_o) = (sc, (ox, oy));
                    }
                }
            }
            if cnt_valid == 0 {
                wt[i] *= 0.5;
                continue;
            }
            let sc_at = |ox: isize, oy: isize| -> Option<f32> {
                (ox.abs() <= search && oy.abs() <= search)
                    .then(|| scores[((oy + search) as usize) * side + (ox + search) as usize])
                    .filter(|v| v.is_finite())
            };
            let sub = |lo: Option<f32>, hi: Option<f32>| -> f32 {
                match (lo, hi) {
                    (Some(l), Some(h)) => {
                        let d = l - 2.0 * best + h;
                        if d > 1e-9 { (0.5 * (l - h) / d).clamp(-0.5, 0.5) } else { 0.0 }
                    }
                    _ => 0.0,
                }
            };
            let (ox, oy) = best_o;
            let fx = (bx0 + ox) as f32 + sub(sc_at(ox - 1, oy), sc_at(ox + 1, oy));
            let fy = (by0 + oy) as f32 + sub(sc_at(ox, oy - 1), sc_at(ox, oy + 1));
            let distinct = (1.0 - best / (sum / cnt_valid as f32 + 1e-9)).clamp(0.0, 1.0);
            (vx[i], vy[i], wt[i]) = (fx * ds as f32, fy * ds as f32, (e_norm / E_REF).min(1.0) * (0.15 + 0.85 * distinct));
        }
    }
}

/// Rejects isolated mismatches with a 3×3 component-wise median over confident nodes, then runs `passes` rounds of
/// confidence-weighted 3×3 smoothing. With `fill`, later rounds only propagate motion into featureless nodes.
fn regularize(vx: &mut Vec<f32>, vy: &mut Vec<f32>, wt: &mut Vec<f32>, gw: usize, gh: usize, passes: usize, fill: bool) {
    let n = gw * gh;
    let (mx, my) = (vx.clone(), vy.clone());
    let mut nb: Vec<f32> = Vec::with_capacity(9);
    for ny in 0..gh {
        for nx in 0..gw {
            let i = ny * gw + nx;
            if wt[i] <= 0.0 {
                continue;
            }
            for (src, dst) in [(&mx, &mut *vx), (&my, &mut *vy)] {
                nb.clear();
                for yy in ny.saturating_sub(1)..=(ny + 1).min(gh - 1) {
                    for xx in nx.saturating_sub(1)..=(nx + 1).min(gw - 1) {
                        if wt[yy * gw + xx] > 0.0 {
                            nb.push(src[yy * gw + xx]);
                        }
                    }
                }
                if nb.len() >= 3 {
                    nb.sort_by(f32::total_cmp);
                    dst[i] = nb[nb.len() / 2];
                }
            }
        }
    }
    let (mut tx, mut ty, mut tw) = (vec![0.0_f32; n], vec![0.0_f32; n], vec![0.0_f32; n]);
    for pass in 0..passes {
        for ny in 0..gh {
            for nx in 0..gw {
                let i = ny * gw + nx;
                if fill && pass >= 3 && wt[i] > 0.0 {
                    (tx[i], ty[i], tw[i]) = (vx[i], vy[i], wt[i]);
                    continue;
                }
                let (mut sx, mut sy, mut sw, mut sk) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
                for yy in ny.saturating_sub(1)..=(ny + 1).min(gh - 1) {
                    for xx in nx.saturating_sub(1)..=(nx + 1).min(gw - 1) {
                        let k = if yy == ny { 2.0 } else { 1.0 } * if xx == nx { 2.0 } else { 1.0 };
                        let j = yy * gw + xx;
                        sx += vx[j] * wt[j] * k;
                        sy += vy[j] * wt[j] * k;
                        sw += wt[j] * k;
                        sk += k;
                    }
                }
                (tx[i], ty[i], tw[i]) = if sw > 1e-6 { (sx / sw, sy / sw, sw / sk) } else { (0.0, 0.0, 0.0) };
            }
        }
        std::mem::swap(vx, &mut tx);
        std::mem::swap(vy, &mut ty);
        std::mem::swap(wt, &mut tw);
    }
}

/// Estimates the hour-long displacement field from raster `a` to raster `b` by coarse-to-fine regularized block matching.
///
/// A wide search on 8× downsampled rasters finds the bulk storm motion; two refinement passes at 4× and 2× then lock
/// onto narrow features (squall lines, frontal rain bands) that the coarse pass can only place to within a few pixels.
/// Featureless nodes inherit the motion of their nearest confident neighbors. `max_px` caps the displacement magnitude
/// in source pixels.
pub(super) fn estimate_flow(a: &Field, b: &Field, max_px: f32) -> Flow {
    if a.w != b.w || a.h != b.h || max_px < 1.0 {
        return Flow::zero();
    }
    let (gw, gh) = (a.w.div_ceil(NODE_PX) + 1, a.h.div_ceil(NODE_PX) + 1);
    let n = gw * gh;
    let (mut vx, mut vy, mut wt) = (vec![0.0_f32; n], vec![0.0_f32; n], vec![0.0_f32; n]);
    let (cw, ch, ca) = a.downsample(DS);
    let (_, _, cb) = b.downsample(DS);
    let search = ((max_px / DS as f32).ceil() as isize).clamp(1, 12);
    match_pass(&ca, &cb, (cw, ch), DS, WIN, search, 0.0025, gw, gh, &mut vx, &mut vy, &mut wt);
    if wt.iter().all(|&w| w <= 0.0) {
        return Flow::zero();
    }
    regularize(&mut vx, &mut vy, &mut wt, gw, gh, 8, true);
    for (ds, win) in [(4_usize, 6_isize), (2, 8)] {
        let (cw, ch, ca) = a.downsample(ds);
        let (_, _, cb) = b.downsample(ds);
        match_pass(&ca, &cb, (cw, ch), ds, win, 2, 0.02, gw, gh, &mut vx, &mut vy, &mut wt);
        regularize(&mut vx, &mut vy, &mut wt, gw, gh, 1, false);
    }
    for (x, y) in vx.iter_mut().zip(vy.iter_mut()) {
        let mag = x.hypot(*y);
        if mag > max_px {
            (*x, *y) = (*x * max_px / mag, *y * max_px / mag);
        }
    }
    Flow { gw, gh, vx, vy }
}

/// Where an output tile sits inside its source raster: output pixel `p` reads source position `origin + p · scale`.
#[derive(Clone, Copy)]
pub(super) struct View {
    x0: f32,
    y0: f32,
    scale: f32,
}

impl View {
    /// The output tile is the source raster's own `TILE×TILE` block (after its `(ox, oy)` margin), pixel for pixel.
    const fn same(ox: usize, oy: usize) -> Self {
        Self { x0: ox as f32, y0: oy as f32, scale: 1.0 }
    }
    /// The output tile is quadrant `(qx, qy)` of the source raster's block, upsampled 2× (pixel centers aligned).
    const fn quadrant(ox: usize, oy: usize, qx: u32, qy: u32) -> Self {
        let half = (TILE / 2) as f32;
        Self { x0: ox as f32 + qx as f32 * half - 0.25, y0: oy as f32 + qy as f32 * half - 0.25, scale: 0.5 }
    }
}

/// Source raster used to build output tile `coord` of `model`, plus the quadrant of it the tile covers (if any).
///
/// HRDPS (2.5 km) tiles are built from a raster of the same tile. RDPS (10 km) and GDPS (15 km) are coarser than a
/// tile's pixels even one zoom level up, so four sibling tiles share ONE parent-zoom raster: a quarter of the WMS
/// requests and bytes, one flow field instead of four, and no loss of real model detail — contours stay crisp because
/// the 2× upsampling happens on the smooth scalar field before color-mapping.
fn source_tile(model: EcccModel, (z, x, y): TileCoord) -> (TileCoord, Option<(u32, u32)>) {
    match model {
        EcccModel::Rdps | EcccModel::Gdps if z >= 2 => ((z - 1, x >> 1, y >> 1), Some((x & 1, y & 1))),
        _ => ((z, x, y), None),
    }
}

/// Synthesizes the `TILE×TILE` scalar field seen through `view` at fraction `f` (`0..1`) of the way from hourly raster
/// `a` to `b`: both rasters are warped toward the intermediate time along `flow` and their intensities blended. A
/// missing (precipitation-free) raster contributes zero, so rain simply decays into / grows out of dry hours.
pub(super) fn warp_blend(a: Option<&Field>, b: Option<&Field>, flow: &Flow, f: f32, view: View, out: &mut [u16]) {
    if a.is_none() && b.is_none() {
        out.fill(0);
        return;
    }
    for py in 0..TILE {
        let y = view.y0 + py as f32 * view.scale;
        let row = &mut out[py * TILE..(py + 1) * TILE];
        for (px, dst) in row.iter_mut().enumerate() {
            let x = view.x0 + px as f32 * view.scale;
            let (vx, vy) = flow.at(x, y);
            let va = a.map_or(0.0, |r| r.sample(x - f * vx, y - f * vy));
            let vb = b.map_or(0.0, |r| r.sample(x + (1.0 - f) * vx, y + (1.0 - f) * vy));
            *dst = (va * (1.0 - f) + vb * f).round().clamp(0.0, 65535.0) as u16;
        }
    }
}

/// Largest plausible hour-long displacement (source pixels) for tile `(z, _, y)`, capped so warped samples stay inside
/// the margin of real data.
fn max_flow_px((z, _, y): TileCoord, gutter: u32) -> f32 {
    let n = (1_u64 << z) as f64;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * (y as f64 + 0.5) / n)).sinh().atan();
    let km_per_px = 40_075.0 * lat.cos() / (n * TILE as f64);
    ((MAX_SPEED_KMH / km_per_px.max(1e-6)) as f32).min(gutter as f32 / 0.75)
}

/// Color-maps a finished `TILE×TILE` scalar field into a GPU tile + optical-flow thumbnail (`(None, None)` if empty).
fn finish(field: &[u16], em: EcccMask, coord: TileCoord) -> (Option<adv_image::Handle>, Option<AlphaThumb>) {
    let mut rgba = vec![0_u8; TILE * TILE * 4];
    if !eccc_colorize(field, em, coord, &mut rgba, TILE, TILE) {
        return (None, None);
    }
    let thumb = build_alpha_thumb(&rgba, TILE as u32, TILE as u32);
    bleed_transparent_edges(&mut rgba, TILE as u32, TILE as u32);
    (Some(adv_image::Handle::from_rgba(TILE as u32, TILE as u32, rgba)), Some(thumb))
}

/// Bounded first-in-first-out memo (hourly sources and flow fields are needed by several neighboring frames).
struct Fifo<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}
impl<K: std::hash::Hash + Eq + Copy, V: Clone> Fifo<K, V> {
    fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), cap }
    }
    fn get(&self, k: &K) -> Option<V> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: K, v: V) {
        if self.map.insert(k, v).is_none() {
            self.order.push_back(k);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

/// `(kind, tier, UTC hour, z, x, y)`; `kind` separates hourly sources (`0`) from flow fields (`1`).
type MemoKey = (u8, u8, i64, u8, u32, u32);
static SOURCES: LazyLock<Mutex<Fifo<MemoKey, Option<Arc<Field>>>>> = LazyLock::new(|| Mutex::new(Fifo::new(72)));
static FLOWS: LazyLock<Mutex<Fifo<MemoKey, Arc<Flow>>>> = LazyLock::new(|| Mutex::new(Fifo::new(256)));
/// Per-key single-flight locks so concurrent frames needing the same hourly raster (or flow) fetch / compute it once.
static KEY_LOCKS: LazyLock<Mutex<HashMap<MemoKey, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn key_lock(k: MemoKey) -> Arc<Mutex<()>> {
    let mut g = KEY_LOCKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if g.len() > 4096 {
        g.retain(|_, v| Arc::strong_count(v) > 1);
    }
    Arc::clone(g.entry(k).or_default())
}

const fn tier(model: EcccModel) -> u8 {
    match model {
        EcccModel::Hrdps => 0,
        EcccModel::Rdps => 1,
        EcccModel::Gdps => 2,
    }
}

/// One hourly WMS raster a tile build is waiting for. The tile pool downloads it as an ordinary (deduplicated,
/// prioritized, disk-cached) tile job and re-runs the build once it has arrived.
#[derive(Debug, Clone)]
pub(super) struct SourceReq {
    pub ns: String,
    pub coord: TileCoord,
    pub url: String,
}

/// Tile namespace under which `model`'s hourly raster for UTC hour `hour` is fetched and cached.
fn source_ns(model: EcccModel, hour: i64) -> String {
    format!("eccc_{}_src_g{}_{hour}", model.tag(), model.gutter_px())
}

/// True if `ns` names hourly source rasters (see [`source_ns`]) rather than displayable tiles.
#[inline]
pub(super) fn is_source_ns(ns: &str) -> bool {
    ns.starts_with("eccc_") && ns.contains("_src_g")
}

fn parse_source_ns(ns: &str) -> Option<(EcccModel, i64)> {
    let mut it = ns.strip_prefix("eccc_")?.split('_');
    let model = model_from_tag(it.next()?)?;
    (it.next()? == "src").then_some(())?;
    Some((model, it.nth(1)?.parse().ok()?))
}

fn model_from_tag(tag: &str) -> Option<EcccModel> {
    EcccModel::ALL.into_iter().find(|m| m.tag() == tag)
}

/// Decodes a downloaded (or disk-cached) hourly raster and keeps its smoothed scalar field in memory for the tile
/// builds that need it. Returns `Some(has_precipitation)`, or `None` if `bytes` is not a valid raster for this tile
/// (e.g. a WMS service-exception document), in which case nothing is stored.
pub(super) fn ingest_source(gate: &DecGate, ns: &str, coord: TileCoord, bytes: &[u8]) -> Option<bool> {
    let (model, hour) = parse_source_ns(ns)?;
    let field = decode_source(gate, model, hour, coord, bytes)?;
    if let Ok(mut g) = SOURCES.lock() {
        g.put((0, tier(model), hour, coord.0, coord.1, coord.2), field.clone());
    }
    Some(field.is_some())
}

/// `Some(None)` = valid raster without precipitation (stored on disk as a 1-byte sentinel).
fn decode_source(gate: &DecGate, model: EcccModel, hour: i64, (z, x, y): TileCoord, bytes: &[u8]) -> Option<Option<Arc<Field>>> {
    if bytes == [0] {
        return Some(None);
    }
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let (_, [gl, gt, pw, ph]) = RainViewerFrame::eccc_wms_request(model, hour, z, x, y, model.gutter_px())?;
    let _permit = gate.enter();
    let img = ::image::load_from_memory_with_format(bytes, ::image::ImageFormat::WebP).ok()?.into_rgba8();
    (img.dimensions() == (pw, ph))
        .then(|| Field::from_rgba(model, img.as_raw(), pw as usize, ph as usize, gl as usize, gt as usize).map(Arc::new))
}

/// Smoothed scalar field of `model`'s hourly raster for `coord` at UTC hour `hour`, from memory or the disk cache.
/// `Ok(None)` means the raster holds no precipitation; `Err` carries the download the caller must wait for.
fn source(gate: &DecGate, ttl: Duration, model: EcccModel, hour: i64, coord: TileCoord) -> Result<Option<Arc<Field>>, SourceReq> {
    let (z, x, y) = coord;
    let key: MemoKey = (0, tier(model), hour, z, x, y);
    let cached = |k: &MemoKey| SOURCES.lock().ok().and_then(|g| g.get(k));
    if let Some(f) = cached(&key) {
        return Ok(f);
    }
    // Single-flight: concurrent builds needing the same raster decode it from disk once.
    let lock = key_lock(key);
    let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(f) = cached(&key) {
        return Ok(f);
    }
    let ns = source_ns(model, hour);
    if let Some(bytes) = load_cached_tile(&ns, z, x, y, ttl) {
        if let Some(field) = decode_source(gate, model, hour, coord, &bytes) {
            if let Ok(mut g) = SOURCES.lock() {
                g.put(key, field.clone());
            }
            return Ok(field);
        }
        // A corrupt cache entry must never pin this tile to failure: drop it and download again.
        delete_cached_tile(&ns, z, x, y);
    }
    let url = RainViewerFrame::eccc_wms_request(model, hour, z, x, y, model.gutter_px()).map_or_else(String::new, |r| r.0);
    Err(SourceReq { ns, coord, url })
}

/// Memoized hour-long displacement field between `a` (UTC hour `h0`) and `b` (`h0 + 1h`) for `coord`.
fn flow_between(gate: &DecGate, model: EcccModel, h0: i64, coord: TileCoord, a: &Field, b: &Field) -> Arc<Flow> {
    let key: MemoKey = (1, tier(model), h0, coord.0, coord.1, coord.2);
    let cached = |k: &MemoKey| FLOWS.lock().ok().and_then(|g| g.get(k));
    if let Some(f) = cached(&key) {
        return f;
    }
    let lock = key_lock(key);
    let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(f) = cached(&key) {
        return f;
    }
    let flow = {
        let _permit = gate.enter();
        Arc::new(estimate_flow(a, b, max_flow_px(coord, model.gutter_px())))
    };
    if let Ok(mut g) = FLOWS.lock() {
        g.put(key, Arc::clone(&flow));
    }
    flow
}

/// Job descriptor (carried in the tile job's URL slot) for ECCC frame `q / SUBSTEPS` of the way from UTC hour `h0`
/// to `h0 + 1h`.
pub(super) fn job_spec(model: EcccModel, h0: i64, q: u8) -> String {
    format!("eccc-morph:{}:{h0}:{q}", model.tag())
}

/// True if a tile job's URL slot holds an [`job_spec`] descriptor rather than a real URL.
#[inline]
pub(super) fn is_job_spec(url: &str) -> bool {
    url.starts_with("eccc-morph:")
}

fn parse_spec(spec: &str) -> Option<(EcccModel, i64, u8)> {
    let mut it = spec.strip_prefix("eccc-morph:")?.split(':');
    let model = model_from_tag(it.next()?)?;
    let (h0, q) = (it.next()?.parse().ok()?, it.next()?.parse::<u8>().ok()?);
    (q < SUBSTEPS).then_some((model, h0, q))
}

pub(super) enum Built {
    /// Finished tile (`None` image = no visible precipitation in this tile).
    Done(Option<adv_image::Handle>, Option<AlphaThumb>),
    /// Hourly rasters that must be downloaded before this tile can be built.
    Need(Vec<SourceReq>),
    Failed,
}

/// Builds the ECCC precipitation tile `coord` of namespace `ns` described by `spec` (see [`job_spec`]) from hourly
/// rasters already in memory or on disk. Never touches the network: missing rasters are reported via [`Built::Need`].
pub(super) fn build(gate: &DecGate, ttl: Duration, ns: &str, spec: &str, coord: TileCoord) -> Built {
    let (Some(em), Some((model, h0, q))) = (EcccMask::from_ns(ns), parse_spec(spec)) else { return Built::Failed };
    let (src, quadrant) = source_tile(model, coord);
    let a = source(gate, ttl, model, h0, src);
    let b = if q == 0 { Ok(None) } else { source(gate, ttl, model, h0 + 3600, src) };
    let (a, b) = match (a, b) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            let need: Vec<SourceReq> = [a.err(), b.err()].into_iter().flatten().collect();
            return if need.iter().any(|r| r.url.is_empty()) { Built::Failed } else { Built::Need(need) };
        }
    };
    let Some((ox, oy)) = a.as_deref().or(b.as_deref()).map(|r| (r.ox, r.oy)) else { return Built::Done(None, None) };
    let flow = match (&a, &b) {
        (Some(fa), Some(fb)) => flow_between(gate, model, h0, src, fa, fb),
        _ => Arc::new(Flow::zero()),
    };
    let view = quadrant.map_or(View::same(ox, oy), |(qx, qy)| View::quadrant(ox, oy, qx, qy));
    let _permit = gate.enter();
    let mut out = vec![0_u16; TILE * TILE];
    warp_blend(a.as_deref(), b.as_deref(), &flow, q as f32 / SUBSTEPS as f32, view, &mut out);
    let (h, th) = finish(&out, em, coord);
    Built::Done(h, th)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Margin used by the synthetic test rasters.
    const GUTTER: usize = 64;
    const VIEW: View = View::same(GUTTER, GUTTER);

    /// Synthetic field: one Gaussian rain cell centered at source pixel `(cx, cy)`.
    fn blob(cx: f32, cy: f32, sigma: f32, peak: f32) -> Field {
        let w = TILE + 2 * GUTTER;
        let mut data = vec![0_u16; w * w];
        for y in 0..w {
            for x in 0..w {
                let d2 = (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2);
                data[y * w + x] = (peak * 65535.0 * (-d2 / (2.0 * sigma * sigma)).exp()) as u16;
            }
        }
        Field { w, h: w, ox: GUTTER, oy: GUTTER, data }
    }

    /// Intensity-weighted centroid `(x, y)` and peak of a `TILE×TILE` field, in tile pixels.
    fn centroid(f: &[u16]) -> (f32, f32, f32) {
        let (mut sx, mut sy, mut sw, mut peak) = (0.0_f64, 0.0_f64, 0.0_f64, 0_u16);
        for y in 0..TILE {
            for x in 0..TILE {
                let v = f[y * TILE + x];
                sx += x as f64 * v as f64;
                sy += y as f64 * v as f64;
                sw += v as f64;
                peak = peak.max(v);
            }
        }
        ((sx / sw) as f32, (sy / sw) as f32, peak as f32 / 65535.0)
    }

    #[test]
    fn moving_cell_travels_instead_of_double_exposing() {
        // A rain cell moves 40 px east and 24 px south over the hour.
        let (a, b) = (blob(300.0, 280.0, 26.0, 0.8), blob(340.0, 304.0, 26.0, 0.8));
        let flow = estimate_flow(&a, &b, 80.0);
        let (vx, vy) = flow.at(320.0, 292.0);
        assert!((vx - 40.0).abs() < 4.0 && (vy - 24.0).abs() < 4.0, "flow at cell = ({vx:.1}, {vy:.1}), expected ~(40, 24)");
        let mut mid = vec![0_u16; TILE * TILE];
        warp_blend(Some(&a), Some(&b), &flow, 0.5, VIEW, &mut mid);
        let (cx, cy, peak) = centroid(&mid);
        // Halfway through the hour the cell sits halfway along its track (tile px = source px - GUTTER) ...
        assert!((cx - (320.0 - GUTTER as f32)).abs() < 3.0 && (cy - (292.0 - GUTTER as f32)).abs() < 3.0, "centroid ({cx:.1}, {cy:.1})");
        // ... at full strength: a plain cross-dissolve of two cells 47 px apart would peak near 0.52 instead of 0.8.
        assert!(peak > 0.72, "mid-hour peak {peak:.2} collapsed like a double exposure");
        // Without motion compensation the same blend really does collapse.
        let mut naive = vec![0_u16; TILE * TILE];
        warp_blend(Some(&a), Some(&b), &Flow::zero(), 0.5, VIEW, &mut naive);
        assert!(centroid(&naive).2 < 0.6);
    }

    #[test]
    fn stationary_rain_stays_put_and_endpoints_are_exact() {
        let a = blob(320.0, 320.0, 30.0, 0.7);
        let flow = estimate_flow(&a, &a, 80.0);
        let (vx, vy) = flow.at(320.0, 320.0);
        assert!(vx.abs() < 1.0 && vy.abs() < 1.0, "stationary rain drifted by ({vx:.2}, {vy:.2})");
        // Frames at the very start of the hour reproduce the first raster regardless of the flow field.
        let b = blob(360.0, 320.0, 30.0, 0.7);
        let moving = estimate_flow(&a, &b, 80.0);
        let mut out = vec![0_u16; TILE * TILE];
        warp_blend(Some(&a), None, &moving, 0.0, VIEW, &mut out);
        let (cx, cy, peak) = centroid(&out);
        assert!((cx - (320.0 - GUTTER as f32)).abs() < 0.5 && (cy - (320.0 - GUTTER as f32)).abs() < 0.5 && (peak - 0.7).abs() < 0.01);
    }

    #[test]
    fn rain_fades_in_from_a_dry_hour() {
        let b = blob(320.0, 320.0, 30.0, 0.8);
        let mut out = vec![0_u16; TILE * TILE];
        warp_blend(None, Some(&b), &Flow::zero(), 0.25, VIEW, &mut out);
        let (cx, _, peak) = centroid(&out);
        assert!((cx - (320.0 - GUTTER as f32)).abs() < 0.5 && (peak - 0.2).abs() < 0.01, "peak {peak:.3}");
    }

    #[test]
    fn sibling_tiles_share_one_parent_raster_and_line_up() {
        // RDPS / GDPS tiles come from the parent-zoom raster; HRDPS keeps its own.
        assert!(matches!(source_tile(EcccModel::Rdps, (4, 5, 9)), ((3, 2, 4), Some((1, 1)))));
        assert!(matches!(source_tile(EcccModel::Gdps, (3, 2, 2)), ((2, 1, 1), Some((0, 0)))));
        assert!(matches!(source_tile(EcccModel::Hrdps, (5, 4, 11)), ((5, 4, 11), None)));
        // A cell centered on the parent block's center (source px 320 = block px 256) lands on the shared corner of the
        // four children: bottom-right pixel of quadrant (0,0), top-left pixel of quadrant (1,1), with matching values.
        let src = blob(320.0, 320.0, 40.0, 0.8);
        let (mut q00, mut q11) = (vec![0_u16; TILE * TILE], vec![0_u16; TILE * TILE]);
        warp_blend(Some(&src), None, &Flow::zero(), 0.0, View::quadrant(GUTTER, GUTTER, 0, 0), &mut q00);
        warp_blend(Some(&src), None, &Flow::zero(), 0.0, View::quadrant(GUTTER, GUTTER, 1, 1), &mut q11);
        let (corner_a, corner_b) = (q00[TILE * TILE - 1] as f32 / 65535.0, q11[0] as f32 / 65535.0);
        assert!(corner_a > 0.79 && (corner_a - corner_b).abs() < 0.002, "seam: {corner_a:.4} vs {corner_b:.4}");
        // The cell is twice as wide in child pixels: one sigma (40 source px = 80 child px) left of the corner it has
        // decayed to exp(-0.5) of the peak.
        let v = q00[(TILE - 1) * TILE + (TILE - 1 - 80)] as f32 / 65535.0;
        assert!((v - 0.8 * (-0.5_f32).exp()).abs() < 0.01, "upsampled profile {v:.3}");
    }

    #[test]
    fn job_spec_round_trips_and_rejects_garbage() {
        assert!(matches!(parse_spec(&job_spec(EcccModel::Rdps, 1_795_510_800, 3)), Some((EcccModel::Rdps, 1_795_510_800, 3))));
        assert!(parse_spec("eccc-morph:rdps:1795510800:4").is_none());
        assert!(parse_spec("https://geo.weather.gc.ca/geomet").is_none());
        assert!(is_job_spec(&job_spec(EcccModel::Gdps, 0, 0)) && !is_job_spec("https://example.org/x.png"));
    }

    /// Offline visual prototype: `WEATHER_RS_MORPH_DIR` must hold `a.webp` / `b.webp` (two consecutive hourly 640×640
    /// RDPS rasters for tile `4/2/5`); writes `morph_{0..4}.png`.
    #[test]
    #[ignore = "visual prototype: needs WEATHER_RS_MORPH_DIR with a.webp + b.webp"]
    fn morph_real_tiles() {
        let Ok(dir) = std::env::var("WEATHER_RS_MORPH_DIR") else { return };
        let dir = std::path::PathBuf::from(dir);
        let load = |n: &str| {
            let img = ::image::open(dir.join(n)).expect("raster").into_rgba8();
            let (w, h) = img.dimensions();
            Field::from_rgba(EcccModel::Rdps, img.as_raw(), w as usize, h as usize, GUTTER, GUTTER).expect("precipitation")
        };
        let (a, b) = (load("a.webp"), load("b.webp"));
        let tc: Vec<u32> = std::env::var("WEATHER_RS_MORPH_TILE")
            .map_or_else(|_| vec![4, 2, 5], |v| v.split(',').filter_map(|p| p.parse().ok()).collect());
        let coord: TileCoord = (tc[0] as u8, tc[1], tc[2]);
        let t0 = std::time::Instant::now();
        let flow = estimate_flow(&a, &b, max_flow_px(coord, GUTTER as u32));
        println!("flow: {:?} ({}x{} nodes, cap {:.0} px)", t0.elapsed(), flow.gw, flow.gh, max_flow_px(coord, GUTTER as u32));
        for ny in (0..flow.gh).step_by(2) {
            let row: Vec<String> = (0..flow.gw)
                .step_by(2)
                .map(|nx| format!("{:>4.0},{:<4.0}", flow.vx[ny * flow.gw + nx], flow.vy[ny * flow.gw + nx]))
                .collect();
            println!("F {}", row.join(" "));
        }
        // Prediction residual: how much of the hour-to-hour change remains after advecting `a` along the flow.
        let resid = |fl: &Flow| -> f64 {
            let mut pred = vec![0_u16; TILE * TILE];
            warp_blend(Some(&a), None, fl, 1.0, VIEW, &mut pred);
            let mut fwd = vec![0_u16; TILE * TILE];
            // `f = 1` of `a` alone is zero-weighted, so evaluate the advected `a` directly instead.
            for py in 0..TILE {
                for px in 0..TILE {
                    let (x, y) = ((px + a.ox) as f32, (py + a.oy) as f32);
                    let (vx, vy) = fl.at(x, y);
                    fwd[py * TILE + px] = a.sample(x - vx, y - vy) as u16;
                }
            }
            let mut se = 0.0_f64;
            for py in 0..TILE {
                for px in 0..TILE {
                    let t = b.data[(py + b.oy) * b.w + px + b.ox] as f64 - fwd[py * TILE + px] as f64;
                    se += t * t;
                }
            }
            (se / (TILE * TILE) as f64).sqrt() / 65535.0
        };
        let (r_flow, r_none) = (resid(&flow), resid(&Flow::zero()));
        println!("residual rms: advected {r_flow:.4} vs static {r_none:.4} ({:.0}% of static)", 100.0 * r_flow / r_none.max(1e-9));
        for (name, fl) in [("morph", &flow), ("naive", &Flow::zero())] {
            for q in 0..=4_u8 {
                let mut out = vec![0_u16; TILE * TILE];
                let t0 = std::time::Instant::now();
                warp_blend(Some(&a), Some(&b), fl, q as f32 / 4.0, VIEW, &mut out);
                let dt = t0.elapsed();
                let mut rgba = vec![0_u8; TILE * TILE * 4];
                eccc_colorize(&out, EcccMask::Legacy { outside_hrrr: false }, coord, &mut rgba, TILE, TILE);
                ::image::RgbaImage::from_raw(TILE as u32, TILE as u32, rgba)
                    .expect("img")
                    .save(dir.join(format!("{name}_{q}.png")))
                    .expect("save");
                println!("{name} q={q}: warp {dt:?}");
            }
        }
    }
}
