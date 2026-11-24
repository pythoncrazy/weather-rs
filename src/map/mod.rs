mod eccc;
mod hud;
mod overlays;
mod tiles;

pub use overlays::{lat_lon_to_world_px, world_px_to_lat_lon};
pub use tiles::TileKey;

use crate::icons::wind_to_color;
use crate::model::{AlertSeverity, Location, Settings, TempUnit, WeatherBundle};
use crate::providers::{
    EcccModel, GridWeatherPoint, RadarFrameSource, RainViewerFrame, RainViewerManifest, SatelliteFrame, WeatherClient, wind_uv,
};
use crate::storage::is_shutting_down;
use crate::style::{fw, rgb8, rgba8};
use iced::advanced::image as adv_image;
use iced::font::Weight;
use iced::keyboard::{self, Key, key::Named};
use iced::mouse;
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path, Stroke, Text as CanvasText};
use iced::{Alignment, Color, Point, Rectangle, Renderer, Size, Theme};
use overlays::{
    CityBadge, IsobarCache, ScalarCache, WIND_TRAIL_LEN, WindParticle, compute_overview_camera, declutter_points, init_wind_particles,
    project_lat_lon_to_screen, project_screen_to_lat_lon, quantize_coord, scale_bar_spec, wrap_lon,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tiles::{
    MapVp, PRIO_MAX, SharedTilePool, TileLayer, TileLayerKind, draw_tile_layer, draw_tile_layer_filtered, effective_tile_zoom,
    effective_tile_zoom_biased, tile_coord_at,
};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseMapStyle {
    CleanLight,
    AppleDark,
    Satellite,
    Terrain,
    Streets,
}
impl BaseMapStyle {
    pub const ALL: [Self; 5] = [Self::CleanLight, Self::AppleDark, Self::Satellite, Self::Terrain, Self::Streets];
    pub fn spec(self) -> (&'static str, u8, &'static str, &'static str) {
        [
            ("Standard Light", 16, "esri_light_base", "Canvas/World_Light_Gray_Base"),
            ("Dark Canvas", 16, "esri_dark_base", "Canvas/World_Dark_Gray_Base"),
            ("Satellite", 17, "esri_world_imagery", "World_Imagery"),
            ("Topographic", 17, "esri_world_topo", "World_Topo_Map"),
            ("OpenStreetMap", 18, "osm_standard", ""),
        ][self as usize]
    }
    pub fn tile_url(self, z: u8, x: u32, y: u32) -> String {
        let p = self.spec().3;
        if p.is_empty() { format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png") } else { Self::arcgis(p, z, x, y) }
    }
    pub fn arcgis(svc: &str, z: u8, x: u32, y: u32) -> String {
        format!("https://services.arcgisonline.com/ArcGIS/rest/services/{svc}/MapServer/tile/{z}/{y}/{x}")
    }
    pub fn label_overlay(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::CleanLight => Some(("esri_light_labels", "Canvas/World_Light_Gray_Reference")),
            Self::AppleDark => Some(("esri_dark_labels", "Canvas/World_Dark_Gray_Reference")),
            Self::Satellite => Some(("esri_places_labels", "Reference/World_Boundaries_and_Places")),
            _ => None,
        }
    }
    pub fn label_tile_url(svc: &str, z: u8, x: u32, y: u32) -> String {
        Self::arcgis(svc, z, x, y)
    }
    pub fn is_dark_hud(self, ov: WeatherOverlay) -> bool {
        matches!(ov, WeatherOverlay::WindFlow | WeatherOverlay::InfraredClouds) || matches!(self, Self::AppleDark | Self::Satellite)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WeatherOverlay {
    RadarTimeline,
    Temperature,
    AirQuality,
    WindFlow,
    InfraredClouds,
    Pressure,
    NexradUsRadar,
}
impl WeatherOverlay {
    pub const ALL: [Self; 7] = [
        Self::RadarTimeline,
        Self::Temperature,
        Self::AirQuality,
        Self::WindFlow,
        Self::InfraredClouds,
        Self::Pressure,
        Self::NexradUsRadar,
    ];
    pub fn label(self) -> &'static str {
        ["Precipitation", "Temperature", "Air Quality", "Wind", "Cloud Satellite", "Pressure", "US NEXRAD HD"][self as usize]
    }
    fn has_timeline(self) -> bool {
        matches!(self, Self::RadarTimeline | Self::Temperature | Self::WindFlow | Self::Pressure | Self::InfraredClouds)
    }
}

#[derive(Debug, Clone)]
pub enum MapMsg {
    ExpandMap,
    CloseMap,
    ToggleFullScreen,
    ToggleLayerMenu,
    ToggleLegend,
    ToggleTerminator,
    SetOverlayOpacity(f32),
    SelectOverlay(WeatherOverlay),
    SelectBaseStyle(BaseMapStyle),
    PanStart,
    PanPixels(f32, f32),
    PanFling(f32, f32),
    WheelZoom(Point, Size, f64),
    DragZoom(Point, Size, f64),
    DoubleClickZoom(Point, Size),
    ZoomStep(f64),
    RecenterActive,
    FitAllLocations,
    ToggleRadarPlay,
    CyclePlaybackSpeed,
    TogglePrecipRange,
    StepTimeline(i32),
    ScrubRadar(usize),
    ScrubRadarPos(f32),
    JumpToLive,
    ClickMap(Point, Size),
    OpenContextMenu(Point, Size),
    CloseContextMenu,
    ContextInspect,
    ContextCenter,
    ContextZoom(f64),
    ContextCopyCoords,
    CloseInspector,
    SaveInspectedLocation,
    HudConsume,
}
pub enum MapExternalAction {
    SetExpanded(bool),
    SelectSaved(String),
    SaveLocation(Location),
    CopyText(String),
}
enum WorkerEvent {
    ManifestLoaded(RainViewerManifest),
    GridSamples(Vec<(i32, i32)>, Vec<GridWeatherPoint>),
    AqiSamples(Vec<(i32, i32)>, Vec<(f64, f64, f64)>),
    PointInspected(u64, Result<Box<WeatherBundle>, String>),
}

/// Mutually-exclusive camera motion state — prevents stale velocity/flight leaks by construction.
#[derive(Default)]
enum CamMotion {
    #[default]
    Idle,
    Fly {
        from: (f64, f64, f64),
        to: (f64, f64, f64),
        arc_dip: f64,
        t: f32,
        dur: f32,
    },
    Zoom(f64, (f64, f64), Point, Size),
    Fling(f64, f64),
}

struct SampleCache<V> {
    data: HashMap<(i32, i32), V>,
    in_flight: HashSet<(i32, i32)>,
}
impl<V> Default for SampleCache<V> {
    fn default() -> Self {
        Self { data: HashMap::new(), in_flight: HashSet::new() }
    }
}
impl<V> SampleCache<V> {
    fn ingest(&mut self, keys: Vec<(i32, i32)>, items: Vec<V>, coord: impl Fn(&V) -> (f64, f64), clat: f64, clon: f64) {
        for q in keys {
            self.in_flight.remove(&q);
        }
        for item in items {
            let (la, lo) = coord(&item);
            let q = quantize_coord(la, lo);
            self.in_flight.remove(&q);
            self.data.insert(q, item);
        }
        if self.data.len() > 384 {
            self.data.retain(|_, v| {
                let (la, lo) = coord(v);
                (la - clat).hypot(wrap_lon(lo - clon)) < 60.0
            });
        }
    }
    fn claim(&mut self, q: (i32, i32)) -> bool {
        !self.data.contains_key(&q) && self.in_flight.insert(q)
    }
}

#[derive(Default)]
pub(super) struct RadarState {
    pub playing: bool,
    pub frame_idx: usize,
    pub play_pos: f32,
    pub target_pos: f32,
    pub loop_hold: f32,
    pub stall_secs: f32,
    pub buffering: bool,
    pub skip_stall_idx: Option<usize>,
}
impl RadarState {
    fn snap(&mut self, idx: usize) {
        (self.frame_idx, self.play_pos, self.target_pos, self.stall_secs, self.buffering, self.skip_stall_idx) =
            (idx, idx as f32, idx as f32, 0.0, false, None);
    }
    fn snap_pos(&mut self, pos: f32, max_i: usize) {
        let c = pos.clamp(0.0, max_i as f32);
        (self.frame_idx, self.play_pos, self.target_pos, self.stall_secs, self.buffering, self.skip_stall_idx) =
            ((c.round() as usize).min(max_i), c, c, 0.0, false, None);
    }
}

#[derive(Clone, Debug)]
pub(super) struct TimelineSpec {
    pub times: Vec<i64>,
    pub live_idx: usize,
    pub past_share: f32,
}
impl TimelineSpec {
    pub fn max_idx(&self) -> usize {
        self.times.len().saturating_sub(1)
    }
    pub fn live_ts(&self) -> i64 {
        self.times[self.live_idx]
    }
    pub fn ts_at_pos(&self, pos: f32) -> i64 {
        let mx = self.max_idx();
        let p = pos.clamp(0.0, mx as f32);
        let i0 = (p.floor() as usize).min(mx);
        let i1 = (i0 + 1).min(mx);
        let (t0, t1) = (self.times[i0], self.times[i1]);
        (t0 as f64 + (t1 - t0) as f64 * (p - i0 as f32).clamp(0.0, 1.0) as f64).round() as i64
    }
    pub fn pos_for_ts(&self, ts: i64) -> f32 {
        let mx = self.max_idx();
        if mx == 0 || ts <= self.times[0] {
            return 0.0;
        }
        if ts >= self.times[mx] {
            return mx as f32;
        }
        let i = self.times.partition_point(|&t| t < ts).clamp(1, mx);
        let (t0, t1) = (self.times[i - 1], self.times[i]);
        (i - 1) as f32 + ((ts - t0) as f32 / (t1 - t0).max(1) as f32).clamp(0.0, 1.0)
    }
    pub fn time_to_u(&self, ts: i64) -> f32 {
        let (t_min, t_live, t_max) = (self.times[0], self.times[self.live_idx], *self.times.last().unwrap_or(&self.times[0]));
        if self.past_share > 0.0 && t_live > t_min && ts <= t_live {
            self.past_share * ((ts - t_min) as f32 / (t_live - t_min) as f32).clamp(0.0, 1.0)
        } else if t_max > t_live {
            self.past_share + (1.0 - self.past_share) * ((ts - t_live) as f32 / (t_max - t_live) as f32).clamp(0.0, 1.0)
        } else {
            ((ts - t_min) as f32 / (t_max - t_min).max(1) as f32).clamp(0.0, 1.0)
        }
    }
    pub fn u_to_ts(&self, u: f32) -> i64 {
        let u = u.clamp(0.0, 1.0);
        let (t_min, t_live, t_max) = (self.times[0], self.times[self.live_idx], *self.times.last().unwrap_or(&self.times[0]));
        if self.past_share > 0.0 && t_live > t_min && u <= self.past_share {
            t_min + ((u / self.past_share) as f64 * (t_live - t_min) as f64).round() as i64
        } else if t_max > t_live {
            t_live + (((u - self.past_share) / (1.0 - self.past_share).max(1e-4)) as f64 * (t_max - t_live) as f64).round() as i64
        } else {
            t_min + (u as f64 * (t_max - t_min) as f64).round() as i64
        }
    }
    pub fn pos_to_u(&self, pos: f32) -> f32 {
        self.time_to_u(self.ts_at_pos(pos))
    }
    pub fn u_to_pos(&self, u: f32) -> f32 {
        self.pos_for_ts(self.u_to_ts(u))
    }
    pub fn nearest_idx(&self, target_ts: i64) -> usize {
        self.times.iter().enumerate().min_by_key(|(_, t)| (*t - target_ts).abs()).map_or(self.live_idx, |(i, _)| i)
    }
}

#[derive(Clone, Copy)]
pub(super) struct ContextMenuState {
    pub screen: Point,
    pub vp: Size,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Default)]
pub(super) struct InspectState {
    pub coord: Option<(f64, f64)>,
    pub dropped_at: Option<Instant>,
    pub loading: bool,
    pub bundle: Option<WeatherBundle>,
    pub error: Option<String>,
    pub seq: u64,
}
type WindGrid = [(f32, f32, f32); 192];

fn pack_size(sz: Size) -> u64 {
    ((sz.width.to_bits() as u64) << 32) | (sz.height.to_bits() as u64)
}
fn unpack_size(bits: u64) -> Size {
    Size::new(f32::from_bits((bits >> 32) as u32), f32::from_bits(bits as u32))
}

type FlowCache = std::cell::RefCell<tiles::FxHashMap<(u64, i32, i32, i32), (f32, f32)>>;

pub struct MapEngine {
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
    pub base_style: BaseMapStyle,
    prev_base_style: Option<(BaseMapStyle, Instant)>,
    user_picked_style: bool,
    pub overlay: WeatherOverlay,
    pub(super) show_layer_menu: bool,
    pub(super) full_screen: bool,
    pub(super) show_terminator: bool,
    pub(super) legend_collapsed: bool,
    pub(super) playback_speed: f32,
    pub(super) overlay_opacity: f32,
    pub(super) precip_short_range: bool,
    pub(super) context_menu: Option<ContextMenuState>,
    pub(super) toast: Option<(String, Instant)>,
    pub(super) radar: RadarState,
    motion: CamMotion,
    pub(super) manifest: Option<RainViewerManifest>,
    /// Pre-interned `[primary_ns_id, eccc_hrdps_id, eccc_rdps_id, eccc_gdps_id]` per manifest frame (0 String allocs per tick!).
    manifest_ns_ids: Vec<[Option<u32>; 4]>,
    /// Pre-interned namespace IDs for `manifest.satellite` 12-hour global IR cloud frames.
    sat_ns_ids: Vec<u32>,
    last_manifest: Option<Instant>,
    last_prewarmed_strip: Option<(u8, i64, i64, i64, i64, usize)>,
    last_rendered_radar_idx: Cell<Option<usize>>,
    /// Cached screen-space optical-flow displacement `(dx_px, dy_px)` per `(pair_hash, cam_lat_q, cam_lon_q, zoom_q)`
    /// so 60fps motion-compensated radar advection runs in 0 µs per tick after a single ~80 µs estimate per frame pair!
    flow_cache: FlowCache,
    last_adj_prewarm: Option<(u8, u32, u32, BaseMapStyle)>,
    grid: SampleCache<GridWeatherPoint>,
    aqi: SampleCache<(f64, f64, f64)>,
    pub(super) grid_gen: u64,
    last_grid_poll: Option<Instant>,
    scalar_tex: Mutex<Option<ScalarCache>>,
    terminator_tex: Mutex<Option<ScalarCache>>,
    isobar_cache: Mutex<Option<IsobarCache>>,
    pub(super) wind_grid: Option<(u64, WindGrid)>,
    pub(super) inspect: InspectState,
    last_vp_bits: AtomicU64,
    last_tick: Instant,
    last_cam_move: Instant,
    /// True while far-future timeline frames are waiting for the view to settle before being prewarmed.
    strip_deferred: bool,
    pool: Arc<Mutex<SharedTilePool>>,
    base_cache: canvas::Cache,
    base_cache_sig: Cell<u64>,
    weather_cache: canvas::Cache,
    weather_cache_sig: Cell<u64>,
    label_cache: canvas::Cache,
    label_cache_sig: Cell<u64>,
    vec_cache: canvas::Cache,
    vec_cache_sig: Cell<u64>,
    mini_tile_cache: canvas::Cache,
    mini_tile_sig: Cell<u64>,
    mini_vec_cache: canvas::Cache,
    mini_vec_sig: Cell<u64>,
    rx: mpsc::Receiver<WorkerEvent>,
    wake: Arc<Notify>,
    client: WeatherClient,
    particles: Vec<WindParticle>,
}

impl MapEngine {
    pub fn new(client: WeatherClient, lat: f64, lon: f64, light: bool, wake: Arc<Notify>) -> Self {
        let ((tx, rx), now) = (mpsc::channel(), Instant::now());
        let pool = Arc::new(Mutex::new(SharedTilePool::new(client.clone(), tx, Arc::clone(&wake))));
        let cache_grp = iced::advanced::graphics::cache::Group::unique();
        let mut engine = Self {
            center_lat: lat,
            center_lon: lon,
            zoom: 5.0,
            base_style: if light { BaseMapStyle::CleanLight } else { BaseMapStyle::AppleDark },
            prev_base_style: None,
            user_picked_style: false,
            overlay: WeatherOverlay::RadarTimeline,
            show_layer_menu: false,
            full_screen: false,
            show_terminator: true,
            legend_collapsed: false,
            playback_speed: 1.0,
            overlay_opacity: 0.85,
            precip_short_range: false,
            context_menu: None,
            toast: None,
            radar: RadarState::default(),
            motion: CamMotion::Idle,
            manifest: None,
            manifest_ns_ids: Vec::new(),
            sat_ns_ids: Vec::new(),
            last_manifest: None,
            last_prewarmed_strip: None,
            last_rendered_radar_idx: Cell::new(None),
            flow_cache: std::cell::RefCell::new(tiles::FxHashMap::default()),
            last_adj_prewarm: None,
            grid: SampleCache::default(),
            aqi: SampleCache::default(),
            grid_gen: 1,
            last_grid_poll: None,
            scalar_tex: Mutex::new(None),
            terminator_tex: Mutex::new(None),
            isobar_cache: Mutex::new(None),
            wind_grid: None,
            inspect: InspectState::default(),
            last_vp_bits: AtomicU64::new(pack_size(Size::new(900.0, 700.0))),
            last_tick: now,
            last_cam_move: now,
            strip_deferred: false,
            pool,
            base_cache: canvas::Cache::with_group(cache_grp),
            base_cache_sig: Cell::new(0),
            weather_cache: canvas::Cache::with_group(cache_grp),
            weather_cache_sig: Cell::new(0),
            label_cache: canvas::Cache::with_group(cache_grp),
            label_cache_sig: Cell::new(0),
            vec_cache: canvas::Cache::with_group(cache_grp),
            vec_cache_sig: Cell::new(0),
            mini_tile_cache: canvas::Cache::with_group(cache_grp),
            mini_tile_sig: Cell::new(0),
            mini_vec_cache: canvas::Cache::with_group(cache_grp),
            mini_vec_sig: Cell::new(0),
            rx,
            wake,
            client,
            particles: init_wind_particles(),
        };
        engine.prewarm_region(lat, lon, false, light);
        // Hydrate radar manifest from disk instantly if <= 4 min old (0 ms, 0 network bytes on warm reopen!).
        // If a cached manifest fell back to EcccNwp-only (e.g. transient HRRR blip), hydrate for instant paint
        // but still refresh in the background so HRRR recovers immediately!
        if let Some(m) = crate::storage::load_cached_manifest(Duration::from_secs(240))
            && !m.frames.is_empty()
        {
            let has_hrrr = m.frames.iter().any(|f| matches!(f.source, RadarFrameSource::NoaaHrrr(..)));
            let li = m.frames.iter().rposition(|f| !f.is_nowcast).unwrap_or(m.frames.len().saturating_sub(1));
            engine.radar.snap(li);
            engine.set_manifest(m);
            if has_hrrr {
                engine.last_manifest = Some(Instant::now());
            } else {
                engine.spawn_manifest_fetch();
            }
        } else {
            engine.spawn_manifest_fetch();
        }
        engine
    }

    fn set_manifest(&mut self, m: RainViewerManifest) {
        let (next_ids, next_sat): (Vec<[Option<u32>; 4]>, Vec<u32>) = if let Ok(mut pool) = self.pool.lock() {
            let fids = m
                .frames
                .iter()
                .map(|f| {
                    let p = f.tile_namespace().map(|s| pool.intern(&s));
                    let [h, r, g] = f.eccc_tier_namespaces().map_or([None; 3], |t| t.map(|s| Some(pool.intern(&s))));
                    [p, h, r, g]
                })
                .collect();
            let sids = m.satellite.iter().map(|s| pool.intern(&s.tile_namespace())).collect();
            (fids, sids)
        } else {
            (Vec::new(), Vec::new())
        };
        let changed = (!self.manifest_ns_ids.is_empty() && self.manifest_ns_ids != next_ids)
            || (!self.sat_ns_ids.is_empty() && self.sat_ns_ids != next_sat);
        self.manifest_ns_ids = next_ids;
        self.sat_ns_ids = next_sat;
        self.manifest = Some(m);
        if changed {
            self.last_rendered_radar_idx.set(None);
            self.flow_cache.borrow_mut().clear();
            if let Ok(mut pool) = self.pool.lock() {
                pool.bump_epoch();
            }
        }
    }

    pub fn is_full_screen(&self) -> bool {
        self.full_screen
    }
    pub fn has_context_menu(&self) -> bool {
        self.context_menu.is_some()
    }
    pub fn close_context_menu(&mut self) {
        self.context_menu = None;
    }
    pub(super) fn viewport_size(&self) -> Size {
        unpack_size(self.last_vp_bits.load(Ordering::Relaxed))
    }
    fn record_viewport_size(&self, sz: Size) {
        if sz.width >= 32.0 && sz.height >= 32.0 {
            self.last_vp_bits.store(pack_size(sz), Ordering::Relaxed);
        }
    }

    /// Syncs the default basemap style with Light/Dark appearance unless the user manually picked a map style.
    pub fn sync_theme(&mut self, light: bool) {
        if !self.user_picked_style {
            let next = if light { BaseMapStyle::CleanLight } else { BaseMapStyle::AppleDark };
            if next != self.base_style {
                self.prev_base_style = Some((self.base_style, Instant::now()));
                self.base_style = next;
                self.last_adj_prewarm = None;
                // Immediately warm the new style's basemap & labels at the mini-map's tile zoom and the current zoom so theme switches never flash blank ocean!
                if let Ok(mut p) = self.pool.lock() {
                    let coord = (self.center_lat, self.center_lon);
                    let mini_z = Self::mini_tile_zoom(next);
                    Self::prewarm_style_2x2(&mut p, next, mini_z, coord, true);
                    let cur_z = effective_tile_zoom(self.zoom, next.spec().1);
                    if cur_z != mini_z {
                        Self::prewarm_style_2x2(&mut p, next, cur_z, coord, true);
                    }
                }
            }
        }
    }

    pub fn show_layer_menu(&self) -> bool {
        self.show_layer_menu
    }
    pub fn close_layer_menu(&mut self) {
        self.show_layer_menu = false;
    }
    pub fn inspected_coord(&self) -> Option<(f64, f64)> {
        self.inspect.coord
    }
    #[cfg(test)]
    pub fn radar_playing(&self) -> bool {
        self.radar.playing
    }
    #[cfg(test)]
    pub fn is_flying(&self) -> bool {
        matches!(self.motion, CamMotion::Fly { .. })
    }
    #[cfg(test)]
    pub fn inject_test_manifest(&mut self, now: i64) {
        let frames = (-3..=6_i64)
            .map(|k| RainViewerFrame { time_unix: now + k * 600, is_nowcast: k > 0, source: RadarFrameSource::EcccNwp })
            .collect();
        self.set_manifest(RainViewerManifest { host: String::new(), frames, satellite: Vec::new() });
        self.radar.snap(3);
    }
    #[cfg(test)]
    pub fn bench_internals(&mut self, bundles: &HashMap<String, WeatherBundle>) {
        use std::time::Instant;
        // Seed 36 realistic grid points around PNW/California
        for r in 0..6 {
            for c in 0..6 {
                let (la, lo) = (34.0 + r as f64 * 3.0, -130.0 + c as f64 * 3.5);
                let precip = if (r == 2 || r == 3) && (c == 1 || c == 2) { 3.5_f32 } else { 0.0 };
                let press = 1013.0 + ((r as f32 - 2.5) * 4.0) - ((c as f32 - 2.5) * 3.0);
                self.grid.data.insert(
                    overlays::quantize_coord(la, lo),
                    GridWeatherPoint::stationary(la, lo, [12.0 + r as f32, precip, 4.0, -3.0, 5.0, press]),
                );
            }
        }
        self.grid_gen = self.grid_gen.wrapping_add(1);
        let vp = (Size::new(960.0, 760.0), 40.5, -122.0, 5.8);

        let t0 = Instant::now();
        for _ in 0..20 {
            let _ = self.scalar_tex.lock().map(|mut g| *g = None);
            let _ = std::hint::black_box(self.build_scalar_overlay_handle(vp, WeatherOverlay::Temperature, bundles, None));
        }
        let scalar_temp_us = t0.elapsed().as_micros() / 20;

        let t0 = Instant::now();
        for _ in 0..20 {
            let _ = self.scalar_tex.lock().map(|mut g| *g = None);
            let _ = std::hint::black_box(self.build_scalar_overlay_handle(vp, WeatherOverlay::Pressure, bundles, Some(1_795_400_000)));
        }
        let scalar_precip_us = t0.elapsed().as_micros() / 20;

        let t0 = Instant::now();
        for _ in 0..20 {
            let _ = self.terminator_tex.lock().map(|mut g| *g = None);
            let _ = std::hint::black_box(self.build_terminator_overlay_handle(vp, BaseMapStyle::CleanLight, 1_795_400_000));
        }
        let terminator_us = t0.elapsed().as_micros() / 20;

        let t0 = Instant::now();
        for _ in 0..30 {
            let _ = self.isobar_cache.lock().map(|mut g| *g = None);
            let _ = std::hint::black_box(self.build_pressure_isobars(vp, bundles, None));
        }
        let isobar_us = t0.elapsed().as_micros() / 30;

        let t0 = Instant::now();
        for _ in 0..200 {
            self.step_wind_particles(bundles, 0.016);
        }
        let wind_step_us = t0.elapsed().as_micros() / 200;

        // Tile stylize & bleed benchmarks (realistic 512x512 LibreWXR s8 + 256x256 HRRR REFP)
        let mut tile512 = vec![0u8; 512 * 512 * 4];
        for y in 160..350 {
            for x in 160..350 {
                let i = (y * 512 + x) * 4;
                tile512[i..i + 4].copy_from_slice(&[54, 85, 171, 210]);
            }
        }
        let t0 = Instant::now();
        for _ in 0..30 {
            let mut buf = tile512.clone();
            let _ = std::hint::black_box(tiles::stylize_radar_tile("lwxr8_1795400000", (5, 4, 11), &mut buf, 512, 512));
        }
        let stylize_lwxr512_us = t0.elapsed().as_micros() / 30;

        let t0 = Instant::now();
        for _ in 0..30 {
            let mut buf = tile512.clone();
            tiles::bleed_transparent_edges(&mut buf, 512, 512);
            std::hint::black_box(buf[0]);
        }
        let bleed_512_us = t0.elapsed().as_micros() / 30;

        let mut tile256 = vec![0u8; 256 * 256 * 4];
        for y in 60..190 {
            for x in 60..190 {
                let i = (y * 256 + x) * 4;
                tile256[i..i + 4].copy_from_slice(&[64, 170, 93, 255]);
            }
        }
        let t0 = Instant::now();
        for _ in 0..30 {
            let mut buf = tile256.clone();
            let _ = std::hint::black_box(tiles::stylize_radar_tile("hrrr_refp_202611221900_0180", (5, 5, 11), &mut buf, 256, 256));
            tiles::bleed_transparent_edges(&mut buf, 256, 256);
        }
        let stylize_hrrr256_us = t0.elapsed().as_micros() / 30;

        let settings = Settings::default();
        let t0 = Instant::now();
        for _ in 0..500 {
            let _ = std::hint::black_box(self.sample_probe_label(39.2, -121.8, WeatherOverlay::Temperature, bundles, &settings));
        }
        let probe_ns = t0.elapsed().as_nanos() / 500;

        // Profile real Esri JPEG, Esri Label PNG, and GOES-W IR PNG decode end-to-end (bytes -> RGBA Handle)
        let bench_url = |ns: &str, url: &str| {
            self.client.fetch_bytes_timeout(url, 6000).ok().map(|b| {
                let t0 = Instant::now();
                for _ in 0..20 {
                    let _ = std::hint::black_box(tiles::decode_tile(ns, (5, 4, 11), &b));
                }
                (t0.elapsed().as_micros() / 20, b.len())
            })
        };
        let esri_jpg_us = bench_url(
            "esri_light_base",
            "https://services.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Light_Gray_Base/MapServer/tile/5/12/5",
        );
        let esri_lbl_us = bench_url(
            "esri_light_labels",
            "https://services.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Light_Gray_Reference/MapServer/tile/5/12/5",
        );
        let goes_ir_us =
            bench_url("iem_goes_ir_w", "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/goes_west_fulldisk_ch13/5/5/12.png");

        println!("  scalar_temp_build_us:      {scalar_temp_us:>7} us");
        println!("  scalar_precip_build_us:    {scalar_precip_us:>7} us");
        println!("  terminator_build_us:       {terminator_us:>7} us");
        println!("  isobar_build_us:           {isobar_us:>7} us");
        println!("  wind_step_us:              {wind_step_us:>7} us");
        println!("  sample_probe_ns:           {probe_ns:>7} ns");
        println!("  stylize_lwxr512_us:        {stylize_lwxr512_us:>7} us");
        println!("  bleed_edges_512_us:        {bleed_512_us:>7} us");
        println!("  stylize_bleed_hrrr256_us:  {stylize_hrrr256_us:>7} us");
        if let Some((us, sz)) = esri_jpg_us {
            println!("  decode_esri_jpg256_us:     {us:>7} us ({sz} B wire)");
        }
        if let Some((us, sz)) = esri_lbl_us {
            println!("  decode_esri_lbl_png256_us: {us:>7} us ({sz} B wire)");
        }
        if let Some((us, sz)) = goes_ir_us {
            println!("  decode_goes_ir_png256_us:  {us:>7} us ({sz} B wire)");
        }
    }

    #[cfg(test)]
    pub fn in_flight_count(&self) -> usize {
        self.pool.lock().map(|p| p.in_flight.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn has_pending_work(&self, expanded: bool) -> bool {
        self.needs_animation_tick(expanded)
            || (expanded && self.strip_deferred && self.overlay == WeatherOverlay::RadarTimeline)
            || self.pool.lock().is_ok_and(|p| !p.in_flight.is_empty())
            || self.inspect.loading
            || !self.grid.in_flight.is_empty()
            || !self.aqi.in_flight.is_empty()
    }
    pub(super) fn active_timeline(&self) -> Option<TimelineSpec> {
        if !self.overlay.has_timeline() {
            return None;
        }
        if self.overlay == WeatherOverlay::RadarTimeline {
            let m = self.manifest.as_ref().filter(|m| m.frames.len() >= 2)?;
            let live_idx = self.live_frame_idx().min(m.frames.len() - 1);
            let live_ts = m.frames[live_idx].time_unix;
            // macOS Weather 1h vs 12h precipitation range toggle: 1h focuses on high-res past + next-hour radar!
            let end_idx = if self.precip_short_range {
                let lim = live_ts + 4200;
                m.frames.iter().rposition(|f| f.time_unix <= lim).unwrap_or(live_idx).max((live_idx + 1).min(m.frames.len() - 1))
            } else {
                m.frames.len() - 1
            };
            let times: Vec<i64> = m.frames[..=end_idx].iter().map(|f| f.time_unix).collect();
            if times.len() < 2 {
                return None;
            }
            let li = live_idx.min(times.len() - 1);
            let past_share = if li > 0 && times[li] > times[0] && *times.last().unwrap_or(&0) > times[li] {
                if self.precip_short_range { 0.40 } else { 0.20 }
            } else {
                0.0
            };
            Some(TimelineSpec { times, live_idx: li, past_share })
        } else if self.overlay == WeatherOverlay::InfraredClouds {
            let m = self.manifest.as_ref().filter(|m| m.satellite.len() >= 2)?;
            let times: Vec<i64> = m.satellite.iter().map(|s| s.time_unix).collect();
            Some(TimelineSpec { live_idx: times.len() - 1, times, past_share: 1.0 })
        } else {
            // Continuous 12-hour forecast overlays (Temperature, Wind, Pressure) use uniform 30-min steps independent of radar manifest!
            let now = chrono::Utc::now().timestamp();
            let base = self
                .manifest
                .as_ref()
                .and_then(|m| m.frames.get(self.live_frame_idx()))
                .map(|f| f.time_unix)
                .filter(|&t| (t - now).abs() <= 2400)
                .unwrap_or_else(|| (now / 600) * 600);
            let times = (0..=24_i64).map(|k| base + k * 1800).collect();
            Some(TimelineSpec { times, live_idx: 0, past_share: 0.0 })
        }
    }
    /// Test-only: `(idx, time_unix, source description)` for every manifest frame (playback-strip audit).
    #[cfg(test)]
    pub fn debug_manifest(&self) -> Vec<(usize, i64, String)> {
        self.manifest.as_ref().map_or_else(Vec::new, |m| {
            m.frames.iter().enumerate().map(|(i, f)| (i, f.time_unix, format!("{:?} nowcast={}", f.source, f.is_nowcast))).collect()
        })
    }
    /// Test-only: pins the exact fractional playback position with the continuous-playback (`playing`) blend curve.
    #[cfg(test)]
    pub fn debug_set_play(&mut self, pos: f32, playing: bool) {
        (self.radar.play_pos, self.radar.target_pos, self.radar.frame_idx, self.radar.playing) = (pos, pos, pos.round() as usize, playing);
    }
    pub fn live_frame_idx(&self) -> usize {
        self.manifest.as_ref().and_then(|m| m.frames.iter().rposition(|f| !f.is_nowcast)).unwrap_or(0)
    }

    fn spawn_manifest_fetch(&mut self) {
        self.last_manifest = Some(Instant::now());
        let (c, tx, wk) = (self.client.clone(), self.pool.lock().expect("pool").tx.clone(), Arc::clone(&self.wake));
        std::thread::spawn(move || {
            if !is_shutting_down() {
                c.fetch_radar_manifest_streaming(|m| {
                    if !is_shutting_down() {
                        let _ = tx.send(WorkerEvent::ManifestLoaded(m));
                        wk.notify_one();
                    }
                });
            }
        });
    }

    fn prewarm_2x2(
        p: &mut SharedTilePool, ns: &str, z: u8, (lat, lon): (f64, f64), ttl: Duration, high: bool, url: impl Fn(u32, u32) -> String,
    ) {
        let (n, (wx, wy)) = (1_i64 << z, lat_lon_to_world_px(lat, lon, z as f64));
        let (x0, y0) = ((wx / 256.0 - 0.5).floor() as i64, (wy / 256.0 - 0.5).floor() as i64);
        for dy in 0..=1_i64 {
            for dx in 0..=1_i64 {
                let (tx, ty) = ((x0 + dx).rem_euclid(n) as u32, (y0 + dy).clamp(0, n - 1) as u32);
                p.ensure(ns, (z, tx, ty), || url(tx, ty), ttl, high, true);
            }
        }
    }
    fn prewarm_viewport_base(&self, st: BaseMapStyle, zoom: f64, (lat, lon): (f64, f64), vp_sz: Size) {
        let Ok(mut p) = self.pool.lock() else { return };
        let (ttl, max_z) = (Duration::from_secs(86_400 * 7), st.spec().1);
        let (tz, min_x, max_x, min_y, max_y) = visible_tile_bounds((vp_sz, lat, lon, zoom), max_z);
        let n = 1_i64 << tz;
        let ns = st.spec().2;
        for ty in min_y..=max_y {
            let wy = ty as u32;
            for tx in min_x..=max_x {
                let wx = tx.rem_euclid(n) as u32;
                p.ensure(ns, (tz, wx, wy), || st.tile_url(tz, wx, wy), ttl, true, true);
            }
        }
        if let Some((lns, svc)) = st.label_overlay() {
            for ty in min_y..=max_y {
                let wy = ty as u32;
                for tx in min_x..=max_x {
                    let wx = tx.rem_euclid(n) as u32;
                    p.ensure(lns, (tz, wx, wy), || BaseMapStyle::label_tile_url(svc, tz, wx, wy), ttl, true, true);
                }
            }
        }
    }
    fn prewarm_style_3x3(p: &mut SharedTilePool, st: BaseMapStyle, z: u8, (lat, lon): (f64, f64), high: bool) {
        let (ttl, n, (cx, cy)) = (Duration::from_secs(86_400 * 7), 1_i64 << z, tile_coord_at(lat, lon, z));
        let ring = [(0_i64, 0_i64), (-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, -1), (-1, 1), (1, 1)];
        let ns = st.spec().2;
        // Push outer tiles first so high-priority LIFO workers pop the center viewport tile FIRST!
        for &(dx, dy) in ring.iter().rev() {
            let (tx, ty) = ((cx as i64 + dx).rem_euclid(n) as u32, (cy as i64 + dy).clamp(0, n - 1) as u32);
            p.ensure(ns, (z, tx, ty), || st.tile_url(z, tx, ty), ttl, high, true);
        }
        if let Some((lns, svc)) = st.label_overlay() {
            for &(dx, dy) in ring.iter().rev() {
                let (tx, ty) = ((cx as i64 + dx).rem_euclid(n) as u32, (cy as i64 + dy).clamp(0, n - 1) as u32);
                p.ensure(lns, (z, tx, ty), || BaseMapStyle::label_tile_url(svc, z, tx, ty), ttl, high, true);
            }
        }
    }
    fn prewarm_style_2x2(p: &mut SharedTilePool, st: BaseMapStyle, z: u8, coord: (f64, f64), high: bool) {
        let ttl = Duration::from_secs(86_400 * 7);
        Self::prewarm_2x2(p, st.spec().2, z, coord, ttl, high, |x, y| st.tile_url(z, x, y));
        let _ = st
            .label_overlay()
            .map(|(lns, svc)| Self::prewarm_2x2(p, lns, z, coord, ttl, high, |x, y| BaseMapStyle::label_tile_url(svc, z, x, y)));
    }
    /// Basemap / label tile zoom the dashboard mini-map draws at (both resolve to the same level at [`MINI_MAP_ZOOM`]).
    fn mini_tile_zoom(style: BaseMapStyle) -> u8 {
        effective_tile_zoom(MINI_MAP_ZOOM, style.spec().1)
    }
    pub fn prewarm_saved_locations(&mut self, locs: &[Location], _light: bool) {
        // Lean boot prewarm: warm ONLY the active city's 2x2 tiles in `self.base_style` (reused 100% by both mini-map & expanded map!).
        let Ok(mut p) = self.pool.lock() else { return };
        let coord = locs.first().map_or((self.center_lat, self.center_lon), |l| (l.lat, l.lon));
        Self::prewarm_style_2x2(&mut p, self.base_style, Self::mini_tile_zoom(self.base_style), coord, true);
    }
    pub fn prewarm_region(&mut self, lat: f64, lon: f64, expanded: bool, _light: bool) {
        let Ok(mut p) = self.pool.lock() else { return };
        let z = if expanded { effective_tile_zoom(self.zoom, self.base_style.spec().1) } else { Self::mini_tile_zoom(self.base_style) };
        Self::prewarm_style_2x2(&mut p, self.base_style, z, (lat, lon), true);
    }

    pub fn jump_to_location(&mut self, lat: f64, lon: f64, expanded: bool, light: bool) {
        self.clear_inspector();
        (self.motion, self.last_grid_poll, self.last_prewarmed_strip) = (CamMotion::Idle, None, None);
        let _ = self.scalar_tex.lock().map(|mut g| *g = None);
        let _ = self.pool.lock().map(|mut p| p.bump_epoch());
        if expanded {
            self.fly_to(lat, lon, None);
        } else {
            (self.center_lat, self.center_lon, self.last_cam_move) = (lat.clamp(-85.0, 85.0), wrap_lon(lon), Instant::now());
        }
        self.prewarm_region(lat, lon, expanded, light);
        if expanded && matches!(self.overlay, WeatherOverlay::RadarTimeline | WeatherOverlay::InfraredClouds) {
            self.prewarm_timeline_strip();
        }
    }

    fn prewarm_live_radar(&self) {
        let Some(m) = self.manifest.as_ref().filter(|m| !m.frames.is_empty()) else { return };
        let li = self.live_frame_idx().min(m.frames.len() - 1);
        self.prewarm_radar_frame(&m.host, &m.frames[li], PRIO_MAX);
    }

    pub fn selected_forecast_timestamp(&self) -> Option<i64> {
        let tl = self.active_timeline()?;
        let (li, max_i) = (tl.live_idx, tl.max_idx());
        if !self.radar.playing {
            let ti = (self.radar.target_pos.round() as usize).min(max_i);
            if ti <= li && self.radar.target_pos <= li as f32 + 0.01 {
                return None;
            }
        }
        let mut pos = self.radar.play_pos.clamp(0.0, max_i as f32);
        if !self.radar.playing && self.radar.target_pos > li as f32 && pos <= li as f32 {
            pos = self.radar.target_pos.min(max_i as f32);
        }
        if pos <= li as f32 + 0.002 {
            return None;
        }
        Some(tl.ts_at_pos(pos))
    }
}

/// Iterates all visible tiles for a layer (up to 96 tiles; falls back to viewport center tile on degenerate zoom-outs).
/// Returns `false` early if the visitor returns `false`.
fn for_each_visible_tile_biased(vp: MapVp, max_z: u8, bias: f64, mut visit: impl FnMut(u8, u32, u32, f64) -> bool) -> bool {
    let (size, lat, lon, zoom) = vp;
    let z = effective_tile_zoom_biased(zoom, max_z, bias);
    let px = 256.0 * 2.0_f64.powf(zoom - z as f64);
    let n = 1_i64 << z;
    let c = lat_lon_to_world_px(lat, lon, zoom);
    let half = (size.width.max(320.0) as f64 * 0.5, size.height.max(240.0) as f64 * 0.5);
    let (min_x, max_x) = (((c.0 - half.0) / px).floor() as i64, ((c.0 + half.0) / px).ceil() as i64 - 1);
    let (min_y, max_y) = ((((c.1 - half.1) / px).floor() as i64).max(0), ((((c.1 + half.1) / px).ceil() as i64) - 1).min(n - 1));
    if min_y > max_y || min_x > max_x || ((max_x - min_x + 1) * (max_y - min_y + 1)) > 96 {
        let (cx, cy) = tile_coord_at(lat, lon, z);
        return visit(z, cx, cy, 0.0);
    }
    let (cx_f, cy_f) = (c.0 / px - 0.5, c.1 / px - 0.5);
    for ty in min_y..=max_y {
        for tx in min_x..=max_x {
            let d2 = (tx as f64 - cx_f).powi(2) + (ty as f64 - cy_f).powi(2);
            if !visit(z, tx.rem_euclid(n) as u32, ty as u32, d2) {
                return false;
            }
        }
    }
    true
}

/// Returns whether ECCC tier `model` is actually drawn somewhere in the part of tile `(z, x, y)` near viewport `vp`
/// (`outside_hrrr == true` for HRRR frames). Tiles for which this is false are never requested, awaited or drawn.
#[inline]
fn eccc_tier_tile_needed(model: EcccModel, outside_hrrr: bool, z: u8, x: u32, y: u32, vp: MapVp) -> bool {
    overlays::eccc_tier_needed_in(model, outside_hrrr, z, x, y, overlays::viewport_geo_window(vp))
}

#[derive(Debug, Clone, Copy)]
struct FrameReadiness {
    /// True iff 100% of visible viewport tiles (core + border + offshore ECCC tiers) are settled at native target zoom `tz`.
    all_ready: bool,
    /// True iff 100% of visible viewport tiles are either settled at `tz` OR visually covered by an adjacent-zoom
    /// parent/child tile already in RAM (so zooming in/out never stalls or blinks blank!).
    visually_covered: bool,
    /// True iff >= 80% of visible tiles (and all tiles near viewport center) are settled/covered — only 1-2 far outer
    /// corner slivers remain in flight (fillable seamlessly by `temporal_fallbacks` without freezing playback!).
    core_ready: bool,
}

/// Returns the exact visible tile grid bounds `(z, min_x, max_x, min_y, max_y)` for weather viewport `vp` at `max_z`.
fn visible_tile_bounds((bounds, lat, lon, zoom): MapVp, max_z: u8) -> (u8, i64, i64, i64, i64) {
    let z = effective_tile_zoom_biased(zoom, max_z, -0.35);
    let (px, n, c) = (256.0 * 2.0_f64.powf(zoom - z as f64), 1_i64 << z, lat_lon_to_world_px(lat, lon, zoom));
    let half = (bounds.width as f64 * 0.5, bounds.height as f64 * 0.5);
    let (min_x, max_x) = (((c.0 - half.0) / px).floor() as i64, ((c.0 + half.0) / px).ceil() as i64 - 1);
    let (min_y, max_y) = ((((c.1 - half.1) / px).floor() as i64).max(0), ((((c.1 + half.1) / px).ceil() as i64) - 1).min(n - 1));
    (z, min_x, max_x, min_y, max_y)
}

/// Evaluates viewport readiness for `f` using pre-interned `[primary, hrdps, rdps, gdps]` IDs (0 String allocs per tick!):
/// tracks full completeness (`all_ready`), zoom-fallback completeness (`visually_covered`), and inner-core readiness
/// (`core_ready`, where 100% of inner core tiles `d2 <= 2.25` are fresh/zoom-covered and any missing outer border
/// sliver is seamlessly bridged by an adjacent temporal frame already in RAM via `temporal_fallbacks`!).
fn radar_frame_readiness_ids(
    f: &RainViewerFrame, ids: Option<[Option<u32>; 4]>, temporal_primary: [Option<u32>; 2], pool: &SharedTilePool, vp: MapVp,
) -> FrameReadiness {
    let mut all_ready = true;
    let mut visually_covered = true;
    let mut core_miss = false;
    let (mut total_req, mut covered_req) = (0_u32, 0_u32);
    let [primary_id, hrdps_id, rdps_id, gdps_id] = match ids {
        Some(arr) => arr,
        None => {
            let p = f.tile_namespace().and_then(|ns| pool.lookup_ns(&ns));
            let [h, r, g] = f.eccc_tier_namespaces().map_or([None; 3], |t| t.map(|ns| pool.lookup_ns(&ns)));
            [p, h, r, g]
        }
    };
    let is_hrrr = matches!(f.source, RadarFrameSource::NoaaHrrr(..));
    let has_eccc = matches!(f.source, RadarFrameSource::NoaaHrrr(..) | RadarFrameSource::EcccNwp);
    if has_eccc {
        for (model, tid) in EcccModel::ALL.into_iter().zip([hrdps_id, rdps_id, gdps_id]) {
            let max_z = model.max_native_zoom();
            for_each_visible_tile_biased(vp, max_z, model.zoom_bias(), |z, x, y, d2| {
                if eccc_tier_tile_needed(model, is_hrrr, z, x, y, vp) {
                    total_req += 1;
                    if tid.is_some_and(|id| pool.is_settled_id(id, (z, x, y))) {
                        covered_req += 1;
                    } else {
                        all_ready = false;
                        if tid.is_some_and(|id| pool.has_zoom_fallback_id(id, (z, x, y), max_z)) {
                            covered_req += 1;
                        } else {
                            visually_covered = false;
                            if d2 <= 2.25 {
                                core_miss = true;
                            }
                        }
                    }
                }
                true
            });
        }
    }
    if primary_id.is_some() || !has_eccc {
        let max_z = f.max_native_zoom();
        let bias = f.primary_zoom_bias();
        for_each_visible_tile_biased(vp, max_z, bias, |tz, x, y, d2| {
            if !is_hrrr || overlays::tile_intersects_hrrr(tz, x, y) {
                total_req += 1;
                if primary_id.is_some_and(|id| pool.is_settled_id(id, (tz, x, y))) {
                    covered_req += 1;
                } else {
                    all_ready = false;
                    if primary_id.is_some_and(|id| pool.has_zoom_fallback_id(id, (tz, x, y), max_z)) {
                        covered_req += 1;
                    } else {
                        visually_covered = false;
                        if d2 <= 2.25 {
                            core_miss = true;
                        } else if temporal_primary.iter().flatten().any(|&pid| pool.is_settled_id(pid, (tz, x, y))) {
                            covered_req += 1;
                        }
                    }
                }
            }
            true
        });
    }
    let core_ready = visually_covered || (!core_miss && total_req > 0 && covered_req * 5 >= total_req * 4);
    FrameReadiness { all_ready, visually_covered, core_ready }
}

/// True iff every visible viewport tile required to render `f` (including HRRR CONUS + applicable ECCC tiers)
/// is already settled in `pool` (loaded or in temporary error cooldown).
#[cfg(test)]
fn is_radar_frame_viewport_ready(f: &RainViewerFrame, pool: &SharedTilePool, vp: MapVp) -> bool {
    radar_frame_readiness_ids(f, None, [None; 2], pool, vp).all_ready
}

/// Enqueues all missing visible viewport tiles for `f` into strict-FIFO priority bucket `prio` (`0..=23`).
/// Returns `true` iff every missing tile was enqueued (or already ready/in-flight) without hitting low-prio backpressure.
fn ensure_radar_frame_visible(pool: &mut SharedTilePool, host: &str, f: &RainViewerFrame, vp: MapVp, prio: u8, mark_wanted: bool) -> bool {
    fn enqueue_layer(
        pool: &mut SharedTilePool, vp: MapVp, ns: &str, opts: (u8, f64, u8, bool), filter: impl Fn(u8, u32, u32) -> bool,
        url_fn: impl Fn(u8, u32, u32) -> String,
    ) -> bool {
        let (max_z, bias, prio, mark_wanted) = opts;
        let ns_id = pool.intern(ns);
        let mut coords: Vec<(u8, u32, u32, f64)> = Vec::with_capacity(24);
        for_each_visible_tile_biased(vp, max_z, bias, |z, wx, wy, d2| {
            if filter(z, wx, wy) && !pool.is_settled_id(ns_id, (z, wx, wy)) {
                coords.push((z, wx, wy, d2));
            }
            true
        });
        if coords.is_empty() {
            return true;
        }
        // Strict FIFO within each priority bucket: sort center-out so center tiles ALWAYS pop first!
        coords.sort_by(|a, b| a.3.total_cmp(&b.3));
        let mut all_ok = true;
        for (z, wx, wy, _) in coords {
            let url = url_fn(z, wx, wy);
            if url.is_empty() {
                continue;
            }
            if mark_wanted {
                pool.mark_wanted(TileLayerKind::Weather, TileKey(ns_id, z, wx, wy));
            }
            all_ok &= pool.ensure_id_prio(ns_id, (z, wx, wy), || url, Duration::from_secs(3600), prio, true);
        }
        all_ok
    }
    let mut ok = true;
    let is_hrrr = matches!(f.source, RadarFrameSource::NoaaHrrr(..));
    // Always enqueue slower ECCC GeoMet WMS tiers BEFORE primary HRRR/LibreWXR so they launch in parallel immediately!
    // Each tier tile is one interpolation slot; its job fetches (or reuses) the bracketing hourly rasters itself.
    if let Some(tiers) = f.eccc_tier_namespaces() {
        let (h0, q) = f.eccc_slot();
        for (model, ns) in EcccModel::ALL.into_iter().zip(tiers) {
            ok &= enqueue_layer(
                pool,
                vp,
                &ns,
                (model.max_native_zoom(), model.zoom_bias(), prio, mark_wanted),
                |z, x, y| eccc_tier_tile_needed(model, is_hrrr, z, x, y, vp),
                |_, _, _| eccc::job_spec(model, h0, q),
            );
        }
    }
    if let Some(ns) = f.tile_namespace() {
        ok &= enqueue_layer(
            pool,
            vp,
            &ns,
            (f.max_native_zoom(), f.primary_zoom_bias(), prio, mark_wanted),
            |z, x, y| !is_hrrr || overlays::tile_intersects_hrrr(z, x, y),
            |z, x, y| f.tile_url(host, z, x, y).unwrap_or_default(),
        );
    }
    ok
}

const SAT_MAX_NATIVE_ZOOM: u8 = 6;
const SAT_ZOOM_BIAS: f64 = -0.95;

fn sat_frame_readiness_id(ns_id: u32, temporal: [Option<u32>; 2], pool: &SharedTilePool, vp: MapVp) -> FrameReadiness {
    let (mut all_ready, mut visually_covered, mut core_miss) = (true, true, false);
    let (mut total_req, mut covered_req) = (0_u32, 0_u32);
    for_each_visible_tile_biased(vp, SAT_MAX_NATIVE_ZOOM, SAT_ZOOM_BIAS, |tz, x, y, d2| {
        total_req += 1;
        if pool.is_settled_id(ns_id, (tz, x, y)) {
            covered_req += 1;
        } else {
            all_ready = false;
            if pool.has_zoom_fallback_id(ns_id, (tz, x, y), SAT_MAX_NATIVE_ZOOM) {
                covered_req += 1;
            } else {
                visually_covered = false;
                if d2 <= 2.25 {
                    core_miss = true;
                } else if temporal.iter().flatten().any(|&pid| pool.is_settled_id(pid, (tz, x, y))) {
                    covered_req += 1;
                }
            }
        }
        true
    });
    let core_ready = visually_covered || (!core_miss && total_req > 0 && covered_req * 5 >= total_req * 4);
    FrameReadiness { all_ready, visually_covered, core_ready }
}

fn ensure_sat_frame_visible(pool: &mut SharedTilePool, host: &str, f: &SatelliteFrame, vp: MapVp, prio: u8, mark_wanted: bool) -> bool {
    let ns = f.tile_namespace();
    let ns_id = pool.intern(&ns);
    let mut coords: Vec<(u8, u32, u32, f64)> = Vec::with_capacity(24);
    for_each_visible_tile_biased(vp, SAT_MAX_NATIVE_ZOOM, SAT_ZOOM_BIAS, |z, wx, wy, d2| {
        if !pool.is_settled_id(ns_id, (z, wx, wy)) {
            coords.push((z, wx, wy, d2));
        }
        true
    });
    if coords.is_empty() {
        return true;
    }
    coords.sort_by(|a, b| a.3.total_cmp(&b.3));
    let mut all_ok = true;
    for &(z, wx, wy, _) in &coords {
        if mark_wanted {
            pool.mark_wanted(TileLayerKind::Weather, TileKey(ns_id, z, wx, wy));
        }
        all_ok &= pool.ensure_id_prio(ns_id, (z, wx, wy), || f.tile_url(host, z, wx, wy), Duration::from_secs(1800), prio, true);
    }
    all_ok
}

impl MapEngine {
    pub fn take_pending_gpu_pins(&self, max: usize) -> Vec<(TileKey, adv_image::Handle)> {
        self.pool.lock().map_or_else(|_| Vec::new(), |mut p| p.take_pending_gpu_pins(max))
    }
    pub fn store_gpu_pins(&self, pins: Vec<(TileKey, Option<iced_runtime::image::Allocation>)>) {
        if let Ok(mut p) = self.pool.lock() {
            p.store_gpu_pins(pins);
        }
    }
    fn prewarm_radar_frame(&self, host: &str, f: &RainViewerFrame, prio: u8) -> bool {
        let Ok(mut p) = self.pool.lock() else { return true };
        let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
        ensure_radar_frame_visible(&mut p, host, f, vp, prio, false)
    }
    fn prewarm_sat_frame(&self, host: &str, f: &SatelliteFrame, prio: u8) -> bool {
        let Ok(mut p) = self.pool.lock() else { return true };
        let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
        ensure_sat_frame_visible(&mut p, host, f, vp, prio, false)
    }
    fn prewarm_timeline_strip(&mut self) {
        let Some(m) = self.manifest.clone() else { return };
        let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
        if self.overlay == WeatherOverlay::InfraredClouds {
            if m.satellite.is_empty() {
                return;
            }
            let (z, min_x, max_x, min_y, max_y) = visible_tile_bounds(vp, SAT_MAX_NATIVE_ZOOM);
            let len = m.satellite.len();
            let cur = self.radar.frame_idx.min(len - 1);
            let key = (z, min_x, max_x, min_y, max_y, (1 << 20) | (cur << 2) | usize::from(self.radar.playing));
            if self.last_prewarmed_strip == Some(key) {
                return;
            }
            // Monotonic priority order starting at `cur` and wrapping to `0` (`-11h`) immediately when paused at `Now`
            // so pressing Play starts from frame 0 with 0 ms buffering!
            let mut all_ok = true;
            for off in 0..len {
                let idx = (cur + off) % len;
                let prio = PRIO_MAX.saturating_sub(off.min(21) as u8).max(2);
                all_ok &= self.prewarm_sat_frame(&m.host, &m.satellite[idx], prio);
            }
            if all_ok {
                self.last_prewarmed_strip = Some(key);
            }
            return;
        }
        if self.overlay != WeatherOverlay::RadarTimeline || m.frames.is_empty() {
            return;
        }
        let (z, min_x, max_x, min_y, max_y) = visible_tile_bounds(vp, 6);
        let max_i = self.active_timeline().map_or(m.frames.len() - 1, |tl| tl.max_idx()).min(m.frames.len() - 1);
        let len = max_i + 1;
        let cur = self.radar.frame_idx.min(max_i);
        // Full-loop monotonic priority queue (`cur` -> 23, `cur+1` -> 22, ..., `cur+21` -> 2):
        // Because `TileWorkers::pop_net_job` is strictly priority-ordered per origin lane (`LibreWxr`, `Iem`, `Eccc`),
        // enqueueing the full strip immediately NEVER delays `cur` on LibreWXR while keeping all 42 IEM+ECCC workers
        // busy downloading `+1h..+12h` in parallel from `t = 0 ms` (both paused AND playing)!
        // Frames more than ~3 hours ahead of the playhead wait until the view has been still for a moment (or playback
        // is running): a pause mid-pan must not download 12 hours of tiles for a viewport the user is already leaving.
        const NEAR_FRAMES: usize = 14;
        let far_ok = self.radar.playing || self.last_cam_move.elapsed() >= Duration::from_millis(350);
        self.strip_deferred = !far_ok;
        let key = (z, min_x, max_x, min_y, max_y, (cur << 10) | (len << 3) | (usize::from(far_ok) << 1) | usize::from(self.radar.playing));
        if self.last_prewarmed_strip == Some(key) {
            return;
        }
        let mut all_ok = true;
        for off in (0..len).take(if far_ok { len } else { NEAR_FRAMES }) {
            let idx = (cur + off) % len;
            let prio = PRIO_MAX.saturating_sub(off.min(21) as u8).max(2);
            all_ok &= self.prewarm_radar_frame(&m.host, &m.frames[idx], prio);
        }
        if all_ok {
            self.last_prewarmed_strip = Some(key);
        }
    }

    fn prewarm_adjacent_zooms(&mut self) {
        let st = if matches!(self.overlay, WeatherOverlay::WindFlow | WeatherOverlay::InfraredClouds)
            && self.base_style == BaseMapStyle::CleanLight
        {
            BaseMapStyle::AppleDark
        } else {
            self.base_style
        };
        let tz = effective_tile_zoom(self.zoom, st.spec().1);
        if tz >= st.spec().1 || self.zoom - (tz as f64) < 0.25 {
            return;
        }
        let (cx, cy) = tile_coord_at(self.center_lat, self.center_lon, tz);
        if self.last_adj_prewarm == Some((tz, cx, cy, st)) {
            return;
        }
        self.last_adj_prewarm = Some((tz, cx, cy, st));
        let Ok(mut p) = self.pool.lock() else { return };
        let ttl = Duration::from_secs(86_400 * 7);
        // Pre-cache only the single center tile one zoom level in when user is close to zooming in (1 tile vs 4!).
        let (nx, ny) = tile_coord_at(self.center_lat, self.center_lon, tz + 1);
        p.ensure(st.spec().2, (tz + 1, nx, ny), || st.tile_url(tz + 1, nx, ny), ttl, false, true);
    }

    pub fn fly_to(&mut self, lat: f64, lon: f64, zoom: Option<f64>) {
        let (tla, tlo, tz) = (lat.clamp(-85.0, 85.0), wrap_lon(lon), zoom.unwrap_or(self.zoom));
        (self.last_cam_move, self.last_prewarmed_strip, self.last_tick) = (Instant::now(), None, Instant::now());
        // Prewarm destination 3x3 basemap + labels right away so overview/city flights land with full crisp viewport!
        if let Ok(mut p) = self.pool.lock() {
            let dz = effective_tile_zoom(tz, self.base_style.spec().1);
            Self::prewarm_style_3x3(&mut p, self.base_style, dz, (tla, tlo), true);
        }
        let dist = (tla - self.center_lat).hypot(wrap_lon(tlo - self.center_lon));
        if dist < 0.002 && (tz - self.zoom).abs() < 0.006 {
            (self.center_lat, self.center_lon, self.zoom, self.motion) = (tla, tlo, tz, CamMotion::Idle);
            return;
        }
        if dist > 25.0 {
            (self.center_lat, self.center_lon, self.zoom, self.motion) = (tla, tlo, tz, CamMotion::Idle);
            let _ = self.pool.lock().map(|mut p| p.bump_epoch());
            return;
        }
        // Subtle Apple Maps parabolic zoom-out arc on regional flights; crisp cubic ease on short pan/zoom.
        let arc_dip = if dist > 2.5 { (dist.ln() * 0.50).clamp(0.30, 1.8) } else { 0.0 };
        let dur = if dist < 0.1 { 0.28 } else { (0.34 + dist.ln_1p() * 0.09).clamp(0.34, 0.68) as f32 };
        self.motion = CamMotion::Fly { from: (self.center_lat, self.center_lon, self.zoom), to: (tla, tlo, tz), arc_dip, t: 0.0, dur };
    }
    fn is_fast_flying(&self) -> bool {
        match self.motion {
            CamMotion::Fly { to: (la, lo, _), .. } => (la - self.center_lat).hypot(wrap_lon(lo - self.center_lon)) > 4.0,
            CamMotion::Fling(vx, vy) => vx.hypot(vy) > 650.0,
            _ => false,
        }
    }
    pub fn needs_animation_tick(&self, expanded: bool) -> bool {
        let (fading, pending_pins) = self.pool.lock().map_or((false, false), |p| (p.is_fading(), p.has_pending_gpu_pins()));
        let drop_anim = self.inspect.dropped_at.is_some_and(|t| t.elapsed() < Duration::from_millis(240));
        let toast_active = self.toast.is_some();
        let scrub = self.overlay.has_timeline() && (self.radar.play_pos - self.radar.target_pos).abs() > 0.006;
        fading
            || pending_pins
            || (expanded
                && (!matches!(self.motion, CamMotion::Idle)
                    || self.radar.playing
                    || scrub
                    || drop_anim
                    || toast_active
                    || self.overlay == WeatherOverlay::WindFlow))
    }

    pub fn tick(&mut self, bundles: &HashMap<String, WeatherBundle>, expanded: bool) -> bool {
        let mut dirty = self.pool.lock().is_ok_and(|mut p| p.drain_ready());
        while let Ok(ev) = self.rx.try_recv() {
            dirty = true;
            match ev {
                WorkerEvent::ManifestLoaded(m) => {
                    crate::storage::save_cached_manifest(&m);
                    let prev_ts = (self.overlay == WeatherOverlay::RadarTimeline)
                        .then(|| self.active_timeline().map(|tl| (tl.ts_at_pos(self.radar.play_pos), tl.live_idx)))
                        .flatten();
                    let was_live =
                        self.manifest.is_none() || prev_ts.is_some_and(|(_, oli)| !self.radar.playing && self.radar.frame_idx == oli);
                    let li = m.frames.iter().rposition(|f| !f.is_nowcast).unwrap_or(m.frames.len().saturating_sub(1));
                    self.set_manifest(m);
                    self.last_prewarmed_strip = None;
                    if self.overlay == WeatherOverlay::RadarTimeline {
                        if was_live {
                            self.radar.snap(li);
                        } else if let (Some((pts, _)), Some(tl)) = (prev_ts, self.active_timeline()) {
                            self.radar.snap_pos(tl.pos_for_ts(pts), tl.max_idx());
                        }
                    }
                    if expanded {
                        if self.overlay == WeatherOverlay::RadarTimeline {
                            self.prewarm_timeline_strip();
                        } else {
                            self.prewarm_live_radar();
                        }
                    }
                }
                WorkerEvent::GridSamples(keys, pts) => {
                    self.grid.ingest(keys, pts, |p| (p.lat, p.lon), self.center_lat, self.center_lon);
                    self.grid_gen = self.grid_gen.wrapping_add(1);
                }
                WorkerEvent::AqiSamples(keys, pts) => {
                    let any = !pts.is_empty();
                    self.aqi.ingest(keys, pts, |&(la, lo, _)| (la, lo), self.center_lat, self.center_lon);
                    if any {
                        self.grid_gen = self.grid_gen.wrapping_add(1);
                    }
                }
                WorkerEvent::PointInspected(seq, res) if seq == self.inspect.seq => {
                    self.inspect.loading = false;
                    (self.inspect.bundle, self.inspect.error) = match res {
                        Ok(b) => (Some(*b), None),
                        Err(e) => (None, Some(e)),
                    };
                }
                WorkerEvent::PointInspected(..) => {}
            }
        }
        if self.last_manifest.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
            self.spawn_manifest_fetch();
        }
        let dt = self.last_tick.elapsed().as_secs_f32().clamp(0.0, 0.08);
        self.last_tick = Instant::now();
        match self.motion {
            CamMotion::Idle => {}
            CamMotion::Fly { from: (fla, flo, fz), to: (tla, tlo, tz), arc_dip, mut t, dur } => {
                t = (t + dt.max(0.008) / dur.max(0.1)).min(1.0);
                // Apple Maps cubic-in-out ease in Web Mercator space + optional subtle parabolic zoom arc.
                let u = if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) * 0.5 } as f64;
                let (fwx, fwy) = lat_lon_to_world_px(fla, flo, 4.0);
                let (mut twx, twy) = lat_lon_to_world_px(tla, tlo, 4.0);
                let ws4 = 256.0 * 16.0;
                if twx - fwx > ws4 * 0.5 {
                    twx -= ws4;
                } else if fwx - twx > ws4 * 0.5 {
                    twx += ws4;
                }
                let (la, lo) = world_px_to_lat_lon((fwx + (twx - fwx) * u).rem_euclid(ws4), fwy + (twy - fwy) * u, 4.0);
                let z = (fz + (tz - fz) * u - arc_dip * (std::f64::consts::PI * u).sin()).clamp(2.2, 16.0);
                (self.center_lat, self.center_lon, self.zoom, self.last_cam_move, dirty) =
                    (la.clamp(-85.0, 85.0), wrap_lon(lo), z, Instant::now(), true);
                self.motion = if t >= 1.0 {
                    (self.center_lat, self.center_lon, self.zoom) = (tla, tlo, tz);
                    CamMotion::Idle
                } else {
                    CamMotion::Fly { from: (fla, flo, fz), to: (tla, tlo, tz), arc_dip, t, dur }
                };
            }
            CamMotion::Zoom(tz, ago, apx, vp) => {
                let prev_z = self.zoom;
                self.zoom += (tz - self.zoom) * (1.0 - (-16.5 * dt.max(0.008)).exp()) as f64;
                if (tz - self.zoom).abs() < 0.003 {
                    (self.zoom, self.motion) = (tz, CamMotion::Idle);
                }
                let scale = 2.0_f32.powf((self.zoom - prev_z) as f32);
                self.zoom_wind_particles(scale, (apx.x / vp.width.max(1.0), apx.y / vp.height.max(1.0)));
                let (awx, awy) = lat_lon_to_world_px(ago.0, ago.1, self.zoom);
                let (cla, clo) =
                    world_px_to_lat_lon(awx - (apx.x - vp.width * 0.5) as f64, awy - (apx.y - vp.height * 0.5) as f64, self.zoom);
                (self.center_lat, self.center_lon, self.last_cam_move, dirty) =
                    (cla.clamp(-85.0, 85.0), wrap_lon(clo), Instant::now(), true);
            }
            CamMotion::Fling(vx, vy) => {
                let sdt = dt.max(0.008) as f64;
                self.apply_pan_px(vx * sdt, vy * sdt);
                // Effortless Apple Maps inertial coast (gentle exponential friction).
                let dec = (-4.0 * sdt).exp();
                let (nvx, nvy) = (vx * dec, vy * dec);
                self.motion = if nvx.hypot(nvy) > 5.0 { CamMotion::Fling(nvx, nvy) } else { CamMotion::Idle };
                (self.last_cam_move, dirty) = (Instant::now(), true);
            }
        }
        if !expanded {
            return dirty;
        }
        if self.toast.as_ref().is_some_and(|(_, t)| t.elapsed() >= Duration::from_millis(1900)) {
            self.toast = None;
            dirty = true;
        }
        let inflight = self.pool.lock().map(|p| p.in_flight.len()).unwrap_or(0);
        let idle_settled = matches!(self.motion, CamMotion::Idle) && self.last_cam_move.elapsed() >= Duration::from_millis(180);
        if idle_settled && inflight <= 6 {
            self.prewarm_adjacent_zooms();
        }
        let radar_idle = matches!(self.motion, CamMotion::Idle) && self.last_cam_move.elapsed() >= Duration::from_millis(40);
        if matches!(self.overlay, WeatherOverlay::RadarTimeline | WeatherOverlay::InfraredClouds) && (self.radar.playing || radar_idle) {
            self.prewarm_timeline_strip();
        }
        if self.radar.playing
            && let Some(tl) = self.active_timeline()
            && tl.times.len() > 1
        {
            let max_i = tl.max_idx();
            let base = self.radar.play_pos.clamp(0.0, max_i as f32);
            let ci = (base.floor() as usize).min(max_i);
            if ci >= max_i {
                self.radar.snap(max_i);
                self.radar.loop_hold += dt;
                if self.radar.loop_hold >= 0.55 {
                    self.radar.loop_hold = 0.0;
                    self.radar.snap(0);
                }
            } else {
                self.radar.loop_hold = 0.0;
                let ni = (ci + 1).min(max_i);
                let n2 = (ci + 2).min(max_i);
                // Smooth media-player readiness & hysteresis buffer gate:
                // - A frame is playback-ready as soon as `all_ready || visually_covered || core_ready` (100% of core
                //   viewport settled & >=80% total settled; any far-outer corner sliver is filled seamlessly via
                //   `temporal_fallbacks` with ZERO stop-start edge-tile stutter!).
                // - Only pause to buffer when the CORE viewport is actually missing tiles; once buffering, hold cleanly
                //   until `ci` + `ni` (plus `n2` during short initial buffer) are ready so playback resumes smoothly
                //   without start-stop chugging, capped at 0.85s with `skip_stall_idx` preventing double-stalls!
                let full_ready = FrameReadiness { all_ready: true, visually_covered: true, core_ready: true };
                let (r0, r1, r2) = if self.overlay == WeatherOverlay::RadarTimeline
                    && let Some(m) = &self.manifest
                    && let Ok(mut pool) = self.pool.lock()
                {
                    let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
                    let temp_pair = |idx: usize| -> [Option<u32>; 2] {
                        let same = |k: Option<usize>| {
                            let f = m.frames.get(idx)?;
                            let nf = m.frames.get(k?)?;
                            (matches!(nf.source, RadarFrameSource::NoaaHrrr(..)) == matches!(f.source, RadarFrameSource::NoaaHrrr(..)))
                                .then(|| self.manifest_ns_ids.get(k?).and_then(|a| a[0]))
                                .flatten()
                        };
                        [same(idx.checked_sub(1)), same((idx + 1 < m.frames.len()).then_some(idx + 1))]
                    };
                    let r0 = m.frames.get(ci).map_or(full_ready, |f| {
                        radar_frame_readiness_ids(f, self.manifest_ns_ids.get(ci).copied(), temp_pair(ci), &pool, vp)
                    });
                    let r1 = m.frames.get(ni).map_or(full_ready, |f| {
                        radar_frame_readiness_ids(f, self.manifest_ns_ids.get(ni).copied(), temp_pair(ni), &pool, vp)
                    });
                    let r2 = if n2 != ni {
                        m.frames.get(n2).map_or(full_ready, |f| {
                            radar_frame_readiness_ids(f, self.manifest_ns_ids.get(n2).copied(), temp_pair(n2), &pool, vp)
                        })
                    } else {
                        full_ready
                    };
                    if !r0.all_ready
                        && let Some(f) = m.frames.get(ci)
                    {
                        let _ = ensure_radar_frame_visible(&mut pool, &m.host, f, vp, PRIO_MAX, true);
                    }
                    if !r1.all_ready
                        && let Some(f) = m.frames.get(ni)
                    {
                        let _ = ensure_radar_frame_visible(&mut pool, &m.host, f, vp, PRIO_MAX - 1, true);
                    }
                    (r0, r1, r2)
                } else if self.overlay == WeatherOverlay::InfraredClouds
                    && let Some(m) = &self.manifest
                    && !m.satellite.is_empty()
                    && let Ok(mut pool) = self.pool.lock()
                {
                    let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
                    let sat_pair = |idx: usize| -> [Option<u32>; 2] {
                        [idx.checked_sub(1).and_then(|k| self.sat_ns_ids.get(k).copied()), self.sat_ns_ids.get(idx + 1).copied()]
                    };
                    let r0 = self.sat_ns_ids.get(ci).copied().map_or(full_ready, |id| sat_frame_readiness_id(id, sat_pair(ci), &pool, vp));
                    let r1 = self.sat_ns_ids.get(ni).copied().map_or(full_ready, |id| sat_frame_readiness_id(id, sat_pair(ni), &pool, vp));
                    let r2 = if n2 != ni {
                        self.sat_ns_ids.get(n2).copied().map_or(full_ready, |id| sat_frame_readiness_id(id, sat_pair(n2), &pool, vp))
                    } else {
                        full_ready
                    };
                    if !r0.all_ready
                        && let Some(f) = m.satellite.get(ci)
                    {
                        let _ = ensure_sat_frame_visible(&mut pool, &m.host, f, vp, PRIO_MAX, true);
                    }
                    if !r1.all_ready
                        && let Some(f) = m.satellite.get(ni)
                    {
                        let _ = ensure_sat_frame_visible(&mut pool, &m.host, f, vp, PRIO_MAX - 1, true);
                    }
                    (r0, r1, r2)
                } else {
                    (full_ready, full_ready, full_ready)
                };
                let playable = |r: FrameReadiness| r.all_ready || r.visually_covered || r.core_ready;
                let r0_ok = playable(r0) || self.radar.skip_stall_idx == Some(ci);
                let r1_ok = playable(r1) || self.radar.skip_stall_idx == Some(ni);
                let r2_ok = playable(r2);
                let frac = (base - ci as f32).clamp(0.0, 1.0);
                let trigger_wait = !r0_ok || (frac >= 0.90 && !r1_ok);
                if trigger_wait {
                    self.radar.buffering = true;
                } else if self.radar.buffering {
                    let runway_ready = r0_ok && r1_ok && (r2_ok || self.radar.stall_secs >= 0.45);
                    if runway_ready {
                        self.radar.buffering = false;
                        self.radar.stall_secs = 0.0;
                    }
                }
                const STALL_CAP: f32 = 0.85;
                if self.radar.buffering && self.radar.stall_secs < STALL_CAP {
                    self.radar.stall_secs += dt;
                } else {
                    if self.radar.buffering {
                        // Safety cap reached: skip re-stalling on the exact same frame when stepping across `ni`!
                        self.radar.skip_stall_idx = Some(if !r0_ok { ci } else { ni });
                        self.radar.buffering = false;
                        self.radar.stall_secs = 0.0;
                    }
                    let dur = if self.overlay == WeatherOverlay::RadarTimeline {
                        let mins = (tl.times[ni] - tl.times[ci]).max(300) as f32 / 60.0;
                        if mins <= 15.0 {
                            0.38
                        } else if mins <= 30.0 {
                            0.48
                        } else {
                            0.58
                        }
                    } else if self.overlay == WeatherOverlay::InfraredClouds {
                        // 12 hourly global IR frames (~6.2s per smooth loop at 1×).
                        0.52
                    } else {
                        // Uniform 12-hour forecast playback (~6.0s per loop at 1×).
                        0.25
                    } / self.playback_speed.clamp(0.5, 2.0);
                    let np = (base + dt / dur).min(max_i as f32);
                    let new_ci = (np.floor() as usize).min(max_i);
                    if new_ci != ci && self.radar.skip_stall_idx == Some(ci) {
                        self.radar.skip_stall_idx = None;
                    }
                    (self.radar.play_pos, self.radar.target_pos, self.radar.frame_idx) = (np, np, new_ci);
                }
            }
            dirty = true;
        } else if self.overlay.has_timeline() {
            (self.radar.stall_secs, self.radar.buffering, self.radar.skip_stall_idx) = (0.0, false, None);
            let diff = self.radar.target_pos - self.radar.play_pos;
            if diff.abs() > 0.006 {
                if diff.abs() > 1.5 {
                    // Direct jump (+3h / Now / timeline click): snap directly so we never flash an unrequested intermediate frame!
                    self.radar.play_pos = self.radar.target_pos;
                } else {
                    self.radar.play_pos += (self.radar.target_pos - self.radar.play_pos) * (1.0 - (-15.0 * dt.max(0.008)).exp());
                    if (self.radar.target_pos - self.radar.play_pos).abs() <= 0.006 {
                        self.radar.play_pos = self.radar.target_pos;
                    }
                }
                dirty = true;
            } else {
                self.radar.play_pos = self.radar.target_pos;
            }
        }
        let need_grid = matches!(
            self.overlay,
            WeatherOverlay::Temperature | WeatherOverlay::AirQuality | WeatherOverlay::WindFlow | WeatherOverlay::Pressure
        );
        if need_grid {
            self.poll_grid_samples();
        }
        if self.overlay == WeatherOverlay::WindFlow {
            self.step_wind_particles(bundles, dt);
            dirty = true;
        }
        dirty
    }

    fn poll_grid_samples(&mut self) {
        if self.last_grid_poll.is_some_and(|t| t.elapsed() < Duration::from_millis(650)) {
            return;
        }
        self.last_grid_poll = Some(Instant::now());
        let (span, is_aqi, mut req) = (overlays::grid_sample_span_deg(self.zoom), self.overlay == WeatherOverlay::AirQuality, Vec::new());
        // 4x5 adaptive grid (20 points in one Open-Meteo batch, -33% payload & faster RBF) with 0.1° snapping.
        let snap_deg = (span / 8.0).clamp(0.1, 0.5);
        for gy in 0..4 {
            for gx in 0..5 {
                let raw_lat = (self.center_lat + ((gy as f64 - 1.5) / 3.0) * span * 1.15).clamp(-80.0, 80.0);
                let raw_lon = wrap_lon(self.center_lon + ((gx as f64 - 2.0) / 4.0) * span * 1.5);
                let sla = (raw_lat / snap_deg).round() * snap_deg;
                let slo = wrap_lon((raw_lon / snap_deg).round() * snap_deg);
                let q = quantize_coord(sla, slo);
                let ok = if is_aqi { self.aqi.claim(q) } else { self.grid.claim(q) };
                let _ = ok.then(|| req.push((q.0 as f64 * 0.1, q.1 as f64 * 0.1)));
            }
        }
        if req.is_empty() {
            return;
        }
        let (c, tx, wk) = (self.client.clone(), self.pool.lock().expect("pool").tx.clone(), Arc::clone(&self.wake));
        let keys: Vec<_> = req.iter().map(|&(la, lo)| quantize_coord(la, lo)).collect();
        std::thread::spawn(move || {
            let ev = if is_aqi {
                WorkerEvent::AqiSamples(keys, c.fetch_aqi_grid_batch(&req))
            } else {
                WorkerEvent::GridSamples(keys, c.fetch_grid_batch(&req))
            };
            let _ = tx.send(ev);
            wk.notify_one();
        });
    }

    pub(super) fn all_weather_points(&self, bundles: &HashMap<String, WeatherBundle>, ts: Option<i64>) -> Vec<GridWeatherPoint> {
        let mut pts: Vec<_> = self.grid.data.values().map(|p| p.at_timestamp(ts)).collect();
        for b in bundles.values() {
            let v = if let Some(t) = ts
                && !b.hourly.is_empty()
            {
                let i = b.hourly.partition_point(|h| h.timestamp_unix < t);
                let (h0, h1) = (&b.hourly[i.saturating_sub(1).min(b.hourly.len() - 1)], &b.hourly[i.min(b.hourly.len() - 1)]);
                let f = ((t - h0.timestamp_unix) as f32 / (h1.timestamp_unix - h0.timestamp_unix).max(1) as f32).clamp(0.0, 1.0);
                let ((u0, v0), (u1, v1)) = (wind_uv(h0.wind_speed_mps, h0.wind_dir_deg), wind_uv(h1.wind_speed_mps, h1.wind_dir_deg));
                let (wu, wv, l) = (u0 + (u1 - u0) * f, v0 + (v1 - v0) * f, |a: f64, c: f64| (a + (c - a) * f as f64) as f32);
                let (Some(p0), Some(p1)) = (h0.pressure_hpa, h1.pressure_hpa) else { continue };
                [l(h0.temp_c, h1.temp_c), l(h0.precip_mm, h1.precip_mm), wu, wv, wu.hypot(wv), l(p0, p1)]
            } else {
                // A bundle is only a sample point when every value is real; calm wind is a zero vector of no direction.
                let c = &b.current;
                let (Some(dir), Some(prs), Some(pr)) =
                    (c.wind_dir_deg.or((c.wind_speed_mps == 0.0).then_some(0.0)), c.pressure_hpa, c.precip_mm)
                else {
                    continue;
                };
                let (u, v) = wind_uv(c.wind_speed_mps, dir);
                [c.temp_c as f32, pr as f32, u, v, c.wind_speed_mps as f32, prs as f32]
            };
            pts.push(GridWeatherPoint::stationary(b.location.lat, b.location.lon, v));
        }
        pts
    }
    pub(super) fn all_aqi_points(&self, bundles: &HashMap<String, WeatherBundle>) -> Vec<(f64, f64, f64)> {
        self.aqi
            .data
            .values()
            .copied()
            .chain(bundles.values().filter_map(|b| b.air_quality.as_ref().map(|a| (b.location.lat, b.location.lon, a.us_aqi as f64))))
            .collect()
    }
    fn jump_radar_idx(&mut self, idx: usize, with_next: bool) {
        let max_i = self.active_timeline().map_or(idx, |tl| tl.max_idx());
        let clamped = idx.min(max_i);
        (
            self.radar.playing,
            self.radar.loop_hold,
            self.radar.stall_secs,
            self.radar.buffering,
            self.radar.skip_stall_idx,
            self.radar.frame_idx,
            self.radar.target_pos,
        ) = (false, 0.0, 0.0, false, None, clamped, clamped as f32);
        if self.overlay == WeatherOverlay::RadarTimeline
            && let Some(m) = &self.manifest
        {
            for i in [Some(clamped), with_next.then_some(clamped + 1)].into_iter().flatten() {
                let _ = m.frames.get(i).map(|f| self.prewarm_radar_frame(&m.host, f, PRIO_MAX));
            }
        } else if self.overlay == WeatherOverlay::InfraredClouds
            && let Some(m) = &self.manifest
            && let Ok(mut p) = self.pool.lock()
        {
            let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
            for i in [Some(clamped), with_next.then_some(clamped + 1)].into_iter().flatten() {
                let _ = m.satellite.get(i).map(|f| ensure_sat_frame_visible(&mut p, &m.host, f, vp, PRIO_MAX, false));
            }
        }
    }
    fn clear_inspector(&mut self) {
        self.inspect = InspectState { seq: self.inspect.seq.wrapping_add(1), ..Default::default() };
    }

    pub fn handle_msg(
        &mut self, msg: MapMsg, saved: &[Location], active_id: Option<&str>, settings: &Settings,
    ) -> Option<MapExternalAction> {
        match msg {
            MapMsg::ExpandMap => {
                self.motion = CamMotion::Idle;
                if let Some(l) = active_id.and_then(|id| saved.iter().find(|l| l.id == id)) {
                    (self.center_lat, self.center_lon) = (l.lat, l.lon);
                    let vp = self.viewport_size();
                    let target_sz = Size::new(vp.width.max(1180.0), vp.height.max(760.0));
                    self.prewarm_viewport_base(self.base_style, self.zoom, (l.lat, l.lon), target_sz);
                }
                (self.last_prewarmed_strip, self.last_grid_poll) = (None, None);
                if matches!(self.overlay, WeatherOverlay::RadarTimeline | WeatherOverlay::InfraredClouds) {
                    self.prewarm_timeline_strip();
                } else {
                    self.prewarm_live_radar();
                }
                if self.grid.data.is_empty() {
                    self.poll_grid_samples();
                }
                return Some(MapExternalAction::SetExpanded(true));
            }
            MapMsg::CloseMap => {
                (
                    self.motion,
                    self.show_layer_menu,
                    self.full_screen,
                    self.context_menu,
                    self.radar.playing,
                    self.radar.loop_hold,
                    self.radar.stall_secs,
                    self.radar.buffering,
                    self.radar.skip_stall_idx,
                ) = (CamMotion::Idle, false, false, None, false, 0.0, 0.0, false, None);
                return Some(MapExternalAction::SetExpanded(false));
            }
            MapMsg::ToggleFullScreen => {
                self.full_screen = !self.full_screen;
                self.context_menu = None;
                self.last_prewarmed_strip = None;
                let vp = self.viewport_size();
                let target_sz = if self.full_screen { Size::new(vp.width.max(1220.0), vp.height.max(820.0)) } else { vp };
                self.prewarm_viewport_base(self.base_style, self.zoom, (self.center_lat, self.center_lon), target_sz);
            }
            MapMsg::ToggleLayerMenu => {
                self.show_layer_menu = !self.show_layer_menu;
                self.context_menu = None;
            }
            MapMsg::ToggleLegend => self.legend_collapsed = !self.legend_collapsed,
            MapMsg::ToggleTerminator => self.show_terminator = !self.show_terminator,
            MapMsg::SetOverlayOpacity(v) => self.overlay_opacity = v.clamp(0.4, 1.0),
            MapMsg::SelectOverlay(ov) => {
                let prev_tl = self.active_timeline().map(|tl| {
                    let ts = tl.ts_at_pos(self.radar.play_pos);
                    (ts, self.radar.play_pos <= tl.live_idx as f32 + 0.02 && self.radar.play_pos >= tl.live_idx as f32 - 0.02)
                });
                (self.overlay, self.show_layer_menu, self.context_menu, self.last_grid_poll) = (ov, false, None, None);
                (self.radar.loop_hold, self.radar.stall_secs, self.radar.buffering, self.radar.skip_stall_idx) = (0.0, 0.0, false, None);
                self.last_rendered_radar_idx.set(None);
                if !ov.has_timeline() {
                    self.radar.playing = false;
                }
                if ov == WeatherOverlay::RadarTimeline {
                    self.last_prewarmed_strip = None;
                    self.prewarm_timeline_strip();
                } else {
                    if matches!(ov, WeatherOverlay::WindFlow | WeatherOverlay::InfraredClouds)
                        && self.base_style == BaseMapStyle::CleanLight
                        && let Ok(mut p) = self.pool.lock()
                    {
                        let z = effective_tile_zoom(self.zoom, 16);
                        Self::prewarm_style_2x2(&mut p, BaseMapStyle::AppleDark, z, (self.center_lat, self.center_lon), true);
                    }
                    if matches!(
                        ov,
                        WeatherOverlay::Temperature | WeatherOverlay::AirQuality | WeatherOverlay::WindFlow | WeatherOverlay::Pressure
                    ) {
                        self.poll_grid_samples();
                    }
                    if ov == WeatherOverlay::InfraredClouds && self.manifest.as_ref().is_some_and(|m| !m.satellite.is_empty()) {
                        self.last_prewarmed_strip = None;
                        self.prewarm_timeline_strip();
                    } else if matches!(ov, WeatherOverlay::NexradUsRadar | WeatherOverlay::InfraredClouds)
                        && let Ok(mut p) = self.pool.lock()
                    {
                        let (ns, mz, slug) = if ov == WeatherOverlay::NexradUsRadar {
                            ("iem_nexrad", 10, "nexrad-n0q-900913")
                        } else if self.center_lon < -105.0 {
                            // Full-disk GOES-West IR eliminates the GOES-East CONUS western/northern cutout!
                            ("iem_goes_ir_w", 6, "goes_west_fulldisk_ch13")
                        } else {
                            ("iem_goes_ir_e", 6, "goes_east_fulldisk_ch13")
                        };
                        let z = effective_tile_zoom(self.zoom, mz);
                        let (cx, cy) = tile_coord_at(self.center_lat, self.center_lon, z);
                        let n = 1_u32 << z;
                        // Push outer tiles first so high-priority LIFO workers pop viewport-center tiles FIRST!
                        for &(dx, dy) in [
                            (0_i64, 0_i64),
                            (-1, 0),
                            (1, 0),
                            (0, -1),
                            (0, 1),
                            (-1, -1),
                            (1, -1),
                            (-1, 1),
                            (1, 1),
                            (-2, 0),
                            (2, 0),
                            (-2, -1),
                            (-2, 1),
                            (2, -1),
                            (2, 1),
                            (0, -2),
                            (-1, -2),
                            (1, -2),
                            (-2, -2),
                            (2, -2),
                        ]
                        .iter()
                        .rev()
                        {
                            let (tx, ty) = ((cx as i64 + dx).rem_euclid(n as i64) as u32, (cy as i64 + dy).clamp(0, n as i64 - 1) as u32);
                            p.ensure(
                                ns,
                                (z, tx, ty),
                                || format!("https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/{slug}/{z}/{tx}/{ty}.png"),
                                Duration::from_secs(300),
                                true,
                                false,
                            );
                        }
                    }
                }
                if let Some(tl) = self.active_timeline() {
                    match prev_tl {
                        Some((_, true)) | None => self.radar.snap(tl.live_idx),
                        Some((pts, false)) => {
                            if pts <= tl.live_ts() && tl.live_idx == 0 {
                                self.radar.snap(0);
                            } else {
                                self.radar.snap_pos(tl.pos_for_ts(pts), tl.max_idx());
                            }
                        }
                    }
                }
            }
            MapMsg::SelectBaseStyle(st) => {
                if st != self.base_style {
                    self.prev_base_style = Some((self.base_style, Instant::now()));
                }
                (self.base_style, self.user_picked_style, self.show_layer_menu, self.context_menu, self.last_adj_prewarm) =
                    (st, true, false, None, None);
                let _ = self.pool.lock().map(|mut p| p.cancel_base_tiles());
                self.prewarm_region(self.center_lat, self.center_lon, true, false);
            }
            MapMsg::PanStart => (self.show_layer_menu, self.context_menu, self.motion) = (false, None, CamMotion::Idle),
            MapMsg::PanPixels(dx, dy) => {
                (self.show_layer_menu, self.context_menu, self.motion, self.last_cam_move) = (false, None, CamMotion::Idle, Instant::now());
                self.apply_pan_px(dx as f64, dy as f64);
            }
            MapMsg::PanFling(vx, vy) => {
                let (vx, vy) = (vx as f64, vy as f64);
                let mag = vx.hypot(vy);
                if mag > 45.0 {
                    let c = mag.min(2400.0) / mag;
                    (self.motion, self.last_tick) = (CamMotion::Fling(vx * c, vy * c), Instant::now());
                }
            }
            MapMsg::WheelZoom(p, sz, dz) => {
                self.context_menu = None;
                self.cursor_zoom_smooth(p, sz, dz);
            }
            MapMsg::DragZoom(p, sz, dz) => {
                (self.show_layer_menu, self.context_menu, self.motion, self.last_cam_move) = (false, None, CamMotion::Idle, Instant::now());
                let prev_z = self.zoom;
                let ago = project_screen_to_lat_lon(p, self.center_lat, self.center_lon, prev_z, Rectangle::with_size(sz));
                self.zoom = (prev_z + dz).clamp(2.2, 16.0);
                let scale = 2.0_f32.powf((self.zoom - prev_z) as f32);
                self.zoom_wind_particles(scale, (p.x / sz.width.max(1.0), p.y / sz.height.max(1.0)));
                let (awx, awy) = lat_lon_to_world_px(ago.0, ago.1, self.zoom);
                let (cla, clo) = world_px_to_lat_lon(awx - (p.x - sz.width * 0.5) as f64, awy - (p.y - sz.height * 0.5) as f64, self.zoom);
                (self.center_lat, self.center_lon) = (cla.clamp(-85.0, 85.0), wrap_lon(clo));
            }
            MapMsg::DoubleClickZoom(p, sz) => {
                (self.show_layer_menu, self.context_menu) = (false, None);
                self.clear_inspector();
                self.cursor_zoom_smooth(p, sz, 1.15);
            }
            MapMsg::ZoomStep(dz) => {
                self.context_menu = None;
                let z = match self.motion {
                    CamMotion::Fly { to: (_, _, z), .. } | CamMotion::Zoom(z, ..) => z,
                    _ => self.zoom,
                };
                // Continuous exponential center-focal zoom so rapid + / - taps accelerate smoothly without velocity resets.
                let sz = Size::new(2.0, 2.0);
                (self.last_cam_move, self.last_tick) = (Instant::now(), Instant::now());
                self.motion = CamMotion::Zoom((z + dz).clamp(2.2, 16.0), (self.center_lat, self.center_lon), Point::new(1.0, 1.0), sz);
            }
            MapMsg::RecenterActive => {
                self.context_menu = None;
                if let Some(l) = active_id.and_then(|id| saved.iter().find(|l| l.id == id)) {
                    let near = (l.lat - self.center_lat).hypot(wrap_lon(l.lon - self.center_lon)) < 0.05;
                    // Apple Maps Locate toggle: cycles regional overview (6.8) <-> close-up city detail (10.5) when centered.
                    let target_z = if near && self.zoom < 9.2 { 10.5 } else { 6.8 };
                    self.fly_to(l.lat, l.lon, Some(target_z));
                }
            }
            MapMsg::FitAllLocations => {
                (self.show_layer_menu, self.context_menu) = (false, None);
                let vp = self.viewport_size();
                let target_sz = if self.full_screen { Size::new(vp.width.max(1220.0), vp.height.max(820.0)) } else { vp };
                if let Some((la, lo, z)) = compute_overview_camera(saved, target_sz) {
                    self.prewarm_viewport_base(self.base_style, z, (la, lo), target_sz);
                    self.fly_to(la, lo, Some(z));
                }
            }
            MapMsg::ToggleRadarPlay => {
                if self.overlay.has_timeline() {
                    (
                        self.radar.playing,
                        self.radar.loop_hold,
                        self.radar.stall_secs,
                        self.radar.buffering,
                        self.radar.skip_stall_idx,
                        self.last_tick,
                    ) = (!self.radar.playing, 0.0, 0.0, false, None, Instant::now());
                    if self.radar.playing {
                        if let Some(tl) = self.active_timeline()
                            && self.radar.play_pos >= tl.max_idx() as f32 - 0.05
                        {
                            self.radar.snap(0);
                        }
                        if matches!(self.overlay, WeatherOverlay::RadarTimeline | WeatherOverlay::InfraredClouds) {
                            self.last_prewarmed_strip = None;
                            self.prewarm_timeline_strip();
                        }
                    }
                }
            }
            MapMsg::CyclePlaybackSpeed => {
                self.playback_speed = if self.playback_speed < 0.9 {
                    1.0
                } else if self.playback_speed < 1.5 {
                    2.0
                } else {
                    0.5
                };
            }
            MapMsg::TogglePrecipRange => {
                if self.overlay == WeatherOverlay::RadarTimeline {
                    let prev_ts = self.active_timeline().map(|tl| tl.ts_at_pos(self.radar.play_pos));
                    self.precip_short_range = !self.precip_short_range;
                    self.last_prewarmed_strip = None;
                    self.last_rendered_radar_idx.set(None);
                    if let Some(tl) = self.active_timeline() {
                        let target = prev_ts.map_or(tl.live_idx as f32, |ts| tl.pos_for_ts(ts));
                        self.radar.snap_pos(target, tl.max_idx());
                    }
                    self.prewarm_timeline_strip();
                }
            }
            MapMsg::StepTimeline(d) => {
                if let Some(tl) = self.active_timeline() {
                    let mx = tl.max_idx() as i32;
                    let next = ((self.radar.target_pos.round() as i32).clamp(0, mx) + d).clamp(0, mx) as usize;
                    self.jump_radar_idx(next, d > 0);
                }
            }
            MapMsg::ScrubRadar(idx) => self.jump_radar_idx(idx, true),
            MapMsg::ScrubRadarPos(pos) => {
                (self.radar.playing, self.radar.loop_hold, self.radar.stall_secs, self.radar.buffering, self.radar.skip_stall_idx) =
                    (false, 0.0, 0.0, false, None);
                if let Some(tl) = self.active_timeline() {
                    let mx = tl.max_idx();
                    let c = pos.clamp(0.0, mx as f32);
                    let (i0, i1) = ((c.floor() as usize).min(mx), ((c.floor() as usize) + 1).min(mx));
                    (self.radar.play_pos, self.radar.target_pos, self.radar.frame_idx) = (c, c, (c.round() as usize).min(mx));
                    if self.overlay == WeatherOverlay::RadarTimeline
                        && let Some(m) = &self.manifest
                    {
                        let _ = m.frames.get(i0).map(|f| self.prewarm_radar_frame(&m.host, f, PRIO_MAX));
                        let _ = (i1 != i0).then(|| m.frames.get(i1).map(|f| self.prewarm_radar_frame(&m.host, f, PRIO_MAX - 1)));
                    } else if self.overlay == WeatherOverlay::InfraredClouds
                        && let Some(m) = &self.manifest
                        && let Ok(mut p) = self.pool.lock()
                    {
                        let vp = (self.viewport_size(), self.center_lat, self.center_lon, self.zoom);
                        let _ = m.satellite.get(i0).map(|f| ensure_sat_frame_visible(&mut p, &m.host, f, vp, PRIO_MAX, false));
                        let _ = (i1 != i0)
                            .then(|| m.satellite.get(i1).map(|f| ensure_sat_frame_visible(&mut p, &m.host, f, vp, PRIO_MAX - 1, false)));
                    }
                }
            }
            MapMsg::JumpToLive => {
                if let Some(tl) = self.active_timeline() {
                    self.jump_radar_idx(tl.live_idx, false);
                }
            }
            MapMsg::ClickMap(pt, sz) => {
                if self.context_menu.take().is_some() {
                    return None;
                }
                if self.show_layer_menu {
                    self.show_layer_menu = false;
                    return None;
                }
                let rect = Rectangle::with_size(sz);
                let coords: Vec<_> = saved.iter().map(|l| (l.lat, l.lon, active_id == Some(l.id.as_str()))).collect();
                for (idx, _, dc) in declutter_points(&coords, self.center_lat, self.center_lon, self.zoom, rect) {
                    if (dc.x - pt.x).hypot(dc.y - pt.y) <= if coords[idx].2 { 24.0 } else { 18.0 } {
                        self.fly_to(saved[idx].lat, saved[idx].lon, Some(self.zoom.max(6.8)));
                        return Some(MapExternalAction::SelectSaved(saved[idx].id.clone()));
                    }
                }
                let (la, lo) = project_screen_to_lat_lon(pt, self.center_lat, self.center_lon, self.zoom, rect);
                self.trigger_inspect(la, lo, settings);
            }
            MapMsg::OpenContextMenu(pt, sz) => {
                self.show_layer_menu = false;
                let (la, lo) = project_screen_to_lat_lon(pt, self.center_lat, self.center_lon, self.zoom, Rectangle::with_size(sz));
                self.context_menu = Some(ContextMenuState { screen: pt, vp: sz, lat: la, lon: lo });
            }
            MapMsg::CloseContextMenu => self.context_menu = None,
            MapMsg::ContextInspect => {
                if let Some(cm) = self.context_menu.take() {
                    self.trigger_inspect(cm.lat, cm.lon, settings);
                }
            }
            MapMsg::ContextCenter => {
                if let Some(cm) = self.context_menu.take() {
                    self.fly_to(cm.lat, cm.lon, None);
                }
            }
            MapMsg::ContextZoom(dz) => {
                if let Some(cm) = self.context_menu.take() {
                    self.cursor_zoom_smooth(cm.screen, cm.vp, dz);
                }
            }
            MapMsg::ContextCopyCoords => {
                if let Some(cm) = self.context_menu.take() {
                    let s = format!("{:.4}, {:.4}", cm.lat, cm.lon);
                    self.toast = Some(("Coordinates Copied".into(), Instant::now()));
                    return Some(MapExternalAction::CopyText(s));
                }
            }
            MapMsg::CloseInspector => self.clear_inspector(),
            MapMsg::SaveInspectedLocation => {
                return self.inspect.bundle.take().map(|b| {
                    (self.inspect.coord, self.inspect.error) = (None, None);
                    MapExternalAction::SaveLocation(b.location)
                });
            }
            MapMsg::HudConsume => {}
        }
        None
    }

    fn apply_pan_px(&mut self, dx: f64, dy: f64) {
        let (ws, (wx, wy)) = (256.0 * 2.0_f64.powf(self.zoom), lat_lon_to_world_px(self.center_lat, self.center_lon, self.zoom));
        let (la, lo) = world_px_to_lat_lon(wx - dx, (wy - dy).clamp(0.0, ws), self.zoom);
        (self.center_lat, self.center_lon) = (la.clamp(-85.0, 85.0), wrap_lon(lo));
        self.shift_wind_particles(dx as f32, dy as f32);
    }
    fn cursor_zoom_smooth(&mut self, p: Point, sz: Size, dz: f64) {
        (self.last_cam_move, self.last_tick) = (Instant::now(), Instant::now());
        let ago = project_screen_to_lat_lon(p, self.center_lat, self.center_lon, self.zoom, Rectangle::with_size(sz));
        let bz = match self.motion {
            CamMotion::Zoom(z, _, ap, _) if (ap.x - p.x).hypot(ap.y - p.y) < 48.0 => z,
            _ => self.zoom,
        };
        self.motion = CamMotion::Zoom((bz + dz).clamp(2.2, 16.0), ago, p, sz);
    }
    fn trigger_inspect(&mut self, lat: f64, lon: f64, settings: &Settings) {
        let ((cla, wlo), seq) = ((lat.clamp(-85.0, 85.0), wrap_lon(lon)), self.inspect.seq.wrapping_add(1));
        self.inspect = InspectState {
            coord: Some((cla, wlo)),
            dropped_at: Some(Instant::now()),
            loading: true,
            bundle: self.inspect.bundle.take(),
            error: None,
            seq,
        };
        let (c, tx, wk, prov) =
            (self.client.clone(), self.pool.lock().expect("pool").tx.clone(), Arc::clone(&self.wake), settings.provider);
        std::thread::spawn(move || {
            let stub = Location::new(format!("{cla:.2}°, {wlo:.2}°"), "", "", "", cla, wlo, "");
            let (loc, res) = std::thread::scope(|s| {
                let g = s.spawn(|| c.reverse_geocode(cla, wlo));
                (g.join().ok(), c.fetch_weather_quick(&stub, prov))
            });
            let out = res.map(|mut b| {
                if let Some(mut l) = loc {
                    if l.timezone.is_empty() {
                        l.timezone = b.location.timezone.clone();
                    }
                    b.location = l;
                }
                Box::new(b)
            });
            let _ = tx.send(WorkerEvent::PointInspected(seq, out));
            wk.notify_one();
        });
    }
}

