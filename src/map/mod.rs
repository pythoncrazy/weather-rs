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
    MapViewport, SharedTilePool, TileKey, TileLayer, draw_tile_layer, effective_tile_zoom,
    tile_coord_at,
};

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
    PanPixels(f32, f32),
    WheelZoom(Point, Size, f64),
    DoubleClickZoom(Point, Size),
    ZoomStep(f64),
    RecenterActive,
    ToggleRadarPlay,
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
    TileLoaded(TileKey, adv_image::Handle),
    TileFailed(TileKey),
    ManifestLoaded(RainViewerManifest),
    GridSamples(Vec<GridWeatherPoint>),
    AqiSamples(Vec<(f64, f64, f64)>),
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

pub struct MapEngine {
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
    target_cam: Option<(f64, f64, f64)>,
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
    pool: Arc<Mutex<SharedTilePool>>,
    rx: mpsc::Receiver<WorkerEvent>,
    client: WeatherClient,
    grid_cache: HashMap<(i32, i32), GridWeatherPoint>,
    in_flight_grid: HashSet<(i32, i32)>,
    aqi_cache: HashMap<(i32, i32), (f64, f64, f64)>,
    in_flight_aqi: HashSet<(i32, i32)>,
    grid_gen: u64,
    last_grid_poll: Option<Instant>,
    scalar_tex: Mutex<Option<(u64, adv_image::Handle)>>,
    particles: Vec<WindParticle>,
    pub(super) inspect: InspectState,
}

