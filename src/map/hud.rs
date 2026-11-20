use super::overlays::CityBadge;
use super::{BaseMapStyle, MapCanvas, MapEngine, MapMsg, WeatherOverlay};
use crate::icons::{
    UiIcon, aqi_to_color, icon_btn, icon_svg, pressure_to_color, temp_to_color, weather_icon_svg,
};
use crate::model::{Location, Settings, WeatherBundle, compass_direction};
use crate::style::{flat_box, hairline, hud_pill, pbtn, rgb8, rgba8, sp_h, sp_w, txt};
use iced::font::Weight;
use iced::mouse;
use iced::widget::canvas::gradient::Linear;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path};
use iced::widget::{column, container, mouse_area, row, slider, stack, text};
use iced::{Alignment, Color, Element, Fill, Length, Point, Rectangle, Renderer, Size, Theme};
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct HudColors {
    bg: Color,
    border: Color,
    fg: Color,
    sub: Color,
}
impl HudColors {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: rgba8(28, 36, 48, 0.88),
                border: rgba8(255, 255, 255, 0.16),
                fg: Color::WHITE,
                sub: rgba8(215, 225, 240, 0.78),
            }
        } else {
            Self {
                bg: rgba8(252, 253, 255, 0.90),
                border: rgba8(20, 32, 52, 0.14),
                fg: rgb8(24, 32, 46),
                sub: rgba8(56, 68, 86, 0.82),
            }
        }
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
            &Path::rounded_rectangle(Point::ORIGIN, b.size(), 2.5.into()),
            g,
        );
        vec![f.into_geometry()]
    }
}

