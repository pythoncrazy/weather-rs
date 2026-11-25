use crate::app::{Msg, WeatherApp};
use crate::astro::compute_moon_info;
use crate::icons::{UiIcon, WidgetCanvas, icon_svg, owned_widget_canvas, sun_event_svg, weather_icon_svg};
use crate::map::MapMsg;
use crate::model::{
    AlertSeverity, DailyForecast, Location, MINUTELY_STEP_SECS, PRESSURE_STEADY_HPA, PrecipUnit, PressureUnit, REFRESH_INTERVAL_SECS,
    WeatherBundle, WeatherCondition, WindUnit, compass_direction, uv_category, uv_display,
};
use crate::style::{
    SkyPalette, alert_card, flat_box, frosted_card, frosted_card_btn, fw, hairline, hud_pill, pbtn, rgb8, rgba8, sleek_scrollable, sp_h,
    sp_w, tb, tbold, tm, txt,
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

/// Shown wherever the source did not report a value.
pub(crate) const DASH: &str = "—";

/// WHO sun-protection guidance for the (rounded) UV index.
fn uv_guidance(uv: f64) -> &'static str {
    match uv.max(0.0).round() {
        u if u < 3.0 => "No protection needed",
        u if u < 8.0 => "Protection needed",
        _ => "Extra protection",
    }
}
/// UV card sentence, based on the highest real UV value left in the location's day (current value + later hourly
/// samples of today). Empty when that peak is unknown: nothing is said about the rest of the day from "now" alone.
pub(crate) fn uv_advice(b: &WeatherBundle, loc: &Location, now: i64, h24: bool) -> String {
    match b.uv_peak_rest_of_day(loc, now) {
        None => String::new(),
        Some((_, pk)) if pk.round() < 3.0 => "Low for the rest of the day.".into(),
        Some((ts, _)) if ts <= now => "At its highest for the rest of the day.".into(),
        Some((ts, pk)) => format!("Peaks at {} ({}) around {} today.", uv_display(pk), uv_category(pk), loc.format_local_hour(ts, h24)),
    }
}
/// "Rising" / "Falling" / "Steady" for a 3-hour pressure change taken from the hourly forecast series.
fn pressure_trend_label(dp: f64) -> &'static str {
    if dp >= PRESSURE_STEADY_HPA {
        "Rising"
    } else if dp <= -PRESSURE_STEADY_HPA {
        "Falling"
    } else {
        "Steady"
    }
}
/// "H:.. L:.." for a daily row; a row covering only the rest of the day says so instead of posing as the day's range.
pub(crate) fn hi_lo_label(s: &crate::model::Settings, d: &DailyForecast) -> String {
    let hl = format!("H:{}  L:{}", s.format_temp(d.temp_max_c), s.format_temp(d.temp_min_c));
    if d.partial { format!("Rest of today  {hl}") } else { hl }
}
/// Compact age such as "25 min", "3 h" or "2 d".
pub(crate) fn format_age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 3600 => format!("{} min", (s / 60).max(1)),
        s if s < 86_400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
}

/// Zero-allocation stack-buffered YYYY-MM-DD comparison for today's daily forecast row. Returns `b.daily.len()` (so
/// `b.daily.get(i)` is `None` and `b.daily[i..]` is empty) when the bundle has no row for today: an older or later row
/// is never presented as "Today".
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
    b.daily.iter().position(|row| row.date.as_bytes() == &buf[..]).unwrap_or(b.daily.len())
}

impl WeatherApp {
    /// Unobtrusive data-status line: the age of the oldest data on display once it exceeds the refresh interval, and
    /// the last refresh error even though a cached bundle is being shown. `None` while everything is current.
    pub(crate) fn data_status_line(&self, b: &WeatherBundle, now: i64) -> Option<String> {
        let age = now - b.data_time_unix();
        let stale = (age > REFRESH_INTERVAL_SECS + 120)
            .then(|| if b.fetched_unix > 0 { format!("Updated {} ago", format_age(age)) } else { "Update time unknown".into() });
        let err = self.refresh_error().map(|e| {
            let short: String = e.chars().take(90).collect();
            format!("Refresh failed: {short}{}", if short.len() < e.len() { "…" } else { "" })
        });
        let parts: Vec<String> = stale.into_iter().chain(err).collect();
        (!parts.is_empty()).then(|| parts.join(" • "))
    }

