use super::overlays::{CityBadge, wrap_lon};
use super::{BaseMapStyle, ContextMenuState, MapCanvas, MapEngine, MapMsg, TimelineSpec, WeatherOverlay};
use crate::icons::{UiIcon, aqi_to_color, icon_btn, icon_svg, pressure_to_color, temp_badge_color, temp_to_color, weather_icon_svg};
use crate::model::{AlertSeverity, Location, Settings, WeatherBundle, compass_direction};
use crate::providers::{RadarFrameSource, wind_uv};
use crate::style::{flat_box, fw, hairline, hud_pill, pbtn, rgb8, rgba8, sp_h, sp_w, txt};
use iced::font::Weight;
use iced::mouse;
use iced::widget::canvas::{self, Action, Canvas, Event, Frame, Geometry, Path, Stroke, Text as CanvasText, gradient::Linear};
use iced::widget::{column, container, mouse_area, row, stack, text};
use iced::{Alignment, Color, Element, Fill, Length, Point, Rectangle, Renderer, Size, Theme};
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Hud([Color; 8], bool);
impl Hud {
    fn new(dark: bool) -> Self {
        let c = if dark {
            [
                rgba8(24, 30, 42, 0.88),
                rgba8(255, 255, 255, 0.16),
                rgba8(255, 255, 255, 0.12),
                Color::WHITE,
                rgba8(216, 226, 242, 0.76),
                rgb8(72, 186, 255),
                rgba8(255, 255, 255, 0.12),
                rgba8(10, 132, 255, 0.24),
            ]
        } else {
            [
                rgba8(249, 251, 254, 0.92),
                rgba8(18, 30, 50, 0.12),
                rgba8(18, 30, 50, 0.09),
                rgb8(20, 28, 42),
                rgba8(56, 68, 88, 0.78),
                rgb8(0, 122, 255),
                rgba8(20, 36, 64, 0.08),
                rgba8(0, 122, 255, 0.13),
            ]
        };
        Self(c, dark)
    }
}
fn overlay_icon(ov: WeatherOverlay) -> UiIcon {
    [UiIcon::Droplet, UiIcon::Thermometer, UiIcon::Air, UiIcon::Wind, UiIcon::Cloud, UiIcon::Gauge, UiIcon::Radar][ov as usize]
}

struct RampBar(Vec<Color>);
#[derive(Default)]
struct CachedCanvasState {
    cache: canvas::Cache,
    sig: std::cell::Cell<u64>,
}
impl<M> canvas::Program<M> for RampBar {
    type State = CachedCanvasState;
    fn draw(&self, st: &CachedCanvasState, r: &Renderer, _: &Theme, b: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (b.width.to_bits(), b.height.to_bits(), self.0.len()).hash(&mut h);
        for c in &self.0 {
            (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()).hash(&mut h);
        }
        let sig = h.finish() | 1;
        if st.sig.get() != sig {
            st.cache.clear();
            st.sig.set(sig);
        }
        vec![st.cache.draw(r, b.size(), |f| {
            let (mut g, last) = (Linear::new(Point::ORIGIN, Point::new(0.0, b.height)), self.0.len().saturating_sub(1).max(1) as f32);
            for (i, c) in self.0.iter().enumerate() {
                g = g.add_stop(i as f32 / last, *c);
            }
            f.fill(&Path::rounded_rectangle(Point::ORIGIN, b.size(), 3.0.into()), g);
        })]
    }
}

struct MapStyleSwatch(BaseMapStyle, bool);
impl<M> canvas::Program<M> for MapStyleSwatch {
    type State = CachedCanvasState;
    fn draw(&self, st: &CachedCanvasState, r: &Renderer, _: &Theme, b: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (b.width.to_bits(), b.height.to_bits(), self.0, self.1).hash(&mut h);
        let sig = h.finish() | 1;
        if st.sig.get() != sig {
            st.cache.clear();
            st.sig.set(sig);
        }
        vec![st.cache.draw(r, b.size(), |f| {
            let sz = b.size();
            let (w, h) = (sz.width, sz.height);
            let (bg, land, water, road, road2) = match self.0 {
                BaseMapStyle::CleanLight => {
                    (rgb8(214, 228, 242), rgb8(244, 246, 248), rgb8(192, 218, 242), rgb8(255, 255, 255), rgb8(226, 206, 152))
                }
                BaseMapStyle::AppleDark => (rgb8(24, 34, 48), rgb8(42, 48, 58), rgb8(18, 30, 48), rgb8(76, 86, 102), rgb8(96, 110, 130)),
                BaseMapStyle::Satellite => (rgb8(20, 44, 68), rgb8(46, 82, 54), rgb8(16, 40, 68), rgb8(116, 134, 92), rgb8(74, 104, 68)),
                BaseMapStyle::Terrain => {
                    (rgb8(204, 224, 236), rgb8(226, 236, 214), rgb8(174, 210, 236), rgb8(184, 198, 166), rgb8(244, 222, 166))
                }
                BaseMapStyle::Streets => {
                    (rgb8(184, 218, 244), rgb8(242, 238, 228), rgb8(170, 211, 242), rgb8(255, 255, 255), rgb8(248, 194, 102))
                }
            };
            f.fill(&Path::rounded_rectangle(Point::ORIGIN, sz, 6.0.into()), bg);
            let land_p = Path::new(|pb| {
                pb.move_to(Point::new(0.0, h * 0.25));
                pb.line_to(Point::new(w * 0.62, 0.0));
                pb.line_to(Point::new(w, 0.0));
                pb.line_to(Point::new(w, h));
                pb.line_to(Point::new(w * 0.22, h));
                pb.line_to(Point::new(0.0, h * 0.72));
                pb.close();
            });
            f.fill(&land_p, land);
            let bay_p = Path::new(|pb| {
                pb.move_to(Point::new(0.0, h * 0.62));
                pb.line_to(Point::new(w * 0.34, h * 0.56));
                pb.line_to(Point::new(w * 0.28, h));
                pb.line_to(Point::new(0.0, h));
                pb.close();
            });
            f.fill(&bay_p, water);
            let r1 = Path::new(|pb| {
                pb.move_to(Point::new(w * 0.12, h * 0.88));
                pb.line_to(Point::new(w * 0.52, h * 0.48));
                pb.line_to(Point::new(w * 0.92, h * 0.16));
            });
            let r2 = Path::new(|pb| {
                pb.move_to(Point::new(w * 0.30, h * 0.10));
                pb.line_to(Point::new(w * 0.56, h * 0.54));
                pb.line_to(Point::new(w * 0.84, h * 0.90));
            });
            f.stroke(&r1, Stroke::default().with_width(2.2).with_color(road2));
            f.stroke(&r2, Stroke::default().with_width(1.4).with_color(road));
            if self.1 {
                f.stroke(
                    &Path::rounded_rectangle(Point::new(1.0, 1.0), Size::new(w - 2.0, h - 2.0), 5.5.into()),
                    Stroke::default().with_width(2.0).with_color(rgb8(10, 132, 255)),
                );
            }
        })]
    }
}

struct ScrubberTick {
    u: f32,
    label: String,
    is_live: bool,
    is_accent: bool,
}

