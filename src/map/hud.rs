use super::overlays::CityBadge;
use super::{BaseMapStyle, MapEngine, MapMsg, MapTilesCanvas, MapVectorsCanvas, WeatherOverlay};
use crate::icons::{
    UiIcon, aqi_to_color, icon_btn, icon_svg, pressure_to_color, temp_badge_color, temp_to_color,
    weather_icon_svg,
};
use crate::model::{Location, Settings, WeatherBundle, compass_direction};
use crate::style::{flat_box, hairline, hud_pill, pbtn, rgb8, rgba8, sp_h, sp_w, txt};
use iced::font::Weight;
use iced::mouse;
use iced::widget::canvas::gradient::Linear;
use iced::widget::canvas::{self, Action, Canvas, Event, Frame, Geometry, Path, Stroke};
use iced::widget::{column, container, mouse_area, row, stack, text};
use iced::{Alignment, Color, Element, Fill, Length, Point, Rectangle, Renderer, Size, Theme};
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct HudColors {
    bg: Color,
    border: Color,
    divider: Color,
    fg: Color,
    sub: Color,
    accent: Color,
    hover: Color,
    active_wash: Color,
    dark: bool,
}
impl HudColors {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: rgba8(24, 30, 42, 0.88),
                border: rgba8(255, 255, 255, 0.16),
                divider: rgba8(255, 255, 255, 0.12),
                fg: Color::WHITE,
                sub: rgba8(216, 226, 242, 0.76),
                accent: rgb8(72, 186, 255),
                hover: rgba8(255, 255, 255, 0.12),
                active_wash: rgba8(10, 132, 255, 0.24),
                dark: true,
            }
        } else {
            Self {
                bg: rgba8(249, 251, 254, 0.92),
                border: rgba8(18, 30, 50, 0.12),
                divider: rgba8(18, 30, 50, 0.09),
                fg: rgb8(20, 28, 42),
                sub: rgba8(56, 68, 88, 0.78),
                accent: rgb8(0, 122, 255),
                hover: rgba8(20, 36, 64, 0.08),
                active_wash: rgba8(0, 122, 255, 0.13),
                dark: false,
            }
        }
    }
}

fn overlay_icon(ov: WeatherOverlay) -> UiIcon {
    match ov {
        WeatherOverlay::RadarTimeline => UiIcon::Droplet,
        WeatherOverlay::Temperature => UiIcon::Thermometer,
        WeatherOverlay::AirQuality => UiIcon::Air,
        WeatherOverlay::WindFlow => UiIcon::Wind,
        WeatherOverlay::InfraredClouds => UiIcon::Cloud,
        WeatherOverlay::Pressure => UiIcon::Gauge,
        WeatherOverlay::NexradUsRadar => UiIcon::Radar,
    }
}

struct RampBar(Vec<Color>);
impl<M> canvas::Program<M> for RampBar {
    type State = ();
    fn draw(
        &self,
        _: &(),
        r: &Renderer,
        _: &Theme,
        b: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut f = Frame::new(r, b.size());
        let mut g = Linear::new(Point::ORIGIN, Point::new(0.0, b.height));
        let last = self.0.len().saturating_sub(1).max(1) as f32;
        for (i, c) in self.0.iter().enumerate() {
            g = g.add_stop(i as f32 / last, *c);
        }
        f.fill(
            &Path::rounded_rectangle(Point::ORIGIN, b.size(), 3.0.into()),
            g,
        );
        vec![f.into_geometry()]
    }
}

#[derive(Default)]
struct ScrubberState {
    dragging: bool,
    hovered: bool,
}

struct TimelineScrubber<M> {
    min_i: f32,
    max_i: f32,
    live_i: f32,
    pos: f32,
    hud: HudColors,
    wrap: fn(MapMsg) -> M,
}

impl<M> TimelineScrubber<M> {
    const PAD_X: f32 = 8.0;

    fn frac_for(&self, val: f32) -> f32 {
        let span = (self.max_i - self.min_i).max(1e-3);
        ((val - self.min_i) / span).clamp(0.0, 1.0)
    }

    fn val_from_x(&self, x: f32, w: f32) -> f32 {
        let tw = (w - Self::PAD_X * 2.0).max(1.0);
        let f = ((x - Self::PAD_X) / tw).clamp(0.0, 1.0);
        self.min_i + f * (self.max_i - self.min_i)
    }
}

