use crate::icons::{UiIcon, WidgetCanvas, icon_btn, icon_svg, widget_canvas};
use crate::map::{MapEngine, MapExternalAction, MapMsg};
use crate::model::{
    Location, PrecipUnit, PressureUnit, ProviderChoice, Settings, TempUnit, ThemeMode,
    WeatherBundle, WeatherCondition, WindUnit,
};
use crate::providers::WeatherClient;
use crate::storage::{
    flush_pending_settings, is_shutting_down, load_cached_bundle, load_settings, request_shutdown,
    save_cached_bundle, save_settings, spawn_background_cache_prune,
};
use crate::style::{
    SkyPalette, ensure_fonts_loaded, flat_box, fw, hairline, hud_pill, palette_for, pbtn, rgb8,
    rgba8, sleek_scrollable, sp_h, sp_w, txt,
};
use chrono::Utc;
use iced::font::Weight;
use iced::keyboard::{self, Key};
use iced::widget::{
    column, container, mouse_area, operation, row, scrollable, stack, text, text_input,
};
use iced::window;
use iced::{Alignment, Color, Element, Fill, Size, Subscription, Task};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

fn local_route_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    Some(sock.local_addr().ok()?.ip())
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
    iced::stream::channel(4, async move |mut output| {
        use iced::futures::SinkExt;
        loop {
            notify.notified().await;
            tokio::time::sleep(Duration::from_millis(12)).await;
            if output.send(Msg::Tick).await.is_err() {
                break;
            }
        }
    })
}

#[derive(Debug, Clone)]
pub enum Pref {
    Theme(ThemeMode),
    Wind(WindUnit),
    Pressure(PressureUnit),
    Use24h(bool),
    Animate(bool),
    Provider(ProviderChoice),
}

#[derive(Debug, Clone)]
pub enum Msg {
    Tick,
    ToggleSidebar,
    ToggleUnits,
    TogglePreferences,
    RefreshAll,
    SelectLocation(String),
    RemoveLocation(String),
    DetectLocation,
    SearchChanged(String),
    SubmitSearch,
    FocusSearch,
    ClearSearch,
    PickSearchResult(Location),
    ToggleAlertDetails(usize),
    ToggleDayDetails(usize),
    ToggleMap,
    Pref(Pref),
    Map(MapMsg),
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

pub struct WeatherApp {
    pub settings: Settings,
    pub window_size: Size,
    pub show_sidebar: bool,
    pub map_expanded: bool,
    pub show_preferences: bool,
    pub expanded_alert_idx: Option<usize>,
    pub selected_day_idx: Option<usize>,
    pub bundles: HashMap<String, WeatherBundle>,
    loading_ids: HashSet<String>,
    pub last_error: Option<String>,
    pub search_query: String,
    last_dispatched_query: String,
    last_search_edit: Option<Instant>,
    search_in_flight: bool,
    pub search_results: Vec<Location>,
    detect_focus_pending: bool,
    detect_in_flight: bool,
    current_location_precise: bool,
    last_location_check: Instant,
    last_location_wall: i64,
    last_route_check: Instant,
    last_route_ip: Option<IpAddr>,
    pub map: MapEngine,
    client: WeatherClient,
    tx: mpsc::Sender<WorkerOut>,
    rx: mpsc::Receiver<WorkerOut>,
    wake: Arc<Notify>,
    boot_instant: Instant,
    last_auto_refresh: Instant,
}

impl WeatherApp {
    pub fn boot() -> (Self, Task<Msg>) {
        let (mut settings, first_run) = load_settings();
        settings.deduplicate_locations();
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
        let (lat, lon) = settings
            .selected_location_id
            .as_ref()
            .and_then(|id| settings.saved_locations.iter().find(|l| &l.id == id))
            .map_or((47.6062, -122.3321), |l| (l.lat, l.lon));
        let light = settings.theme_mode == ThemeMode::Light;
        let (client, (tx, rx), wake, now) = (
            WeatherClient::new(),
            mpsc::channel(),
            Arc::new(Notify::new()),
            Instant::now(),
        );
        let mut app = Self {
            map: MapEngine::new(client.clone(), lat, lon, light, Arc::clone(&wake)),
            settings,
            window_size: Size::new(1180.0, 760.0),
            show_sidebar: true,
            map_expanded: false,
            show_preferences: false,
            expanded_alert_idx: None,
            selected_day_idx: None,
            bundles,
            loading_ids: HashSet::new(),
            last_error: None,
            search_query: String::new(),
            last_dispatched_query: String::new(),
            last_search_edit: None,
            search_in_flight: false,
            search_results: Vec::new(),
            detect_focus_pending: first_run,
            detect_in_flight: false,
            current_location_precise: false,
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
        };
        let light = app.active_palette().is_light;
        #[cfg(target_os = "linux")]
        {
            let wk = Arc::clone(&app.wake);
            sctk_adwaita::on_ready(move || wk.notify_one());
        }
        app.map
            .prewarm_saved_locations(&app.settings.saved_locations, light);
        app.spawn_refresh_all();
        app.spawn_detect_location();
        (app, Task::none())
    }

    pub fn title(&self) -> String {
        self.active_location()
            .map_or_else(|| "Weather".into(), |l| format!("Weather — {}", l.name))
    }
    pub fn theme(&self) -> iced::Theme {
        if self.settings.theme_mode == ThemeMode::Light {
            iced::Theme::Light
        } else {
            iced::Theme::Dark
        }
    }

    pub fn subscription(&self) -> Subscription<Msg> {
        use WeatherCondition as W;
        let search_debounce = self.search_query.trim().len() >= 2
            && self.search_query.trim() != self.last_dispatched_query;
        let sky_anim = self.settings.animate_weather
            && !self.active_palette().is_light
            && self.active_bundle().is_some_and(|b| {
                !b.current.is_day
                    || matches!(
                        b.current.condition,
                        W::Drizzle
                            | W::Rain
                            | W::HeavyRain
                            | W::FreezingRain
                            | W::Thunderstorm
                            | W::Snow
                            | W::HeavySnow
                            | W::Sleet
                    )
            });
        let tick = if self.map.needs_animation_tick(self.map_expanded) || sky_anim {
            window::frames().map(|_| Msg::Tick)
        } else if search_debounce {
            iced::time::every(Duration::from_millis(80)).map(|_| Msg::Tick)
        } else {
            iced::time::every(Duration::from_secs(30)).map(|_| Msg::Tick)
        };
        let worker_wake = Subscription::run_with(WakeHandle(Arc::clone(&self.wake)), wake_stream);
        let keys = keyboard::listen().map(|ev| match ev {
            keyboard::Event::KeyPressed {
                key: Key::Named(keyboard::key::Named::Escape),
                ..
            } => Msg::Escape,
            keyboard::Event::KeyPressed {
                key: Key::Character(c),
                modifiers,
                ..
            } if modifiers.command() => match c.as_str() {
                "r" | "R" => Msg::RefreshAll,
                "k" | "K" => Msg::FocusSearch,
                "m" | "M" => Msg::ToggleMap,
                "q" | "Q" => Msg::WindowCloseRequested,
                _ => Msg::Noop,
            },
            _ => Msg::Noop,
        });
        Subscription::batch([
            tick,
            worker_wake,
            window::resize_events().map(|(_, s)| Msg::WindowResized(s)),
            window::close_requests().map(|_| Msg::WindowCloseRequested),
            keys,
        ])
    }

    fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_results.clear();
        self.last_dispatched_query.clear();
    }

    fn dispatch_search(&mut self, q: String) {
        (self.last_dispatched_query, self.search_in_flight) = (q.clone(), true);
        let (c, tx, wk) = (self.client.clone(), self.tx.clone(), Arc::clone(&self.wake));
        std::thread::spawn(move || {
            if !is_shutting_down() {
                let _ = tx.send(WorkerOut::SearchDone(q.clone(), c.search_locations(&q)));
                wk.notify_one();
            }
        });
    }