impl MapEngine {
    pub fn new(client: WeatherClient, lat: f64, lon: f64) -> Self {
        let (tx, rx) = mpsc::channel();
        let pool = Arc::new(Mutex::new(SharedTilePool {
            tiles: HashMap::new(),
            lru: VecDeque::new(),
            in_flight: HashSet::new(),
            failed_until: HashMap::new(),
            tx,
            client: client.clone(),
        }));
        let now = Instant::now();
        let mut engine = Self {
            center_lat: lat,
            center_lon: lon,
            zoom: 5.0,
            target_cam: None,
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
            pool,
            rx,
            client,
            grid_cache: HashMap::new(),
            in_flight_grid: HashSet::new(),
            aqi_cache: HashMap::new(),
            in_flight_aqi: HashSet::new(),
            grid_gen: 1,
            last_grid_poll: None,
            scalar_tex: Mutex::new(None),
            particles: init_wind_particles(),
            inspect: InspectState::default(),
        };
        engine.prewarm_region(lat, lon);
        engine
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
                p.ensure(ns, (z, tx, ty), || url(tx, ty), ttl, high);
            }
        }
    }

    pub fn prewarm_region(&mut self, lat: f64, lon: f64) {
        let Ok(mut p) = self.pool.lock() else { return };
        let ttl = Duration::from_secs(86_400 * 7);
        for (st, zooms) in [
            (BaseMapStyle::AppleDark, &[5_u8, 6][..]),
            (BaseMapStyle::Terrain, &[5, 6][..]),
            (BaseMapStyle::CleanLight, &[3, 4, 5, 6][..]),
        ] {
            for &z in zooms {
                let high = z == 5 && matches!(st, BaseMapStyle::AppleDark | BaseMapStyle::Terrain);
                Self::prewarm_3x3(&mut p, st.namespace(), z, (lat, lon), ttl, high, |x, y| {
                    st.tile_url(z, x, y)
                });
                if let Some((lns, lfn)) = st.label_overlay() {
                    Self::prewarm_3x3(&mut p, lns, z, (lat, lon), ttl, high, |x, y| lfn(z, x, y));
                }
            }
        }
        for wy in 0..4_u32 {
            for wx in 0..4_u32 {
                for st in [BaseMapStyle::CleanLight, BaseMapStyle::AppleDark] {
                    p.ensure(
                        st.namespace(),
                        (2, wx, wy),
                        || st.tile_url(2, wx, wy),
                        ttl,
                        false,
                    );
                }
            }
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
        let li = self.live_frame_idx().min(m.frames.len() - 1);
        for i in li..m.frames.len() {
            self.prewarm_radar_frame(&m.host, &m.frames[i], i <= li + 1);
        }
        for i in (0..li).rev() {
            self.prewarm_radar_frame(&m.host, &m.frames[i], false);
        }
    }

    pub fn fly_to(&mut self, lat: f64, lon: f64, zoom: Option<f64>) {
        self.last_cam_move = Instant::now();
        self.target_cam = Some((
            lat.clamp(-85.0, 85.0),
            wrap_lon(lon),
            zoom.unwrap_or(self.zoom),
        ));
    }

    pub fn needs_fast_tick(&self, expanded: bool) -> bool {
        let scrub = Self::overlay_supports_timeline(self.overlay)
            && (self.radar_play_pos - self.radar_target_pos).abs() > 0.006;
        self.pool.lock().is_ok_and(|p| !p.in_flight.is_empty())
            || self.inspect.loading
            || !self.in_flight_grid.is_empty()
            || !self.in_flight_aqi.is_empty()
            || (expanded
                && (self.target_cam.is_some()
                    || self.radar_playing
                    || scrub
                    || self.overlay == WeatherOverlay::WindFlow))
    }

    pub fn tick(&mut self, bundles: &HashMap<String, WeatherBundle>, expanded: bool) -> bool {
        let mut dirty = false;
        while let Ok(ev) = self.rx.try_recv() {
            dirty = true;
            match ev {
                WorkerEvent::TileLoaded(k, h) => {
                    if let Ok(mut p) = self.pool.lock() {
                        p.in_flight.remove(&k);
                        p.insert(k, h);
                    }
                }
                WorkerEvent::TileFailed(k) => {
                    if let Ok(mut p) = self.pool.lock() {
                        p.in_flight.remove(&k);
                        p.failed_until
                            .insert(k, Instant::now() + Duration::from_secs(18));
                    }
                }
                WorkerEvent::ManifestLoaded(m) => {
                    let li = m
                        .frames
                        .iter()
                        .rposition(|f| !f.is_nowcast)
                        .unwrap_or(m.frames.len().saturating_sub(1));
                    if self.manifest.is_none() || self.radar_frame_idx >= m.frames.len() {
                        (
                            self.radar_frame_idx,
                            self.radar_play_pos,
                            self.radar_target_pos,
                        ) = (li, li as f32, li as f32);
                    }
                    self.manifest = Some(m);
                    self.prewarm_timeline_strip();
                }
                WorkerEvent::GridSamples(pts) => {
                    for pt in pts {
                        let q = quantize_coord(pt.lat, pt.lon);
                        self.in_flight_grid.remove(&q);
                        self.grid_cache.insert(q, pt);
                    }
                    self.grid_gen = self.grid_gen.wrapping_add(1);
                }
                WorkerEvent::AqiSamples(pts) => {
                    for (la, lo, a) in pts {
                        let q = quantize_coord(la, lo);
                        self.in_flight_aqi.remove(&q);
                        self.aqi_cache.insert(q, (la, lo, a));
                    }
                    self.grid_gen = self.grid_gen.wrapping_add(1);
                }
                WorkerEvent::PointInspected(seq, res) if seq == self.inspect.seq => {
                    self.inspect.loading = false;
                    match res {
                        Ok(b) => {
                            self.inspect.bundle = Some(*b);
                            self.inspect.error = None;
                        }
                        Err(e) => {
                            self.inspect.bundle = None;
                            self.inspect.error = Some(e);
                        }
                    }
                }
                WorkerEvent::PointInspected(..) => {}
            }
        }
        if self
            .last_manifest
            .is_none_or(|t| t.elapsed() > Duration::from_secs(300))
        {
            self.last_manifest = Some(Instant::now());
            let (c, tx) = (
                self.client.clone(),
                self.pool.lock().expect("pool").tx.clone(),
            );
            rayon::spawn(move || {
                if !is_shutting_down()
                    && let Some(m) = c.fetch_rainviewer_manifest()
                {
                    let _ = tx.send(WorkerEvent::ManifestLoaded(m));
                }
            });
        }
        let dt = self.last_tick.elapsed().as_secs_f32().clamp(0.0, 0.08);
        self.last_tick = Instant::now();
        if let Some((tla, tlo, tz)) = self.target_cam {
            let k = (1.0 - (-12.5 * dt.max(0.008)).exp()) as f64;
            let mut dl = tlo - self.center_lon;
            if dl > 180.0 {
                dl -= 360.0;
            } else if dl < -180.0 {
                dl += 360.0;
            }
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
        if expanded {
            dirty |= self.tick_expanded(bundles, dt);
        }
        dirty
    }

    fn tick_expanded(&mut self, bundles: &HashMap<String, WeatherBundle>, dt: f32) -> bool {
        let mut dirty = false;
        if self.overlay == WeatherOverlay::RadarTimeline
            && self.target_cam.is_none()
            && self.last_cam_move.elapsed() >= Duration::from_millis(260)
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
            let (c, tx) = (
                self.client.clone(),
                self.pool.lock().expect("pool").tx.clone(),
            );
            let keys: Vec<_> = nw.iter().map(|&(la, lo)| quantize_coord(la, lo)).collect();
            rayon::spawn(move || {
                let mut pts = c.fetch_grid_batch(&nw);
                if pts.is_empty() {
                    pts = keys
                        .into_iter()
                        .map(|(a, b)| {
                            GridWeatherPoint::stationary(
                                a as f64 * 0.5,
                                b as f64 * 0.5,
                                [15.0, 0.0, 2.0, 0.5, 2.0, 1013.25],
                            )
                        })
                        .collect();
                }
                let _ = tx.send(WorkerEvent::GridSamples(pts));
            });
        }
        if !na.is_empty() {
            let (c, tx) = (
                self.client.clone(),
                self.pool.lock().expect("pool").tx.clone(),
            );
            rayon::spawn(move || {
                let pts = c.fetch_aqi_grid_batch(&na);
                if !pts.is_empty() {
                    let _ = tx.send(WorkerEvent::AqiSamples(pts));
                }
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
                if let Some(l) = active_id.and_then(|id| saved.iter().find(|l| l.id == id)) {
                    (self.center_lat, self.center_lon) = (l.lat, l.lon);
                }
                self.prewarm_timeline_strip();
                return Some(MapExternalAction::SetExpanded(true));
            }
            MapMsg::CloseMap => {
                self.show_layer_menu = false;
                return Some(MapExternalAction::SetExpanded(false));
            }
            MapMsg::ToggleLayerMenu => self.show_layer_menu = !self.show_layer_menu,
            MapMsg::SelectOverlay(ov) => {
                (self.overlay, self.show_layer_menu) = (ov, false);
                if ov == WeatherOverlay::RadarTimeline {
                    self.prewarm_timeline_strip();
                } else {
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
            MapMsg::SelectBaseStyle(st) => (self.base_style, self.show_layer_menu) = (st, false),
            MapMsg::PanPixels(dx, dy) => {
                (self.target_cam, self.last_cam_move) = (None, Instant::now());
                let (ws, (wx, wy)) = (
                    256.0 * 2.0_f64.powf(self.zoom),
                    lat_lon_to_world_px(self.center_lat, self.center_lon, self.zoom),
                );
                let (la, lo) =
                    world_px_to_lat_lon(wx - dx as f64, (wy - dy as f64).clamp(0.0, ws), self.zoom);
                (self.center_lat, self.center_lon) = (la.clamp(-85.0, 85.0), wrap_lon(lo));
            }
            MapMsg::WheelZoom(p, sz, dz) => self.cursor_zoom(p, sz, dz),
            MapMsg::DoubleClickZoom(p, sz) => {
                self.clear_inspector();
                self.cursor_zoom(p, sz, 1.0);
            }
            MapMsg::ZoomStep(dz) => self.fly_to(
                self.center_lat,
                self.center_lon,
                Some((self.zoom + dz).clamp(2.2, 16.0)),
            ),
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
            MapMsg::ScrubRadar(idx) => {
                (
                    self.radar_playing,
                    self.radar_loop_hold,
                    self.radar_frame_idx,
                    self.radar_target_pos,
                ) = (false, 0.0, idx, idx as f32);
                if let Some(m) = &self.manifest {
                    for i in [idx, idx + 1] {
                        if let Some(f) = m.frames.get(i) {
                            self.prewarm_radar_frame(&m.host, f, true);
                        }
                    }
                }
            }
            MapMsg::ScrubRadarPos(pos) => {
                (self.radar_playing, self.radar_loop_hold) = (false, 0.0);
                if let Some(m) = &self.manifest
                    && !m.frames.is_empty()
                {
                    let max_i = m.frames.len() - 1;
                    let c = pos.clamp(0.0, max_i as f32);
                    let (i0, i1) = (
                        (c.floor() as usize).min(max_i),
                        ((c.floor() as usize) + 1).min(max_i),
                    );
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
            MapMsg::JumpToLive => {
                let li = self.live_frame_idx();
                (
                    self.radar_playing,
                    self.radar_loop_hold,
                    self.radar_frame_idx,
                    self.radar_target_pos,
                ) = (false, 0.0, li, li as f32);
                if let Some(m) = &self.manifest
                    && let Some(f) = m.frames.get(li)
                {
                    self.prewarm_radar_frame(&m.host, f, true);
                }
            }
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

    fn cursor_zoom(&mut self, p: Point, sz: Size, dz: f64) {
        (self.target_cam, self.last_cam_move) = (None, Instant::now());
        let (ala, alo) = project_screen_to_lat_lon(
            p,
            self.center_lat,
            self.center_lon,
            self.zoom,
            Rectangle::with_size(sz),
        );
        self.zoom = (self.zoom + dz).clamp(2.2, 16.0);
        let (awx, awy) = lat_lon_to_world_px(ala, alo, self.zoom);
        let (cla, clo) = world_px_to_lat_lon(
            awx - (p.x - sz.width * 0.5) as f64,
            awy - (p.y - sz.height * 0.5) as f64,
            self.zoom,
        );
        (self.center_lat, self.center_lon) = (cla.clamp(-85.0, 85.0), wrap_lon(clo));
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
        let (c, tx, prov) = (
            self.client.clone(),
            self.pool.lock().expect("pool").tx.clone(),
            settings.provider,
        );
        rayon::spawn(move || {
            let loc = c.reverse_geocode(cla, wlo);
            let _ = tx.send(WorkerEvent::PointInspected(
                seq,
                c.fetch_weather(&loc, prov).map(Box::new),
            ));
        });
    }
}

#[derive(Default)]
struct DragState {
    dragging: bool,
    dist: f32,
    last_pos: Option<Point>,
    last_click: Option<(Instant, Point)>,
}

struct MapCanvas<'a, M> {
    engine: &'a MapEngine,
    interactive: bool,
    live_only: bool,
    cam: (f64, f64, f64),
    base_style: BaseMapStyle,
    overlay: WeatherOverlay,
    corner_r: f32,
    bundles: &'a HashMap<String, WeatherBundle>,
    badges: Vec<CityBadge>,
    wrap: Option<fn(MapMsg) -> M>,
}

impl<M> MapCanvas<'_, M> {
    fn draw_radar_timeline(&self, frame: &mut Frame, pool: &mut SharedTilePool, vp: MapViewport) {
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

    fn draw_vectors(&self, f: &mut Frame, rect: Rectangle) {
        if self.interactive && self.overlay == WeatherOverlay::WindFlow {
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
                    for s in 0..n - 1 {
                        let (p0, p1) = (
                            Point::new(p.trail[s].0 * rect.width, p.trail[s].1 * rect.height),
                            Point::new(
                                p.trail[s + 1].0 * rect.width,
                                p.trail[s + 1].1 * rect.height,
                            ),
                        );
                        if (p0.x - p1.x).hypot(p0.y - p1.y) > 48.0 {
                            break;
                        }
                        let hw = 1.0 - s as f32 / (n - 1) as f32;
                        let st = Stroke::default()
                            .with_width(0.95 + 1.05 * hw)
                            .with_line_cap(canvas::LineCap::Round)
                            .with_color(wind_to_color(p.spd as f64, fade * (0.20 + 0.80 * hw)));
                        f.stroke(&Path::line(p0, p1), st);
                    }
                } else {
                    let (sx, sy, hl) = (
                        p.u * rect.width,
                        p.v * rect.height,
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
        if self.interactive
            && let Some((ila, ilo)) = self.engine.inspect.coord
        {
            let ip = project_lat_lon_to_screen(ila, ilo, self.cam.0, self.cam.1, self.cam.2, rect);
            f.fill(&Path::circle(ip, 12.0), rgba8(10, 132, 255, 0.28));
            f.stroke(
                &Path::circle(ip, 7.5),
                Stroke::default()
                    .with_width(2.0)
                    .with_color(rgb8(10, 132, 255)),
            );
            f.fill(&Path::circle(ip, 3.2), Color::WHITE);
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
        for (_, b, pos, dc) in vis {
            let rad = if !self.interactive {
                15.5
            } else if b.active {
                20.0
            } else {
                13.5
            };
            if b.active {
                f.fill(&Path::circle(pos, 3.5), Color::WHITE);
                f.stroke(
                    &Path::circle(pos, 3.5),
                    Stroke::default()
                        .with_width(1.2)
                        .with_color(rgba8(0, 0, 0, 0.45)),
                );
            }
            f.fill(
                &Path::circle(Point::new(dc.x, dc.y + 1.5), rad + 2.4),
                rgba8(0, 0, 0, 0.24),
            );
            f.fill(&Path::circle(dc, rad + 2.2), Color::WHITE);
            f.fill(&Path::circle(dc, rad), b.col);
            let vc = if self.overlay == WeatherOverlay::WindFlow {
                rgb8(24, 36, 54)
            } else {
                Color::WHITE
            };
            label(
                f,
                b.val.clone(),
                Point::new(dc.x, if b.sub.is_empty() { dc.y } else { dc.y - 4.0 }),
                vc,
                if b.active { 13.5 } else { 10.5 },
                Weight::Bold,
            );
            if !b.sub.is_empty() {
                label(
                    f,
                    b.sub.clone(),
                    Point::new(dc.x, dc.y + 7.5),
                    rgb8(70, 84, 104),
                    7.5,
                    Weight::Bold,
                );
            }
            if !b.title.is_empty() {
                let lp = Point::new(pos.x, dc.y + rad + 10.5);
                let (sh, fg) = if dark {
                    (rgba8(0, 0, 0, 0.65), Color::WHITE)
                } else {
                    (rgba8(255, 255, 255, 0.80), rgb8(22, 30, 44))
                };
                label(
                    f,
                    b.title.clone(),
                    Point::new(lp.x, lp.y + 0.8),
                    sh,
                    10.0,
                    Weight::Semibold,
                );
                label(f, b.title.clone(), lp, fg, 10.0, Weight::Semibold);
            }
        }
    }
}

impl<M: Clone> canvas::Program<M> for MapCanvas<'_, M> {
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
                    && (p.x - prev.x).hypot(p.y - prev.y) <= 7.0
                {
                    (st.last_click, st.dragging, st.dist) = (None, false, 0.0);
                    return emit(MapMsg::DoubleClickZoom(p, b.size()));
                }
                (st.dragging, st.dist, st.last_pos) = (true, 0.0, cur.position());
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                emit(MapMsg::WheelZoom(cur.position_in(b)?, b.size(), -1.0))
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                let last = st.last_pos.unwrap_or(*position);
                let (dx, dy) = (position.x - last.x, position.y - last.y);
                st.last_pos = Some(*position);
                st.dist += dx.hypot(dy);
                if dx.abs() > 0.05 || dy.abs() > 0.05 {
                    emit(MapMsg::PanPixels(dx, dy))
                } else {
                    Some(Action::capture())
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if st.dragging => {
                let d = st.dist;
                (st.dragging, st.dist) = (false, 0.0);
                if d < 5.0
                    && let Some(p) = cur.position_in(b)
                {
                    st.last_click = Some((Instant::now(), p));
                    return emit(MapMsg::ClickMap(p, b.size()));
                }
                st.last_click = None;
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let p = cur.position_in(b)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y as f64 * 0.35,
                    mouse::ScrollDelta::Pixels { y, .. } => *y as f64 * 0.0045,
                };
                (dy.abs() > 0.0001).then(|| {
                    Action::publish(wrap(MapMsg::WheelZoom(p, b.size(), dy))).and_capture()
                })
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
        let Some(pos) = cur.position_in(b) else {
            return mouse::Interaction::default();
        };
        let hit = declutter_badges(
            &self.badges,
            self.cam.0,
            self.cam.1,
            self.cam.2,
            Rectangle::with_size(b.size()),
        )
        .into_iter()
        .any(|(_, badge, _, dc)| {
            (pos.x - dc.x).hypot(pos.y - dc.y) <= if badge.active { 22.0 } else { 15.5 }
        });
        if hit {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::Grab
        }
    }

    fn draw(
        &self,
        _: &DragState,
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
            .and_then(|m| {
                let idx = if self.live_only {
                    self.engine.live_frame_idx()
                } else {
                    self.engine
                        .radar_frame_idx
                        .min(m.frames.len().saturating_sub(1))
                };
                m.frames.get(idx)
            })
            .is_some_and(|f| matches!(f.source, RadarFrameSource::NoaaHrrr { .. }));
        let fts = (!self.live_only)
            .then(|| self.engine.selected_forecast_timestamp())
            .flatten();
        let vp = MapViewport {
            bounds: size,
            lat: self.cam.0,
            lon: self.cam.1,
            zoom: self.cam.2,
            corner_r: self.corner_r,
        };
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
                ttl: Duration::from_secs(86_400 * 7),
                offset: (0.0, 0.0),
            };
            draw_tile_layer(&mut frame, &mut pool, vp, base, |z, x, y| {
                self.base_style.tile_url(z, x, y)
            });
            match self.overlay {
                WeatherOverlay::RadarTimeline => {
                    self.draw_radar_timeline(&mut frame, &mut pool, vp)
                }
                WeatherOverlay::NexradUsRadar => draw_tile_layer(
                    &mut frame,
                    &mut pool,
                    vp,
                    TileLayer::overlay("iem_nexrad", 10, 0.80, Duration::from_secs(300)),
                    |z, x, y| {
                        format!(
                            "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/nexrad-n0q-900913/{z}/{x}/{y}.png"
                        )
                    },
                ),
                WeatherOverlay::InfraredClouds => draw_tile_layer(
                    &mut frame,
                    &mut pool,
                    vp,
                    TileLayer::overlay("iem_goes_ir", 8, 0.72, Duration::from_secs(300)),
                    |z, x, y| {
                        format!(
                            "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/goes-ir-4km-900913/{z}/{x}/{y}.png"
                        )
                    },
                ),
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
                draw_tile_layer(&mut frame, &mut pool, vp, l, lfn);
            }
        }
        let rect = Rectangle::with_size(size);
        frame.with_clip(rect, |fr| self.draw_vectors(fr, rect));
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
}