#[derive(Default)]
struct DragState {
    dragging: bool,
    drag_zoom: Option<(Point, f32, f32)>,
    dist: f32,
    last_pos: Option<Point>,
    last_move: Option<Instant>,
    vel: (f32, f32),
    last_click: Option<(Instant, Point)>,
    hovered_badge: Option<usize>,
    hover_pos: Option<Point>,
    shift_held: bool,
    probe_cache: canvas::Cache,
    probe_sig: Cell<u64>,
    probe_lookup: std::cell::RefCell<(u64, String)>,
}
struct MapCanvas<'a, M> {
    engine: &'a MapEngine,
    vectors_only: bool,
    interactive: bool,
    accept_keys: bool,
    cam: (f64, f64, f64),
    base_style: BaseMapStyle,
    overlay: WeatherOverlay,
    corner_r: f32,
    bundles: &'a HashMap<String, WeatherBundle>,
    settings: &'a Settings,
    badges: Vec<CityBadge>,
    wrap: Option<fn(MapMsg) -> M>,
}

fn fill_tri(f: &mut Frame, a: Point, b: Point, c: Point, col: Color) {
    f.fill(
        &Path::new(|p| {
            p.move_to(a);
            p.line_to(b);
            p.line_to(c);
            p.close();
        }),
        col,
    );
}
fn fill_lbl(f: &mut Frame, content: String, position: Point, color: Color, size: f32, w: Weight) {
    f.fill_text(CanvasText {
        content,
        position,
        color,
        size: size.into(),
        font: fw(w),
        align_x: Alignment::Center.into(),
        align_y: Alignment::Center.into(),
        ..CanvasText::default()
    });
}
fn dot(f: &mut Frame, p: Point, r: f32, c: Color) {
    f.fill(&Path::circle(p, r), c);
}