    fn spawn_detect_location(&mut self) {
        if self.detect_in_flight {
            return;
        }
        self.detect_in_flight = true;
        self.last_location_check = Instant::now();
        self.last_location_wall = Utc::now().timestamp();
        let (c, tx, wk) = (self.client.clone(), self.tx.clone(), Arc::clone(&self.wake));
        let _ = std::thread::Builder::new()
            .name("loc-detect".into())
            .spawn(move || {
                if is_shutting_down() {
                    return;
                }
                let out = match c.detect_current_location() {
                    Some((loc, precise)) => WorkerOut::CurrentLocation(loc, precise),
                    None => WorkerOut::CurrentLocationFailed,
                };
                let _ = tx.send(out);
                wk.notify_one();
            });
    }

    fn spawn_refresh_batch(&mut self, locs: Vec<Location>) {
        let jobs: Vec<(Location, bool)> = locs
            .into_iter()
            .filter(|l| self.loading_ids.insert(l.id.clone()))
            .map(|l| {
                let cached = self.bundles.contains_key(&l.id);
                (l, cached)
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let (c, tx, wk, prov) = (
            self.client.clone(),
            self.tx.clone(),
            Arc::clone(&self.wake),
            self.settings.provider,
        );
        std::thread::spawn(move || {
            for (loc, has_cached) in jobs {
                if is_shutting_down() {
                    return;
                }
                let res = if has_cached {
                    c.fetch_weather(&loc, prov)
                } else {
                    c.fetch_weather_streaming(&loc, prov, |early| {
                        if !is_shutting_down() {
                            let _ =
                                tx.send(WorkerOut::WeatherLoaded(loc.id.clone(), Box::new(early)));
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

    fn spawn_refresh_location(&mut self, loc: Location) {
        self.spawn_refresh_batch(vec![loc]);
    }

    fn spawn_refresh_all(&mut self) {
        let mut locs = self.settings.saved_locations.clone();
        if let Some(aid) = &self.settings.selected_location_id
            && let Some(p) = locs.iter().position(|l| &l.id == aid)
            && p > 0
        {
            let act = locs.remove(p);
            locs.insert(0, act);
        }
        // Active city gets its own immediate lane; remaining saved cities refresh on 1 background thread
        // so startup never spawns a thundering herd of 20+ concurrent HTTP connections.
        if !locs.is_empty() {
            let rest = locs.split_off(1);
            self.spawn_refresh_batch(locs);
            self.spawn_refresh_batch(rest);
        }
        self.last_auto_refresh = Instant::now();
    }

    fn reset_scroll_tasks() -> Task<Msg> {
        Task::batch([
            operation::snap_to("bento-scroll", operation::RelativeOffset::START),
            operation::snap_to("split-scroll", operation::RelativeOffset::START),
            operation::snap_to("hourly-strip-scroll", operation::RelativeOffset::START),
        ])
    }

    fn select_location(&mut self, id: String, jump_map: bool) -> Task<Msg> {
        self.detect_focus_pending = false;
        (
            self.settings.selected_location_id,
            self.expanded_alert_idx,
            self.selected_day_idx,
            self.last_error,
        ) = (Some(id.clone()), None, None, None);
        save_settings(&self.settings);
        if let Some(loc) = self
            .settings
            .saved_locations
            .iter()
            .find(|l| l.id == id)
            .cloned()
        {
            if jump_map {
                let light = self.active_palette().is_light;
                self.map
                    .jump_to_location(loc.lat, loc.lon, self.map_expanded, light);
            }
            if !self.bundles.contains_key(&id) {
                self.spawn_refresh_location(loc);
            }
        }
        Self::reset_scroll_tasks()
    }

    fn add_or_select_location(&mut self, loc: Location) -> Task<Msg> {
        if let Some(ex) = self
            .settings
            .saved_locations
            .iter()
            .find(|l| !l.is_current_location && l.is_same_place(&loc))
        {
            let id = ex.id.clone();
            return self.select_location(id, true);
        }
        let id = loc.id.clone();
        self.settings.saved_locations.push(loc);
        self.select_location(id, true)
    }

    fn apply_detected_location(&mut self, mut loc: Location, precise: bool) {
        self.detect_in_flight = false;
        let focus = std::mem::take(&mut self.detect_focus_pending);
        let (lat, lon) = (loc.lat, loc.lon);
        let prev = self
            .settings
            .saved_locations
            .iter()
            .find(|l| l.is_current_location)
            .cloned();
        if loc.timezone.is_empty()
            && let Some(p) = &prev
            && !p.timezone.is_empty()
            && (p.lat - lat).abs() < 1.0
            && (p.lon - lon).abs() < 1.0
        {
            loc.timezone = p.timezone.clone();
        }
        let big_move = prev.as_ref().is_none_or(|p| {
            (p.lat - lat).abs() > 0.15 || (p.lon - lon).abs() > 0.15 || p.name != loc.name
        });
        // Never downgrade a precise Wi-Fi fix to a nearby coarse IP fallback unless the user moved cities.
        let moved = if !precise && self.current_location_precise {
            big_move
        } else {
            let tol = if precise { 0.002 } else { 0.015 };
            prev.as_ref().is_none_or(|p| {
                (p.lat - lat).abs() > tol || (p.lon - lon).abs() > tol || p.name != loc.name
            })
        };
        if precise {
            self.current_location_precise = true;
        } else if big_move {
            self.current_location_precise = false;
        }
        let viewing_current =
            self.settings.selected_location_id.as_deref() == Some(loc.id.as_str());
        if !moved && !focus {
            return;
        }
        if moved {
            if big_move {
                self.bundles.remove(&loc.id);
            }
            self.settings
                .saved_locations
                .retain(|l| !l.is_current_location);
            self.settings.saved_locations.insert(0, loc.clone());
        }
        let should_focus =
            focus || (moved && viewing_current) || self.settings.selected_location_id.is_none();
        if should_focus {
            (
                self.settings.selected_location_id,
                self.expanded_alert_idx,
                self.selected_day_idx,
                self.last_error,
            ) = (Some(loc.id.clone()), None, None, None);
            let light = self.active_palette().is_light;
            self.map
                .jump_to_location(lat, lon, self.map_expanded, light);
        }
        if moved || should_focus {
            save_settings(&self.settings);
        }
        if moved || !self.bundles.contains_key(&loc.id) {
            self.loading_ids.remove(&loc.id);
            self.spawn_refresh_location(loc);
        }
    }

    fn drain_worker_messages(&mut self) {
        while let Ok(out) = self.rx.try_recv() {
            match out {
                WorkerOut::WeatherLoaded(id, mut bundle) => {
                    self.loading_ids.remove(&id);
                    if let Some(sv) = self
                        .settings
                        .saved_locations
                        .iter_mut()
                        .find(|l| l.id == id)
                    {
                        // Drop stale weather responses if My Location moved while fetch was in flight.
                        if sv.is_current_location
                            && ((sv.lat - bundle.location.lat).abs() > 0.05
                                || (sv.lon - bundle.location.lon).abs() > 0.05)
                        {
                            continue;
                        }
                        if sv.timezone.is_empty() && !bundle.location.timezone.is_empty() {
                            sv.timezone = bundle.location.timezone.clone();
                            save_settings(&self.settings);
                        } else if !sv.timezone.is_empty() {
                            bundle.location.timezone = sv.timezone.clone();
                        }
                    } else {
                        continue;
                    }
                    if self.settings.selected_location_id.as_deref() == Some(id.as_str()) {
                        self.last_error = None;
                    }
                    self.bundles.insert(id, *bundle);
                }
                WorkerOut::WeatherFailed(id, err) => {
                    self.loading_ids.remove(&id);
                    if self.settings.selected_location_id.as_deref() == Some(id.as_str()) {
                        self.last_error = Some(err);
                    }
                }
                WorkerOut::SearchDone(query, results) => {
                    if query == self.search_query.trim() {
                        self.search_results = results;
                    }
                    self.search_in_flight = false;
                }
                WorkerOut::CurrentLocation(loc, precise) => {
                    self.apply_detected_location(loc, precise)
                }
                WorkerOut::CurrentLocationFailed => {
                    self.detect_in_flight = false;
                    self.detect_focus_pending = false;
                }
            }
        }
    }

    pub fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Noop => {}
            Msg::Tick => {
                self.drain_worker_messages();
                let q = self.search_query.trim().to_string();
                if q.len() >= 2
                    && q != self.last_dispatched_query
                    && !self.search_in_flight
                    && self
                        .last_search_edit
                        .is_some_and(|t| t.elapsed() >= Duration::from_millis(200))
                {
                    self.dispatch_search(q);
                }
                if self.last_route_check.elapsed() >= Duration::from_secs(10) {
                    self.last_route_check = Instant::now();
                    let cur_ip = local_route_ip();
                    if cur_ip != self.last_route_ip {
                        self.last_route_ip = cur_ip;
                        if cur_ip.is_some() {
                            self.spawn_detect_location();
                        }
                    }
                }
                let now_wall = Utc::now().timestamp();
                if self.last_location_check.elapsed() >= Duration::from_secs(180)
                    || (now_wall - self.last_location_wall).abs() >= 120
                {
                    self.spawn_detect_location();
                }
                if self.last_auto_refresh.elapsed() > Duration::from_secs(600) {
                    self.spawn_refresh_all();
                }
                self.map.tick(&self.bundles, self.map_expanded);
            }
            Msg::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            Msg::ToggleUnits => {
                let f = self.settings.temp_unit == TempUnit::Celsius;
                self.settings.temp_unit = if f {
                    TempUnit::Fahrenheit
                } else {
                    TempUnit::Celsius
                };
                self.settings.wind_unit = if f { WindUnit::Mph } else { WindUnit::KmH };
                self.settings.pressure_unit = if f {
                    PressureUnit::InHg
                } else {
                    PressureUnit::HPa
                };
                self.settings.precip_unit = if f {
                    PrecipUnit::Inches
                } else {
                    PrecipUnit::Mm
                };
                save_settings(&self.settings);
                self.map.tick(&self.bundles, self.map_expanded);
            }
            Msg::TogglePreferences => {
                self.show_preferences = !self.show_preferences;
                if self.show_preferences {
                    self.clear_search();
                }
            }
            Msg::RefreshAll => {
                self.spawn_refresh_all();
                self.spawn_detect_location();
            }
            Msg::SelectLocation(id) => return self.select_location(id, true),
            Msg::RemoveLocation(id) if self.settings.saved_locations.len() > 1 => {
                let was_selected =
                    self.settings.selected_location_id.as_deref() == Some(id.as_str());
                self.settings.saved_locations.retain(|l| l.id != id);
                self.bundles.remove(&id);
                self.loading_ids.remove(&id);
                if was_selected
                    && let Some(next_id) =
                        self.settings.saved_locations.first().map(|l| l.id.clone())
                {
                    return self.select_location(next_id, true);
                }
                save_settings(&self.settings);
            }
            Msg::RemoveLocation(_) => {}
            Msg::DetectLocation => {
                self.detect_focus_pending = true;
                self.spawn_detect_location();
            }
            Msg::SearchChanged(v) => {
                (self.search_query, self.last_search_edit) = (v, Some(Instant::now()));
                if self.search_query.trim().len() < 2 {
                    self.search_results.clear();
                    self.last_dispatched_query.clear();
                }
            }
            Msg::SubmitSearch => {
                if let Some(first) = self.search_results.first().cloned() {
                    self.clear_search();
                    return self.add_or_select_location(first);
                } else {
                    let q = self.search_query.trim().to_string();
                    if q.len() >= 2 && !self.search_in_flight {
                        self.dispatch_search(q);
                    }
                }
            }
            Msg::FocusSearch => return operation::focus("city-search-input"),
            Msg::ClearSearch => self.clear_search(),
            Msg::PickSearchResult(l) => {
                self.clear_search();
                return self.add_or_select_location(l);
            }
            Msg::ToggleAlertDetails(i) => {
                self.expanded_alert_idx = (self.expanded_alert_idx != Some(i)).then_some(i)
            }
            Msg::ToggleDayDetails(i) => {
                self.selected_day_idx = (self.selected_day_idx != Some(i)).then_some(i)
            }
            Msg::ToggleMap => {
                return self.update(Msg::Map(if self.map_expanded {
                    MapMsg::CloseMap
                } else {
                    MapMsg::ExpandMap
                }));
            }
            Msg::Pref(p) => {
                let (mut refresh, mut theme_changed) = (false, false);
                match p {
                    Pref::Theme(m) => {
                        self.settings.theme_mode = m;
                        theme_changed = true;
                    }
                    Pref::Wind(u) => self.settings.wind_unit = u,
                    Pref::Pressure(u) => self.settings.pressure_unit = u,
                    Pref::Use24h(v) => self.settings.use_24h_time = v,
                    Pref::Animate(v) => self.settings.animate_weather = v,
                    Pref::Provider(pr) => {
                        self.settings.provider = pr;
                        refresh = true;
                    }
                }
                save_settings(&self.settings);
                if theme_changed && let Some(loc) = self.active_location().cloned() {
                    let light = self.active_palette().is_light;
                    self.map
                        .prewarm_region(loc.lat, loc.lon, self.map_expanded, light);
                }
                if refresh {
                    self.loading_ids.clear();
                    self.spawn_refresh_all();
                }
            }
            Msg::Map(mm) => {
                let (saved, active) = (
                    self.settings.saved_locations.clone(),
                    self.settings.selected_location_id.clone(),
                );
                if let Some(ext) =
                    self.map
                        .handle_msg(mm, &saved, active.as_deref(), &self.settings)
                {
                    match ext {
                        MapExternalAction::SetExpanded(e) => self.map_expanded = e,
                        MapExternalAction::SelectSaved(id) => {
                            return self.select_location(id, false);
                        }
                        MapExternalAction::SaveLocation(loc) => {
                            return self.add_or_select_location(loc);
                        }
                    }
                }
            }
            Msg::Escape => {
                if self.show_preferences {
                    self.show_preferences = false;
                } else if !self.search_query.is_empty() || !self.search_results.is_empty() {
                    self.clear_search();
                } else if self.map.show_layer_menu {
                    self.map.show_layer_menu = false;
                } else if self.map.inspected_coord().is_some() {
                    let _ = self.map.handle_msg(
                        MapMsg::CloseInspector,
                        &self.settings.saved_locations,
                        self.settings.selected_location_id.as_deref(),
                        &self.settings,
                    );
                } else if self.map_expanded {
                    return self.update(Msg::Map(MapMsg::CloseMap));
                } else {
                    self.selected_day_idx = None;
                }
            }
            Msg::WindowResized(s) if s.width > 100.0 && s.height > 100.0 => self.window_size = s,
            Msg::WindowResized(_) => {}
            Msg::WindowCloseRequested => {
                request_shutdown();
                flush_pending_settings();
                return iced::exit();
            }
        }
        Task::none()
    }

    pub fn active_location(&self) -> Option<&Location> {
        self.settings
            .selected_location_id
            .as_ref()
            .and_then(|id| self.settings.saved_locations.iter().find(|l| &l.id == id))
    }
    pub fn active_bundle(&self) -> Option<&WeatherBundle> {
        self.settings
            .selected_location_id
            .as_ref()
            .and_then(|id| self.bundles.get(id))
    }
    pub fn active_palette(&self) -> SkyPalette {
        let (c, d) = self
            .active_bundle()
            .map_or((WeatherCondition::Overcast, true), |b| {
                (b.current.condition, b.current.is_day)
            });
        palette_for(c, d, self.settings.theme_mode)
    }

    pub fn view(&self) -> Element<'_, Msg> {
        ensure_fonts_loaded();
        let pal = self.active_palette();
        let (cond, day) = self
            .active_bundle()
            .map_or((WeatherCondition::Overcast, true), |b| {
                (b.current.condition, b.current.is_day)
            });
        let phase = if self.settings.animate_weather {
            self.boot_instant.elapsed().as_secs_f32()
        } else {
            0.0
        };
        let main = if self.map_expanded {
            self.view_split_map_layout(pal)
        } else {
            self.view_full_bento_layout(pal)
        };
        let side = if self.show_sidebar {
            self.view_sidebar(pal)
        } else {
            sp_w(0)
        };
        let mut layers = vec![
            widget_canvas(WidgetCanvas::SkyBackdrop(pal, cond), Fill, Fill),
            widget_canvas(
                WidgetCanvas::SkyParticles(pal.is_light, cond, day, phase),
                Fill,
                Fill,
            ),
            row![side, main].width(Fill).height(Fill).into(),
        ];
        if !self.search_results.is_empty()
            || self.search_in_flight
            || (self.search_query.trim().len() >= 2
                && self.last_dispatched_query == self.search_query.trim())
        {
            layers.push(self.view_search_dropdown());
        }
        if self.show_preferences {
            layers.push(self.view_preferences_overlay(pal));
        }
        stack(layers).width(Fill).height(Fill).into()
    }

    fn view_sidebar(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let (now, hv) = (
            Utc::now().timestamp(),
            if pal.is_light {
                rgba8(20, 40, 75, 0.10)
            } else {
                rgba8(255, 255, 255, 0.12)
            },
        );
        let top = row![
            icon_btn(
                UiIcon::Sidebar,
                15.0,
                (28, 26),
                (pal.text_secondary, Color::TRANSPARENT, hv),
                6.0,
                Msg::ToggleSidebar
            ),
            sp_w(Fill),
            icon_btn(
                UiIcon::Locate,
                13.5,
                (26, 26),
                (pal.text_secondary, Color::TRANSPARENT, hv),
                6.0,
                Msg::DetectLocation
            ),
        ]
        .padding([8, 10])
        .align_y(Alignment::Center);
        let total = self.settings.saved_locations.len();
        let mut items: Vec<Element<'_, Msg>> = Vec::with_capacity(total * 2);
        for (idx, loc) in self.settings.saved_locations.iter().enumerate() {
            if idx > 0 {
                items.push(container(hairline(pal.divider)).padding([0, 12]).into());
            }
            let (sel, b) = (
                self.settings.selected_location_id.as_deref() == Some(&loc.id),
                self.bundles.get(&loc.id),
            );
            let (pri, sub) = if loc.is_current_location {
                ("My Location".into(), loc.name.clone())
            } else {
                (
                    loc.name.clone(),
                    loc.format_local_time(now, self.settings.use_24h_time),
                )
            };
            let hl = b
                .and_then(|b| b.daily.get(crate::cards::today_daily_index(b, loc)))
                .map(|d| {
                    format!(
                        "H:{} L:{}",
                        self.settings.format_temp(d.temp_max_c),
                        self.settings.format_temp(d.temp_min_c)
                    )
                })
                .unwrap_or_default();
            let lcol = column![
                txt(pri, 13.0, Weight::Semibold, pal.text_primary),
                txt(sub, 10.0, Weight::Medium, pal.text_secondary),
                sp_h(4),
                text(b.map_or("Loading…", |b| b.current.condition.label()))
                    .size(10.5)
                    .color(pal.text_muted)
            ]
            .spacing(1)
            .width(Fill);
            let rcol = column![
                txt(
                    b.map_or_else(
                        || "--".into(),
                        |b| self.settings.format_temp(b.current.temp_c)
                    ),
                    27.0,
                    Weight::Normal,
                    pal.text_primary
                ),
                sp_h(Fill),
                text(hl).size(10.0).color(pal.text_secondary)
            ]
            .align_x(Alignment::End);
            let sbtn = pbtn(
                row![lcol, rcol]
                    .width(Fill)
                    .height(56)
                    .align_y(Alignment::Center),
                [6, 8],
                (
                    Color::TRANSPARENT,
                    if sel {
                        Color::TRANSPARENT
                    } else {
                        pal.sidebar_selected
                    },
                    pal.text_primary,
                ),
                8.0,
                Msg::SelectLocation(loc.id.clone()),
            )
            .width(Fill);
            let del_slot: Element<'_, Msg> = if total > 1 && !loc.is_current_location && sel {
                let del = icon_btn(
                    UiIcon::Close,
                    10.5,
                    (20, 20),
                    (pal.text_muted, Color::TRANSPARENT, hv),
                    6.0,
                    Msg::RemoveLocation(loc.id.clone()),
                );
                container(del)
                    .padding(iced::Padding {
                        top: 6.0,
                        right: 4.0,
                        bottom: 0.0,
                        left: 0.0,
                    })
                    .align_y(Alignment::Start)
                    .into()
            } else {
                sp_w(0)
            };
            let row_el: Element<'_, Msg> = row![sbtn, del_slot].align_y(Alignment::Start).into();
            items.push(
                container(container(row_el).width(Fill).style(flat_box(
                    if sel {
                        pal.sidebar_selected
                    } else {
                        Color::TRANSPARENT
                    },
                    8.0,
                )))
                .padding([2, 6])
                .into(),
            );
        }
        let rail = container(
            column![
                top,
                scrollable(column(items).width(Fill))
                    .height(Fill)
                    .style(sleek_scrollable)
            ]
            .width(204)
            .height(Fill),
        )
        .style(flat_box(pal.sidebar_bg, 0.0));
        row![
            rail,
            container(sp_w(1))
                .height(Fill)
                .style(flat_box(pal.divider, 0.0))
        ]
        .height(Fill)
        .into()
    }

    fn view_top_toolbar_row(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let (bbg, bhv, sbg, sbdr) = if pal.is_light {
            (
                rgba8(255, 255, 255, 0.72),
                rgba8(255, 255, 255, 0.94),
                rgba8(255, 255, 255, 0.78),
                rgba8(30, 50, 80, 0.12),
            )
        } else {
            (
                rgba8(20, 28, 40, 0.36),
                rgba8(255, 255, 255, 0.18),
                rgba8(20, 28, 40, 0.44),
                rgba8(255, 255, 255, 0.16),
            )
        };
        let left = if !self.show_sidebar {
            icon_btn(
                UiIcon::Sidebar,
                15.0,
                (28, 28),
                (pal.text_primary, bbg, bhv),
                7.0,
                Msg::ToggleSidebar,
            )
        } else {
            sp_w(1)
        };
        let unit = pbtn(
            txt(
                if self.settings.temp_unit == TempUnit::Celsius {
                    "°C"
                } else {
                    "°F"
                },
                11.5,
                Weight::Semibold,
                pal.text_primary,
            ),
            [4, 8],
            (bbg, bhv, pal.text_primary),
            7.0,
            Msg::ToggleUnits,
        );
        let inp = text_input("Search", &self.search_query)
            .id("city-search-input")
            .on_input(Msg::SearchChanged)
            .on_submit(Msg::SubmitSearch)
            .size(12.5)
            .font(fw(Weight::Normal))
            .padding([4, 4])
            .width(if self.search_query.is_empty() {
                164
            } else {
                144
            })
            .style(move |_, _| text_input::Style {
                background: iced::Background::Color(Color::TRANSPARENT),
                border: iced::Border::default(),
                icon: Color::TRANSPARENT,
                placeholder: pal.text_muted,
                value: pal.text_primary,
                selection: Color {
                    a: 0.38,
                    ..pal.accent
                },
            });
        let mut srow = row![icon_svg(UiIcon::Search, 12.5, pal.text_muted), inp]
            .spacing(4)
            .padding([1, 8])
            .align_y(Alignment::Center);
        if !self.search_query.is_empty() {
            srow = srow.push(pbtn(
                icon_svg(UiIcon::Close, 10.0, pal.text_muted),
                [2, 2],
                (Color::TRANSPARENT, bhv, pal.text_primary),
                5.0,
                Msg::ClearSearch,
            ));
        }
        row![
            left,
            sp_w(Fill),
            unit,
            icon_btn(
                UiIcon::Refresh,
                13.5,
                (26, 26),
                (pal.text_primary, bbg, bhv),
                7.0,
                Msg::RefreshAll
            ),
            icon_btn(
                UiIcon::Sliders,
                13.5,
                (26, 26),
                (pal.text_primary, bbg, bhv),
                7.0,
                Msg::TogglePreferences
            ),
            container(srow).style(hud_pill(sbg, sbdr, 8.0)),
        ]
        .spacing(6)
        .padding([8, 12])
        .align_y(Alignment::Center)
        .width(Fill)
        .into()
    }

    fn view_header_stack<'a>(
        &'a self,
        b: &'a WeatherBundle,
        pal: SkyPalette,
        w: f32,
        gap: f32,
    ) -> Element<'a, Msg> {
        let mut h = column![self.view_hero(b, pal)]
            .spacing(gap)
            .width(w)
            .align_x(Alignment::Center);
        if !b.alerts.is_empty() {
            h = h.push(self.view_alerts_card(b, pal, w));
        }
        if let Some(mc) = self.view_minutely_card(b, pal, w) {
            h = h.push(mc);
        }
        h.into()
    }

    fn view_full_bento_layout(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let toolbar = self.view_top_toolbar_row(pal);
        let Some(b) = self.active_bundle() else {
            let load = container(
                column![
                    txt("Loading Weather…", 18.0, Weight::Medium, pal.text_primary),
                    text(self.last_error.as_deref().unwrap_or(""))
                        .size(12.0)
                        .color(rgb8(255, 120, 120))
                ]
                .spacing(6)
                .align_x(Alignment::Center),
            )
            .center_x(Fill)
            .center_y(Fill);
            return column![toolbar, load].width(Fill).height(Fill).into();
        };
        let (avail, g, th) = (
            (self.window_size.width - if self.show_sidebar { 205.0 } else { 0.0 }).max(480.0),
            12.0_f32,
            136.0_f32,
        );
        let wide = avail >= 820.0;
        let u = if wide {
            ((avail - 64.0 - g * 5.0) / 6.0).clamp(112.0, 132.0).floor()
        } else {
            ((avail - 40.0 - g * 3.0) / 4.0).clamp(108.0, 150.0).floor()
        };
        let (w2, grid_w, mh, d10h) = (
            u * 2.0 + g,
            if wide {
                u * 6.0 + g * 5.0
            } else {
                u * 4.0 + g * 3.0
            },
            th * 2.0 + g,
            th * 3.0 + g * 2.0,
        );
        let mut content = column![
            self.view_header_stack(b, pal, grid_w, 16.0),
            self.view_hourly_card(b, pal, grid_w, 16)
        ]
        .spacing(16)
        .width(grid_w)
        .align_x(Alignment::Center);
        let (d10, aqi, wind, map, moon) = (
            self.view_ten_day_card(b, pal, w2, Some(d10h)),
            self.view_aqi_card(b, pal, w2, th),
            self.view_wind_card(b, pal, w2, th),
            self.view_map_bento_card(pal, w2, mh),
            self.view_moon_card(pal, w2, th),
        );
        let (uv, sun, feels, prec) = (
            self.view_uv_card(b, pal, u, th),
            self.view_sun_card(b, pal, u, th),
            self.view_feels_like_card(b, pal, u, th),
            self.view_precip_summary_card(b, pal, u, th),
        );
        let (hum, vis, prs, avg) = (
            self.view_humidity_card(b, pal, u, th),
            self.view_visibility_card(b, pal, u, th),
            self.view_pressure_card(b, pal, u, th),
            self.view_averages_card(b, pal, u, th),
        );
        content = if wide {
            let mid = column![aqi, wind, row![uv, sun].spacing(g)].spacing(g);
            let right = column![map, row![feels, prec].spacing(g)].spacing(g);
            content
                .push(row![d10, mid, right].spacing(g).align_y(Alignment::Start))
                .push(row![moon, hum, vis, prs, avg].spacing(g))
        } else {
            content
                .push(row![d10, column![map, aqi].spacing(g)].spacing(g))
                .push(row![wind, uv, sun].spacing(g))
                .push(row![moon, feels, prec].spacing(g))
                .push(row![hum, vis, prs, avg].spacing(g))
        };
        let attr = format!(
            "Weather: {} • Radar: LibreWXR (CC BY 4.0), NOAA HRRR/MRMS • Map: Esri & OSM",
            b.source_name
        );
        content = content.push(text(attr).size(10.5).color(pal.text_muted));
        let body =
            scrollable(column![sp_h(8), container(content).center_x(Fill), sp_h(28)].width(Fill))
                .id("bento-scroll")
                .width(Fill)
                .height(Fill)
                .style(sleek_scrollable);
        column![toolbar, body].width(Fill).height(Fill).into()
    }

    fn view_split_map_layout(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let toolbar = self.view_top_toolbar_row(pal);
        let avail = self.window_size.width - if self.show_sidebar { 205.0 } else { 0.0 };
        let left: Element<'_, Msg> = if avail >= 820.0
            && let Some(b) = self.active_bundle()
        {
            let (cw, g, th) = (344.0_f32, 10.0_f32, 122.0_f32);
            let u = (cw - g) * 0.5;
            let col = column![
                self.view_header_stack(b, pal, cw, g),
                self.view_hourly_card(b, pal, cw, 9),
                row![
                    self.view_ten_day_card(b, pal, u + 16.0, None),
                    column![
                        self.view_aqi_card(b, pal, u - 16.0, th),
                        self.view_wind_card(b, pal, u - 16.0, th),
                        self.view_uv_card(b, pal, u - 16.0, th)
                    ]
                    .spacing(g)
                ]
                .spacing(g),
                row![
                    self.view_sun_card(b, pal, u, th),
                    self.view_feels_like_card(b, pal, u, th)
                ]
                .spacing(g),
                row![
                    self.view_precip_summary_card(b, pal, u, th),
                    self.view_humidity_card(b, pal, u, th)
                ]
                .spacing(g),
                self.view_moon_card(pal, cw, th),
                row![
                    self.view_visibility_card(b, pal, u, th),
                    self.view_pressure_card(b, pal, u, th)
                ]
                .spacing(g),
                self.view_averages_card(b, pal, cw, th),
                sp_h(16),
            ]
            .spacing(g)
            .width(cw);
            scrollable(container(col).padding([4, 12]))
                .id("split-scroll")
                .width(cw + 24.0)
                .height(Fill)
                .style(sleek_scrollable)
                .into()
        } else {
            sp_w(0)
        };
        let map_border = if pal.is_light {
            rgba8(20, 35, 60, 0.12)
        } else {
            rgba8(255, 255, 255, 0.14)
        };
        let map_pane = container(
            container(self.map.view_expanded(
                &self.settings.saved_locations,
                &self.bundles,
                self.settings.selected_location_id.as_deref(),
                &self.settings,
                Msg::Map,
            ))
            .width(Fill)
            .height(Fill)
            .clip(true)
            .style(move |_| container::Style {
                border: iced::Border {
                    color: map_border,
                    width: 1.0,
                    radius: 13.0.into(),
                },
                ..Default::default()
            }),
        )
        .width(Fill)
        .height(Fill)
        .padding([0, 12]);
        column![
            toolbar,
            row![left, map_pane].width(Fill).height(Fill),
            sp_h(10)
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn view_search_dropdown(&self) -> Element<'_, Msg> {
        let items: Vec<Element<'_, Msg>> =
            if self.search_in_flight && self.search_results.is_empty() {
                vec![
                    container(txt(
                        "Searching cities…",
                        12.0,
                        Weight::Medium,
                        rgba8(215, 226, 244, 0.82),
                    ))
                    .padding([6, 10])
                    .into(),
                ]
            } else if self.search_results.is_empty() {
                vec![
                    container(txt(
                        "No matching cities found",
                        12.0,
                        Weight::Medium,
                        rgba8(215, 226, 244, 0.72),
                    ))
                    .padding([6, 10])
                    .into(),
                ]
            } else {
                self.search_results
                    .iter()
                    .map(|r| {
                        let sub = r.subtitle();
                        let label = if sub.is_empty() {
                            r.name.clone()
                        } else {
                            format!("{} — {sub}", r.name)
                        };
                        let lbl = txt(label, 12.0, Weight::Medium, Color::WHITE);
                        pbtn(
                            lbl,
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
        let menu = mouse_area(
            container(column(items).spacing(2).width(268))
                .padding(6)
                .style(hud_pill(
                    rgba8(24, 32, 46, 0.96),
                    rgba8(255, 255, 255, 0.18),
                    10.0,
                )),
        )
        .on_press(Msg::Noop);
        mouse_area(
            column![sp_h(40), row![sp_w(Fill), menu, sp_w(12)], sp_h(Fill)]
                .width(Fill)
                .height(Fill),
        )
        .on_press(Msg::ClearSearch)
        .into()
    }

    fn view_preferences_overlay(&self, pal: SkyPalette) -> Element<'_, Msg> {
        let s = &self.settings;
        let is_c = s.temp_unit == TempUnit::Celsius;
        let head = row![
            txt("Weather Preferences", 15.5, Weight::Semibold, Color::WHITE),
            sp_w(Fill),
            pbtn(
                icon_svg(UiIcon::Close, 14.0, Color::WHITE),
                [4, 4],
                (Color::TRANSPARENT, rgba8(255, 255, 255, 0.16), Color::WHITE),
                6.0,
                Msg::TogglePreferences
            ),
        ]
        .align_y(Alignment::Center);
        let provs: Vec<_> = [
            ProviderChoice::Auto,
            ProviderChoice::MetNorway,
            ProviderChoice::OpenMeteo,
        ]
        .into_iter()
        .map(|p| pref_chip(p.label(), s.provider == p, Msg::Pref(Pref::Provider(p))))
        .collect();
        let chips = |lbl, items: &[(&'static str, bool, Msg)]| {
            pref_row(
                lbl,
                items
                    .iter()
                    .map(|(l, a, m)| pref_chip(l, *a, m.clone()))
                    .collect(),
            )
        };
        let modal = container(
            column![
                head,
                hairline(pal.divider),
                chips(
                    "Units",
                    &[
                        (
                            "Celsius (°C)",
                            is_c,
                            if is_c { Msg::Noop } else { Msg::ToggleUnits }
                        ),
                        (
                            "Fahrenheit (°F)",
                            !is_c,
                            if !is_c { Msg::Noop } else { Msg::ToggleUnits }
                        ),
                    ]
                ),
                chips(
                    "Wind",
                    &[
                        (
                            "km/h",
                            s.wind_unit == WindUnit::KmH,
                            Msg::Pref(Pref::Wind(WindUnit::KmH))
                        ),
                        (
                            "mph",
                            s.wind_unit == WindUnit::Mph,
                            Msg::Pref(Pref::Wind(WindUnit::Mph))
                        ),
                        (
                            "m/s",
                            s.wind_unit == WindUnit::Ms,
                            Msg::Pref(Pref::Wind(WindUnit::Ms))
                        ),
                        (
                            "knots",
                            s.wind_unit == WindUnit::Knots,
                            Msg::Pref(Pref::Wind(WindUnit::Knots))
                        ),
                    ]
                ),
                chips(
                    "Pressure",
                    &[
                        (
                            "hPa",
                            s.pressure_unit == PressureUnit::HPa,
                            Msg::Pref(Pref::Pressure(PressureUnit::HPa))
                        ),
                        (
                            "inHg",
                            s.pressure_unit == PressureUnit::InHg,
                            Msg::Pref(Pref::Pressure(PressureUnit::InHg))
                        ),
                    ]
                ),
                chips(
                    "Appearance",
                    &[
                        (
                            "Dynamic Sky",
                            s.theme_mode == ThemeMode::DynamicSky,
                            Msg::Pref(Pref::Theme(ThemeMode::DynamicSky))
                        ),
                        (
                            "Dark",
                            s.theme_mode == ThemeMode::Dark,
                            Msg::Pref(Pref::Theme(ThemeMode::Dark))
                        ),
                        (
                            "Light",
                            s.theme_mode == ThemeMode::Light,
                            Msg::Pref(Pref::Theme(ThemeMode::Light))
                        ),
                    ]
                ),
                chips(
                    "Clock & Sky",
                    &[
                        (
                            "24-Hour Time",
                            s.use_24h_time,
                            Msg::Pref(Pref::Use24h(!s.use_24h_time))
                        ),
                        (
                            "Sky Motion",
                            s.animate_weather,
                            Msg::Pref(Pref::Animate(!s.animate_weather))
                        ),
                    ]
                ),
                hairline(pal.divider),
                txt(
                    "Forecast Model",
                    11.5,
                    Weight::Semibold,
                    rgba8(200, 214, 236, 0.75)
                ),
                column(provs).spacing(5),
            ]
            .spacing(10)
            .width(385),
        )
        .padding(18)
        .style(hud_pill(
            rgba8(22, 30, 44, 0.96),
            rgba8(255, 255, 255, 0.16),
            14.0,
        ));
        mouse_area(
            container(mouse_area(modal).on_press(Msg::Noop))
                .width(Fill)
                .height(Fill)
                .center_x(Fill)
                .center_y(Fill)
                .style(flat_box(rgba8(0, 0, 0, 0.42), 0.0)),
        )
        .on_press(Msg::TogglePreferences)
        .into()
    }
}

fn pref_chip<'a>(lbl: &'static str, act: bool, msg: Msg) -> Element<'a, Msg> {
    let cols = if act {
        (rgb8(10, 132, 255), rgb8(10, 132, 255), Color::WHITE)
    } else {
        (
            rgba8(255, 255, 255, 0.10),
            rgba8(255, 255, 255, 0.20),
            Color::WHITE,
        )
    };
    pbtn(
        txt(
            lbl,
            12.0,
            if act {
                Weight::Semibold
            } else {
                Weight::Normal
            },
            Color::WHITE,
        ),
        [5, 11],
        cols,
        7.0,
        msg,
    )
    .into()
}

fn pref_row<'a>(lbl: &'static str, chips: Vec<Element<'a, Msg>>) -> Element<'a, Msg> {
    let mut items = Vec::with_capacity(chips.len() + 1);
    items.push(
        container(text(lbl).size(12.5).color(rgba8(220, 230, 245, 0.85)))
            .width(105)
            .into(),
    );
    items.extend(chips);
    row(items).spacing(6).align_y(Alignment::Center).into()
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
            if stop && app.active_bundle().is_some() && !app.map.has_pending_work(app.map_expanded)
            {
                break;
            }
        }
    }

    fn snap(
        app: &WeatherApp,
        st: &iced::Settings,
        sz: Size,
        th: &iced::Theme,
        dir: Option<&PathBuf>,
        name: &str,
    ) {
        let mut sim = Simulator::with_size(st.clone(), sz, app.view());
        let s = sim.snapshot(th).expect(name);
        if let Some(d) = dir {
            let _ = s.matches_image(d.join(name));
        }
    }

    #[test]
    fn simulator_renders_bento_and_split_map_views() {
        ensure_fonts_loaded();
        let (mut app, _) = WeatherApp::boot();
        let dir = reset_dir("WEATHER_RS_TEST_SHOTS");
        pump(&mut app, if dir.is_some() { 35 } else { 8 }, 120, true);
        let st = iced::Settings {
            default_font: fw(Weight::Normal),
            ..iced::Settings::default()
        };
        let (sz_m, sz_b) = (Size::new(1360.0, 860.0), Size::new(1360.0, 980.0));
        if dir.is_some() {
            snap(&app, &st, sz_m, &iced::Theme::Dark, None, "warm");
            pump(&mut app, 18, 100, true);
        }
        for (name, m) in [
            ("01_bento_dynamic", ThemeMode::DynamicSky),
            ("02_bento_dark", ThemeMode::Dark),
            ("03_bento_light", ThemeMode::Light),
        ] {
            let _ = app.update(Msg::Pref(Pref::Theme(m)));
            snap(
                &app,
                &st,
                sz_b,
                if m == ThemeMode::Light {
                    &iced::Theme::Light
                } else {
                    &iced::Theme::Dark
                },
                dir.as_ref(),
                name,
            );
        }
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::DynamicSky)));
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        if dir.is_some() {
            snap(&app, &st, sz_m, &iced::Theme::Dark, None, "warm_map");
            pump(&mut app, 25, 120, true);
        }
        for ov in WeatherOverlay::ALL {
            let _ = app.update(Msg::Map(MapMsg::SelectOverlay(ov)));
            if dir.is_some() {
                pump(&mut app, 8, 100, false);
            }
            snap(
                &app,
                &st,
                sz_m,
                &iced::Theme::Dark,
                dir.as_ref(),
                &format!("04_map_{ov:?}").to_lowercase(),
            );
        }
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(
            WeatherOverlay::RadarTimeline,
        )));
        let _ = app.update(Msg::Map(MapMsg::ToggleLayerMenu));
        assert!(app.map.show_layer_menu);
        snap(
            &app,
            &st,
            sz_m,
            &iced::Theme::Dark,
            dir.as_ref(),
            "05_map_layer_popover",
        );
        let _ = app.update(Msg::Escape);
        assert!(!app.map.show_layer_menu && app.map_expanded);
        for step in 0..4 {
            let _ = app.update(Msg::Map(MapMsg::PanPixels(28.0 + step as f32 * 8.0, -18.0)));
            snap(
                &app,
                &st,
                sz_m,
                &iced::Theme::Dark,
                dir.as_ref(),
                &format!("06_pan_frame_{step}"),
            );
        }
        let _ = app.update(Msg::Escape);
        assert!(!app.map_expanded);
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
        fn new(logical: Size, scale: f32, cur: iced::Point, env_dir: &str) -> Self {
            use iced_test::core::renderer::Headless;
            let bk = std::env::var("ICED_TEST_BACKEND").ok();
            let r: iced::Renderer =
                iced_test::futures::futures::executor::block_on(<iced::Renderer as Headless>::new(
                    fw(Weight::Normal),
                    iced::Pixels(16.0),
                    bk.as_deref(),
                ))
                .expect("headless renderer");
            Self {
                r,
                cache: Default::default(),
                logical,
                phys: Size::new(
                    (logical.width * scale) as u32,
                    (logical.height * scale) as u32,
                ),
                scale,
                cur: iced::mouse::Cursor::Available(cur),
                dir: reset_dir(env_dir),
            }
        }

        fn render(
            &mut self,
            app: &mut WeatherApp,
            evs: &[iced::Event],
            name: Option<&str>,
        ) -> Vec<u8> {
            use iced_test::core::{clipboard::Null, renderer::Headless, theme::Base as _};
            use iced_test::runtime::UserInterface;
            if !evs.is_empty() {
                let mut msgs = Vec::new();
                let mut ui = UserInterface::build(
                    app.view(),
                    self.logical,
                    std::mem::take(&mut self.cache),
                    &mut self.r,
                );
                let _ = ui.update(evs, self.cur, &mut self.r, &mut Null, &mut msgs);
                self.cache = ui.into_cache();
                for m in msgs {
                    let _ = app.update(m);
                }
            }
            let (th, base) = (app.theme(), app.theme().base());
            let mut ui = UserInterface::build(
                app.view(),
                self.logical,
                std::mem::take(&mut self.cache),
                &mut self.r,
            );
            let _ = ui.update(
                &[iced::Event::Window(window::Event::RedrawRequested(
                    Instant::now(),
                ))],
                self.cur,
                &mut self.r,
                &mut Null,
                &mut Vec::new(),
            );
            ui.draw(
                &mut self.r,
                &th,
                &iced_test::core::renderer::Style {
                    text_color: base.text_color,
                },
                self.cur,
            );
            self.cache = ui.into_cache();
            let rgba = self
                .r
                .screenshot(self.phys, self.scale, base.background_color);
            if let (Some(d), Some(n)) = (&self.dir, name)
                && let Some(img) =
                    ::image::RgbaImage::from_raw(self.phys.width, self.phys.height, rgba.clone())
            {
                let _ = img.save(d.join(format!("{n}.png")));
            }
            rgba
        }

        fn px(&self, rgba: &[u8], lx: u32, ly: u32) -> (u8, u8, u8) {
            let s = self.scale as u32;
            let o = ((((ly * s).min(self.phys.height - 1)) * self.phys.width
                + (lx * s).min(self.phys.width - 1))
                * 4) as usize;
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

    fn mk_loc(n: &str, la: f64, lo: f64, tz: &str, cur: bool) -> Location {
        let mut l = Location::new(n, "", "", "", la, lo, tz);
        if cur {
            (l.id, l.is_current_location) = ("current_location".into(), true);
        }
        l
    }

    #[test]
    fn persistent_renderer_zoom_in_out_repro() {
        ensure_fonts_loaded();
        let mut h = Harness::new(
            Size::new(1360.0, 900.0),
            2.0,
            iced::Point::new(980.0, 460.0),
            "WEATHER_RS_ZOOM_REPRO",
        );
        let (mut app, _) = WeatherApp::boot();
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
        pump(&mut app, if h.dir.is_some() { 30 } else { 12 }, 100, true);
        let no_bleed = |h: &Harness, rgba: &[u8], st: &str| {
            for (pt, lbl) in [((576, 350), "left"), ((920, 12), "top")] {
                let (r, g, b) = h.px(rgba, pt.0, pt.1);
                assert!(
                    r < 80 && g < 100 && b < 135,
                    "{st}: {lbl} bleed ({r},{g},{b})"
                );
            }
        };
        let _ = h.render(&mut app, &[], None);
        pump(&mut app, 10, 80, false);
        let img = h.render(&mut app, &[], Some("01_bento_ready"));
        let (br, bg, bb) = h.px(&img, 1335, 575);
        assert!(
            br < 75 && bg < 95 && bb < 130,
            "Bento bleed ({br},{bg},{bb})"
        );
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = h.render(&mut app, &[], None);
        pump(&mut app, 12, 80, false);
        let img = h.render(&mut app, &[], Some("02_split_map_z5"));
        let (tr, tg, tb) = h.px(&img, 980, 12);
        assert!(
            tr < 75 && tg < 95 && tb < 130,
            "Split bleed ({tr},{tg},{tb})"
        );
        for (pre, n, dy) in [("03_zoom_in", 6_usize, 3.5_f32), ("05_zoom_out", 7, -4.0)] {
            for i in 1..=n {
                let ev = iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: dy },
                });
                let img = h.render(&mut app, &[ev], Some(&format!("{pre}_step{i}")));
                no_bleed(&h, &img, pre);
            }
            h.pump_render(&mut app, 16, 90);
            let settled = if dy > 0.0 {
                "04_zoom_in_settled"
            } else {
                "06_zoom_out_settled"
            };
            let img = h.render(&mut app, &[], Some(settled));
            no_bleed(&h, &img, pre);
        }
        let _ = app.update(Msg::Map(MapMsg::RecenterActive));
        for _ in 0..16 {
            let _ = app.update(Msg::Tick);
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
        let mut h = Harness::new(
            Size::new(1340.0, 860.0),
            1.0,
            iced::Point::new(500.0, 400.0),
            "WEATHER_RS_GB_REPRO",
        );
        let (mut app, _) = WeatherApp::boot();
        for loc in [
            mk_loc("Gaborone", -24.653, 25.905, "Africa/Gaborone", false),
            mk_loc("Berkeley", 37.872, -122.273, "America/Los_Angeles", false),
        ] {
            if !app
                .settings
                .saved_locations
                .iter()
                .any(|l| l.name == loc.name)
            {
                app.settings.saved_locations.push(loc);
            }
        }
        let find_id = |app: &WeatherApp, n: &str| {
            app.settings
                .saved_locations
                .iter()
                .find(|l| l.name == n)
                .unwrap()
                .id
                .clone()
        };
        let (gab_id, brk_id) = (find_id(&app, "Gaborone"), find_id(&app, "Berkeley"));
        let phys = h.phys;
        let count_badge_blue = |rgba: &[u8], y1: u32| -> usize {
            (400..y1.min(phys.height))
                .flat_map(|y| (900..1250_u32.min(phys.width)).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let i = ((y * phys.width + x) * 4) as usize;
                    rgba[i + 2] > 160
                        && rgba[i + 2] as i32 - rgba[i] as i32 > 75
                        && rgba[i + 1] > 70
                })
                .count()
        };

