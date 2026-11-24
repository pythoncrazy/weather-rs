use crate::app::{Msg, WeatherApp};
use crate::astro::compute_moon_info;
use crate::icons::{UiIcon, WidgetCanvas, icon_svg, owned_widget_canvas, sun_event_svg, weather_icon_svg};
use crate::map::MapMsg;
use crate::model::{
    AlertSeverity, Location, PrecipUnit, PressureUnit, WeatherBundle, WeatherCondition, WindUnit, compass_direction, uv_category,
};
use crate::style::{
    SkyPalette, alert_card, flat_box, frosted_card, frosted_card_btn, fw, hairline, hud_pill, pbtn, rgb8, rgba8, sleek_scrollable, sp_h,
    sp_w, t, tb, tbold, tm, txt,
};
use chrono::{Datelike, NaiveDate};
use iced::font::Weight;
use iced::widget::canvas::{self, Canvas, Frame, Path, Stroke};
use iced::widget::{button, column, container, mouse_area, row, scrollable, text};
use iced::{Alignment, Color, Element, Length::Fill, Point, Rectangle, Size};

type El<'a> = Element<'a, Msg>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetailMetric {
    Temperature,
    Precipitation,
    Wind,
    UvIndex,
    Pressure,
}

impl DetailMetric {
    const ALL: [Self; 5] = [Self::Temperature, Self::Precipitation, Self::Wind, Self::UvIndex, Self::Pressure];
    fn title(self) -> &'static str {
        match self {
            Self::Temperature => "TEMPERATURE",
            Self::Precipitation => "PRECIPITATION",
            Self::Wind => "WIND",
            Self::UvIndex => "UV INDEX",
            Self::Pressure => "PRESSURE",
        }
    }
    fn tab_label(self) -> &'static str {
        match self {
            Self::Temperature => "Temp",
            Self::Precipitation => "Precip",
            Self::Wind => "Wind",
            Self::UvIndex => "UV",
            Self::Pressure => "Pressure",
        }
    }
    fn icon(self) -> UiIcon {
        match self {
            Self::Temperature => UiIcon::Thermometer,
            Self::Precipitation => UiIcon::Droplet,
            Self::Wind => UiIcon::Wind,
            Self::UvIndex => UiIcon::Sun,
            Self::Pressure => UiIcon::Gauge,
        }
    }
}

fn hdr<'b>(p: SkyPalette, ic: UiIcon, title: &str) -> El<'b> {
    row![icon_svg(ic, 12.0, p.text_muted), tb(title.to_string(), 10.5, p.text_muted)].spacing(5).align_y(Alignment::Center).into()
}
fn bento<'b>(p: SkyPalette, c: impl Into<El<'b>>, (w, h): (f32, f32), pad: u16) -> El<'b> {
    container(c).width(w).height(h).padding(pad).clip(true).style(frosted_card(p, 15.0)).into()
}
fn bento_click<'b>(p: SkyPalette, c: impl Into<El<'b>>, (w, h): (f32, f32), pad: u16, msg: Msg) -> El<'b> {
    button(container(c).width(Fill).height(Fill).clip(true))
        .width(w)
        .height(h)
        .padding(pad)
        .style(frosted_card_btn(p, 15.0))
        .on_press(msg)
        .into()
}
fn simple<'b>(p: SkyPalette, sz: (f32, f32), ic: UiIcon, title: &str, val: String, note: &str, click: Option<DetailMetric>) -> El<'b> {
    let col = column![
        hdr(p, ic, title),
        tb(val, 27.0, p.text_primary),
        sp_h(Fill),
        text(note.to_string()).size(10.5).line_height(1.18).color(p.text_secondary)
    ];
    match click {
        Some(m) => bento_click(p, col.spacing(3).height(Fill), sz, 12, Msg::OpenDetail(m)),
        None => bento(p, col.spacing(3).height(Fill), sz, 12),
    }
}
fn spec(
    sig: u64, p: SkyPalette, sz: (f32, f32), (ic, title): (UiIcon, &str), (val, cat): (String, &'static str),
    (frac, click): (f32, Option<DetailMetric>), note: String,
) -> El<'static> {
    let compact = sz.0 < 175.0 || sz.1 < 145.0;
    let (vsz, csz, nsz, pad) = if compact { (22.5, 12.0, 10.0, 10) } else { (26.0, 13.0, 10.5, 12) };
    let col = column![
        hdr(p, ic, title),
        tb(val, vsz, p.text_primary),
        tb(cat, csz, p.text_primary),
        owned_widget_canvas(sig, WidgetCanvas::Spectrum(frac), Fill, 10),
        sp_h(Fill),
        text(note).size(nsz).line_height(1.18).color(p.text_secondary),
    ];
    match click {
        Some(m) => bento_click(p, col.spacing(2).height(Fill), sz, pad, Msg::OpenDetail(m)),
        None => bento(p, col.spacing(2).height(Fill), sz, pad),
    }
}

/// Zero-allocation stack-buffered YYYY-MM-DD comparison for today's daily forecast row.
pub(crate) fn today_daily_index_at(b: &WeatherBundle, loc: &Location, now: i64) -> usize {
    let d = loc.local_date(now);
    let (y, m, day) = (d.year(), d.month(), d.day());
    let buf = [
        b'0' + ((y / 1000) % 10) as u8,
        b'0' + ((y / 100) % 10) as u8,
        b'0' + ((y / 10) % 10) as u8,
        b'0' + (y % 10) as u8,
        b'-',
        b'0' + (m / 10) as u8,
        b'0' + (m % 10) as u8,
        b'-',
        b'0' + (day / 10) as u8,
        b'0' + (day % 10) as u8,
    ];
    b.daily.iter().position(|row| row.date.as_bytes() >= &buf[..]).unwrap_or(0)
}