/// Camera zoom of the dashboard mini-map. At 5.7 both the basemap (`+0.58` bias) and its label overlay (`+0.35` bias)
/// resolve to zoom-6 tiles drawn at 0.81× — crisp, never upscaled raster text (5.5 drew zoom-5 labels at 1.41×).
pub(super) const MINI_MAP_ZOOM: f64 = 5.7;

/// Cross-dissolve weight exponent between consecutive model-forecast frames (see `draw_radar_timeline`).
const NWP_DISSOLVE_EXP: f32 = 0.8;

impl<M> MapCanvas<'_, M> {
    fn radar_blend(&self, m: &RainViewerManifest) -> (usize, Option<usize>, f32) {
        let len = m.frames.len();
        if !self.interactive {
            return (self.engine.live_frame_idx().min(len - 1), None, 0.0);
        }
        if len <= 1 {
            return (self.engine.radar.frame_idx.min(len - 1), None, 0.0);
        }
        let pos = self.engine.radar.play_pos.clamp(0.0, (len - 1) as f32);
        let (a, rf) = ((pos.floor() as usize).min(len - 1), (pos - pos.floor()).clamp(0.0, 1.0));
        if rf > 0.02 && a + 1 < len {
            if rf >= 0.98 {
                return (a + 1, None, 0.0);
            }
            // Constant-velocity linear crossfade during continuous playback (zero frame-boundary pulsing!); smooth ease when manually scrubbing.
            let b = if self.engine.radar.playing { rf } else { rf * rf * (3.0 - 2.0 * rf) };
            (a, Some(a + 1), b)
        } else {
            (a, None, 0.0)
        }
    }

    fn cached_flow_px(&self, pool: &SharedTilePool, layers_a: &[(u32, u8, f64)], layers_b: &[(u32, u8, f64)], vp: MapVp) -> (f32, f32) {
        if layers_a.is_empty() || layers_b.is_empty() || layers_a == layers_b {
            return (0.0, 0.0);
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let settled_count = |h: &mut std::collections::hash_map::DefaultHasher, layers: &[(u32, u8, f64)]| -> u32 {
            let mut n = 0_u32;
            for &(id, mz, bias) in layers {
                (id, mz).hash(h);
                for_each_visible_tile_biased(vp, mz, bias, |tz, wx, wy, _| {
                    if pool.get_thumb_id(id, (tz, wx, wy)).is_some() {
                        n += 1;
                    }
                    true
                });
            }
            n
        };
        let sa = settled_count(&mut h, layers_a);
        0xA5A5_5A5A_u32.hash(&mut h);
        let sb = settled_count(&mut h, layers_b);
        if sa == 0 || sb == 0 {
            return (0.0, 0.0);
        }
        // Key on the exact thumbnail count of `layers_a` & `layers_b` (NOT global `pool.weather_rev`!) so unrelated
        // background tile arrivals NEVER invalidate `flow_cache` or trigger redundant UI-thread flow recomputation!
        (sa, sb).hash(&mut h);
        let key = (h.finish(), (vp.1 * 4.0).round() as i32, (vp.2 * 4.0).round() as i32, (vp.3 * 4.0).round() as i32);
        if let Some(&v) = self.engine.flow_cache.borrow().get(&key) {
            return v;
        }
        let v = tiles::estimate_flow_px(pool, layers_a, layers_b, vp);
        let mut fc = self.engine.flow_cache.borrow_mut();
        if fc.len() > 160 {
            fc.clear();
        }
        fc.insert(key, v);
        v
    }

    fn draw_radar_timeline(&self, frame: &mut Frame, pool: &mut SharedTilePool, vp: MapVp, allow_fetch: bool) {
        let Some(m) = self.engine.manifest.as_ref().filter(|m| !m.frames.is_empty()) else { return };
        let (i0, i1_opt, blend) = self.radar_blend(m);
        let rf0 = &m.frames[i0];
        let ids_of = |idx: usize| self.engine.manifest_ns_ids.get(idx).copied();
        let resolve_ids = |p: &mut SharedTilePool, idx: usize, rf: &RainViewerFrame| -> [Option<u32>; 4] {
            let cached = ids_of(idx).unwrap_or([None; 4]);
            let pid = cached[0].or_else(|| rf.tile_namespace().map(|ns| p.intern(&ns)));
            let tiers = rf.eccc_tier_namespaces();
            let h = cached[1].or_else(|| tiers.as_ref().map(|t| p.intern(&t[0])));
            let r = cached[2].or_else(|| tiers.as_ref().map(|t| p.intern(&t[1])));
            let g = cached[3].or_else(|| tiers.as_ref().map(|t| p.intern(&t[2])));
            [pid, h, r, g]
        };
        let ids0 = resolve_ids(pool, i0, rf0);
        let rf1 = i1_opt.and_then(|i| m.frames.get(i).map(|f| (i, f)));
        let ids1 = rf1.map_or([None; 4], |(i, f)| resolve_ids(pool, i, f));
        pool.set_active_weather_ns([ids0[0], ids0[1], ids0[2], ids0[3], ids1[0], ids1[1], ids1[2], ids1[3]]);

        // Draws one complete timeline frame: its ECCC tiers (coarsest first) under its primary radar / HRRR layer.
        // A tile that hit a transient network error is bridged by the same tile of an adjacent same-source frame.
        let draw_rf_single = |fr: &mut Frame, p: &mut SharedTilePool, idx: usize, rf: &RainViewerFrame, op: f32, shift: (f32, f32)| {
            if op <= 0.015 {
                return;
            }
            let ids = resolve_ids(p, idx, rf);
            let is_hrrr = matches!(rf.source, RadarFrameSource::NoaaHrrr(..));
            let neighbor = |j: Option<usize>| -> Option<[Option<u32>; 4]> {
                let k = j?;
                let nf = m.frames.get(k)?;
                (std::mem::discriminant(&nf.source) == std::mem::discriminant(&rf.source)).then(|| ids_of(k)).flatten()
            };
            let nb = [neighbor(idx.checked_sub(1)), neighbor(Some(idx + 1))];
            if matches!(rf.source, RadarFrameSource::NoaaHrrr(..) | RadarFrameSource::EcccNwp) {
                let (h0, q) = rf.eccc_slot();
                for (slot, model) in [(3, EcccModel::Gdps), (2, EcccModel::Rdps), (1, EcccModel::Hrdps)] {
                    let Some(eid) = ids[slot] else { continue };
                    let el = TileLayer::overlay(
                        "",
                        TileLayerKind::Weather,
                        model.max_native_zoom(),
                        op,
                        Duration::from_secs(3600),
                        allow_fetch,
                        false,
                    )
                    .with_zoom_bias(model.zoom_bias())
                    .with_temporal_fallbacks(nb.map(|n| n.and_then(|a| a[slot])))
                    .with_shift_px(shift);
                    draw_tile_layer_filtered(
                        fr,
                        p,
                        eid,
                        vp,
                        el,
                        |z, x, y| eccc_tier_tile_needed(model, is_hrrr, z, x, y, vp),
                        |_, _, _| eccc::job_spec(model, h0, q),
                    );
                }
            }
            let Some(pid) = ids[0] else { return };
            let l = TileLayer::overlay("", TileLayerKind::Weather, rf.max_native_zoom(), op, Duration::from_secs(3600), allow_fetch, false)
                .with_zoom_bias(rf.primary_zoom_bias())
                .with_temporal_fallbacks(nb.map(|n| n.and_then(|a| a[0])))
                .with_shift_px(shift);
            draw_tile_layer_filtered(
                fr,
                p,
                pid,
                vp,
                l,
                |z, x, y| !is_hrrr || overlays::tile_intersects_hrrr(z, x, y),
                |z, x, y| rf.tile_url(&m.host, z, x, y).unwrap_or_default(),
            );
        };
        // Prefer 100% settled/zoom-covered frames; if only a far-outer corner sliver is still loading (`core_ready`),
        // allow seamless rendering via `temporal_fallbacks` so 1 slow corner tile NEVER freezes the map!
        let temp_pair = |idx: usize| -> [Option<u32>; 2] {
            let same = |k: Option<usize>| {
                let f = m.frames.get(idx)?;
                let nf = m.frames.get(k?)?;
                (matches!(nf.source, RadarFrameSource::NoaaHrrr(..)) == matches!(f.source, RadarFrameSource::NoaaHrrr(..)))
                    .then(|| ids_of(k?).and_then(|a| a[0]))
                    .flatten()
            };
            [same(idx.checked_sub(1)), same((idx + 1 < m.frames.len()).then_some(idx + 1))]
        };
        let r0 = radar_frame_readiness_ids(rf0, Some(ids0), temp_pair(i0), pool, vp);
        let r1 = rf1.map(|(i, f)| radar_frame_readiness_ids(f, Some(ids1), temp_pair(i), pool, vp));
        let rf0_ready = r0.all_ready || r0.visually_covered || r0.core_ready;
        let next_ok = r1.is_some_and(|r| r.all_ready || r.visually_covered || r.core_ready);
        if allow_fetch {
            if !r0.all_ready {
                let _ = ensure_radar_frame_visible(pool, &m.host, rf0, vp, PRIO_MAX, true);
            }
            if let (Some((_, f1)), Some(r1s)) = (rf1, r1)
                && !r1s.all_ready
            {
                let _ = ensure_radar_frame_visible(pool, &m.host, f1, vp, PRIO_MAX - 1, true);
            }
        }
        let base_op = self.engine.overlay_opacity.clamp(0.4, 1.0);
        if !rf0_ready {
            if self.interactive {
                // Hold the exact last complete frame while `rf0` finishes downloading — never jump backwards & forwards in time!
                let frame_ok = |idx: usize, f: &RainViewerFrame| {
                    let r = radar_frame_readiness_ids(f, ids_of(idx), temp_pair(idx), pool, vp);
                    r.all_ready || r.visually_covered || r.core_ready
                };
                if let Some(prev_i) = self.engine.last_rendered_radar_idx.get()
                    && let Some(prev_f) = m.frames.get(prev_i)
                    && frame_ok(prev_i, prev_f)
                {
                    draw_rf_single(frame, pool, prev_i, prev_f, base_op, (0.0, 0.0));
                    return;
                }
                if let Some((fbi, fb)) = (1..=10_usize)
                    .find_map(|d| i0.checked_sub(d).and_then(|i| m.frames.get(i).map(|f| (i, f))).filter(|&(i, f)| frame_ok(i, f)))
                {
                    self.engine.last_rendered_radar_idx.set(Some(fbi));
                    draw_rf_single(frame, pool, fbi, fb, base_op, (0.0, 0.0));
                    return;
                }
            }
            // Initial cold load before any earlier frame is viewport-ready: progressively draw `rf0` as tiles arrive.
            draw_rf_single(frame, pool, i0, rf0, base_op, (0.0, 0.0));
            return;
        }
        if self.interactive {
            self.engine.last_rendered_radar_idx.set(Some(i0));
        }
        let Some((i1, f1)) = rf1.filter(|_| next_ok && blend > 0.015) else {
            draw_rf_single(frame, pool, i0, rf0, base_op, (0.0, 0.0));
            return;
        };
        let nwp0 = matches!(rf0.source, RadarFrameSource::NoaaHrrr(..) | RadarFrameSource::EcccNwp);
        let nwp1 = matches!(f1.source, RadarFrameSource::NoaaHrrr(..) | RadarFrameSource::EcccNwp);
        if nwp0 != nwp1 {
            // Cross-source handoff (`LibreWXR` observed/nowcast radar <-> `HRRR`/`ECCC` NWP model):
            // Use a clean linear cross-dissolve (sum = base_op, zero brightness surge!) with zero cross-source
            // optical-flow shift so radar coverage dissolves smoothly into full-domain NWP forecast without warping.
            let w0 = base_op * (1.0 - blend);
            let w1 = base_op * blend;
            draw_rf_single(frame, pool, i0, rf0, w0, (0.0, 0.0));
            draw_rf_single(frame, pool, i1, f1, w1, (0.0, 0.0));
        } else {
            // Same-family frames (radar -> radar, or model -> model) are at most 30 minutes of storm travel apart
            // (hourly ECCC rasters are already motion-interpolated per frame), so a single viewport-wide optical-flow
            // shift removes the small residual displacement while the two frames cross-dissolve.
            let layers = |ids: [Option<u32>; 4], f: &RainViewerFrame| -> Vec<(u32, u8, f64)> {
                let mut v: Vec<(u32, u8, f64)> = Vec::with_capacity(4);
                if let Some(pid) = ids[0] {
                    v.push((pid, f.max_native_zoom(), f.primary_zoom_bias()));
                }
                for (slot, model) in [(1, EcccModel::Hrdps), (2, EcccModel::Rdps), (3, EcccModel::Gdps)] {
                    if let Some(eid) = ids[slot] {
                        v.push((eid, model.max_native_zoom(), model.zoom_bias()));
                    }
                }
                v
            };
            let (dx, dy) = self.cached_flow_px(pool, &layers(ids0, rf0), &layers(ids1, f1), vp);
            // Two translucent layers composited source-over are brighter than either alone, so the dissolve weights must
            // sum to more than 1 without overshooting. Measured on rendered frames, `t^0.8` holds total model-overlay
            // brightness constant through the dissolve (`sqrt` pulsed +8..12% between every pair of model frames);
            // observed radar keeps its established `sqrt` curve.
            let p = if nwp0 { NWP_DISSOLVE_EXP } else { 0.5 };
            let w0 = base_op * (1.0 - blend).powf(p);
            let w1 = base_op * blend.powf(p);
            draw_rf_single(frame, pool, i0, rf0, w0, (blend * dx, blend * dy));
            draw_rf_single(frame, pool, i1, f1, w1, (-(1.0 - blend) * dx, -(1.0 - blend) * dy));
        }
    }

    /// Global 12-hour animated infrared cloud satellite (`LibreWXR` `satellite.infrared`, `512×512`) with 60fps
    /// optical-flow cloud advection, returning `false` only if no satellite frames are present in the manifest yet.
    fn draw_satellite_timeline(&self, frame: &mut Frame, pool: &mut SharedTilePool, vp: MapVp, allow_fetch: bool) -> bool {
        let Some(m) = self.engine.manifest.as_ref().filter(|m| !m.satellite.is_empty()) else {
            return false;
        };
        let len = m.satellite.len();
        let (i0, i1_opt, blend) = if !self.interactive || len == 1 {
            (len - 1, None, 0.0_f32)
        } else {
            let pos = self.engine.radar.play_pos.clamp(0.0, (len - 1) as f32);
            let (a, rf) = ((pos.floor() as usize).min(len - 1), (pos - pos.floor()).clamp(0.0, 1.0));
            if rf > 0.02 && a + 1 < len {
                if rf >= 0.98 {
                    (a + 1, None, 0.0)
                } else {
                    let b = if self.engine.radar.playing { rf } else { rf * rf * (3.0 - 2.0 * rf) };
                    (a, Some(a + 1), b)
                }
            } else {
                (a, None, 0.0)
            }
        };
        let id_of = |p: &mut SharedTilePool, idx: usize| -> u32 {
            self.engine.sat_ns_ids.get(idx).copied().unwrap_or_else(|| p.intern(&m.satellite[idx].tile_namespace()))
        };
        let id0 = id_of(pool, i0);
        let id1 = i1_opt.map(|j| id_of(pool, j));
        pool.set_active_weather_ns([Some(id0), id1, None, None, None, None, None, None]);
        let sat_pair = |idx: usize| -> [Option<u32>; 2] {
            [idx.checked_sub(1).and_then(|k| self.engine.sat_ns_ids.get(k).copied()), self.engine.sat_ns_ids.get(idx + 1).copied()]
        };
        let r0 = sat_frame_readiness_id(id0, sat_pair(i0), pool, vp);
        let r1 = i1_opt.zip(id1).map(|(j, id)| sat_frame_readiness_id(id, sat_pair(j), pool, vp));
        if allow_fetch {
            if !r0.all_ready {
                let _ = ensure_sat_frame_visible(pool, &m.host, &m.satellite[i0], vp, PRIO_MAX, true);
            }
            if let (Some(j), Some(r1s)) = (i1_opt, r1)
                && !r1s.all_ready
            {
                let _ = ensure_sat_frame_visible(pool, &m.host, &m.satellite[j], vp, PRIO_MAX - 1, true);
            }
        }
        let draw_sat = |fr: &mut Frame, p: &mut SharedTilePool, idx: usize, op: f32, shift: (f32, f32)| {
            let f = &m.satellite[idx];
            let ns = f.tile_namespace();
            let nid = id_of(p, idx);
            let mut tfb = [None; 2];
            if idx > 0 {
                tfb[0] = Some(id_of(p, idx - 1));
            }
            if idx + 1 < len {
                tfb[1] = Some(id_of(p, idx + 1));
            }
            let l = TileLayer::overlay(&ns, TileLayerKind::Weather, SAT_MAX_NATIVE_ZOOM, op, Duration::from_secs(1800), allow_fetch, false)
                .with_zoom_bias(SAT_ZOOM_BIAS)
                .with_temporal_fallbacks(tfb)
                .with_shift_px(shift);
            draw_tile_layer_filtered(fr, p, nid, vp, l, |_, _, _| true, |z, x, y| f.tile_url(&m.host, z, x, y));
        };
        let base_op = (self.engine.overlay_opacity * 0.92).clamp(0.38, 0.95);
        let sat_ok = |r: FrameReadiness| r.all_ready || r.visually_covered || r.core_ready;
        if !sat_ok(r0) {
            if self.interactive {
                let prev_ok = |idx: usize, p: &mut SharedTilePool| {
                    let nid = id_of(p, idx);
                    sat_ok(sat_frame_readiness_id(nid, sat_pair(idx), p, vp))
                };
                if let Some(prev_i) = self.engine.last_rendered_radar_idx.get()
                    && prev_i < len
                    && prev_ok(prev_i, pool)
                {
                    draw_sat(frame, pool, prev_i, base_op, (0.0, 0.0));
                    return true;
                }
                if let Some(fbi) = (1..=6_usize).find_map(|d| i0.checked_sub(d).filter(|&k| prev_ok(k, pool))) {
                    self.engine.last_rendered_radar_idx.set(Some(fbi));
                    draw_sat(frame, pool, fbi, base_op, (0.0, 0.0));
                    return true;
                }
            }
            draw_sat(frame, pool, i0, base_op, (0.0, 0.0));
            return true;
        }
        if self.interactive {
            self.engine.last_rendered_radar_idx.set(Some(i0));
        }
        let next_ok = r1.is_some_and(sat_ok);
        if let (Some(j), Some(nid1)) = (i1_opt.filter(|_| next_ok && blend > 0.015), id1) {
            let (dx, dy) =
                self.cached_flow_px(pool, &[(id0, SAT_MAX_NATIVE_ZOOM, SAT_ZOOM_BIAS)], &[(nid1, SAT_MAX_NATIVE_ZOOM, SAT_ZOOM_BIAS)], vp);
            let w0 = base_op * (1.0 - blend).sqrt();
            let w1 = base_op * blend.sqrt();
            draw_sat(frame, pool, i0, w0, (blend * dx, blend * dy));
            draw_sat(frame, pool, j, w1, (-(1.0 - blend) * dx, -(1.0 - blend) * dy));
        } else {
            draw_sat(frame, pool, i0, base_op, (0.0, 0.0));
        }
        true
    }

    /// Draws static map vector layers (pressure isobars + H/L centers, settled inspect pin, city pins, scale bar).
    /// Cached in `vec_cache` / `mini_vec_cache` so cursor hovers replay straight from GPU vertex buffers!
    fn draw_static_vectors(&self, f: &mut Frame, rect: Rectangle, hovered_badge: Option<usize>) {
        let dark = self.base_style.is_dark_hud(self.overlay);
        if self.interactive && self.overlay == WeatherOverlay::Pressure {
            let fts = self.engine.selected_forecast_timestamp();
            let iso = self.engine.build_pressure_isobars((rect.size(), self.cam.0, self.cam.1, self.cam.2), self.bundles, fts);
            if !iso.segs.is_empty() {
                let path = Path::new(|pb| {
                    for &(a, b) in &iso.segs {
                        pb.move_to(a);
                        pb.line_to(b);
                    }
                });
                let lcol = if dark { rgba8(235, 244, 255, 0.42) } else { rgba8(24, 42, 72, 0.36) };
                f.stroke(&path, Stroke::default().with_width(1.15).with_color(lcol));
                let (lbg, lfg) = if dark {
                    (rgba8(18, 26, 40, 0.72), rgba8(236, 244, 255, 0.88))
                } else {
                    (rgba8(248, 251, 255, 0.78), rgba8(24, 42, 72, 0.86))
                };
                for (pt, lbl) in &iso.labels {
                    if self.badges.iter().any(|b| {
                        let bp = project_lat_lon_to_screen(b.lat, b.lon, self.cam.0, self.cam.1, self.cam.2, rect);
                        (bp.x - pt.x).hypot(bp.y - pt.y) < 46.0 || (bp.x - pt.x).hypot(bp.y - 26.0 - pt.y) < 46.0
                    }) {
                        continue;
                    }
                    let p = Path::rounded_rectangle(Point::new(pt.x - 16.0, pt.y - 6.5), Size::new(32.0, 13.0), 4.0.into());
                    f.fill(&p, lbg);
                    fill_lbl(f, lbl.clone(), *pt, lfg, 8.5, Weight::Semibold);
                }
            }
            for &(pt, is_high, hpa) in &iso.centers {
                if self.badges.iter().any(|b| {
                    let bp = project_lat_lon_to_screen(b.lat, b.lon, self.cam.0, self.cam.1, self.cam.2, rect);
                    (bp.x - pt.x).hypot(bp.y - 24.0 - pt.y) < 46.0
                }) {
                    continue;
                }
                let col = if is_high { rgb8(10, 132, 255) } else { rgb8(235, 64, 52) };
                let halo = if dark { rgba8(8, 14, 24, 0.78) } else { rgba8(255, 255, 255, 0.88) };
                for (ox, oy) in [(-1.4, 0.0), (1.4, 0.0), (0.0, -1.4), (0.0, 1.4)] {
                    fill_lbl(f, if is_high { "H".into() } else { "L".into() }, Point::new(pt.x + ox, pt.y + oy), halo, 20.0, Weight::Bold);
                }
                fill_lbl(f, if is_high { "H".into() } else { "L".into() }, pt, col, 20.0, Weight::Bold);
                fill_lbl(
                    f,
                    format!("{hpa:.0}"),
                    Point::new(pt.x, pt.y + 14.0),
                    if dark { Color::WHITE } else { rgb8(24, 36, 56) },
                    9.0,
                    Weight::Bold,
                );
            }
        }
        let inspect_settled = self.engine.inspect.dropped_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(240));
        if self.interactive
            && inspect_settled
            && let Some((ila, ilo)) = self.engine.inspect.coord
        {
            self.draw_inspect_pin(f, rect, ila, ilo, 1.0);
        }
        let pts: Vec<_> = self.badges.iter().map(|b| (b.lat, b.lon, b.active)).collect();
        let mut vis = declutter_points(&pts, self.cam.0, self.cam.1, self.cam.2, rect);
        vis.reverse();
        for (idx, pos, dc_raw) in vis {
            let (b, hov) = (&self.badges[idx], self.interactive && hovered_badge == Some(idx));
            let rad = if !self.interactive {
                15.0
            } else if b.active {
                19.5
            } else {
                14.0
            } + if hov { 1.6 } else { 0.0 };
            let dc = if !self.interactive && b.active { Point::new(pos.x, pos.y - 24.0) } else { dc_raw };
            if b.active {
                let (r0, r1, r2, c2) = if b.is_current_location {
                    dot(f, pos, 9.5, rgba8(10, 132, 255, 0.22));
                    (5.2, 5.0, 3.3, rgb8(10, 132, 255))
                } else {
                    (4.6, 4.4, 2.7, b.col)
                };
                dot(f, Point::new(pos.x, pos.y + 0.8), r0, rgba8(0, 0, 0, 0.23));
                dot(f, pos, r1, Color::WHITE);
                dot(f, pos, r2, c2);
            }
            dot(f, Point::new(dc.x, dc.y + 2.2), rad + 3.2, rgba8(0, 0, 0, if dark { 0.34 } else { 0.18 }));
            if let Some(sev) = b.alert_severity {
                let acol = match sev {
                    AlertSeverity::Extreme | AlertSeverity::Severe => rgba8(255, 69, 58, 0.42),
                    _ => rgba8(255, 159, 10, 0.40),
                };
                dot(f, dc, rad + 5.2, acol);
            }
            let _ = hov.then(|| dot(f, dc, rad + 5.0, rgba8(10, 132, 255, 0.26)));
            let tip_y = (pos.y - 6.0).max(dc.y + rad + 4.0);
            if b.active {
                let bw = if self.interactive { 7.6 } else { 6.0 };
                let p0 = Point::new(dc.x - bw, dc.y + rad * 0.52);
                fill_tri(f, p0, Point::new(dc.x, tip_y + 1.8), Point::new(dc.x + bw, dc.y + rad * 0.52), Color::WHITE);
            }
            dot(f, dc, rad + 2.3, Color::WHITE);
            dot(f, dc, rad, b.col);
            if b.active {
                let iw = if self.interactive { 5.2 } else { 4.0 };
                let p0 = Point::new(dc.x - iw, dc.y + rad * 0.50);
                fill_tri(f, p0, Point::new(dc.x, tip_y - 0.6), Point::new(dc.x + iw, dc.y + rad * 0.50), b.col);
            }
            if self.overlay != WeatherOverlay::WindFlow {
                dot(f, Point::new(dc.x, dc.y - rad * 0.28), rad * 0.68, rgba8(255, 255, 255, 0.08));
            } else if let Some(deg) = b.wind_deg {
                let ((sa, ca), br) = (((deg + 90.0).to_radians() as f32).sin_cos(), rad + 1.2);
                let p0 = Point::new(dc.x + ca * (rad + 7.2), dc.y + sa * (rad + 7.2));
                let p1 = Point::new(dc.x + ca * br - sa * 4.4, dc.y + sa * br + ca * 4.4);
                fill_tri(f, p0, p1, Point::new(dc.x + ca * br + sa * 4.4, dc.y + sa * br - ca * 4.4), Color::WHITE);
            }
            let vc = if self.overlay == WeatherOverlay::WindFlow { rgb8(20, 32, 50) } else { Color::WHITE };
            let fsz = if b.active { if self.interactive { 13.5 } else { 11.5 } } else { 10.5 };
            fill_lbl(f, b.val.clone(), Point::new(dc.x, if b.sub.is_empty() { dc.y } else { dc.y - 4.2 }), vc, fsz, Weight::Bold);
            let _ =
                (!b.sub.is_empty()).then(|| fill_lbl(f, b.sub.clone(), Point::new(dc.x, dc.y + 7.5), rgb8(64, 78, 98), 7.5, Weight::Bold));
            if self.interactive
                && let Some(sev) = b.alert_severity
            {
                let ap = Point::new(dc.x + rad * 0.72, dc.y - rad * 0.72);
                let acol = match sev {
                    AlertSeverity::Extreme | AlertSeverity::Severe => rgb8(255, 59, 48),
                    _ => rgb8(255, 149, 0),
                };
                dot(f, ap, 6.2, Color::WHITE);
                dot(f, ap, 5.0, acol);
                fill_lbl(f, "!".into(), ap, Color::WHITE, 8.5, Weight::Bold);
            }
            // Match original macOS Weather: keep active pin clean (no "My Location" label colliding with city place names unless hovered).
            let show_lbl = !b.title.is_empty() && (!b.active || hov);
            if show_lbl {
                let lp = Point::new(pos.x, if b.active { pos.y + 14.5 } else { dc.y + rad + 11.0 });
                let (sh, fg) = if dark { (rgba8(8, 12, 20, 0.85), Color::WHITE) } else { (rgba8(255, 255, 255, 0.92), rgb8(20, 28, 42)) };
                for (ox, oy) in [(-0.85, 0.65), (0.85, 0.65)] {
                    fill_lbl(f, b.title.clone(), Point::new(lp.x + ox, lp.y + oy), sh, 10.5, Weight::Semibold);
                }
                fill_lbl(f, b.title.clone(), lp, fg, 10.5, Weight::Semibold);
            }
        }
        // Apple Maps bottom-left latitude-corrected distance scale bar.
        let has_tl = self.overlay.has_timeline();
        let bottom_clear = !has_tl || rect.width >= 760.0;
        if self.interactive && rect.width >= 420.0 && rect.height >= 280.0 {
            let (pill_bg, pill_bdr, fg) = if dark {
                (rgba8(22, 28, 40, 0.78), rgba8(255, 255, 255, 0.16), rgba8(242, 246, 254, 0.92))
            } else {
                (rgba8(249, 251, 254, 0.86), rgba8(18, 30, 50, 0.14), rgba8(26, 36, 52, 0.88))
            };
            let (sw, slbl) = scale_bar_spec(self.cam.0, self.cam.2, self.settings.temp_unit == TempUnit::Fahrenheit);
            let (bx, by, bw, bh) = (16.0_f32, if bottom_clear { rect.height - 34.0 } else { rect.height - 122.0 }, sw + 18.0, 22.0_f32);
            let bg_p = Path::rounded_rectangle(Point::new(bx, by), Size::new(bw, bh), 7.0.into());
            f.fill(&bg_p, pill_bg);
            f.stroke(&bg_p, Stroke::default().with_width(0.8).with_color(pill_bdr));
            let (lx0, lx1, ly) = (bx + 9.0, bx + 9.0 + sw, by + 16.5);
            let bar = Path::new(|pb| {
                pb.move_to(Point::new(lx0, ly - 3.5));
                pb.line_to(Point::new(lx0, ly));
                pb.line_to(Point::new(lx1, ly));
                pb.line_to(Point::new(lx1, ly - 3.5));
            });
            f.stroke(&bar, Stroke::default().with_width(1.35).with_color(fg));
            fill_lbl(f, slbl, Point::new(bx + bw * 0.5, by + 7.5), fg, 9.5, Weight::Semibold);
        }
    }

    fn draw_inspect_pin(&self, f: &mut Frame, rect: Rectangle, ila: f64, ilo: f64, dt_drop: f32) {
        let ip = project_lat_lon_to_screen(ila, ilo, self.cam.0, self.cam.1, self.cam.2, rect);
        let ease = 1.0 - (1.0 - dt_drop).powi(3);
        let lift = (1.0 - ease) * 18.0;
        let hd = Point::new(ip.x, ip.y - 16.0 - lift);
        dot(f, ip, 10.5 * ease, rgba8(10, 132, 255, 0.22 * ease));
        dot(f, Point::new(ip.x, ip.y + 0.5), 2.0 + 1.8 * ease, rgba8(0, 0, 0, 0.24 * ease));
        fill_tri(
            f,
            Point::new(hd.x - 5.5, hd.y + 6.0),
            Point::new(ip.x, ip.y - lift * 0.65),
            Point::new(hd.x + 5.5, hd.y + 6.0),
            Color::WHITE,
        );
        dot(f, Point::new(hd.x, hd.y + 1.5), 11.2, rgba8(0, 0, 0, 0.22));
        for (r, c) in [(10.2, Color::WHITE), (8.2, rgb8(10, 132, 255)), (3.0, Color::WHITE)] {
            dot(f, hd, r, c);
        }
    }

    fn draw_wind_streamlines(&self, f: &mut Frame, rect: Rectangle) {
        if self.engine.wind_grid.is_none() {
            return;
        }
        let (rw, rh) = (rect.width, rect.height);
        const BAND_SPD: [f64; 4] = [2.5, 8.0, 15.0, 24.0];
        // Zero-allocation single-pass binning into 8 (speed x alpha) fixed stack buckets (0 heap allocs/frame!).
        let mut tails = [[0_u8; 256]; 8];
        let mut t_len = [0_usize; 8];
        let mut heads = [[0_u8; 256]; 8];
        let mut h_len = [0_usize; 8];
        let mut valids = [0_u8; 256];
        for (idx, p) in self.engine.particles.iter().enumerate().take(256) {
            let u = (p.age / p.max_age.max(0.1)).clamp(0.0, 1.0);
            let fade = 4.0 * u * (1.0 - u) * 0.86;
            if fade < 0.04 {
                continue;
            }
            let sb = if p.spd < 5.0 {
                0
            } else if p.spd < 11.0 {
                1
            } else if p.spd < 19.0 {
                2
            } else {
                3
            };
            let bi = (sb << 1) | usize::from(fade >= 0.44);
            let n = (p.tlen as usize).min(WIND_TRAIL_LEN);
            let mut valid = 0_usize;
            if n >= 2 {
                valid = 1;
                for k in 1..n {
                    let dx = (p.trail[k - 1].0 - p.trail[k].0) * rw;
                    let dy = (p.trail[k - 1].1 - p.trail[k].1) * rh;
                    if dx * dx + dy * dy > 2304.0 {
                        break;
                    }
                    valid += 1;
                }
            }
            valids[idx] = valid as u8;
            if valid >= 3 {
                let ti = t_len[bi];
                tails[bi][ti] = idx as u8;
                t_len[bi] = ti + 1;
            }
            let hi = h_len[bi];
            heads[bi][hi] = idx as u8;
            h_len[bi] = hi + 1;
        }
        for bi in 0..8_usize {
            let (sb, ab) = (bi >> 1, bi & 1);
            let tl = t_len[bi];
            if tl > 0 {
                let ids = &tails[bi][..tl];
                let path = Path::new(|pb| {
                    for &idx_u8 in ids {
                        let idx = idx_u8 as usize;
                        let p = &self.engine.particles[idx];
                        let valid = valids[idx] as usize;
                        let mid = valid / 2;
                        pb.move_to(Point::new(p.trail[mid].0 * rw, p.trail[mid].1 * rh));
                        for k in (mid + 1)..valid {
                            pb.line_to(Point::new(p.trail[k].0 * rw, p.trail[k].1 * rh));
                        }
                    }
                });
                let (al, w) = if ab == 1 { (0.34, 1.25) } else { (0.15, 1.0) };
                let st = Stroke::default().with_width(w).with_color(wind_to_color(BAND_SPD[sb], al));
                f.stroke(&path, st);
            }
            let hl = h_len[bi];
            if hl > 0 {
                let ids = &heads[bi][..hl];
                let path = Path::new(|pb| {
                    for &idx_u8 in ids {
                        let idx = idx_u8 as usize;
                        let p = &self.engine.particles[idx];
                        let valid = valids[idx] as usize;
                        if valid >= 2 {
                            let end = if valid >= 3 { (valid / 2) + 1 } else { valid };
                            pb.move_to(Point::new(p.trail[0].0 * rw, p.trail[0].1 * rh));
                            for k in 1..end {
                                pb.line_to(Point::new(p.trail[k].0 * rw, p.trail[k].1 * rh));
                            }
                        } else if p.spd >= 0.25 && p.dx.hypot(p.dy) >= 0.05 {
                            let (sx, sy, hl) = (p.u * rw, p.v * rh, (8.0 + p.spd * 1.1).clamp(8.0, 20.0) * 0.5);
                            pb.move_to(Point::new(sx - p.dx * hl, sy - p.dy * hl));
                            pb.line_to(Point::new(sx + p.dx * hl, sy + p.dy * hl));
                        }
                    }
                });
                let (al, w) = if ab == 1 { (0.82, 1.95) } else { (0.36, 1.45) };
                let st = Stroke::default().with_width(w).with_line_cap(canvas::LineCap::Round).with_color(wind_to_color(BAND_SPD[sb], al));
                f.stroke(&path, st);
            }
        }
    }

    /// Tiny uncached dynamic vector overlay: only draws wind streamlines or bouncing inspect pin!
    fn draw_dynamic_vectors(&self, f: &mut Frame, rect: Rectangle) {
        if !self.interactive {
            return;
        }
        if self.overlay == WeatherOverlay::WindFlow {
            self.draw_wind_streamlines(f, rect);
        }
        if let Some((ila, ilo)) = self.engine.inspect.coord
            && let Some(t) = self.engine.inspect.dropped_at
            && t.elapsed() < Duration::from_millis(240)
        {
            self.draw_inspect_pin(f, rect, ila, ilo, (t.elapsed().as_secs_f32() / 0.22).clamp(0.0, 1.0));
        }
    }
}