        let _ = app.update(Msg::SelectLocation(gab_id.clone()));
        (app.expanded_alert_idx, app.selected_day_idx, app.last_error) =
            (Some(0), Some(2), Some("stale".into()));
        pump(&mut app, 15, 60, false);
        let gab_img = h.render(&mut app, &[], Some("01_bento_gaborone"));
        if app.bundles.contains_key(&gab_id) {
            assert!(
                count_badge_blue(&gab_img, 720) >= 80,
                "expected city badge on Gaborone mini-map"
            );
        }
        let _ = app.update(Msg::SelectLocation(brk_id.clone()));
        assert_eq!(
            (
                app.expanded_alert_idx,
                app.selected_day_idx,
                app.last_error.as_ref()
            ),
            (None, None, None)
        );
        let _ = h.render(&mut app, &[], Some("02_bento_berkeley_immediate"));
        pump(&mut app, 15, 60, false);
        let brk_img = h.render(&mut app, &[], Some("03_bento_berkeley_settled"));
        if app.bundles.contains_key(&brk_id) {
            assert!(
                count_badge_blue(&brk_img, 820) >= 80,
                "expected city badge on Berkeley mini-map"
            );
        }
        let _ = app.update(Msg::SelectLocation(gab_id.clone()));
        let _ = h.render(&mut app, &[], Some("04_bento_gaborone_immediate"));
        pump(&mut app, 15, 60, false);
        let _ = h.render(&mut app, &[], Some("05_bento_gaborone_settled"));

        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        pump(&mut app, 15, 60, false);
        let _ = h.render(&mut app, &[], Some("06_split_gaborone"));
        for (id, exp_lat, tag) in [
            (&brk_id, 37.872, "berkeley"),
            (&gab_id, -24.653, "gaborone"),
        ] {
            let _ = app.update(Msg::SelectLocation(id.clone()));
            assert!(
                (app.map.center_lat - exp_lat).abs() < 0.25 && !app.map.is_flying(),
                "expected intercontinental jump to {tag} to snap, got lat={}",
                app.map.center_lat
            );
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
        let (mut app, _) = WeatherApp::boot();
        let tokyo = mk_loc("Tokyo", 35.676, 139.650, "Asia/Tokyo", false);
        let tid = tokyo.id.clone();
        let _ = app.add_or_select_location(tokyo);
        assert_eq!(
            app.settings.selected_location_id.as_deref(),
            Some(tid.as_str())
        );
        (app.expanded_alert_idx, app.selected_day_idx, app.last_error) =
            (Some(0), Some(3), Some("err".into()));
        let (first_id, first_lat) = (
            app.settings.saved_locations[0].id.clone(),
            app.settings.saved_locations[0].lat,
        );

        let _ = app.update(Msg::RemoveLocation(tid));
        assert_eq!(
            app.settings.selected_location_id.as_deref(),
            Some(first_id.as_str())
        );
        assert_eq!(
            (
                app.expanded_alert_idx,
                app.selected_day_idx,
                app.last_error.as_ref()
            ),
            (None, None, None)
        );
        assert!((app.map.center_lat - first_lat).abs() < 0.25);

        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = app.update(Msg::Map(MapMsg::ToggleRadarPlay));
        assert!(app.map_expanded && app.map.radar_playing);
        let _ = app.update(Msg::Escape);
        assert!(!app.map_expanded && !app.map.radar_playing);

        let _ = app.update(Msg::SearchChanged("London".into()));
        let _ = app.update(Msg::TogglePreferences);
        assert!(app.show_preferences && app.search_query.is_empty());

        let cur_sf = mk_loc(
            "San Francisco",
            37.775,
            -122.419,
            "America/Los_Angeles",
            true,
        );
        let saved_brk = mk_loc("Berkeley", 37.872, -122.273, "America/Los_Angeles", false);
        let brk_id = saved_brk.id.clone();
        app.settings.saved_locations = vec![cur_sf, saved_brk];
        let _ = app.select_location("current_location".into(), true);

        let cur_brk = mk_loc("Berkeley", 37.892, -122.264, "America/Los_Angeles", true);
        app.apply_detected_location(cur_brk.clone(), false);
        assert_eq!(app.settings.saved_locations[0].name, "Berkeley");
        assert!(app.settings.saved_locations[0].is_current_location);
        assert!(app.settings.saved_locations.iter().any(|l| l.id == brk_id));
        assert!((app.map.center_lat - 37.892).abs() < 0.05);

        // Background re-check at the SAME location must be a no-op.
        (app.selected_day_idx, app.map.center_lat) = (Some(2), 10.0);
        app.apply_detected_location(cur_brk.clone(), false);
        assert_eq!(app.selected_day_idx, Some(2));
        assert!((app.map.center_lat - 10.0).abs() < 0.05);

        // Explicit Locate jumps back to My Location.
        app.detect_focus_pending = true;
        app.apply_detected_location(cur_brk.clone(), false);
        assert!((app.map.center_lat - 37.892).abs() < 0.05);
        assert_eq!(app.selected_day_idx, None);

        // Wi-Fi refinement updates coords & inherits timezone; coarse IP fallback must NOT downgrade.
        let cur_wifi = mk_loc("Berkeley", 37.8781, -122.2668, "", true);
        app.apply_detected_location(cur_wifi, true);
        assert!((app.settings.saved_locations[0].lat - 37.8781).abs() < 0.0005);
        assert_eq!(
            app.settings.saved_locations[0].timezone,
            "America/Los_Angeles"
        );
        app.apply_detected_location(cur_brk, false);
        assert!((app.settings.saved_locations[0].lat - 37.8781).abs() < 0.0005);
    }
}