    pub(crate) fn view_hero(&self, b: &WeatherBundle, p: SkyPalette, di: usize, now: i64) -> El<'static> {
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
        let hl = b.daily.get(di).map_or_else(String::new, |d| hi_lo_label(&self.settings, d));
        let mut c = column![
            hd,
            txt(self.settings.format_temp(b.current.temp_c), 88.0, Weight::ExtraLight, p.text_primary),
            tm(b.current.condition.label(), 17.0, p.text_secondary),
            tb(hl, 14.5, p.text_primary)
        ];
        // macOS Sequoia / Tahoe parity: prominently show Feels Like under H/L when it diverges noticeably from actual temp.
        if let Some(fl) = b.current.feels_like_c.filter(|fl| (fl - b.current.temp_c).abs() >= 2.5) {
            c = c.push(tm(format!("Feels Like {}", self.settings.format_temp(fl)), 12.5, p.text_secondary));
        }
        if let Some(status) = self.data_status_line(b, now) {
            c = c.push(tm(status, 11.0, p.text_muted));
        }
        c.spacing(1).align_x(Alignment::Center).into()
    }

    pub(crate) fn view_alerts_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, now: i64) -> El<'static> {
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
        // Alerts kept from an earlier answer (the feed has not answered since) say when they were fetched.
        let as_of = b.alerts_fetched_unix.filter(|ts| now - ts > REFRESH_INTERVAL_SECS + 120).map_or_else(String::new, |ts| {
            let loc = self.active_location().unwrap_or(&b.location);
            format!("as of {} ({} ago)", loc.format_local_time(ts, self.settings.use_24h_time), format_age(now - ts))
        });
        let hd = row![
            icon_svg(UiIcon::Alert, 12.5, accent),
            tb("WEATHER ALERTS", 10.5, p.text_muted),
            sp_w(Fill),
            tm(as_of, 10.0, p.text_muted)
        ]
        .spacing(5)
        .align_y(Alignment::Center);
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
        // One segment per real 15-minute sample, clipped to the next hour: `(start, end, mm/h)`. A sample is stamped with
        // the end of the 15 minutes it covers. Nothing is interpolated or extended past the last sample: if the data
        // stops before the hour is up, so does the chart.
        let segs: Vec<(i64, i64, f64)> = b
            .minutely
            .iter()
            .map(|s| ((s.timestamp_unix - MINUTELY_STEP_SECS).max(now), s.timestamp_unix.min(now + 3600), s.precip_mm_hr))
            .filter(|&(a, e, _)| e > a)
            .collect();
        if !segs.iter().any(|s| s.2 > 0.0) {
            return None;
        }
        let (loc, h24) = (self.active_location().unwrap_or(&b.location), self.settings.use_24h_time);
        let at = |ts: i64| loc.format_local_time(ts, h24);
        let wet_now = segs.first().is_some_and(|s| s.0 <= now && s.2 > 0.0);
        let hl = if wet_now {
            match (segs.iter().find(|s| s.2 <= 0.0), segs.last()) {
                (Some(dry), _) => format!("Precipitation forecast to stop around {}.", at(dry.0)),
                (None, Some(last)) if last.1 < now + 3600 => format!("Precipitation forecast until at least {}.", at(last.1)),
                _ => "Precipitation forecast through the next hour.".into(),
            }
        } else {
            segs.iter().find(|s| s.2 > 0.0).map_or_else(String::new, |wet| format!("Precipitation forecast to start around {}.", at(wet.0)))
        };
        // Twelve 5-minute bars, each showing the rate of the real 15-minute sample that covers it (a restatement of the
        // sample, not an interpolation). The run stops at the first bar no sample covers and the chart is only as wide
        // as that run, so the axis stays true to time and nothing is drawn beyond the data.
        let slots: Vec<f64> = (0..12)
            .map_while(|k| {
                let mid = now + k * 300 + 150;
                segs.iter().find(|s| s.0 <= mid && mid < s.1).map(|s| s.2)
            })
            .collect();
        let (n, light) = (slots.len() as u16, p.is_light);
        if n == 0 {
            return None;
        }
        let bars = owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Minutely(slots, light), iced::Length::FillPortion(n), 48);
        let chart = if n < 12 { row![bars, sp_w(iced::Length::FillPortion(12 - n))] } else { row![bars] }.width(Fill);
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
        let col = column![hdr(p, UiIcon::Umbrella, "NEXT-HOUR PRECIPITATION"), tb(hl, 12.5, p.text_primary), chart, ticks].spacing(6);
        Some(container(col).width(w).padding(13).style(frosted_card(p, 15.0)).into())
    }

    pub(crate) fn view_hourly_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, max_n: usize, now: i64) -> El<'static> {
        let (loc, h24) = (self.active_location().unwrap_or(&b.location), self.settings.use_24h_time);
        let i0 = b.hourly.partition_point(|h| h.timestamp_unix < now - 3000);
        let slice = &b.hourly[i0..(i0 + max_n).min(b.hourly.len())];
        // The headline only restates what the strip itself shows: the forecast condition of one hour, at that hour.
        let summary = slice.get(2).map_or_else(
            || format!("{} right now.", b.current.condition.label()),
            |h3| format!("{} forecast at {}.", h3.condition.label(), loc.format_local_hour(h3.timestamp_unix, h24)),
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
        let uv_name = if b.uv_clear_sky { "Clear-sky UV" } else { "UV" };
        let (hbg, abg, dbg) = if p.is_light {
            (rgba8(20, 40, 72, 0.06), rgba8(10, 122, 235, 0.09), rgba8(24, 48, 86, 0.05))
        } else {
            (rgba8(255, 255, 255, 0.08), rgba8(255, 255, 255, 0.12), rgba8(255, 255, 255, 0.07))
        };
        let row_h = match (h_opt, self.selected_day_idx) {
            (Some(h), None) if !days.is_empty() => ((h - 46.0 - days.len() as f32 * 5.0) / days.len() as f32).clamp(32.0, 36.5),
            _ => 33.0,
        };
        let title = if days.is_empty() { "DAILY FORECAST".to_string() } else { format!("{}-DAY FORECAST", days.len()) };
        let mut rows: Vec<El<'static>> = vec![hdr(p, UiIcon::Calendar, &title)];
        for (i, d) in days.iter().enumerate() {
            rows.push(hairline(p.divider));
            // `di0` is today's row; when the source only covers the rest of today the row is labelled accordingly.
            let lbl = if i == 0 {
                if d.partial { "Later" } else { "Today" }.into()
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
                let pv = match (d.precip_sum_mm, d.precip_prob_pct) {
                    (Some(mm), Some(pr)) => format!("{} ({pr:.0}%)", self.settings.format_precip(mm)),
                    (Some(mm), None) => self.settings.format_precip(mm),
                    (None, Some(pr)) => format!("{pr:.0}%"),
                    (None, None) => DASH.into(),
                };
                let uv_s =
                    d.uv_max.map_or_else(|| format!("{uv_name} {DASH}"), |u| format!("{uv_name} {} • {}", uv_display(u), uv_category(u)));
                let uv_t = text(uv_s).size(10.5).color(p.text_secondary);
                let cond = if d.partial { format!("Rest of today: {}", d.condition.label()) } else { d.condition.label().into() };
                let r1 = row![tb(cond, 11.5, p.text_primary), sp_w(Fill), uv_t].align_y(Alignment::Center);
                let ws = d.wind_max_mps.map_or_else(|| DASH.into(), |w| self.settings.format_wind(w));
                let wt = text(format!("Max wind {ws}")).size(10.5).color(p.text_secondary);
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
            // The forecast is painted before the slower air-quality request returns: until this bundle's full fetch
            // has completed, the value is still on its way rather than unavailable.
            let pending = !b.lean && b.full_fetched_unix.is_none();
            let note = if pending { "Loading…" } else { "Not available from the air quality feed." };
            return simple(p, (w, h), UiIcon::Air, "AIR QUALITY", DASH.into(), note, None);
        };
        let (cat, sum) = a.info();
        let note = match a.pm2_5 {
            Some(pm) if w < 200.0 => format!("PM2.5 • {pm:.1} µg/m³"),
            _ => sum.to_string(),
        };
        let cat_row: El<'static> = match a.pm2_5 {
            Some(pm) if w >= 200.0 => row![tb(cat, 13.0, p.text_primary), sp_w(Fill), tm(format!("PM2.5 {pm:.1}"), 10.5, p.text_muted)]
                .align_y(Alignment::Center)
                .into(),
            _ => tb(cat, 13.0, p.text_primary).into(),
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
            None => (DASH.into(), su),
        };
        let compact = w < 200.0;
        let (vsz, usz, lsz, sp, dsz, pad) = if compact { (17.5, 7.5, 9.5, 4, 52.0, 10) } else { (24.0, 8.5, 11.0, 7, 88.0, 12) };
        let stat = |v: String, u: &'static str, l: &'static str| {
            // A missing value is a quiet muted dash, not a heavy white bar in the headline weight.
            let col = if v == DASH { p.text_muted } else { p.text_primary };
            row![tb(v, vsz, col), column![tbold(u.to_uppercase(), usz, p.text_muted), tm(l, lsz, p.text_primary)]]
                .spacing(if compact { 4 } else { 6 })
                .align_y(Alignment::Center)
        };
        let lhs = column![stat(sv.clone(), su, "Wind"), hairline(p.divider), stat(gv, gu, "Gusts")].spacing(sp).width(Fill);
        // No direction (calm / variable wind, or none reported): the dial is drawn without a needle.
        let dial = owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Compass(b.current.wind_dir_deg, sv, su, p), dsz, dsz);
        let body = row![lhs, dial].spacing(if compact { 5 } else { 8 }).height(Fill).align_y(Alignment::Center);
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

    pub(crate) fn view_uv_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, now: i64) -> El<'static> {
        let title = if b.uv_clear_sky { "UV (CLEAR SKY)" } else { "UV INDEX" };
        let click = Some(DetailMetric::UvIndex);
        let Some(uv) = b.current.uv_index else {
            return simple(p, (w, h), UiIcon::Sun, title, DASH.into(), "Not available from this source.", click);
        };
        let loc = self.active_location().unwrap_or(&b.location);
        spec(
            self.bento_sig.get(),
            p,
            (w, h),
            (UiIcon::Sun, title),
            (uv_display(uv), uv_category(uv)),
            ((uv.round() as f32 / 11.0).clamp(0.02, 0.98), click),
            uv_advice(b, loc, now, self.settings.use_24h_time),
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
            tb(mts.map(fmt).unwrap_or_else(|| DASH.into()), 22.0, p.text_primary),
            owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::SunArc(r0, s0, now, p.is_light), Fill, 38),
            sp_h(Fill),
            foot
        ];
        bento(p, col.spacing(2).height(Fill), (w, h), 12)
    }

    pub(crate) fn view_feels_like_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32) -> El<'static> {
        let click = Some(DetailMetric::Temperature);
        let Some(fl) = b.current.feels_like_c else {
            return simple(p, (w, h), UiIcon::Thermometer, "FEELS LIKE", DASH.into(), "Not available from this source.", click);
        };
        // The note states only the difference between the two temperatures as displayed, and how the value was obtained.
        let s = &self.settings;
        let d = (s.convert_temp(fl).round() - s.convert_temp(b.current.temp_c).round()) as i64;
        let mut note = match d {
            0 => "Same as the actual temperature.".to_string(),
            d if d < 0 => format!("{}° below the actual temperature.", -d),
            d => format!("{d}° above the actual temperature."),
        };
        if b.current.feels_like_computed {
            note.push_str(" Computed, not measured.");
        }
        simple(p, (w, h), UiIcon::Thermometer, "FEELS LIKE", s.format_temp(fl), &note, click)
    }

    pub(crate) fn view_precip_summary_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, di: usize) -> El<'static> {
        // For today, like Apple Weather: what fell in the last 24 hours and what is expected in the next 24 — when the
        // provider reports the past hours. Otherwise (another day selected, or no past data) the day's own totals.
        if let (0, Some(last)) = (di, b.precip_last_24h_mm) {
            let next = match b.precip_next_24h_mm {
                Some(mm) if mm > 0.0 => format!("{} expected in next 24h.", self.settings.format_precip(mm)),
                Some(_) => "None expected in next 24h.".into(),
                None => String::new(),
            };
            let col = column![
                hdr(p, UiIcon::Droplet, "PRECIPITATION"),
                tb(self.settings.format_precip(last), 26.0, p.text_primary),
                tb("in last 24h", 12.0, p.text_primary),
                sp_h(Fill),
                text(next).size(10.5).line_height(1.18).color(p.text_secondary)
            ];
            return bento_click(p, col.spacing(2).height(Fill), (w, h), 12, Msg::OpenDetail(DetailMetric::Precipitation));
        }
        let d0 = b.daily.get(di);
        let today = d0.and_then(|d| d.precip_sum_mm).map_or_else(|| DASH.into(), |mm| self.settings.format_precip(mm));
        // Only tomorrow's own total is stated; nothing is said about the days after it.
        let next = match b.daily.get(di + 1).filter(|_| d0.is_some()).and_then(|d| d.precip_sum_mm) {
            Some(mm) if mm > 0.0 => format!("{} forecast for tomorrow.", self.settings.format_precip(mm)),
            Some(_) => "None forecast for tomorrow.".into(),
            None => String::new(),
        };
        let col = column![
            hdr(p, UiIcon::Droplet, "PRECIPITATION"),
            tb(today, 26.0, p.text_primary),
            tb(if d0.is_some_and(|d| d.partial) { "Rest of today" } else { "Today" }, 12.0, p.text_primary),
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
        // The card states the reported value and, at most, that it is a lower bound; it does not describe the view.
        let (val, note) = match b.current.visibility_m {
            Some(v) if b.current.visibility_at_least => {
                (self.settings.format_visibility(v, true), "Reported as a minimum: this far or farther.")
            }
            Some(v) => (self.settings.format_visibility(v, false), ""),
            None => (DASH.into(), "Not reported by this source."),
        };
        simple(p, (w, h), UiIcon::Eye, "VISIBILITY", val, note, None)
    }

    pub(crate) fn view_pressure_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, now: i64) -> El<'static> {
        let click = Some(DetailMetric::Pressure);
        let Some(hpa) = b.current.pressure_hpa else {
            return simple(p, (w, h), UiIcon::Gauge, "PRESSURE", DASH.into(), "Not reported by this source.", click);
        };
        let (val, unit) = match self.settings.pressure_unit {
            PressureUnit::HPa => (format!("{hpa:.0}"), "hPa"),
            PressureUnit::InHg => (format!("{:.2}", hpa * 0.02953), "inHg"),
        };
        // The trend arrow comes from the hourly forecast series alone; without it the card shows the value and no arrow.
        let Some(dp) = b.pressure_trend_3h(now) else {
            return simple(p, (w, h), UiIcon::Gauge, "PRESSURE", format!("{val} {unit}"), "No forecast trend available.", click);
        };
        let trend = match pressure_trend_label(dp) {
            "Rising" => 1_i8,
            "Falling" => -1,
            _ => 0,
        };
        let col = column![
            hdr(p, UiIcon::Gauge, "PRESSURE"),
            owned_widget_canvas(self.bento_sig.get(), WidgetCanvas::Pressure(hpa, trend, val, unit, p), Fill, Fill)
        ];
        bento_click(p, col.spacing(2).height(Fill), (w, h), 12, Msg::OpenDetail(DetailMetric::Pressure))
    }

    pub(crate) fn view_averages_card(&self, b: &WeatherBundle, p: SkyPalette, w: f32, h: f32, di: usize) -> El<'static> {
        // Mean of the forecast highs of the whole days from today on; a "rest of today" row is not a day's high.
        let highs: Vec<f64> = b.daily[di..].iter().filter(|d| !d.partial).map(|d| d.temp_max_c).collect();
        let today = b.daily.get(di).filter(|d| !d.partial).map(|d| d.temp_max_c);
        let mean = (!highs.is_empty()).then(|| highs.iter().sum::<f64>() / highs.len() as f64);
        let n = highs.len();
        let delta = today.zip(mean).map(|(t, m)| {
            // Difference of the two highs exactly as displayed below (both rounded), so the three numbers always agree.
            let rd = self.settings.convert_temp(t).round() - self.settings.convert_temp(m).round();
            if rd.abs() < 0.5 { 0.0 } else { rd }
        });
        let sub = match delta {
            Some(d) if d > 0.0 => format!("above {n}-day avg high"),
            Some(d) if d < 0.0 => format!("below {n}-day avg high"),
            Some(_) => format!("near {n}-day avg high"),
            None => "No daily highs to compare.".into(),
        };
        let hi_row = |l: String, tc: Option<f64>| {
            let v = tc.map_or_else(|| DASH.into(), |tc| self.settings.format_temp(tc));
            row![text(l).size(10.5).color(p.text_muted), sp_w(Fill), tb(format!("H:{v}"), 10.5, p.text_primary)]
        };
        let col = column![
            hdr(p, UiIcon::Trend, &format!("{n}-DAY AVG")),
            tb(delta.map_or_else(|| DASH.into(), |d| format!("{}{d:.0}°", if d > 0.0 { "+" } else { "" })), 25.0, p.text_primary),
            tm(sub, 10.0, p.text_secondary),
            sp_h(Fill),
            hairline(p.divider),
            hi_row("Today".into(), today),
            hi_row(format!("{n}-Day Avg"), mean)
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
        // The sheet is fully opaque: with no backdrop blur, even 3–4% translucency lets the dashboard's text ghost
        // through the chart.
        let (sheet_bg, sheet_bdr, fg, sub_fg, muted_fg, track_bg, div_col, hov_bg, tile_bg) = if lt {
            (
                rgb8(246, 249, 254),
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
                rgb8(20, 28, 42),
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

        // Build chart series, hero value, axis bounds, summary tiles, and copy for the active metric. Every number is a
        // real value from the bundle: an hour the source gives no value for is left out of the series, its maxima and its
        // sums, and a metric without data shows "—" and an explicit "not available" state instead of a chart.
        let mut pts: Vec<ChartPt> = Vec::with_capacity(hrs.len());
        let (nh, s, cur) = (hrs.len(), &self.settings, &b.current);
        let hour_lbl = |k: usize, ts: i64| if k == 0 { "Now".to_string() } else { loc.format_local_hour(ts, h24) };
        let range = |pts: &[ChartPt]| {
            pts.iter().map(|p| p.val).fold(None, |a: Option<(f64, f64)>, v| Some(a.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v)))))
        };
        let mean = |pts: &[ChartPt]| (!pts.is_empty()).then(|| pts.iter().map(|p| p.val).sum::<f64>() / pts.len() as f64);
        let dash = || DASH.to_string();
        let mut unavailable: Option<String> = None;
        type Detail = (String, String, Color, bool, f64, f64, &'static str, [(String, String); 3], String);
        let (hero_val, hero_sub, accent, is_bars, y_min, y_max, y_unit, stats, summary): Detail = match metric {
            DetailMetric::Temperature => {
                for (k, h) in hrs.iter().enumerate() {
                    // "Now" is the current reading with the current condition; the hours after it are the forecast.
                    let (tc, cond) = if k == 0 { (cur.temp_c, cur.condition) } else { (h.temp_c, h.condition) };
                    pts.push(ChartPt {
                        hour_lbl: hour_lbl(k, h.timestamp_unix),
                        val: s.convert_temp(tc),
                        aux: h.precip_prob_pct,
                        callout: format!("{} · {}", s.format_temp(tc), cond.label()),
                    });
                }
                let (r, avg) = (range(&pts), mean(&pts));
                let fl = cur.feels_like_c.map_or_else(dash, |f| s.format_temp(f));
                let stats = [
                    (format!("{nh}H HIGH"), r.map_or_else(dash, |(_, hi)| format!("{hi:.0}°"))),
                    (format!("{nh}H LOW"), r.map_or_else(dash, |(lo, _)| format!("{lo:.0}°"))),
                    ("FEELS LIKE".into(), fl.clone()),
                ];
                let note = r.zip(avg).map_or_else(
                    || "No hourly forecast is available.".to_string(),
                    |((lo, hi), avg)| {
                        format!("Temperatures range from {lo:.0}° to {hi:.0}° over the next {nh} hours (average {avg:.0}°). Hover the chart to inspect any hour.")
                    },
                );
                let (lo, hi) = r.unwrap_or_default();
                let pad = ((hi - lo) * 0.22).max(3.0);
                let sub = format!("{} • Feels like {fl}", cur.condition.label());
                (s.format_temp(cur.temp_c), sub, rgb8(255, 168, 46), false, (lo - pad).floor(), (hi + pad).ceil(), "°", stats, note)
            }
            DetailMetric::Precipitation => {
                let inches = s.precip_unit == PrecipUnit::Inches;
                for (k, h) in hrs.iter().enumerate() {
                    let amt = s.format_precip(h.precip_mm);
                    pts.push(ChartPt {
                        hour_lbl: hour_lbl(k, h.timestamp_unix),
                        val: if inches { h.precip_mm.max(0.0) / 25.4 } else { h.precip_mm.max(0.0) },
                        aux: h.precip_prob_pct,
                        callout: h.precip_prob_pct.map_or_else(|| amt.clone(), |pr| format!("{amt} ({pr:.0}%)")),
                    });
                }
                let tot_mm: f64 = hrs.iter().map(|h| h.precip_mm.max(0.0)).sum();
                let max_v = pts.iter().map(|p| p.val).fold(0.0_f64, f64::max);
                // Highest hourly probability the source actually reports; `None` when it reports none at all.
                let max_p = hrs.iter().filter_map(|h| h.precip_prob_pct).reduce(f64::max);
                let total = if hrs.is_empty() { dash() } else { s.format_precip(tot_mm) };
                let d0 = b.daily.get(di);
                let stats = [
                    (format!("{nh}H TOTAL"), total.clone()),
                    ("PEAK CHANCE".into(), max_p.map_or_else(dash, |p| format!("{p:.0}%"))),
                    (
                        if d0.is_some_and(|d| d.partial) { "REST OF TODAY" } else { "TODAY" }.into(),
                        d0.and_then(|d| d.precip_sum_mm).map_or_else(dash, |mm| s.format_precip(mm)),
                    ),
                ];
                let note = match max_p {
                    _ if hrs.is_empty() => "No hourly forecast is available.".to_string(),
                    Some(p) => format!("{total} forecast over the next {nh} hours; the highest hourly chance is {p:.0}%."),
                    None => format!("{total} forecast over the next {nh} hours. This source reports no precipitation probability."),
                };
                let top = if inches { (max_v * 1.25).max(0.24) } else { (max_v * 1.25).max(3.0) };
                let u = if inches { " in" } else { " mm" };
                (total, format!("Forecast for the next {nh} hours"), rgb8(58, 178, 255), true, 0.0, top, u, stats, note)
            }
            DetailMetric::Wind => {
                let conv = |mps: f64| match s.wind_unit {
                    WindUnit::KmH => mps * 3.6,
                    WindUnit::Mph => mps * 2.23694,
                    WindUnit::Ms => mps,
                    WindUnit::Knots => mps * 1.94384,
                };
                let (_, unit_lbl) = s.wind_value_and_unit(cur.wind_speed_mps);
                for (k, h) in hrs.iter().enumerate() {
                    let (mps, deg) = if k == 0 { (cur.wind_speed_mps, cur.wind_dir_deg) } else { (h.wind_speed_mps, Some(h.wind_dir_deg)) };
                    let w = s.format_wind(mps);
                    pts.push(ChartPt {
                        hour_lbl: hour_lbl(k, h.timestamp_unix),
                        val: conv(mps),
                        aux: deg,
                        callout: deg.map_or_else(|| w.clone(), |d| format!("{w} {}", compass_direction(d))),
                    });
                }
                let (r, avg) = (range(&pts), mean(&pts));
                let peak = r.map_or_else(dash, |(_, hi)| format!("{hi:.0} {unit_lbl}"));
                let stats = [
                    (format!("PEAK {nh}H"), peak.clone()),
                    ("GUSTS NOW".into(), cur.wind_gust_mps.map_or_else(dash, |g| s.format_wind(g))),
                    ("DIRECTION NOW".into(), cur.wind_dir_deg.map_or_else(dash, |d| compass_direction(d).to_string())),
                ];
                let note = avg.map_or_else(
                    || "No hourly forecast is available.".to_string(),
                    |avg| format!("Wind speed averages {avg:.0} {unit_lbl} over the next {nh} hours, peaking near {peak}."),
                );
                // Only what was reported: no direction for calm / variable wind, no gust unless one was measured.
                let sub: Vec<String> = [
                    cur.wind_dir_deg.map(|d| format!("From {}", compass_direction(d))),
                    cur.wind_gust_mps.map(|g| format!("Gusts {}", s.format_wind(g))),
                ]
                .into_iter()
                .flatten()
                .collect();
                let top = (r.map_or(0.0, |(_, hi)| hi) * 1.25).max(10.0).ceil();
                (s.format_wind(cur.wind_speed_mps), sub.join(" • "), rgb8(82, 212, 200), false, 0.0, top, "", stats, note)
            }
            DetailMetric::UvIndex => {
                let name = if b.uv_clear_sky { "Clear-sky UV" } else { "UV" };
                // The real hourly UV values the provider returned ("Now" is the current reading). Nothing is modelled.
                for (k, h) in hrs.iter().enumerate() {
                    let Some(uv) = (if k == 0 { cur.uv_index } else { h.uv_index }) else { continue };
                    pts.push(ChartPt {
                        hour_lbl: hour_lbl(k, h.timestamp_unix),
                        val: uv,
                        aux: None,
                        callout: format!("{name} {} · {}", uv_display(uv), uv_category(uv)),
                    });
                }
                if pts.len() < 2 {
                    unavailable = Some(format!("Hourly UV index is not available from this source ({}).", b.source_name));
                }
                let peak = pts.iter().max_by(|a, c| a.val.total_cmp(&c.val)).map(|p| (p.val, p.hour_lbl.clone()));
                let stats = [
                    ("CURRENT".into(), cur.uv_index.map_or_else(dash, |u| format!("{} {}", uv_display(u), uv_category(u)))),
                    (format!("PEAK {nh}H"), peak.as_ref().map_or_else(dash, |(pk, _)| format!("{} {}", uv_display(*pk), uv_category(*pk)))),
                    ("WHO GUIDANCE".into(), peak.as_ref().map_or_else(dash, |(pk, _)| uv_guidance(*pk).to_string())),
                ];
                let note = peak.as_ref().filter(|_| unavailable.is_none()).map_or_else(
                    || "No hourly UV forecast is available.".to_string(),
                    |(pk, at)| {
                        let when = if at == "Now" { "now".to_string() } else { format!("around {at}") };
                        format!("{name} index peaks at {} ({}) {when} within the next {nh} hours.", uv_display(*pk), uv_category(*pk))
                    },
                );
                let top = (peak.as_ref().map_or(0.0, |(pk, _)| *pk).max(6.0) + 1.5).ceil();
                let hero = cur.uv_index.map_or_else(dash, uv_display);
                (hero, cur.uv_index.map_or("", uv_category).into(), rgb8(255, 198, 42), false, 0.0, top, "", stats, note)
            }
            DetailMetric::Pressure => {
                let in_hg = s.pressure_unit == PressureUnit::InHg;
                let conv = |hpa: f64| if in_hg { hpa * 0.02953 } else { hpa };
                let fmt_p = |v: f64| if in_hg { format!("{v:.2} inHg") } else { format!("{v:.0} hPa") };
                // The chart is the hourly forecast series alone. The current reading may be a station observation, so it is
                // never spliced onto model hours: every point carries its own hour and the trend compares like with like.
                for h in hrs {
                    let Some(hpa) = h.pressure_hpa else { continue };
                    pts.push(ChartPt {
                        hour_lbl: loc.format_local_hour(h.timestamp_unix, h24),
                        val: conv(hpa),
                        aux: None,
                        callout: fmt_p(conv(hpa)),
                    });
                }
                let (r, trend) = (range(&pts), b.pressure_trend_3h(now));
                let delta = |dp: f64| if in_hg { format!("{:+.2} inHg", conv(dp)) } else { format!("{dp:+.1} hPa") };
                let now_s = cur.pressure_hpa.map_or_else(dash, |hpa| fmt_p(conv(hpa)));
                let stats = [
                    ("FORECAST TREND (3H)".into(), trend.map_or_else(dash, |dp| format!("{} ({})", pressure_trend_label(dp), delta(dp)))),
                    (format!("{nh}H HIGH"), r.map_or_else(dash, |(_, hi)| fmt_p(hi))),
                    (format!("{nh}H LOW"), r.map_or_else(dash, |(lo, _)| fmt_p(lo))),
                ];
                let note = match trend {
                    Some(dp) => format!(
                        "Pressure is {now_s} now. The hourly forecast changes by {} over the next 3 hours ({}).",
                        delta(dp),
                        pressure_trend_label(dp).to_lowercase()
                    ),
                    None => format!("Pressure is {now_s} now. No forecast trend is available."),
                };
                let sub = trend.map_or_else(
                    || "No forecast trend available".to_string(),
                    |dp| format!("Forecast: {} over the next 3 hours", pressure_trend_label(dp).to_lowercase()),
                );
                let (lo, hi) = r.unwrap_or_default();
                let pad = ((hi - lo) * 0.30).max(if in_hg { 0.12 } else { 4.0 });
                (now_s, sub, rgb8(130, 174, 255), false, lo - pad, hi + pad, "", stats, note)
            }
        };
        if pts.len() < 2 && unavailable.is_none() {
            unavailable = Some("No hourly forecast is available to chart.".into());
        }

        let hero = row![
            column![txt(hero_val, 30.0, Weight::Bold, fg), txt(hero_sub, 12.0, Weight::Medium, sub_fg)].spacing(1),
            sp_w(Fill),
            txt(if unavailable.is_some() { "" } else { "Hover chart to scrub" }, 10.5, Weight::Medium, muted_fg),
        ]
        .align_y(Alignment::End);

        let chart: El<'static> = match unavailable {
            Some(msg) => container(txt(msg, 12.0, Weight::Medium, sub_fg)).width(Fill).height(176).center_x(Fill).center_y(176).into(),
            None => Canvas::new(DetailChartCanvas {
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
            .height(176)
            .into(),
        };
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
    /// Secondary value (precipitation probability for the bar chart); `None` when the source does not report it.
    aux: Option<f64>,
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
            (p.val.to_bits(), p.aux.map(f64::to_bits), &p.hour_lbl).hash(&mut sh);
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
                // The probability line only joins hours that report a probability; it breaks where the source has none.
                let prob_path = Path::new(|pb| {
                    let mut pen_down = false;
                    for (i, pt) in self.pts.iter().enumerate() {
                        let Some(prob) = pt.aux else {
                            pen_down = false;
                            continue;
                        };
                        let p = Point::new(px(i), pad_t + (1.0 - (prob / 100.0).clamp(0.0, 1.0) as f32) * ph);
                        if pen_down {
                            pb.line_to(p);
                        } else {
                            pb.move_to(p);
                        }
                        pen_down = true;
                    }
                });
                let prob_col = Color { a: 0.62, ..self.accent };
                f.stroke(&prob_path, Stroke::default().with_width(1.4).with_color(Color { a: 0.45, ..self.accent }));
                // The trend line is chance of precipitation on its own 0–100% scale (full chart height), NOT an amount on
                // the right-hand axis: label that scale along the left edge in the line's own color.
                for (frac, lbl) in [(0.0_f32, "100% chance"), (0.5, "50%")] {
                    f.fill_text(canvas::Text {
                        content: lbl.into(),
                        position: Point::new(pad_l + 4.0, pad_t + frac * ph + 7.0),
                        color: prob_col,
                        size: iced::Pixels(9.0),
                        font: fw(Weight::Medium),
                        align_x: iced::alignment::Horizontal::Left.into(),
                        align_y: iced::alignment::Vertical::Center,
                        ..Default::default()
                    });
                }
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
    cond: WeatherCondition, day: bool, prob: Option<f64>, (sz_p, sz_n, fsz, sp, h): (f32, f32, f32, f32, f32), w: Option<f32>, light: bool,
) -> El<'a> {
    let inner: El<'a> = if let Some(prob) = prob.filter(|p| *p >= 25.0) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Settings, test_bundle, test_hour};

    #[test]
    fn uv_advice_is_based_on_the_remaining_real_hourly_peak() {
        // 2026-11-24 10:30 PST with hourly samples from 10:00 for 26 hours (peak 7.65 at noon, 9.0 at noon TOMORROW).
        let (now, mut b) = (1_795_545_000, test_bundle(1_795_545_000));
        let loc = b.location.clone();
        let uv_at = |i: i64| {
            if i == 2 {
                7.65
            } else if i == 26 {
                9.0
            } else {
                0.4
            }
        };
        b.hourly = (0..=26).map(|i| test_hour(now - 1800 + i * 3600, Some(uv_at(i)), None)).collect();
        // "Now" is 2.95 (shown as 3, Moderate) but the day is not "low for the rest of the day": noon peaks at 8.
        b.current.uv_index = Some(2.95);
        assert_eq!(uv_advice(&b, &loc, now, false), "Peaks at 8 (Very High) around 12 PM today.");
        assert_eq!(uv_advice(&b, &loc, now, true), "Peaks at 8 (Very High) around 12:00 today.");
        // After the peak hours only low values remain, and only then does the card say so.
        b.current.uv_index = Some(1.2);
        assert_eq!(uv_advice(&b, &loc, now + 5 * 3600, false), "Low for the rest of the day.");
        // The current value is the highest left today.
        b.current.uv_index = Some(8.4);
        assert_eq!(uv_advice(&b, &loc, now, false), "At its highest for the rest of the day.");
        // No hourly UV from the provider: nothing is claimed about the rest of the day.
        b.hourly.iter_mut().for_each(|h| h.uv_index = None);
        b.current.uv_index = Some(0.0);
        assert_eq!(uv_advice(&b, &loc, now, false), "");
        assert_eq!(
            (uv_guidance(2.4), uv_guidance(2.5), uv_guidance(7.4), uv_guidance(7.65)),
            ("No protection needed", "Protection needed", "Protection needed", "Extra protection")
        );
    }

    #[test]
    fn labels_say_only_what_the_data_supports() {
        let s = Settings::default();
        let mut b = test_bundle(1_795_545_000);
        let loc = b.location.clone();
        let day = |date: &str, partial: bool| DailyForecast {
            date: date.into(),
            temp_min_c: 9.0,
            temp_max_c: 16.0,
            condition: WeatherCondition::Rain,
            precip_prob_pct: None,
            precip_sum_mm: None,
            wind_max_mps: None,
            uv_max: None,
            sunrise_unix: None,
            sunset_unix: None,
            partial,
        };
        // Today's row is found by exact local date; a bundle without one never presents another day as "Today".
        b.daily = vec![day("2026-11-23", false), day("2026-11-24", false), day("2026-11-25", false)];
        assert_eq!(today_daily_index_at(&b, &loc, 1_795_545_000), 1);
        b.daily = vec![day("2026-11-25", false), day("2026-11-26", false)];
        assert_eq!(today_daily_index_at(&b, &loc, 1_795_545_000), 2);
        assert!(b.daily.get(today_daily_index_at(&b, &loc, 1_795_545_000)).is_none());
        // A rest-of-day row is labelled as such instead of posing as the day's high / low.
        assert_eq!(hi_lo_label(&s, &day("2026-11-24", false)), "H:16°  L:9°");
        assert_eq!(hi_lo_label(&s, &day("2026-11-24", true)), "Rest of today  H:16°  L:9°");
        assert_eq!((pressure_trend_label(0.9), pressure_trend_label(-0.8), pressure_trend_label(0.5)), ("Rising", "Falling", "Steady"));
        assert_eq!([30, 1500, 7200, 200_000].map(format_age), ["1 min", "25 min", "2 h", "2 d"]);
    }
}