struct Scrubber<'a, M> {
    tl: TimelineSpec,
    pos: f32,
    ticks: Vec<ScrubberTick>,
    loc: Option<&'a Location>,
    use_24h: bool,
    hud: Hud,
    wrap: fn(MapMsg) -> M,
}
impl<M> Scrubber<'_, M> {
    const X_PAD: f32 = 14.0;
    fn u_at(&self, x: f32, w: f32) -> f32 {
        ((x - Self::X_PAD) / (w - Self::X_PAD * 2.0).max(16.0)).clamp(0.0, 1.0)
    }
    fn fmt_ts(&self, ts: i64) -> String {
        let live_ts = self.tl.live_ts();
        if (ts - live_ts).abs() <= 240 {
            return "Now".into();
        }
        self.loc.map_or_else(|| fmt_dm(((ts - live_ts) as f64 / 60.0).round() as i64, true), |l| l.format_local_time(ts, self.use_24h))
    }
}
#[derive(Default)]
struct ScrubberState {
    dragging: bool,
    hover_x: Option<f32>,
    track_cache: canvas::Cache,
    track_sig: std::cell::Cell<u64>,
}

impl<M: Clone> canvas::Program<M> for Scrubber<'_, M> {
    type State = ScrubberState;
    fn update(&self, st: &mut ScrubberState, ev: &Event, b: Rectangle, cur: mouse::Cursor) -> Option<Action<M>> {
        let pub_x = |x| {
            let pos = self.tl.u_to_pos(self.u_at(x, b.width));
            Some(Action::publish((self.wrap)(MapMsg::ScrubRadarPos(pos))).and_capture())
        };
        match ev {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let x = cur.position_in(b)?.x;
                (st.dragging, st.hover_x) = (true, Some(x));
                pub_x(x)
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                let x = position.x - b.x;
                st.hover_x = Some(x);
                pub_x(x)
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hx = cur.position_in(b).map(|p| p.x);
                let changed = match (st.hover_x, hx) {
                    (Some(a), Some(c)) => (a - c).abs() >= 1.5,
                    (None, None) => false,
                    _ => true,
                };
                changed.then(|| {
                    st.hover_x = hx;
                    Action::request_redraw()
                })
            }
            Event::Mouse(mouse::Event::CursorLeft) if !st.dragging && st.hover_x.is_some() => {
                st.hover_x = None;
                Some(Action::request_redraw())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if st.dragging => {
                st.dragging = false;
                st.hover_x = cur.position_in(b).map(|p| p.x);
                Some(Action::capture())
            }
            Event::Window(iced::window::Event::Unfocused) if st.dragging || st.hover_x.is_some() => {
                (st.dragging, st.hover_x) = (false, None);
                Some(Action::request_redraw())
            }
            _ => None,
        }
    }
    fn mouse_interaction(&self, st: &ScrubberState, b: Rectangle, cur: mouse::Cursor) -> mouse::Interaction {
        if st.dragging {
            mouse::Interaction::Grabbing
        } else if cur.position_in(b).is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
    fn draw(&self, st: &ScrubberState, r: &Renderer, _: &Theme, b: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        use std::hash::{Hash, Hasher};
        let (cy, x0, tw, th) = (21.5_f32, Self::X_PAD, (b.width - Self::X_PAD * 2.0).max(16.0), 5.0_f32);
        let pill = |fr: &mut Frame, x: f32, y: f32, w: f32, h: f32, c: Color| {
            fr.fill(&Path::rounded_rectangle(Point::new(x, y), Size::new(w, h), (h * 0.5).into()), c)
        };
        let (trk, past, pip) = if self.hud.1 {
            (rgba8(255, 255, 255, 0.16), rgba8(180, 200, 226, 0.36), rgba8(255, 255, 255, 0.22))
        } else {
            (rgba8(20, 32, 52, 0.13), rgba8(80, 98, 124, 0.30), rgba8(20, 32, 52, 0.18))
        };
        let lf = self.tl.time_to_u(self.tl.live_ts());
        let pf = self.tl.pos_to_u(self.pos);
        let (lx, tx) = (x0 + lf * tw, x0 + pf * tw);

        // Layer 1 (GPU-cached): static track, tick pips, Now marker, and all tick text labels —
        // NEVER clears or re-shapes text during 60fps radar playback!
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            b.width.to_bits(),
            b.height.to_bits(),
            self.hud.1,
            self.tl.live_idx,
            self.tl.times.first().copied(),
            self.tl.times.last().copied(),
            self.ticks.len(),
        )
            .hash(&mut h);
        for tk in &self.ticks {
            (tk.u.to_bits(), &tk.label, tk.is_live, tk.is_accent).hash(&mut h);
        }
        let sig = h.finish() | 1;
        if st.track_sig.get() != sig {
            st.track_cache.clear();
            st.track_sig.set(sig);
        }
        let track_geo = st.track_cache.draw(r, b.size(), |f| {
            pill(f, x0, cy - th * 0.5, tw, th, trk);
            for tk in &self.ticks {
                if tk.u > 0.02 && tk.u < 0.98 && !tk.is_live {
                    pill(f, x0 + tk.u * tw - 0.75, cy - 3.5, 1.5, 7.0, pip);
                }
            }
            for tk in &self.ticks {
                let (tx_pos, ax) = if tk.u <= 0.01 {
                    (x0 - 2.0, Alignment::Start)
                } else if tk.u >= 0.99 {
                    (x0 + tw + 2.0, Alignment::End)
                } else {
                    (x0 + tk.u * tw, Alignment::Center)
                };
                let col = if tk.is_accent {
                    self.hud.0[5]
                } else if tk.is_live {
                    self.hud.0[3]
                } else {
                    self.hud.0[4]
                };
                let wt = if tk.is_live || tk.is_accent { Weight::Semibold } else { Weight::Medium };
                f.fill_text(CanvasText {
                    content: tk.label.clone(),
                    position: Point::new(tx_pos, 36.5),
                    color: col,
                    size: 9.5.into(),
                    font: fw(wt),
                    align_x: ax.into(),
                    align_y: Alignment::Center.into(),
                    ..CanvasText::default()
                });
            }
        });

        // Layer 2 (tiny uncached dynamic overlay): active fill bar, Now pip, thumb, and optional scrub pill.
        let mut f = Frame::new(r, b.size());
        if pf > 0.002 {
            let pe = tx.min(lx);
            let _ = (pe > x0 + 1.0).then(|| pill(&mut f, x0, cy - th * 0.5, pe - x0, th, past));
            let _ = (tx > lx + 1.0).then(|| pill(&mut f, lx.max(x0), cy - th * 0.5, tx - lx.max(x0), th, self.hud.0[5]));
        }
        if lf > 0.02 && lf < 0.98 {
            pill(&mut f, lx - 1.25, cy - 5.5, 2.5, 11.0, if self.hud.1 { rgba8(255, 255, 255, 0.82) } else { rgba8(24, 34, 52, 0.65) });
        }
        if st.dragging {
            let lbl = self.fmt_ts(self.tl.ts_at_pos(self.pos));
            let bw = (lbl.len() as f32 * 5.6 + 12.0).max(36.0);
            let bx = (tx - bw * 0.5).clamp(2.0, (b.width - bw - 2.0).max(2.0));
            f.fill(&Path::rounded_rectangle(Point::new(bx, 0.0), Size::new(bw, 13.0), 4.5.into()), rgb8(10, 132, 255));
            f.fill_text(CanvasText {
                content: lbl,
                position: Point::new(bx + bw * 0.5, 6.5),
                color: Color::WHITE,
                size: 8.8.into(),
                font: fw(Weight::Semibold),
                align_x: Alignment::Center.into(),
                align_y: Alignment::Center.into(),
                ..CanvasText::default()
            });
        } else if let Some(hx_raw) = st.hover_x {
            let hx = hx_raw.clamp(x0, x0 + tw);
            let lbl = self.fmt_ts(self.tl.u_to_ts(self.u_at(hx, b.width)));
            pill(&mut f, hx - 1.0, cy - 4.5, 2.0, 9.0, self.hud.0[5]);
            let bw = (lbl.len() as f32 * 5.6 + 12.0).max(36.0);
            let bx = (hx - bw * 0.5).clamp(2.0, (b.width - bw - 2.0).max(2.0));
            let tag_bg = if self.hud.1 { rgba8(255, 255, 255, 0.18) } else { rgba8(18, 30, 50, 0.11) };
            f.fill(&Path::rounded_rectangle(Point::new(bx, 0.0), Size::new(bw, 13.0), 4.5.into()), tag_bg);
            f.fill_text(CanvasText {
                content: lbl,
                position: Point::new(bx + bw * 0.5, 6.5),
                color: self.hud.0[3],
                size: 8.8.into(),
                font: fw(Weight::Semibold),
                align_x: Alignment::Center.into(),
                align_y: Alignment::Center.into(),
                ..CanvasText::default()
            });
        }
        let (tr, tc) = (
            if st.dragging {
                7.8
            } else if st.hover_x.is_some() {
                7.2
            } else {
                6.5
            },
            Point::new(tx, cy),
        );
        f.fill(&Path::circle(Point::new(tc.x, tc.y + 1.2), tr + 1.4), rgba8(0, 0, 0, if self.hud.1 { 0.36 } else { 0.20 }));
        f.fill(&Path::circle(tc, tr), Color::WHITE);
        let bc = if self.hud.1 { rgba8(255, 255, 255, 0.35) } else { rgba8(18, 30, 50, 0.18) };
        f.stroke(&Path::circle(tc, tr), Stroke::default().with_width(1.0).with_color(bc));
        vec![track_geo, f.into_geometry()]
    }
}

