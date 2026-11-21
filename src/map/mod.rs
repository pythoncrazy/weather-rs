mod hud;
mod overlays;
mod tiles;

pub use overlays::{lat_lon_to_world_px, world_px_to_lat_lon};

use crate::icons::wind_to_color;
use crate::model::{Location, Settings, WeatherBundle};
use crate::providers::{
    GridWeatherPoint, RadarFrameSource, RainViewerFrame, RainViewerManifest, WeatherClient, wind_uv,
};
use crate::storage::is_shutting_down;
use crate::style::{fw, rgb8, rgba8};
use iced::advanced::image as adv_image;
use iced::font::Weight;
use iced::keyboard::{self, Key, key::Named};
use iced::mouse;
use iced::widget::canvas::{
    self, Action, Event, Frame, Geometry, Path, Stroke, Text as CanvasText,
};
use iced::{Alignment, Color, Point, Rectangle, Renderer, Size, Theme};
use overlays::{
    CityBadge, WIND_TRAIL_LEN, WindParticle, declutter_badges, idw_wind, init_wind_particles,
    project_lat_lon_to_screen, project_screen_to_lat_lon, quantize_coord, wrap_lon,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tiles::{
    MapViewport, SharedTilePool, TileKey, TileLayer, TileWorkers, draw_tile_layer,
    effective_tile_zoom, tile_coord_at,
};
use tokio::sync::Notify;

pub type LabelTileFn = fn(u8, u32, u32) -> String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseMapStyle {
    CleanLight,
    AppleDark,
    Satellite,
    Terrain,
    Streets,
}

impl BaseMapStyle {
    pub const ALL: [Self; 5] = [
        Self::CleanLight,
        Self::AppleDark,
        Self::Satellite,
        Self::Terrain,
        Self::Streets,
    ];
    fn spec(self) -> (&'static str, u8, &'static str, &'static str) {
        const SPECS: [(&str, u8, &str, &str); 5] = [
            (
                "Standard Light",
                16,
                "esri_light_base",
                "Canvas/World_Light_Gray_Base",
            ),
            (
                "Dark Canvas",
                16,
                "esri_dark_base",
                "Canvas/World_Dark_Gray_Base",
            ),
            ("Satellite", 17, "esri_world_imagery", "World_Imagery"),
            ("Topographic", 17, "esri_world_topo", "World_Topo_Map"),
            ("OpenStreetMap", 18, "osm_standard", ""),
        ];
        SPECS[self as usize]
    }
    pub fn label(self) -> &'static str {
        self.spec().0
    }
    pub fn max_zoom(self) -> u8 {
        self.spec().1
    }
    pub fn namespace(self) -> &'static str {
        self.spec().2
    }
    pub fn tile_url(self, z: u8, x: u32, y: u32) -> String {
        let p = self.spec().3;
        if p.is_empty() {
            format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png")
        } else {
            format!(
                "https://services.arcgisonline.com/ArcGIS/rest/services/{p}/MapServer/tile/{z}/{y}/{x}"
            )
        }
    }
    pub fn label_overlay(self) -> Option<(&'static str, LabelTileFn)> {
        fn places(z: u8, x: u32, y: u32) -> String {
            format!(
                "https://services.arcgisonline.com/ArcGIS/rest/services/Reference/World_Boundaries_and_Places/MapServer/tile/{z}/{y}/{x}"
            )
        }
        fn light(z: u8, x: u32, y: u32) -> String {
            format!(
                "https://services.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Light_Gray_Reference/MapServer/tile/{z}/{y}/{x}"
            )
        }
        match self {
            Self::CleanLight => Some(("esri_light_labels", light)),
            Self::AppleDark | Self::Satellite => Some(("esri_places_labels", places)),
            _ => None,
        }
    }
    pub fn is_dark_hud(self, ov: WeatherOverlay) -> bool {
        ov == WeatherOverlay::WindFlow || matches!(self, Self::AppleDark | Self::Satellite)
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
        const L: [&str; 7] = [
            "Precipitation",
            "Temperature",
            "Air Quality",
            "Wind",
            "Cloud Satellite",
            "Pressure",
            "US NEXRAD HD",
        ];
        L[self as usize]
    }
}

#[derive(Debug, Clone)]
pub enum MapMsg {
    ExpandMap,
    CloseMap,
    ToggleLayerMenu,
    SelectOverlay(WeatherOverlay),
    SelectBaseStyle(BaseMapStyle),
    PanStart,
    PanPixels(f32, f32),
    PanFling(f32, f32),
    WheelZoom(Point, Size, f64),
    DoubleClickZoom(Point, Size),
    ZoomStep(f64),
    RecenterActive,
    ToggleRadarPlay,
    StepTimeline(i32),
    ScrubRadar(usize),
    ScrubRadarPos(f32),
    JumpToLive,
    ClickMap(Point, Size),
    CloseInspector,
    SaveInspectedLocation,
    HudConsume,
}

pub enum MapExternalAction {
    SetExpanded(bool),
    SelectSaved(String),
    SaveLocation(Location),
}

enum WorkerEvent {
    ManifestLoaded(RainViewerManifest),
    GridSamples(Vec<(i32, i32)>, Vec<GridWeatherPoint>),
    AqiSamples(Vec<(i32, i32)>, Vec<(f64, f64, f64)>),
    PointInspected(u64, Result<Box<WeatherBundle>, String>),
}

#[derive(Default)]
pub(super) struct InspectState {
    pub coord: Option<(f64, f64)>,
    pub loading: bool,
    pub bundle: Option<WeatherBundle>,
    pub error: Option<String>,
    pub seq: u64,
}

type WindGrid = [(f32, f32, f32); 192];

#[derive(Clone, Copy)]
struct ZoomAnim {
    target_zoom: f64,
    anchor_geo: (f64, f64),
    anchor_px: Point,
    viewport: Size,
}

pub struct MapEngine {
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
    target_cam: Option<(f64, f64, f64)>,
    zoom_anim: Option<ZoomAnim>,
    pan_vel: (f64, f64),
    pub base_style: BaseMapStyle,
    pub overlay: WeatherOverlay,
    pub show_layer_menu: bool,
    pub radar_playing: bool,
    pub radar_frame_idx: usize,
    radar_play_pos: f32,
    radar_target_pos: f32,
    radar_loop_hold: f32,
    last_tick: Instant,
    last_cam_move: Instant,
    steering_mps: (f64, f64),
    last_steering: Option<Instant>,
    manifest: Option<RainViewerManifest>,
    last_manifest: Option<Instant>,
    last_prewarmed_strip: Option<(u8, u32, u32, usize)>,
    pool: Arc<Mutex<SharedTilePool>>,
    rx: mpsc::Receiver<WorkerEvent>,
    wake: Arc<Notify>,
    client: WeatherClient,
    grid_cache: HashMap<(i32, i32), GridWeatherPoint>,
    in_flight_grid: HashSet<(i32, i32)>,
    aqi_cache: HashMap<(i32, i32), (f64, f64, f64)>,
    in_flight_aqi: HashSet<(i32, i32)>,
    grid_gen: u64,
    last_grid_poll: Option<Instant>,
    scalar_tex: Mutex<Option<(u64, adv_image::Handle)>>,
    wind_grid: Option<(u64, WindGrid)>,
    particles: Vec<WindParticle>,
    pub(super) inspect: InspectState,
}

impl MapEngine {
    pub fn new(client: WeatherClient, lat: f64, lon: f64, light: bool, wake: Arc<Notify>) -> Self {
        let (tx, rx) = mpsc::channel();
        let (tile_tx, tile_rx) = mpsc::channel();
        let workers = TileWorkers::spawn(client.clone(), tile_tx.clone(), Arc::clone(&wake));
        let pool = Arc::new(Mutex::new(SharedTilePool {
            tiles: HashMap::new(),
            lru: VecDeque::new(),
            in_flight: HashSet::new(),
            failed_until: HashMap::new(),
            tx,
            tile_tx,
            tile_rx,
            workers,
        }));
        let now = Instant::now();
        let mut engine = Self {
            center_lat: lat,
            center_lon: lon,
            zoom: 5.0,
            target_cam: None,
            zoom_anim: None,
            pan_vel: (0.0, 0.0),
            base_style: BaseMapStyle::CleanLight,
            overlay: WeatherOverlay::RadarTimeline,
            show_layer_menu: false,
            radar_playing: false,
            radar_frame_idx: 0,
            radar_play_pos: 0.0,
            radar_target_pos: 0.0,
            radar_loop_hold: 0.0,
            last_tick: now,
            last_cam_move: now,
            steering_mps: (6.0, 1.5),
            last_steering: None,
            manifest: None,
            last_manifest: None,
            last_prewarmed_strip: None,
            pool,
            rx,
            wake,
            client,
            grid_cache: HashMap::new(),
            in_flight_grid: HashSet::new(),
            aqi_cache: HashMap::new(),
            in_flight_aqi: HashSet::new(),
            grid_gen: 1,
            last_grid_poll: None,
            scalar_tex: Mutex::new(None),
            wind_grid: None,
            particles: init_wind_particles(),
            inspect: InspectState::default(),
        };
        engine.prewarm_region(lat, lon, false, light);
        engine.spawn_manifest_fetch();
        engine
    }

