use crate::cards::DetailMetric;
use crate::icons::{UiIcon, WidgetCanvas, cached_widget_canvas, icon_btn, icon_svg, sky_backdrop_canvas};
use crate::map::{MapEngine, MapExternalAction, MapMsg, TileKey};
use crate::model::{
    Location, PrecipUnit, PressureUnit, ProviderChoice, REFRESH_INTERVAL_SECS, Settings, TempUnit, ThemeMode, WeatherBundle,
    WeatherCondition, WindUnit,
};
use crate::providers::WeatherClient;
use crate::storage::{
    flush_pending_settings, is_shutting_down, load_cached_bundle, load_settings, request_shutdown, save_cached_bundle, save_settings,
    spawn_background_cache_prune,
};
use crate::style::{
    SkyPalette, ensure_fonts_loaded, flat_box, fw, hairline, hud_pill, palette_for, pbtn, rgb8, rgba8, sky_mini_card, sleek_scrollable,
    sp_h, sp_w, txt,
};
use chrono::Utc;
use iced::font::Weight;
use iced::keyboard::{self, Key};
use iced::widget::{canvas, column, container, mouse_area, operation, row, scrollable, stack, text, text_input};
use iced::{Alignment, Color, Element, Fill, Size, Subscription, Task, window};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

fn local_route_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("1.1.1.1:80").ok()?;
    Some(s.local_addr().ok()?.ip())
}

#[derive(Clone)]
struct WakeHandle(Arc<Notify>);
impl Hash for WakeHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