impl<M: Clone> canvas::Program<M> for MapCanvas<'_, M> {
    type State = DragState;
    fn update(&self, st: &mut DragState, event: &Event, b: Rectangle, cur: mouse::Cursor) -> Option<Action<M>> {
        if !self.interactive || !self.vectors_only {
            return None;
        }
        let wrap = self.wrap?;
        let emit = |m: MapMsg| Some(Action::publish(wrap(m)).and_capture());
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cur.position_in(b)?;
                st.hover_pos = None;
                if let Some((t, prev)) = st.last_click
                    && t.elapsed() < Duration::from_millis(320)
                    && (p.x - prev.x).hypot(p.y - prev.y) <= 10.0
                {
                    // Apple Maps double-click-and-drag one-handed zoom (or quick double-click zoom on release).
                    (st.last_click, st.dragging, st.dist, st.vel) = (None, false, 0.0, (0.0, 0.0));
                    st.drag_zoom = Some((p, cur.position().map_or(p.y, |gp| gp.y), 0.0));
                    return emit(MapMsg::PanStart);
                }
                (st.dragging, st.drag_zoom, st.dist, st.vel, st.last_pos, st.last_move) =
                    (true, None, 0.0, (0.0, 0.0), cur.position(), Some(Instant::now()));
                emit(MapMsg::PanStart)
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let p = cur.position_in(b)?;
                (st.dragging, st.drag_zoom, st.hover_pos) = (false, None, None);
                emit(MapMsg::OpenContextMenu(p, b.size()))
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.drag_zoom.is_some() => {
                if let Some((anchor, prev_y, moved)) = st.drag_zoom {
                    let dy = position.y - prev_y;
                    st.drag_zoom = Some((anchor, position.y, moved + dy.abs()));
                    if dy.abs() > 0.2 {
                        return emit(MapMsg::DragZoom(anchor, b.size(), dy as f64 * 0.015));
                    }
                }
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                let last = st.last_pos.unwrap_or(*position);
                let dt = st.last_move.map_or(0.016_f32, |t| t.elapsed().as_secs_f32().clamp(0.004, 0.1));
                let (dx, dy) = (position.x - last.x, position.y - last.y);
                st.vel = (st.vel.0 * 0.35 + dx / dt * 0.65, st.vel.1 * 0.35 + dy / dt * 0.65);
                (st.last_pos, st.last_move, st.dist) = (Some(*position), Some(Instant::now()), st.dist + dx.hypot(dy));
                if dx.abs() > 0.05 || dy.abs() > 0.05 { emit(MapMsg::PanPixels(dx, dy)) } else { Some(Action::capture()) }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // Zero-allocation badge hover hit-test (avoids two Vec allocs per pointer motion event!).
                let hpos = cur.position_in(b);
                let rect = Rectangle::with_size(b.size());
                let next = hpos.and_then(|pos| {
                    let mut best = None;
                    for act in [true, false] {
                        for (i, bd) in self.badges.iter().enumerate() {
                            if bd.active != act {
                                continue;
                            }
                            let p = project_lat_lon_to_screen(bd.lat, bd.lon, self.cam.0, self.cam.1, self.cam.2, rect);
                            let dc = Point::new(p.x, if act { p.y - 31.0 } else { p.y });
                            let r = if act { 23.0 } else { 16.5 };
                            if (pos.x - dc.x).powi(2) + (pos.y - dc.y).powi(2) <= r * r {
                                best = Some(i);
                                break;
                            }
                        }
                        if best.is_some() {
                            break;
                        }
                    }
                    best
                });
                let has_probe_ov = !matches!(self.overlay, WeatherOverlay::InfraredClouds | WeatherOverlay::NexradUsRadar);
                let moved = has_probe_ov
                    && match (st.hover_pos, hpos) {
                        (Some(a), Some(c)) => (a.x - c.x).hypot(a.y - c.y) >= 12.0,
                        (None, None) => false,
                        _ => true,
                    };
                if st.hovered_badge != next || moved {
                    (st.hovered_badge, st.hover_pos) = (next, hpos);
                    Some(Action::request_redraw())
                } else {
                    None
                }
            }
            Event::Mouse(mouse::Event::CursorLeft) if st.hover_pos.is_some() || st.hovered_badge.is_some() => {
                (st.hover_pos, st.hovered_badge) = (None, None);
                Some(Action::request_redraw())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if st.drag_zoom.is_some() => {
                let Some((anchor, _, moved)) = st.drag_zoom.take() else { return Some(Action::capture()) };
                if moved < 5.0 {
                    return emit(if st.shift_held {
                        MapMsg::WheelZoom(anchor, b.size(), -1.15)
                    } else {
                        MapMsg::DoubleClickZoom(anchor, b.size())
                    });
                }
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if st.dragging => {
                let (d, vel, recent) = (st.dist, st.vel, st.last_move.is_some_and(|t| t.elapsed() < Duration::from_millis(110)));
                (st.dragging, st.dist, st.vel) = (false, 0.0, (0.0, 0.0));
                if d < 5.0
                    && let Some(p) = cur.position_in(b)
                {
                    st.last_click = Some((Instant::now(), p));
                    return emit(MapMsg::ClickMap(p, b.size()));
                }
                st.last_click = None;
                if recent && vel.0.hypot(vel.1) > 45.0 { emit(MapMsg::PanFling(vel.0, vel.1)) } else { Some(Action::capture()) }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let p = cur.position_in(b)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y as f64 * 0.48,
                    mouse::ScrollDelta::Pixels { y, .. } => *y as f64 * 0.0065,
                };
                (dy.abs() > 0.0001).then(|| Action::publish(wrap(MapMsg::WheelZoom(p, b.size(), dy))).and_capture())
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                st.shift_held = m.shift();
                None
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if self.accept_keys && !modifiers.command() && !modifiers.alt() =>
            {
                match key {
                    Key::Named(Named::Space) => emit(MapMsg::ToggleRadarPlay),
                    Key::Named(Named::ArrowLeft) => emit(MapMsg::StepTimeline(-1)),
                    Key::Named(Named::ArrowRight) => emit(MapMsg::StepTimeline(1)),
                    Key::Character(c) if matches!(c.as_str(), "+" | "=") => emit(MapMsg::ZoomStep(0.85)),
                    Key::Character(c) if matches!(c.as_str(), "-" | "_") => emit(MapMsg::ZoomStep(-0.85)),
                    Key::Character(c) if c.as_str() == "0" => emit(MapMsg::RecenterActive),
                    Key::Character(c) if matches!(c.as_str(), "f" | "F") => emit(MapMsg::ToggleFullScreen),
                    Key::Character(c) if matches!(c.as_str(), "o" | "O") => emit(MapMsg::FitAllLocations),
                    Key::Character(c) if matches!(c.as_str(), "n" | "N") => emit(MapMsg::ToggleTerminator),
                    Key::Character(c) if matches!(c.as_str(), "h" | "H") => emit(MapMsg::ToggleLegend),
                    Key::Character(c) if matches!(c.as_str(), "l" | "L") => emit(MapMsg::ToggleLayerMenu),
                    Key::Character(c) if matches!(c.as_str(), "w" | "W") => emit(MapMsg::PanFling(0.0, 560.0)),
                    Key::Character(c) if matches!(c.as_str(), "s" | "S") => emit(MapMsg::PanFling(0.0, -560.0)),
                    Key::Character(c) if matches!(c.as_str(), "a" | "A") => emit(MapMsg::PanFling(560.0, 0.0)),
                    Key::Character(c) if matches!(c.as_str(), "d" | "D") => emit(MapMsg::PanFling(-560.0, 0.0)),
                    Key::Character(c) if matches!(c.as_str(), "1" | "2" | "3" | "4" | "5" | "6" | "7") => {
                        emit(MapMsg::SelectOverlay(WeatherOverlay::ALL[(c.as_bytes()[0] - b'1') as usize]))
                    }
                    _ => None,
                }
            }
            Event::Window(iced::window::Event::Unfocused) if st.dragging || st.drag_zoom.is_some() => {
                (st.dragging, st.drag_zoom, st.dist, st.vel) = (false, None, 0.0, (0.0, 0.0));
                Some(Action::request_redraw())
            }
            _ => None,
        }
    }

    fn mouse_interaction(&self, st: &DragState, b: Rectangle, cur: mouse::Cursor) -> mouse::Interaction {
        if !self.interactive {
            if self.vectors_only { mouse::Interaction::Pointer } else { mouse::Interaction::None }
        } else if !self.vectors_only {
            mouse::Interaction::None
        } else if st.drag_zoom.is_some() {
            mouse::Interaction::ResizingVertically
        } else if st.dragging {
            mouse::Interaction::Grabbing
        } else if cur.position_in(b).is_none() {
            mouse::Interaction::default()
        } else if st.hovered_badge.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::Grab
        }
    }

    fn draw(&self, st: &DragState, renderer: &Renderer, _: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let size = bounds.size();
        if size.width < 4.0 || size.height < 4.0 {
            return Vec::new();
        }
        if self.vectors_only {
            let rect = Rectangle::with_size(size);
            let fts = self.interactive.then(|| self.engine.selected_forecast_timestamp()).flatten();
            let inspect_settled = self.engine.inspect.dropped_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(240));
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                size.width.to_bits(),
                size.height.to_bits(),
                self.cam.0.to_bits(),
                self.cam.1.to_bits(),
                self.cam.2.to_bits(),
                self.base_style,
                self.overlay,
            )
                .hash(&mut h);
            (
                self.interactive,
                st.hovered_badge,
                self.settings.temp_unit as u8,
                self.engine.grid_gen,
                (self.overlay == WeatherOverlay::Pressure).then_some(fts.map(|t| t / 180)),
                inspect_settled,
                self.engine.inspect.coord.map(|(a, b)| (a.to_bits(), b.to_bits())),
            )
                .hash(&mut h);
            for bd in &self.badges {
                (
                    bd.lat.to_bits(),
                    bd.lon.to_bits(),
                    &bd.val,
                    &bd.sub,
                    &bd.title,
                    bd.active,
                    bd.col.r.to_bits(),
                    bd.col.g.to_bits(),
                    bd.col.b.to_bits(),
                    bd.alert_severity.map(|s| s as u8),
                )
                    .hash(&mut h);
            }
            let sig = h.finish() | 1;
            let (vcache, vsig) = if self.interactive {
                (&self.engine.vec_cache, &self.engine.vec_cache_sig)
            } else {
                (&self.engine.mini_vec_cache, &self.engine.mini_vec_sig)
            };
            if vsig.get() != sig {
                vcache.clear();
                vsig.set(sig);
            }
            let static_geom = vcache.draw(renderer, size, |fr| {
                fr.with_clip(rect, |sub| self.draw_static_vectors(sub, rect, st.hovered_badge));
            });
            let need_dyn =
                self.interactive && (self.overlay == WeatherOverlay::WindFlow || (!inspect_settled && self.engine.inspect.coord.is_some()));
            let has_probe = self.interactive
                && !st.dragging
                && rect.width >= 420.0
                && rect.height >= 280.0
                && (!self.overlay.has_timeline() || rect.width >= 760.0)
                && st.hover_pos.is_some_and(|hp| {
                    hp.x > 24.0
                        && hp.x < rect.width - 60.0
                        && hp.y > 54.0
                        && hp.y < rect.height - if self.overlay.has_timeline() { 126.0 } else { 52.0 }
                });
            let mut geoms = Vec::with_capacity(3);
            geoms.push(static_geom);
            if need_dyn {
                let mut dyn_frame = Frame::new(renderer, size);
                dyn_frame.with_clip(rect, |sub| self.draw_dynamic_vectors(sub, rect));
                geoms.push(dyn_frame.into_geometry());
            }
            if has_probe && let Some(hp) = st.hover_pos {
                let mut lkh = std::collections::hash_map::DefaultHasher::new();
                (
                    (hp.x * 0.5).round() as i32,
                    (hp.y * 0.5).round() as i32,
                    self.cam.0.to_bits(),
                    self.cam.1.to_bits(),
                    self.cam.2.to_bits(),
                    self.overlay,
                    self.engine.grid_gen,
                    self.settings.temp_unit as u8,
                    self.settings.wind_unit as u8,
                    self.settings.pressure_unit as u8,
                    self.settings.precip_unit as u8,
                )
                    .hash(&mut lkh);
                let lkey = lkh.finish() | 1;
                let plbl = {
                    let mut lk = st.probe_lookup.borrow_mut();
                    if lk.0 != lkey {
                        let (hla, hlo) = project_screen_to_lat_lon(hp, self.cam.0, self.cam.1, self.cam.2, rect);
                        lk.1 = self.engine.sample_probe_label(hla, hlo, self.overlay, self.bundles, self.settings);
                        lk.0 = lkey;
                    }
                    lk.1.clone()
                };
                if !plbl.is_empty() {
                    let dark = self.base_style.is_dark_hud(self.overlay);
                    let mut ph = std::collections::hash_map::DefaultHasher::new();
                    (size.width.to_bits(), size.height.to_bits(), dark, &plbl).hash(&mut ph);
                    let psig = ph.finish() | 1;
                    if st.probe_sig.get() != psig {
                        st.probe_cache.clear();
                        st.probe_sig.set(psig);
                    }
                    geoms.push(st.probe_cache.draw(renderer, size, |f| {
                        let (pill_bg, pill_bdr, fg) = if dark {
                            (rgba8(22, 28, 40, 0.78), rgba8(255, 255, 255, 0.16), rgba8(242, 246, 254, 0.92))
                        } else {
                            (rgba8(249, 251, 254, 0.86), rgba8(18, 30, 50, 0.14), rgba8(26, 36, 52, 0.88))
                        };
                        let (by, bh) = (rect.height - 34.0, 22.0_f32);
                        let pw = (plbl.len() as f32 * 6.2 + 18.0).clamp(48.0, 148.0);
                        let px = rect.width - 16.0 - pw;
                        let pp = Path::rounded_rectangle(Point::new(px, by), Size::new(pw, bh), 7.0.into());
                        f.fill(&pp, pill_bg);
                        f.stroke(&pp, Stroke::default().with_width(0.8).with_color(pill_bdr));
                        fill_lbl(f, plbl.clone(), Point::new(px + pw * 0.5, by + bh * 0.5), fg, 10.0, Weight::Semibold);
                    }));
                }
            }
            return geoms;
        }
        if self.interactive {
            self.engine.record_viewport_size(size);
        }
        let fts = self.interactive.then(|| self.engine.selected_forecast_timestamp()).flatten();
        let allow_fetch = !self.interactive || !self.engine.is_fast_flying();
        let (base_rev, weather_rev, label_rev, base_fading, label_fading) =
            self.engine.pool.lock().map_or((0, 0, 0, false, false), |mut p| {
                // Drain all finished tile events atomically BEFORE snapshotting revisions & fade flags so GPU caches
                // never render a newly-arrived tile against stale revision/fade state!
                let _ = p.drain_ready();
                (p.base_rev, p.weather_rev, p.label_rev, p.is_base_fading(), p.is_label_fading())
            });
        let style_fading = self.interactive
            && self.engine.prev_base_style.is_some_and(|(ps, t)| {
                ps != self.base_style
                    && ps.is_dark_hud(self.overlay) == self.base_style.is_dark_hud(self.overlay)
                    && t.elapsed() < Duration::from_millis(1400)
            });
        let radar_key = if self.overlay == WeatherOverlay::RadarTimeline {
            self.engine.manifest.as_ref().filter(|m| !m.frames.is_empty()).map(|m| {
                let (i0, i1, bl) = self.radar_blend(m);
                // 64 sub-frame steps per interval => silky full 60Hz VSync advection updates!
                (m.frames.len(), i0, i1, (bl * 64.0).round() as u32)
            })
        } else if self.overlay == WeatherOverlay::InfraredClouds {
            self.engine.manifest.as_ref().filter(|m| !m.satellite.is_empty()).map(|m| {
                let len = m.satellite.len();
                let pos = self.engine.radar.play_pos.clamp(0.0, (len - 1) as f32);
                let (i0, rf) = ((pos.floor() as usize).min(len - 1), (pos - pos.floor()).clamp(0.0, 1.0));
                (len, i0, (rf > 0.02 && i0 + 1 < len).then_some(i0 + 1), (rf * 64.0).round() as u32)
            })
        } else {
            None
        };
        let term_bucket =
            (self.interactive && self.engine.show_terminator).then(|| fts.unwrap_or_else(|| chrono::Utc::now().timestamp()) / 1800);
        let dark_ocean = matches!(self.overlay, WeatherOverlay::WindFlow | WeatherOverlay::InfraredClouds)
            || matches!(self.base_style, BaseMapStyle::AppleDark | BaseMapStyle::Satellite);
        let vp = (size, self.cam.0, self.cam.1, self.cam.2);
        let draw_base = |frame: &mut Frame| {
            let ocean = if dark_ocean { rgb8(26, 32, 42) } else { rgb8(212, 224, 236) };
            frame.fill(&Path::rounded_rectangle(Point::ORIGIN, size, self.corner_r.into()), ocean);
            if let Ok(mut pool) = self.engine.pool.lock() {
                if style_fading && let Some((prev_st, _)) = self.engine.prev_base_style {
                    let (_, pmax, pns, _) = prev_st.spec();
                    let prev = TileLayer {
                        ns: pns,
                        kind: TileLayerKind::Base,
                        max_z: pmax,
                        zoom_bias: tiles::BASE_ZOOM_BIAS,
                        opacity: 1.0,
                        opaque_base: true,
                        fallback: true,
                        fade: true,
                        allow_fetch: false,
                        ttl: Duration::from_secs(86_400 * 7),
                        temporal_fallbacks: [None; 2],
                        shift_px: (0.0, 0.0),
                    };
                    draw_tile_layer(frame, &mut pool, vp, prev, |z, x, y| prev_st.tile_url(z, x, y));
                }
                let (_, max_z, ns, _) = self.base_style.spec();
                let base = TileLayer {
                    ns,
                    kind: TileLayerKind::Base,
                    max_z,
                    zoom_bias: tiles::BASE_ZOOM_BIAS,
                    opacity: 1.0,
                    opaque_base: true,
                    fallback: true,
                    fade: true,
                    allow_fetch,
                    ttl: Duration::from_secs(86_400 * 7),
                    temporal_fallbacks: [None; 2],
                    shift_px: (0.0, 0.0),
                };
                draw_tile_layer(frame, &mut pool, vp, base, |z, x, y| self.base_style.tile_url(z, x, y));
            }
        };
        let draw_weather = |frame: &mut Frame| {
            // Draw day/night twilight terminator at the bottom of `weather_cache` (identical z-order: above basemap,
            // below radar/scalar & labels) so `base_cache` has ZERO invalidations across the entire 12-hour loop!
            if let Some((th, tdst, tfade)) = self
                .interactive
                .then(|| {
                    let ts = fts.unwrap_or_else(|| chrono::Utc::now().timestamp());
                    self.engine.build_terminator_overlay_handle(vp, self.base_style, ts)
                })
                .flatten()
            {
                let img = adv_image::Image::new(th).filter_method(adv_image::FilterMethod::Linear).opacity(tfade);
                frame.with_clip(Rectangle::with_size(size), |fr| fr.draw_image(tdst, img));
            }
            let scalar = self.engine.build_scalar_overlay_handle(vp, self.overlay, self.bundles, fts);
            if let Ok(mut pool) = self.engine.pool.lock() {
                match self.overlay {
                    WeatherOverlay::RadarTimeline => self.draw_radar_timeline(frame, &mut pool, vp, allow_fetch),
                    WeatherOverlay::InfraredClouds if self.draw_satellite_timeline(frame, &mut pool, vp, allow_fetch) => {}
                    WeatherOverlay::NexradUsRadar | WeatherOverlay::InfraredClouds => {
                        let (ns, mz, op, slug) = if self.overlay == WeatherOverlay::NexradUsRadar {
                            ("iem_nexrad", 10, (self.engine.overlay_opacity * 0.95).clamp(0.38, 1.0), "nexrad-n0q-900913")
                        } else if self.cam.1 < -105.0 {
                            ("iem_goes_ir_w", 6, (self.engine.overlay_opacity * 0.85).clamp(0.35, 0.92), "goes_west_fulldisk_ch13")
                        } else {
                            ("iem_goes_ir_e", 6, (self.engine.overlay_opacity * 0.85).clamp(0.35, 0.92), "goes_east_fulldisk_ch13")
                        };
                        let nid = pool.intern(ns);
                        pool.set_active_weather_ns([Some(nid), None, None, None, None, None, None, None]);
                        let l = TileLayer::overlay(ns, TileLayerKind::Weather, mz, op, Duration::from_secs(300), allow_fetch, false);
                        draw_tile_layer_filtered(
                            frame,
                            &mut pool,
                            nid,
                            vp,
                            l,
                            |_, _, _| true,
                            |z, x, y| format!("https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/{slug}/{z}/{x}/{y}.png"),
                        );
                    }
                    _ => {}
                }
                if let Some((sh, dst)) = scalar {
                    let scalar_op = (self.engine.overlay_opacity / 0.85).clamp(0.45, 1.0);
                    let img = adv_image::Image::new(sh).filter_method(adv_image::FilterMethod::Linear).opacity(scalar_op);
                    frame.with_clip(Rectangle::with_size(size), |fr| fr.draw_image(dst, img));
                }
            }
        };
        let draw_labels = |frame: &mut Frame| {
            if let Some((lns, svc)) = self.base_style.label_overlay()
                && let Ok(mut pool) = self.engine.pool.lock()
            {
                let l = TileLayer::overlay(lns, TileLayerKind::Label, 16, 0.96, Duration::from_secs(86_400 * 7), allow_fetch, true)
                    .with_zoom_bias(tiles::LABEL_ZOOM_BIAS);
                draw_tile_layer(frame, &mut pool, vp, l, |z, x, y| BaseMapStyle::label_tile_url(svc, z, x, y));
            }
        };
        if !self.interactive {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                size.width.to_bits(),
                size.height.to_bits(),
                self.cam.0.to_bits(),
                self.cam.1.to_bits(),
                self.cam.2.to_bits(),
                self.base_style,
                self.overlay,
                self.corner_r.to_bits(),
            )
                .hash(&mut h);
            (base_rev, weather_rev, label_rev, self.engine.grid_gen, self.bundles.len(), radar_key).hash(&mut h);
            let sig = h.finish() | 1;
            if base_fading || label_fading || self.engine.mini_tile_sig.get() != sig {
                self.engine.mini_tile_cache.clear();
                self.engine.mini_tile_sig.set(if base_fading || label_fading { 0 } else { sig });
            }
            return vec![self.engine.mini_tile_cache.draw(renderer, size, |frame| {
                draw_base(frame);
                draw_weather(frame);
                draw_labels(frame);
            })];
        }
        // Layered 3-cache GPU compositing for interactive map:
        // Base & Label caches NEVER rebuild during 60fps radar/scalar/wind playback!
        let mut bh = std::collections::hash_map::DefaultHasher::new();
        (
            size.width.to_bits(),
            size.height.to_bits(),
            self.cam.0.to_bits(),
            self.cam.1.to_bits(),
            self.cam.2.to_bits(),
            self.base_style,
            dark_ocean,
            self.corner_r.to_bits(),
            allow_fetch,
            base_rev,
        )
            .hash(&mut bh);
        let bsig = bh.finish() | 1;
        if base_fading || style_fading || self.engine.base_cache_sig.get() != bsig {
            self.engine.base_cache.clear();
            self.engine.base_cache_sig.set(if base_fading || style_fading { 0 } else { bsig });
        }
        let weather_fts_key = (self.overlay != WeatherOverlay::RadarTimeline).then_some(fts.map(|t| t / 180));
        let mut wh = std::collections::hash_map::DefaultHasher::new();
        (
            size.width.to_bits(),
            size.height.to_bits(),
            self.cam.0.to_bits(),
            self.cam.1.to_bits(),
            self.cam.2.to_bits(),
            self.overlay,
            allow_fetch,
            weather_rev,
            self.engine.grid_gen,
            self.bundles.len(),
            radar_key,
            weather_fts_key,
        )
            .hash(&mut wh);
        (term_bucket, self.base_style, self.engine.overlay_opacity.to_bits()).hash(&mut wh);
        let wsig = wh.finish() | 1;
        if self.engine.weather_cache_sig.get() != wsig {
            self.engine.weather_cache.clear();
            self.engine.weather_cache_sig.set(wsig);
        }
        let base_geom = self.engine.base_cache.draw(renderer, size, draw_base);
        let weather_geom = self.engine.weather_cache.draw(renderer, size, draw_weather);
        if self.base_style.label_overlay().is_some() {
            let mut lh = std::collections::hash_map::DefaultHasher::new();
            (
                size.width.to_bits(),
                size.height.to_bits(),
                self.cam.0.to_bits(),
                self.cam.1.to_bits(),
                self.cam.2.to_bits(),
                self.base_style,
                allow_fetch,
                label_rev,
            )
                .hash(&mut lh);
            let lsig = lh.finish() | 1;
            if label_fading || self.engine.label_cache_sig.get() != lsig {
                self.engine.label_cache.clear();
                self.engine.label_cache_sig.set(if label_fading { 0 } else { lsig });
            }
            let label_geom = self.engine.label_cache.draw(renderer, size, draw_labels);
            vec![base_geom, weather_geom, label_geom]
        } else {
            vec![base_geom, weather_geom]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiles::stylize_radar_tile;

    #[test]
    fn mercator_and_radar_stylizer() {
        for (la, lo, z) in [(47.6062, -122.3321, 8.5), (-33.8688, 151.2093, 6.0), (0.0, 0.0, 4.0)] {
            let (wx, wy) = lat_lon_to_world_px(la, lo, z);
            let (rla, rlo) = world_px_to_lat_lon(wx, wy, z);
            assert!((rla - la).abs() < 1e-5 && (rlo - lo).abs() < 1e-5);
        }
        let mut refd = vec![0u8; 64];
        refd[0..4].copy_from_slice(&[135, 138, 134, 240]);
        stylize_radar_tile("hrrr_refd_202611200000_0060", (5, 5, 11), &mut refd, 4, 4);
        assert!(refd[3] < 40, "got alpha {}", refd[3]);
        let mut refp = vec![0u8; 64];
        for i in 0..4 {
            refp[i * 4..i * 4 + 4].copy_from_slice(&[30, 210, 45, 240]);
        }
        for i in 8..12 {
            refp[i * 4..i * 4 + 4].copy_from_slice(&[80, 225, 255, 240]);
        }
        stylize_radar_tile("hrrr_refp_202611200000_0060", (5, 5, 11), &mut refp, 4, 4);
        assert!(refp[6] > refp[5] && refp[7] > 60 && refp[38] >= 200 && refp[39] > 60);
        // Verify ECCC GeoMet RDPS/GDPS `PRECIPPRTMMH-LINEAR` (MATLAB jet) remaps cleanly to Apple Weather blue/purple
        // and `eccc_out_*` masks CONUS interior to 0 while keeping offshore Pacific (z=5, x=4, y=12) opaque!
        let mut eccc_pac = vec![0u8; 64];
        for i in 0..16 {
            eccc_pac[i * 4..i * 4 + 4].copy_from_slice(&[21, 240, 228, 255]); // ~0.5 mm/h cyan in ECCC PRECIPPRTMMH-LINEAR
        }
        assert!(stylize_radar_tile("eccc_out_1795431600", (5, 4, 12), &mut eccc_pac, 4, 4));
        assert!(eccc_pac[2] > eccc_pac[0] && eccc_pac[3] >= 75, "expected offshore ECCC rain blue, got {:?}", &eccc_pac[0..4]);
        let mut eccc_conus = vec![0u8; 64];
        for i in 0..16 {
            eccc_conus[i * 4..i * 4 + 4].copy_from_slice(&[20, 230, 240, 255]);
        }
        assert!(!stylize_radar_tile("eccc_out_1795431600", (5, 7, 12), &mut eccc_conus, 4, 4));
        // Verify LibreWXR scheme 8 preserves straight-RGBA hues without false un-premultiply spikes, softens snow, & despeckles 1px clutter.
        let mut lwxr8 = vec![0u8; 64];
        for i in 0..8 {
            lwxr8[i * 4..i * 4 + 4].copy_from_slice(&[14, 91, 179, 179]); // straight-RGBA rain where b == a (must NOT explode!)
        }
        for i in 8..16 {
            lwxr8[i * 4..i * 4 + 4].copy_from_slice(&[120, 228, 248, 250]); // opaque snow slab (softened!)
        }
        assert!(stylize_radar_tile("lwxr8_1795378800", (5, 4, 11), &mut lwxr8, 4, 4));
        assert!(lwxr8[2] < 215 && lwxr8[3] > 140 && lwxr8[51] < 210, "got rain {:?} snow {:?}", &lwxr8[0..4], &lwxr8[48..52]);
        let mut speck = vec![0u8; 64];
        speck[5 * 4..5 * 4 + 4].copy_from_slice(&[50, 120, 220, 90]); // lone 1px speckle
        assert!(stylize_radar_tile("lwxr8_1795378800", (5, 4, 11), &mut speck, 4, 4));
        assert_eq!(speck[5 * 4 + 3], 0, "expected isolated 1px speckle cleared");
        // Verify NEXRAD Apple Weather palette harmonization & GOES IR clear-sky transparency.
        let mut nex = vec![0u8; 64];
        for i in 0..16 {
            nex[i * 4..i * 4 + 4].copy_from_slice(&[20, 210, 40, 255]);
        }
        assert!(stylize_radar_tile("iem_nexrad", (5, 5, 11), &mut nex, 4, 4));
        assert!(nex[2] > nex[1] && nex[3] > 100, "expected NEXRAD green remapped to Apple blue, got {:?}", &nex[0..4]);
        let mut ir = vec![0u8; 64];
        for i in 0..8 {
            ir[i * 4..i * 4 + 4].copy_from_slice(&[35, 35, 35, 255]);
        }
        for i in 8..16 {
            ir[i * 4..i * 4 + 4].copy_from_slice(&[240, 240, 240, 255]);
        }
        assert!(stylize_radar_tile("iem_goes_ir", (5, 4, 11), &mut ir, 4, 4));
        assert!(ir[3] < 60 && ir[51] > 150, "expected GOES IR clear ground transparent & high clouds luminous");
        // Verify LibreWXR global 512×512 IR satellite lifts cloud tops cleanly while keeping clear pixels transparent.
        let mut lsat = vec![0u8; 64];
        lsat[8 * 4..8 * 4 + 4].copy_from_slice(&[172, 172, 177, 150]);
        assert!(stylize_radar_tile("lwxr_sat_1795496400", (4, 8, 5), &mut lsat, 4, 4));
        assert_eq!(lsat[3], 0);
        assert!(lsat[32] > 210 && lsat[35] > 165, "expected lifted luminous cloud pixel, got {:?}", &lsat[32..36]);
    }

    #[test]
    fn smooth_zoom_and_fling_momentum() {
        let mut eng = MapEngine::new(WeatherClient::new(), 37.8715, -122.273, false, Arc::new(Notify::new()));
        let (settings, bundles, sz, p) = (Settings::default(), HashMap::new(), Size::new(800.0, 600.0), Point::new(560.0, 240.0));
        let (before, z0) = (project_screen_to_lat_lon(p, eng.center_lat, eng.center_lon, eng.zoom, Rectangle::with_size(sz)), eng.zoom);
        let _ = eng.handle_msg(MapMsg::DoubleClickZoom(p, sz), &[], None, &settings);
        assert!(eng.needs_animation_tick(true));
        for _ in 0..40 {
            eng.last_tick = Instant::now() - Duration::from_millis(16);
            eng.tick(&bundles, true);
        }
        let after = project_screen_to_lat_lon(p, eng.center_lat, eng.center_lon, eng.zoom, Rectangle::with_size(sz));
        assert!(eng.zoom > z0 + 0.9 && (before.0 - after.0).abs() < 0.05 && (before.1 - after.1).abs() < 0.05);
        let lon0 = eng.center_lon;
        let _ = eng.handle_msg(MapMsg::PanFling(600.0, 0.0), &[], None, &settings);
        assert!(eng.needs_animation_tick(true));
        eng.last_tick = Instant::now() - Duration::from_millis(16);
        eng.tick(&bundles, true);
        assert!(eng.center_lon < lon0);
        // Verify Apple Maps regional Mercator fly-to with parabolic zoom arc.
        eng.zoom = 6.5;
        eng.fly_to(34.0522, -118.2437, Some(6.5));
        assert!(eng.is_flying());
        let mut min_z = eng.zoom;
        for _ in 0..45 {
            eng.last_tick = Instant::now() - Duration::from_millis(16);
            eng.tick(&bundles, true);
            min_z = min_z.min(eng.zoom);
        }
        assert!(min_z < 6.2 && !eng.is_flying() && (eng.center_lat - 34.0522).abs() < 0.01 && (eng.zoom - 6.5).abs() < 0.01);
        // Verify empty scalar grid returns None (never fabricates a uniform 15°C fallback blanket!).
        eng.overlay = WeatherOverlay::Temperature;
        let vp0 = (sz, eng.center_lat, eng.center_lon, eng.zoom);
        assert!(eng.build_scalar_overlay_handle(vp0, WeatherOverlay::Temperature, &bundles, None).is_none());
        // Seed real samples and verify overscanned geo-anchored scalar texture avoids rebuilds during pan & slight zoom.
        for (la, lo, p) in [(36.0, -121.0, 1026.0_f32), (32.0, -115.0, 1002.0), (38.0, -116.0, 1014.0), (34.0, -122.0, 1015.0)] {
            eng.grid.data.insert(quantize_coord(la, lo), GridWeatherPoint::stationary(la, lo, [15.0, 0.0, 2.0, 0.5, 2.0, p]));
        }
        eng.grid_gen += 1;
        let (h0, r0) = eng.build_scalar_overlay_handle(vp0, WeatherOverlay::Temperature, &bundles, None).unwrap();
        eng.apply_pan_px(90.0, -50.0);
        let vp1 = (sz, eng.center_lat, eng.center_lon, eng.zoom + 0.15);
        let (h1, r1) = eng.build_scalar_overlay_handle(vp1, WeatherOverlay::Temperature, &bundles, None).unwrap();
        assert_eq!(h0, h1, "expected geo-anchored overscan cache hit with zero texture rebuild");
        // Verify RadarTimeline NEVER renders a coarse RBF precipitation blob (all precip comes from real radar/NWP tiles!).
        assert!(eng.build_scalar_overlay_handle(vp0, WeatherOverlay::RadarTimeline, &bundles, Some(1_795_380_000)).is_none());
        let (c0, c1) = (r0.x + r0.width * 0.5, r1.x + r1.width * 0.5);
        assert!((c1 - c0).abs() > 80.0 && r1.width > r0.width + 100.0);
        // Verify Apple Maps latitude-corrected scale bar & theme sync behavior.
        let (sw_eq, lbl_mi) = scale_bar_spec(0.0, 6.0, true);
        let (sw_hi, lbl_km) = scale_bar_spec(60.0, 6.0, false);
        assert!(sw_eq >= 36.0 && lbl_mi.ends_with("mi") && sw_hi >= 36.0 && lbl_km.ends_with("km"));
        eng.sync_theme(true);
        assert_eq!(eng.base_style, BaseMapStyle::CleanLight);
        eng.sync_theme(false);
        assert_eq!(eng.base_style, BaseMapStyle::AppleDark);
        let _ = eng.handle_msg(MapMsg::SelectBaseStyle(BaseMapStyle::Satellite), &[], None, &settings);
        eng.sync_theme(true);
        assert_eq!(eng.base_style, BaseMapStyle::Satellite);
        // Verify Pressure marching-squares isobar contours & H/L center detection.
        for (la, lo, p) in [(36.0, -121.0, 1026.0_f32), (32.0, -115.0, 1002.0), (38.0, -116.0, 1014.0), (34.0, -122.0, 1015.0)] {
            eng.grid.data.insert(quantize_coord(la, lo), GridWeatherPoint::stationary(la, lo, [15.0, 0.0, 2.0, 0.5, 2.0, p]));
        }
        eng.grid_gen += 1;
        let iso = eng.build_pressure_isobars((sz, 35.0, -118.0, 5.5), &bundles, None);
        assert!(iso.segs.len() > 20 && !iso.labels.is_empty(), "expected pressure isobar contours, got {}", iso.segs.len());
        // Verify astronomical Day/Night solar terminator overlay + overscan cache hit + street-level fade-out.
        let (th0, _, tf0) = eng.build_terminator_overlay_handle((sz, 35.0, -118.0, 4.5), BaseMapStyle::CleanLight, 1_795_200_000).unwrap();
        let (th1, _, tf1) = eng.build_terminator_overlay_handle((sz, 35.4, -117.5, 4.6), BaseMapStyle::CleanLight, 1_795_200_000).unwrap();
        assert_eq!(th0, th1, "expected terminator geo-anchored cache hit");
        assert!(tf0 > 0.9 && tf1 > 0.9);
        assert!(eng.build_terminator_overlay_handle((sz, 35.0, -118.0, 9.0), BaseMapStyle::CleanLight, 1_795_200_000).is_none());
        // Verify multi-city overview framing (including antimeridian-spanning Tokyo <-> San Francisco).
        let sf = Location::new("San Francisco", "CA", "US", "US", 37.7749, -122.4194, "America/Los_Angeles");
        let tokyo = Location::new("Tokyo", "", "Japan", "JP", 35.6762, 139.6503, "Asia/Tokyo");
        let (olat, olon, oz) = compute_overview_camera(&[sf.clone(), tokyo.clone()], sz).unwrap();
        assert!((25.0..=45.0).contains(&olat) && olon.abs() > 140.0 && (2.3..=6.0).contains(&oz), "pacific overview=({olat},{olon},{oz})");
        // Verify Smart Locate toggle (6.8 <-> 10.5), full-screen toggle, playback speed cycle, and right-click context menu.
        let saved = vec![sf.clone(), tokyo];
        (eng.center_lat, eng.center_lon, eng.zoom) = (sf.lat, sf.lon, 6.8);
        let _ = eng.handle_msg(MapMsg::RecenterActive, &saved, Some(&sf.id), &settings);
        assert!(matches!(eng.motion, CamMotion::Fly { to: (_, _, z), .. } if (z - 10.5).abs() < 0.01));
        let _ = eng.handle_msg(MapMsg::ToggleFullScreen, &saved, Some(&sf.id), &settings);
        assert!(eng.is_full_screen());
        let _ = eng.handle_msg(MapMsg::CyclePlaybackSpeed, &saved, Some(&sf.id), &settings);
        assert!((eng.playback_speed - 2.0).abs() < 0.01);
        let _ = eng.handle_msg(MapMsg::OpenContextMenu(Point::new(300.0, 220.0), sz), &saved, Some(&sf.id), &settings);
        assert!(eng.has_context_menu());
        let ext = eng.handle_msg(MapMsg::ContextCopyCoords, &saved, Some(&sf.id), &settings);
        assert!(matches!(ext, Some(MapExternalAction::CopyText(s)) if s.contains(',')) && !eng.has_context_menu() && eng.toast.is_some());
        // Verify one-handed DragZoom preserves anchor point.
        let z_pre = eng.zoom;
        let _ = eng.handle_msg(MapMsg::DragZoom(p, sz, 0.75), &saved, Some(&sf.id), &settings);
        assert!((eng.zoom - (z_pre + 0.75)).abs() < 0.01);
    }

    #[test]
    fn timeline_player_math_and_state_transitions() {
        let mut eng = MapEngine::new(WeatherClient::new(), 37.8715, -122.273, false, Arc::new(Notify::new()));
        let (settings, bundles) = (Settings::default(), HashMap::new());
        // 1. Temperature / Wind / Pressure provide a clean uniform 12h timeline even with NO radar manifest!
        for ov in [WeatherOverlay::Temperature, WeatherOverlay::WindFlow, WeatherOverlay::Pressure] {
            let _ = eng.handle_msg(MapMsg::SelectOverlay(ov), &[], None, &settings);
            let tl = eng.active_timeline().expect("continuous 12h timeline without manifest");
            assert_eq!(tl.live_idx, 0);
            assert_eq!(tl.times.len(), 25);
            assert_eq!(tl.times[24] - tl.times[0], 12 * 3600);
            assert!((tl.time_to_u(tl.times[0]) - 0.0).abs() < 1e-4);
            assert!((tl.time_to_u(tl.times[0] + 6 * 3600) - 0.5).abs() < 1e-4);
            assert!((tl.time_to_u(tl.times[24]) - 1.0).abs() < 1e-4);
            for step in 0..=24 {
                let u = tl.pos_to_u(step as f32);
                let back = tl.u_to_pos(u);
                assert!((back - step as f32).abs() < 0.05, "roundtrip {step} -> {u} -> {back}");
            }
        }
        // 2. Precipitation (-1h .. Now .. +12h with uneven 10m + 1h frames) maps Now -> 0.20, +3h -> 0.40, +6h -> 0.60, +9h -> 0.80, +12h -> 1.00!
        let now = 1_795_380_000_i64;
        let mut frames = Vec::new();
        for m in [-60_i64, -50, -40, -30, -20, -10, 0] {
            frames.push(RainViewerFrame { time_unix: now + m * 60, is_nowcast: false, source: RadarFrameSource::EcccNwp });
        }
        for m in [10_i64, 20, 30, 60, 90, 120, 180, 240, 360, 480, 600, 720] {
            frames.push(RainViewerFrame { time_unix: now + m * 60, is_nowcast: true, source: RadarFrameSource::EcccNwp });
        }
        eng.manifest = Some(RainViewerManifest { host: String::new(), frames, satellite: Vec::new() });
        let _ = eng.handle_msg(MapMsg::SelectOverlay(WeatherOverlay::RadarTimeline), &[], None, &settings);
        let tl = eng.active_timeline().expect("radar timeline");
        assert_eq!(tl.live_idx, 6);
        assert!((tl.time_to_u(now - 3600) - 0.0).abs() < 1e-4);
        assert!((tl.time_to_u(now) - 0.20).abs() < 1e-4);
        assert!((tl.time_to_u(now + 3 * 3600) - 0.40).abs() < 1e-4);
        assert!((tl.time_to_u(now + 6 * 3600) - 0.60).abs() < 1e-4);
        assert!((tl.time_to_u(now + 9 * 3600) - 0.80).abs() < 1e-4);
        assert!((tl.time_to_u(now + 12 * 3600) - 1.00).abs() < 1e-4);
        // 3. Pressing Play at the end of the timeline immediately wraps to frame 0 and starts playing.
        let _ = eng.handle_msg(MapMsg::ScrubRadar(tl.max_idx()), &[], None, &settings);
        eng.radar.play_pos = tl.max_idx() as f32;
        let _ = eng.handle_msg(MapMsg::ToggleRadarPlay, &[], None, &settings);
        assert!(eng.radar_playing() && eng.radar.frame_idx == 0);
        // 4. Switching to a non-timeline overlay stops playback so we never burn 60fps idle frames!
        let _ = eng.handle_msg(MapMsg::SelectOverlay(WeatherOverlay::AirQuality), &[], None, &settings);
        assert!(!eng.radar_playing() && eng.active_timeline().is_none());
        // 5. Scrubbing past radar (-30m) then switching to Temperature clamps cleanly to Live (index 0).
        let _ = eng.handle_msg(MapMsg::SelectOverlay(WeatherOverlay::RadarTimeline), &[], None, &settings);
        let _ = eng.handle_msg(MapMsg::ScrubRadarPos(3.0), &[], None, &settings);
        let _ = eng.handle_msg(MapMsg::SelectOverlay(WeatherOverlay::Temperature), &[], None, &settings);
        assert_eq!(eng.radar.frame_idx, 0);
        assert!((eng.radar.play_pos - 0.0).abs() < 1e-3);
        // 6. Playing Temperature advances smoothly across ticks.
        let _ = eng.handle_msg(MapMsg::ToggleRadarPlay, &[], None, &settings);
        for _ in 0..10 {
            eng.last_tick = Instant::now() - Duration::from_millis(40);
            eng.tick(&bundles, true);
        }
        assert!(eng.radar.play_pos > 1.0 && eng.selected_forecast_timestamp().is_some());
        // 7. Global 12-hour Satellite IR overlay exposes a -11h..Now timeline & honors opacity updates.
        let sat: Vec<SatelliteFrame> = (0..12_i64)
            .map(|h| SatelliteFrame { time_unix: now - (11 - h) * 3600, path: format!("/v2/satellite/{}", now - (11 - h) * 3600) })
            .collect();
        if let Some(m) = eng.manifest.as_mut() {
            m.satellite = sat;
        }
        let _ = eng.handle_msg(MapMsg::SelectOverlay(WeatherOverlay::InfraredClouds), &[], None, &settings);
        let stl = eng.active_timeline().expect("satellite IR timeline");
        assert_eq!(stl.live_idx, 11);
        assert!((stl.time_to_u(now - 11 * 3600) - 0.0).abs() < 1e-4);
        assert!((stl.time_to_u(now) - 1.0).abs() < 1e-4);
        let _ = eng.handle_msg(MapMsg::SetOverlayOpacity(0.60), &[], None, &settings);
        assert!((eng.overlay_opacity - 0.60).abs() < 1e-3);
    }

    #[test]
    fn replay_viewport_readiness_and_buffering_gate() {
        // California coast viewport (z=6.2): visible tiles span BOTH HRRR land AND offshore Pacific ECCC!
        let mut eng = MapEngine::new(WeatherClient::new(), 37.7749, -122.4194, false, Arc::new(Notify::new()));
        eng.zoom = 6.2;
        let vp_sz = Size::new(1280.0, 800.0);
        eng.record_viewport_size(vp_sz);
        let vp: MapVp = (vp_sz, eng.center_lat, eng.center_lon, eng.zoom);
        let now = 1_795_380_000_i64;
        let f0 = RainViewerFrame { time_unix: now, is_nowcast: false, source: RadarFrameSource::NoaaHrrr("202611231200".into(), 0, true) };
        let f1 =
            RainViewerFrame { time_unix: now + 3600, is_nowcast: true, source: RadarFrameSource::NoaaHrrr("202611231200".into(), 1, true) };
        eng.set_manifest(RainViewerManifest { host: String::new(), frames: vec![f0.clone(), f1.clone()], satellite: Vec::new() });
        eng.overlay = WeatherOverlay::RadarTimeline;

        // 1. Before any tiles are loaded, frame 0 is NOT viewport-ready; paused map does NOT spin 60fps idle frames
        //    (background prewarming wakes cleanly on tile arrival via `wake: Arc<Notify>` instead of burning CPU/GPU!).
        assert!(!eng.needs_animation_tick(true));
        {
            let pool = eng.pool.lock().unwrap();
            assert!(!is_radar_frame_viewport_ready(&f0, &pool, vp));
        }

        // 2. Loading ONLY the single center HRRR tile must NOT mark a coastal viewport ready (offshore ECCC + side tiles still missing!).
        let hrrr_ns0 = f0.tile_namespace().unwrap();
        let (cz, cx, cy) = {
            let z = effective_tile_zoom_biased(eng.zoom, f0.max_native_zoom(), f0.primary_zoom_bias());
            let (x, y) = tile_coord_at(eng.center_lat, eng.center_lon, z);
            (z, x, y)
        };
        {
            let mut pool = eng.pool.lock().unwrap();
            let id = pool.intern(&hrrr_ns0);
            pool.insert_ready_for_test(TileKey(id, cz, cx, cy));
            assert!(!is_radar_frame_viewport_ready(&f0, &pool, vp), "single center tile must not pass coastal viewport readiness");
        }

        // 3. Settling ALL visible HRRR + ECCC tier tiles for f0 makes f0 viewport-ready while f1 remains cold.
        let settle_frame = |pool: &mut SharedTilePool, f: &RainViewerFrame| {
            let is_hrrr = matches!(f.source, RadarFrameSource::NoaaHrrr(..));
            if let Some(ns) = f.tile_namespace() {
                let id = pool.intern(&ns);
                for_each_visible_tile_biased(vp, f.max_native_zoom(), f.primary_zoom_bias(), |z, x, y, _| {
                    if !is_hrrr || overlays::tile_intersects_hrrr(z, x, y) {
                        pool.insert_ready_for_test(TileKey(id, z, x, y));
                    }
                    true
                });
            }
            if let Some(tiers) = f.eccc_tier_namespaces() {
                for (model, ens) in EcccModel::ALL.into_iter().zip(tiers) {
                    let id = pool.intern(&ens);
                    for_each_visible_tile_biased(vp, model.max_native_zoom(), model.zoom_bias(), |z, x, y, _| {
                        if eccc_tier_tile_needed(model, is_hrrr, z, x, y, vp) {
                            pool.insert_ready_for_test(TileKey(id, z, x, y));
                        }
                        true
                    });
                }
            }
        };
        {
            let mut pool = eng.pool.lock().unwrap();
            settle_frame(&mut pool, &f0);
            assert!(is_radar_frame_viewport_ready(&f0, &pool, vp));
            assert!(!is_radar_frame_viewport_ready(&f1, &pool, vp));
        }

        // 4. Starting playback with f0 ready and f1 cold: playback glides smoothly across f0 up to frac >= 0.92
        //    (giving f1 the full frame window to finish downloading!), then buffers cleanly before crossing into cold f1!
        let bundles = HashMap::new();
        eng.radar.snap(0);
        eng.radar.playing = true;
        for _ in 0..20 {
            eng.last_tick = Instant::now() - Duration::from_millis(50);
            eng.tick(&bundles, true);
        }
        assert!(eng.radar.stall_secs > 0.0, "expected playback to buffer at end of f0 while f1 is still unloaded");
        assert!(
            eng.radar.play_pos >= 0.90 && eng.radar.play_pos < 1.0,
            "expected play_pos to glide across ready f0 and hold before crossing into cold f1, got {}",
            eng.radar.play_pos
        );

        // 5. The moment f1's visible tiles finish settling, stall_secs resets to 0 and playback glides smoothly into f1!
        {
            let mut pool = eng.pool.lock().unwrap();
            settle_frame(&mut pool, &f1);
            assert!(is_radar_frame_viewport_ready(&f1, &pool, vp));
        }
        for _ in 0..4 {
            eng.last_tick = Instant::now() - Duration::from_millis(50);
            eng.tick(&bundles, true);
        }
        assert_eq!(eng.radar.stall_secs, 0.0);
        assert!(eng.radar.play_pos >= 1.0, "expected smooth progression to frame 1 once settled, got {}", eng.radar.play_pos);
    }

    #[test]
    fn eccc_wms_url_insets_only_antimeridian_edges() {
        let bbox = |url: String| -> [f64; 4] {
            let raw = url.split("BBOX=").nth(1).unwrap().split('&').next().unwrap();
            let v: Vec<f64> = raw.split(',').map(|s| s.parse::<f64>().unwrap()).collect();
            [v[0], v[1], v[2], v[3]]
        };
        const R: f64 = 20_037_508.342_789_244;
        for z in [2_u8, 3, 4] {
            let n = 1_u32 << z;
            let b0 = bbox(RainViewerFrame::eccc_wms_url(1_795_471_200, z, 0, 1));
            let b1 = bbox(RainViewerFrame::eccc_wms_url(1_795_471_200, z, 1, 1));
            let blast = bbox(RainViewerFrame::eccc_wms_url(1_795_471_200, z, n - 1, 1));
            // Antimeridian outer edges are inset by >= 16 km so ECCC GeoMet stays in smooth bilinear mode.
            assert!(b0[0] > -R + 15_900.0, "z={z} x=0 minx must inset from -R: {}", b0[0]);
            assert!(blast[2] < R - 15_900.0, "z={z} x=n-1 maxx must inset from +R: {}", blast[2]);
            // Interior boundary between x=0 and x=1 remains pixel-exact (zero interior seam!).
            assert!((b0[2] - b1[0]).abs() < 0.05, "z={z} interior edge mismatch: {} vs {}", b0[2], b1[0]);
        }
    }

    #[test]
    fn cancelled_active_weather_tile_invalidates_weather_rev() {
        let (tx, _rx) = mpsc::channel();
        let mut pool = SharedTilePool::new(WeatherClient::new(), tx, Arc::new(Notify::new()));
        let nid = pool.intern("eccc_rdps_out_v3_1795471200");
        let k = TileKey(nid, 5, 4, 12);
        pool.set_active_weather_ns([Some(nid), None, None, None, None, None, None, None]);
        pool.in_flight.insert(k);
        let rev_before = pool.weather_rev;
        pool.tile_tx.send(tiles::TileEvent::Cancelled(k)).unwrap();
        assert!(pool.drain_ready(), "cancelled active weather tile must mark dirty so draw() re-enqueues it");
        assert!(!pool.in_flight.contains(&k));
        assert_ne!(pool.weather_rev, rev_before);
    }

    #[test]
    fn nwp_hierarchical_partition_of_unity_is_exact_globally() {
        // Verify HRRR (3 km) -> HRDPS (2.5 km) -> RDPS (10 km) -> GDPS (15 km) forms an exact
        // partition of unity (sum == 1.000) everywhere on Earth AND picks the finest model in every key region!
        for lat_i in -80..=85 {
            for lon_i in (-180..=180).step_by(3) {
                let (lat, lon) = (lat_i as f64 + 0.37, lon_i as f64 + 0.21);
                let wh = overlays::hrrr_weight(lat, lon);
                let wd = overlays::hrdps_weight(lat, lon);
                let wr = overlays::rdps_weight(lat, lon);
                let m_hrrr = wh;
                let m_hrdps = (1.0 - wh) * wd;
                let m_rdps = (1.0 - wh) * (1.0 - wd) * wr;
                let m_gdps = (1.0 - wh) * (1.0 - wd) * (1.0 - wr);
                let sum = m_hrrr + m_hrdps + m_rdps + m_gdps;
                assert!((sum - 1.0).abs() < 1e-5, "partition failed at ({lat}, {lon}): sum={sum}");
            }
        }
        // Key metro spot-checks:
        // Chicago (CONUS interior): 100% HRRR 3 km
        assert_eq!(overlays::hrrr_weight(41.88, -87.63), 1.0);
        // Vancouver / Toronto (Canada): 0% HRRR, 100% HRDPS 2.5 km!
        assert_eq!(overlays::hrrr_weight(52.0, -123.1), 0.0);
        assert_eq!(overlays::hrdps_weight(52.0, -123.1), 1.0);
        // Offshore California Pacific (37.7°N, -135°W): 0% HRRR, 0% HRDPS, 100% RDPS 10 km (NOT 15 km GDPS!)
        assert_eq!(overlays::hrrr_weight(37.7, -135.0), 0.0);
        assert_eq!(overlays::hrdps_weight(37.7, -135.0), 0.0);
        assert_eq!(overlays::rdps_weight(37.7, -135.0), 1.0);
        // London / Sydney: 100% GDPS 15 km
        assert_eq!(overlays::rdps_weight(51.5, -0.12), 0.0);
        assert_eq!(overlays::rdps_weight(-33.87, 151.21), 0.0);
    }

    #[test]
    #[ignore]
    fn bench_live_radar_replay_buffering() {
        let client = WeatherClient::new();
        let m = client.fetch_rainviewer_manifest().expect("live manifest");
        println!("\n=== LIVE RADAR REPLAY BUFFERING BENCHMARK ({} frames) ===", m.frames.len());
        let _ = tiles::take_lane_stats();
        for &(label, lat, lon, zoom, w, h) in &[
            ("Berkeley 1360x860 z=5.6", 37.8779, -122.2672, 5.6, 1360.0_f32, 860.0_f32),
            ("Berkeley 2560x1440 z=5.5", 37.8779, -122.2672, 5.5, 2560.0_f32, 1440.0_f32),
            ("Berkeley 2560x1440 z=6.2", 37.8779, -122.2672, 6.2, 2560.0_f32, 1440.0_f32),
            ("Seattle 2560x1440 z=6.0", 47.6062, -122.3321, 6.0, 2560.0_f32, 1440.0_f32),
            ("NYC Metro 1920x1080 z=7.5", 40.7128, -74.0060, 7.5, 1920.0_f32, 1080.0_f32),
            ("London 2560x1440 z=5.8", 51.5072, -0.1276, 5.8, 2560.0_f32, 1440.0_f32),
        ] {
            let mut eng = MapEngine::new(client.clone(), lat, lon, false, Arc::new(Notify::new()));
            eng.zoom = zoom;
            eng.record_viewport_size(Size::new(w, h));
            eng.set_manifest(m.clone());
            // Prevent MapEngine::new's background manifest thread from replacing `m` mid-benchmark!
            let (_tx, rx) = mpsc::channel();
            eng.rx = rx;
            eng.last_manifest = Some(Instant::now());
            eng.overlay = WeatherOverlay::RadarTimeline;
            let li = m.frames.iter().rposition(|f| !f.is_nowcast).unwrap_or(0);
            eng.radar.snap(li);
            let vp: MapVp = (Size::new(w, h), lat, lon, zoom);
            let bundles = HashMap::new();
            let settings = Settings::default();
            // Simulate worst case: expand map and IMMEDIATELY hit Play (0 ms idle prewarm!)
            let t0 = Instant::now();
            let _ = eng.handle_msg(MapMsg::ToggleRadarPlay, &[], None, &settings);
            let mut first_ready_ms = None;
            let mut all_ready_ms = None;
            let mut frame_ready_ms: Vec<Option<u128>> = vec![None; m.frames.len()];
            let mut stall_ticks = 0_u32;
            let mut max_stall_s = 0.0_f32;
            let mut stalled_frames: Vec<(usize, String, usize)> = Vec::new();
            while t0.elapsed() < Duration::from_secs(12) {
                eng.tick(&bundles, true);
                let _ = eng.take_pending_gpu_pins(48);
                let (ready_cnt, inflight_cnt) = {
                    let pool = eng.pool.lock().unwrap();
                    let mut rc = 0;
                    for (idx, f) in m.frames.iter().enumerate() {
                        if is_radar_frame_viewport_ready(f, &pool, vp) {
                            rc += 1;
                            if frame_ready_ms[idx].is_none() {
                                frame_ready_ms[idx] = Some(t0.elapsed().as_millis());
                            }
                        }
                    }
                    (rc, pool.in_flight.len())
                };
                if ready_cnt > 0 && first_ready_ms.is_none() {
                    first_ready_ms = Some(t0.elapsed().as_millis());
                }
                if ready_cnt == m.frames.len() && all_ready_ms.is_none() {
                    all_ready_ms = Some(t0.elapsed().as_millis());
                }
                if eng.radar.stall_secs > 0.0 {
                    stall_ticks += 1;
                    max_stall_s = max_stall_s.max(eng.radar.stall_secs);
                    let ci = eng.radar.frame_idx;
                    let ni = (ci + 1).min(m.frames.len() - 1);
                    let pool = eng.pool.lock().unwrap();
                    let miss_ci = !is_radar_frame_viewport_ready(&m.frames[ci], &pool, vp);
                    let who = if miss_ci { ci } else { ni };
                    if stalled_frames.last().map(|x| x.0) != Some(who) {
                        stalled_frames.push((who, format!("{:?}", m.frames[who].source), inflight_cnt));
                    }
                }
                if all_ready_ms.is_some() && t0.elapsed() >= Duration::from_secs(4) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(16));
            }
            let per_f: Vec<String> = frame_ready_ms
                .iter()
                .enumerate()
                .map(|(i, ms)| format!("f{i}:{}", ms.map_or_else(|| "MISS".into(), |v| format!("{v}ms"))))
                .collect();
            println!(
                "  {label:28} | first_ready={:>4} ms | all_ready={:>5} ms | stall_ticks={:>3} ({:>4} ms) | max_stall={:.2}s | stalled={:?}",
                first_ready_ms.unwrap_or(9999),
                all_ready_ms.unwrap_or(9999),
                stall_ticks,
                stall_ticks * 16,
                max_stall_s,
                stalled_frames,
            );
            println!("    per-frame ready: {}", per_f.join(" "));
            let lanes: Vec<String> = tiles::take_lane_stats()
                .iter()
                .filter(|(_, s)| s[0] > 0)
                .map(|(n, s)| format!("{n}: {} req, {} KB, avg {} ms, max {} ms, {} failed", s[0], s[1] / 1024, s[2] / s[0], s[3], s[4]))
                .collect();
            println!("    network: {}", lanes.join(" | "));
        }
    }

    #[test]
    fn optical_flow_advection_and_continuous_eccc_hours() {
        // 1. Every forecast frame maps to one ECCC interpolation slot: `q` quarter-hours after UTC hour `h0`.
        let slot = |t: i64| RainViewerFrame { time_unix: t, is_nowcast: true, source: RadarFrameSource::EcccNwp }.eccc_slot();
        assert_eq!(slot(1_700_002_800), (1_700_002_800, 0));
        assert_eq!(slot(1_700_002_800 + 900), (1_700_002_800, 1));
        assert_eq!(slot(1_700_002_800 + 2700), (1_700_002_800, 3));
        // Timestamps within 7.5 min of the next hour belong to that hour's own raster, never a 4th quarter.
        assert_eq!(slot(1_700_002_800 + 3300), (1_700_006_400, 0));
        let f =
            RainViewerFrame { time_unix: 1_700_002_800 + 1800, is_nowcast: true, source: RadarFrameSource::NoaaHrrr("x".into(), 30, true) };
        assert_eq!(f.eccc_tier_namespaces().unwrap()[1], "eccc_rdps_out_v7_1700002800_2");

        // 2. Optical-flow estimator recovers a moving storm blob's screen-space displacement accurately!
        let (tx, _rx) = mpsc::channel();
        let mut pool = SharedTilePool::new(WeatherClient::new(), tx, Arc::new(Notify::new()));
        let (ns_a, ns_b) = (pool.intern("hrrr_refp_a"), pool.intern("hrrr_refp_b"));
        let vp: MapVp = (Size::new(640.0, 480.0), 37.8, -122.2, 6.0);
        let tz = effective_tile_zoom_biased(vp.3, 6, -0.35);
        let (tpx, c) = (256.0 * 2.0_f64.powf(vp.3 - tz as f64), lat_lon_to_world_px(vp.1, vp.2, vp.3));
        let (true_dx, true_dy) = (24.0_f64, -14.0_f64);
        for_each_visible_tile_biased(vp, 6, -0.35, |z, wx, wy, _| {
            let mk = |shift_x: f64, shift_y: f64| {
                let mut arr = [0_u8; tiles::FLOW_THUMB_N * tiles::FLOW_THUMB_N];
                for py in 0..tiles::FLOW_THUMB_N {
                    for px in 0..tiles::FLOW_THUMB_N {
                        let gx = (wx as f64 + (px as f64 + 0.5) / tiles::FLOW_THUMB_N as f64) * tpx - c.0;
                        let gy = (wy as f64 + (py as f64 + 0.5) / tiles::FLOW_THUMB_N as f64) * tpx - c.1;
                        let d2 = (gx - shift_x).powi(2) + (gy - shift_y).powi(2);
                        arr[py * tiles::FLOW_THUMB_N + px] = (230.0 * (-d2 / (2.0 * 52.0 * 52.0)).exp()).round() as u8;
                    }
                }
                Arc::new(arr)
            };
            pool.insert_thumb_for_test(TileKey(ns_a, z, wx, wy), mk(-12.0, 8.0));
            pool.insert_thumb_for_test(TileKey(ns_b, z, wx, wy), mk(-12.0 + true_dx, 8.0 + true_dy));
            true
        });
        let (dx, dy) = tiles::estimate_flow_px(&pool, &[(ns_a, 6, -0.35)], &[(ns_b, 6, -0.35)], vp);
        assert!(
            (dx - true_dx as f32).abs() < 3.5 && (dy - true_dy as f32).abs() < 3.5,
            "expected flow ≈ ({true_dx}, {true_dy}), got ({dx:.2}, {dy:.2})"
        );
    }
}