impl<M: Clone> canvas::Program<M> for TimelineScrubber<M> {
    type State = ScrubberState;

    fn update(
        &self,
        st: &mut ScrubberState,
        event: &Event,
        b: Rectangle,
        cur: mouse::Cursor,
    ) -> Option<Action<M>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cur.position_in(b)?;
                st.dragging = true;
                let v = self.val_from_x(p.x, b.width);
                Some(Action::publish((self.wrap)(MapMsg::ScrubRadarPos(v))).and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                let lx = position.x - b.x;
                let v = self.val_from_x(lx, b.width);
                Some(Action::publish((self.wrap)(MapMsg::ScrubRadarPos(v))).and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hov = cur.position_in(b).is_some();
                if st.hovered != hov {
                    st.hovered = hov;
                    Some(Action::request_redraw())
                } else {
                    None
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if st.dragging => {
                st.dragging = false;
                Some(Action::capture())
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        st: &ScrubberState,
        b: Rectangle,
        cur: mouse::Cursor,
    ) -> mouse::Interaction {
        if st.dragging {
            mouse::Interaction::Grabbing
        } else if cur.position_in(b).is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        st: &ScrubberState,
        r: &Renderer,
        _: &Theme,
        b: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut f = Frame::new(r, b.size());
        let cy = b.height * 0.5;
        let x0 = Self::PAD_X;
        let tw = (b.width - Self::PAD_X * 2.0).max(8.0);
        let th = 5.0_f32;
        let track_bg = if self.hud.dark {
            rgba8(255, 255, 255, 0.16)
        } else {
            rgba8(20, 32, 52, 0.13)
        };
        let past_tint = if self.hud.dark {
            rgba8(180, 200, 226, 0.34)
        } else {
            rgba8(80, 98, 124, 0.30)
        };
        // Full rounded capsule rail.
        f.fill(
            &Path::rounded_rectangle(
                Point::new(x0, cy - th * 0.5),
                Size::new(tw, th),
                (th * 0.5).into(),
            ),
            track_bg,
        );
        let live_f = self.frac_for(self.live_i);
        let pos_f = self.frac_for(self.pos);
        let live_x = x0 + live_f * tw;
        let thumb_x = x0 + pos_f * tw;
        // Subtle forecast tick pips along the rail (Apple Weather 12-hour scrubber feel).
        let pip_col = if self.hud.dark {
            rgba8(255, 255, 255, 0.18)
        } else {
            rgba8(20, 32, 52, 0.15)
        };
        for k in 1..6 {
            let fx = live_f + (1.0 - live_f) * (k as f32 / 6.0);
            let px = x0 + fx * tw;
            f.fill(
                &Path::rounded_rectangle(
                    Point::new(px - 0.75, cy - 3.5),
                    Size::new(1.5, 7.0),
                    0.75.into(),
                ),
                pip_col,
            );
        }
        // Active fill segment: past radar in subtle cool slate, future forecast in Apple Sky Blue.
        if pos_f > 0.002 {
            let past_end = thumb_x.min(live_x);
            if past_end > x0 + 1.0 {
                f.fill(
                    &Path::rounded_rectangle(
                        Point::new(x0, cy - th * 0.5),
                        Size::new(past_end - x0, th),
                        (th * 0.5).into(),
                    ),
                    past_tint,
                );
            }
            if thumb_x > live_x + 1.0 {
                let seg_start = live_x.max(x0);
                f.fill(
                    &Path::rounded_rectangle(
                        Point::new(seg_start, cy - th * 0.5),
                        Size::new(thumb_x - seg_start, th),
                        (th * 0.5).into(),
                    ),
                    self.hud.accent,
                );
            }
        }
        // Crisp vertical "Now" marker notch on the rail when past frames exist.
        if live_f > 0.02 && live_f < 0.98 {
            let now_col = if self.hud.dark {
                rgba8(255, 255, 255, 0.78)
            } else {
                rgba8(24, 34, 52, 0.62)
            };
            f.fill(
                &Path::rounded_rectangle(
                    Point::new(live_x - 1.25, cy - 5.5),
                    Size::new(2.5, 11.0),
                    1.25.into(),
                ),
                now_col,
            );
        }
        // Apple circular white thumb with soft elevation shadow & hover/drag scale.
        let tr = if st.dragging {
            8.0
        } else if st.hovered {
            7.4
        } else {
            6.6
        };
        let tc = Point::new(thumb_x, cy);
        f.fill(
            &Path::circle(Point::new(tc.x, tc.y + 1.2), tr + 1.4),
            rgba8(0, 0, 0, if self.hud.dark { 0.36 } else { 0.20 }),
        );
        f.fill(&Path::circle(tc, tr), Color::WHITE);
        f.stroke(
            &Path::circle(tc, tr),
            Stroke::default()
                .with_width(1.0)
                .with_color(if self.hud.dark {
                    rgba8(255, 255, 255, 0.35)
                } else {
                    rgba8(18, 30, 50, 0.16)
                }),
        );
        vec![f.into_geometry()]
    }
}

fn shield<'a, M: Clone + 'a>(el: Element<'a, M>, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
    mouse_area(el)
        .interaction(mouse::Interaction::Idle)
        .on_press(wrap(MapMsg::HudConsume))
        .on_right_press(wrap(MapMsg::HudConsume))
        .on_scroll(move |_| wrap(MapMsg::HudConsume))
        .into()
}

fn fmt_dm(dm: i64, compact: bool) -> String {
    let (s, am) = (if dm > 0 { "+" } else { "-" }, dm.abs());
    if am < 60 {
        format!("{s}{am}m")
    } else if compact || am % 60 == 0 {
        format!("{s}{}h", (am + if compact { 29 } else { 0 }) / 60)
    } else {
        format!("{s}{}h {}m", am / 60, am % 60)
    }
}

fn fmt_apple_day_time(loc: Option<&Location>, ts: i64, live_ts: i64, h24: bool) -> String {
    match loc {
        Some(l) => {
            let (d, ld) = (l.local_date(ts), l.local_date(live_ts));
            let day = if d == ld {
                "Today".to_string()
            } else {
                d.format("%a").to_string()
            };
            format!("{day}, {}", l.format_local_time(ts, h24))
        }
        None => chrono::DateTime::from_timestamp(ts, 0)
            .map_or_else(String::new, |d| d.format("%a, %H:%M UTC").to_string()),
    }
}

impl MapEngine {
    pub fn view_mini_card<'a, M: Clone + 'a>(
        &'a self,
        size: Size,
        loc: Option<&'a Location>,
        bundles: &'a HashMap<String, WeatherBundle>,
        settings: &'a Settings,
        light: bool,
        on_expand: M,
    ) -> Element<'a, M> {
        let (clat, clon) = loc.map_or((self.center_lat, self.center_lon), |l| (l.lat, l.lon));
        let pin = loc.map(|l| {
            let temp = bundles.get(&l.id).map(|b| b.current.temp_c);
            CityBadge {
                title: String::new(),
                lat: l.lat,
                lon: l.lon,
                val: temp.map_or_else(|| "--".into(), |c| settings.format_temp(c)),
                sub: String::new(),
                col: temp.map_or_else(|| rgb8(46, 108, 186), temp_badge_color),
                active: true,
                is_current_location: l.is_current_location,
                wind_deg: None,
            }
        });
        let st = if light {
            BaseMapStyle::Terrain
        } else {
            BaseMapStyle::AppleDark
        };
        let tiles_prog = MapTilesCanvas {
            engine: self,
            live_only: true,
            cam: (clat, clon, 5.5),
            base_style: st,
            overlay: WeatherOverlay::RadarTimeline,
            corner_r: 10.0,
            bundles,
        };
        let vectors_prog = MapVectorsCanvas::<M> {
            engine: self,
            interactive: false,
            cam: (clat, clon, 5.5),
            base_style: st,
            overlay: WeatherOverlay::RadarTimeline,
            badges: pin.into_iter().collect(),
            wrap: None,
        };
        let border_col = if light {
            rgba8(20, 35, 60, 0.12)
        } else {
            rgba8(255, 255, 255, 0.12)
        };
        let layered = stack![
            sp_w(0),
            Canvas::new(tiles_prog)
                .width(size.width)
                .height(size.height),
            Canvas::new(vectors_prog)
                .width(size.width)
                .height(size.height),
            container(sp_w(Fill))
                .width(size.width)
                .height(size.height)
                .style(move |_| container::Style {
                    border: iced::Border {
                        color: border_col,
                        width: 1.0,
                        radius: 10.0.into(),
                    },
                    ..Default::default()
                }),
        ]
        .width(size.width)
        .height(size.height)
        .clip(true);
        mouse_area(
            container(layered)
                .width(size.width)
                .height(size.height)
                .clip(true),
        )
        .interaction(mouse::Interaction::Pointer)
        .on_press(on_expand)
        .into()
    }

    pub fn view_expanded<'a, M: Clone + 'a>(
        &'a self,
        saved: &'a [Location],
        bundles: &'a HashMap<String, WeatherBundle>,
        active_id: Option<&'a str>,
        settings: &'a Settings,
        wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let base = if self.overlay == WeatherOverlay::WindFlow
            && self.base_style == BaseMapStyle::CleanLight
        {
            BaseMapStyle::AppleDark
        } else {
            self.base_style
        };
        let fts = self.selected_forecast_timestamp();
        let badges: Vec<CityBadge> = saved
            .iter()
            .map(|loc| {
                let act = active_id == Some(loc.id.as_str());
                let b = bundles.get(&loc.id);
                let h = fts.and_then(|ts| {
                    b.and_then(|b| {
                        b.hourly
                            .iter()
                            .min_by_key(|h| (h.timestamp_unix - ts).abs())
                    })
                });
                let (val, sub, col, wind_deg) = match self.overlay {
                    WeatherOverlay::AirQuality => {
                        if let Some(a) = b.and_then(|b| b.air_quality.as_ref()).map(|a| a.us_aqi) {
                            (a.to_string(), String::new(), aqi_to_color(a), None)
                        } else {
                            ("--".into(), String::new(), rgb8(96, 110, 130), None)
                        }
                    }
                    WeatherOverlay::WindFlow => {
                        let (ws, wd) = h
                            .map(|h| (h.wind_speed_mps, h.wind_dir_deg))
                            .or_else(|| {
                                b.map(|b| (b.current.wind_speed_mps, b.current.wind_dir_deg))
                            })
                            .unwrap_or((0.0, 0.0));
                        let (v, u) = if b.is_some() {
                            settings.wind_value_and_unit(ws)
                        } else {
                            ("--".into(), "")
                        };
                        (
                            v,
                            if act && b.is_some() {
                                format!("{} {u}", compass_direction(wd))
                            } else {
                                String::new()
                            },
                            Color::WHITE,
                            b.map(|_| wd),
                        )
                    }
                    _ => {
                        let tc = h.map(|h| h.temp_c).or_else(|| b.map(|b| b.current.temp_c));
                        let t = tc.map_or_else(
                            || "--".into(),
                            |c| {
                                if act {
                                    settings.format_temp(c)
                                } else {
                                    settings.format_temp_bare(c)
                                }
                            },
                        );
                        (
                            t,
                            String::new(),
                            tc.map_or_else(|| rgb8(68, 86, 110), temp_badge_color),
                            None,
                        )
                    }
                };
                CityBadge {
                    title: if loc.is_current_location {
                        "My Location".into()
                    } else {
                        loc.name.clone()
                    },
                    lat: loc.lat,
                    lon: loc.lon,
                    val,
                    sub,
                    col,
                    active: act,
                    is_current_location: loc.is_current_location,
                    wind_deg,
                }
            })
            .collect();
        let tiles_prog = MapTilesCanvas {
            engine: self,
            live_only: false,
            cam: (self.center_lat, self.center_lon, self.zoom),
            base_style: base,
            overlay: self.overlay,
            corner_r: 13.0,
            bundles,
        };
        let vectors_prog = MapVectorsCanvas {
            engine: self,
            interactive: true,
            cam: (self.center_lat, self.center_lon, self.zoom),
            base_style: base,
            overlay: self.overlay,
            badges,
            wrap: Some(wrap),
        };
        let hud = HudColors::new(base.is_dark_hud(self.overlay));
        let pill =
            |el: Element<'a, M>, r: f32| container(el).style(hud_pill(hud.bg, hud.border, r));
        let hbtn =
            |ic, sz, dim: (u32, u32), cols, r, msg| icon_btn(ic, sz, dim, cols, r, wrap(msg));
        let norm = (hud.fg, Color::TRANSPARENT, hud.hover);
        let close = pill(
            hbtn(UiIcon::Close, 13.5, (34, 34), norm, 17.0, MapMsg::CloseMap),
            17.0,
        );
        let mut left = column![
            shield(close.into(), wrap),
            shield(self.view_legend(hud, settings), wrap)
        ]
        .spacing(10);
        if self.inspect.coord.is_some() {
            left = left.push(shield(self.view_inspector(hud, settings, wrap), wrap));
        }
        let lcol = if self.show_layer_menu {
            (Color::WHITE, rgb8(10, 132, 255), rgb8(28, 146, 255))
        } else {
            norm
        };
        // Exact Apple Weather macOS right-rail: unified Locate + Layers capsule, then Zoom +/- capsule.
        let nav_col = container(
            column![
                hbtn(
                    UiIcon::Locate,
                    14.5,
                    (32, 32),
                    norm,
                    8.0,
                    MapMsg::RecenterActive
                ),
                container(sp_w(20))
                    .height(1)
                    .style(flat_box(hud.divider, 0.0)),
                hbtn(
                    UiIcon::Layers,
                    15.0,
                    (32, 32),
                    lcol,
                    8.0,
                    MapMsg::ToggleLayerMenu
                ),
            ]
            .spacing(2)
            .align_x(Alignment::Center),
        )
        .padding(2);
        let zcol = container(
            column![
                hbtn(
                    UiIcon::Plus,
                    14.0,
                    (32, 30),
                    norm,
                    8.0,
                    MapMsg::ZoomStep(1.0)
                ),
                container(sp_w(20))
                    .height(1)
                    .style(flat_box(hud.divider, 0.0)),
                hbtn(
                    UiIcon::Minus,
                    14.0,
                    (32, 30),
                    norm,
                    8.0,
                    MapMsg::ZoomStep(-1.0)
                ),
            ]
            .spacing(2)
            .align_x(Alignment::Center),
        )
        .padding(2);
        let ctrls = shield(
            column![pill(nav_col.into(), 10.0), pill(zcol.into(), 10.0)]
                .spacing(10)
                .align_x(Alignment::End)
                .into(),
            wrap,
        );
        let right: Element<'a, M> = if self.show_layer_menu {
            row![shield(self.view_layer_menu(hud, wrap), wrap), ctrls]
                .spacing(10)
                .align_y(Alignment::Start)
                .into()
        } else {
            ctrls
        };
        let active_loc = active_id.and_then(|id| saved.iter().find(|l| l.id == id));
        let hud_el = column![
            row![left, sp_w(Fill), right]
                .width(Fill)
                .align_y(Alignment::Start),
            sp_h(Fill),
            self.view_timeline(hud, active_loc, settings, wrap)
        ]
        .width(Fill)
        .height(Fill)
        .padding(14);
        stack![
            sp_w(0),
            Canvas::new(tiles_prog).width(Fill).height(Fill),
            Canvas::new(vectors_prog).width(Fill).height(Fill),
            hud_el
        ]
        .width(Fill)
        .height(Fill)
        .clip(true)
        .into()
    }

    fn view_legend<'a, M: 'a>(&self, hud: HudColors, s: &Settings) -> Element<'a, M> {
        let (title, labels, stops): (&str, Vec<String>, Vec<Color>) = match self.overlay {
            WeatherOverlay::RadarTimeline | WeatherOverlay::NexradUsRadar => (
                "Precipitation",
                ["Extreme", "Heavy", "Moderate", "Light"]
                    .map(String::from)
                    .into(),
                vec![
                    rgb8(245, 224, 66),
                    rgb8(235, 64, 52),
                    rgb8(148, 82, 235),
                    rgb8(56, 168, 255),
                ],
            ),
            WeatherOverlay::Temperature => (
                "Temperature",
                [40.0, 28.0, 16.0, 4.0, -10.0, -25.0]
                    .map(|c| s.format_temp(c))
                    .into(),
                [40.0, 25.0, 12.0, -15.0]
                    .map(|c| temp_to_color(c, 1.0))
                    .into(),
            ),
            WeatherOverlay::AirQuality => (
                "AQI (US)",
                ["500", "400", "300", "200", "100", "0"]
                    .map(String::from)
                    .into(),
                vec![
                    rgb8(142, 28, 62),
                    rgb8(175, 82, 222),
                    rgb8(255, 59, 48),
                    rgb8(255, 204, 0),
                    rgb8(52, 199, 89),
                ],
            ),
            WeatherOverlay::WindFlow => (
                "Wind",
                [28.0, 18.0, 9.0, 0.0].map(|m| s.format_wind(m)).into(),
                vec![
                    rgb8(255, 112, 82),
                    rgb8(255, 214, 92),
                    rgb8(120, 240, 215),
                    rgb8(145, 210, 255),
                ],
            ),
            WeatherOverlay::Pressure => (
                "Pressure",
                [1040.0, 1025.0, 1013.0, 995.0]
                    .map(|p| s.format_pressure(p))
                    .into(),
                [1040.0, 1020.0, 1005.0, 985.0]
                    .map(|p| pressure_to_color(p, 1.0))
                    .into(),
            ),
            WeatherOverlay::InfraredClouds => (
                "IR Clouds",
                ["Deep", "Mid", "Thin", "Clear"].map(String::from).into(),
                vec![
                    Color::WHITE,
                    rgb8(180, 210, 245),
                    rgb8(100, 130, 170),
                    rgb8(45, 60, 82),
                ],
            ),
        };
        let bh = (labels.len() as f32 * 17.0).max(66.0);
        let col = column(
            labels
                .into_iter()
                .map(|l| {
                    container(txt(l, 10.5, Weight::Medium, hud.sub))
                        .height(17.0)
                        .align_y(Alignment::Center)
                        .into()
                })
                .collect::<Vec<_>>(),
        );
        container(
            column![
                row![
                    icon_svg(overlay_icon(self.overlay), 12.0, hud.sub),
                    txt(title, 11.5, Weight::Semibold, hud.fg),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
                row![Canvas::new(RampBar(stops)).width(6.0).height(bh), col]
                    .spacing(8)
                    .align_y(Alignment::Center)
            ]
            .spacing(7),
        )
        .padding([10, 12])
        .style(hud_pill(hud.bg, hud.border, 12.0))
        .into()
    }

    fn view_layer_menu<'a, M: Clone + 'a>(
        &self,
        hud: HudColors,
        wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let layer_row = |ic: Option<UiIcon>,
                         lbl: &'static str,
                         sz: f32,
                         act: bool,
                         msg: MapMsg|
         -> Element<'a, M> {
            let fg = if act { hud.accent } else { hud.fg };
            let cols = if act {
                (hud.active_wash, hud.active_wash, fg)
            } else {
                (Color::TRANSPARENT, hud.hover, fg)
            };
            let mut r = row![].spacing(8).align_y(Alignment::Center);
            if let Some(icon) = ic {
                r = r.push(icon_svg(icon, 14.0, if act { hud.accent } else { hud.sub }));
            }
            r = r.push(
                txt(
                    lbl,
                    sz,
                    if act {
                        Weight::Semibold
                    } else {
                        Weight::Medium
                    },
                    fg,
                )
                .width(Fill),
            );
            if act {
                r = r.push(icon_svg(UiIcon::Check, 13.0, hud.accent));
            }
            pbtn(r, [6, 9], cols, 8.0, wrap(msg)).width(Fill).into()
        };
        let mut items: Vec<Element<'a, M>> = vec![
            container(txt("WEATHER LAYER", 9.5, Weight::Bold, hud.sub))
                .padding([2, 8])
                .into(),
        ];
        items.extend(WeatherOverlay::ALL.map(|ov| {
            layer_row(
                Some(overlay_icon(ov)),
                ov.label(),
                12.5,
                self.overlay == ov,
                MapMsg::SelectOverlay(ov),
            )
        }));
        items.push(container(hairline(hud.divider)).padding([4, 4]).into());
        items.push(
            container(txt("MAP STYLE", 9.5, Weight::Bold, hud.sub))
                .padding([2, 8])
                .into(),
        );
        items.extend(BaseMapStyle::ALL.map(|st| {
            layer_row(
                None,
                st.label(),
                12.0,
                self.base_style == st,
                MapMsg::SelectBaseStyle(st),
            )
        }));
        container(column(items).spacing(2).width(198))
            .padding(8)
            .style(hud_pill(hud.bg, hud.border, 13.0))
            .into()
    }

    fn view_timeline<'a, M: Clone + 'a>(
        &self,
        hud: HudColors,
        loc: Option<&Location>,
        s: &Settings,
        wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let Some(m) = self
            .manifest
            .as_ref()
            .filter(|m| Self::overlay_supports_timeline(self.overlay) && !m.frames.is_empty())
        else {
            return sp_h(0);
        };
        let (max_i, live_i) = (
            m.frames.len() - 1,
            self.live_frame_idx().min(m.frames.len() - 1),
        );
        let min_i = if self.overlay == WeatherOverlay::RadarTimeline {
            0
        } else {
            live_i
        };
        if min_i >= max_i {
            return sp_h(0);
        }
        let pos = self.radar_play_pos.clamp(min_i as f32, max_i as f32);
        let (cur_i, i0) = (
            (pos.round() as usize).clamp(min_i, max_i),
            (pos.floor() as usize).clamp(min_i, max_i),
        );
        let i1 = (i0 + 1).min(max_i);
        let ts = (m.frames[i0].time_unix as f64
            + (m.frames[i1].time_unix - m.frames[i0].time_unix) as f64
                * (pos - i0 as f32).clamp(0.0, 1.0) as f64)
            .round() as i64;
        let (live_ts, dm) = (
            m.frames[live_i].time_unix,
            ((ts - m.frames[live_i].time_unix) as f64 / 60.0).round() as i64,
        );
        let future = m.frames[cur_i].is_nowcast || cur_i > live_i;
        let is_live = cur_i == live_i || (!future && dm.abs() <= 4);
        let (status, scol) = if is_live {
            (format!("{} • Live", self.overlay.label()), hud.fg)
        } else if future {
            (
                format!("12-Hour Forecast • {}", fmt_dm(dm.max(1), false)),
                hud.accent,
            )
        } else {
            (format!("Past Radar • {}", fmt_dm(dm, false)), hud.fg)
        };
        let tstr = fmt_apple_day_time(loc, ts, live_ts, s.use_24h_time);
        let (al, ac, abg, ahv, amsg) = if cur_i != live_i {
            (
                "Now",
                hud.fg,
                hud.hover,
                hud.active_wash,
                MapMsg::JumpToLive,
            )
        } else {
            let p2h = m
                .frames
                .iter()
                .position(|f| f.is_nowcast && f.time_unix >= live_ts + 105 * 60)
                .unwrap_or(max_i);
            (
                "+2h ›",
                hud.accent,
                hud.active_wash,
                rgba8(10, 132, 255, 0.28),
                MapMsg::ScrubRadar(p2h),
            )
        };
        let tick = |i: usize| {
            if i == live_i {
                "Now".into()
            } else {
                fmt_dm(
                    ((m.frames[i].time_unix - live_ts) as f64 / 60.0).round() as i64,
                    true,
                )
            }
        };
        let (ma, mb, wl, wr) = if min_i < live_i && live_i < max_i {
            (
                live_i,
                live_i + (max_i - live_i) / 2,
                ((live_i - min_i).max(1) * 2) as u16,
                (max_i - live_i).max(1) as u16,
            )
        } else {
            let sp = max_i - min_i;
            (min_i + sp / 3, min_i + (sp * 2) / 3, 1, 1)
        };
        let tlbl = |i: usize, bold: bool, col: Color| {
            txt(
                tick(i),
                10.0,
                if bold {
                    Weight::Semibold
                } else {
                    Weight::Normal
                },
                col,
            )
        };
        let ticks = row![
            tlbl(
                min_i,
                min_i == live_i,
                if min_i == live_i { hud.fg } else { hud.sub }
            ),
            sp_w(Length::FillPortion(wl)),
            tlbl(ma, ma == live_i, if ma == cur_i { hud.fg } else { hud.sub }),
            sp_w(Length::FillPortion(wr)),
            text(tick(mb)).size(10.0).color(hud.sub),
            sp_w(Length::FillPortion(wr)),
            txt(tick(max_i), 10.0, Weight::Semibold, hud.accent),
        ]
        .width(Fill)
        .align_y(Alignment::Center);
        let play_bg = if hud.dark {
            rgba8(255, 255, 255, 0.14)
        } else {
            rgba8(20, 32, 52, 0.08)
        };
        let play_hv = if hud.dark {
            rgba8(255, 255, 255, 0.24)
        } else {
            rgba8(20, 32, 52, 0.15)
        };
        let play = icon_btn(
            if self.radar_playing {
                UiIcon::Pause
            } else {
                UiIcon::Play
            },
            13.0,
            (32, 32),
            (hud.fg, play_bg, play_hv),
            16.0,
            wrap(MapMsg::ToggleRadarPlay),
        );
        let status_row = if is_live {
            row![
                container(sp_w(6))
                    .width(6)
                    .height(6)
                    .style(flat_box(rgb8(52, 199, 89), 3.0)),
                txt(status, 12.0, Weight::Semibold, scol),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
        } else {
            row![txt(status, 12.0, Weight::Semibold, scol)].align_y(Alignment::Center)
        };
        let top = row![
            play,
            column![status_row, txt(tstr, 10.5, Weight::Medium, hud.sub)]
                .spacing(1)
                .align_x(Alignment::Center)
                .width(Fill),
            pbtn(
                txt(al, 11.0, Weight::Semibold, ac),
                [5, 10],
                (abg, ahv, ac),
                12.0,
                wrap(amsg)
            )
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let scrubber = Canvas::new(TimelineScrubber {
            min_i: min_i as f32,
            max_i: max_i as f32,
            live_i: live_i as f32,
            pos,
            hud,
            wrap,
        })
        .width(Fill)
        .height(20);
        let card = shield(
            container(column![top, scrubber, ticks].spacing(3).width(Fill))
                .width(Fill)
                .max_width(430)
                .padding([10, 14])
                .style(hud_pill(hud.bg, hud.border, 15.0))
                .into(),
            wrap,
        );
        container(card).width(Fill).center_x(Fill).into()
    }

    fn view_inspector<'a, M: Clone + 'a>(
        &self,
        hud: HudColors,
        s: &Settings,
        wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let close = pbtn(
            icon_svg(UiIcon::Close, 11.5, hud.sub),
            [4, 4],
            (Color::TRANSPARENT, hud.hover, hud.fg),
            10.0,
            wrap(MapMsg::CloseInspector),
        );
        let body: Element<'a, M> = if self.inspect.loading {
            row![
                txt("Loading weather…", 12.0, Weight::Medium, hud.fg).width(Fill),
                close
            ]
            .align_y(Alignment::Center)
            .into()
        } else if let Some(b) = &self.inspect.bundle {
            let sub = if !b.location.region.is_empty() {
                format!("{}, {}", b.location.region, b.location.country_code)
            } else {
                format!("{:.2}°, {:.2}°", b.location.lat, b.location.lon)
            };
            let head = row![
                column![
                    txt(b.location.name.clone(), 13.5, Weight::Semibold, hud.fg),
                    txt(sub, 10.5, Weight::Normal, hud.sub)
                ]
                .spacing(1)
                .width(Fill),
                close
            ]
            .align_y(Alignment::Start);
            let hl = b.daily.first().map(|d| {
                format!(
                    "H:{}  L:{}",
                    s.format_temp(d.temp_max_c),
                    s.format_temp(d.temp_min_c)
                )
            });
            let mid = row![
                weather_icon_svg(b.current.condition, b.current.is_day, 30.0),
                txt(
                    s.format_temp(b.current.temp_c),
                    28.0,
                    Weight::Medium,
                    hud.fg
                ),
                column![
                    txt(b.current.condition.label(), 11.5, Weight::Semibold, hud.fg),
                    txt(
                        hl.unwrap_or_else(|| format!(
                            "Feels {}",
                            s.format_temp(b.current.feels_like_c)
                        )),
                        10.5,
                        Weight::Medium,
                        hud.sub,
                    ),
                ]
                .spacing(1),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            let meta = row![
                icon_svg(UiIcon::Wind, 11.5, hud.sub),
                txt(
                    s.format_wind(b.current.wind_speed_mps),
                    10.5,
                    Weight::Medium,
                    hud.sub
                ),
                sp_w(6),
                icon_svg(UiIcon::Gauge, 11.5, hud.sub),
                txt(
                    s.format_pressure(b.current.pressure_hpa),
                    10.5,
                    Weight::Medium,
                    hud.sub
                ),
            ]
            .spacing(4)
            .align_y(Alignment::Center);
            let add = pbtn(
                container(txt(
                    "+ Add to Locations",
                    11.5,
                    Weight::Semibold,
                    Color::WHITE,
                ))
                .center_x(Fill),
                [6, 10],
                (rgb8(0, 122, 255), rgb8(28, 144, 255), Color::WHITE),
                9.0,
                wrap(MapMsg::SaveInspectedLocation),
            )
            .width(Fill);
            column![head, mid, meta, add].spacing(8).into()
        } else {
            row![
                text(
                    self.inspect
                        .error
                        .clone()
                        .unwrap_or_else(|| "Unable to load weather for this point.".into())
                )
                .size(11.0)
                .color(rgb8(235, 78, 72))
                .width(Fill),
                close
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        };
        container(body)
            .width(232)
            .padding(12)
            .style(hud_pill(hud.bg, hud.border, 14.0))
            .into()
    }
}