fn wake_stream(handle: &WakeHandle) -> impl iced::futures::Stream<Item = Msg> + use<> {
    let notify = Arc::clone(&handle.0);
    iced::stream::channel(4, async move |mut out| {
        use iced::futures::SinkExt;
        loop {
            notify.notified().await;
            // Wake the UI IMMEDIATELY on the first worker event (0 ms latency!), then coalesce burst follow-ups to 60Hz (16 ms)
            // so 40 parallel tile arrivals batch into a single UI frame instead of 250 view()/layout rebuilds per second!
            if out.send(Msg::Tick).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    })
}

#[derive(Debug, Clone)]
pub enum Pref {
    Theme(ThemeMode),
    Wind(WindUnit),
    Pressure(PressureUnit),
    Precip(PrecipUnit),
    Use24h(bool),
    Animate(bool),
    InterpolateForecast(bool),
    Provider(ProviderChoice),
}

#[derive(Debug, Clone)]
pub enum Msg {
    Tick,
    ToggleSidebar,
    ToggleUnits,
    TogglePreferences,
    OpenDetail(DetailMetric),
    CloseDetail,
    RefreshAll,
    SelectLocation(String),
    SelectIndex(usize),
    RemoveLocation(String),
    DetectLocation,
    SearchChanged(String),
    SubmitSearch,
    FocusSearch,
    ClearSearch,
    PickSearchResult(Location),
    ToggleAlertDetails(usize),
    ToggleDayDetails(usize),
    BentoScrolled(f32),
    ToggleMap,
    Pref(Pref),
    Map(MapMsg),
    TilesGpuPinned(Vec<(TileKey, Option<iced_runtime::image::Allocation>)>),
    Escape,
    WindowResized(Size),
    WindowCloseRequested,
    Noop,
}

enum WorkerOut {
    WeatherLoaded(String, Box<WeatherBundle>),
    WeatherFailed(String, String),
    SearchDone(String, Vec<Location>),
    CurrentLocation(Location, bool),
    CurrentLocationFailed,
}

#[derive(Default)]
struct UiRt {
    map_expanded: bool,
    show_preferences: bool,
    detail_metric: Option<DetailMetric>,
    loading_ids: HashSet<String>,
    last_error: Option<String>,
    search_query: String,
    last_dispatched_query: String,
    last_search_edit: Option<Instant>,
    search_in_flight: bool,
    search_results: Vec<Location>,
    detect_focus_pending: bool,
    detect_in_flight: bool,
    current_location_precise: bool,
}

pub struct WeatherApp {
    pub settings: Settings,
    pub window_size: Size,
    pub show_sidebar: bool,
    pub expanded_alert_idx: Option<usize>,
    pub selected_day_idx: Option<usize>,
    pub bundles: HashMap<String, WeatherBundle>,
    pub map: MapEngine,
    pub(crate) bento_sig: std::cell::Cell<u64>,
    bento_scroll_y: f32,
    sky_cache: canvas::Cache,
    sky_key: std::cell::Cell<(WeatherCondition, bool, ThemeMode)>,
    sky_particles_cache: canvas::Cache,
    sky_particles_key: std::cell::Cell<(WeatherCondition, bool, u32)>,
    ui: UiRt,
    last_location_check: Instant,
    last_location_wall: i64,
    last_route_check: Instant,
    last_route_ip: Option<IpAddr>,
    client: WeatherClient,
    tx: mpsc::Sender<WorkerOut>,
    rx: mpsc::Receiver<WorkerOut>,
    wake: Arc<Notify>,
    boot_instant: Instant,
    last_auto_refresh: Instant,
    /// Wall-clock twin of `last_auto_refresh`: `Instant` does not advance while the machine is suspended.
    last_auto_refresh_wall: i64,
    /// Wall-clock time of the last out-of-cycle full refresh attempt for the selected location.
    last_active_retry_wall: i64,
}

impl WeatherApp {
    pub fn boot() -> (Self, Task<Msg>) {
        let (mut settings, first_run) = load_settings();
        settings.deduplicate_locations();
        crate::providers::set_forecast_interpolation(settings.interpolate_forecast);
        let bundles = settings
            .saved_locations
            .iter()
            .filter_map(|loc| {
                let mut c = load_cached_bundle(&loc.id)?;
                if !loc.timezone.is_empty() {
                    c.location.timezone = loc.timezone.clone();
                }
                Some((loc.id.clone(), c))
            })
            .collect();
        spawn_background_cache_prune();
        let sel = settings.selected_location_id.as_ref().and_then(|id| settings.saved_locations.iter().find(|l| &l.id == id));
        let (lat, lon) = sel.map_or((47.6062, -122.3321), |l| (l.lat, l.lon));
        let (client, (tx, rx), wake, now) = (WeatherClient::new(), mpsc::channel(), Arc::new(Notify::new()), Instant::now());
        let mut app = Self {
            map: MapEngine::new(client.clone(), lat, lon, settings.theme_mode == ThemeMode::Light, Arc::clone(&wake)),
            bento_sig: std::cell::Cell::new(1),
            bento_scroll_y: 0.0,
            sky_cache: canvas::Cache::new(),
            sky_key: std::cell::Cell::new((WeatherCondition::Overcast, true, settings.theme_mode)),
            sky_particles_cache: canvas::Cache::new(),
            sky_particles_key: std::cell::Cell::new((WeatherCondition::Overcast, true, u32::MAX)),
            settings,
            window_size: Size::new(1180.0, 760.0),
            show_sidebar: true,
            expanded_alert_idx: None,
            selected_day_idx: None,
            bundles,
            ui: UiRt { detect_focus_pending: first_run, ..Default::default() },
            last_location_check: now,
            last_location_wall: Utc::now().timestamp(),
            last_route_check: now,
            last_route_ip: local_route_ip(),
            client,
            tx,
            rx,
            wake,
            boot_instant: now,
            last_auto_refresh: now,
            last_auto_refresh_wall: Utc::now().timestamp(),
            last_active_retry_wall: Utc::now().timestamp(),
        };
        #[cfg(target_os = "linux")]
        {
            let wk = Arc::clone(&app.wake);
            sctk_adwaita::on_ready(move || wk.notify_one());
        }
        app.map.prewarm_saved_locations(&app.settings.saved_locations, app.active_palette().is_light);
        // Skip redundant network fetches on warm restarts if cached weather is < 3 minutes old (Cmd+R still forces fresh).
        app.spawn_refresh_stale(180);
        // Reuse cached geo-IP location if we're still on the exact same local network route (< 6h old); saves 1-2 HTTP calls every boot!
        if let Some(ip) = app.last_route_ip.map(|ip| ip.to_string())
            && let Some((loc, precise)) = crate::storage::load_cached_geo_ip(&ip, Duration::from_secs(6 * 3600))
        {
            app.apply_detected_location(loc, precise);
        } else {
            app.spawn_detect_location();
        }
        (app, Task::none())
    }

    pub fn title(&self) -> String {
        self.active_location().map_or_else(|| "Weather".into(), |l| format!("Weather — {}", l.name))
    }
    pub fn theme(&self) -> iced::Theme {
        if self.settings.theme_mode == ThemeMode::Light { iced::Theme::Light } else { iced::Theme::Dark }
    }

    pub fn subscription(&self) -> Subscription<Msg> {
        use WeatherCondition as W;
        let q = self.ui.search_query.trim();
        let search_deb = q.len() >= 2 && q != self.ui.last_dispatched_query;
        // Don't burn 60fps sky particle redraws behind an expanded map!
        let sky_visible = !self.ui.map_expanded;
        let sky_anim = sky_visible
            && self.settings.animate_weather
            && !self.active_palette().is_light
            && self.active_bundle().is_some_and(|b| {
                !b.current.is_day
                    || matches!(
                        b.current.condition,
                        W::Drizzle | W::Rain | W::HeavyRain | W::FreezingRain | W::Thunderstorm | W::Snow | W::HeavySnow | W::Sleet
                    )
            });
        let tick = if self.map.needs_animation_tick(self.ui.map_expanded) || sky_anim {
            window::frames().map(|_| Msg::Tick)
        } else {
            iced::time::every(Duration::from_millis(if search_deb { 80 } else { 30_000 })).map(|_| Msg::Tick)
        };
        let keys = keyboard::listen().map(|ev| match ev {
            keyboard::Event::KeyPressed { key: Key::Named(keyboard::key::Named::Escape), .. } => Msg::Escape,
            keyboard::Event::KeyPressed { key: Key::Character(c), modifiers, .. } if modifiers.command() => match c.as_str() {
                "r" | "R" => Msg::RefreshAll,
                "k" | "K" | "f" | "F" => Msg::FocusSearch,
                "m" | "M" => Msg::ToggleMap,
                "," => Msg::TogglePreferences,
                "q" | "Q" | "w" | "W" => Msg::WindowCloseRequested,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => Msg::SelectIndex((c.as_bytes()[0] - b'1') as usize),
                _ => Msg::Noop,
            },
            _ => Msg::Noop,
        });
        Subscription::batch([
            tick,
            Subscription::run_with(WakeHandle(Arc::clone(&self.wake)), wake_stream),
            window::resize_events().map(|(_, s)| Msg::WindowResized(s)),
            window::close_requests().map(|_| Msg::WindowCloseRequested),
            keys,
        ])
    }

    fn clear_search(&mut self) {
        self.ui.search_query.clear();
        self.ui.search_results.clear();
        self.ui.last_dispatched_query.clear();
    }

    fn dispatch_search(&mut self, q: String) {
        (self.ui.last_dispatched_query, self.ui.search_in_flight) = (q.clone(), true);
        let (c, tx, wk) = (self.client.clone(), self.tx.clone(), Arc::clone(&self.wake));
        std::thread::spawn(move || {
            if !is_shutting_down() {
                let _ = tx.send(WorkerOut::SearchDone(q.clone(), c.search_locations(&q)));
                wk.notify_one();
            }
        });
    }

    fn spawn_detect_location(&mut self) {
        if self.ui.detect_in_flight {
            return;
        }
        (self.ui.detect_in_flight, self.last_location_check, self.last_location_wall) = (true, Instant::now(), Utc::now().timestamp());
        let (c, tx, wk) = (self.client.clone(), self.tx.clone(), Arc::clone(&self.wake));
        let _ = std::thread::Builder::new().name("loc-detect".into()).spawn(move || {
            if is_shutting_down() {
                return;
            }
            let out = c.detect_current_location().map_or(WorkerOut::CurrentLocationFailed, |(l, p)| WorkerOut::CurrentLocation(l, p));
            let _ = tx.send(out);
            wk.notify_one();
        });
    }

    fn spawn_refresh_batch(&mut self, locs: Vec<Location>) {
        let jobs: Vec<_> = locs
            .into_iter()
            .filter(|l| self.ui.loading_ids.insert(l.id.clone()))
            .map(|l| {
                let cached = self.bundles.contains_key(&l.id);
                (l, cached)
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let (c, tx, wk, prov) = (self.client.clone(), self.tx.clone(), Arc::clone(&self.wake), self.settings.provider);
        std::thread::spawn(move || {
            for (loc, cached) in jobs {
                if is_shutting_down() {
                    return;
                }
                let res = if cached {
                    c.fetch_weather(&loc, prov)
                } else {
                    c.fetch_weather_streaming(&loc, prov, |early| {
                        if !is_shutting_down() {
                            let _ = tx.send(WorkerOut::WeatherLoaded(loc.id.clone(), Box::new(early)));
                            wk.notify_one();
                        }
                    })
                };
                match res {
                    Ok(b) if !is_shutting_down() => {
                        let disk = b.clone();
                        let _ = tx.send(WorkerOut::WeatherLoaded(loc.id, Box::new(b)));
                        wk.notify_one();
                        if !is_shutting_down() {
                            save_cached_bundle(&disk);
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let _ = tx.send(WorkerOut::WeatherFailed(loc.id, err));
                        wk.notify_one();
                    }
                }
            }
        });
    }

    fn spawn_refresh_all(&mut self) {
        self.spawn_refresh_stale(0);
    }

    fn spawn_refresh_stale(&mut self, max_age_secs: i64) {
        let now = Utc::now().timestamp();
        let mut locs: Vec<Location> = self
            .settings
            .saved_locations
            .iter()
            .filter(|l| max_age_secs <= 0 || self.bundles.get(&l.id).is_none_or(|b| (now - b.fetched_unix).abs() >= max_age_secs))
            .cloned()
            .collect();
        // The selected location is always fetched in full; if it only holds a lean / outdated bundle it is refreshed even
        // when its current conditions are recent enough to skip.
        if let Some(act) = self.active_location().filter(|l| !locs.iter().any(|x| x.id == l.id)).cloned()
            && self.bundles.get(&act.id).is_some_and(|b| b.needs_full_refresh(now))
        {
            locs.push(act);
        }
        if let Some(aid) = &self.settings.selected_location_id
            && let Some(p) = locs.iter().position(|l| &l.id == aid)
            && p > 0
        {
            let act = locs.remove(p);
            locs.insert(0, act);
        }
        if !locs.is_empty() {
            let rest = locs.split_off(1);
            self.spawn_refresh_batch(locs);
            self.spawn_sidebar_batch(rest);
        }
        (self.last_auto_refresh, self.last_auto_refresh_wall) = (Instant::now(), now);
    }

    /// The last refresh error for the selected location (shown even while a cached bundle is on display).
    pub(crate) fn refresh_error(&self) -> Option<&str> {
        self.ui.last_error.as_deref()
    }

    /// Fetches all background sidebar cities in a SINGLE batched Open-Meteo HTTP call instead of 4 calls per city!
    fn spawn_sidebar_batch(&mut self, locs: Vec<Location>) {
        let jobs: Vec<_> = locs.into_iter().filter(|l| self.ui.loading_ids.insert(l.id.clone())).collect();
        if jobs.is_empty() {
            return;
        }
        let (c, tx, wk, prov) = (self.client.clone(), self.tx.clone(), Arc::clone(&self.wake), self.settings.provider);
        std::thread::spawn(move || {
            if is_shutting_down() {
                return;
            }
            let got = c.fetch_sidebar_batch(&jobs, prov);
            let mut ok_ids = HashSet::new();
            for (id, b) in got {
                ok_ids.insert(id.clone());
                if !is_shutting_down() {
                    let _ = tx.send(WorkerOut::WeatherLoaded(id, Box::new(b)));
                }
            }
            for l in jobs {
                if !ok_ids.contains(&l.id) {
                    let _ = tx.send(WorkerOut::WeatherFailed(l.id, "Batch refresh failed".into()));
                }
            }
            wk.notify_one();
        });
    }

    fn select_location(&mut self, id: String, jump_map: bool) -> Task<Msg> {
        (
            self.ui.detect_focus_pending,
            self.settings.selected_location_id,
            self.expanded_alert_idx,
            self.selected_day_idx,
            self.ui.last_error,
            self.bento_scroll_y,
        ) = (false, Some(id.clone()), None, None, None, 0.0);
        save_settings(&self.settings);
        if let Some(loc) = self.settings.saved_locations.iter().find(|l| l.id == id).cloned() {
            let map_light = self.active_palette().is_light;
            if jump_map {
                self.map.jump_to_location(loc.lat, loc.lon, self.ui.map_expanded, map_light);
            } else {
                self.map.sync_theme(map_light);
            }
            // A lean sidebar bundle, or one whose full data is older than the refresh interval, is fetched in full now:
            // hourly detail / air quality / alerts carried over from an earlier fetch are never left standing as current.
            if self.bundles.get(&id).is_none_or(|b| b.needs_full_refresh(Utc::now().timestamp())) {
                self.spawn_refresh_batch(vec![loc]);
            }
        }
        Task::batch(
            ["bento-scroll", "split-scroll", "hourly-strip-scroll"].map(|s| operation::snap_to(s, operation::RelativeOffset::START)),
        )
    }

    fn add_or_select_location(&mut self, loc: Location) -> Task<Msg> {
        if let Some(ex) = self.settings.saved_locations.iter().find(|l| !l.is_current_location && l.is_same_place(&loc)) {
            let id = ex.id.clone();
            return self.select_location(id, true);
        }
        let id = loc.id.clone();
        self.settings.saved_locations.push(loc);
        self.select_location(id, true)
    }

    fn apply_detected_location(&mut self, mut loc: Location, precise: bool) {
        self.ui.detect_in_flight = false;
        let (focus, lat, lon) = (std::mem::take(&mut self.ui.detect_focus_pending), loc.lat, loc.lon);
        let prev = self.settings.saved_locations.iter().find(|l| l.is_current_location).cloned();
        if loc.timezone.is_empty()
            && let Some(p) = &prev
            && !p.timezone.is_empty()
            && (p.lat - lat).abs() < 1.0
            && (p.lon - lon).abs() < 1.0
        {
            loc.timezone = p.timezone.clone();
        }
        let diff = |tol: f64| prev.as_ref().is_none_or(|p| (p.lat - lat).abs() > tol || (p.lon - lon).abs() > tol || p.name != loc.name);
        let big_move = diff(0.15);
        let moved = if !precise && self.ui.current_location_precise { big_move } else { diff(if precise { 0.002 } else { 0.015 }) };
        if precise {
            self.ui.current_location_precise = true;
        } else if big_move {
            self.ui.current_location_precise = false;
        }
        let viewing_cur = self.settings.selected_location_id.as_deref() == Some(loc.id.as_str());
        if !moved && !focus {
            return;
        }
        if moved {
            if big_move {
                self.bundles.remove(&loc.id);
            }
            self.settings.saved_locations.retain(|l| !l.is_current_location);
            self.settings.saved_locations.insert(0, loc.clone());
        }
        let should_focus = focus || (moved && viewing_cur) || self.settings.selected_location_id.is_none();
        if should_focus {
            (self.settings.selected_location_id, self.expanded_alert_idx, self.selected_day_idx, self.ui.last_error) =
                (Some(loc.id.clone()), None, None, None);
            self.map.jump_to_location(lat, lon, self.ui.map_expanded, self.active_palette().is_light);
        }
        if moved || should_focus {
            save_settings(&self.settings);
        }
        if moved || !self.bundles.contains_key(&loc.id) {
            self.ui.loading_ids.remove(&loc.id);
            self.spawn_refresh_batch(vec![loc]);
        }
    }

    fn drain_worker_messages(&mut self) {
        while let Ok(out) = self.rx.try_recv() {
            match out {
                WorkerOut::WeatherLoaded(id, mut b) => {
                    self.ui.loading_ids.remove(&id);
                    let Some(sv) = self.settings.saved_locations.iter_mut().find(|l| l.id == id) else { continue };
                    if sv.is_current_location && ((sv.lat - b.location.lat).abs() > 0.05 || (sv.lon - b.location.lon).abs() > 0.05) {
                        continue;
                    }
                    if sv.timezone.is_empty() && !b.location.timezone.is_empty() {
                        sv.timezone = b.location.timezone.clone();
                        save_settings(&self.settings);
                    } else if !sv.timezone.is_empty() {
                        b.location.timezone = sv.timezone.clone();
                    }
                    if let Some(prev) = self.bundles.get(&id) {
                        // A lean update that was already in flight must not replace a full bundle fetched since.
                        if b.lean && !prev.lean && prev.fetched_unix >= b.fetched_unix - 60 {
                            continue;
                        }
                        if b.lean {
                            // Lean sidebar update: fresh current conditions; the detail of the last full fetch stays,
                            // labelled with the time of that fetch (see `WeatherBundle::merge_lean_onto`).
                            *b = (*b).merge_lean_onto(prev);
                        } else {
                            // A failed alert request must not wipe the alerts already known for this place.
                            b.keep_known_alerts(prev);
                        }
                    }
                    let is_sel = self.settings.selected_location_id.as_deref() == Some(id.as_str());
                    if is_sel && !b.lean {
                        self.ui.last_error = None;
                    }
                    self.bundles.insert(id, *b);
                    if is_sel {
                        let map_light = self.active_palette().is_light;
                        self.map.sync_theme(map_light);
                    }
                }
                WorkerOut::WeatherFailed(id, err) => {
                    self.ui.loading_ids.remove(&id);
                    if self.settings.selected_location_id.as_deref() == Some(id.as_str()) {
                        self.ui.last_error = Some(err);
                    }
                }
                WorkerOut::SearchDone(q, res) => {
                    if q == self.ui.search_query.trim() {
                        self.ui.search_results = res;
                    }
                    self.ui.search_in_flight = false;
                }
                WorkerOut::CurrentLocation(loc, precise) => {
                    if let Some(ip) = self.last_route_ip.map(|ip| ip.to_string()) {
                        crate::storage::save_cached_geo_ip(&ip, &loc, precise);
                    }
                    self.apply_detected_location(loc, precise);
                }
                WorkerOut::CurrentLocationFailed => (self.ui.detect_in_flight, self.ui.detect_focus_pending) = (false, false),
            }
        }
    }

    pub fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Noop | Msg::RemoveLocation(_) if matches!(msg, Msg::Noop) || self.settings.saved_locations.len() <= 1 => {}
            Msg::Tick => {
                self.drain_worker_messages();
                if self.ui.search_query.len() >= 2
                    && !self.ui.search_in_flight
                    && self.ui.last_search_edit.is_some_and(|t| t.elapsed() >= Duration::from_millis(200))
                {
                    let q = self.ui.search_query.trim();
                    if q.len() >= 2 && q != self.ui.last_dispatched_query {
                        self.dispatch_search(q.to_string());
                    }
                }
                if self.last_route_check.elapsed() >= Duration::from_secs(10) {
                    self.last_route_check = Instant::now();
                    let cur = local_route_ip();
                    if cur != self.last_route_ip {
                        self.last_route_ip = cur;
                        if cur.is_some() {
                            self.spawn_detect_location();
                        }
                    }
                }
                if self.last_location_check.elapsed() >= Duration::from_secs(180)
                    || (Utc::now().timestamp() - self.last_location_wall).abs() >= 120
                {
                    self.spawn_detect_location();
                }
                // The refresh timer is checked against the wall clock as well: `Instant` stands still while the machine
                // is suspended, so after a resume the data would otherwise sit unrefreshed and be shown as current.
                let wall = Utc::now().timestamp();
                if self.last_auto_refresh.elapsed() > Duration::from_secs(REFRESH_INTERVAL_SECS as u64)
                    || (wall - self.last_auto_refresh_wall).abs() > REFRESH_INTERVAL_SECS
                {
                    self.spawn_refresh_all();
                } else if (wall - self.last_active_retry_wall).abs() >= 60
                    && let Some(loc) = self.active_location().filter(|l| !self.ui.loading_ids.contains(&l.id)).cloned()
                    && self.bundles.get(&loc.id).is_none_or(|b| b.needs_full_refresh(wall))
                {
                    // The selected location has no full, current bundle (failed refresh, lean update that landed after it
                    // was selected, or data older than the interval): fetch it in full, at most once a minute.
                    self.last_active_retry_wall = wall;
                    self.spawn_refresh_batch(vec![loc]);
                }
                self.map.tick(&self.bundles, self.ui.map_expanded);
                return self.flush_gpu_pins();
            }
            Msg::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            Msg::ToggleUnits => {
                let s = &mut self.settings;
                (s.temp_unit, s.wind_unit, s.pressure_unit, s.precip_unit) = if s.temp_unit == TempUnit::Celsius {
                    (TempUnit::Fahrenheit, WindUnit::Mph, PressureUnit::InHg, PrecipUnit::Inches)
                } else {
                    (TempUnit::Celsius, WindUnit::KmH, PressureUnit::HPa, PrecipUnit::Mm)
                };
                save_settings(&self.settings);
                self.map.tick(&self.bundles, self.ui.map_expanded);
            }
            Msg::TogglePreferences => {
                self.ui.show_preferences = !self.ui.show_preferences;
                if self.ui.show_preferences {
                    self.ui.detail_metric = None;
                    self.clear_search();
                }
            }
            Msg::OpenDetail(m) => {
                self.ui.detail_metric = Some(m);
                self.ui.show_preferences = false;
                self.clear_search();
            }
            Msg::CloseDetail => self.ui.detail_metric = None,
            Msg::RefreshAll => {
                self.spawn_refresh_all();
                self.spawn_detect_location();
            }
            Msg::SelectLocation(id) => return self.select_location(id, true),
            Msg::SelectIndex(i) => {
                if let Some(id) = self.settings.saved_locations.get(i).map(|l| l.id.clone()) {
                    return self.select_location(id, true);
                }
            }
            Msg::RemoveLocation(id) => {
                let was_sel = self.settings.selected_location_id.as_deref() == Some(id.as_str());
                self.settings.saved_locations.retain(|l| l.id != id);
                self.bundles.remove(&id);
                self.ui.loading_ids.remove(&id);
                if was_sel && let Some(next) = self.settings.saved_locations.first().map(|l| l.id.clone()) {
                    return self.select_location(next, true);
                }
                save_settings(&self.settings);
            }
            Msg::DetectLocation => {
                self.ui.detect_focus_pending = true;
                self.spawn_detect_location();
            }
            Msg::SearchChanged(v) => {
                (self.ui.search_query, self.ui.last_search_edit) = (v, Some(Instant::now()));
                if self.ui.search_query.trim().len() < 2 {
                    self.ui.search_results.clear();
                    self.ui.last_dispatched_query.clear();
                }
            }
            Msg::SubmitSearch => {
                if let Some(first) = self.ui.search_results.first().cloned() {
                    self.clear_search();
                    return self.add_or_select_location(first);
                }
                let q = self.ui.search_query.trim().to_string();
                if q.len() >= 2 && !self.ui.search_in_flight {
                    self.dispatch_search(q);
                }
            }
            Msg::FocusSearch => return operation::focus("city-search-input"),
            Msg::ClearSearch => self.clear_search(),
            Msg::PickSearchResult(l) => {
                self.clear_search();
                return self.add_or_select_location(l);
            }
            Msg::ToggleAlertDetails(i) => self.expanded_alert_idx = (self.expanded_alert_idx != Some(i)).then_some(i),
            Msg::ToggleDayDetails(i) => self.selected_day_idx = (self.selected_day_idx != Some(i)).then_some(i),
            Msg::BentoScrolled(y) => self.bento_scroll_y = y.max(0.0),
            Msg::ToggleMap => return self.update(Msg::Map(if self.ui.map_expanded { MapMsg::CloseMap } else { MapMsg::ExpandMap })),
            Msg::Pref(p) => {
                let (mut refresh, mut theme_ch) = (false, false);
                match p {
                    Pref::Theme(m) => {
                        self.settings.theme_mode = m;
                        theme_ch = true;
                    }
                    Pref::Wind(u) => self.settings.wind_unit = u,
                    Pref::Pressure(u) => self.settings.pressure_unit = u,
                    Pref::Precip(u) => self.settings.precip_unit = u,
                    Pref::Use24h(v) => self.settings.use_24h_time = v,
                    Pref::Animate(v) => self.settings.animate_weather = v,
                    Pref::InterpolateForecast(v) => {
                        self.settings.interpolate_forecast = v;
                        crate::providers::set_forecast_interpolation(v);
                        self.map.reapply_manifest();
                    }
                    Pref::Provider(pr) => {
                        self.settings.provider = pr;
                        refresh = true;
                    }
                }
                save_settings(&self.settings);
                if theme_ch {
                    let map_light = self.active_palette().is_light;
                    self.map.sync_theme(map_light);
                    if let Some(loc) = self.active_location().cloned() {
                        self.map.prewarm_region(loc.lat, loc.lon, self.ui.map_expanded, map_light);
                    }
                }
                if refresh {
                    self.ui.loading_ids.clear();
                    self.spawn_refresh_all();
                }
            }
            Msg::Map(mm) => {
                let ext =
                    self.map.handle_msg(mm, &self.settings.saved_locations, self.settings.selected_location_id.as_deref(), &self.settings);
                if let Some(ext) = ext {
                    match ext {
                        MapExternalAction::SetExpanded(e) => self.ui.map_expanded = e,
                        MapExternalAction::SelectSaved(id) => return self.select_location(id, false),
                        MapExternalAction::SaveLocation(loc) => return self.add_or_select_location(loc),
                        MapExternalAction::CopyText(text) => return iced::clipboard::write(text),
                    }
                }
                return self.flush_gpu_pins();
            }
            Msg::TilesGpuPinned(pins) => self.map.store_gpu_pins(pins),
            Msg::Escape => {
                if self.ui.detail_metric.is_some() {
                    self.ui.detail_metric = None;
                } else if self.ui.show_preferences {
                    self.ui.show_preferences = false;
                } else if !self.ui.search_query.is_empty() || !self.ui.search_results.is_empty() {
                    self.clear_search();
                } else if self.map.has_context_menu() {
                    self.map.close_context_menu();
                } else if self.map.show_layer_menu() {
                    self.map.close_layer_menu();
                } else if self.map.inspected_coord().is_some() {
                    let (sv, act) = (&self.settings.saved_locations, self.settings.selected_location_id.as_deref());
                    let _ = self.map.handle_msg(MapMsg::CloseInspector, sv, act, &self.settings);
                } else if self.ui.map_expanded && self.map.is_full_screen() {
                    return self.update(Msg::Map(MapMsg::ToggleFullScreen));
                } else if self.ui.map_expanded {
                    return self.update(Msg::Map(MapMsg::CloseMap));
                } else {
                    self.selected_day_idx = None;
                }
            }
            Msg::WindowResized(s) if s.width > 100.0 && s.height > 100.0 => self.window_size = s,
            Msg::WindowResized(_) | Msg::Noop => {}
            Msg::WindowCloseRequested => {
                request_shutdown();
                flush_pending_settings();
                return iced::exit();
            }
        }
        Task::none()
    }

    fn flush_gpu_pins(&self) -> Task<Msg> {
        // Tiles are queued ONLY AFTER `draw_tile_layer_filtered` has rendered them into `iced_wgpu`'s shared atlas,
        // so `image::allocate(h)` hits the ~20 ns in-place atlas pin fast path (ZERO worker textures, ZERO `device.poll(Wait)`,
        // ZERO `Shell::invalidate_layout`, and 100% shared-atlas GPU instancing!).
        // Collect the whole batch into ONE `Msg::TilesGpuPinned` so 64 pins cause 1 update instead of a 64-frame render storm!
        let pins = self.map.take_pending_gpu_pins(64);
        if pins.is_empty() {
            return Task::none();
        }
        Task::batch(pins.into_iter().map(|(k, h)| iced_runtime::image::allocate(h).map(move |r| (k, r.ok()))))
            .collect()
            .map(Msg::TilesGpuPinned)
    }

    pub fn active_location(&self) -> Option<&Location> {
        self.settings.selected_location_id.as_ref().and_then(|id| self.settings.saved_locations.iter().find(|l| &l.id == id))
    }
    pub fn active_bundle(&self) -> Option<&WeatherBundle> {
        self.settings.selected_location_id.as_ref().and_then(|id| self.bundles.get(id))
    }
    pub fn active_palette(&self) -> SkyPalette {
        let (c, d) = self.active_bundle().map_or((WeatherCondition::Overcast, true), |b| (b.current.condition, b.current.is_day));
        palette_for(c, d, self.settings.theme_mode)
    }

    pub fn view(&self) -> Element<'_, Msg> {
        ensure_fonts_loaded();
        let now = Utc::now().timestamp();
        let pal = self.active_palette();
        let (cond, day) = self.active_bundle().map_or((WeatherCondition::Overcast, true), |b| (b.current.condition, b.current.is_day));
        let skey = (cond, day, self.settings.theme_mode);
        if self.sky_key.get() != skey {
            self.sky_cache.clear();
            self.sky_key.set(skey);
        }
        // Sync GPU-cached Bento card canvases (invalidates only when data, units, theme, expansion, or minute change).
        {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                self.settings.selected_location_id.as_deref(),
                self.active_bundle().map(|b| (b.current.updated_unix, b.air_quality.as_ref().map(|a| a.us_aqi))),
                self.settings.theme_mode,
                self.settings.temp_unit,
                self.settings.wind_unit,
                self.settings.pressure_unit,
                self.settings.precip_unit,
                self.settings.use_24h_time,
                self.ui.map_expanded,
                self.show_sidebar,
                self.expanded_alert_idx,
                self.selected_day_idx,
            )
                .hash(&mut h);
            ((self.window_size.width / 20.0).round() as i32, now / 60).hash(&mut h);
            // Fetch times, the alert set and the last refresh error are all on display (status line, alert card).
            let fetch = self.active_bundle().map(|b| (b.fetched_unix, b.full_fetched_unix, b.alerts_fetched_unix, b.alerts.len(), b.lean));
            (fetch, self.ui.last_error.as_deref()).hash(&mut h);
            self.bento_sig.set(h.finish() | 1);
        }
        let main = if self.ui.map_expanded { self.view_split_map_layout(pal, now) } else { self.view_full_bento_layout(pal, now) };
        let side = if self.show_sidebar { self.view_sidebar(pal, now) } else { sp_w(0) };
        let full_map = self.ui.map_expanded && self.map.is_full_screen();
        let mut layers: Vec<Element<'_, Msg>> = Vec::with_capacity(5);
        if !full_map {
            layers.push(sky_backdrop_canvas(&self.sky_cache, pal, cond, day));
        }
        // Skip full-window particle canvas when map is expanded or current weather has no stars/rain/snow;
        // cache tessellated stars/precip at 30fps steps so mouse hovers over Bento cost 0 GPU uploads!
        let has_particles = !self.ui.map_expanded
            && !pal.is_light
            && (!day
                || matches!(
                    cond,
                    WeatherCondition::Drizzle
                        | WeatherCondition::Rain
                        | WeatherCondition::HeavyRain
                        | WeatherCondition::FreezingRain
                        | WeatherCondition::Thunderstorm
                        | WeatherCondition::Snow
                        | WeatherCondition::HeavySnow
                        | WeatherCondition::Sleet
                ));
        if has_particles {
            let phase = if self.settings.animate_weather { self.boot_instant.elapsed().as_secs_f32() } else { 0.0 };
            let bucket = (phase * 30.0) as u32;
            let pkey = (cond, day, bucket);
            if self.sky_particles_key.get() != pkey {
                self.sky_particles_cache.clear();
                self.sky_particles_key.set(pkey);
            }
            layers.push(cached_widget_canvas(
                &self.sky_particles_cache,
                WidgetCanvas::SkyParticles(pal.is_light, cond, day, bucket as f32 / 30.0),
                Fill,
                Fill,
            ));
        }
        layers.push(row![side, main].width(Fill).height(Fill).into());
        let q = self.ui.search_query.trim();
        if !self.ui.search_results.is_empty() || self.ui.search_in_flight || (q.len() >= 2 && self.ui.last_dispatched_query == q) {
            layers.push(self.view_search_dropdown(pal));
        }
        if self.ui.show_preferences {
            layers.push(self.view_preferences_overlay(pal));
        } else if let Some(dm) = self.ui.detail_metric {
            let dsig = (self.bento_sig.get(), dm);
            layers.push(iced::widget::lazy(dsig, move |_| self.view_detail_overlay(dm, pal, now)).into());
        }
        stack(layers).width(Fill).height(Fill).into()
    }

    fn view_sidebar(&self, pal: SkyPalette, now: i64) -> Element<'_, Msg> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            self.settings.selected_location_id.as_deref(),
            self.settings.theme_mode,
            self.settings.temp_unit,
            self.settings.use_24h_time,
            pal.is_light,
            now / 60,
            self.settings.saved_locations.len(),
        )
            .hash(&mut h);
        for loc in &self.settings.saved_locations {
            let b = self.bundles.get(&loc.id);
            (
                &loc.id,
                &loc.name,
                loc.is_current_location,
                b.map(|x| (x.current.updated_unix, x.current.temp_c.to_bits(), x.current.condition as u8, x.current.is_day)),
            )
                .hash(&mut h);
        }
        let sig = h.finish() | 1;
        iced::widget::lazy(sig, move |_| self.build_sidebar_owned(pal, now)).into()
    }

    fn build_sidebar_owned(&self, pal: SkyPalette, now: i64) -> Element<'static, Msg> {
        let hv = if pal.is_light { rgba8(20, 40, 75, 0.10) } else { rgba8(255, 255, 255, 0.14) };
        let cols = (pal.text_secondary, Color::TRANSPARENT, hv);
        let top = row![
            icon_btn(UiIcon::Sidebar, 15.0, (28, 26), cols, 7.0, Msg::ToggleSidebar),
            sp_w(Fill),
            icon_btn(UiIcon::Locate, 13.5, (26, 26), cols, 7.0, Msg::DetectLocation),
        ];
        // Reserve top clearance for native macOS traffic-light buttons on transparent fullsize titlebar.
        let top_pad = iced::Padding { top: if cfg!(target_os = "macos") { 34.0 } else { 8.0 }, right: 10.0, bottom: 8.0, left: 10.0 };
        let top = top.padding(top_pad).align_y(Alignment::Center);
        let total = self.settings.saved_locations.len();
        let mut items: Vec<Element<'static, Msg>> = Vec::with_capacity(total);
        for loc in &self.settings.saved_locations {
            let (sel, b) = (self.settings.selected_location_id.as_deref() == Some(&loc.id), self.bundles.get(&loc.id));
            // Signature Apple Weather live mini-sky gradient per saved city!
            let cp = b.map_or_else(
                || palette_for(WeatherCondition::Overcast, true, ThemeMode::DynamicSky),
                |x| palette_for(x.current.condition, x.current.is_day, ThemeMode::DynamicSky),
            );
            let title_row: Element<'static, Msg> = if loc.is_current_location {
                row![txt("My Location", 13.5, Weight::Bold, Color::WHITE), icon_svg(UiIcon::Locate, 10.0, rgba8(255, 255, 255, 0.88))]
                    .spacing(4)
                    .align_y(Alignment::Center)
                    .into()
            } else {
                txt(loc.name.clone(), 13.5, Weight::Bold, Color::WHITE).into()
            };
            let sub = if loc.is_current_location { loc.name.clone() } else { loc.format_local_time(now, self.settings.use_24h_time) };
            // A row that only covers the rest of today is not the day's high / low, so the compact sidebar omits it.
            let hl = b
                .and_then(|b| b.daily.get(crate::cards::today_daily_index_at(b, loc, now)))
                .filter(|d| !d.partial)
                .map_or_else(String::new, |d| {
                    format!("H:{}  L:{}", self.settings.format_temp(d.temp_max_c), self.settings.format_temp(d.temp_min_c))
                });
            let cnd = text(b.map_or("Loading…", |b| b.current.condition.label())).size(10.5).color(rgba8(240, 246, 255, 0.86));
            let mut sub_row = row![txt(sub, 10.5, Weight::Medium, rgba8(236, 244, 255, 0.82))].spacing(5).align_y(Alignment::Center);
            if total > 1 && !loc.is_current_location && sel {
                sub_row = sub_row.push(icon_btn(
                    UiIcon::Close,
                    8.5,
                    (15, 15),
                    (rgba8(255, 255, 255, 0.90), rgba8(0, 0, 0, 0.28), rgba8(0, 0, 0, 0.50)),
                    7.5,
                    Msg::RemoveLocation(loc.id.clone()),
                ));
            }
            let lcol = column![title_row, sub_row, sp_h(Fill), cnd].spacing(1).height(Fill);
            let tmp = b.map_or_else(|| crate::cards::DASH.into(), |b| self.settings.format_temp(b.current.temp_c));
            let rcol = column![
                txt(tmp, 34.0, Weight::Normal, Color::WHITE),
                sp_h(Fill),
                txt(hl, 10.0, Weight::Medium, rgba8(240, 246, 255, 0.90))
            ]
            .height(Fill)
            .align_x(Alignment::End);
            let inner = row![lcol.width(Fill), rcol].width(Fill).height(64).align_y(Alignment::Center);
            let sbtn = pbtn(
                inner,
                [8, 10],
                (Color::TRANSPARENT, rgba8(255, 255, 255, 0.08), Color::WHITE),
                12.0,
                Msg::SelectLocation(loc.id.clone()),
            )
            .width(Fill);
            let card = container(sbtn).width(Fill).clip(true).style(sky_mini_card(cp.sky_top, cp.sky_bot, sel, pal.is_light));
            items.push(container(card).padding([4, 8]).into());
        }
        let body = column![top, scrollable(column(items).padding([2, 0]).width(Fill)).height(Fill).style(sleek_scrollable)];
        let rail = container(body).width(214).height(Fill).style(flat_box(pal.sidebar_bg, 0.0));
        row![rail, container(sp_w(1)).height(Fill).style(flat_box(pal.divider, 0.0))].height(Fill).into()
    }

    fn view_top_toolbar_row(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let (bbg, bhv, sbg, sbdr) = if pal.is_light {
            (rgba8(255, 255, 255, 0.76), rgba8(255, 255, 255, 0.96), rgba8(255, 255, 255, 0.82), rgba8(28, 48, 82, 0.12))
        } else {
            (rgba8(18, 28, 46, 0.34), rgba8(255, 255, 255, 0.18), rgba8(18, 28, 46, 0.42), rgba8(255, 255, 255, 0.18))
        };
        let bcol = (pal.text_primary, bbg, bhv);
        let left: Element<'_, Msg> = if !self.show_sidebar {
            let btn = icon_btn(UiIcon::Sidebar, 15.0, (28, 28), bcol, 8.0, Msg::ToggleSidebar);
            if cfg!(target_os = "macos") { row![sp_w(68), btn].align_y(Alignment::Center).into() } else { btn }
        } else {
            sp_w(1)
        };
        let ulbl = if self.settings.temp_unit == TempUnit::Celsius { "°C" } else { "°F" };
        let unit = pbtn(txt(ulbl, 11.5, Weight::Semibold, pal.text_primary), [4, 9], (bbg, bhv, pal.text_primary), 8.0, Msg::ToggleUnits);
        let inp =
            text_input("Search", &self.ui.search_query).id("city-search-input").on_input(Msg::SearchChanged).on_submit(Msg::SubmitSearch);
        let inp = inp.size(12.5).font(fw(Weight::Normal)).padding([4, 4]).width(if self.ui.search_query.is_empty() { 166 } else { 146 });
        let inp = inp.style(move |_, _| text_input::Style {
            background: iced::Background::Color(Color::TRANSPARENT),
            border: iced::Border::default(),
            icon: Color::TRANSPARENT,
            placeholder: pal.text_muted,
            value: pal.text_primary,
            selection: Color { a: 0.38, ..pal.accent },
        });
        let mut srow = row![icon_svg(UiIcon::Search, 12.5, pal.text_muted), inp].spacing(5).padding([1, 9]).align_y(Alignment::Center);
        if !self.ui.search_query.is_empty() {
            srow = srow.push(pbtn(
                icon_svg(UiIcon::Close, 10.0, pal.text_muted),
                [2, 2],
                (Color::TRANSPARENT, bhv, pal.text_primary),
                5.0,
                Msg::ClearSearch,
            ));
        }
        // Authentic macOS Weather sticky compact hero summary when scrolled past the main hero!
        let sticky: Element<'_, Msg> = if !self.ui.map_expanded
            && self.bento_scroll_y > 120.0
            && let Some(b) = self.active_bundle()
        {
            let loc = self.active_location().unwrap_or(&b.location);
            let pill = row![
                txt(loc.name.clone(), 13.0, Weight::Semibold, pal.text_primary),
                txt("·", 12.5, Weight::Medium, pal.text_muted),
                txt(self.settings.format_temp(b.current.temp_c), 13.0, Weight::Bold, pal.text_primary),
                txt("|", 11.5, Weight::Normal, pal.text_muted),
                txt(b.current.condition.label(), 12.5, Weight::Medium, pal.text_secondary),
            ]
            .spacing(6)
            .padding([4, 12])
            .align_y(Alignment::Center);
            container(pill).style(hud_pill(sbg, sbdr, 12.0)).into()
        } else {
            sp_w(0)
        };
        let bar = row![
            left,
            sp_w(Fill),
            sticky,
            sp_w(Fill),
            unit,
            icon_btn(UiIcon::Refresh, 13.5, (28, 28), bcol, 8.0, Msg::RefreshAll),
            icon_btn(UiIcon::Sliders, 13.5, (28, 28), bcol, 8.0, Msg::TogglePreferences),
            container(srow).style(hud_pill(sbg, sbdr, 9.0))
        ];
        bar.spacing(6).padding([9, 14]).align_y(Alignment::Center).width(Fill).into()
    }

    fn view_header_stack(&self, b: &WeatherBundle, pal: SkyPalette, w: f32, gap: f32, now: i64, di: usize) -> Element<'static, Msg> {
        let mut h = column![self.view_hero(b, pal, di, now)].spacing(gap).width(w).align_x(Alignment::Center);
        if !b.alerts.is_empty() {
            h = h.push(self.view_alerts_card(b, pal, w, now));
        }
        if let Some(mc) = self.view_minutely_card(b, pal, w, now) {
            h = h.push(mc);
        }
        h.into()
    }

    fn view_full_bento_layout(&self, pal: SkyPalette, now: i64) -> Element<'_, Msg> {
        let toolbar = self.view_top_toolbar_row(pal);
        let Some(b) = self.active_bundle() else {
            let err = text(self.ui.last_error.as_deref().unwrap_or("")).size(12.0).color(rgb8(255, 120, 120));
            let load = container(
                column![txt("Loading Weather…", 18.0, Weight::Medium, pal.text_primary), err].spacing(6).align_x(Alignment::Center),
            );
            return column![toolbar, load.center_x(Fill).center_y(Fill)].width(Fill).height(Fill).into();
        };
        let di = crate::cards::today_daily_index_at(b, self.active_location().unwrap_or(&b.location), now);
        let (avail, g, th) = ((self.window_size.width - if self.show_sidebar { 215.0 } else { 0.0 }).max(480.0), 12.0_f32, 156.0_f32);
        let wide = avail >= 820.0;
        let u = if wide {
            ((avail - 64.0 - g * 5.0) / 6.0).clamp(112.0, 134.0).floor()
        } else {
            ((avail - 40.0 - g * 3.0) / 4.0).clamp(108.0, 150.0).floor()
        };
        let (w2, grid_w) = (u * 2.0 + g, if wide { u * 6.0 + g * 5.0 } else { u * 4.0 + g * 3.0 });
        let lkey = (self.bento_sig.get(), wide, u.to_bits());
        let map = self.view_map_bento_card(pal, w2, th * 2.0 + g);
        // Memoize all 14 static Bento cards via `lazy` so mouse scrubs, DetailChart hovers, search typing,
        // scrolling, and sky animations allocate ZERO Bento widget trees!
        let top_lazy: Element<'_, Msg> = iced::widget::lazy(lkey, move |_| -> Element<'static, Msg> {
            column![self.view_header_stack(b, pal, grid_w, 18.0, now, di), self.view_hourly_card(b, pal, grid_w, 16, now)]
                .spacing(16)
                .width(grid_w)
                .align_x(Alignment::Center)
                .into()
        })
        .into();
        // The footer names the sources actually used for this bundle and, when it is not current, how old it is / why.
        let status = self.data_status_line(b, now).map_or_else(String::new, |s| format!("{s} • "));
        let attr = format!("{status}Weather: {} • Radar: LibreWXR (CC BY 4.0), NOAA HRRR/MRMS • Map: Esri & OSM", b.source_name);
        let content = if wide {
            let left_mid: Element<'_, Msg> = iced::widget::lazy(lkey, move |_| -> Element<'static, Msg> {
                let d10 = self.view_ten_day_card(b, pal, w2, Some(th * 3.0 + g * 2.0), di);
                let aqi = self.view_aqi_card(b, pal, w2, th);
                let wind = self.view_wind_card(b, pal, w2, th);
                let uv = self.view_uv_card(b, pal, u, th, now);
                let sun = self.view_sun_card(b, pal, u, th, now, di);
                let mid = column![aqi, wind, row![uv, sun].spacing(g)].spacing(g);
                row![d10, mid].spacing(g).align_y(Alignment::Start).into()
            })
            .into();
            let right_bot: Element<'_, Msg> = iced::widget::lazy(lkey, move |_| -> Element<'static, Msg> {
                row![self.view_feels_like_card(b, pal, u, th), self.view_precip_summary_card(b, pal, u, th, di)].spacing(g).into()
            })
            .into();
            let bottom_lazy: Element<'_, Msg> = iced::widget::lazy(lkey, move |_| -> Element<'static, Msg> {
                let r = row![
                    self.view_moon_card(pal, w2, th, now),
                    self.view_humidity_card(b, pal, u, th),
                    self.view_visibility_card(b, pal, u, th),
                    self.view_pressure_card(b, pal, u, th, now),
                    self.view_averages_card(b, pal, u, th, di),
                ]
                .spacing(g);
                column![r, text(attr.clone()).size(10.5).color(pal.text_muted)].spacing(16).align_x(Alignment::Center).into()
            })
            .into();
            column![top_lazy, row![left_mid, column![map, right_bot].spacing(g)].spacing(g).align_y(Alignment::Start), bottom_lazy]
                .spacing(16)
                .width(grid_w)
                .align_x(Alignment::Center)
        } else {
            let d10_lazy: Element<'_, Msg> =
                iced::widget::lazy(lkey, move |_| self.view_ten_day_card(b, pal, w2, Some(th * 3.0 + g * 2.0), di)).into();
            let aqi_lazy: Element<'_, Msg> = iced::widget::lazy(lkey, move |_| self.view_aqi_card(b, pal, w2, th)).into();
            let lower_lazy: Element<'_, Msg> = iced::widget::lazy(lkey, move |_| -> Element<'static, Msg> {
                column![
                    row![
                        self.view_wind_card(b, pal, w2, th),
                        self.view_uv_card(b, pal, u, th, now),
                        self.view_sun_card(b, pal, u, th, now, di)
                    ]
                    .spacing(g),
                    row![
                        self.view_moon_card(pal, w2, th, now),
                        self.view_feels_like_card(b, pal, u, th),
                        self.view_precip_summary_card(b, pal, u, th, di),
                    ]
                    .spacing(g),
                    row![
                        self.view_humidity_card(b, pal, u, th),
                        self.view_visibility_card(b, pal, u, th),
                        self.view_pressure_card(b, pal, u, th, now),
                        self.view_averages_card(b, pal, u, th, di),
                    ]
                    .spacing(g),
                    text(attr.clone()).size(10.5).color(pal.text_muted),
                ]
                .spacing(16)
                .align_x(Alignment::Center)
                .into()
            })
            .into();
            column![top_lazy, row![d10_lazy, column![map, aqi_lazy].spacing(g)].spacing(g), lower_lazy]
                .spacing(16)
                .width(grid_w)
                .align_x(Alignment::Center)
        };
        let body = scrollable(column![sp_h(10), container(content).center_x(Fill), sp_h(28)].width(Fill))
            .id("bento-scroll")
            .on_scroll(|vp| Msg::BentoScrolled(vp.absolute_offset().y))
            .width(Fill)
            .height(Fill);
        column![toolbar, body.style(sleek_scrollable)].width(Fill).height(Fill).into()
    }

    fn view_split_map_layout(&self, pal: SkyPalette, now: i64) -> Element<'_, Msg> {
        let (toolbar, avail) = (self.view_top_toolbar_row(pal), self.window_size.width - if self.show_sidebar { 215.0 } else { 0.0 });
        let left: Element<'_, Msg> = if !self.map.is_full_screen() && avail >= 820.0 && self.active_bundle().is_some() {
            // Cache the entire 15-card left Bento column via `lazy` so 60fps radar playback, panning, zooming,
            // and wind streamlines do ZERO Bento widget allocations!
            iced::widget::lazy(self.bento_sig.get(), move |_| self.build_split_bento_owned(pal, now)).into()
        } else {
            sp_w(0)
        };
        let bdr = if pal.is_light { rgba8(20, 35, 60, 0.12) } else { rgba8(255, 255, 255, 0.15) };
        let keys = !self.ui.show_preferences && self.ui.detail_metric.is_none() && self.ui.search_query.is_empty();
        let mv = self.map.view_expanded(
            &self.settings.saved_locations,
            &self.bundles,
            self.settings.selected_location_id.as_deref(),
            &self.settings,
            keys,
            Msg::Map,
        );
        let framed = container(mv).width(Fill).height(Fill).clip(true).style(move |_| container::Style {
            border: iced::Border { color: bdr, width: 1.0, radius: 14.0.into() },
            ..Default::default()
        });
        let map_pane = container(framed).width(Fill).height(Fill).padding([0, 12]);
        column![toolbar, row![left, map_pane].width(Fill).height(Fill), sp_h(10)].width(Fill).height(Fill).into()
    }

    fn build_split_bento_owned(&self, pal: SkyPalette, now: i64) -> Element<'static, Msg> {
        let Some(b) = self.active_bundle() else { return sp_w(0) };
        let di = crate::cards::today_daily_index_at(b, self.active_location().unwrap_or(&b.location), now);
        let (cw, g, th) = (344.0_f32, 10.0_f32, 142.0_f32);
        let u = (cw - g) * 0.5;
        let side3 = column![
            self.view_aqi_card(b, pal, u - 16.0, th),
            self.view_wind_card(b, pal, u - 16.0, th),
            self.view_uv_card(b, pal, u - 16.0, th, now)
        ]
        .spacing(g);
        let col = column![
            self.view_header_stack(b, pal, cw, g, now, di),
            self.view_hourly_card(b, pal, cw, 9, now),
            row![self.view_ten_day_card(b, pal, u + 16.0, None, di), side3].spacing(g),
            row![self.view_sun_card(b, pal, u, th, now, di), self.view_feels_like_card(b, pal, u, th)].spacing(g),
            row![self.view_precip_summary_card(b, pal, u, th, di), self.view_humidity_card(b, pal, u, th)].spacing(g),
            self.view_moon_card(pal, cw, th, now),
            row![self.view_visibility_card(b, pal, u, th), self.view_pressure_card(b, pal, u, th, now)].spacing(g),
            self.view_averages_card(b, pal, cw, th, di),
            sp_h(16),
        ]
        .spacing(g)
        .width(cw);
        scrollable(container(col).padding([4, 12])).id("split-scroll").width(cw + 24.0).height(Fill).style(sleek_scrollable).into()
    }

    fn view_search_dropdown(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let (bg, bdr, fg, muted) = if pal.is_light {
            (rgba8(248, 251, 255, 0.97), rgba8(20, 40, 75, 0.14), rgb8(20, 28, 42), rgba8(46, 62, 86, 0.72))
        } else {
            (rgba8(22, 30, 44, 0.96), rgba8(255, 255, 255, 0.18), Color::WHITE, rgba8(215, 226, 244, 0.78))
        };
        let items: Vec<Element<'_, Msg>> = if self.ui.search_results.is_empty() {
            let lbl = if self.ui.search_in_flight { "Searching cities…" } else { "No matching cities found" };
            vec![container(txt(lbl, 12.0, Weight::Medium, muted)).padding([6, 10]).into()]
        } else {
            self.ui
                .search_results
                .iter()
                .map(|r| {
                    let sub = r.subtitle();
                    let lbl = if sub.is_empty() { r.name.clone() } else { format!("{} — {sub}", r.name) };
                    pbtn(
                        txt(lbl, 12.0, Weight::Medium, fg),
                        [6, 10],
                        (Color::TRANSPARENT, rgba8(10, 132, 255, 0.85), Color::WHITE),
                        7.0,
                        Msg::PickSearchResult(r.clone()),
                    )
                    .width(Fill)
                    .into()
                })
                .collect()
        };
        let menu = mouse_area(container(column(items).spacing(2).width(268)).padding(6).style(hud_pill(bg, bdr, 10.0))).on_press(Msg::Noop);
        // Leave the top 44px toolbar unblocked so clicking inside the search input never wipes the query!
        let dismiss = mouse_area(column![row![sp_w(Fill), menu, sp_w(14)], sp_h(Fill)].width(Fill).height(Fill)).on_press(Msg::ClearSearch);
        column![sp_h(44), dismiss].width(Fill).height(Fill).into()
    }

    fn view_preferences_overlay(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let (s, is_c, lt) = (&self.settings, self.settings.temp_unit == TempUnit::Celsius, pal.is_light);
        let (sheet_bg, sheet_bdr, fg, sub_fg, track_bg, div_col, hov_bg, prv_bg) = if lt {
            (
                rgba8(246, 249, 254, 0.97),
                rgba8(20, 40, 75, 0.14),
                rgb8(18, 26, 40),
                rgba8(32, 46, 68, 0.82),
                rgba8(20, 40, 75, 0.08),
                rgba8(20, 40, 75, 0.12),
                rgba8(20, 40, 75, 0.12),
                rgba8(20, 40, 75, 0.05),
            )
        } else {
            (
                rgba8(20, 28, 42, 0.96),
                rgba8(255, 255, 255, 0.18),
                Color::WHITE,
                rgba8(224, 234, 248, 0.88),
                rgba8(255, 255, 255, 0.08),
                rgba8(255, 255, 255, 0.14),
                rgba8(255, 255, 255, 0.12),
                rgba8(255, 255, 255, 0.06),
            )
        };
        let seg = |lbl: &'static str, act: bool, msg: Msg| -> Element<'_, Msg> {
            let cols = if act { (rgb8(10, 132, 255), rgb8(28, 146, 255), Color::WHITE) } else { (Color::TRANSPARENT, hov_bg, sub_fg) };
            pbtn(txt(lbl, 11.5, if act { Weight::Semibold } else { Weight::Medium }, cols.2), [5, 10], cols, 7.0, msg).into()
        };
        fn prow<'a>(lbl: &'static str, items: Vec<Element<'a, Msg>>, sub_fg: Color, track_bg: Color) -> Element<'a, Msg> {
            let head = container(txt(lbl, 12.5, Weight::Medium, sub_fg)).width(102);
            let track = container(row(items).spacing(3)).padding(3).style(flat_box(track_bg, 9.0));
            row![head, track].spacing(8).align_y(Alignment::Center).into()
        }
        let close = pbtn(icon_svg(UiIcon::Close, 13.5, fg), [5, 5], (track_bg, hov_bg, fg), 12.0, Msg::TogglePreferences);
        let head = row![txt("Weather Settings", 15.5, Weight::Semibold, fg), sp_w(Fill), close].align_y(Alignment::Center);
        let wind_c = [("km/h", WindUnit::KmH), ("mph", WindUnit::Mph), ("m/s", WindUnit::Ms), ("knots", WindUnit::Knots)]
            .into_iter()
            .map(|(l, u)| seg(l, s.wind_unit == u, Msg::Pref(Pref::Wind(u))))
            .collect();
        let prs_c = [("hPa", PressureUnit::HPa), ("inHg", PressureUnit::InHg)]
            .into_iter()
            .map(|(l, u)| seg(l, s.pressure_unit == u, Msg::Pref(Pref::Pressure(u))))
            .collect();
        let prc_c = [("mm", PrecipUnit::Mm), ("in", PrecipUnit::Inches)]
            .into_iter()
            .map(|(l, u)| seg(l, s.precip_unit == u, Msg::Pref(Pref::Precip(u))))
            .collect();
        let th_c = [("Dynamic", ThemeMode::DynamicSky), ("Dark", ThemeMode::Dark), ("Light", ThemeMode::Light)]
            .into_iter()
            .map(|(l, m)| seg(l, s.theme_mode == m, Msg::Pref(Pref::Theme(m))))
            .collect();
        let prv_c: Vec<_> = [ProviderChoice::Auto, ProviderChoice::MetNorway, ProviderChoice::OpenMeteo]
            .into_iter()
            .map(|p| {
                let act = s.provider == p;
                let cols = if act {
                    (rgba8(10, 132, 255, if lt { 0.16 } else { 0.24 }), rgba8(10, 132, 255, if lt { 0.22 } else { 0.30 }), fg)
                } else {
                    (prv_bg, hov_bg, sub_fg)
                };
                let mut r = row![txt(p.label(), 12.0, if act { Weight::Semibold } else { Weight::Normal }, cols.2).width(Fill)]
                    .align_y(Alignment::Center);
                if act {
                    r = r.push(icon_svg(UiIcon::Check, 13.0, if lt { rgb8(0, 114, 232) } else { rgb8(72, 186, 255) }));
                }
                pbtn(r, [7, 11], cols, 9.0, Msg::Pref(Pref::Provider(p))).width(Fill).into()
            })
            .collect();
        let body = column![
            head,
            hairline(div_col),
            prow(
                "Temperature",
                vec![
                    seg("°C", is_c, if is_c { Msg::Noop } else { Msg::ToggleUnits }),
                    seg("°F", !is_c, if !is_c { Msg::Noop } else { Msg::ToggleUnits })
                ],
                sub_fg,
                track_bg,
            ),
            prow("Wind", wind_c, sub_fg, track_bg),
            prow("Pressure", prs_c, sub_fg, track_bg),
            prow("Precipitation", prc_c, sub_fg, track_bg),
            prow("Appearance", th_c, sub_fg, track_bg),
            prow(
                "Clock & Sky",
                vec![
                    seg("24-Hour", s.use_24h_time, Msg::Pref(Pref::Use24h(!s.use_24h_time))),
                    seg("Sky Motion", s.animate_weather, Msg::Pref(Pref::Animate(!s.animate_weather)))
                ],
                sub_fg,
                track_bg,
            ),
            prow(
                "Forecast Map",
                vec![
                    seg("Smooth", s.interpolate_forecast, Msg::Pref(Pref::InterpolateForecast(true))),
                    seg("Model Frames", !s.interpolate_forecast, Msg::Pref(Pref::InterpolateForecast(false)))
                ],
                sub_fg,
                track_bg,
            ),
            hairline(div_col),
            txt("FORECAST PROVIDER", 10.0, Weight::Bold, if lt { rgba8(46, 62, 86, 0.68) } else { rgba8(200, 214, 236, 0.65) }),
            column(prv_c).spacing(5),
        ]
        .spacing(10)
        .width(396);
        let modal = container(body).padding(18).style(hud_pill(sheet_bg, sheet_bdr, 16.0));
        let bg = container(mouse_area(modal).on_press(Msg::Noop))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .style(flat_box(rgba8(0, 0, 0, if lt { 0.28 } else { 0.44 }), 0.0));
        mouse_area(bg).on_press(Msg::TogglePreferences).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::WeatherOverlay;
    use iced_test::simulator::Simulator;
    use std::path::PathBuf;

    fn reset_dir(key: &str) -> Option<PathBuf> {
        let d = std::env::var_os(key).map(PathBuf::from)?;
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        Some(d)
    }
    fn pump(app: &mut WeatherApp, n: usize, ms: u64, stop: bool) {
        for _ in 0..n {
            std::thread::sleep(Duration::from_millis(ms));
            let _ = app.update(Msg::Tick);
            if stop && app.active_bundle().is_some() && !app.map.has_pending_work(app.ui.map_expanded) {
                break;
            }
        }
    }
    fn mk_loc(n: &str, la: f64, lo: f64, tz: &str, cur: bool) -> Location {
        let mut l = Location::new(n, "", "", "", la, lo, tz);
        if cur {
            (l.id, l.is_current_location) = ("current_location".into(), true);
        }
        l
    }

    struct Harness {
        r: iced::Renderer,
        cache: iced_test::runtime::user_interface::Cache,
        logical: Size,
        phys: Size<u32>,
        scale: f32,
        cur: iced::mouse::Cursor,
        dir: Option<PathBuf>,
    }
    impl Harness {
        fn new(logical: Size, scale: f32, cur: iced::Point, env: &str) -> Self {
            use iced_test::core::renderer::Headless;
            let bk = std::env::var("ICED_TEST_BACKEND").ok();
            let r = iced_test::futures::futures::executor::block_on(<iced::Renderer as Headless>::new(
                fw(Weight::Normal),
                iced::Pixels(16.0),
                bk.as_deref(),
            ))
            .expect("headless");
            Self {
                r,
                cache: Default::default(),
                logical,
                phys: Size::new((logical.width * scale) as u32, (logical.height * scale) as u32),
                scale,
                cur: iced::mouse::Cursor::Available(cur),
                dir: reset_dir(env),
            }
        }
        fn draw_only(&mut self, app: &mut WeatherApp, evs: &[iced::Event]) {
            use iced_test::core::{clipboard::Null, theme::Base as _};
            use iced_test::runtime::UserInterface;
            if !evs.is_empty() {
                let mut msgs = Vec::new();
                let mut ui = UserInterface::build(app.view(), self.logical, std::mem::take(&mut self.cache), &mut self.r);
                let _ = ui.update(evs, self.cur, &mut self.r, &mut Null, &mut msgs);
                self.cache = ui.into_cache();
                for m in msgs {
                    let _ = app.update(m);
                }
            }
            let (th, base) = (app.theme(), app.theme().base());
            let mut ui = UserInterface::build(app.view(), self.logical, std::mem::take(&mut self.cache), &mut self.r);
            let _ = ui.update(
                &[iced::Event::Window(window::Event::RedrawRequested(Instant::now()))],
                self.cur,
                &mut self.r,
                &mut Null,
                &mut Vec::new(),
            );
            ui.draw(&mut self.r, &th, &iced_test::core::renderer::Style { text_color: base.text_color }, self.cur);
            self.cache = ui.into_cache();
        }
        fn render(&mut self, app: &mut WeatherApp, evs: &[iced::Event], name: Option<&str>) -> Vec<u8> {
            use iced_test::core::{renderer::Headless, theme::Base as _};
            self.draw_only(app, evs);
            let base = app.theme().base();
            let rgba = self.r.screenshot(self.phys, self.scale, base.background_color);
            if let (Some(d), Some(n)) = (&self.dir, name)
                && let Some(img) = ::image::RgbaImage::from_raw(self.phys.width, self.phys.height, rgba.clone())
            {
                let _ = img.save(d.join(format!("{n}.png")));
            }
            rgba
        }
        fn px(&self, rgba: &[u8], lx: u32, ly: u32) -> (u8, u8, u8) {
            let s = self.scale as u32;
            let o = ((((ly * s).min(self.phys.height - 1)) * self.phys.width + (lx * s).min(self.phys.width - 1)) * 4) as usize;
            (rgba[o], rgba[o + 1], rgba[o + 2])
        }
        fn pump_render(&mut self, app: &mut WeatherApp, n: usize, ms: u64) {
            if self.dir.is_some() {
                for _ in 0..n {
                    std::thread::sleep(Duration::from_millis(ms));
                    let _ = app.update(Msg::Tick);
                    let _ = self.render(app, &[], None);
                }
            }
        }
    }

    #[test]
    fn simulator_renders_bento_and_split_map_views() {
        ensure_fonts_loaded();
        let (mut app, dir) = (WeatherApp::boot().0, reset_dir("WEATHER_RS_TEST_SHOTS"));
        pump(&mut app, if dir.is_some() { 35 } else { 8 }, 120, true);
        let (st, sz_m, sz_b) =
            (iced::Settings { default_font: fw(Weight::Normal), ..Default::default() }, Size::new(1360.0, 860.0), Size::new(1360.0, 980.0));
        let snap = |a: &WeatherApp, sz: Size, th: &iced::Theme, name: &str| {
            let s = Simulator::with_size(st.clone(), sz, a.view()).snapshot(th).expect(name);
            if let Some(d) = &dir {
                let _ = s.matches_image(d.join(name));
            }
        };
        if dir.is_some() {
            snap(&app, sz_m, &iced::Theme::Dark, "warm");
            pump(&mut app, 18, 100, true);
        }
        for (n, m) in
            [("01_bento_dynamic", ThemeMode::DynamicSky), ("02_bento_dark", ThemeMode::Dark), ("03_bento_light", ThemeMode::Light)]
        {
            let _ = app.update(Msg::Pref(Pref::Theme(m)));
            if dir.is_some() && m == ThemeMode::Light {
                pump(&mut app, 6, 40, false);
            }
            snap(&app, sz_b, if m == ThemeMode::Light { &iced::Theme::Light } else { &iced::Theme::Dark }, n);
        }
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::DynamicSky)));
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        if dir.is_some() {
            snap(&app, sz_m, &iced::Theme::Dark, "warm_map");
            pump(&mut app, 25, 120, true);
        }
        for ov in WeatherOverlay::ALL {
            let _ = app.update(Msg::Map(MapMsg::SelectOverlay(ov)));
            if dir.is_some() {
                pump(&mut app, 8, 100, false);
            }
            snap(&app, sz_m, &iced::Theme::Dark, &format!("04_map_{ov:?}").to_lowercase());
        }
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(WeatherOverlay::RadarTimeline)));
        if dir.is_some() {
            // Also verify LibreWXR Nowcast (+30m) and NOAA HRRR Forecast (+2h) render cleanly without any blue NWP blob.
            let _ = app.update(Msg::Map(MapMsg::StepTimeline(3)));
            pump(&mut app, 12, 100, false);
            snap(&app, sz_m, &iced::Theme::Dark, "04b_map_nowcast_30m");
            let _ = app.update(Msg::Map(MapMsg::StepTimeline(4)));
            pump(&mut app, 14, 100, false);
            snap(&app, sz_m, &iced::Theme::Dark, "04c_map_hrrr_forecast");
            let _ = app.update(Msg::Map(MapMsg::StepTimeline(-7)));
            pump(&mut app, 6, 80, false);
        }
        let _ = app.update(Msg::Map(MapMsg::ToggleLayerMenu));
        assert!(app.map.show_layer_menu());
        snap(&app, sz_m, &iced::Theme::Dark, "05_map_layer_popover");
        let _ = app.update(Msg::Escape);
        assert!(!app.map.show_layer_menu() && app.ui.map_expanded);
        for step in 0..4 {
            let _ = app.update(Msg::Map(MapMsg::PanPixels(28.0 + step as f32 * 8.0, -18.0)));
            snap(&app, sz_m, &iced::Theme::Dark, &format!("06_pan_frame_{step}"));
        }
        let _ = app.update(Msg::Map(MapMsg::ToggleFullScreen));
        assert!(app.map.is_full_screen());
        let _ = app.update(Msg::Map(MapMsg::FitAllLocations));
        if dir.is_some() {
            pump(&mut app, 18, 80, false);
        }
        snap(&app, sz_m, &iced::Theme::Dark, "07_map_fullscreen_overview");
        let _ = app.update(Msg::Map(MapMsg::OpenContextMenu(iced::Point::new(560.0, 340.0), iced::Size::new(1120.0, 780.0))));
        assert!(app.map.has_context_menu());
        snap(&app, sz_m, &iced::Theme::Dark, "08_map_context_menu");
        let _ = app.update(Msg::Escape);
        assert!(!app.map.has_context_menu() && app.map.is_full_screen());
        let _ = app.update(Msg::Escape);
        assert!(!app.map.is_full_screen() && app.ui.map_expanded);
        if dir.is_some() {
            // Clean Light map basemap & light HUD verification
            let _ = app.update(Msg::Map(MapMsg::SelectBaseStyle(crate::map::BaseMapStyle::CleanLight)));
            pump(&mut app, 14, 90, false);
            snap(&app, sz_m, &iced::Theme::Light, "16_map_clean_light");
            let _ = app.update(Msg::Map(MapMsg::SelectBaseStyle(crate::map::BaseMapStyle::AppleDark)));
        }
        let _ = app.update(Msg::Escape);
        assert!(!app.ui.map_expanded);
        if dir.is_some() {
            // Full Bento bottom cards (Moon, Humidity, Visibility, Pressure, 10-Day Avg) + Expanded 10-Day + Modals + Narrow!
            snap(&app, Size::new(1360.0, 1280.0), &iced::Theme::Dark, "09_bento_full_tall");
            let _ = app.update(Msg::ToggleDayDetails(1));
            snap(&app, Size::new(1360.0, 1180.0), &iced::Theme::Dark, "10_bento_day_expanded");
            let _ = app.update(Msg::ToggleDayDetails(1));
            let _ = app.update(Msg::OpenDetail(DetailMetric::Temperature));
            snap(&app, sz_m, &iced::Theme::Dark, "11_detail_temp_dark");
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Light)));
            let _ = app.update(Msg::OpenDetail(DetailMetric::Precipitation));
            snap(&app, sz_m, &iced::Theme::Light, "12_detail_precip_light");
            let _ = app.update(Msg::CloseDetail);
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
            let _ = app.update(Msg::TogglePreferences);
            snap(&app, sz_m, &iced::Theme::Dark, "13_preferences_dark");
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Light)));
            snap(&app, sz_m, &iced::Theme::Light, "14_preferences_light");
            let _ = app.update(Msg::TogglePreferences);
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::DynamicSky)));
            let _ = app.update(Msg::WindowResized(Size::new(760.0, 1020.0)));
            snap(&app, Size::new(760.0, 1020.0), &iced::Theme::Dark, "15_bento_narrow");
        }
    }

    #[test]
    fn persistent_renderer_zoom_in_out_repro() {
        ensure_fonts_loaded();
        let (mut h, mut app) =
            (Harness::new(Size::new(1360.0, 900.0), 2.0, iced::Point::new(980.0, 460.0), "WEATHER_RS_ZOOM_REPRO"), WeatherApp::boot().0);
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
        pump(&mut app, if h.dir.is_some() { 30 } else { 12 }, 100, true);
        let no_bleed = |h: &Harness, rgba: &[u8], st: &str| {
            for (pt, lbl) in [((576, 350), "left"), ((920, 12), "top")] {
                let (r, g, b) = h.px(rgba, pt.0, pt.1);
                assert!(r < 80 && g < 100 && b < 135, "{st}: {lbl} bleed ({r},{g},{b})");
            }
        };
        let _ = h.render(&mut app, &[], None);
        pump(&mut app, 10, 80, false);
        let img = h.render(&mut app, &[], Some("01_bento_ready"));
        let (br, bg, bb) = h.px(&img, 1335, 575);
        assert!(br < 75 && bg < 95 && bb < 130, "Bento bleed ({br},{bg},{bb})");
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = h.render(&mut app, &[], None);
        pump(&mut app, 12, 80, false);
        let img = h.render(&mut app, &[], Some("02_split_map_z5"));
        let (tr, tg, tb) = h.px(&img, 980, 12);
        assert!(tr < 75 && tg < 95 && tb < 130, "Split bleed ({tr},{tg},{tb})");
        for (pre, n, dy) in [("03_zoom_in", 6_usize, 3.5_f32), ("05_zoom_out", 7, -4.0)] {
            let ev = iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: dy } });
            for i in 1..=n {
                let img = h.render(&mut app, std::slice::from_ref(&ev), Some(&format!("{pre}_step{i}")));
                no_bleed(&h, &img, pre);
            }
            h.pump_render(&mut app, 16, 90);
            let img = h.render(&mut app, &[], Some(if dy > 0.0 { "04_zoom_in_settled" } else { "06_zoom_out_settled" }));
            no_bleed(&h, &img, pre);
        }
        let _ = app.update(Msg::Map(MapMsg::RecenterActive));
        for _ in 0..16 {
            let _ = app.update(Msg::Tick);
        }
        // The live radar manifest normally arrives during the pumps above; when the network is slow, fall back to the
        // synthetic manifest so this rendering test never depends on upstream latency.
        if app.map.debug_manifest().len() < app.map.live_frame_idx() + 8 {
            app.map.inject_test_manifest(chrono::Utc::now().timestamp());
        }
        let live_i = app.map.live_frame_idx();
        let _ = app.update(Msg::Map(MapMsg::ScrubRadar(live_i + 7)));
        assert!(app.map.selected_forecast_timestamp().is_some());
        h.pump_render(&mut app, 14, 85);
        let img = h.render(&mut app, &[], Some("07_future_radar_forecast"));
        no_bleed(&h, &img, "07_future_radar_forecast");
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(WeatherOverlay::Temperature)));
        let _ = app.update(Msg::Map(MapMsg::ScrubRadar(live_i + 10)));
        h.pump_render(&mut app, 12, 85);
        let img = h.render(&mut app, &[], Some("08_future_temp_forecast"));
        no_bleed(&h, &img, "08_future_temp_forecast");
        let _ = app.update(Msg::Map(MapMsg::JumpToLive));
        assert!(app.map.selected_forecast_timestamp().is_none());
    }

    #[test]
    fn gaborone_berkeley_switch_repro() {
        ensure_fonts_loaded();
        let (mut h, mut app) =
            (Harness::new(Size::new(1340.0, 860.0), 1.0, iced::Point::new(500.0, 400.0), "WEATHER_RS_GB_REPRO"), WeatherApp::boot().0);
        for loc in [
            mk_loc("Gaborone", -24.653, 25.905, "Africa/Gaborone", false),
            mk_loc("Berkeley", 37.872, -122.273, "America/Los_Angeles", false),
        ] {
            if !app.settings.saved_locations.iter().any(|l| l.name == loc.name) {
                app.settings.saved_locations.push(loc);
            }
        }
        let find_id = |a: &WeatherApp, n: &str| a.settings.saved_locations.iter().find(|l| l.name == n).unwrap().id.clone();
        let (gab_id, brk_id, phys) = (find_id(&app, "Gaborone"), find_id(&app, "Berkeley"), h.phys);
        let count_badge = |rgba: &[u8], y1: u32| -> usize {
            (400..y1.min(phys.height))
                .flat_map(|y| (900..1250_u32.min(phys.width)).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let i = ((y * phys.width + x) * 4) as usize;
                    let (r, g, b) = (rgba[i], rgba[i + 1], rgba[i + 2]);
                    r.max(g).max(b) > 135 && r.max(g).max(b) - r.min(g).min(b) > 65
                })
                .count()
        };
        let _ = app.update(Msg::SelectLocation(gab_id.clone()));
        (app.expanded_alert_idx, app.selected_day_idx, app.ui.last_error) = (Some(0), Some(2), Some("stale".into()));
        pump(&mut app, 15, 60, false);
        let gab_img = h.render(&mut app, &[], Some("01_bento_gaborone"));
        if app.bundles.contains_key(&gab_id) {
            assert!(count_badge(&gab_img, 720) >= 80, "expected city badge on Gaborone mini-map");
        }
        let _ = app.update(Msg::SelectLocation(brk_id.clone()));
        assert_eq!((app.expanded_alert_idx, app.selected_day_idx, app.ui.last_error.as_ref()), (None, None, None));
        let _ = h.render(&mut app, &[], Some("02_bento_berkeley_immediate"));
        pump(&mut app, 15, 60, false);
        let brk_img = h.render(&mut app, &[], Some("03_bento_berkeley_settled"));
        if app.bundles.contains_key(&brk_id) {
            assert!(count_badge(&brk_img, 820) >= 80, "expected city badge on Berkeley mini-map");
        }
        let _ = app.update(Msg::SelectLocation(gab_id.clone()));
        pump(&mut app, 15, 60, false);
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        pump(&mut app, 15, 60, false);
        for (id, exp_lat, tag) in [(&brk_id, 37.872, "berkeley"), (&gab_id, -24.653, "gaborone")] {
            let _ = app.update(Msg::SelectLocation(id.clone()));
            assert!((app.map.center_lat - exp_lat).abs() < 0.25 && !app.map.is_flying(), "{tag}: lat={}", app.map.center_lat);
            let _ = h.render(&mut app, &[], Some(&format!("split_{tag}_t0")));
            for step in 1..=6 {
                std::thread::sleep(Duration::from_millis(80));
                let _ = app.update(Msg::Tick);
                let _ = h.render(&mut app, &[], Some(&format!("split_{tag}_step{step}")));
            }
        }
    }

    #[test]
    fn app_state_transitions_and_cleanup() {
        ensure_fonts_loaded();
        let mut app = WeatherApp::boot().0;
        let tokyo = mk_loc("Tokyo", 35.676, 139.650, "Asia/Tokyo", false);
        let tid = tokyo.id.clone();
        let _ = app.add_or_select_location(tokyo);
        assert_eq!(app.settings.selected_location_id.as_deref(), Some(tid.as_str()));
        (app.expanded_alert_idx, app.selected_day_idx, app.ui.last_error) = (Some(0), Some(3), Some("err".into()));
        let (first_id, first_lat) = (app.settings.saved_locations[0].id.clone(), app.settings.saved_locations[0].lat);
        let _ = app.update(Msg::RemoveLocation(tid));
        assert_eq!(
            (app.settings.selected_location_id.as_deref(), app.expanded_alert_idx, app.selected_day_idx, app.ui.last_error.as_ref()),
            (Some(first_id.as_str()), None, None, None)
        );
        assert!((app.map.center_lat - first_lat).abs() < 0.25);
        // Opening the precipitation map starts the loop by itself; the Play / Pause control toggles it.
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        assert!(app.ui.map_expanded && app.map.radar_playing());
        let _ = app.update(Msg::Map(MapMsg::ToggleRadarPlay));
        assert!(!app.map.radar_playing());
        let _ = app.update(Msg::Map(MapMsg::ToggleRadarPlay));
        assert!(app.map.radar_playing());
        let _ = app.update(Msg::Escape);
        assert!(!app.ui.map_expanded && !app.map.radar_playing());
        let _ = app.update(Msg::SearchChanged("London".into()));
        let _ = app.update(Msg::TogglePreferences);
        assert!(app.ui.show_preferences && app.ui.search_query.is_empty());
        let (cur_sf, saved_brk) = (
            mk_loc("San Francisco", 37.775, -122.419, "America/Los_Angeles", true),
            mk_loc("Berkeley", 37.872, -122.273, "America/Los_Angeles", false),
        );
        let brk_id = saved_brk.id.clone();
        app.settings.saved_locations = vec![cur_sf, saved_brk];
        let _ = app.select_location("current_location".into(), true);
        let cur_brk = mk_loc("Berkeley", 37.892, -122.264, "America/Los_Angeles", true);
        app.apply_detected_location(cur_brk.clone(), false);
        assert!(
            app.settings.saved_locations[0].name == "Berkeley"
                && app.settings.saved_locations[0].is_current_location
                && app.settings.saved_locations.iter().any(|l| l.id == brk_id)
                && (app.map.center_lat - 37.892).abs() < 0.05
        );
        (app.selected_day_idx, app.map.center_lat) = (Some(2), 10.0);
        app.apply_detected_location(cur_brk.clone(), false);
        assert!(app.selected_day_idx == Some(2) && (app.map.center_lat - 10.0).abs() < 0.05);
        app.ui.detect_focus_pending = true;
        app.apply_detected_location(cur_brk.clone(), false);
        assert!((app.map.center_lat - 37.892).abs() < 0.05 && app.selected_day_idx.is_none());
        app.apply_detected_location(mk_loc("Berkeley", 37.8781, -122.2668, "", true), true);
        assert!(
            (app.settings.saved_locations[0].lat - 37.8781).abs() < 0.0005
                && app.settings.saved_locations[0].timezone == "America/Los_Angeles"
        );
        app.apply_detected_location(cur_brk, false);
        assert!((app.settings.saved_locations[0].lat - 37.8781).abs() < 0.0005);
    }

    #[test]
    fn map_drag_and_ui_interaction_audit() {
        ensure_fonts_loaded();
        let mut h = Harness::new(Size::new(1340.0, 860.0), 1.0, iced::Point::new(920.0, 420.0), "WEATHER_RS_DRAG_AUDIT");
        let mut app = WeatherApp::boot().0;
        let _ = app.update(Msg::WindowResized(Size::new(1340.0, 860.0)));
        // The drag regions below assume the split layout WITH its left Bento column, which needs a bundle for the selected
        // location. Use a fixed test bundle unless one is loaded, instead of relying on a forecast that an earlier run of
        // the app happened to leave in the disk cache (absent on a clean machine, discarded when its schema is outdated).
        if let Some(id) = app.settings.selected_location_id.clone() {
            app.bundles.entry(id).or_insert_with(|| crate::model::test_bundle(chrono::Utc::now().timestamp()));
        }
        // Load a real manifest so the timeline scrubber + HUD are fully active!
        app.map.inject_test_manifest(chrono::Utc::now().timestamp());
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = h.render(&mut app, &[], None);
        // 1. Test dragging from 6 different regions on the expanded map WITH timeline HUD active.
        for (label, sx, sy, dx, dy) in [
            ("center", 920.0_f32, 420.0_f32, 120.0_f32, 80.0_f32),
            ("top_center", 920.0, 120.0, 80.0, 40.0),
            ("bottom_left_of_scrubber", 620.0, 790.0, 80.0, -40.0),
            ("bottom_right_of_scrubber", 1260.0, 790.0, -80.0, -40.0),
            ("left_below_legend", 650.0, 450.0, 80.0, 40.0),
            ("right_below_controls", 1290.0, 450.0, -80.0, 40.0),
        ] {
            let (lat0, lon0) = (app.map.center_lat, app.map.center_lon);
            h.cur = iced::mouse::Cursor::Available(iced::Point::new(sx, sy));
            let _ = h.render(&mut app, &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left))], None);
            for step in 1..=5 {
                let p = iced::Point::new(sx + dx * step as f32 / 5.0, sy + dy * step as f32 / 5.0);
                h.cur = iced::mouse::Cursor::Available(p);
                let _ = h.render(&mut app, &[iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: p })], None);
            }
            let _ = h.render(&mut app, &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left))], None);
            let moved = (app.map.center_lat - lat0).hypot(app.map.center_lon - lon0);
            assert!(moved > 0.2, "map drag FAILED at {label} ({sx},{sy}): moved only {moved:.6}");
        }
        // 2. Test Window::Unfocused clears in-progress map drag (no stuck drag when releasing outside window).
        let (lat_u, lon_u) = (app.map.center_lat, app.map.center_lon);
        h.cur = iced::mouse::Cursor::Available(iced::Point::new(920.0, 420.0));
        let _ = h.render(&mut app, &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left))], None);
        let _ = h.render(&mut app, &[iced::Event::Window(window::Event::Unfocused)], None);
        let p_after = iced::Point::new(1040.0, 500.0);
        h.cur = iced::mouse::Cursor::Available(p_after);
        let _ = h.render(&mut app, &[iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: p_after })], None);
        assert!((app.map.center_lat - lat_u).hypot(app.map.center_lon - lon_u) < 1e-4, "map remained stuck dragging after Unfocused");

        // 3. Test clicking search box while dropdown is open does NOT clear search_query, while clicking below toolbar DOES.
        let _ = app.update(Msg::SearchChanged("Tokyo".into()));
        app.ui.search_results = vec![mk_loc("Tokyo", 35.676, 139.650, "Asia/Tokyo", false)];
        let _ = h.render(&mut app, &[], None);
        h.cur = iced::mouse::Cursor::Available(iced::Point::new(1240.0, 22.0));
        let _ = h.render(
            &mut app,
            &[
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
            ],
            None,
        );
        assert_eq!(app.ui.search_query, "Tokyo", "search dropdown stole click from search input!");
        h.cur = iced::mouse::Cursor::Available(iced::Point::new(500.0, 300.0));
        let _ = h.render(
            &mut app,
            &[
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)),
            ],
            None,
        );
        assert!(app.ui.search_query.is_empty(), "clicking outside search dropdown failed to dismiss it");

        // 4. Test Cmd+1..9 city selection shortcut + render visual audit shots when requested.
        let nyc = mk_loc("New York", 40.713, -74.006, "America/New_York", false);
        let nyc_id = nyc.id.clone();
        let _ = app.add_or_select_location(nyc);
        let _ = app.update(Msg::SelectIndex(0));
        assert_eq!(app.settings.selected_location_id.as_deref(), Some(app.settings.saved_locations[0].id.as_str()));
        let idx = app.settings.saved_locations.iter().position(|l| l.id == nyc_id).unwrap();
        let _ = app.update(Msg::SelectIndex(idx));
        assert_eq!(app.settings.selected_location_id.as_deref(), Some(nyc_id.as_str()));
        // 5. Test interactive Detail Chart Sheet open/switch/Escape & Precipitation unit preference
        let _ = app.update(Msg::OpenDetail(DetailMetric::Temperature));
        assert_eq!(app.ui.detail_metric, Some(DetailMetric::Temperature));
        let _ = app.update(Msg::OpenDetail(DetailMetric::Wind));
        assert_eq!(app.ui.detail_metric, Some(DetailMetric::Wind));
        let _ = app.update(Msg::Escape);
        assert_eq!(app.ui.detail_metric, None);
        let _ = app.update(Msg::Pref(Pref::Precip(PrecipUnit::Inches)));
        assert_eq!(app.settings.precip_unit, PrecipUnit::Inches);
        let _ = app.update(Msg::Pref(Pref::Precip(PrecipUnit::Mm)));
        assert_eq!(app.settings.precip_unit, PrecipUnit::Mm);

        if h.dir.is_some() {
            let _ = app.update(Msg::Map(MapMsg::CloseMap));
            pump(&mut app, 14, 100, true);
            // Capture scrolled Bento bottom half (UV, Sunset, Feels Like, Precip, Moon, Humidity, Visibility, Pressure, Averages + Sticky Header!)
            let _ = app.update(Msg::SelectIndex(0));
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::DynamicSky)));
            let _ = app.update(Msg::ToggleAlertDetails(0));
            let _ = h.render(&mut app, &[], Some("00a_bento_alert_exp"));
            let _ = app.update(Msg::ToggleAlertDetails(0));
            let _ = app.update(Msg::ToggleDayDetails(1));
            let _ = h.render(&mut app, &[], Some("00c_bento_day_exp"));
            let _ = app.update(Msg::ToggleDayDetails(1));
            // Capture interactive 24-hour Detail Chart Sheet with live cursor hover scrub tooltip!
            let _ = app.update(Msg::OpenDetail(DetailMetric::Temperature));
            let scrub_pt = iced::Point::new(690.0, 445.0);
            h.cur = iced::mouse::Cursor::Available(scrub_pt);
            let _ = h.render(
                &mut app,
                &[iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: scrub_pt })],
                Some("05_detail_temp_scrub"),
            );
            let _ = app.update(Msg::OpenDetail(DetailMetric::Precipitation));
            let _ =
                h.render(&mut app, &[iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: scrub_pt })], Some("06_detail_precip"));
            let _ = app.update(Msg::CloseDetail);
            // Capture Split Map with 1h/12h player range toggle & crisp narrow Wind Compass!
            let _ = app.update(Msg::Map(MapMsg::ExpandMap));
            let _ = app.update(Msg::Map(MapMsg::TogglePrecipRange));
            let _ = h.render(&mut app, &[], Some("07_split_map_1h"));
            let _ = app.update(Msg::Map(MapMsg::TogglePrecipRange));
            let _ = app.update(Msg::Map(MapMsg::CloseMap));
            h.cur = iced::mouse::Cursor::Available(iced::Point::new(720.0, 440.0));
            for _ in 0..8 {
                let _ = h.render(
                    &mut app,
                    &[iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                        delta: iced::mouse::ScrollDelta::Pixels { x: 0.0, y: -90.0 },
                    })],
                    None,
                );
            }
            let _ = h.render(&mut app, &[], Some("00b_bento_scrolled_bottom"));
            let _ = app.update(Msg::SelectIndex(idx));
            let _ = app.update(Msg::TogglePreferences);
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
            let _ = h.render(&mut app, &[], Some("01_settings_dark"));
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Light)));
            let _ = h.render(&mut app, &[], Some("02_settings_light"));
            let _ = app.update(Msg::TogglePreferences);
            let _ = app.update(Msg::SearchChanged("Tokyo".into()));
            app.ui.search_results = vec![mk_loc("Tokyo", 35.676, 139.650, "Asia/Tokyo", false)];
            let _ = h.render(&mut app, &[], Some("03_search_light"));
            let _ = app.update(Msg::ClearSearch);
            let mut hn = Harness::new(Size::new(760.0, 860.0), 1.0, iced::Point::new(380.0, 380.0), "WEATHER_RS_DRAG_AUDIT_NARROW");
            let _ = app.update(Msg::SelectIndex(0));
            let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::DynamicSky)));
            let _ = app.update(Msg::WindowResized(Size::new(760.0, 860.0)));
            let _ = hn.render(&mut app, &[], Some("04_narrow_760"));
        }
    }

    #[test]
    #[ignore]
    fn perf_benchmark() {
        use crate::model::default_locations;
        use crate::providers::{reset_net_metrics, snapshot_net_metrics, snapshot_wire_bytes};
        println!("\n================ WEATHER-RS PERF BENCHMARK ================");
        // 1. Cold startup CPU init costs
        let t0 = Instant::now();
        let sfnt_bytes = crate::style::bench_woff_decode_all();
        let woff_us = t0.elapsed().as_micros();
        let t0 = Instant::now();
        crate::icons::bench_cloud_tex();
        let cloud_us = t0.elapsed().as_micros();
        ensure_fonts_loaded();
        println!("STARTUP / INIT:");
        println!("  woff_decode_fonts_us:      {woff_us:>7} us ({sfnt_bytes} B SFNT)");
        println!("  cloud_tex_gen_us:          {cloud_us:>7} us");

        // 2. Network latency & bytes transferred (live upstream!)
        let c = WeatherClient::new();
        let locs = default_locations();
        reset_net_metrics();
        let t0 = Instant::now();
        let mut first_paint_ms = None;
        let b = c
            .fetch_weather_streaming(&locs[0], ProviderChoice::Auto, |_| {
                if first_paint_ms.is_none() {
                    first_paint_ms = Some(t0.elapsed().as_millis());
                }
            })
            .expect("fetch_weather");
        let wx_cold_ms = t0.elapsed().as_millis();
        let wx_fp_ms = first_paint_ms.unwrap_or(wx_cold_ms);
        let (r1, b1, jr1, jb1, _, _) = snapshot_net_metrics();
        let w1 = snapshot_wire_bytes();

        reset_net_metrics();
        let t0 = Instant::now();
        let _ = c.fetch_weather(&locs[0], ProviderChoice::Auto).expect("fetch_weather warm");
        let wx_warm_ms = t0.elapsed().as_millis();
        let (r2, b2, _, _, _, _) = snapshot_net_metrics();
        let w2 = snapshot_wire_bytes();

        reset_net_metrics();
        let t0 = Instant::now();
        let m = c.fetch_rainviewer_manifest().expect("manifest");
        let manifest_ms = t0.elapsed().as_millis();
        let (rm, bm, _, _, _, _) = snapshot_net_metrics();

        let grid_coords: Vec<(f64, f64)> =
            (0..4).flat_map(|r| (0..5).map(move |col| (34.0 + r as f64 * 4.2, -132.0 + col as f64 * 5.0))).collect();
        reset_net_metrics();
        let t0 = Instant::now();
        let gpts = c.fetch_grid_batch(&grid_coords);
        let grid_ms = t0.elapsed().as_millis();
        let (rg, bg, _, _, _, _) = snapshot_net_metrics();
        let wg = snapshot_wire_bytes();

        reset_net_metrics();
        let t0 = Instant::now();
        let sb = c.fetch_sidebar_batch(&locs[1..], ProviderChoice::Auto);
        let sb_ms = t0.elapsed().as_millis();
        let (rsb, bsb, _, _, _, _) = snapshot_net_metrics();
        let wsb = snapshot_wire_bytes();

        println!("NETWORK (LIVE):");
        println!(
            "  weather_cold:              {wx_cold_ms:>5} ms (first_paint {wx_fp_ms} ms) | {r1:>2} reqs | {b1:>7} B decoded / {w1:>6} B wire (json {jr1}, {jb1} B)"
        );
        println!("  weather_warm_cached:       {wx_warm_ms:>5} ms | {r2:>2} reqs | {b2:>7} B decoded / {w2:>6} B wire");
        println!("  sidebar_batch_3cities:     {sb_ms:>5} ms | {rsb:>2} reqs | {bsb:>7} B decoded / {wsb:>6} B wire ({} cities)", sb.len());
        println!("  radar_manifest:            {manifest_ms:>5} ms | {rm:>2} reqs | {bm:>7} B ({} frames)", m.frames.len());
        println!("  grid_batch_20pts:          {grid_ms:>5} ms | {rg:>2} reqs | {bg:>7} B decoded / {wg:>6} B wire ({} pts)", gpts.len());

        // 3. Map prewarm & CPU compute internals
        let mut bundles = HashMap::new();
        bundles.insert(locs[0].id.clone(), b);
        let mut eng = MapEngine::new(c.clone(), locs[0].lat, locs[0].lon, true, Arc::new(Notify::new()));
        let t0 = Instant::now();
        eng.prewarm_saved_locations(&locs, true);
        let boot_prewarm_tiles = eng.in_flight_count();
        let boot_prewarm_us = t0.elapsed().as_micros();
        println!("MAP PREWARM BUDGET (4 saved cities):");
        println!("  boot_prewarm_enqueued:     {boot_prewarm_tiles:>7} tiles ({boot_prewarm_us} us)");

        println!("CPU COMPUTE HOT PATHS:");
        eng.bench_internals(&bundles);

        // 4. Iced view() & full WGPU frame render timings
        reset_net_metrics();
        let (mut app, _) = WeatherApp::boot();
        pump(&mut app, 25, 100, true);
        let (boot_reqs, boot_bytes, _, _, boot_treqs, boot_tbytes) = snapshot_net_metrics();
        let boot_wire = snapshot_wire_bytes();
        println!("BOOT SESSION (warm disk cache):");
        println!(
            "  session_net:               {boot_reqs:>4} reqs | {boot_bytes:>8} B dec / {boot_wire:>7} B wire (tiles: {boot_treqs} reqs, {boot_tbytes} B)"
        );

        let t0 = Instant::now();
        for _ in 0..100 {
            let _ = std::hint::black_box(app.view());
        }
        let view_bento_us = t0.elapsed().as_micros() / 100;

        reset_net_metrics();
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        pump(&mut app, 25, 120, true);
        let (exp_reqs, exp_bytes, _, _, exp_treqs, exp_tbytes) = snapshot_net_metrics();
        let exp_wire = snapshot_wire_bytes();
        println!("EXPAND MAP SESSION:");
        println!(
            "  expand_net:                {exp_reqs:>4} reqs | {exp_bytes:>8} B dec / {exp_wire:>7} B wire (tiles: {exp_treqs} reqs, {exp_tbytes} B)"
        );
        let t0 = Instant::now();
        for _ in 0..100 {
            let _ = std::hint::black_box(app.view());
        }
        let view_map_us = t0.elapsed().as_micros() / 100;

        let mut h = Harness::new(Size::new(1360.0, 860.0), 1.0, iced::Point::new(820.0, 440.0), "");
        let _ = h.render(&mut app, &[], None); // warm GPU pipeline & tile/bento caches
        let t0 = Instant::now();
        for i in 0..40 {
            let ev = iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: iced::Point::new(720.0 + i as f32 * 4.0, 440.0) });
            h.draw_only(&mut app, &[ev]);
        }
        let map_hover_draw_us = t0.elapsed().as_micros() / 40;
        let t0 = Instant::now();
        for i in 0..15 {
            let ev = iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: iced::Point::new(720.0 + i as f32 * 8.0, 440.0) });
            let _ = h.render(&mut app, &[ev], None);
        }
        let map_hover_frame_us = t0.elapsed().as_micros() / 15;

        let t0 = Instant::now();
        for _ in 0..40 {
            let _ = app.update(Msg::Map(MapMsg::PanPixels(4.0, -2.0)));
            h.draw_only(&mut app, &[]);
        }
        let map_pan_draw_us = t0.elapsed().as_micros() / 40;
        let t0 = Instant::now();
        for _ in 0..15 {
            let _ = app.update(Msg::Map(MapMsg::PanPixels(6.0, -3.0)));
            let _ = h.render(&mut app, &[], None);
        }
        let map_pan_frame_us = t0.elapsed().as_micros() / 15;

        // Live 60fps radar timeline playback frame timing (tests 3-layer base/weather/label GPU cache + split Scrubber cache!)
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(crate::map::WeatherOverlay::RadarTimeline)));
        let _ = h.render(&mut app, &[], None);
        let t0 = Instant::now();
        for i in 0..40 {
            let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos((i as f32) * 0.08)));
            h.draw_only(&mut app, &[]);
        }
        let radar_play_draw_us = t0.elapsed().as_micros() / 40;
        let t0 = Instant::now();
        for i in 0..15 {
            let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos((i as f32) * 0.14)));
            let _ = h.render(&mut app, &[], None);
        }
        let radar_play_frame_us = t0.elapsed().as_micros() / 15;

        // Wind Flow live streamline playback frame timing (tests single-pass parabolic fade!)
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(crate::map::WeatherOverlay::WindFlow)));
        let _ = h.render(&mut app, &[], None);
        let t0 = Instant::now();
        for _ in 0..40 {
            let _ = app.update(Msg::Tick);
            h.draw_only(&mut app, &[]);
        }
        let wind_play_draw_us = t0.elapsed().as_micros() / 40;

        let _ = app.update(Msg::Map(MapMsg::CloseMap));
        let _ = h.render(&mut app, &[], None);
        let t0 = Instant::now();
        for _ in 0..60 {
            h.draw_only(&mut app, &[]);
        }
        let bento_draw_us = t0.elapsed().as_micros() / 60;
        let t0 = Instant::now();
        for _ in 0..15 {
            let _ = h.render(&mut app, &[], None);
        }
        let bento_frame_us = t0.elapsed().as_micros() / 15;

        // Detail Chart Sheet live cursor scrub timing (tests GPU-cached DetailChartCanvas!)
        let _ = app.update(Msg::OpenDetail(DetailMetric::Temperature));
        let _ = h.render(&mut app, &[], None);
        let t0 = Instant::now();
        for i in 0..40 {
            let ev = iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: iced::Point::new(420.0 + i as f32 * 10.0, 445.0) });
            h.draw_only(&mut app, &[ev]);
        }
        let detail_scrub_draw_us = t0.elapsed().as_micros() / 40;
        let _ = app.update(Msg::CloseDetail);

        println!("UI & RENDER FRAMES:");
        println!("  view_bento_us:             {view_bento_us:>7} us");
        println!("  view_map_us:               {view_map_us:>7} us");
        println!("  bento_cpu_draw_us:         {bento_draw_us:>7} us");
        println!("  detail_scrub_cpu_draw_us:  {detail_scrub_draw_us:>7} us");
        println!("  map_hover_cpu_draw_us:     {map_hover_draw_us:>7} us");
        println!("  radar_play_cpu_draw_us:    {radar_play_draw_us:>7} us");
        println!("  wind_play_cpu_draw_us:     {wind_play_draw_us:>7} us");
        println!("  map_pan_cpu_draw_us:       {map_pan_draw_us:>7} us");
        println!("  bento_full_frame_us:       {bento_frame_us:>7} us");
        println!("  map_hover_cached_frame_us: {map_hover_frame_us:>7} us");
        println!("  radar_play_full_frame_us:  {radar_play_frame_us:>7} us");
        println!("  map_pan_full_frame_us:     {map_pan_frame_us:>7} us");
        println!("===========================================================\n");
    }

    #[test]
    #[ignore = "visual audit: run with WEATHER_RS_PRECIP_STRIP=/tmp/precip_strip cargo test --release audit_precip_playback_strip -- --ignored --nocapture"]
    fn audit_precip_playback_strip() {
        ensure_fonts_loaded();
        let mut h = Harness::new(Size::new(1360.0, 860.0), 1.0, iced::Point::new(10.0, 10.0), "WEATHER_RS_PRECIP_STRIP");
        let mut app = WeatherApp::boot().0;
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
        pump(&mut app, 25, 100, true);
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(crate::map::WeatherOverlay::RadarTimeline)));
        let _ = app.update(Msg::Map(MapMsg::ToggleFullScreen));
        // WEATHER_RS_STRIP_MODEL_FRAMES=1 captures the "Model Frames" setting (no app-interpolated ECCC frames).
        if std::env::var_os("WEATHER_RS_STRIP_MODEL_FRAMES").is_some() {
            let _ = app.update(Msg::Pref(Pref::InterpolateForecast(false)));
        }
        h.pump_render(&mut app, 35, 100);
        let live_i = app.map.live_frame_idx();
        let frames = app.map.debug_manifest();
        let live_ts = frames.get(live_i).map_or(0, |f| f.1);
        println!("MANIFEST live_i={live_i} now={}", chrono::Utc::now().timestamp());
        for (i, t, src) in &frames {
            println!("  frame {i:>2} t={t} off={:+}m {src}", (t - live_ts) / 60);
        }
        let views: Vec<(String, f64, f64, f64)> = std::env::var("WEATHER_RS_STRIP_VIEWS").map_or_else(
            |_| vec![("a".to_string(), 37.87, -122.27, 5.6)],
            |v| {
                v.split(';')
                    .filter_map(|s| {
                        let p: Vec<&str> = s.split(',').collect();
                        Some((p.first()?.to_string(), p.get(1)?.parse().ok()?, p.get(2)?.parse().ok()?, p.get(3)?.parse().ok()?))
                    })
                    .collect()
            },
        );
        let step: f32 = std::env::var("WEATHER_RS_STRIP_STEP").ok().and_then(|s| s.parse().ok()).unwrap_or(0.25);
        let span: usize = std::env::var("WEATHER_RS_STRIP_SPAN").ok().and_then(|s| s.parse().ok()).unwrap_or(7);
        let first_off: usize = std::env::var("WEATHER_RS_STRIP_FIRST").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
        let (first, last) = (live_i + first_off, (live_i + first_off + span).min(frames.len().saturating_sub(1)));
        for (name, lat, lon, zoom) in views {
            (app.map.center_lat, app.map.center_lon, app.map.zoom) = (lat, lon, zoom);
            // Two passes over every integer frame so all tiers / tiles of each frame are fully settled before capturing.
            for _ in 0..2 {
                for idx in first..=last {
                    let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos(idx as f32)));
                    h.pump_render(&mut app, 12, 90);
                }
            }
            let n = ((last - first) as f32 / step).round() as usize;
            for k in 0..=n {
                let pos = first as f32 + k as f32 * step;
                let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos(pos)));
                h.pump_render(&mut app, 3, 60);
                app.map.debug_set_play(pos, true);
                let _ = h.render(&mut app, &[], Some(&format!("{name}_{k:03}_pos{:05.2}", pos - live_i as f32)));
                app.map.debug_set_play(pos, false);
            }
        }
    }

    /// Captures the dashboard and every detail sheet, and prints the upstream values behind them, so what is drawn can
    /// be checked against what was fetched.
    #[test]
    #[ignore = "visual audit: run with WEATHER_RS_DASH_AUDIT=/tmp/dash cargo test --release audit_dashboard_cards -- --ignored --nocapture"]
    fn audit_dashboard_cards() {
        ensure_fonts_loaded();
        let mut h = Harness::new(Size::new(1360.0, 1500.0), 1.0, iced::Point::new(10.0, 10.0), "WEATHER_RS_DASH_AUDIT");
        let mut app = WeatherApp::boot().0;
        let theme = match std::env::var("WEATHER_RS_DASH_THEME").as_deref() {
            Ok("light") => ThemeMode::Light,
            Ok("dark") => ThemeMode::Dark,
            _ => ThemeMode::DynamicSky,
        };
        let _ = app.update(Msg::Pref(Pref::Theme(theme)));
        let _ = app.update(Msg::WindowResized(Size::new(1360.0, 1500.0)));
        pump(&mut app, 40, 120, true);
        // Map tiles are requested by drawing, so draw a few frames before the capture.
        h.pump_render(&mut app, 25, 100);
        let _ = h.render(&mut app, &[], Some("00_dashboard"));
        let _ = app.update(Msg::ToggleDayDetails(1));
        let _ = h.render(&mut app, &[], Some("01_day_expanded"));
        let _ = app.update(Msg::ToggleDayDetails(1));
        for (i, m) in
            [DetailMetric::Temperature, DetailMetric::Precipitation, DetailMetric::Wind, DetailMetric::UvIndex, DetailMetric::Pressure]
                .into_iter()
                .enumerate()
        {
            let _ = app.update(Msg::OpenDetail(m));
            let _ = h.render(&mut app, &[], Some(&format!("1{i}_detail_{m:?}").to_lowercase()));
            let _ = app.update(Msg::CloseDetail);
        }
        if let Some(b) = app.settings.selected_location_id.as_ref().and_then(|id| app.bundles.get(id)) {
            println!("DASH source={} fetched={} lean={}", b.source_name, b.fetched_unix, b.lean);
            println!("DASH current={:?}", b.current);
            println!("DASH air={:?}", b.air_quality);
            for d in b.daily.iter().take(10) {
                println!("DASH day={d:?}");
            }
            for hr in b.hourly.iter().take(26) {
                println!("DASH hour={hr:?}");
            }
            println!("DASH alerts={:?}", b.alerts);
        }
    }

    /// Plays the precipitation timeline in real time, exactly as the user would see it, and saves a frame every
    /// ~50 ms. Prints, per frame, the playhead and whether playback was waiting, so stalls and flicker are measurable.
    #[test]
    #[ignore = "visual audit: run with WEATHER_RS_PLAYBACK=/tmp/playback cargo test --release audit_precip_live_playback -- --ignored --nocapture"]
    fn audit_precip_live_playback() {
        ensure_fonts_loaded();
        // WEATHER_RS_PLAYBACK_SIZE=2560x1440 captures a larger window (more tiles to build per frame).
        let (w, hgt) = std::env::var("WEATHER_RS_PLAYBACK_SIZE")
            .ok()
            .and_then(|v| v.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
            .unwrap_or((1360.0_f32, 860.0_f32));
        let mut h = Harness::new(Size::new(w, hgt), 1.0, iced::Point::new(10.0, 10.0), "WEATHER_RS_PLAYBACK");
        let mut app = WeatherApp::boot().0;
        let light = std::env::var("WEATHER_RS_PLAYBACK_THEME").is_ok_and(|t| t == "light");
        let _ = app.update(Msg::Pref(Pref::Theme(if light { ThemeMode::Light } else { ThemeMode::Dark })));
        pump(&mut app, 25, 100, true);
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(crate::map::WeatherOverlay::RadarTimeline)));
        let _ = app.update(Msg::Map(MapMsg::ToggleFullScreen));
        if let Some((lat, lon, zoom)) = std::env::var("WEATHER_RS_PLAYBACK_VIEW").ok().and_then(|v| {
            let p: Vec<f64> = v.split(',').filter_map(|x| x.parse().ok()).collect();
            (p.len() == 3).then(|| (p[0], p[1], p[2]))
        }) {
            (app.map.center_lat, app.map.center_lon, app.map.zoom) = (lat, lon, zoom);
        }
        if std::env::var_os("WEATHER_RS_PLAYBACK_MODEL_FRAMES").is_some() {
            let _ = app.update(Msg::Pref(Pref::InterpolateForecast(false)));
        }
        // Give the map the same couple of seconds a user spends looking at it before pressing Play.
        let settle_ms: u64 = std::env::var("WEATHER_RS_PLAYBACK_SETTLE_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(2000);
        h.pump_render(&mut app, (settle_ms / 50) as usize, 50);
        let secs: f32 = std::env::var("WEATHER_RS_PLAYBACK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(16.0);
        // WEATHER_RS_PLAYBACK_STILLS=1 first saves paused stills of the live radar frame and the frames after it, to
        // compare how observed radar and the model forecast colour the same rain.
        if std::env::var_os("WEATHER_RS_PLAYBACK_STILLS").is_some() {
            let live = app.map.live_frame_idx();
            for off in [0_usize, 1, 3, 5, 6, 7, 9] {
                app.map.debug_set_play((live + off) as f32, false);
                h.pump_render(&mut app, 30, 80);
                let _ = h.render(&mut app, &[], Some(&format!("still_live+{off:02}")));
            }
            app.map.debug_set_play(live as f32, false);
        }
        // WEATHER_RS_PLAYBACK_2X=1 plays at double speed (twice as many interpolated tiles to build per second).
        if std::env::var_os("WEATHER_RS_PLAYBACK_2X").is_some() {
            let _ = app.update(Msg::Map(MapMsg::CyclePlaybackSpeed));
        }
        // The loop starts by itself when the map opens; press Play only if something paused it.
        if !app.map.radar_playing() {
            let _ = app.update(Msg::Map(MapMsg::ToggleRadarPlay));
        }
        let (t0, mut k, mut waited) = (Instant::now(), 0_u32, 0.0_f32);
        let mut last = t0;
        while t0.elapsed().as_secs_f32() < secs {
            let _ = app.update(Msg::Tick);
            let pos = app.map.debug_play_pos();
            let wait = app.map.radar.buffering;
            let _ = h.render(
                &mut app,
                &[],
                Some(&format!("f{k:04}_t{:05}_pos{pos:06.2}{}", t0.elapsed().as_millis(), if wait { "_WAIT" } else { "" })),
            );
            let dt = last.elapsed().as_secs_f32();
            last = Instant::now();
            if wait {
                waited += dt;
            }
            k += 1;
            let spent = last.elapsed();
            std::thread::sleep(Duration::from_millis(45).saturating_sub(spent));
        }
        println!("PLAYBACK frames={k} waited={waited:.2}s");
    }

    #[test]
    #[ignore = "visual audit: run with WEATHER_RS_PRECIP_AUDIT=/tmp/precip_audit cargo test --release audit_precip_map_visuals -- --ignored --nocapture"]
    fn audit_precip_map_visuals() {
        ensure_fonts_loaded();
        let mut h = Harness::new(Size::new(1360.0, 860.0), 1.0, iced::Point::new(10.0, 10.0), "WEATHER_RS_PRECIP_AUDIT");
        let mut app = WeatherApp::boot().0;
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
        pump(&mut app, 25, 100, true);
        let _ = h.render(&mut app, &[], None);
        for _ in 0..16 {
            std::thread::sleep(Duration::from_millis(80));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("00_minimap_precip"));

        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(crate::map::WeatherOverlay::RadarTimeline)));
        let _ = h.render(&mut app, &[], None);
        // Wait for initial live + full-timeline prewarm
        for _ in 0..35 {
            std::thread::sleep(Duration::from_millis(100));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("01_live_z55"));
        // Capture key frames across the entire timeline, addressed by forecast offset (frame cadence is not uniform).
        let live_i = app.map.live_frame_idx();
        let frames = app.map.debug_manifest();
        let live_ts = frames.get(live_i).map_or(0, |f| f.1);
        let idx_at = |mins: i64| frames.iter().min_by_key(|f| (f.1 - (live_ts + mins * 60)).abs()).map_or(live_i, |f| f.0);
        let fcst_3h = idx_at(180);
        for (idx, name) in [
            (idx_at(30), "02_nowcast_30m"),
            (idx_at(60), "03_fcst_1h"),
            (idx_at(90), "03b_fcst_1h30"),
            (fcst_3h, "04_fcst_3h"),
            (idx_at(360), "05_fcst_6h"),
            (idx_at(720), "06_fcst_12h"),
        ] {
            let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos(idx as f32)));
            for _ in 0..16 {
                std::thread::sleep(Duration::from_millis(80));
                let _ = app.update(Msg::Tick);
                let _ = h.render(&mut app, &[], None);
            }
            let _ = h.render(&mut app, &[], Some(name));
        }
        // Mid-crossfade LibreWXR (+30m) -> HRRR+ECCC (+45m)
        let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos(idx_at(30) as f32 + 0.5)));
        let _ = app.update(Msg::Tick);
        let _ = h.render(&mut app, &[], Some("07_crossfade_lwxr_hrrr"));

        // Instant zoom-in test (capture IMMEDIATELY after zoom before new z=7 tiles arrive!)
        let _ = app.update(Msg::Map(MapMsg::JumpToLive));
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(60));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = app.update(Msg::Map(MapMsg::DragZoom(iced::Point::new(900.0, 430.0), Size::new(820.0, 820.0), 2.0)));
        let _ = app.update(Msg::Tick);
        let _ = h.render(&mut app, &[], Some("08_zoom_in_instant_fallback"));
        for _ in 0..16 {
            std::thread::sleep(Duration::from_millis(80));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("09_zoom_in_settled"));

        // Instant zoom-out test to continental view z~3.5 on +3h HRRR+ECCC (checks ALL seams & antimeridian!)
        let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos(fcst_3h as f32)));
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(80));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = app.update(Msg::Map(MapMsg::DragZoom(iced::Point::new(900.0, 430.0), Size::new(820.0, 820.0), -3.8)));
        let _ = app.update(Msg::Tick);
        let _ = h.render(&mut app, &[], Some("10_zoom_out_instant_fallback"));
        let _ = app.update(Msg::Map(MapMsg::PanPixels(-140.0, 0.0)));
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(100));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("11_continental_z35_hrrr"));

        // Pan to Europe / London on +3h HRRR+ECCC (tests pure global ECCC GDPS outside CONUS!)
        app.map.fly_to(51.5, -0.1, Some(5.0));
        for _ in 0..35 {
            std::thread::sleep(Duration::from_millis(100));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("12_europe_london_hrrr"));

        // Fullscreen + 1h mode back at Berkeley
        app.map.fly_to(37.87, -122.27, Some(5.6));
        let _ = app.update(Msg::Map(MapMsg::ToggleFullScreen));
        let _ = app.update(Msg::Map(MapMsg::TogglePrecipRange));
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(100));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("13_fullscreen_1h_mode"));

        // Light + Satellite basemap styles with +3h precipitation
        let _ = app.update(Msg::Map(MapMsg::TogglePrecipRange));
        let _ = app.update(Msg::Map(MapMsg::ScrubRadarPos(fcst_3h as f32)));
        for (st, name) in [
            (crate::map::BaseMapStyle::CleanLight, "14_light_style_precip"),
            (crate::map::BaseMapStyle::Satellite, "15_satellite_style_precip"),
        ] {
            let _ = app.update(Msg::Map(MapMsg::SelectBaseStyle(st)));
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(80));
                let _ = app.update(Msg::Tick);
                let _ = h.render(&mut app, &[], None);
            }
            let _ = h.render(&mut app, &[], Some(name));
        }

        // Continuous playback loop + mid-play pan stress test on Dark style
        let _ = app.update(Msg::Map(MapMsg::SelectBaseStyle(crate::map::BaseMapStyle::AppleDark)));
        let _ = app.update(Msg::Map(MapMsg::ToggleRadarPlay));
        let mut max_stall = 0.0_f32;
        for step in 0..45 {
            if step == 20 {
                let _ = app.update(Msg::Map(MapMsg::PanPixels(60.0, -35.0)));
            }
            std::thread::sleep(Duration::from_millis(45));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
            max_stall = max_stall.max(app.map.radar.stall_secs);
        }
        assert!(max_stall < 1.2, "unexpected playback stall during live loop + pan: {max_stall:.2}s");
        let _ = h.render(&mut app, &[], Some("16_mid_play_loop"));

        // Global Animated Satellite IR (LibreWXR 512x512, 12h timeline) over Europe + open Layer Menu w/ Opacity
        let _ = app.update(Msg::Map(MapMsg::ToggleRadarPlay));
        (app.map.center_lat, app.map.center_lon, app.map.zoom) = (51.5074, -0.1278, 4.8);
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(crate::map::WeatherOverlay::InfraredClouds)));
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(90));
            let _ = app.update(Msg::Tick);
            let _ = h.render(&mut app, &[], None);
        }
        let _ = h.render(&mut app, &[], Some("17_global_satellite_ir_europe"));
        let _ = app.update(Msg::Map(MapMsg::ToggleLayerMenu));
        let _ = h.render(&mut app, &[], Some("18_layer_menu_opacity"));
    }
}