impl WeatherApp {
    pub(crate) fn view_hero(&self, b: &WeatherBundle, p: SkyPalette, di: usize) -> El<'static> {
        let loc = self.active_location().unwrap_or(&b.location);
        let hd: El<'static> = if loc.is_current_location {
            let tag = row![icon_svg(UiIcon::Locate, 10.5, p.text_secondary), tb("MY LOCATION", 10.5, p.text_secondary)]
                .spacing(4)
                .align_y(Alignment::Center);
            column![tag, tm(loc.name.clone(), 28.0, p.text_primary)].spacing(1).align_x(Alignment::Center).into()
        } else {
            column![tm(loc.name.clone(), 28.0, p.text_primary), tb(loc.subtitle().to_uppercase(), 10.5, p.text_secondary)]
                .spacing(1)
                .align_x(Alignment::Center)
                .into()
        };
        let hl = b.daily.get(di).map_or_else(String::new, |d| {
            format!("H:{}  L:{}", self.settings.format_temp(d.temp_max_c), self.settings.format_temp(d.temp_min_c))
        });
        let mut c = column![
            hd,
            t(self.settings.format_temp(b.current.temp_c), 84.0, p.text_primary),
            tm(b.current.condition.label(), 17.0, p.text_secondary),
            tb(hl, 14.5, p.text_primary)
        ];
        // macOS Sequoia / Tahoe parity: prominently show Feels Like under H/L when it diverges noticeably from actual temp.
        if (b.current.feels_like_c - b.current.temp_c).abs() >= 2.5 {
            c = c.push(tm(format!("Feels Like {}", self.settings.format_temp(b.current.feels_like_c)), 12.5, p.text_secondary));
        }
        c.spacing(1).align_x(Alignment::Center).into()
    }

    pub(crate) fn view_alerts_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32) -> El<'static> {
        let accent = match b.alerts.first().map_or(AlertSeverity::Moderate, |a| a.severity) {
            AlertSeverity::Extreme => rgb8(255, 69, 58),
            AlertSeverity::Severe => rgb8(255, 159, 10),
            AlertSeverity::Moderate => rgb8(255, 214, 10),
            AlertSeverity::Minor => rgb8(100, 210, 255),
        };
        let bc = if p.is_light {
            (rgba8(24, 42, 72, 0.08), rgba8(24, 42, 72, 0.16), p.text_primary)
        } else {
            (rgba8(255, 255, 255, 0.14), rgba8(255, 255, 255, 0.24), p.text_primary)
        };
        let hd =
            row![icon_svg(UiIcon::Alert, 12.5, accent), tb("SEVERE WEATHER", 10.5, p.text_muted)].spacing(5).align_y(Alignment::Center);
        let mut rows: Vec<El<'static>> = vec![hd.into()];
        for (i, a) in b.alerts.iter().enumerate() {
            let open = self.expanded_alert_idx == Some(i);
            let info =
                column![tb(a.event.clone(), 13.0, p.text_primary), text(a.headline.clone()).size(11.0).color(p.text_secondary)].width(Fill);
            let btn = pbtn(tm(if open { "Hide" } else { "Details" }, 11.0, p.text_primary), [3, 9], bc, 7.0, Msg::ToggleAlertDetails(i));
            rows.push(row![info, btn].spacing(8).align_y(Alignment::Center).into());
            let _ = (open && !a.description.is_empty())
                .then(|| rows.push(text(reflow_alert_text(&a.description)).size(11.5).color(p.text_secondary).into()));
        }
        container(column(rows).spacing(6)).width(w).padding(13).style(alert_card(p, accent)).into()
    }

    pub(crate) fn view_minutely_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, now: i64) -> Option<El<'static>> {
        let raw: Vec<_> = b
            .minutely
            .iter()
            .filter(|s| s.timestamp_unix >= now - 900 && s.timestamp_unix <= now + 4500)
            .map(|s| (s.timestamp_unix, s.precip_mm_hr.max(0.0)))
            .collect();
        if raw.len() < 2 || !raw.iter().any(|&(_, r)| r > 0.06) {
            return None;
        }
        let samp: Vec<f64> = (0..13)
            .map(|i| {
                let ts = now + i * 300;
                let k = raw.partition_point(|&(t, _)| t < ts);
                let (a, b) = (raw[k.saturating_sub(1).min(raw.len() - 1)], raw[k.min(raw.len() - 1)]);
                a.1 + (b.1 - a.1) * ((ts - a.0) as f64 / (b.0 - a.0).max(1) as f64).clamp(0.0, 1.0)
            })
            .collect();
        if !samp.iter().any(|&r| r > 0.06) {
            return None;
        }
        let max_r = samp.iter().copied().fold(0.0_f64, f64::max);
        let hl = if samp[0] <= 0.06 {
            "Precipitation starting within the hour."
        } else if max_r > 4.0 {
            "Heavy precipitation continuing over the next hour."
        } else {
            "Precipitation continuing over the next hour."
        };
        let tk = |s: &'static str| text(s).size(10.0).color(p.text_muted);
        let ticks = row![
            tb("Now", 10.0, p.text_primary),
            sp_w(Fill),
            tk("15m"),
            sp_w(Fill),
            tk("30m"),
            sp_w(Fill),
            tk("45m"),
            sp_w(Fill),
            tk("60m")
        ]
        .width(Fill);
        let col = column![
            hdr(p, UiIcon::Umbrella, "NEXT-HOUR PRECIPITATION"),
            tb(hl, 12.5, p.text_primary),
            owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Minutely(samp, p.is_light), Fill, 48),
            ticks
        ]
        .spacing(6);
        Some(container(col).width(w).padding(13).style(frosted_card(p, 15.0)).into())
    }

    pub(crate) fn view_hourly_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, max_n: usize, now: i64) -> El<'static> {
        let (loc, h24) = (self.active_location().unwrap_or(&b.location), self.settings.use_24h_time);
        let i0 = b.hourly.partition_point(|h| h.timestamp_unix < now - 3000);
        let slice = &b.hourly[i0..(i0 + max_n).min(b.hourly.len())];
        let summary = slice.get(2).map_or_else(
            || format!("{} conditions right now.", b.current.condition.label()),
            |h3| {
                format!(
                    "{} conditions expected around {}, with winds near {}.",
                    h3.condition.label(),
                    loc.format_local_hour(h3.timestamp_unix, h24),
                    self.settings.format_wind(b.current.wind_speed_mps)
                )
            },
        );
        let mut sun: Vec<_> = b
            .daily
            .iter()
            .take(3)
            .flat_map(|d| d.sunrise_unix.map(|t| (t, true, "Sunrise")).into_iter().chain(d.sunset_unix.map(|t| (t, false, "Sunset"))))
            .collect();
        sun.sort_by_key(|e| e.0);
        // Compute exact column width so visible timestamps never clip mid-character at the right edge (e.g. "19:0" in split-map!).
        let inner_w = (w - 26.0).max(200.0);
        let col_gap = 6.0_f32;
        let vis_cols = ((inner_w + col_gap) / 56.0).floor().max(5.0);
        let col_w = ((inner_w - col_gap * (vis_cols - 1.0)) / vis_cols).clamp(44.0, 56.0);
        let sun_w = (col_w + 6.0).min(60.0);
        let mut cols: Vec<El<'static>> = Vec::with_capacity(slice.len() + 2);
        for (i, h) in slice.iter().enumerate() {
            let hr = if i == 0 { "Now".into() } else { loc.format_local_hour(h.timestamp_unix, h24) };
            let (cond, day, tc) =
                if i == 0 { (b.current.condition, b.current.is_day, b.current.temp_c) } else { (h.condition, h.is_day, h.temp_c) };
            let ic = icon_prob_cell(cond, day, h.precip_prob_pct, (21.0, 24.0, 9.5, 4.0, 46.0), None, p.is_light);
            let c = column![tb(hr, 11.5, p.text_primary), ic, tm(self.settings.format_temp(tc), 14.5, p.text_primary)]
                .spacing(5)
                .width(col_w)
                .align_x(Alignment::Center);
            cols.push(c.into());
            let next_ts = slice.get(i + 1).map_or(h.timestamp_unix + 3600, |nh| nh.timestamp_unix);
            for &(ets, rise, name) in &sun {
                if ets >= h.timestamp_unix && ets < next_ts {
                    let sc = column![
                        tb(loc.format_local_time(ets, h24), 11.0, p.text_primary),
                        container(sun_event_svg(rise, 23.0)).height(46).center_y(46),
                        tm(name, 13.0, p.text_primary)
                    ];
                    cols.push(sc.spacing(5).width(sun_w).align_x(Alignment::Center).into());
                }
            }
        }
        let dir = scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(3).scroller_width(3));
        let strip = scrollable(row(cols).spacing(col_gap).padding(iced::Padding { top: 2.0, right: 0.0, bottom: 7.0, left: 0.0 }))
            .id("hourly-strip-scroll")
            .direction(dir)
            .width(Fill)
            .style(sleek_scrollable);
        let hhv = if p.is_light { rgba8(20, 40, 72, 0.06) } else { rgba8(255, 255, 255, 0.08) };
        let sum_btn = pbtn(
            row![tm(summary, 12.0, p.text_primary).width(Fill), tb("24h ›", 11.0, p.text_muted)].spacing(6).align_y(Alignment::Center),
            [2, 4],
            (Color::TRANSPARENT, hhv, p.text_primary),
            7.0,
            Msg::OpenDetail(DetailMetric::Temperature),
        )
        .width(Fill);
        container(column![sum_btn, hairline(p.divider), strip].spacing(7))
            .width(w)
            .padding(iced::Padding { top: 11.0, right: 13.0, bottom: 8.0, left: 13.0 })
            .style(frosted_card(p, 15.0))
            .into()
    }

    pub(crate) fn view_ten_day_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h_opt: Option<f32>, di0: usize) -> El<'static> {
        let loc = self.active_location().unwrap_or(&b.location);
        let days = &b.daily[di0..(di0 + 10).min(b.daily.len())];
        let (omin, omax) = (
            days.iter().map(|d| d.temp_min_c).fold(f64::INFINITY, f64::min),
            days.iter().map(|d| d.temp_max_c).fold(f64::NEG_INFINITY, f64::max),
        );
        let (hbg, abg, dbg) = if p.is_light {
            (rgba8(20, 40, 72, 0.06), rgba8(10, 122, 235, 0.09), rgba8(24, 48, 86, 0.05))
        } else {
            (rgba8(255, 255, 255, 0.08), rgba8(255, 255, 255, 0.12), rgba8(255, 255, 255, 0.07))
        };
        let row_h = match (h_opt, self.selected_day_idx) {
            (Some(h), None) if !days.is_empty() => ((h - 46.0 - days.len() as f32 * 5.0) / days.len() as f32).clamp(32.0, 36.5),
            _ => 33.0,
        };
        let mut rows: Vec<El<'static>> = vec![hdr(p, UiIcon::Calendar, "10-DAY FORECAST")];
        for (i, d) in days.iter().enumerate() {
            rows.push(hairline(p.divider));
            let lbl = if i == 0 {
                "Today".into()
            } else {
                NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").map_or_else(|_| d.date.clone(), |nd| nd.weekday().to_string())
            };
            let (exp, compact) = (self.selected_day_idx == Some(i), w < 220.0);
            let bar = owned_widget_canvas(
                self.bento_sig.get().wrapping_add(i as u64 + 1),
                WidgetCanvas::TempBar((d.temp_min_c, d.temp_max_c), (omin, omax), (i == 0).then_some(b.current.temp_c), p.is_light),
                Fill,
                16,
            );
            let ic = icon_prob_cell(
                d.condition,
                true,
                d.precip_prob_pct,
                if compact { (13.0, 16.0, 8.0, 1.5, 28.0) } else { (15.0, 19.0, 8.5, 2.5, 31.0) },
                Some(if compact { 23.0 } else { 30.0 }),
                p.is_light,
            );
            let (dw, tw, fsz, sp) = if compact { (36.0, 24.0, 11.5, 3) } else { (48.0, 32.0, 13.5, 5) };
            let lo = container(tm(self.settings.format_temp(d.temp_min_c), fsz, p.text_muted)).width(tw).align_x(Alignment::End);
            let hi = container(tb(self.settings.format_temp(d.temp_max_c), fsz, p.text_primary)).width(tw).align_x(Alignment::End);
            let line = row![container(tb(lbl, fsz, p.text_primary)).width(dw), ic, lo, bar, hi]
                .spacing(sp)
                .height(row_h)
                .align_y(Alignment::Center);
            let bg = if exp { abg } else { Color::TRANSPARENT };
            rows.push(pbtn(line, [0, 4], (bg, hbg, p.text_primary), 8.0, Msg::ToggleDayDetails(i)).width(Fill).into());
            if exp {
                let h24 = self.settings.use_24h_time;
                let sun = d.sunrise_unix.zip(d.sunset_unix).map_or_else(
                    || "—".into(),
                    |(sr, ss)| format!("{} / {}", loc.format_local_time(sr, h24), loc.format_local_time(ss, h24)),
                );
                let pv = if d.precip_sum_mm >= 0.1 {
                    format!("{} ({:.0}%)", self.settings.format_precip(d.precip_sum_mm), d.precip_prob_pct)
                } else {
                    format!("{:.0}%", d.precip_prob_pct)
                };
                let uv_t = text(format!("UV {:.0} • {}", d.uv_max.round(), uv_category(d.uv_max))).size(10.5).color(p.text_secondary);
                let r1 = row![tb(d.condition.label(), 11.5, p.text_primary), sp_w(Fill), uv_t].align_y(Alignment::Center);
                let wt = text(format!("Wind {}", self.settings.format_wind(d.wind_max_mps))).size(10.5).color(p.text_secondary);
                let r2 = row![
                    text(format!("Precip {pv}")).size(10.5).color(p.text_secondary),
                    sp_w(Fill),
                    wt,
                    sp_w(Fill),
                    text(sun).size(10.5).color(p.text_muted)
                ]
                .align_y(Alignment::Center);
                rows.push(container(column![r1, r2].spacing(3)).width(Fill).padding([6, 8]).style(flat_box(dbg, 9.0)).into());
            }
        }
        let inner: El<'static> = if h_opt.is_some() && self.selected_day_idx.is_some() {
            scrollable(column(rows).spacing(2)).height(Fill).style(sleek_scrollable).into()
        } else {
            column(rows).spacing(2).into()
        };
        let mut c = container(inner).width(w).padding(13).clip(true).style(frosted_card(p, 15.0));
        if let Some(h) = h_opt {
            c = c.height(h);
        }
        c.into()
    }

    pub(crate) fn view_aqi_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let Some(a) = &b.air_quality else {
            return simple(p, (w, h), UiIcon::Air, "AIR QUALITY", "--".into(), "Air quality feed unavailable.", None);
        };
        let (cat, sum) = a.info();
        let note = if w < 200.0 { format!("PM2.5 • {:.1} µg/m³", a.pm2_5) } else { sum.to_string() };
        let cat_row: El<'static> = if w >= 200.0 {
            row![tb(cat, 13.0, p.text_primary), sp_w(Fill), tm(format!("PM2.5 {:.1}", a.pm2_5), 10.5, p.text_muted)]
                .align_y(Alignment::Center)
                .into()
        } else {
            tb(cat, 13.0, p.text_primary).into()
        };
        let pos = (a.us_aqi as f32 / 250.0).clamp(0.02, 0.98);
        let col = column![
            hdr(p, UiIcon::Air, "AIR QUALITY"),
            tb(a.us_aqi.to_string(), 24.0, p.text_primary),
            cat_row,
            owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Spectrum(pos), Fill, 10),
            sp_h(Fill),
            text(note).size(10.5).color(p.text_secondary)
        ];
        bento(p, col.spacing(3).height(Fill), (w, h), 12)
    }

    pub(crate) fn view_wind_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let (sv, su) = self.settings.wind_value_and_unit(b.current.wind_speed_mps);
        let (gv, gu) = match b.current.wind_gust_mps {
            Some(g) => self.settings.wind_value_and_unit(g),
            None => ("--".into(), su),
        };
        let compact = w < 200.0;
        let (vsz, usz, lsz, sp, dsz, pad) = if compact { (17.5, 7.5, 9.5, 4, 52.0, 10) } else { (24.0, 8.5, 11.0, 7, 88.0, 12) };
        let stat = |v: String, u: &'static str, l: &'static str| {
            row![tb(v, vsz, p.text_primary), column![tbold(u.to_uppercase(), usz, p.text_muted), tm(l, lsz, p.text_primary)]]
                .spacing(if compact { 4 } else { 6 })
                .align_y(Alignment::Center)
        };
        let lhs = column![stat(sv.clone(), su, "Wind"), hairline(p.divider), stat(gv, gu, "Gusts")].spacing(sp).width(Fill);
        let body = row![lhs, owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Compass(b.current.wind_dir_deg, sv, su, p), dsz, dsz)]
            .spacing(if compact { 5 } else { 8 })
            .height(Fill)
            .align_y(Alignment::Center);
        bento_click(
            p,
            column![hdr(p, UiIcon::Wind, "WIND"), body].spacing(if compact { 2 } else { 4 }),
            (w, h),
            pad,
            Msg::OpenDetail(DetailMetric::Wind),
        )
    }

    pub(crate) fn view_map_bento_card(&self, p: SkyPalette, w: f32, h: f32) -> El<'_> {
        let sz = Size::new((w - 22.0).max(80.0), (h - 40.0).max(80.0));
        let m = self.map.view_mini_card(sz, self.active_location(), &self.bundles, &self.settings, p.is_light, Msg::Map(MapMsg::ExpandMap));
        bento(p, column![hdr(p, UiIcon::Umbrella, "PRECIPITATION"), m].spacing(7), (w, h), 11)
    }

    pub(crate) fn view_uv_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let uv = b.current.uv_index;
        let adv = if uv < 3.0 {
            "Low for the rest of the day."
        } else if uv < 6.0 {
            "Use sun protection until late afternoon."
        } else {
            "Seek shade during midday hours."
        };
        spec(
            self.bento_sig.get(),
            p,
            (w, h),
            (UiIcon::Sun, "UV INDEX"),
            (format!("{uv:.0}"), uv_category(uv)),
            ((uv as f32 / 11.0).clamp(0.02, 0.98), Some(DetailMetric::UvIndex)),
            adv.into(),
        )
    }

    pub(crate) fn view_sun_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, now: i64, di: usize) -> El<'static> {
        let (loc, fmt) = (self.active_location().unwrap_or(&b.location), |t: i64| {
            self.active_location().unwrap_or(&b.location).format_local_time(t, self.settings.use_24h_time)
        });
        let _ = loc;
        let (r0, s0, r1) = (
            b.daily.get(di).and_then(|d| d.sunrise_unix),
            b.daily.get(di).and_then(|d| d.sunset_unix),
            b.daily.get(di + 1).and_then(|d| d.sunrise_unix),
        );
        let (ic, title, mts, sl, sts) = if !r0.is_some_and(|r| now < r) && s0.is_some_and(|s| now < s) {
            (UiIcon::Sunset, "SUNSET", s0, "Sunrise", r1.or(r0))
        } else {
            (UiIcon::Sunrise, "SUNRISE", if r0.is_some_and(|r| now < r) { r0 } else { r1.or(r0) }, "Sunset", s0)
        };
        let foot = text(sts.map(|t| format!("{sl}: {}", fmt(t))).unwrap_or_default()).size(10.5).color(p.text_secondary);
        let col = column![
            hdr(p, ic, title),
            tb(mts.map(fmt).unwrap_or_else(|| "--:--".into()), 22.0, p.text_primary),
            owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::SunArc(r0, s0, now, p.is_light), Fill, 38),
            sp_h(Fill),
            foot
        ];
        bento(p, col.spacing(2).height(Fill), (w, h), 12)
    }

    pub(crate) fn view_feels_like_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let d = b.current.feels_like_c - b.current.temp_c;
        let note = if d <= -1.5 {
            "Wind is making it feel cooler."
        } else if d >= 1.5 {
            "Humidity is making it feel warmer."
        } else {
            "Similar to the actual temperature."
        };
        simple(
            p,
            (w, h),
            UiIcon::Thermometer,
            "FEELS LIKE",
            self.settings.format_temp(b.current.feels_like_c),
            note,
            Some(DetailMetric::Temperature),
        )
    }

    pub(crate) fn view_precip_summary_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, di: usize) -> El<'static> {
        let (today, tmrw) =
            (b.daily.get(di).map_or(b.current.precip_mm, |d| d.precip_sum_mm), b.daily.get(di + 1).map_or(0.0, |d| d.precip_sum_mm));
        let next =
            if tmrw > 0.1 { format!("{} expected tomorrow.", self.settings.format_precip(tmrw)) } else { "Dry conditions ahead.".into() };
        let col = column![
            hdr(p, UiIcon::Droplet, "PRECIPITATION"),
            tb(self.settings.format_precip(today), 26.0, p.text_primary),
            tb("Today", 12.0, p.text_primary),
            sp_h(Fill),
            text(next).size(10.5).line_height(1.18).color(p.text_secondary)
        ];
        bento_click(p, col.spacing(2).height(Fill), (w, h), 12, Msg::OpenDetail(DetailMetric::Precipitation))
    }

    pub(crate) fn view_moon_card(&self, p: SkyPalette, w: f32, h: f32, now: i64) -> El<'static> {
        let (m, compact) = (compute_moon_info(now), w < 245.0);
        let fd = |n: i32, z: &'static str| match n {
            0 => z.to_string(),
            1 => "1 DAY".into(),
            _ => format!("{n} DAYS"),
        };
        let fsz = if compact { 9.8 } else { 11.0 };
        let kv = |l: &'static str, v: String| row![tm(l, fsz, p.text_primary), sp_w(Fill), text(v).size(fsz).color(p.text_secondary)];
        let st = column![
            kv("Illumination", format!("{:.0}%", m.illumination_pct)),
            hairline(p.divider),
            kv("Lunar Age", fd(m.age_days.round() as i32, "0 DAYS")),
            hairline(p.divider),
            kv(if compact { "Full Moon" } else { "Next Full Moon" }, fd((14.765 - m.age_days).rem_euclid(29.53).round() as i32, "TODAY"))
        ]
        .spacing(6)
        .width(Fill);
        let msz = if compact { 64 } else { 80 };
        let body = row![st, owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Moon(m.phase), msz, msz)]
            .spacing(if compact { 8 } else { 12 })
            .height(Fill)
            .align_y(Alignment::Center);
        bento(p, column![hdr(p, UiIcon::Moon, &m.phase_name.to_uppercase()), body].spacing(4), (w, h), 12)
    }

    pub(crate) fn view_humidity_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let note = format!("The dew point is {} right now.", self.settings.format_temp(b.current.dew_point_c));
        simple(p, (w, h), UiIcon::Humidity, "HUMIDITY", format!("{:.0}%", b.current.humidity_pct), &note, None)
    }

    pub(crate) fn view_visibility_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let (val, note) = match b.current.visibility_m {
            Some(v) => {
                let n = if v >= 14_000.0 {
                    "Perfectly clear view."
                } else if v >= 7_000.0 {
                    "Light haze on horizon."
                } else {
                    "Reduced by mist or rain."
                };
                (self.settings.format_visibility(v), n)
            }
            None => ("--".into(), "No visibility observation."),
        };
        simple(p, (w, h), UiIcon::Eye, "VISIBILITY", val, note, None)
    }

    pub(crate) fn view_pressure_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, now: i64) -> El<'static> {
        let (val, unit) = match self.settings.pressure_unit {
            PressureUnit::HPa => (format!("{:.0}", b.current.pressure_hpa), "hPa"),
            PressureUnit::InHg => (format!("{:.2}", b.current.pressure_hpa * 0.02953), "inHg"),
        };
        let i0 = b.hourly.partition_point(|h| h.timestamp_unix < now);
        let trend = b.hourly.get(i0 + 3).map_or(0_i8, |h3| {
            let dp = h3.pressure_hpa - b.current.pressure_hpa;
            if dp >= 0.8 {
                1
            } else if dp <= -0.8 {
                -1
            } else {
                0
            }
        });
        let col = column![
            hdr(p, UiIcon::Gauge, "PRESSURE"),
            owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Pressure(b.current.pressure_hpa, trend, val, unit, p), Fill, Fill)
        ];
        bento_click(p, col.spacing(2).height(Fill), (w, h), 12, Msg::OpenDetail(DetailMetric::Pressure))
    }

    pub(crate) fn view_averages_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, di: usize) -> El<'static> {
        let slice = &b.daily[di..];
        let today = slice.first().map_or(b.current.temp_c, |d| d.temp_max_c);
        let mean = if slice.is_empty() { today } else { slice.iter().map(|d| d.temp_max_c).sum::<f64>() / slice.len() as f64 };
        let rd = (self.settings.convert_temp(today) - self.settings.convert_temp(mean)).round();
        let delta = if rd.abs() < 0.5 { 0.0 } else { rd };
        let sub = if delta > 0.0 {
            "above 10-day avg high"
        } else if delta < 0.0 {
            "below 10-day avg high"
        } else {
            "near 10-day avg high"
        };
        let hi_row = |l: &'static str, tc: f64| {
            row![
                text(l).size(10.5).color(p.text_muted),
                sp_w(Fill),
                tb(format!("H:{}", self.settings.format_temp(tc)), 10.5, p.text_primary)
            ]
        };
        let col = column![
            hdr(p, UiIcon::Trend, "10-DAY AVG"),
            tb(format!("{}{delta:.0}°", if delta > 0.0 { "+" } else { "" }), 25.0, p.text_primary),
            tm(sub, 10.0, p.text_secondary),
            sp_h(Fill),
            hairline(p.divider),
            hi_row("Today", today),
            hi_row("10-Day Avg", mean)
        ];
        bento_click(p, col.spacing(2).height(Fill), (w, h), 12, Msg::OpenDetail(DetailMetric::Temperature))
    }

    /// Signature macOS Weather floating 24-hour interactive detail chart sheet with hover scrub line & metric tabs.
    pub(crate) fn view_detail_overlay(&self, metric: DetailMetric, pal: SkyPalette, now: i64) -> El<'static> {
        let Some(b) = self.active_bundle() else {
            return sp_w(0);
        };
        let loc = self.active_location().unwrap_or(&b.location);
        let lt = pal.is_light;
        let (sheet_bg, sheet_bdr, fg, sub_fg, muted_fg, track_bg, div_col, hov_bg, tile_bg) = if lt {
            (
                rgba8(246, 249, 254, 0.97),
                rgba8(20, 40, 75, 0.14),
                rgb8(18, 26, 40),
                rgba8(36, 52, 76, 0.84),
                rgba8(74, 90, 114, 0.78),
                rgba8(20, 40, 75, 0.07),
                rgba8(20, 40, 75, 0.11),
                rgba8(20, 40, 75, 0.12),
                rgba8(20, 40, 75, 0.045),
            )
        } else {
            (
                rgba8(20, 28, 42, 0.96),
                rgba8(255, 255, 255, 0.18),
                Color::WHITE,
                rgba8(224, 234, 248, 0.86),
                rgba8(186, 204, 228, 0.70),
                rgba8(255, 255, 255, 0.08),
                rgba8(255, 255, 255, 0.13),
                rgba8(255, 255, 255, 0.14),
                rgba8(255, 255, 255, 0.06),
            )
        };
        let di = today_daily_index_at(b, loc, now);
        let i0 = b.hourly.partition_point(|h| h.timestamp_unix < now - 1800);
        let hrs = &b.hourly[i0..(i0 + 24).min(b.hourly.len())];
        let h24 = self.settings.use_24h_time;

        let close = pbtn(icon_svg(UiIcon::Close, 13.0, fg), [5, 5], (track_bg, hov_bg, fg), 12.0, Msg::CloseDetail);
        let title_row = row![
            icon_svg(metric.icon(), 14.0, pal.accent),
            txt(metric.title(), 12.5, Weight::Bold, sub_fg),
            txt("·", 12.0, Weight::Medium, muted_fg),
            txt(loc.name.clone(), 12.5, Weight::Semibold, fg),
            sp_w(Fill),
            close
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let tabs: Vec<El<'_>> = DetailMetric::ALL
            .into_iter()
            .map(|m| {
                let act = m == metric;
                let cols = if act { (rgb8(10, 132, 255), rgb8(28, 146, 255), Color::WHITE) } else { (Color::TRANSPARENT, hov_bg, sub_fg) };
                pbtn(
                    container(txt(m.tab_label(), 11.5, if act { Weight::Semibold } else { Weight::Medium }, cols.2))
                        .width(Fill)
                        .center_x(Fill),
                    [5, 8],
                    cols,
                    7.0,
                    Msg::OpenDetail(m),
                )
                .width(Fill)
                .into()
            })
            .collect();
        let tab_bar = container(row(tabs).spacing(3).width(Fill)).padding(3).width(Fill).style(flat_box(track_bg, 10.0));

        // Build chart series, hero value, axis bounds, summary tiles, and guidance copy for the active metric.
        let mut pts: Vec<ChartPt> = Vec::with_capacity(hrs.len());
        let (hero_val, hero_sub, accent, is_bars, y_min, y_max, y_unit, stats, summary) = match metric {
            DetailMetric::Temperature => {
                for (k, h) in hrs.iter().enumerate() {
                    let tc = if k == 0 { b.current.temp_c } else { h.temp_c };
                    let v = self.settings.convert_temp(tc);
                    let hr = if k == 0 { "Now".into() } else { loc.format_local_hour(h.timestamp_unix, h24) };
                    pts.push(ChartPt {
                        hour_lbl: hr,
                        val: v,
                        aux: h.precip_prob_pct,
                        callout: format!("{} · {}", self.settings.format_temp(tc), h.condition.label()),
                    });
                }
                let lo = pts.iter().map(|p| p.val).fold(f64::INFINITY, f64::min);
                let hi = pts.iter().map(|p| p.val).fold(f64::NEG_INFINITY, f64::max);
                let avg = if pts.is_empty() { 0.0 } else { pts.iter().map(|p| p.val).sum::<f64>() / pts.len() as f64 };
                let pad = ((hi - lo) * 0.22).max(3.0);
                let s = [
                    ("24H HIGH", format!("{hi:.0}°")),
                    ("24H LOW", format!("{lo:.0}°")),
                    ("FEELS LIKE", self.settings.format_temp(b.current.feels_like_c)),
                ];
                let note = format!(
                    "Temperatures range from {lo:.0}° to {hi:.0}° over the next 24 hours (average {avg:.0}°). Hover the chart to inspect any hour."
                );
                (
                    self.settings.format_temp(b.current.temp_c),
                    format!("{} • Feels like {}", b.current.condition.label(), self.settings.format_temp(b.current.feels_like_c)),
                    rgb8(255, 168, 46),
                    false,
                    (lo - pad).floor(),
                    (hi + pad).ceil(),
                    "°",
                    s,
                    note,
                )
            }
            DetailMetric::Precipitation => {
                let to_u = |mm: f64| if self.settings.precip_unit == PrecipUnit::Inches { mm / 25.4 } else { mm };
                let u = if self.settings.precip_unit == PrecipUnit::Inches { " in" } else { " mm" };
                for (k, h) in hrs.iter().enumerate() {
                    let v = to_u(h.precip_mm.max(0.0));
                    let hr = if k == 0 { "Now".into() } else { loc.format_local_hour(h.timestamp_unix, h24) };
                    pts.push(ChartPt {
                        hour_lbl: hr,
                        val: v,
                        aux: h.precip_prob_pct,
                        callout: format!("{} ({:.0}%)", self.settings.format_precip(h.precip_mm), h.precip_prob_pct),
                    });
                }
                let tot_mm: f64 = hrs.iter().map(|h| h.precip_mm.max(0.0)).sum();
                let max_v = pts.iter().map(|p| p.val).fold(0.0_f64, f64::max);
                let max_p = hrs.iter().map(|h| h.precip_prob_pct).fold(0.0_f64, f64::max);
                let top = if self.settings.precip_unit == PrecipUnit::Inches { (max_v * 1.25).max(0.24) } else { (max_v * 1.25).max(3.0) };
                let s = [
                    ("24H TOTAL", self.settings.format_precip(tot_mm)),
                    ("PEAK CHANCE", format!("{max_p:.0}%")),
                    ("TODAY", self.settings.format_precip(b.daily.get(di).map_or(tot_mm, |d| d.precip_sum_mm))),
                ];
                let note = if tot_mm > 0.2 {
                    format!(
                        "Total accumulation of {} expected over the next 24 hours, peaking at {max_p:.0}% probability.",
                        self.settings.format_precip(tot_mm)
                    )
                } else {
                    "Little to no measurable precipitation expected over the next 24 hours.".into()
                };
                (
                    self.settings.format_precip(tot_mm),
                    "Expected in the next 24 hours".into(),
                    rgb8(58, 178, 255),
                    true,
                    0.0,
                    top,
                    u,
                    s,
                    note,
                )
            }
            DetailMetric::Wind => {
                let conv = |mps: f64| match self.settings.wind_unit {
                    WindUnit::KmH => mps * 3.6,
                    WindUnit::Mph => mps * 2.23694,
                    WindUnit::Ms => mps,
                    WindUnit::Knots => mps * 1.94384,
                };
                let (_, unit_lbl) = self.settings.wind_value_and_unit(b.current.wind_speed_mps);
                for (k, h) in hrs.iter().enumerate() {
                    let mps = if k == 0 { b.current.wind_speed_mps } else { h.wind_speed_mps };
                    let deg = if k == 0 { b.current.wind_dir_deg } else { h.wind_dir_deg };
                    let hr = if k == 0 { "Now".into() } else { loc.format_local_hour(h.timestamp_unix, h24) };
                    pts.push(ChartPt {
                        hour_lbl: hr,
                        val: conv(mps),
                        aux: deg,
                        callout: format!("{} {}", self.settings.format_wind(mps), compass_direction(deg)),
                    });
                }
                let hi = pts.iter().map(|p| p.val).fold(0.0_f64, f64::max);
                let avg = if pts.is_empty() { 0.0 } else { pts.iter().map(|p| p.val).sum::<f64>() / pts.len() as f64 };
                let gust_lbl = b.current.wind_gust_mps.map_or_else(|| "--".into(), |g| self.settings.format_wind(g));
                let s = [
                    ("PEAK 24H", format!("{hi:.0} {unit_lbl}")),
                    ("GUSTS NOW", gust_lbl),
                    ("DIRECTION", compass_direction(b.current.wind_dir_deg).into()),
                ];
                let note = format!(
                    "Winds average {avg:.0} {unit_lbl} over the next 24 hours from {}, peaking near {hi:.0} {unit_lbl}.",
                    compass_direction(b.current.wind_dir_deg)
                );
                let sub = match b.current.wind_gust_mps {
                    Some(g) => format!("From {} • Gusts {}", compass_direction(b.current.wind_dir_deg), self.settings.format_wind(g)),
                    None => format!("From {}", compass_direction(b.current.wind_dir_deg)),
                };
                (
                    self.settings.format_wind(b.current.wind_speed_mps),
                    sub,
                    rgb8(82, 212, 200),
                    false,
                    0.0,
                    (hi * 1.25).max(10.0).ceil(),
                    "",
                    s,
                    note,
                )
            }
            DetailMetric::UvIndex => {
                let d0 = b.daily.get(di);
                let uv_peak_day = d0.map_or(b.current.uv_index.max(3.0), |d| d.uv_max.max(b.current.uv_index));
                let (sr, ss) = (d0.and_then(|d| d.sunrise_unix), d0.and_then(|d| d.sunset_unix));
                for (k, h) in hrs.iter().enumerate() {
                    let uv = if k == 0 {
                        b.current.uv_index
                    } else if let (Some(r), Some(s)) = (sr, ss) {
                        // Project solar day cycle into the hour's time-of-day (handles next-day hours seamlessly)
                        let span = (s - r).max(3600) as f64;
                        let off = ((h.timestamp_unix - r).rem_euclid(86400)) as f64;
                        if off >= 0.0 && off <= span {
                            let sun_sin = (std::f64::consts::PI * off / span).sin().powf(1.25);
                            let cloud_att = match h.condition {
                                WeatherCondition::Clear | WeatherCondition::MostlyClear => 1.0,
                                WeatherCondition::PartlyCloudy => 0.82,
                                WeatherCondition::Overcast | WeatherCondition::Fog => 0.52,
                                _ => 0.35,
                            };
                            (uv_peak_day * sun_sin * cloud_att).clamp(0.0, 12.0)
                        } else {
                            0.0
                        }
                    } else if h.is_day {
                        (uv_peak_day * 0.6).clamp(0.0, 11.0)
                    } else {
                        0.0
                    };
                    let hr = if k == 0 { "Now".into() } else { loc.format_local_hour(h.timestamp_unix, h24) };
                    pts.push(ChartPt { hour_lbl: hr, val: uv, aux: 0.0, callout: format!("UV {uv:.1} · {}", uv_category(uv)) });
                }
                let peak = pts.iter().map(|p| p.val).fold(0.0_f64, f64::max);
                let s = [
                    ("CURRENT", format!("{:.0} {}", b.current.uv_index, uv_category(b.current.uv_index))),
                    ("PEAK UV", format!("{peak:.1} {}", uv_category(peak))),
                    (
                        "GUIDANCE",
                        if peak >= 6.0 {
                            "SPF 30+".into()
                        } else if peak >= 3.0 {
                            "Shade Midday".into()
                        } else {
                            "Low Risk".into()
                        },
                    ),
                ];
                let note = if peak >= 6.0 {
                    "High midday UV expected. Use broad-spectrum sun protection and sunglasses between late morning and mid-afternoon."
                        .into()
                } else if peak >= 3.0 {
                    "Moderate UV levels around solar noon. Light sun protection is advised outdoors.".into()
                } else {
                    "UV levels remain low throughout the next 24 hours. No special sun protection needed.".into()
                };
                (
                    format!("{:.0}", b.current.uv_index),
                    uv_category(b.current.uv_index).into(),
                    rgb8(255, 198, 42),
                    false,
                    0.0,
                    (peak.max(6.0) + 1.5).ceil().min(12.0),
                    "",
                    s,
                    note,
                )
            }
            DetailMetric::Pressure => {
                let in_hg = self.settings.pressure_unit == PressureUnit::InHg;
                let conv = |hpa: f64| if in_hg { hpa * 0.02953 } else { hpa };
                for (k, h) in hrs.iter().enumerate() {
                    let hpa = if k == 0 { b.current.pressure_hpa } else { h.pressure_hpa };
                    let v = conv(hpa);
                    let hr = if k == 0 { "Now".into() } else { loc.format_local_hour(h.timestamp_unix, h24) };
                    let lbl = if in_hg { format!("{v:.2} inHg") } else { format!("{v:.0} hPa") };
                    pts.push(ChartPt { hour_lbl: hr, val: v, aux: 0.0, callout: lbl });
                }
                let lo = pts.iter().map(|p| p.val).fold(f64::INFINITY, f64::min);
                let hi = pts.iter().map(|p| p.val).fold(f64::NEG_INFINITY, f64::max);
                let dp3 = hrs.get(3).map_or(0.0, |h3| h3.pressure_hpa - b.current.pressure_hpa);
                let trend = if dp3 >= 0.8 {
                    "Rising"
                } else if dp3 <= -0.8 {
                    "Falling"
                } else {
                    "Steady"
                };
                let pad = ((hi - lo) * 0.30).max(if in_hg { 0.12 } else { 4.0 });
                let fmt_p = |v: f64| if in_hg { format!("{v:.2} inHg") } else { format!("{v:.0} hPa") };
                let s = [("TREND (3H)", trend.into()), ("24H HIGH", fmt_p(hi)), ("24H LOW", fmt_p(lo))];
                let note = format!(
                    "Barometric pressure is currently {} and {trend}. Hover the timeline to inspect hourly changes.",
                    fmt_p(conv(b.current.pressure_hpa))
                );
                (
                    fmt_p(conv(b.current.pressure_hpa)),
                    format!("{trend} over the next 3 hours"),
                    rgb8(130, 174, 255),
                    false,
                    lo - pad,
                    hi + pad,
                    "",
                    s,
                    note,
                )
            }
        };

        let hero = row![
            column![txt(hero_val, 30.0, Weight::Bold, fg), txt(hero_sub, 12.0, Weight::Medium, sub_fg)].spacing(1),
            sp_w(Fill),
            txt("Hover chart to scrub", 10.5, Weight::Medium, muted_fg),
        ]
        .align_y(Alignment::End);

        let chart = Canvas::new(DetailChartCanvas {
            pts,
            accent,
            is_bars,
            y_min,
            y_max,
            y_unit,
            decimals: if (metric == DetailMetric::Pressure && self.settings.pressure_unit == PressureUnit::InHg)
                || (metric == DetailMetric::Precipitation && self.settings.precip_unit == PrecipUnit::Inches)
            {
                2
            } else if metric == DetailMetric::Precipitation {
                1
            } else {
                0
            },
            light: lt,
        })
        .width(Fill)
        .height(176);
        let chart_box = container(chart)
            .width(Fill)
            .padding(iced::Padding { top: 8.0, right: 6.0, bottom: 4.0, left: 6.0 })
            .style(flat_box(tile_bg, 12.0));

        let stat_tiles: Vec<El<'_>> = stats
            .into_iter()
            .map(|(k, v)| {
                container(column![txt(k, 9.5, Weight::Bold, muted_fg), txt(v, 14.0, Weight::Semibold, fg)].spacing(2))
                    .width(Fill)
                    .padding([8, 10])
                    .style(flat_box(tile_bg, 10.0))
                    .into()
            })
            .collect();
        let body = column![
            title_row,
            tab_bar,
            hero,
            chart_box,
            row(stat_tiles).spacing(8).width(Fill),
            hairline(div_col),
            text(summary).size(11.5).color(sub_fg),
        ]
        .spacing(10)
        .width(488);

        let modal = container(body).padding(18).style(hud_pill(sheet_bg, sheet_bdr, 16.0));
        let bg = container(mouse_area(modal).on_press(Msg::Noop))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .style(flat_box(rgba8(0, 0, 0, if lt { 0.28 } else { 0.44 }), 0.0));
        mouse_area(bg).on_press(Msg::CloseDetail).into()
    }
}