fn build_scrubber_ticks(tl: &TimelineSpec) -> Vec<ScrubberTick> {
    let (t_min, live_ts, t_max) = (tl.times[0], tl.live_ts(), *tl.times.last().unwrap_or(&tl.times[0]));
    let past_mins = ((live_ts - t_min) as f64 / 60.0).round() as i64;
    let fcst_mins = ((t_max - live_ts) as f64 / 60.0).round() as i64;
    let mut out = Vec::with_capacity(6);
    if past_mins >= 15 {
        out.push(ScrubberTick { u: 0.0, label: fmt_dm(-past_mins, true), is_live: false, is_accent: false });
        if fcst_mins < 15 && past_mins >= 240 {
            let step_h = if past_mins >= 540 { 3_i64 } else { 2 };
            let mut h = ((past_mins / 60) / step_h) * step_h;
            while h >= step_h {
                let ts = live_ts - h * 3600;
                if ts > t_min + 1800 {
                    let u = tl.time_to_u(ts);
                    if out.last().is_none_or(|p| u - p.u >= 0.16) && 1.0 - u >= 0.16 {
                        out.push(ScrubberTick { u, label: format!("-{h}h"), is_live: false, is_accent: false });
                    }
                }
                h -= step_h;
            }
        }
    }
    let live_u = tl.time_to_u(live_ts);
    out.push(ScrubberTick { u: live_u, label: "Now".into(), is_live: true, is_accent: false });
    if fcst_mins >= 15 {
        if fcst_mins >= 600 {
            for h in [3_i64, 6, 9] {
                let ts = live_ts + h * 3600;
                if ts < t_max - 3600 {
                    out.push(ScrubberTick { u: tl.time_to_u(ts), label: format!("+{h}h"), is_live: false, is_accent: false });
                }
            }
        } else if fcst_mins >= 180 {
            let step_h = if fcst_mins >= 360 { 2_i64 } else { 1 };
            let mut h = step_h;
            while h * 60 < fcst_mins - 45 {
                let u = tl.time_to_u(live_ts + h * 3600);
                if out.last().is_none_or(|p| u - p.u >= 0.15) && 1.0 - u >= 0.14 {
                    out.push(ScrubberTick { u, label: format!("+{h}h"), is_live: false, is_accent: false });
                }
                h += step_h;
            }
        } else if fcst_mins >= 45 {
            let mid = ((fcst_mins / 2 + 7) / 15) * 15;
            if mid >= 15 && fcst_mins - mid >= 15 {
                let u = tl.time_to_u(live_ts + mid * 60);
                if u - live_u >= 0.18 && 1.0 - u >= 0.18 {
                    out.push(ScrubberTick { u, label: fmt_dm(mid, true), is_live: false, is_accent: false });
                }
            }
        }
        out.push(ScrubberTick { u: 1.0, label: fmt_dm(fcst_mins, true), is_live: false, is_accent: true });
    }
    out
}

fn shield<'a, M: Clone + 'a>(el: Element<'a, M>, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
    let m = wrap(MapMsg::HudConsume);
    let ma = mouse_area(el).interaction(mouse::Interaction::Idle).on_press(m.clone()).on_right_press(m);
    ma.on_scroll(move |_| wrap(MapMsg::HudConsume)).into()
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
fn canvas_stack<'a, M: Clone + 'a>(
    tp: MapCanvas<'a, M>, badges: Vec<CityBadge>, top: Element<'a, M>, w: impl Into<Length> + Copy, h: impl Into<Length> + Copy,
) -> Element<'a, M> {
    let vp = MapCanvas { vectors_only: true, badges, ..tp };
    stack![sp_w(0), Canvas::new(tp).width(w).height(h), Canvas::new(vp).width(w).height(h), top].width(w).height(h).clip(true).into()
}

impl MapEngine {
    pub fn view_mini_card<'a, M: Clone + 'a>(
        &'a self, size: Size, loc: Option<&'a Location>, bundles: &'a HashMap<String, WeatherBundle>, settings: &'a Settings, light: bool,
        on_expand: M,
    ) -> Element<'a, M> {
        let (clat, clon) = loc.map_or((self.center_lat, self.center_lon), |l| (l.lat, l.lon));
        let pin = loc.map(|l| {
            let t = bundles.get(&l.id).map(|b| b.current.temp_c);
            CityBadge {
                title: String::new(),
                lat: l.lat,
                lon: l.lon,
                val: t.map_or_else(|| "--".into(), |c| settings.format_temp(c)),
                sub: String::new(),
                col: t.map_or_else(|| rgb8(46, 108, 186), temp_badge_color),
                active: true,
                is_current_location: l.is_current_location,
                wind_deg: None,
                alert_severity: bundles.get(&l.id).and_then(|b| b.alerts.first().map(|a| a.severity)),
            }
        });
        let tp = MapCanvas::<M> {
            engine: self,
            vectors_only: false,
            interactive: false,
            accept_keys: false,
            cam: (clat, clon, super::MINI_MAP_ZOOM),
            base_style: self.base_style,
            overlay: WeatherOverlay::RadarTimeline,
            corner_r: 10.0,
            bundles,
            settings,
            badges: Vec::new(),
            wrap: None,
        };
        let bcol = if light { rgba8(20, 35, 60, 0.12) } else { rgba8(255, 255, 255, 0.12) };
        let bdr = container(sp_w(Fill)).width(size.width).height(size.height).style(move |_| container::Style {
            border: iced::Border { color: bcol, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        });
        let layered = canvas_stack(tp, pin.into_iter().collect(), bdr.into(), size.width, size.height);
        let clip = container(layered).width(size.width).height(size.height).clip(true);
        mouse_area(clip).interaction(mouse::Interaction::Pointer).on_press(on_expand).into()
    }

