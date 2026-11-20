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
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
    CurrentLocation(Location),
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
    pub map: MapEngine,
    client: WeatherClient,
    tx: mpsc::Sender<WorkerOut>,
    rx: mpsc::Receiver<WorkerOut>,
    boot_instant: Instant,
    last_auto_refresh: Instant,
}

impl WeatherApp {
    pub fn boot() -> (Self, Task<Msg>) {
        use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
        ensure_fonts_loaded();
        let (mut settings, first_run) = load_settings();
        settings.deduplicate_locations();
        let bundles = settings
            .saved_locations
            .par_iter()
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
        let (client, (tx, rx), now) = (WeatherClient::new(), mpsc::channel(), Instant::now());
        let mut app = Self {
            map: MapEngine::new(client.clone(), lat, lon),
            settings,
            window_size: Size::new(1340.0, 860.0),
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
            client,
            tx,
            rx,
            boot_instant: now,
            last_auto_refresh: now,
        };
        app.spawn_refresh_all();
        if first_run
            || !app
                .settings
                .saved_locations
                .iter()
                .any(|l| l.is_current_location)
        {
            app.spawn_detect_location();
        }
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
        let fast = self.map.needs_fast_tick(self.map_expanded)
            || !self.loading_ids.is_empty()
            || self.search_in_flight
            || (self.search_query.trim().len() >= 2
                && self.search_query.trim() != self.last_dispatched_query);
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
        let tick = if fast || sky_anim {
            window::frames().map(|_| Msg::Tick)
        } else {
            iced::time::every(Duration::from_secs(2)).map(|_| Msg::Tick)
        };
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
                _ => Msg::Noop,
            },
            _ => Msg::Noop,
        });
        Subscription::batch([
            tick,
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
        let (c, tx) = (self.client.clone(), self.tx.clone());
        rayon::spawn(move || {
            if !is_shutting_down() {
                let _ = tx.send(WorkerOut::SearchDone(q.clone(), c.search_locations(&q)));
            }
        });
    }

    fn spawn_detect_location(&self) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        rayon::spawn(move || {
            if !is_shutting_down()
                && let Some(loc) = c.detect_current_location()
            {
                let _ = tx.send(WorkerOut::CurrentLocation(loc));
            }
        });
    }

    fn spawn_refresh_location(&mut self, loc: Location) {
        if !self.loading_ids.insert(loc.id.clone()) {
            return;
        }
        let (c, tx, prov) = (self.client.clone(), self.tx.clone(), self.settings.provider);
        rayon::spawn(move || {
            if is_shutting_down() {
                return;
            }
            match c.fetch_weather(&loc, prov) {
                Ok(b) if !is_shutting_down() => {
                    let disk = b.clone();
                    let _ = tx.send(WorkerOut::WeatherLoaded(loc.id, Box::new(b)));
                    if !is_shutting_down() {
                        save_cached_bundle(&disk);
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    let _ = tx.send(WorkerOut::WeatherFailed(loc.id, err));
                }
            }
        });
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
        for loc in locs {
            self.spawn_refresh_location(loc);
        }
        self.last_auto_refresh = Instant::now();
    }

    fn add_or_select_location(&mut self, loc: Location) {
        if let Some(ex) = self
            .settings
            .saved_locations
            .iter()
            .find(|l| l.is_same_place(&loc))
        {
            let (id, lat, lon) = (ex.id.clone(), ex.lat, ex.lon);
            self.settings.selected_location_id = Some(id);
            self.map.fly_to(lat, lon, Some(6.5));
            save_settings(&self.settings);
            return;
        }
        let (id, lat, lon) = (loc.id.clone(), loc.lat, loc.lon);
        self.settings.saved_locations.push(loc.clone());
        self.settings.selected_location_id = Some(id);
        save_settings(&self.settings);
        self.spawn_refresh_location(loc);
        self.map.fly_to(lat, lon, Some(6.5));
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
                        if sv.timezone.is_empty() && !bundle.location.timezone.is_empty() {
                            sv.timezone = bundle.location.timezone.clone();
                            save_settings(&self.settings);
                        } else if !sv.timezone.is_empty() {
                            bundle.location.timezone = sv.timezone.clone();
                        }
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
                WorkerOut::CurrentLocation(loc) => {
                    let (lat, lon) = (loc.lat, loc.lon);
                    self.settings
                        .saved_locations
                        .retain(|l| !l.is_current_location && !l.is_same_place(&loc));
                    self.settings.saved_locations.insert(0, loc.clone());
                    self.settings.selected_location_id = Some(loc.id.clone());
                    (self.map.center_lat, self.map.center_lon) = (lat, lon);
                    self.map.prewarm_region(lat, lon);
                    save_settings(&self.settings);
                    self.spawn_refresh_location(loc);
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
            Msg::TogglePreferences => self.show_preferences = !self.show_preferences,
            Msg::RefreshAll => self.spawn_refresh_all(),
            Msg::SelectLocation(id) => {
                (
                    self.settings.selected_location_id,
                    self.expanded_alert_idx,
                    self.selected_day_idx,
                ) = (Some(id.clone()), None, None);
                save_settings(&self.settings);
                if let Some(loc) = self
                    .settings
                    .saved_locations
                    .iter()
                    .find(|l| l.id == id)
                    .cloned()
                {
                    if self.map_expanded {
                        self.map.fly_to(loc.lat, loc.lon, None);
                    } else {
                        (self.map.center_lat, self.map.center_lon) = (loc.lat, loc.lon);
                    }
                    self.map.prewarm_region(loc.lat, loc.lon);
                    if !self.bundles.contains_key(&id) {
                        self.spawn_refresh_location(loc);
                    }
                }
            }
            Msg::RemoveLocation(id) if self.settings.saved_locations.len() > 1 => {
                self.settings.saved_locations.retain(|l| l.id != id);
                if self.settings.selected_location_id.as_deref() == Some(id.as_str()) {
                    self.settings.selected_location_id =
                        self.settings.saved_locations.first().map(|l| l.id.clone());
                }
                save_settings(&self.settings);
            }
            Msg::RemoveLocation(_) => {}
            Msg::DetectLocation => self.spawn_detect_location(),
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
                    self.add_or_select_location(first);
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
                self.add_or_select_location(l);
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
                let mut refresh = false;
                match p {
                    Pref::Theme(m) => self.settings.theme_mode = m,
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
                if refresh {
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
                            self.settings.selected_location_id = Some(id);
                            save_settings(&self.settings);
                        }
                        MapExternalAction::SaveLocation(loc) => self.add_or_select_location(loc),
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
                    self.map_expanded = false;
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
            widget_canvas(WidgetCanvas::Sky(pal, cond, day, phase), Fill, Fill),
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
                .and_then(|b| b.daily.first())
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
            let row_el: Element<'_, Msg> = if total > 1 && !loc.is_current_location && sel {
                let del = icon_btn(
                    UiIcon::Close,
                    10.5,
                    (20, 20),
                    (pal.text_muted, Color::TRANSPARENT, hv),
                    6.0,
                    Msg::RemoveLocation(loc.id.clone()),
                );
                row![
                    sbtn,
                    container(del)
                        .padding(iced::Padding {
                            top: 6.0,
                            right: 4.0,
                            bottom: 0.0,
                            left: 0.0
                        })
                        .align_y(Alignment::Start)
                ]
                .align_y(Alignment::Start)
                .into()
            } else {
                sbtn.into()
            };
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
        let mut content = column![self.view_hero(b, pal)]
            .spacing(16)
            .width(grid_w)
            .align_x(Alignment::Center);
        if !b.alerts.is_empty() {
            content = content.push(self.view_alerts_card(b, pal, grid_w));
        }
        if let Some(mc) = self.view_minutely_card(b, pal, grid_w) {
            content = content.push(mc);
        }
        content = content.push(self.view_hourly_card(b, pal, grid_w, 16));
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
                self.view_hero(b, pal),
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
                self.view_moon_card(pal, cw, th),
                row![
                    self.view_visibility_card(b, pal, u, th),
                    self.view_humidity_card(b, pal, u, th)
                ]
                .spacing(g),
                row![
                    self.view_pressure_card(b, pal, u, th),
                    self.view_averages_card(b, pal, u, th)
                ]
                .spacing(g),
                sp_h(16),
            ]
            .spacing(g)
            .width(cw);
            scrollable(container(col).padding([4, 12]))
                .width(cw + 24.0)
                .height(Fill)
                .style(sleek_scrollable)
                .into()
        } else {
            sp_w(0)
        };
        let map_pane = container(self.map.view_expanded(
            &self.settings.saved_locations,
            &self.bundles,
            self.settings.selected_location_id.as_deref(),
            &self.settings,
            Msg::Map,
        ))
        .width(Fill)
        .height(Fill)
        .padding([0, 12])
        .clip(true);
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
                        let lbl = txt(
                            format!("{} — {}", r.name, r.subtitle()),
                            12.0,
                            Weight::Medium,
                            Color::WHITE,
                        );
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
        let modal = container(
            column![
                head,
                hairline(pal.divider),
                pref_row(
                    "Units",
                    vec![
                        pref_chip(
                            "Celsius (°C)",
                            is_c,
                            if is_c { Msg::Noop } else { Msg::ToggleUnits }
                        ),
                        pref_chip(
                            "Fahrenheit (°F)",
                            !is_c,
                            if !is_c { Msg::Noop } else { Msg::ToggleUnits }
                        )
                    ]
                ),
                pref_row(
                    "Wind",
                    [
                        ("km/h", WindUnit::KmH),
                        ("mph", WindUnit::Mph),
                        ("m/s", WindUnit::Ms),
                        ("knots", WindUnit::Knots)
                    ]
                    .into_iter()
                    .map(|(l, u)| pref_chip(l, s.wind_unit == u, Msg::Pref(Pref::Wind(u))))
                    .collect()
                ),
                pref_row(
                    "Pressure",
                    [("hPa", PressureUnit::HPa), ("inHg", PressureUnit::InHg)]
                        .into_iter()
                        .map(|(l, u)| pref_chip(
                            l,
                            s.pressure_unit == u,
                            Msg::Pref(Pref::Pressure(u))
                        ))
                        .collect()
                ),
                pref_row(
                    "Appearance",
                    [
                        ("Dynamic Sky", ThemeMode::DynamicSky),
                        ("Dark", ThemeMode::Dark),
                        ("Light", ThemeMode::Light)
                    ]
                    .into_iter()
                    .map(|(l, m)| pref_chip(l, s.theme_mode == m, Msg::Pref(Pref::Theme(m))))
                    .collect()
                ),
                pref_row(
                    "Clock & Sky",
                    vec![
                        pref_chip(
                            "24-Hour Time",
                            s.use_24h_time,
                            Msg::Pref(Pref::Use24h(!s.use_24h_time))
                        ),
                        pref_chip(
                            "Sky Motion",
                            s.animate_weather,
                            Msg::Pref(Pref::Animate(!s.animate_weather))
                        )
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
            if stop && app.active_bundle().is_some() && !app.map.needs_fast_tick(app.map_expanded) {
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

    #[test]
    fn persistent_renderer_zoom_in_out_repro() {
        use iced_test::core::{clipboard::Null, renderer::Headless, theme::Base as _};
        use iced_test::runtime::{UserInterface, user_interface::Cache};
        let out_dir = reset_dir("WEATHER_RS_ZOOM_REPRO");
        ensure_fonts_loaded();
        let (mut app, _) = WeatherApp::boot();
        let _ = app.update(Msg::Pref(Pref::Theme(ThemeMode::Dark)));
        pump(&mut app, if out_dir.is_some() { 30 } else { 12 }, 100, true);
        let bk = std::env::var("ICED_TEST_BACKEND").ok();
        let mut r: iced::Renderer =
            iced_test::futures::futures::executor::block_on(<iced::Renderer as Headless>::new(
                fw(Weight::Normal),
                iced::Pixels(16.0),
                bk.as_deref(),
            ))
            .expect("headless renderer");
        let (mut cache, logical, scale) = (Cache::default(), Size::new(1360.0, 900.0), 2.0_f32);
        let phys = Size::new(
            (logical.width * scale) as u32,
            (logical.height * scale) as u32,
        );
        let px = |rgba: &[u8], lx: u32, ly: u32| {
            let o = ((((ly * 2).min(phys.height - 1)) * phys.width + (lx * 2).min(phys.width - 1))
                * 4) as usize;
            (rgba[o], rgba[o + 1], rgba[o + 2])
        };
        let cur = iced::mouse::Cursor::Available(iced::Point::new(980.0, 460.0));
        let mut render =
            |app: &mut WeatherApp, evs: &[iced::Event], name: Option<&str>| -> Vec<u8> {
                let mut msgs = Vec::new();
                let mut ui =
                    UserInterface::build(app.view(), logical, std::mem::take(&mut cache), &mut r);
                if !evs.is_empty() {
                    let _ = ui.update(evs, cur, &mut r, &mut Null, &mut msgs);
                }
                cache = ui.into_cache();
                for m in msgs {
                    let _ = app.update(m);
                }
                let (th, base) = (iced::Theme::Dark, iced::Theme::Dark.base());
                let mut ui =
                    UserInterface::build(app.view(), logical, std::mem::take(&mut cache), &mut r);
                let _ = ui.update(
                    &[iced::Event::Window(window::Event::RedrawRequested(
                        Instant::now(),
                    ))],
                    cur,
                    &mut r,
                    &mut Null,
                    &mut Vec::new(),
                );
                ui.draw(
                    &mut r,
                    &th,
                    &iced_test::core::renderer::Style {
                        text_color: base.text_color,
                    },
                    cur,
                );
                cache = ui.into_cache();
                let rgba = r.screenshot(phys, scale, base.background_color);
                if let (Some(d), Some(n)) = (&out_dir, name)
                    && let Some(img) =
                        ::image::RgbaImage::from_raw(phys.width, phys.height, rgba.clone())
                {
                    let _ = img.save(d.join(format!("{n}.png")));
                }
                rgba
            };
        let no_bleed = |rgba: &[u8], st: &str| {
            let ((lr, lg, lb), (ur, ug, ub)) = (px(rgba, 576, 350), px(rgba, 920, 12));
            assert!(
                lr < 80 && lg < 100 && lb < 135,
                "{st}: left bleed ({lr},{lg},{lb})"
            );
            assert!(
                ur < 80 && ug < 100 && ub < 135,
                "{st}: top bleed ({ur},{ug},{ub})"
            );
        };
        let _ = render(&mut app, &[], None);
        pump(&mut app, 10, 80, false);
        let (br, bg, bb) = px(&render(&mut app, &[], Some("01_bento_ready")), 1335, 575);
        assert!(
            br < 75 && bg < 95 && bb < 130,
            "Bento bleed ({br},{bg},{bb})"
        );
        let _ = app.update(Msg::Map(MapMsg::ExpandMap));
        let _ = render(&mut app, &[], None);
        pump(&mut app, 12, 80, false);
        let (tr, tg, tb) = px(&render(&mut app, &[], Some("02_split_map_z5")), 980, 12);
        assert!(
            tr < 75 && tg < 95 && tb < 130,
            "Split bleed ({tr},{tg},{tb})"
        );
        for (pre, n, dy) in [("03_zoom_in", 6_usize, 3.5_f32), ("05_zoom_out", 7, -4.0)] {
            for i in 1..=n {
                let ev = iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: dy },
                });
                no_bleed(
                    &render(&mut app, &[ev], Some(&format!("{pre}_step{i}"))),
                    pre,
                );
            }
            if out_dir.is_some() {
                for _ in 0..16 {
                    std::thread::sleep(Duration::from_millis(90));
                    let _ = app.update(Msg::Tick);
                    let _ = render(&mut app, &[], None);
                }
            }
            no_bleed(
                &render(
                    &mut app,
                    &[],
                    Some(if dy > 0.0 {
                        "04_zoom_in_settled"
                    } else {
                        "06_zoom_out_settled"
                    }),
                ),
                pre,
            );
        }
        let _ = app.update(Msg::Map(MapMsg::RecenterActive));
        for _ in 0..16 {
            let _ = app.update(Msg::Tick);
        }
        let live_i = app.map.live_frame_idx();
        let _ = app.update(Msg::Map(MapMsg::ScrubRadar(live_i + 7)));
        assert!(app.map.selected_forecast_timestamp().is_some());
        if out_dir.is_some() {
            for _ in 0..14 {
                std::thread::sleep(Duration::from_millis(85));
                let _ = app.update(Msg::Tick);
                let _ = render(&mut app, &[], None);
            }
        }
        no_bleed(
            &render(&mut app, &[], Some("07_future_radar_forecast")),
            "07_future_radar_forecast",
        );
        let _ = app.update(Msg::Map(MapMsg::SelectOverlay(WeatherOverlay::Temperature)));
        let _ = app.update(Msg::Map(MapMsg::ScrubRadar(live_i + 10)));
        if out_dir.is_some() {
            for _ in 0..12 {
                std::thread::sleep(Duration::from_millis(85));
                let _ = app.update(Msg::Tick);
                let _ = render(&mut app, &[], None);
            }
        }
        no_bleed(
            &render(&mut app, &[], Some("08_future_temp_forecast")),
            "08_future_temp_forecast",
        );
        let _ = app.update(Msg::Map(MapMsg::JumpToLive));
        assert!(app.map.selected_forecast_timestamp().is_none());
    }
}