fn shield<'a, M: Clone + 'a>(el: Element<'a, M>, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
    mouse_area(el)
        .interaction(mouse::Interaction::Idle)
        .on_press(wrap(MapMsg::HudConsume))
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
        let pin = loc.map(|l| CityBadge {
            title: String::new(),
            lat: l.lat,
            lon: l.lon,
            val: bundles.get(&l.id).map_or_else(
                || "--".into(),
                |b| format!("{:.0}°", settings.convert_temp(b.current.temp_c).round()),
            ),
            sub: String::new(),
            col: rgb8(46, 108, 186),
            active: true,
        });
        let st = if light {
            BaseMapStyle::Terrain
        } else {
            BaseMapStyle::AppleDark
        };
        let prog = MapCanvas::<M> {
            engine: self,
            interactive: false,
            live_only: true,
            cam: (clat, clon, 5.5),
            base_style: st,
            overlay: WeatherOverlay::RadarTimeline,
            corner_r: 9.0,
            bundles,
            badges: pin.into_iter().collect(),
            wrap: None,
        };
        let cv = Canvas::new(prog).width(size.width).height(size.height);
        mouse_area(
            container(cv)
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
                let (val, sub, col) = match self.overlay {
                    WeatherOverlay::AirQuality => {
                        let a = b
                            .and_then(|b| b.air_quality.as_ref())
                            .map_or(28, |a| a.us_aqi);
                        (a.to_string(), String::new(), aqi_to_color(a))
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
                            ("0".into(), "")
                        };
                        (
                            v,
                            if act && b.is_some() {
                                format!("{} {u}", compass_direction(wd))
                            } else {
                                String::new()
                            },
                            Color::WHITE,
                        )
                    }
                    _ => {
                        let t = h
                            .map(|h| h.temp_c)
                            .or_else(|| b.map(|b| b.current.temp_c))
                            .map_or_else(
                                || "--".into(),
                                |c| {
                                    if act {
                                        settings.format_temp(c)
                                    } else {
                                        format!("{:.0}", settings.convert_temp(c).round())
                                    }
                                },
                            );
                        (
                            t,
                            String::new(),
                            if act {
                                rgb8(76, 92, 114)
                            } else {
                                rgb8(42, 132, 210)
                            },
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
                }
            })
            .collect();
        let prog = MapCanvas {
            engine: self,
            interactive: true,
            live_only: false,
            cam: (self.center_lat, self.center_lon, self.zoom),
            base_style: base,
            overlay: self.overlay,
            corner_r: 12.0,
            bundles,
            badges,
            wrap: Some(wrap),
        };
        let (hud, hv) = (
            HudColors::new(base.is_dark_hud(self.overlay)),
            rgba8(128, 145, 175, 0.22),
        );
        let close = icon_btn(
            UiIcon::Close,
            14.0,
            (30, 30),
            (
                Color::WHITE,
                rgba8(58, 68, 82, 0.88),
                rgba8(82, 94, 112, 0.96),
            ),
            15.0,
            wrap(MapMsg::CloseMap),
        );
        let mut left = column![
            shield(close, wrap),
            shield(self.view_legend(hud, settings), wrap)
        ]
        .spacing(10);
        if self.inspect.coord.is_some() {
            left = left.push(shield(self.view_inspector(settings, wrap), wrap));
        }
        let lcol = if self.show_layer_menu {
            (Color::WHITE, rgb8(10, 132, 255), rgb8(28, 146, 255))
        } else {
            (hud.fg, Color::TRANSPARENT, hv)
        };
        let pill = |el: Element<'a, M>| container(el).style(hud_pill(hud.bg, hud.border, 9.0));
        let zcol = column![
            icon_btn(
                UiIcon::Plus,
                14.0,
                (32, 30),
                (hud.fg, Color::TRANSPARENT, hv),
                7.0,
                wrap(MapMsg::ZoomStep(1.0))
            ),
            container(sp_w(22))
                .height(1)
                .style(flat_box(hud.border, 0.0)),
            icon_btn(
                UiIcon::Minus,
                14.0,
                (32, 30),
                (hud.fg, Color::TRANSPARENT, hv),
                7.0,
                wrap(MapMsg::ZoomStep(-1.0))
            ),
        ]
        .align_x(Alignment::Center);
        let ctrls = shield(
            column![
                pill(icon_btn(
                    UiIcon::Locate,
                    14.0,
                    (32, 32),
                    (hud.fg, Color::TRANSPARENT, hv),
                    8.0,
                    wrap(MapMsg::RecenterActive)
                )),
                pill(icon_btn(
                    UiIcon::Layers,
                    15.0,
                    (32, 32),
                    lcol,
                    8.0,
                    wrap(MapMsg::ToggleLayerMenu)
                )),
                pill(zcol.into()),
            ]
            .spacing(8)
            .align_x(Alignment::End)
            .into(),
            wrap,
        );
        let right: Element<'a, M> = if self.show_layer_menu {
            row![shield(self.view_layer_menu(hud, wrap), wrap), ctrls]
                .spacing(8)
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
        .padding(12);
        stack![sp_w(0), Canvas::new(prog).width(Fill).height(Fill), hud_el]
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
        let bh = (labels.len() as f32 * 18.0).max(68.0);
        let col = column(
            labels
                .into_iter()
                .map(|l| {
                    container(txt(l, 10.5, Weight::Medium, hud.sub))
                        .height(18.0)
                        .align_y(Alignment::Center)
                        .into()
                })
                .collect::<Vec<_>>(),
        );
        container(
            column![
                txt(title, 11.5, Weight::Semibold, hud.fg),
                row![Canvas::new(RampBar(stops)).width(5.0).height(bh), col]
                    .spacing(8)
                    .align_y(Alignment::Center)
            ]
            .spacing(7),
        )
        .padding([9, 11])
        .style(hud_pill(hud.bg, hud.border, 10.0))
        .into()
    }

    fn view_layer_menu<'a, M: Clone + 'a>(
        &self,
        hud: HudColors,
        wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let item = |lbl: &'static str, sz: f32, act: bool, msg: MapMsg| -> Element<'a, M> {
            let cols = if act {
                (rgb8(10, 132, 255), rgb8(10, 132, 255), Color::WHITE)
            } else {
                (Color::TRANSPARENT, rgba8(128, 145, 175, 0.20), hud.fg)
            };
            pbtn(
                txt(
                    lbl,
                    sz,
                    if act {
                        Weight::Semibold
                    } else {
                        Weight::Normal
                    },
                    cols.2,
                ),
                [5, 9],
                cols,
                7.0,
                wrap(msg),
            )
            .width(Fill)
            .into()
        };
        let mut items: Vec<Element<'a, M>> =
            vec![txt("WEATHER LAYER", 10.0, Weight::Bold, hud.sub).into()];
        items.extend(WeatherOverlay::ALL.map(|ov| {
            item(
                ov.label(),
                12.5,
                self.overlay == ov,
                MapMsg::SelectOverlay(ov),
            )
        }));
        items.push(hairline(hud.border));
        items.push(txt("BASE MAP", 10.0, Weight::Bold, hud.sub).into());
        items.extend(BaseMapStyle::ALL.map(|st| {
            item(
                st.label(),
                12.0,
                self.base_style == st,
                MapMsg::SelectBaseStyle(st),
            )
        }));
        container(column(items).spacing(3).width(172))
            .padding(9)
            .style(hud_pill(hud.bg, hud.border, 11.0))
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
        let accent = if hud.fg.r > 0.5 {
            rgb8(76, 194, 255)
        } else {
            rgb8(0, 112, 224)
        };
        let future = m.frames[cur_i].is_nowcast || cur_i > live_i;
        let (status, scol) = if cur_i == live_i || (!future && dm.abs() <= 4) {
            (format!("{} • Live Now", self.overlay.label()), hud.fg)
        } else if future {
            (
                format!("Forecast Prediction • {}", fmt_dm(dm.max(1), false)),
                accent,
            )
        } else {
            (format!("Radar History • {}", fmt_dm(dm, false)), hud.fg)
        };
        let tstr = loc.map_or_else(
            || {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map_or_else(String::new, |d| d.format("%H:%M UTC").to_string())
            },
            |l| format!("{} • {}", l.name, l.format_local_time(ts, s.use_24h_time)),
        );
        let (al, ac, abg, ahv, amsg) = if cur_i != live_i {
            (
                "Now",
                hud.fg,
                rgba8(128, 140, 160, 0.22),
                rgba8(128, 140, 160, 0.38),
                MapMsg::JumpToLive,
            )
        } else {
            let p2h = m
                .frames
                .iter()
                .position(|f| f.is_nowcast && f.time_unix >= live_ts + 105 * 60)
                .unwrap_or(max_i);
            (
                "+2h >",
                accent,
                rgba8(56, 168, 255, 0.16),
                rgba8(56, 168, 255, 0.30),
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
        let ticks = row![
            txt(
                tick(min_i),
                10.0,
                if min_i == live_i {
                    Weight::Bold
                } else {
                    Weight::Normal
                },
                if min_i == live_i { hud.fg } else { hud.sub }
            ),
            sp_w(Length::FillPortion(wl)),
            txt(
                tick(ma),
                10.0,
                if ma == live_i {
                    Weight::Bold
                } else {
                    Weight::Normal
                },
                if ma == cur_i { hud.fg } else { hud.sub }
            ),
            sp_w(Length::FillPortion(wr)),
            text(tick(mb)).size(10.0).color(hud.sub),
            sp_w(Length::FillPortion(wr)),
            txt(tick(max_i), 10.0, Weight::Semibold, accent),
        ]
        .width(Fill)
        .align_y(Alignment::Center);
        let play = icon_btn(
            if self.radar_playing {
                UiIcon::Pause
            } else {
                UiIcon::Play
            },
            13.0,
            (30, 30),
            (
                hud.fg,
                rgba8(128, 140, 160, 0.24),
                rgba8(128, 140, 160, 0.38),
            ),
            15.0,
            wrap(MapMsg::ToggleRadarPlay),
        );
        let top = row![
            play,
            column![
                txt(status, 11.5, Weight::Semibold, scol),
                text(tstr).size(10.0).color(hud.sub)
            ]
            .align_x(Alignment::Center)
            .width(Fill),
            pbtn(
                txt(al, 10.5, Weight::Semibold, ac),
                [4, 9],
                (abg, ahv, ac),
                10.0,
                wrap(amsg)
            )
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let card = shield(
            container(
                column![
                    top,
                    slider(min_i as f32..=max_i as f32, pos, move |v| wrap(
                        MapMsg::ScrubRadarPos(v)
                    ))
                    .step(0.01_f32)
                    .height(14),
                    ticks
                ]
                .spacing(4)
                .width(Fill),
            )
            .width(Fill)
            .max_width(450)
            .padding([8, 14])
            .style(hud_pill(hud.bg, hud.border, 12.0))
            .into(),
            wrap,
        );
        container(card).width(Fill).center_x(Fill).into()
    }

    fn view_inspector<'a, M: Clone + 'a>(
        &self,
        s: &Settings,
        wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let close = pbtn(
            icon_svg(UiIcon::Close, 12.0, Color::WHITE),
            [4, 4],
            (Color::TRANSPARENT, rgba8(255, 255, 255, 0.16), Color::WHITE),
            6.0,
            wrap(MapMsg::CloseInspector),
        );
        let body: Element<'a, M> = if self.inspect.loading {
            row![
                txt("Inspecting point…", 12.0, Weight::Medium, Color::WHITE).width(Fill),
                close
            ]
            .align_y(Alignment::Center)
            .into()
        } else if let Some(b) = &self.inspect.bundle {
            let head = row![
                column![
                    txt(
                        b.location.name.clone(),
                        13.5,
                        Weight::Semibold,
                        Color::WHITE
                    ),
                    text(format!("{:.2}°, {:.2}°", b.location.lat, b.location.lon))
                        .size(10.5)
                        .color(rgba8(210, 222, 240, 0.75))
                ]
                .width(Fill),
                close
            ]
            .align_y(Alignment::Start);
            let mid = row![
                weather_icon_svg(b.current.condition, b.current.is_day, 28.0),
                txt(
                    s.format_temp(b.current.temp_c),
                    26.0,
                    Weight::Medium,
                    Color::WHITE
                ),
                column![
                    txt(
                        b.current.condition.label(),
                        11.5,
                        Weight::Medium,
                        Color::WHITE
                    ),
                    text(format!(
                        "{} • {}",
                        s.format_wind(b.current.wind_speed_mps),
                        s.format_pressure(b.current.pressure_hpa)
                    ))
                    .size(10.5)
                    .color(rgba8(215, 226, 244, 0.80))
                ]
                .spacing(1),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            let add = pbtn(
                container(txt(
                    "+ Add to Locations",
                    11.5,
                    Weight::Semibold,
                    Color::WHITE,
                ))
                .center_x(Fill),
                [5, 8],
                (rgb8(10, 132, 255), rgb8(36, 148, 255), Color::WHITE),
                7.0,
                wrap(MapMsg::SaveInspectedLocation),
            )
            .width(Fill);
            column![head, mid, add].spacing(7).into()
        } else {
            row![
                text(
                    self.inspect
                        .error
                        .clone()
                        .unwrap_or_else(|| "Unable to load weather for this point.".into())
                )
                .size(11.0)
                .color(rgba8(255, 165, 165, 0.95))
                .width(Fill),
                close
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        };
        container(body)
            .width(220)
            .padding(10)
            .style(hud_pill(
                rgba8(24, 32, 46, 0.93),
                rgba8(255, 255, 255, 0.18),
                11.0,
            ))
            .into()
    }
}