    fn spawn_manifest_fetch(&mut self) {
        self.last_manifest = Some(Instant::now());
        let (c, tx, wk) = (
            self.client.clone(),
            self.pool.lock().expect("pool").tx.clone(),
            Arc::clone(&self.wake),
        );
        let _ = std::thread::Builder::new()
            .name("radar-manifest".into())
            .spawn(move || {
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

    pub fn inspected_coord(&self) -> Option<(f64, f64)> {
        self.inspect.coord
    }

    fn prewarm_3x3(
        p: &mut SharedTilePool,
        ns: &str,
        z: u8,
        (lat, lon): (f64, f64),
        ttl: Duration,
        high: bool,
        url: impl Fn(u32, u32) -> String,
    ) {
        let (n, (cx, cy)) = (1_i64 << z, tile_coord_at(lat, lon, z));
        for dy in -1..=1_i64 {
            for dx in -1..=1_i64 {
                let (tx, ty) = (
                    (cx as i64 + dx).rem_euclid(n) as u32,
                    (cy as i64 + dy).clamp(0, n - 1) as u32,
                );
                p.ensure(ns, (z, tx, ty), || url(tx, ty), ttl, high, true);
            }
        }
    }

    fn prewarm_style(
        p: &mut SharedTilePool,
        st: BaseMapStyle,
        z: u8,
        coord: (f64, f64),
        high: bool,
    ) {
        let ttl = Duration::from_secs(86_400 * 7);
        Self::prewarm_3x3(p, st.namespace(), z, coord, ttl, high, |x, y| {
            st.tile_url(z, x, y)
        });
        if let Some((lns, lfn)) = st.label_overlay() {
            Self::prewarm_3x3(p, lns, z, coord, ttl, high, |x, y| lfn(z, x, y));
        }
    }

    pub fn prewarm_saved_locations(&mut self, locs: &[Location], light: bool) {
        let Ok(mut p) = self.pool.lock() else { return };
        let ttl = Duration::from_secs(86_400 * 7);
        // Pin active theme's world z2 fallback tiles so epoch bumps never cancel global fallback coverage.
        let st = if light {
            BaseMapStyle::CleanLight
        } else {
            BaseMapStyle::AppleDark
        };
        for wy in 0..4_u32 {
            for wx in 0..4_u32 {
                p.ensure(
                    st.namespace(),
                    (2, wx, wy),
                    || st.tile_url(2, wx, wy),
                    ttl,
                    false,
                    false,
                );
            }
        }
        let mini_st = if light {
            BaseMapStyle::Terrain
        } else {
            BaseMapStyle::AppleDark
        };
        for loc in locs.iter().take(6) {
            Self::prewarm_style(&mut p, mini_st, 5, (loc.lat, loc.lon), false);
            Self::prewarm_style(&mut p, self.base_style, 5, (loc.lat, loc.lon), false);
        }
    }

    pub fn prewarm_region(&mut self, lat: f64, lon: f64, expanded: bool, light: bool) {
        let Ok(mut p) = self.pool.lock() else { return };
        let mini_st = if light {
            BaseMapStyle::Terrain
        } else {
            BaseMapStyle::AppleDark
        };
        if expanded {
            let z = effective_tile_zoom(self.zoom, self.base_style.max_zoom());
            Self::prewarm_style(&mut p, self.base_style, z, (lat, lon), true);
            Self::prewarm_style(&mut p, mini_st, 5, (lat, lon), false);
        } else {
            Self::prewarm_style(&mut p, mini_st, 5, (lat, lon), true);
            Self::prewarm_style(&mut p, self.base_style, 5, (lat, lon), false);
        }
    }

    pub fn jump_to_location(&mut self, lat: f64, lon: f64, expanded: bool, light: bool) {
        self.clear_inspector();
        self.last_grid_poll = None;
        self.last_steering = None;
        self.last_prewarmed_strip = None;
        if let Ok(mut g) = self.scalar_tex.lock() {
            *g = None;
        }
        if let Ok(mut p) = self.pool.lock() {
            p.bump_epoch();
        }
        if expanded {
            self.fly_to(lat, lon, None);
        } else {
            self.target_cam = None;
            (self.center_lat, self.center_lon) = (lat.clamp(-85.0, 85.0), wrap_lon(lon));
            self.last_cam_move = Instant::now();
        }
        self.prewarm_region(lat, lon, expanded, light);
        if expanded && self.overlay == WeatherOverlay::RadarTimeline {
            self.prewarm_live_radar();
        }
    }

    fn prewarm_live_radar(&self) {
        let Some(m) = self.manifest.as_ref().filter(|m| !m.frames.is_empty()) else {
            return;
        };
        let li = self.live_frame_idx().min(m.frames.len() - 1);
        self.prewarm_radar_frame(&m.host, &m.frames[li], true);
        if let Some(next) = m.frames.get(li + 1) {
            self.prewarm_radar_frame(&m.host, next, false);
        }
    }

    pub fn live_frame_idx(&self) -> usize {
        self.manifest
            .as_ref()
            .and_then(|m| m.frames.iter().rposition(|f| !f.is_nowcast))
            .unwrap_or(0)
    }
    fn overlay_supports_timeline(ov: WeatherOverlay) -> bool {
        matches!(
            ov,
            WeatherOverlay::RadarTimeline
                | WeatherOverlay::Temperature
                | WeatherOverlay::WindFlow
                | WeatherOverlay::Pressure
        )
    }

    pub fn selected_forecast_timestamp(&self) -> Option<i64> {
        if !Self::overlay_supports_timeline(self.overlay) {
            return None;
        }
        let m = self.manifest.as_ref().filter(|m| !m.frames.is_empty())?;
        let (li, max_i) = (self.live_frame_idx(), m.frames.len() - 1);
        let min_i = if self.overlay == WeatherOverlay::RadarTimeline {
            0
        } else {
            li
        };
        if !self.radar_playing {
            let ti = (self.radar_target_pos.round() as usize).clamp(min_i, max_i);
            if !m.frames[ti].is_nowcast && ti <= li {
                return None;
            }
        }
        let mut pos = self.radar_play_pos.clamp(min_i as f32, max_i as f32);
        if !self.radar_playing && self.radar_target_pos > li as f32 && pos <= li as f32 {
            pos = (li + 1).min(max_i) as f32;
        }
        let (i0, fr) = (
            (pos.floor() as usize).min(max_i),
            (pos - pos.floor()).clamp(0.0, 1.0),
        );
        let (f0, f1) = (&m.frames[i0], &m.frames[(i0 + 1).min(max_i)]);
        if !f0.is_nowcast && (!f1.is_nowcast || fr <= 0.001) {
            return None;
        }
        Some(
            (f0.time_unix as f64 + (f1.time_unix - f0.time_unix) as f64 * fr as f64).round() as i64,
        )
    }

    fn is_frame_ready(&self, f: &RainViewerFrame) -> bool {
        let (Some(ns), Ok(p)) = (f.tile_namespace(), self.pool.lock()) else {
            return true;
        };
        let z = effective_tile_zoom(self.zoom, f.max_native_zoom().min(7));
        let (x, y) = tile_coord_at(self.center_lat, self.center_lon, z);
        p.has_tile(&ns, (z, x, y))
            || p.failed_until
                .get(&TileKey(ns, z, x, y))
                .is_some_and(|u| *u > Instant::now())
    }

    pub fn prewarm_radar_frame(&self, host: &str, f: &RainViewerFrame, high: bool) {
        let (Some(ns), Ok(mut p)) = (f.tile_namespace(), self.pool.lock()) else {
            return;
        };
        let z = effective_tile_zoom(self.zoom, f.max_native_zoom().min(7));
        Self::prewarm_3x3(
            &mut p,
            &ns,
            z,
            (self.center_lat, self.center_lon),
            Duration::from_secs(900),
            high,
            |x, y| f.tile_url(host, z, x, y).unwrap_or_default(),
        );
    }

    pub fn prewarm_timeline_strip(&mut self) {
        let Some(m) = self.manifest.clone().filter(|m| !m.frames.is_empty()) else {
            return;
        };
        let z = effective_tile_zoom(self.zoom, 7);
        let (cx, cy) = tile_coord_at(self.center_lat, self.center_lon, z);
        let key = (z, cx, cy, m.frames.len());
        if self.last_prewarmed_strip == Some(key) {
            return;
        }
        self.last_prewarmed_strip = Some(key);
        let li = self.live_frame_idx().min(m.frames.len() - 1);
        for i in li..m.frames.len() {
            self.prewarm_radar_frame(&m.host, &m.frames[i], i <= li + 1);
        }
        for i in (0..li).rev() {
            self.prewarm_radar_frame(&m.host, &m.frames[i], false);
        }
    }

    pub fn fly_to(&mut self, lat: f64, lon: f64, zoom: Option<f64>) {
        let (tla, tlo, tz) = (
            lat.clamp(-85.0, 85.0),
            wrap_lon(lon),
            zoom.unwrap_or(self.zoom),
        );
        self.zoom_anim = None;
        self.pan_vel = (0.0, 0.0);
        self.last_cam_move = Instant::now();
        self.last_prewarmed_strip = None;
        let dl = wrap_lon(tlo - self.center_lon);
        if (tla - self.center_lat).hypot(dl) > 20.0 {
            // Intercontinental jump: snap straight to destination to avoid flooding ocean tiles.
            (self.center_lat, self.center_lon, self.zoom, self.target_cam) = (tla, tlo, tz, None);
            if let Ok(mut p) = self.pool.lock() {
                p.bump_epoch();
            }
        } else {
            self.target_cam = Some((tla, tlo, tz));
        }
    }

    fn is_fast_flying(&self) -> bool {
        self.target_cam.is_some_and(|(tla, tlo, _)| {
            (tla - self.center_lat).hypot(wrap_lon(tlo - self.center_lon)) > 2.5
        }) || self.pan_vel.0.hypot(self.pan_vel.1) > 420.0
    }

    pub fn needs_animation_tick(&self, expanded: bool) -> bool {
        let scrub = Self::overlay_supports_timeline(self.overlay)
            && (self.radar_play_pos - self.radar_target_pos).abs() > 0.006;
        expanded
            && (self.target_cam.is_some()
                || self.zoom_anim.is_some()
                || self.pan_vel.0.hypot(self.pan_vel.1) > 6.0
                || self.radar_playing
                || scrub
                || self.overlay == WeatherOverlay::WindFlow)
    }

    pub fn in_flight_count(&self) -> usize {
        self.pool.lock().map(|p| p.in_flight.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn is_flying(&self) -> bool {
        self.target_cam.is_some()
    }

    #[cfg(test)]
    pub fn has_pending_work(&self, expanded: bool) -> bool {
        self.needs_animation_tick(expanded)
            || self.pool.lock().is_ok_and(|p| !p.in_flight.is_empty())
            || self.inspect.loading
            || !self.in_flight_grid.is_empty()
            || !self.in_flight_aqi.is_empty()
    }

    pub fn tick(&mut self, bundles: &HashMap<String, WeatherBundle>, expanded: bool) -> bool {
        let mut dirty = self.pool.lock().is_ok_and(|mut p| p.drain_ready());
        while let Ok(ev) = self.rx.try_recv() {
            dirty = true;
            match ev {
                WorkerEvent::ManifestLoaded(m) => {
                    let was_live = self.manifest.is_none()
                        || (!self.radar_playing && self.radar_frame_idx == self.live_frame_idx());
                    let li = m
                        .frames
                        .iter()
                        .rposition(|f| !f.is_nowcast)
                        .unwrap_or(m.frames.len().saturating_sub(1));
                    if was_live || self.radar_frame_idx >= m.frames.len() {
                        (
                            self.radar_frame_idx,
                            self.radar_play_pos,
                            self.radar_target_pos,
                        ) = (li, li as f32, li as f32);
                    }
                    self.manifest = Some(m);
                    self.last_prewarmed_strip = None;
                    if expanded {
                        self.prewarm_live_radar();
                    }
                }
                WorkerEvent::GridSamples(req_keys, pts) => {
                    for q in req_keys {
                        self.in_flight_grid.remove(&q);
                    }
                    for pt in pts {
                        let q = quantize_coord(pt.lat, pt.lon);
                        self.in_flight_grid.remove(&q);
                        self.grid_cache.insert(q, pt);
                    }
                    prune_geo_cache(
                        &mut self.grid_cache,
                        (self.center_lat, self.center_lon),
                        |p| (p.lat, p.lon),
                    );
                    self.grid_gen = self.grid_gen.wrapping_add(1);
                }
                WorkerEvent::AqiSamples(req_keys, pts) => {
                    for q in req_keys {
                        self.in_flight_aqi.remove(&q);
                    }
                    if !pts.is_empty() {
                        for (la, lo, a) in pts {
                            let q = quantize_coord(la, lo);
                            self.in_flight_aqi.remove(&q);
                            self.aqi_cache.insert(q, (la, lo, a));
                        }
                        prune_geo_cache(
                            &mut self.aqi_cache,
                            (self.center_lat, self.center_lon),
                            |&(la, lo, _)| (la, lo),
                        );
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
        if self
            .last_manifest
            .is_none_or(|t| t.elapsed() > Duration::from_secs(300))
        {
            self.spawn_manifest_fetch();
        }
        let dt = self.last_tick.elapsed().as_secs_f32().clamp(0.0, 0.08);
        self.last_tick = Instant::now();
        if let Some((tla, tlo, tz)) = self.target_cam {
            let (k, dl) = (
                (1.0 - (-12.5 * dt.max(0.008)).exp()) as f64,
                wrap_lon(tlo - self.center_lon),
            );
            self.center_lat += (tla - self.center_lat) * k;
            self.center_lon = wrap_lon(self.center_lon + dl * k);
            self.zoom += (tz - self.zoom) * k;
            if (tla - self.center_lat).abs() < 0.0012
                && dl.abs() < 0.0012
                && (tz - self.zoom).abs() < 0.006
            {
                (self.center_lat, self.center_lon, self.zoom, self.target_cam) =
                    (tla, tlo, tz, None);
            }
            dirty = true;
        }
        if let Some(za) = self.zoom_anim {
            let k = (1.0 - (-18.0 * dt.max(0.008)).exp()) as f64;
            self.zoom += (za.target_zoom - self.zoom) * k;
            if (za.target_zoom - self.zoom).abs() < 0.004 {
                self.zoom = za.target_zoom;
                self.zoom_anim = None;
            }
            let (awx, awy) = lat_lon_to_world_px(za.anchor_geo.0, za.anchor_geo.1, self.zoom);
            let (cla, clo) = world_px_to_lat_lon(
                awx - (za.anchor_px.x - za.viewport.width * 0.5) as f64,
                awy - (za.anchor_px.y - za.viewport.height * 0.5) as f64,
                self.zoom,
            );
            (self.center_lat, self.center_lon) = (cla.clamp(-85.0, 85.0), wrap_lon(clo));
            self.last_cam_move = Instant::now();
            dirty = true;
        }
        let spd = self.pan_vel.0.hypot(self.pan_vel.1);
        if spd > 6.0 {
            let step_dt = dt.max(0.008) as f64;
            let (dx, dy) = (self.pan_vel.0 * step_dt, self.pan_vel.1 * step_dt);
            self.apply_pan_px(dx, dy);
            let decay = (-6.8 * step_dt).exp();
            self.pan_vel = (self.pan_vel.0 * decay, self.pan_vel.1 * decay);
            if self.pan_vel.0.hypot(self.pan_vel.1) <= 6.0 {
                self.pan_vel = (0.0, 0.0);
            }
            self.last_cam_move = Instant::now();
            dirty = true;
        } else if spd > 0.0 {
            self.pan_vel = (0.0, 0.0);
        }
        if expanded {
            dirty |= self.tick_expanded(bundles, dt);
        }
        dirty
    }

    fn tick_expanded(&mut self, bundles: &HashMap<String, WeatherBundle>, dt: f32) -> bool {
        let mut dirty = false;
        if self.overlay == WeatherOverlay::RadarTimeline
            && (self.radar_playing
                || (self.target_cam.is_none()
                    && self.last_cam_move.elapsed() >= Duration::from_millis(600)
                    && self.in_flight_count() <= 8))
        {
            self.prewarm_timeline_strip();
        }
        if self.overlay == WeatherOverlay::RadarTimeline
            && self
                .last_steering
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(400))
        {
            self.last_steering = Some(Instant::now());
            let pts = self.all_weather_points(bundles, self.selected_forecast_timestamp());
            if !pts.is_empty() {
                let (wu, wv, _) = idw_wind(&pts, self.center_lat, self.center_lon);
                self.steering_mps.0 +=
                    ((wu * 1.45).clamp(-32.0, 32.0) - self.steering_mps.0) * 0.45;
                self.steering_mps.1 +=
                    ((wv * 1.45).clamp(-32.0, 32.0) - self.steering_mps.1) * 0.45;
            }
        }
        if self.radar_playing
            && Self::overlay_supports_timeline(self.overlay)
            && let Some(m) = &self.manifest
            && m.frames.len() > 1
        {
            let (min_i, max_i) = (
                if self.overlay == WeatherOverlay::RadarTimeline {
                    0
                } else {
                    self.live_frame_idx()
                },
                m.frames.len() - 1,
            );
            let base = self.radar_play_pos.clamp(min_i as f32, max_i as f32);
            let ci = (base.floor() as usize).clamp(min_i, max_i);
            if ci >= max_i {
                (
                    self.radar_play_pos,
                    self.radar_target_pos,
                    self.radar_frame_idx,
                ) = (max_i as f32, max_i as f32, max_i);
                self.radar_loop_hold += dt;
                if self.radar_loop_hold >= 0.65 {
                    (
                        self.radar_loop_hold,
                        self.radar_play_pos,
                        self.radar_target_pos,
                        self.radar_frame_idx,
                    ) = (0.0, min_i as f32, min_i as f32, min_i);
                }
            } else {
                self.radar_loop_hold = 0.0;
                let ni = (ci + 1).min(max_i);
                if self.overlay != WeatherOverlay::RadarTimeline
                    || self.is_frame_ready(&m.frames[ni])
                {
                    let mins =
                        (m.frames[ni].time_unix - m.frames[ci].time_unix).max(300) as f32 / 60.0;
                    let np = (base
                        + dt / if mins <= 15.0 {
                            0.62
                        } else if mins <= 30.0 {
                            0.84
                        } else {
                            1.08
                        })
                    .min(max_i as f32);
                    (
                        self.radar_play_pos,
                        self.radar_target_pos,
                        self.radar_frame_idx,
                    ) = (np, np, (np.floor() as usize).clamp(min_i, max_i));
                } else {
                    for k in 0..=2 {
                        if let Some(f) = m.frames.get(ni + k) {
                            self.prewarm_radar_frame(&m.host, f, true);
                        }
                    }
                }
            }
            dirty = true;
        } else if Self::overlay_supports_timeline(self.overlay) {
            let diff = self.radar_target_pos - self.radar_play_pos;
            if diff.abs() > 0.006 {
                if diff.abs() > 2.2 {
                    self.radar_play_pos = self.radar_target_pos - diff.signum();
                }
                self.radar_play_pos += (self.radar_target_pos - self.radar_play_pos)
                    * (1.0 - (-15.0 * dt.max(0.008)).exp());
                if (self.radar_target_pos - self.radar_play_pos).abs() <= 0.006 {
                    self.radar_play_pos = self.radar_target_pos;
                }
                dirty = true;
            } else {
                self.radar_play_pos = self.radar_target_pos;
            }
        }
        if matches!(
            self.overlay,
            WeatherOverlay::Temperature
                | WeatherOverlay::AirQuality
                | WeatherOverlay::WindFlow
                | WeatherOverlay::Pressure
        ) || (self.overlay == WeatherOverlay::RadarTimeline
            && (self.radar_playing || self.selected_forecast_timestamp().is_some()))
        {
            self.poll_grid_samples();
        }
        if self.overlay == WeatherOverlay::WindFlow {
            self.step_wind_particles(bundles, dt);
            dirty = true;
        }
        dirty
    }

    fn poll_grid_samples(&mut self) {
        if self
            .last_grid_poll
            .is_some_and(|t| t.elapsed() < Duration::from_millis(650))
        {
            return;
        }
        self.last_grid_poll = Some(Instant::now());
        let span = (135.0 / 2.0_f64.powf(self.zoom - 2.0)).clamp(2.0, 55.0);
        let (mut nw, mut na) = (Vec::new(), Vec::new());
        for gy in 0..4 {
            for gx in 0..5 {
                let q = quantize_coord(
                    (self.center_lat + ((gy as f64 - 1.5) / 3.0) * span).clamp(-80.0, 80.0),
                    wrap_lon(self.center_lon + ((gx as f64 - 2.0) / 4.0) * span * 1.4),
                );
                let pt = (q.0 as f64 * 0.5, q.1 as f64 * 0.5);
                if self.overlay == WeatherOverlay::AirQuality {
                    if !self.aqi_cache.contains_key(&q) && self.in_flight_aqi.insert(q) {
                        na.push(pt);
                    }
                } else if !self.grid_cache.contains_key(&q) && self.in_flight_grid.insert(q) {
                    nw.push(pt);
                }
            }
        }
        if !nw.is_empty() {
            let (c, tx, wk) = (
                self.client.clone(),
                self.pool.lock().expect("pool").tx.clone(),
                Arc::clone(&self.wake),
            );
            let keys: Vec<_> = nw.iter().map(|&(la, lo)| quantize_coord(la, lo)).collect();
            std::thread::spawn(move || {
                let mut pts = c.fetch_grid_batch(&nw);
                if pts.is_empty() {
                    pts = keys
                        .iter()
                        .map(|&(a, b)| {
                            GridWeatherPoint::stationary(
                                a as f64 * 0.5,
                                b as f64 * 0.5,
                                [15.0, 0.0, 2.0, 0.5, 2.0, 1013.25],
                            )
                        })
                        .collect();
                }
                let _ = tx.send(WorkerEvent::GridSamples(keys, pts));
                wk.notify_one();
            });
        }
        if !na.is_empty() {
            let (c, tx, wk) = (
                self.client.clone(),
                self.pool.lock().expect("pool").tx.clone(),
                Arc::clone(&self.wake),
            );
            let keys: Vec<_> = na.iter().map(|&(la, lo)| quantize_coord(la, lo)).collect();
            std::thread::spawn(move || {
                let pts = c.fetch_aqi_grid_batch(&na);
                let _ = tx.send(WorkerEvent::AqiSamples(keys, pts));
                wk.notify_one();
            });
        }
    }

    pub(super) fn all_weather_points(
        &self,
        bundles: &HashMap<String, WeatherBundle>,
        ts: Option<i64>,
    ) -> Vec<GridWeatherPoint> {
        let mut pts: Vec<_> = self
            .grid_cache
            .values()
            .map(|p| p.at_timestamp(ts))
            .collect();
        for b in bundles.values() {
            let v = if let Some(t) = ts
                && !b.hourly.is_empty()
            {
                let (h0, h1, f) = match b.hourly.binary_search_by_key(&t, |h| h.timestamp_unix) {
                    Ok(i) => (&b.hourly[i], &b.hourly[i], 0.0_f32),
                    Err(0) => (&b.hourly[0], &b.hourly[0], 0.0),
                    Err(i) if i >= b.hourly.len() => {
                        let l = &b.hourly[b.hourly.len() - 1];
                        (l, l, 0.0)
                    }
                    Err(i) => {
                        let (a, c) = (&b.hourly[i - 1], &b.hourly[i]);
                        (
                            a,
                            c,
                            ((t - a.timestamp_unix) as f32
                                / (c.timestamp_unix - a.timestamp_unix).max(1) as f32)
                                .clamp(0.0, 1.0),
                        )
                    }
                };
                let ((u0, v0), (u1, v1)) = (
                    wind_uv(h0.wind_speed_mps, h0.wind_dir_deg),
                    wind_uv(h1.wind_speed_mps, h1.wind_dir_deg),
                );
                let (wu, wv) = (u0 + (u1 - u0) * f, v0 + (v1 - v0) * f);
                [
                    (h0.temp_c + (h1.temp_c - h0.temp_c) * f as f64) as f32,
                    (h0.precip_mm + (h1.precip_mm - h0.precip_mm) * f as f64) as f32,
                    wu,
                    wv,
                    wu.hypot(wv),
                    (h0.pressure_hpa + (h1.pressure_hpa - h0.pressure_hpa) * f as f64) as f32,
                ]
            } else {
                let (u, v) = wind_uv(b.current.wind_speed_mps, b.current.wind_dir_deg);
                [
                    b.current.temp_c as f32,
                    b.current.precip_mm as f32,
                    u,
                    v,
                    b.current.wind_speed_mps as f32,
                    b.current.pressure_hpa as f32,
                ]
            };
            pts.push(GridWeatherPoint::stationary(
                b.location.lat,
                b.location.lon,
                v,
            ));
        }
        pts
    }

    pub(super) fn all_aqi_points(
        &self,
        bundles: &HashMap<String, WeatherBundle>,
    ) -> Vec<(f64, f64, f64)> {
        let mut pts: Vec<_> = self.aqi_cache.values().copied().collect();
        for b in bundles.values() {
            if let Some(aq) = &b.air_quality {
                pts.push((b.location.lat, b.location.lon, aq.us_aqi as f64));
            }
        }
        pts
    }

    fn jump_radar_idx(&mut self, idx: usize, with_next: bool) {
        (
            self.radar_playing,
            self.radar_loop_hold,
            self.radar_frame_idx,
            self.radar_target_pos,
        ) = (false, 0.0, idx, idx as f32);
        if let Some(m) = &self.manifest {
            for i in [Some(idx), with_next.then_some(idx + 1)]
                .into_iter()
                .flatten()
            {
                if let Some(f) = m.frames.get(i) {
                    self.prewarm_radar_frame(&m.host, f, true);
                }
            }
        }
    }

    fn clear_inspector(&mut self) {
        self.inspect = InspectState {
            seq: self.inspect.seq.wrapping_add(1),
            ..Default::default()
        };
    }

    pub fn handle_msg(
        &mut self,
        msg: MapMsg,
        saved: &[Location],
        active_id: Option<&str>,
        settings: &Settings,
    ) -> Option<MapExternalAction> {
        match msg {
            MapMsg::ExpandMap => {
                let light = settings.theme_mode == crate::model::ThemeMode::Light;
                if let Some(l) = active_id.and_then(|id| saved.iter().find(|l| l.id == id)) {
                    (self.center_lat, self.center_lon, self.target_cam) = (l.lat, l.lon, None);
                    self.prewarm_region(l.lat, l.lon, true, light);
                }
                self.last_prewarmed_strip = None;
                self.last_grid_poll = None;
                self.prewarm_live_radar();
                return Some(MapExternalAction::SetExpanded(true));
            }
            MapMsg::CloseMap => {
                self.show_layer_menu = false;
                self.target_cam = None;
                self.zoom_anim = None;
                self.pan_vel = (0.0, 0.0);
                self.radar_playing = false;
                self.radar_loop_hold = 0.0;
                return Some(MapExternalAction::SetExpanded(false));
            }
            MapMsg::ToggleLayerMenu => self.show_layer_menu = !self.show_layer_menu,
            MapMsg::SelectOverlay(ov) => {
                (self.overlay, self.show_layer_menu) = (ov, false);
                self.last_grid_poll = None;
                if ov == WeatherOverlay::RadarTimeline {
                    self.last_prewarmed_strip = None;
                    self.prewarm_live_radar();
                } else {
                    if ov == WeatherOverlay::WindFlow
                        && self.base_style == BaseMapStyle::CleanLight
                        && let Ok(mut p) = self.pool.lock()
                    {
                        let z = effective_tile_zoom(self.zoom, BaseMapStyle::AppleDark.max_zoom());
                        Self::prewarm_style(
                            &mut p,
                            BaseMapStyle::AppleDark,
                            z,
                            (self.center_lat, self.center_lon),
                            true,
                        );
                    }
                    let li = self.live_frame_idx();
                    if self.radar_frame_idx < li {
                        (
                            self.radar_frame_idx,
                            self.radar_play_pos,
                            self.radar_target_pos,
                        ) = (li, li as f32, li as f32);
                    }
                }
            }
            MapMsg::SelectBaseStyle(st) => {
                (self.base_style, self.show_layer_menu) = (st, false);
                if let Ok(mut p) = self.pool.lock() {
                    p.bump_epoch();
                }
                self.prewarm_region(self.center_lat, self.center_lon, true, false);
            }
            MapMsg::PanStart => {
                self.show_layer_menu = false;
                self.target_cam = None;
                self.zoom_anim = None;
                self.pan_vel = (0.0, 0.0);
            }
            MapMsg::PanPixels(dx, dy) => {
                self.show_layer_menu = false;
                (
                    self.target_cam,
                    self.zoom_anim,
                    self.pan_vel,
                    self.last_cam_move,
                ) = (None, None, (0.0, 0.0), Instant::now());
                self.apply_pan_px(dx as f64, dy as f64);
            }
            MapMsg::PanFling(vx, vy) => {
                let (vx, vy) = (vx as f64, vy as f64);
                let mag = vx.hypot(vy);
                if mag > 45.0 {
                    let cap = mag.min(2400.0) / mag;
                    self.pan_vel = (vx * cap, vy * cap);
                    self.last_tick = Instant::now();
                }
            }
            MapMsg::WheelZoom(p, sz, dz) => self.cursor_zoom_smooth(p, sz, dz),
            MapMsg::DoubleClickZoom(p, sz) => {
                self.show_layer_menu = false;
                self.clear_inspector();
                self.cursor_zoom_smooth(p, sz, 1.15);
            }
            MapMsg::ZoomStep(dz) => {
                let base_z = self
                    .target_cam
                    .map(|c| c.2)
                    .or_else(|| self.zoom_anim.map(|z| z.target_zoom))
                    .unwrap_or(self.zoom);
                self.fly_to(
                    self.center_lat,
                    self.center_lon,
                    Some((base_z + dz).clamp(2.2, 16.0)),
                );
            }
            MapMsg::RecenterActive => {
                if let Some(l) = active_id.and_then(|id| saved.iter().find(|l| l.id == id)) {
                    self.fly_to(l.lat, l.lon, Some(6.8));
                }
            }
            MapMsg::ToggleRadarPlay => {
                self.radar_playing = !self.radar_playing;
                self.radar_loop_hold = 0.0;
                self.last_tick = Instant::now();
                if self.radar_playing {
                    self.prewarm_timeline_strip();
                }
            }
            MapMsg::StepTimeline(delta) => {
                if Self::overlay_supports_timeline(self.overlay)
                    && let Some(m) = &self.manifest
                    && !m.frames.is_empty()
                {
                    let max_i = m.frames.len() - 1;
                    let min_i = if self.overlay == WeatherOverlay::RadarTimeline {
                        0
                    } else {
                        self.live_frame_idx().min(max_i)
                    };
                    let cur =
                        (self.radar_target_pos.round() as i32).clamp(min_i as i32, max_i as i32);
                    let next = (cur + delta).clamp(min_i as i32, max_i as i32) as usize;
                    self.jump_radar_idx(next, delta > 0);
                }
            }
            MapMsg::ScrubRadar(idx) => self.jump_radar_idx(idx, true),
            MapMsg::ScrubRadarPos(pos) => {
                (self.radar_playing, self.radar_loop_hold) = (false, 0.0);
                if let Some(m) = &self.manifest
                    && !m.frames.is_empty()
                {
                    let max_i = m.frames.len() - 1;
                    let c = pos.clamp(0.0, max_i as f32);
                    let i0 = (c.floor() as usize).min(max_i);
                    let i1 = (i0 + 1).min(max_i);
                    (
                        self.radar_play_pos,
                        self.radar_target_pos,
                        self.radar_frame_idx,
                    ) = (c, c, (c.round() as usize).min(max_i));
                    self.prewarm_radar_frame(&m.host, &m.frames[i0], true);
                    if i1 != i0 {
                        self.prewarm_radar_frame(&m.host, &m.frames[i1], true);
                    }
                }
            }
            MapMsg::JumpToLive => self.jump_radar_idx(self.live_frame_idx(), false),
            MapMsg::ClickMap(pt, sz) => {
                if self.show_layer_menu {
                    self.show_layer_menu = false;
                    return None;
                }
                let rect = Rectangle::with_size(sz);
                let tmp: Vec<CityBadge> = saved
                    .iter()
                    .map(|l| CityBadge {
                        title: String::new(),
                        lat: l.lat,
                        lon: l.lon,
                        val: String::new(),
                        sub: String::new(),
                        col: Color::WHITE,
                        active: active_id == Some(l.id.as_str()),
                        is_current_location: l.is_current_location,
                        wind_deg: None,
                    })
                    .collect();
                for (idx, b, _, dc) in
                    declutter_badges(&tmp, self.center_lat, self.center_lon, self.zoom, rect)
                {
                    if (dc.x - pt.x).hypot(dc.y - pt.y) <= if b.active { 24.0 } else { 18.0 } {
                        self.fly_to(saved[idx].lat, saved[idx].lon, Some(self.zoom.max(6.8)));
                        return Some(MapExternalAction::SelectSaved(saved[idx].id.clone()));
                    }
                }
                let (la, lo) = project_screen_to_lat_lon(
                    pt,
                    self.center_lat,
                    self.center_lon,
                    self.zoom,
                    rect,
                );
                self.trigger_inspect(la, lo, settings);
            }
            MapMsg::CloseInspector => self.clear_inspector(),
            MapMsg::SaveInspectedLocation => {
                return self.inspect.bundle.take().map(|b| {
                    self.inspect.coord = None;
                    self.inspect.error = None;
                    MapExternalAction::SaveLocation(b.location)
                });
            }
            MapMsg::HudConsume => {}
        }
        None
    }

    fn apply_pan_px(&mut self, dx: f64, dy: f64) {
        let (ws, (wx, wy)) = (
            256.0 * 2.0_f64.powf(self.zoom),
            lat_lon_to_world_px(self.center_lat, self.center_lon, self.zoom),
        );
        let (la, lo) = world_px_to_lat_lon(wx - dx, (wy - dy).clamp(0.0, ws), self.zoom);
        (self.center_lat, self.center_lon) = (la.clamp(-85.0, 85.0), wrap_lon(lo));
    }

    fn cursor_zoom_smooth(&mut self, p: Point, sz: Size, dz: f64) {
        self.target_cam = None;
        self.pan_vel = (0.0, 0.0);
        self.last_cam_move = Instant::now();
        self.last_tick = Instant::now();
        let (ala, alo) = project_screen_to_lat_lon(
            p,
            self.center_lat,
            self.center_lon,
            self.zoom,
            Rectangle::with_size(sz),
        );
        let base_z = self
            .zoom_anim
            .filter(|z| (z.anchor_px.x - p.x).hypot(z.anchor_px.y - p.y) < 48.0)
            .map_or(self.zoom, |z| z.target_zoom);
        let target_zoom = (base_z + dz).clamp(2.2, 16.0);
        self.zoom_anim = Some(ZoomAnim {
            target_zoom,
            anchor_geo: (ala, alo),
            anchor_px: p,
            viewport: sz,
        });
    }

    pub fn trigger_inspect(&mut self, lat: f64, lon: f64, settings: &Settings) {
        let (cla, wlo) = (lat.clamp(-85.0, 85.0), wrap_lon(lon));
        let seq = self.inspect.seq.wrapping_add(1);
        self.inspect = InspectState {
            coord: Some((cla, wlo)),
            loading: true,
            bundle: self.inspect.bundle.take(),
            error: None,
            seq,
        };
        let (c, tx, wk, prov) = (
            self.client.clone(),
            self.pool.lock().expect("pool").tx.clone(),
            Arc::clone(&self.wake),
            settings.provider,
        );
        std::thread::spawn(move || {
            let stub = Location::new(format!("{cla:.2}°, {wlo:.2}°"), "", "", "", cla, wlo, "");
            let (loc, res) = std::thread::scope(|s| {
                let geo = s.spawn(|| c.reverse_geocode(cla, wlo));
                let wx = c.fetch_weather_quick(&stub, prov);
                (geo.join().ok(), wx)
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
    dist: f32,
    last_pos: Option<Point>,
    last_move: Option<Instant>,
    vel: (f32, f32),
    last_click: Option<(Instant, Point)>,
    hovered_badge: Option<usize>,
}

fn prune_geo_cache<V>(
    cache: &mut HashMap<(i32, i32), V>,
    (cla, clo): (f64, f64),
    coord: impl Fn(&V) -> (f64, f64),
) {
    if cache.len() <= 256 {
        return;
    }
    let mut by_d: Vec<_> = cache
        .iter()
        .map(|(&k, v)| {
            let (la, lo) = coord(v);
            (k, (la - cla).hypot(wrap_lon(lo - clo)))
        })
        .collect();
    by_d.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (k, _) in by_d.into_iter().take(cache.len() - 192) {
        cache.remove(&k);
    }
}

struct MapTilesCanvas<'a> {
    engine: &'a MapEngine,
    live_only: bool,
    cam: (f64, f64, f64),
    base_style: BaseMapStyle,
    overlay: WeatherOverlay,
    corner_r: f32,
    bundles: &'a HashMap<String, WeatherBundle>,
}

struct MapVectorsCanvas<'a, M> {
    engine: &'a MapEngine,
    interactive: bool,
    cam: (f64, f64, f64),
    base_style: BaseMapStyle,
    overlay: WeatherOverlay,
    badges: Vec<CityBadge>,
    wrap: Option<fn(MapMsg) -> M>,
}

impl MapTilesCanvas<'_> {
    fn draw_radar_timeline(
        &self,
        frame: &mut Frame,
        pool: &mut SharedTilePool,
        vp: MapViewport,
        allow_fetch: bool,
    ) {
        let Some(m) = self
            .engine
            .manifest
            .as_ref()
            .filter(|m| !m.frames.is_empty())
        else {
            return;
        };
        let len = m.frames.len();
        let (i0, i1_opt, raw_f, blend) = if self.live_only {
            (
                self.engine.live_frame_idx().min(len - 1),
                None,
                0.0_f32,
                0.0_f32,
            )
        } else if len > 1 {
            let pos = self.engine.radar_play_pos.clamp(0.0, (len - 1) as f32);
            let (a, rf) = (
                (pos.floor() as usize).min(len - 1),
                (pos - pos.floor()).clamp(0.0, 1.0),
            );
            if rf > 0.004 && a + 1 < len {
                (
                    a,
                    Some(a + 1),
                    rf,
                    rf * rf * rf * (rf * (rf * 6.0 - 15.0) + 10.0),
                )
            } else {
                (a, None, 0.0, 0.0)
            }
        } else {
            (self.engine.radar_frame_idx.min(len - 1), None, 0.0, 0.0)
        };
        let rf0 = &m.frames[i0];
        let rz = effective_tile_zoom(self.cam.2, rf0.max_native_zoom().min(7));
        let (ctx, cty) = tile_coord_at(self.cam.0, self.cam.1, rz);
        let ready = |f: &RainViewerFrame, p: &SharedTilePool| {
            f.tile_namespace()
                .is_some_and(|ns| p.has_tile(&ns, (rz, ctx, cty)))
        };
        let draw_rf = |fr: &mut Frame,
                       p: &mut SharedTilePool,
                       rf: &RainViewerFrame,
                       op: f32,
                       off: (f64, f64)| {
            if let Some(ns) = rf.tile_namespace() {
                let mut l = TileLayer::overlay(
                    &ns,
                    rf.max_native_zoom().min(7),
                    op,
                    Duration::from_secs(900),
                );
                l.offset = off;
                l.allow_fetch = allow_fetch;
                draw_tile_layer(fr, p, vp, l, |z, x, y| {
                    rf.tile_url(&m.host, z, x, y).unwrap_or_default()
                });
            }
        };
        if !self.live_only
            && !self.engine.radar_playing
            && !ready(rf0, pool)
            && let Some(fb) = (1..=5_usize).find_map(|d| {
                i0.checked_sub(d)
                    .and_then(|i| m.frames.get(i))
                    .filter(|f| ready(f, pool))
                    .or_else(|| m.frames.get(i0 + d).filter(|f| ready(f, pool)))
            })
        {
            draw_rf(frame, pool, fb, 0.48, (0.0, 0.0));
        }
        let (ax, ay) = if let Some(rf1) = i1_opt.and_then(|i| m.frames.get(i)) {
            let dt_s = (rf1.time_unix - rf0.time_unix).clamp(0, 3600) as f64;
            let mpp = 156_543.033_9 * self.cam.0.to_radians().cos().abs().max(0.25)
                / 2.0_f64.powf(self.cam.2);
            let (dx, dy) = (
                self.engine.steering_mps.0 * dt_s / mpp * 0.45,
                -self.engine.steering_mps.1 * dt_s / mpp * 0.45,
            );
            let mag = dx.hypot(dy);
            if mag > 16.0 {
                (dx * 16.0 / mag, dy * 16.0 / mag)
            } else {
                (dx, dy)
            }
        } else {
            (0.0, 0.0)
        };
        let next_ok = i1_opt
            .and_then(|i| m.frames.get(i))
            .is_some_and(|f| ready(f, pool));
        let w0 = if i1_opt.is_some() && next_ok {
            0.84 * (1.0 - blend)
        } else {
            0.84
        };
        if w0 > 0.015 {
            draw_rf(
                frame,
                pool,
                rf0,
                w0,
                if next_ok {
                    (ax * raw_f as f64, ay * raw_f as f64)
                } else {
                    (0.0, 0.0)
                },
            );
        }
        if let Some(rf1) = i1_opt.and_then(|i| m.frames.get(i))
            && blend > 0.015
        {
            draw_rf(
                frame,
                pool,
                rf1,
                0.84 * blend,
                (-ax * (1.0 - raw_f as f64), -ay * (1.0 - raw_f as f64)),
            );
        }
    }
}

impl<M> canvas::Program<M> for MapTilesCanvas<'_> {
    type State = ();

    fn draw(
        &self,
        _: &(),
        renderer: &Renderer,
        _: &Theme,
        bounds: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<Geometry> {
        let size = bounds.size();
        if size.width < 4.0 || size.height < 4.0 {
            return Vec::new();
        }
        let mut frame = Frame::new(renderer, size);
        let ocean = if self.overlay == WeatherOverlay::WindFlow {
            rgb8(18, 92, 152)
        } else if matches!(
            self.base_style,
            BaseMapStyle::AppleDark | BaseMapStyle::Satellite
        ) {
            rgb8(26, 32, 42)
        } else {
            rgb8(212, 224, 236)
        };
        frame.fill(
            &Path::rounded_rectangle(Point::ORIGIN, size, self.corner_r.into()),
            ocean,
        );
        let hrrr = self
            .engine
            .manifest
            .as_ref()
            .filter(|m| !m.frames.is_empty())
            .is_some_and(|m| {
                let last = m.frames.len() - 1;
                if self.live_only {
                    matches!(
                        m.frames[self.engine.live_frame_idx().min(last)].source,
                        RadarFrameSource::NoaaHrrr { .. }
                    )
                } else {
                    let pos = self.engine.radar_play_pos.clamp(0.0, last as f32);
                    let i0 = (pos.floor() as usize).min(last);
                    let i1 = (i0 + 1).min(last);
                    let blend = (pos - pos.floor()) > 0.004;
                    matches!(m.frames[i0].source, RadarFrameSource::NoaaHrrr { .. })
                        || (blend
                            && matches!(m.frames[i1].source, RadarFrameSource::NoaaHrrr { .. }))
                }
            });
        let fts = (!self.live_only)
            .then(|| self.engine.selected_forecast_timestamp())
            .flatten();
        let vp = MapViewport {
            bounds: size,
            lat: self.cam.0,
            lon: self.cam.1,
            zoom: self.cam.2,
        };
        let allow_fetch = self.live_only || !self.engine.is_fast_flying();
        let scalar =
            self.engine
                .build_scalar_overlay_handle(vp, self.overlay, self.bundles, fts, hrrr);
        if let Ok(mut pool) = self.engine.pool.lock() {
            let base = TileLayer {
                ns: self.base_style.namespace(),
                max_z: self.base_style.max_zoom(),
                opacity: 1.0,
                opaque_base: true,
                fallback: true,
                allow_fetch,
                ttl: Duration::from_secs(86_400 * 7),
                offset: (0.0, 0.0),
            };
            draw_tile_layer(&mut frame, &mut pool, vp, base, |z, x, y| {
                self.base_style.tile_url(z, x, y)
            });
            match self.overlay {
                WeatherOverlay::RadarTimeline => {
                    self.draw_radar_timeline(&mut frame, &mut pool, vp, allow_fetch)
                }
                WeatherOverlay::NexradUsRadar | WeatherOverlay::InfraredClouds => {
                    let (ns, mz, op, slug) = if self.overlay == WeatherOverlay::NexradUsRadar {
                        ("iem_nexrad", 10, 0.80, "nexrad-n0q-900913")
                    } else {
                        ("iem_goes_ir", 8, 0.72, "goes-ir-4km-900913")
                    };
                    let mut l = TileLayer::overlay(ns, mz, op, Duration::from_secs(300));
                    l.allow_fetch = allow_fetch;
                    draw_tile_layer(&mut frame, &mut pool, vp, l, |z, x, y| {
                        format!(
                            "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/{slug}/{z}/{x}/{y}.png"
                        )
                    });
                }
                _ => {}
            }
            if let Some(sh) = scalar {
                let mut img =
                    adv_image::Image::new(sh).filter_method(adv_image::FilterMethod::Linear);
                img.border_radius = self.corner_r.into();
                frame.draw_image(Rectangle::with_size(size), img);
            }
            if let Some((lns, lfn)) = self.base_style.label_overlay() {
                let mut l = TileLayer::overlay(lns, 16, 0.96, Duration::from_secs(86_400 * 7));
                l.fallback = false;
                l.allow_fetch = allow_fetch;
                draw_tile_layer(&mut frame, &mut pool, vp, l, lfn);
            }
        }
        vec![frame.into_geometry()]
    }
}

impl<M> MapVectorsCanvas<'_, M> {
    fn draw_vectors(&self, f: &mut Frame, rect: Rectangle, hovered_badge: Option<usize>) {
        if self.interactive && self.overlay == WeatherOverlay::WindFlow {
            let (rw, rh) = (rect.width, rect.height);
            for p in &self.engine.particles {
                let fade = ((p.age / p.max_age.max(0.1)).clamp(0.0, 1.0) * std::f32::consts::PI)
                    .sin()
                    .powf(0.85)
                    * 0.86;
                if fade < 0.02 {
                    continue;
                }
                let n = (p.tlen as usize).min(WIND_TRAIL_LEN);
                if n >= 2 {
                    let pt = |k: usize| Point::new(p.trail[k].0 * rw, p.trail[k].1 * rh);
                    let mut valid = 1_usize;
                    while valid < n {
                        let (a, b) = (pt(valid - 1), pt(valid));
                        if (a.x - b.x).powi(2) + (a.y - b.y).powi(2) > 2304.0 {
                            break;
                        }
                        valid += 1;
                    }
                    if valid >= 2 {
                        let mid = (valid / 2).max(1);
                        let head = Path::new(|b| {
                            b.move_to(pt(0));
                            for k in 1..=mid {
                                b.line_to(pt(k));
                            }
                        });
                        f.stroke(
                            &head,
                            Stroke::default()
                                .with_width(1.9)
                                .with_line_cap(canvas::LineCap::Round)
                                .with_color(wind_to_color(p.spd as f64, fade * 0.92)),
                        );
                        if valid > mid + 1 {
                            let tail = Path::new(|b| {
                                b.move_to(pt(mid));
                                for k in mid + 1..valid {
                                    b.line_to(pt(k));
                                }
                            });
                            f.stroke(
                                &tail,
                                Stroke::default()
                                    .with_width(1.15)
                                    .with_line_cap(canvas::LineCap::Round)
                                    .with_color(wind_to_color(p.spd as f64, fade * 0.36)),
                            );
                        }
                    }
                } else {
                    let (sx, sy, hl) = (
                        p.u * rw,
                        p.v * rh,
                        (8.0 + p.spd * 1.1).clamp(8.0, 20.0) * 0.5,
                    );
                    let st = Stroke::default()
                        .with_width(1.5)
                        .with_line_cap(canvas::LineCap::Round)
                        .with_color(wind_to_color(p.spd as f64, fade * 0.7));
                    f.stroke(
                        &Path::line(
                            Point::new(sx - p.dx * hl, sy - p.dy * hl),
                            Point::new(sx + p.dx * hl, sy + p.dy * hl),
                        ),
                        st,
                    );
                }
            }
        }
        // Apple Maps-style dropped pin for inspected coordinate.
        if self.interactive
            && let Some((ila, ilo)) = self.engine.inspect.coord
        {
            let ip = project_lat_lon_to_screen(ila, ilo, self.cam.0, self.cam.1, self.cam.2, rect);
            let head = Point::new(ip.x, ip.y - 16.0);
            f.fill(&Path::circle(ip, 10.5), rgba8(10, 132, 255, 0.22));
            f.fill(
                &Path::circle(Point::new(ip.x, ip.y + 0.5), 3.6),
                rgba8(0, 0, 0, 0.24),
            );
            let stem = Path::new(|b| {
                b.move_to(Point::new(head.x - 5.5, head.y + 6.0));
                b.line_to(ip);
                b.line_to(Point::new(head.x + 5.5, head.y + 6.0));
                b.close();
            });
            f.fill(&stem, Color::WHITE);
            f.fill(
                &Path::circle(Point::new(head.x, head.y + 1.5), 11.2),
                rgba8(0, 0, 0, 0.22),
            );
            f.fill(&Path::circle(head, 10.2), Color::WHITE);
            f.fill(&Path::circle(head, 8.2), rgb8(10, 132, 255));
            f.fill(&Path::circle(head, 3.0), Color::WHITE);
        }
        let dark = self.base_style.is_dark_hud(self.overlay);
        let mut vis = declutter_badges(&self.badges, self.cam.0, self.cam.1, self.cam.2, rect);
        vis.reverse();
        let label = |fr: &mut Frame,
                     content: String,
                     position: Point,
                     color: Color,
                     size: f32,
                     w: Weight| {
            fr.fill_text(CanvasText {
                content,
                position,
                color,
                size: size.into(),
                font: fw(w),
                align_x: Alignment::Center.into(),
                align_y: Alignment::Center.into(),
                ..CanvasText::default()
            });
        };
        for (idx, b, pos, dc_raw) in vis {
            let hovered = self.interactive && hovered_badge == Some(idx);
            let base_rad = if !self.interactive {
                15.0
            } else if b.active {
                19.5
            } else {
                14.0
            };
            let rad = if hovered { base_rad + 1.6 } else { base_rad };
            let dc = if !self.interactive && b.active {
                Point::new(pos.x, pos.y - 24.0)
            } else {
                dc_raw
            };
            // Apple Weather signature current-location blue dot + soft translucent halo.
            if b.active {
                if b.is_current_location {
                    let halo_r = if self.interactive { 11.5 } else { 9.5 };
                    f.fill(&Path::circle(pos, halo_r), rgba8(10, 132, 255, 0.22));
                    f.fill(
                        &Path::circle(Point::new(pos.x, pos.y + 0.8), 5.2),
                        rgba8(0, 0, 0, 0.22),
                    );
                    f.fill(&Path::circle(pos, 5.0), Color::WHITE);
                    f.fill(&Path::circle(pos, 3.3), rgb8(10, 132, 255));
                } else {
                    f.fill(
                        &Path::circle(Point::new(pos.x, pos.y + 0.8), 4.6),
                        rgba8(0, 0, 0, 0.24),
                    );
                    f.fill(&Path::circle(pos, 4.4), Color::WHITE);
                    f.fill(&Path::circle(pos, 2.7), b.col);
                }
            }
            // Soft ambient drop shadow beneath circular pin + teardrop pointer.
            f.fill(
                &Path::circle(Point::new(dc.x, dc.y + 2.2), rad + 3.2),
                rgba8(0, 0, 0, if dark { 0.34 } else { 0.18 }),
            );
            if hovered {
                f.fill(&Path::circle(dc, rad + 5.0), rgba8(10, 132, 255, 0.26));
            }
            // Teardrop pointer beak connecting active badge cleanly toward coordinate dot.
            if b.active {
                let tip_y = (pos.y - 6.0).max(dc.y + rad + 4.0);
                let beak_w = if self.interactive { 7.6 } else { 6.0 };
                let outer_beak = Path::new(|pb| {
                    pb.move_to(Point::new(dc.x - beak_w, dc.y + rad * 0.52));
                    pb.line_to(Point::new(dc.x, tip_y + 1.8));
                    pb.line_to(Point::new(dc.x + beak_w, dc.y + rad * 0.52));
                    pb.close();
                });
                f.fill(&outer_beak, Color::WHITE);
            }
            // Crisp white outer ring & rich color-coded Apple Weather fill.
            f.fill(&Path::circle(dc, rad + 2.3), Color::WHITE);
            f.fill(&Path::circle(dc, rad), b.col);
            if b.active {
                let tip_y = (pos.y - 6.0).max(dc.y + rad + 4.0);
                let inner_w = if self.interactive { 5.2 } else { 4.0 };
                let inner_beak = Path::new(|pb| {
                    pb.move_to(Point::new(dc.x - inner_w, dc.y + rad * 0.50));
                    pb.line_to(Point::new(dc.x, tip_y - 0.6));
                    pb.line_to(Point::new(dc.x + inner_w, dc.y + rad * 0.50));
                    pb.close();
                });
                f.fill(&inner_beak, b.col);
            }
            // Subtle glass-enamel top specular highlight on colored badges.
            if self.overlay != WeatherOverlay::WindFlow {
                f.fill(
                    &Path::circle(Point::new(dc.x, dc.y - rad * 0.28), rad * 0.68),
                    rgba8(255, 255, 255, 0.08),
                );
            } else if let Some(deg) = b.wind_deg {
                // Apple Weather Wind map pin: crisp directional pointer on the badge perimeter
                // showing where the wind is blowing toward (deg + 180°).
                let ang = ((deg + 180.0 - 90.0).to_radians()) as f32;
                let (ca, sa) = (ang.cos(), ang.sin());
                let (px, py) = (-sa, ca);
                let tip = Point::new(dc.x + ca * (rad + 7.2), dc.y + sa * (rad + 7.2));
                let base_r = rad + 1.2;
                let arrow = Path::new(|pb| {
                    pb.move_to(tip);
                    pb.line_to(Point::new(
                        dc.x + ca * base_r + px * 4.4,
                        dc.y + sa * base_r + py * 4.4,
                    ));
                    pb.line_to(Point::new(
                        dc.x + ca * base_r - px * 4.4,
                        dc.y + sa * base_r - py * 4.4,
                    ));
                    pb.close();
                });
                f.fill(&arrow, Color::WHITE);
            }
            let vc = if self.overlay == WeatherOverlay::WindFlow {
                rgb8(20, 32, 50)
            } else {
                Color::WHITE
            };
            label(
                f,
                b.val.clone(),
                Point::new(dc.x, if b.sub.is_empty() { dc.y } else { dc.y - 4.2 }),
                vc,
                if b.active {
                    if self.interactive { 13.5 } else { 11.5 }
                } else {
                    10.5
                },
                Weight::Bold,
            );
            if !b.sub.is_empty() {
                label(
                    f,
                    b.sub.clone(),
                    Point::new(dc.x, dc.y + 7.5),
                    rgb8(64, 78, 98),
                    7.5,
                    Weight::Bold,
                );
            }
            if !b.title.is_empty() {
                let ly = if b.active {
                    pos.y + 13.5
                } else {
                    dc.y + rad + 11.0
                };
                let lp = Point::new(pos.x, ly);
                let (sh, fg) = if dark {
                    (rgba8(8, 12, 20, 0.85), Color::WHITE)
                } else {
                    (rgba8(255, 255, 255, 0.92), rgb8(20, 28, 42))
                };
                // 4-way crisp Apple Maps label halo for legibility over dense cartography.
                for (ox, oy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
                    label(
                        f,
                        b.title.clone(),
                        Point::new(lp.x + ox, lp.y + oy),
                        sh,
                        10.5,
                        Weight::Semibold,
                    );
                }
                label(f, b.title.clone(), lp, fg, 10.5, Weight::Semibold);
            }
        }
    }
}

impl<M: Clone> canvas::Program<M> for MapVectorsCanvas<'_, M> {
    type State = DragState;
    fn update(
        &self,
        st: &mut DragState,
        event: &Event,
        b: Rectangle,
        cur: mouse::Cursor,
    ) -> Option<Action<M>> {
        if !self.interactive {
            return None;
        }
        let wrap = self.wrap?;
        let emit = |m: MapMsg| Some(Action::publish(wrap(m)).and_capture());
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cur.position_in(b)?;
                if let Some((t, prev)) = st.last_click
                    && t.elapsed() < Duration::from_millis(300)
                    && (p.x - prev.x).hypot(p.y - prev.y) <= 8.0
                {
                    (st.last_click, st.dragging, st.dist, st.vel) = (None, false, 0.0, (0.0, 0.0));
                    return emit(MapMsg::DoubleClickZoom(p, b.size()));
                }
                st.dragging = true;
                st.dist = 0.0;
                st.vel = (0.0, 0.0);
                st.last_pos = cur.position();
                st.last_move = Some(Instant::now());
                emit(MapMsg::PanStart)
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                emit(MapMsg::WheelZoom(cur.position_in(b)?, b.size(), -1.15))
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                let last = st.last_pos.unwrap_or(*position);
                let (dx, dy) = (position.x - last.x, position.y - last.y);
                let now = Instant::now();
                let dt = st
                    .last_move
                    .map_or(0.016_f32, |t| t.elapsed().as_secs_f32().clamp(0.004, 0.1));
                let (ivx, ivy) = (dx / dt, dy / dt);
                st.vel = (st.vel.0 * 0.45 + ivx * 0.55, st.vel.1 * 0.45 + ivy * 0.55);
                st.last_pos = Some(*position);
                st.last_move = Some(now);
                st.dist += dx.hypot(dy);
                if dx.abs() > 0.05 || dy.abs() > 0.05 {
                    emit(MapMsg::PanPixels(dx, dy))
                } else {
                    Some(Action::capture())
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let next_hover = cur.position_in(b).and_then(|pos| {
                    declutter_badges(
                        &self.badges,
                        self.cam.0,
                        self.cam.1,
                        self.cam.2,
                        Rectangle::with_size(b.size()),
                    )
                    .into_iter()
                    .find(|(_, badge, _, dc)| {
                        (pos.x - dc.x).hypot(pos.y - dc.y) <= if badge.active { 23.0 } else { 16.5 }
                    })
                    .map(|(idx, _, _, _)| idx)
                });
                if st.hovered_badge != next_hover {
                    st.hovered_badge = next_hover;
                    Some(Action::request_redraw())
                } else {
                    None
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if st.dragging => {
                let (d, vel, recent) = (
                    st.dist,
                    st.vel,
                    st.last_move
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(65)),
                );
                (st.dragging, st.dist, st.vel) = (false, 0.0, (0.0, 0.0));
                if d < 5.0
                    && let Some(p) = cur.position_in(b)
                {
                    st.last_click = Some((Instant::now(), p));
                    return emit(MapMsg::ClickMap(p, b.size()));
                }
                st.last_click = None;
                if recent && vel.0.hypot(vel.1) > 60.0 {
                    emit(MapMsg::PanFling(vel.0, vel.1))
                } else {
                    Some(Action::capture())
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let p = cur.position_in(b)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y as f64 * 0.42,
                    mouse::ScrollDelta::Pixels { y, .. } => *y as f64 * 0.0055,
                };
                (dy.abs() > 0.0001).then(|| {
                    Action::publish(wrap(MapMsg::WheelZoom(p, b.size(), dy))).and_capture()
                })
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if !modifiers.command() && !modifiers.alt() =>
            {
                match key {
                    Key::Named(Named::Space) => emit(MapMsg::ToggleRadarPlay),
                    Key::Named(Named::ArrowLeft) => emit(MapMsg::StepTimeline(-1)),
                    Key::Named(Named::ArrowRight) => emit(MapMsg::StepTimeline(1)),
                    Key::Character(c) if matches!(c.as_str(), "+" | "=") => {
                        emit(MapMsg::ZoomStep(0.85))
                    }
                    Key::Character(c) if matches!(c.as_str(), "-" | "_") => {
                        emit(MapMsg::ZoomStep(-0.85))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        st: &DragState,
        b: Rectangle,
        cur: mouse::Cursor,
    ) -> mouse::Interaction {
        if !self.interactive {
            return mouse::Interaction::Pointer;
        }
        if st.dragging {
            return mouse::Interaction::Grabbing;
        }
        if cur.position_in(b).is_none() {
            return mouse::Interaction::default();
        }
        if st.hovered_badge.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::Grab
        }
    }

    fn draw(
        &self,
        st: &DragState,
        renderer: &Renderer,
        _: &Theme,
        bounds: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<Geometry> {
        let size = bounds.size();
        if size.width < 4.0 || size.height < 4.0 {
            return Vec::new();
        }
        let mut frame = Frame::new(renderer, size);
        let rect = Rectangle::with_size(size);
        frame.with_clip(rect, |fr| self.draw_vectors(fr, rect, st.hovered_badge));
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiles::stylize_radar_tile;

    #[test]
    fn mercator_and_radar_stylizer() {
        for (la, lo, z) in [
            (47.6062, -122.3321, 8.5),
            (-33.8688, 151.2093, 6.0),
            (0.0, 0.0, 4.0),
        ] {
            let (wx, wy) = lat_lon_to_world_px(la, lo, z);
            let (rla, rlo) = world_px_to_lat_lon(wx, wy, z);
            assert!((rla - la).abs() < 1e-5 && (rlo - lo).abs() < 1e-5);
        }
        let mut refd = vec![0u8; 64];
        refd[0..4].copy_from_slice(&[135, 138, 134, 240]);
        stylize_radar_tile("hrrr_refd_202611200000_0060", &mut refd, 4, 4);
        assert!(refd[3] < 40, "got alpha {}", refd[3]);
        let mut refp = vec![0u8; 64];
        for i in 0..4 {
            refp[i * 4..i * 4 + 4].copy_from_slice(&[30, 210, 45, 240]);
        }
        for i in 8..12 {
            refp[i * 4..i * 4 + 4].copy_from_slice(&[80, 225, 255, 240]);
        }
        stylize_radar_tile("hrrr_refp_202611200000_0060", &mut refp, 4, 4);
        assert!(refp[6] > refp[5] && refp[7] > 60 && refp[38] >= 200 && refp[39] > 60);
    }

    #[test]
    fn smooth_zoom_and_fling_momentum() {
        let mut eng = MapEngine::new(
            WeatherClient::new(),
            37.8715,
            -122.273,
            false,
            Arc::new(Notify::new()),
        );
        let settings = Settings::default();
        let bundles = HashMap::new();
        // Smooth cursor-anchored zoom keeps the geo anchor stable under the cursor.
        let sz = Size::new(800.0, 600.0);
        let p = Point::new(560.0, 240.0);
        let before = project_screen_to_lat_lon(
            p,
            eng.center_lat,
            eng.center_lon,
            eng.zoom,
            Rectangle::with_size(sz),
        );
        let z0 = eng.zoom;
        let _ = eng.handle_msg(MapMsg::DoubleClickZoom(p, sz), &[], None, &settings);
        assert!(eng.needs_animation_tick(true));
        for _ in 0..40 {
            eng.last_tick = Instant::now() - Duration::from_millis(16);
            eng.tick(&bundles, true);
        }
        assert!(eng.zoom > z0 + 0.9, "zoom did not animate: {}", eng.zoom);
        let after = project_screen_to_lat_lon(
            p,
            eng.center_lat,
            eng.center_lon,
            eng.zoom,
            Rectangle::with_size(sz),
        );
        assert!(
            (before.0 - after.0).abs() < 0.05 && (before.1 - after.1).abs() < 0.05,
            "anchor drifted: before={before:?} after={after:?}"
        );
        // Inertial fling glides smoothly and cancels immediately on PanStart.
        let lon0 = eng.center_lon;
        let _ = eng.handle_msg(MapMsg::PanFling(600.0, 0.0), &[], None, &settings);
        assert!(eng.needs_animation_tick(true));
        eng.last_tick = Instant::now() - Duration::from_millis(16);
        eng.tick(&bundles, true);
        assert!(eng.center_lon < lon0, "expected westward glide");
        let _ = eng.handle_msg(MapMsg::PanStart, &[], None, &settings);
        assert_eq!(eng.pan_vel, (0.0, 0.0));
    }
}