    pub fn view_expanded<'a, M: Clone + 'a>(
        &'a self, saved: &'a [Location], bundles: &'a HashMap<String, WeatherBundle>, active_id: Option<&'a str>, settings: &'a Settings,
        accept_keys: bool, wrap: fn(MapMsg) -> M,
    ) -> Element<'a, M> {
        let base = if matches!(self.overlay, WeatherOverlay::WindFlow | WeatherOverlay::InfraredClouds)
            && self.base_style == BaseMapStyle::CleanLight
        {
            BaseMapStyle::AppleDark
        } else {
            self.base_style
        };
        let fts = self.selected_forecast_timestamp();
        let sample_bundle = |x: &WeatherBundle| -> (f64, f64, f64, f64) {
            if let Some(t) = fts
                && !x.hourly.is_empty()
            {
                let i = x.hourly.partition_point(|h| h.timestamp_unix < t);
                let (h0, h1) = (&x.hourly[i.saturating_sub(1).min(x.hourly.len() - 1)], &x.hourly[i.min(x.hourly.len() - 1)]);
                let f = ((t - h0.timestamp_unix) as f64 / (h1.timestamp_unix - h0.timestamp_unix).max(1) as f64).clamp(0.0, 1.0);
                let ((u0, v0), (u1, v1)) = (wind_uv(h0.wind_speed_mps, h0.wind_dir_deg), wind_uv(h1.wind_speed_mps, h1.wind_dir_deg));
                let (wu, wv) = (u0 as f64 + (u1 - u0) as f64 * f, v0 as f64 + (v1 - v0) as f64 * f);
                let wdir = ((-wu).atan2(-wv).to_degrees() + 360.0) % 360.0;
                (h0.temp_c + (h1.temp_c - h0.temp_c) * f, wu.hypot(wv), wdir, h0.pressure_hpa + (h1.pressure_hpa - h0.pressure_hpa) * f)
            } else {
                (x.current.temp_c, x.current.wind_speed_mps, x.current.wind_dir_deg, x.current.pressure_hpa)
            }
        };
        let badges: Vec<_> = saved
            .iter()
            .map(|l| {
                let (b, act) = (bundles.get(&l.id), active_id == Some(l.id.as_str()));
                let (val, sub, col, wdeg) = match self.overlay {
                    WeatherOverlay::AirQuality => {
                        b.and_then(|x| x.air_quality.as_ref()).map_or(("--".into(), String::new(), rgb8(90, 108, 134), None), |a| {
                            (a.us_aqi.to_string(), "AQI".into(), aqi_to_color(a.us_aqi), None)
                        })
                    }
                    WeatherOverlay::WindFlow => b.map_or(("--".into(), String::new(), rgb8(232, 238, 246), None), |x| {
                        let (_, wspd, wdir, _) = sample_bundle(x);
                        (settings.wind_value_and_unit(wspd).0, compass_direction(wdir).into(), rgb8(245, 248, 252), Some(wdir))
                    }),
                    WeatherOverlay::Pressure => b.map_or(("--".into(), String::new(), rgb8(84, 118, 168), None), |x| {
                        let (_, _, _, prs) = sample_bundle(x);
                        let pv = match settings.pressure_unit {
                            crate::model::PressureUnit::HPa => format!("{:.0}", prs),
                            crate::model::PressureUnit::InHg => format!("{:.2}", prs * 0.02953),
                        };
                        (pv, String::new(), pressure_to_color(prs, 1.0), None)
                    }),
                    _ => b.map_or(("--".into(), String::new(), rgb8(68, 118, 186), None), |x| {
                        let (tc, _, _, _) = sample_bundle(x);
                        (settings.format_temp(tc), String::new(), temp_badge_color(tc), None)
                    }),
                };
                CityBadge {
                    title: if l.is_current_location { "My Location".into() } else { l.name.clone() },
                    lat: l.lat,
                    lon: l.lon,
                    val,
                    sub,
                    col,
                    active: act,
                    is_current_location: l.is_current_location,
                    wind_deg: wdeg,
                    alert_severity: b.and_then(|x| x.alerts.first().map(|a| a.severity)),
                }
            })
            .collect();
        let tp = MapCanvas {
            engine: self,
            vectors_only: false,
            interactive: true,
            accept_keys: accept_keys && !self.show_layer_menu && self.context_menu.is_none(),
            cam: (self.center_lat, self.center_lon, self.zoom),
            base_style: base,
            overlay: self.overlay,
            corner_r: 13.0,
            bundles,
            settings,
            badges: Vec::new(),
            wrap: Some(wrap),
        };
        let h = Hud::new(base.is_dark_hud(self.overlay));
        let norm = (h.0[3], Color::TRANSPARENT, h.0[6]);
        let pill = |el: Element<'a, M>, r: f32| container(el).style(hud_pill(h.0[0], h.0[1], r));
        let hbtn = |ic, sz, dim: (u32, u32), cols, r, msg| icon_btn(ic, sz, dim, cols, r, wrap(msg));
        let close_btn = pill(hbtn(UiIcon::Close, 13.5, (34, 34), norm, 17.0, MapMsg::CloseMap), 17.0);
        let fs_ic = if self.full_screen { UiIcon::Compress } else { UiIcon::Expand };
        let fs_col = if self.full_screen { (h.0[5], h.0[7], h.0[7]) } else { norm };
        let fs_btn = pill(hbtn(fs_ic, 13.5, (34, 34), fs_col, 17.0, MapMsg::ToggleFullScreen), 17.0);
        let mut left = column![
            shield(row![close_btn, fs_btn].spacing(8).align_y(Alignment::Center).into(), wrap),
            shield(self.view_legend(h, settings, wrap), wrap),
        ]
        .spacing(10);
        if self.inspect.coord.is_some() {
            left = left.push(shield(self.view_inspector(h, saved, settings, wrap), wrap));
        }
        let active_loc = active_id.and_then(|id| saved.iter().find(|l| l.id == id));
        let at_active = active_loc.is_some_and(|l| (l.lat - self.center_lat).hypot(wrap_lon(l.lon - self.center_lon)) < 0.05);
        let loc_col = if at_active { (h.0[5], h.0[7], h.0[7]) } else { norm };
        let lcol = if self.show_layer_menu { (Color::WHITE, rgb8(10, 132, 255), rgb8(28, 146, 255)) } else { norm };
        let cap2 = |a, b| {
            let sep = container(sp_w(20)).height(1).style(flat_box(h.0[2], 0.0));
            pill(container(column![a, sep, b].spacing(2).align_x(Alignment::Center)).padding(2).into(), 10.0)
        };
        let sep_a = container(sp_w(20)).height(1).style(flat_box(h.0[2], 0.0));
        let sep_b = container(sp_w(20)).height(1).style(flat_box(h.0[2], 0.0));
        let mut c1_col = column![hbtn(UiIcon::Locate, 14.5, (32, 32), loc_col, 8.0, MapMsg::RecenterActive), sep_a,]
            .spacing(2)
            .align_x(Alignment::Center);
        if saved.len() > 1 {
            c1_col = c1_col.push(hbtn(UiIcon::Globe, 14.5, (32, 32), norm, 8.0, MapMsg::FitAllLocations)).push(sep_b);
        }
        c1_col = c1_col.push(hbtn(UiIcon::Layers, 15.0, (32, 32), lcol, 8.0, MapMsg::ToggleLayerMenu));
        let c1 = pill(container(c1_col).padding(2).into(), 10.0);
        let c2 = cap2(
            hbtn(UiIcon::Plus, 14.5, (32, 32), norm, 8.0, MapMsg::ZoomStep(1.0)),
            hbtn(UiIcon::Minus, 14.5, (32, 32), norm, 8.0, MapMsg::ZoomStep(-1.0)),
        );
        let ctrls = shield(column![c1, c2].spacing(10).align_x(Alignment::End).into(), wrap);
        let right: Element<'a, M> = if self.show_layer_menu {
            row![shield(self.view_layer_menu(h, wrap), wrap), ctrls].spacing(10).align_y(Alignment::Start).into()
        } else {
            ctrls
        };
        let top_row = row![left, sp_w(Fill), right].width(Fill).align_y(Alignment::Start);
        let mut bottom = column![].spacing(8).width(Fill).align_x(Alignment::Center);
        if let Some((msg, _)) = &self.toast {
            let toast_el = container(
                row![icon_svg(UiIcon::Check, 12.0, rgb8(52, 199, 89)), txt(msg.clone(), 11.5, Weight::Semibold, h.0[3])]
                    .spacing(6)
                    .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .style(hud_pill(h.0[0], h.0[1], 14.0));
            bottom = bottom.push(toast_el);
        }
        bottom = bottom.push(self.view_timeline(h, active_loc, settings, wrap));
        let hud_el: Element<'a, M> = column![top_row, sp_h(Fill), bottom].width(Fill).height(Fill).padding(14).into();
        // In Full-Screen Map mode, float a mathematically centered Apple Weather active-location pill top-center.
        let mut hud_stack = stack![hud_el];
        if self.full_screen
            && let Some(loc) = active_loc
        {
            let b_opt = bundles.get(&loc.id);
            let mut pr = row![].spacing(7).align_y(Alignment::Center);
            if let Some(b) = b_opt {
                pr = pr.push(weather_icon_svg(b.current.condition, b.current.is_day, 18.0));
            }
            pr = pr.push(txt(loc.name.clone(), 12.5, Weight::Semibold, h.0[3]));
            if let Some(b) = b_opt {
                pr = pr.push(txt(settings.format_temp(b.current.temp_c), 12.5, Weight::Bold, h.0[5]));
            }
            let btn = pbtn(pr, [6, 12], (Color::TRANSPARENT, h.0[6], h.0[3]), 15.0, wrap(MapMsg::RecenterActive));
            let cp = container(shield(pill(btn.into(), 16.0).into(), wrap)).padding([14, 0]).width(Fill).center_x(Fill);
            hud_stack = hud_stack.push(cp);
        }
        if let Some(cm) = self.context_menu {
            hud_stack = hud_stack.push(self.view_context_menu(h, cm, wrap));
        }
        canvas_stack(tp, badges, hud_stack.width(Fill).height(Fill).into(), Fill, Fill)
    }

    fn view_legend<'a, M: Clone + 'a>(&self, h: Hud, s: &Settings, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
        let (title, labels, stops): (&str, Vec<String>, Vec<Color>) = match self.overlay {
            WeatherOverlay::RadarTimeline | WeatherOverlay::NexradUsRadar => (
                "Precipitation",
                ["Extreme", "Heavy", "Moderate", "Light"].map(String::from).into(),
                vec![rgb8(250, 208, 76), rgb8(242, 96, 134), rgb8(192, 82, 160), rgb8(118, 78, 182), rgb8(36, 98, 202), rgb8(12, 98, 198)],
            ),
            WeatherOverlay::Temperature => (
                "Temperature",
                [40.0, 28.0, 16.0, 4.0, -10.0, -25.0].map(|c| s.format_temp(c)).into(),
                [40.0, 25.0, 12.0, -15.0].map(|c| temp_to_color(c, 1.0)).into(),
            ),
            WeatherOverlay::AirQuality => (
                "AQI (US)",
                ["500", "400", "300", "200", "100", "0"].map(String::from).into(),
                vec![rgb8(142, 28, 62), rgb8(175, 82, 222), rgb8(255, 59, 48), rgb8(255, 204, 0), rgb8(52, 199, 89)],
            ),
            WeatherOverlay::WindFlow => (
                "Wind",
                [30.0, 20.0, 10.0, 3.0].map(|m| s.format_wind(m)).into(),
                vec![rgb8(255, 112, 82), rgb8(255, 214, 92), rgb8(120, 240, 215), rgb8(145, 210, 255)],
            ),
            WeatherOverlay::Pressure => (
                "Pressure",
                [1040.0, 1025.0, 1013.0, 995.0].map(|p| s.format_pressure(p)).into(),
                [1040.0, 1020.0, 1005.0, 985.0].map(|p| pressure_to_color(p, 1.0)).into(),
            ),
            WeatherOverlay::InfraredClouds => (
                "IR Clouds",
                ["Deep", "Mid", "Thin", "Clear"].map(String::from).into(),
                vec![Color::WHITE, rgb8(180, 210, 245), rgb8(100, 130, 170), rgb8(45, 60, 82)],
            ),
        };
        let hd_row = row![icon_svg(overlay_icon(self.overlay), 12.0, h.0[4]), txt(title, 11.5, Weight::Semibold, h.0[3])]
            .spacing(5)
            .align_y(Alignment::Center);
        let hd = pbtn(hd_row, [2, 2], (Color::TRANSPARENT, h.0[6], h.0[3]), 6.0, wrap(MapMsg::ToggleLegend));
        if self.legend_collapsed {
            return container(hd).padding([6, 10]).style(hud_pill(h.0[0], h.0[1], 11.0)).into();
        }
        let bh = (labels.len() as f32 * 17.0).max(66.0);
        let mk_lbl = |l: String| container(txt(l, 10.5, Weight::Medium, h.0[4])).height(17.0).align_y(Alignment::Center).into();
        let col = column(labels.into_iter().map(mk_lbl).collect::<Vec<_>>());
        let body = row![Canvas::new(RampBar(stops)).width(6.0).height(bh), col].spacing(8).align_y(Alignment::Center);
        let mut content = column![hd, body].spacing(7);
        let has_hrrr = self.overlay == WeatherOverlay::RadarTimeline
            && self.manifest.as_ref().is_some_and(|m| m.frames.iter().any(|f| matches!(f.source, RadarFrameSource::NoaaHrrr(..))));
        if has_hrrr {
            let dot_lbl = |col: Color, lbl: &'static str| {
                row![container(sp_w(6)).width(6).height(6).style(flat_box(col, 3.0)), txt(lbl, 9.5, Weight::Medium, h.0[4])]
                    .spacing(3)
                    .align_y(Alignment::Center)
            };
            let types =
                row![dot_lbl(rgb8(56, 168, 255), "Rain"), dot_lbl(rgb8(224, 102, 232), "Mix"), dot_lbl(rgb8(130, 224, 255), "Snow"),]
                    .spacing(7)
                    .align_y(Alignment::Center);
            let sep = container(sp_w(118)).height(1).style(flat_box(h.0[2], 0.0));
            content = content.push(sep).push(types);
        }
        let prov = match self.overlay {
            WeatherOverlay::RadarTimeline if has_hrrr => "LibreWXR • HRRR • ECCC",
            WeatherOverlay::RadarTimeline => "LibreWXR • ECCC",
            WeatherOverlay::InfraredClouds if self.manifest.as_ref().is_some_and(|m| !m.satellite.is_empty()) => "LibreWXR Global IR",
            WeatherOverlay::InfraredClouds => "NOAA GOES-16/18 IR",
            WeatherOverlay::NexradUsRadar => "NOAA NEXRAD HD",
            WeatherOverlay::AirQuality => "CAMS / Open-Meteo",
            WeatherOverlay::Temperature | WeatherOverlay::WindFlow | WeatherOverlay::Pressure => "Open-Meteo",
        };
        content = content.push(txt(prov, 8.8, Weight::Medium, h.0[4]));
        container(content).padding([9, 11]).style(hud_pill(h.0[0], h.0[1], 12.0)).into()
    }

    fn view_layer_menu<'a, M: Clone + 'a>(&self, h: Hud, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
        let item = |ic: Option<UiIcon>, lbl: &'static str, sz: f32, act: bool, msg: MapMsg| -> Element<'a, M> {
            let fg = if act { h.0[5] } else { h.0[3] };
            let cols = if act { (h.0[7], h.0[7], fg) } else { (Color::TRANSPARENT, h.0[6], fg) };
            let mut r = row![].spacing(8).align_y(Alignment::Center);
            if let Some(icon) = ic {
                r = r.push(icon_svg(icon, 14.0, if act { h.0[5] } else { h.0[4] }));
            }
            r = r.push(txt(lbl, sz, if act { Weight::Semibold } else { Weight::Medium }, fg).width(Fill));
            if act {
                r = r.push(icon_svg(UiIcon::Check, 13.0, h.0[5]));
            }
            pbtn(r, [6, 9], cols, 8.0, wrap(msg)).width(Fill).into()
        };
        let mut items: Vec<Element<'a, M>> = vec![container(txt("WEATHER LAYER", 9.5, Weight::Bold, h.0[4])).padding([2, 8]).into()];
        items.extend(
            WeatherOverlay::ALL.map(|ov| item(Some(overlay_icon(ov)), ov.label(), 12.5, self.overlay == ov, MapMsg::SelectOverlay(ov))),
        );
        items.push(container(hairline(h.0[2])).padding([4, 4]).into());
        items.push(container(txt("MAP STYLE", 9.5, Weight::Bold, h.0[4])).padding([2, 8]).into());
        // Apple Maps-style visual thumbnail grid for base map styles!
        let style_card = |st: BaseMapStyle| -> Element<'a, M> {
            let act = self.base_style == st;
            let short = match st {
                BaseMapStyle::CleanLight => "Standard",
                BaseMapStyle::AppleDark => "Dark",
                BaseMapStyle::Satellite => "Satellite",
                BaseMapStyle::Terrain => "Topo",
                BaseMapStyle::Streets => "Streets",
            };
            let fg = if act { h.0[5] } else { h.0[3] };
            let bg = if act { h.0[7] } else { Color::TRANSPARENT };
            let card = column![
                Canvas::new(MapStyleSwatch(st, act)).width(Fill).height(30),
                txt(short, 10.5, if act { Weight::Semibold } else { Weight::Medium }, fg),
            ]
            .spacing(3)
            .align_x(Alignment::Center)
            .width(Fill);
            pbtn(card, [4, 4], (bg, h.0[6], fg), 8.0, wrap(MapMsg::SelectBaseStyle(st))).width(Fill).into()
        };
        let mut cards: Vec<Element<'a, M>> = BaseMapStyle::ALL.into_iter().map(style_card).collect();
        let tail = cards.split_off(3);
        items.push(row(cards).spacing(4).width(Fill).into());
        items.push(row(tail).push(sp_w(Fill)).spacing(4).width(Fill).into());
        items.push(container(hairline(h.0[2])).padding([4, 4]).into());
        items.push(item(Some(UiIcon::Moon), "Day / Night Shading", 12.0, self.show_terminator, MapMsg::ToggleTerminator));
        let op_btns: Vec<Element<'a, M>> = [(0.60_f32, "60%"), (0.85, "85%"), (1.00, "100%")]
            .into_iter()
            .map(|(v, lbl)| {
                let act = (self.overlay_opacity - v).abs() < 0.06;
                let (fg, bg) = if act { (h.0[5], h.0[7]) } else { (h.0[4], Color::TRANSPARENT) };
                pbtn(
                    container(txt(lbl, 10.5, if act { Weight::Semibold } else { Weight::Medium }, fg)).center_x(Fill),
                    [4, 6],
                    (bg, h.0[6], fg),
                    7.0,
                    wrap(MapMsg::SetOverlayOpacity(v)),
                )
                .width(Fill)
                .into()
            })
            .collect();
        items.push(
            row![txt("Opacity", 11.5, Weight::Medium, h.0[4]), row(op_btns).spacing(3).width(Fill)]
                .spacing(8)
                .align_y(Alignment::Center)
                .padding([3, 8])
                .into(),
        );
        container(column(items).spacing(2).width(226)).padding(8).style(hud_pill(h.0[0], h.0[1], 13.0)).into()
    }

    fn view_timeline<'a, M: Clone + 'a>(&self, h: Hud, loc: Option<&'a Location>, s: &Settings, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
        if !self.overlay.has_timeline() {
            return sp_h(0);
        }
        let Some(tl) = self.active_timeline() else {
            let load = container(txt("Loading precipitation radar…", 11.5, Weight::Medium, h.0[4]))
                .padding([8, 16])
                .style(hud_pill(h.0[0], h.0[1], 14.0));
            return container(shield(load.into(), wrap)).width(Fill).center_x(Fill).into();
        };
        let (max_i, live_i) = (tl.max_idx(), tl.live_idx);
        if max_i == 0 {
            return sp_h(0);
        }
        let pos = self.radar.play_pos.clamp(0.0, max_i as f32);
        let cur_i = (pos.round() as usize).min(max_i);
        let ts = tl.ts_at_pos(pos);
        let live_ts = tl.live_ts();
        let dm = ((ts - live_ts) as f64 / 60.0).round() as i64;
        let future = cur_i > live_i || dm > 4;
        let is_live = cur_i == live_i && dm.abs() <= 4;
        let buffering = self.radar.playing && self.radar.stall_secs >= 0.45;
        let layer = self.overlay.label();
        let (status, scol) = if buffering {
            (format!("{layer} • Buffering…"), h.0[5])
        } else if is_live {
            (format!("{layer} • Live"), h.0[3])
        } else if future {
            (format!("{layer} • {}", fmt_dm(dm.max(1), false)), h.0[5])
        } else {
            (format!("{layer} • {}", fmt_dm(dm.min(-1), false)), h.0[3])
        };
        let tbase = match loc {
            Some(l) => {
                let day =
                    if l.local_date(ts) == l.local_date(live_ts) { "Today".into() } else { l.local_date(ts).format("%a").to_string() };
                format!("{day}, {}", l.format_local_time(ts, s.use_24h_time))
            }
            None => chrono::DateTime::from_timestamp(ts, 0).map_or_else(String::new, |d| d.format("%a, %H:%M UTC").to_string()),
        };
        let src_tag = if self.overlay == WeatherOverlay::RadarTimeline {
            self.manifest.as_ref().and_then(|m| m.frames.get(cur_i)).map(|f| match f.source {
                RadarFrameSource::Tile(..) if !f.is_nowcast => "Radar",
                RadarFrameSource::Tile(..) => "Radar Nowcast",
                // Quarter-hour slots between ECCC's hourly model outputs are motion-interpolated by the app, not issued
                // by the model: say so rather than presenting them as upstream forecast frames.
                RadarFrameSource::NoaaHrrr(..) | RadarFrameSource::EcccNwp if f.eccc_slot().1 != 0 => "Interpolated Forecast",
                RadarFrameSource::NoaaHrrr(..) | RadarFrameSource::EcccNwp => "Model Forecast",
            })
        } else if self.overlay == WeatherOverlay::InfraredClouds {
            Some("Satellite IR")
        } else {
            None
        };
        let tstr = match src_tag {
            Some(tag) => format!("{tbase} • {tag}"),
            None => tbase,
        };
        let (al, ac, abg, ahv, amsg): (String, Color, Color, Color, MapMsg) = if !is_live {
            ("Now".into(), h.0[3], h.0[6], h.0[7], MapMsg::JumpToLive)
        } else if live_i == max_i && live_i > 0 {
            let past_mins = ((live_ts - tl.times[0]) as f64 / 60.0).round() as i64;
            let want_mins = if past_mins >= 360 { 360 } else { past_mins.max(60) };
            let jump_i = tl.nearest_idx(live_ts - want_mins * 60).min(live_i - 1);
            let jump_dm = ((tl.times[jump_i] - live_ts) as f64 / 60.0).round() as i64;
            (format!("‹ {}", fmt_dm(jump_dm.min(-1), true)), h.0[5], h.0[7], rgba8(10, 132, 255, 0.28), MapMsg::ScrubRadar(jump_i))
        } else {
            let fcst_mins = ((tl.times[max_i] - live_ts) as f64 / 60.0).round() as i64;
            let want_mins = if fcst_mins >= 180 {
                180
            } else if fcst_mins >= 120 {
                120
            } else {
                fcst_mins.max(30)
            };
            let jump_i = tl.nearest_idx(live_ts + want_mins * 60).max((live_i + 1).min(max_i));
            let jump_dm = ((tl.times[jump_i] - live_ts) as f64 / 60.0).round() as i64;
            (format!("{} ›", fmt_dm(jump_dm.max(1), true)), h.0[5], h.0[7], rgba8(10, 132, 255, 0.28), MapMsg::ScrubRadar(jump_i))
        };
        let (pbg, phv) =
            if h.1 { (rgba8(255, 255, 255, 0.14), rgba8(255, 255, 255, 0.24)) } else { (rgba8(20, 32, 52, 0.08), rgba8(20, 32, 52, 0.15)) };
        let ic = if self.radar.playing { UiIcon::Pause } else { UiIcon::Play };
        let play = icon_btn(ic, 12.5, (30, 28), (h.0[3], pbg, phv), 14.0, wrap(MapMsg::ToggleRadarPlay));
        let spd_lbl = if self.playback_speed < 0.9 {
            "0.5×"
        } else if self.playback_speed > 1.5 {
            "2×"
        } else {
            "1×"
        };
        let spd_col = if (self.playback_speed - 1.0).abs() > 0.1 { h.0[5] } else { h.0[4] };
        let spd_btn =
            pbtn(txt(spd_lbl, 10.5, Weight::Semibold, spd_col), [5, 7], (pbg, phv, spd_col), 11.0, wrap(MapMsg::CyclePlaybackSpeed));
        let left_ctrls = container(row![play, spd_btn].spacing(5).align_y(Alignment::Center)).width(94).align_x(Alignment::Start);
        let srow = if is_live || buffering {
            let dot_col = if buffering { h.0[5] } else { rgb8(52, 199, 89) };
            let dot = container(sp_w(6)).width(6).height(6).style(flat_box(dot_col, 3.0));
            row![dot, txt(status, 12.0, Weight::Semibold, scol)].spacing(6).align_y(Alignment::Center)
        } else {
            row![txt(status, 12.0, Weight::Semibold, scol)].align_y(Alignment::Center)
        };
        let mid = column![srow, txt(tstr, 10.5, Weight::Medium, h.0[4])].spacing(1).align_x(Alignment::Center).width(Fill);
        let right_el: Element<'a, M> = if self.overlay == WeatherOverlay::RadarTimeline && is_live {
            let r_lbl = if self.precip_short_range { "1h" } else { "12h" };
            let (r_fg, r_bg) = if self.precip_short_range { (h.0[5], h.0[7]) } else { (h.0[4], pbg) };
            let rng_btn = pbtn(txt(r_lbl, 10.5, Weight::Semibold, r_fg), [5, 7], (r_bg, phv, r_fg), 11.0, wrap(MapMsg::TogglePrecipRange));
            let jump_btn = pbtn(txt(al, 10.5, Weight::Semibold, ac), [5, 8], (abg, ahv, ac), 11.0, wrap(amsg));
            row![rng_btn, jump_btn].spacing(4).align_y(Alignment::Center).into()
        } else {
            pbtn(txt(al, 11.0, Weight::Semibold, ac), [5, 10], (abg, ahv, ac), 12.0, wrap(amsg)).into()
        };
        let right_ctrls = container(right_el).width(94).align_x(Alignment::End);
        let top = row![left_ctrls, mid, right_ctrls].spacing(6).align_y(Alignment::Center);
        let ticks = build_scrubber_ticks(&tl);
        let scrub = Canvas::new(Scrubber { tl, pos, ticks, loc, use_24h: s.use_24h_time, hud: h, wrap }).width(Fill).height(42);
        let pill = container(column![top, scrub].spacing(0).width(Fill))
            .width(Fill)
            .max_width(440)
            .padding([8, 12])
            .style(hud_pill(h.0[0], h.0[1], 15.0));
        container(shield(pill.into(), wrap)).width(Fill).center_x(Fill).into()
    }

    fn view_inspector<'a, M: Clone + 'a>(&self, h: Hud, saved: &[Location], s: &Settings, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
        let close =
            pbtn(icon_svg(UiIcon::Close, 11.5, h.0[4]), [4, 4], (Color::TRANSPARENT, h.0[6], h.0[3]), 10.0, wrap(MapMsg::CloseInspector));
        let body: Element<'a, M> = if self.inspect.loading {
            row![txt("Loading weather…", 12.0, Weight::Medium, h.0[3]).width(Fill), close].align_y(Alignment::Center).into()
        } else if let Some(b) = &self.inspect.bundle {
            let now = chrono::Utc::now().timestamp();
            let ltime = b.location.format_local_time(now, s.use_24h_time);
            let sub = if !b.location.region.is_empty() {
                format!("{}, {} • {ltime}", b.location.region, b.location.country_code)
            } else {
                format!("{:.2}°, {:.2}° • {ltime}", b.location.lat, b.location.lon)
            };
            let hl = b.daily.first().map_or_else(
                || format!("Feels {}", s.format_temp(b.current.feels_like_c)),
                |d| format!("H:{}  L:{}", s.format_temp(d.temp_max_c), s.format_temp(d.temp_min_c)),
            );
            let hcol = column![txt(b.location.name.clone(), 13.5, Weight::Semibold, h.0[3]), txt(sub, 10.5, Weight::Normal, h.0[4])]
                .spacing(1)
                .width(Fill);
            let head = row![hcol, close].align_y(Alignment::Start);
            let mcol =
                column![txt(b.current.condition.label(), 11.5, Weight::Semibold, h.0[3]), txt(hl, 10.5, Weight::Medium, h.0[4])].spacing(1);
            let wic = weather_icon_svg(b.current.condition, b.current.is_day, 30.0);
            let mid =
                row![wic, txt(s.format_temp(b.current.temp_c), 28.0, Weight::Medium, h.0[3]), mcol].spacing(8).align_y(Alignment::Center);
            let wtxt = txt(s.format_wind(b.current.wind_speed_mps), 10.0, Weight::Medium, h.0[4]);
            let htxt = txt(format!("Hum {:.0}%", b.current.humidity_pct), 10.0, Weight::Medium, h.0[4]);
            let ftxt = txt(format!("Feels {}", s.format_temp(b.current.feels_like_c)), 10.0, Weight::Medium, h.0[4]);
            let meta =
                row![icon_svg(UiIcon::Wind, 11.0, h.0[4]), wtxt, sp_w(Fill), ftxt, sp_w(Fill), htxt].spacing(4).align_y(Alignment::Center);
            let next3: Vec<Element<'a, M>> = b
                .hourly
                .iter()
                .filter(|hr| hr.timestamp_unix >= now + 1800)
                .take(4)
                .map(|hr| {
                    column![
                        txt(b.location.format_local_time(hr.timestamp_unix, s.use_24h_time), 9.5, Weight::Medium, h.0[4]),
                        weather_icon_svg(hr.condition, hr.is_day, 16.0),
                        txt(s.format_temp(hr.temp_c), 11.0, Weight::Semibold, h.0[3]),
                    ]
                    .spacing(2)
                    .align_x(Alignment::Center)
                    .width(Fill)
                    .into()
                })
                .collect();
            let already = saved.iter().any(|l| (l.lat - b.location.lat).hypot(wrap_lon(l.lon - b.location.lon)) < 0.08);
            let add_lbl =
                container(txt(if already { "Open Location" } else { "+ Add to Locations" }, 11.5, Weight::Semibold, Color::WHITE))
                    .center_x(Fill);
            let add_cols = (rgb8(0, 122, 255), rgb8(28, 144, 255), Color::WHITE);
            let add = pbtn(add_lbl, [6, 10], add_cols, 9.0, wrap(MapMsg::SaveInspectedLocation)).width(Fill);
            let mut col = column![head, mid, meta].spacing(7);
            if let Some(alert) = b.alerts.first() {
                let (abg, afg) = match alert.severity {
                    AlertSeverity::Extreme | AlertSeverity::Severe => (rgba8(255, 59, 48, 0.18), rgb8(255, 69, 58)),
                    _ => (rgba8(255, 149, 0, 0.18), rgb8(255, 159, 10)),
                };
                let pill = container(
                    row![icon_svg(UiIcon::Alert, 12.0, afg), txt(alert.event.clone(), 10.5, Weight::Semibold, afg)]
                        .spacing(5)
                        .align_y(Alignment::Center),
                )
                .padding([5, 8])
                .width(Fill)
                .style(flat_box(abg, 8.0));
                col = col.push(pill);
            }
            if !next3.is_empty() {
                col = col.push(hairline(h.0[2])).push(row(next3).spacing(4).width(Fill));
            }
            col.push(add).into()
        } else {
            let err = self.inspect.error.clone().unwrap_or_else(|| "Unable to load weather for this point.".into());
            row![text(err).size(11.0).color(rgb8(235, 78, 72)).width(Fill), close].spacing(6).align_y(Alignment::Center).into()
        };
        container(body).width(244).padding(12).style(hud_pill(h.0[0], h.0[1], 14.0)).into()
    }

    fn view_context_menu<'a, M: Clone + 'a>(&self, h: Hud, cm: ContextMenuState, wrap: fn(MapMsg) -> M) -> Element<'a, M> {
        let menu_w = 196.0_f32;
        let menu_h = 186.0_f32;
        let left_pad = cm.screen.x.clamp(12.0, (cm.vp.width - menu_w - 12.0).max(12.0));
        let top_pad = cm.screen.y.clamp(12.0, (cm.vp.height - menu_h - 12.0).max(12.0));
        let coord_lbl = {
            let (ns, ew) = (if cm.lat >= 0.0 { "N" } else { "S" }, if cm.lon >= 0.0 { "E" } else { "W" });
            format!("{:.3}° {ns}, {:.3}° {ew}", cm.lat.abs(), cm.lon.abs())
        };
        let row_btn = |ic: UiIcon, lbl: &'static str, msg: MapMsg| -> Element<'a, M> {
            let r =
                row![icon_svg(ic, 13.0, h.0[4]), txt(lbl, 12.0, Weight::Medium, h.0[3]).width(Fill)].spacing(8).align_y(Alignment::Center);
            pbtn(r, [6, 9], (Color::TRANSPARENT, h.0[6], h.0[3]), 7.0, wrap(msg)).width(Fill).into()
        };
        let menu = container(
            column![
                container(txt(coord_lbl, 9.5, Weight::Semibold, h.0[4])).padding([3, 9]),
                hairline(h.0[2]),
                row_btn(UiIcon::Pin, "Inspect Weather Here", MapMsg::ContextInspect),
                row_btn(UiIcon::Locate, "Center Map Here", MapMsg::ContextCenter),
                row_btn(UiIcon::Plus, "Zoom In", MapMsg::ContextZoom(1.2)),
                row_btn(UiIcon::Minus, "Zoom Out", MapMsg::ContextZoom(-1.2)),
                hairline(h.0[2]),
                row_btn(UiIcon::Copy, "Copy Coordinates", MapMsg::ContextCopyCoords),
            ]
            .spacing(2)
            .width(menu_w),
        )
        .padding(6)
        .style(hud_pill(h.0[0], h.0[1], 12.0));
        let placed = column![sp_h(top_pad), row![sp_w(left_pad), shield(menu.into(), wrap)]].width(Fill).height(Fill);
        mouse_area(placed).on_press(wrap(MapMsg::CloseContextMenu)).on_right_press(wrap(MapMsg::CloseContextMenu)).into()
    }
}