#[derive(Clone)]
struct ChartPt {
    hour_lbl: String,
    val: f64,
    aux: f64,
    callout: String,
}

struct DetailChartCanvas {
    pts: Vec<ChartPt>,
    accent: Color,
    is_bars: bool,
    y_min: f64,
    y_max: f64,
    y_unit: &'static str,
    decimals: usize,
    light: bool,
}

#[derive(Default)]
struct DetailChartState {
    hovered_idx: Option<usize>,
    base_cache: canvas::Cache,
    base_sig: std::cell::Cell<u64>,
}

impl canvas::Program<Msg> for DetailChartCanvas {
    type State = DetailChartState;

    fn update(
        &self, state: &mut Self::State, event: &iced::Event, bounds: Rectangle, cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Msg>> {
        if let iced::Event::Mouse(iced::mouse::Event::CursorMoved { .. } | iced::mouse::Event::CursorLeft) = event {
            let n = self.pts.len();
            let (pad_l, pad_r) = (10.0_f32, 46.0_f32);
            let pw = (bounds.width - pad_l - pad_r).max(10.0);
            let next = cursor
                .position_in(bounds)
                .filter(|p| n >= 2 && p.x >= pad_l - 8.0 && p.x <= pad_l + pw + 8.0)
                .map(|p| (((p.x - pad_l) / pw).clamp(0.0, 1.0) * (n - 1) as f32).round() as usize);
            // Only redraw when cursor actually crosses to a different hour bucket (0 GPU work on intra-hour pixel jitter!)
            if state.hovered_idx != next {
                state.hovered_idx = next;
                return Some(canvas::Action::request_redraw());
            }
        }
        None
    }

    fn draw(
        &self, state: &Self::State, renderer: &iced::Renderer, _: &iced::Theme, bounds: Rectangle, _: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        use std::hash::{Hash, Hasher};
        let (w, h) = (bounds.width, bounds.height);
        let n = self.pts.len();
        if n < 2 || w < 60.0 || h < 60.0 {
            return Vec::new();
        }
        let (pad_l, pad_r, pad_t, pad_b) = (10.0_f32, 46.0_f32, 24.0_f32, 22.0_f32);
        let (pw, ph) = ((w - pad_l - pad_r).max(10.0), (h - pad_t - pad_b).max(10.0));
        let span = (self.y_max - self.y_min).max(1e-4);
        let px = |i: usize| pad_l + (i as f32 / (n - 1) as f32) * pw;
        let py = |v: f64| pad_t + (1.0 - ((v - self.y_min) / span).clamp(0.0, 1.0) as f32) * ph;

        let mut sh = std::collections::hash_map::DefaultHasher::new();
        (w.to_bits(), h.to_bits(), self.is_bars, self.y_min.to_bits(), self.y_max.to_bits(), self.decimals, self.light, self.y_unit)
            .hash(&mut sh);
        for p in &self.pts {
            (p.val.to_bits(), p.aux.to_bits(), &p.hour_lbl).hash(&mut sh);
        }
        let sig = sh.finish() | 1;
        if state.base_sig.get() != sig {
            state.base_cache.clear();
            state.base_sig.set(sig);
        }
        let base_geom = state.base_cache.draw(renderer, bounds.size(), |f| {
            let grid_col = if self.light { rgba8(20, 40, 75, 0.10) } else { rgba8(255, 255, 255, 0.11) };
            let axis_col = if self.light { rgba8(56, 72, 96, 0.78) } else { rgba8(204, 220, 242, 0.66) };

            // 4 horizontal reference gridlines + crisp right-aligned Y-axis labels
            for step in 0..=3 {
                let frac = step as f32 / 3.0;
                let y = pad_t + frac * ph;
                f.stroke(
                    &Path::line(Point::new(pad_l, y), Point::new(pad_l + pw, y)),
                    Stroke::default().with_width(0.8).with_color(grid_col),
                );
                let val = self.y_max - (self.y_max - self.y_min) * frac as f64;
                let lbl = match self.decimals {
                    2 => format!("{val:.2}{}", self.y_unit),
                    1 => format!("{val:.1}{}", self.y_unit),
                    _ => format!("{val:.0}{}", self.y_unit),
                };
                f.fill_text(canvas::Text {
                    content: lbl,
                    position: Point::new(pad_l + pw + 6.0, y),
                    color: axis_col,
                    size: iced::Pixels(9.5),
                    font: fw(Weight::Medium),
                    align_x: iced::alignment::Horizontal::Left.into(),
                    align_y: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
            }

            // X-axis local hour ticks (every 4 hours + Now)
            for i in (0..n).step_by(4) {
                let x = px(i);
                f.stroke(
                    &Path::line(Point::new(x, pad_t), Point::new(x, pad_t + ph)),
                    Stroke::default().with_width(0.6).with_color(Color { a: grid_col.a * 0.65, ..grid_col }),
                );
                f.fill_text(canvas::Text {
                    content: self.pts[i].hour_lbl.clone(),
                    position: Point::new(x, pad_t + ph + 11.0),
                    color: if i == 0 { self.accent } else { axis_col },
                    size: iced::Pixels(9.5),
                    font: fw(if i == 0 { Weight::Semibold } else { Weight::Medium }),
                    align_x: iced::alignment::Horizontal::Center.into(),
                    align_y: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
            }

            if self.is_bars {
                // Sleek macOS Weather precipitation bars + probability trend line
                let bw = (pw / n as f32 * 0.62).clamp(4.0, 12.0);
                for (i, pt) in self.pts.iter().enumerate() {
                    let x = px(i);
                    let y = py(pt.val);
                    let bh = (pad_t + ph - y).max(if pt.val > 0.001 { 3.0 } else { 0.0 });
                    if bh > 0.5 {
                        let r = (bw * 0.35).min(3.5);
                        f.fill(
                            &Path::rounded_rectangle(Point::new(x - bw * 0.5, pad_t + ph - bh), Size::new(bw, bh), r.into()),
                            Color { a: 0.82, ..self.accent },
                        );
                    }
                }
                let prob_path = Path::new(|pb| {
                    for (i, pt) in self.pts.iter().enumerate() {
                        let p = Point::new(px(i), pad_t + (1.0 - (pt.aux / 100.0).clamp(0.0, 1.0) as f32) * ph);
                        if i == 0 {
                            pb.move_to(p);
                        } else {
                            pb.line_to(p);
                        }
                    }
                });
                f.stroke(&prob_path, Stroke::default().with_width(1.4).with_color(Color { a: 0.45, ..self.accent }));
            } else {
                // Smooth cubic-interpolated macOS Weather area fill + luminous curve
                let pts_xy: Vec<Point> = self.pts.iter().enumerate().map(|(i, p)| Point::new(px(i), py(p.val))).collect();
                let build_curve = |pb: &mut canvas::path::Builder| {
                    pb.move_to(pts_xy[0]);
                    for i in 0..pts_xy.len() - 1 {
                        let p0 = pts_xy[i.saturating_sub(1)];
                        let p1 = pts_xy[i];
                        let p2 = pts_xy[i + 1];
                        let p3 = pts_xy[(i + 2).min(pts_xy.len() - 1)];
                        let c1 = Point::new(p1.x + (p2.x - p0.x) / 6.0, (p1.y + (p2.y - p0.y) / 6.0).clamp(pad_t, pad_t + ph));
                        let c2 = Point::new(p2.x - (p3.x - p1.x) / 6.0, (p2.y - (p3.y - p1.y) / 6.0).clamp(pad_t, pad_t + ph));
                        pb.bezier_curve_to(c1, c2, p2);
                    }
                };
                let area = Path::new(|pb| {
                    build_curve(pb);
                    pb.line_to(Point::new(pts_xy[n - 1].x, pad_t + ph));
                    pb.line_to(Point::new(pts_xy[0].x, pad_t + ph));
                    pb.close();
                });
                let grad = canvas::gradient::Linear::new(Point::new(0.0, pad_t), Point::new(0.0, pad_t + ph))
                    .add_stop(0.0, Color { a: if self.light { 0.32 } else { 0.38 }, ..self.accent })
                    .add_stop(1.0, Color { a: 0.02, ..self.accent });
                f.fill(&area, grad);
                let line = Path::new(build_curve);
                f.stroke(&line, Stroke::default().with_width(2.4).with_color(self.accent));
                f.fill(&Path::circle(pts_xy[0], 5.5), Color { a: 0.28, ..self.accent });
                f.fill(&Path::circle(pts_xy[0], 3.2), Color::WHITE);
                f.stroke(&Path::circle(pts_xy[0], 3.2), Stroke::default().with_width(1.6).with_color(self.accent));
            }
        });

        // Interactive cursor scrub overlay (tiny 4-primitive layer on top of GPU-cached base chart!)
        let Some(idx) = state.hovered_idx else {
            return vec![base_geom];
        };
        let mut f = Frame::new(renderer, bounds.size());
        let idx = idx.min(n - 1);
        let pt = &self.pts[idx];
        let (sx, sy) = (px(idx), py(pt.val));
        let line_col = if self.light { rgba8(20, 40, 75, 0.35) } else { rgba8(255, 255, 255, 0.42) };
        f.stroke(&Path::line(Point::new(sx, pad_t), Point::new(sx, pad_t + ph)), Stroke::default().with_width(1.2).with_color(line_col));
        f.fill(&Path::circle(Point::new(sx, sy), 6.0), Color { a: 0.30, ..self.accent });
        f.fill(&Path::circle(Point::new(sx, sy), 3.4), Color::WHITE);
        f.stroke(&Path::circle(Point::new(sx, sy), 3.4), Stroke::default().with_width(1.8).with_color(self.accent));

        let txt_str = format!("{} · {}", pt.hour_lbl, pt.callout);
        let pill_w = (txt_str.len() as f32 * 5.9 + 16.0).clamp(84.0, pw);
        let pill_x = (sx - pill_w * 0.5).clamp(pad_l, (pad_l + pw - pill_w).max(pad_l));
        let pill_bg = if self.light { rgba8(255, 255, 255, 0.96) } else { rgba8(28, 38, 56, 0.96) };
        let pill_bdr = if self.light { rgba8(20, 40, 75, 0.16) } else { rgba8(255, 255, 255, 0.24) };
        let pill_fg = if self.light { rgb8(18, 26, 40) } else { Color::WHITE };
        let pr = Path::rounded_rectangle(Point::new(pill_x, 2.0), Size::new(pill_w, 19.0), 9.5.into());
        f.fill(&pr, pill_bg);
        f.stroke(&pr, Stroke::default().with_width(0.9).with_color(pill_bdr));
        f.fill_text(canvas::Text {
            content: txt_str,
            position: Point::new(pill_x + pill_w * 0.5, 11.5),
            color: pill_fg,
            size: iced::Pixels(10.0),
            font: fw(Weight::Semibold),
            align_x: iced::alignment::Horizontal::Center.into(),
            align_y: iced::alignment::Vertical::Center,
            ..Default::default()
        });

        vec![base_geom, f.into_geometry()]
    }
}

fn icon_prob_cell<'a>(
    cond: WeatherCondition, day: bool, prob: f64, (sz_p, sz_n, fsz, sp, h): (f32, f32, f32, f32, f32), w: Option<f32>, light: bool,
) -> El<'a> {
    let inner: El<'a> = if prob >= 25.0 {
        let pc = if light { rgb8(0, 122, 255) } else { rgb8(94, 202, 255) };
        column![weather_icon_svg(cond, day, sz_p), tbold(format!("{prob:.0}%"), fsz, pc)].spacing(sp).align_x(Alignment::Center).into()
    } else {
        weather_icon_svg(cond, day, sz_n)
    };
    let mut c = container(inner).height(h).center_y(h);
    if let Some(w) = w {
        c = c.width(w).center_x(w);
    }
    c.into()
}

/// Reflows legacy 60-column NOAA NWS teletype line breaks into smooth full-width paragraphs while keeping bullet/section breaks.
fn reflow_alert_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_blank = true;
    for line in raw.trim().lines() {
        let s = line.trim();
        if s.is_empty() {
            if !prev_blank && !out.is_empty() {
                out.push_str("\n\n");
            }
            prev_blank = true;
            continue;
        }
        let bullet = s.starts_with('*') || s.starts_with('-') || s.starts_with('•');
        if !out.is_empty() && !prev_blank {
            out.push(if bullet { '\n' } else { ' ' });
        }
        out.push_str(s);
        prev_blank = false;
    }
    out
}
