use crate::app::{Msg, WeatherApp};
use crate::astro::compute_moon_info;
use crate::icons::{
    UiIcon, WidgetCanvas, icon_svg, sun_event_svg, weather_icon_svg, widget_canvas,
};
use crate::map::MapMsg;
use crate::model::{
    AlertSeverity, PressureUnit, Settings, WeatherBundle, WeatherCondition, uv_category,
};
use crate::style::{
    SkyPalette, alert_card, flat_box, frosted_card, hairline, pbtn, rgb8, rgba8, sleek_scrollable,
    sp_h, sp_w, t, tb, tbold, tm,
};
use chrono::{Datelike, NaiveDate, Utc};
use iced::widget::{column, container, row, scrollable, text};
use iced::{Alignment, Color, Element, Length::Fill, Size};

type El<'a> = Element<'a, Msg>;

struct Ctx<'a> {
    s: &'a Settings,
    p: SkyPalette,
}

impl Ctx<'_> {
    fn hdr<'b>(&self, ic: UiIcon, title: &str) -> El<'b> {
        let ic = icon_svg(ic, 11.5, self.p.text_muted);
        let lb = tb(title.to_string(), 10.0, self.p.text_muted);
        row![ic, lb].spacing(5).align_y(Alignment::Center).into()
    }
    fn bento<'b>(&self, c: impl Into<El<'b>>, (w, h): (f32, f32), pad: u16) -> El<'b> {
        let st = frosted_card(self.p, 14.0);
        container(c)
            .width(w)
            .height(h)
            .padding(pad)
            .clip(true)
            .style(st)
            .into()
    }
    fn simple<'b>(
        &self,
        sz: (f32, f32),
        ic: UiIcon,
        title: &str,
        val: String,
        note: &str,
    ) -> El<'b> {
        let v = tb(val, 25.0, self.p.text_primary);
        let n = text(note.to_string())
            .size(10.5)
            .color(self.p.text_secondary);
        self.bento(
            column![self.hdr(ic, title), v, sp_h(Fill), n]
                .spacing(3)
                .height(Fill),
            sz,
            11,
        )
    }
    fn spec<'b>(
        &self,
        sz: (f32, f32),
        (ic, title): (UiIcon, &str),
        val: String,
        cat: &'static str,
        frac: f32,
        note: String,
    ) -> El<'b> {
        let v = tb(val, 23.5, self.p.text_primary);
        let c = tb(cat, 12.5, self.p.text_primary);
        let bar = widget_canvas(WidgetCanvas::Spectrum(frac), Fill, 11);
        let n = text(note).size(10.0).color(self.p.text_secondary);
        let col = column![self.hdr(ic, title), v, c, bar, sp_h(Fill), n]
            .spacing(2)
            .height(Fill);
        self.bento(col, sz, 11)
    }
}

impl WeatherApp {
    fn ctx(&self, p: SkyPalette) -> Ctx<'_> {
        Ctx {
            s: &self.settings,
            p,
        }
    }

    pub(crate) fn view_hero<'a>(&'a self, b: &'a WeatherBundle, p: SkyPalette) -> El<'a> {
        let loc = self.active_location().unwrap_or(&b.location);
        let cur = loc.is_current_location;
        let title = if cur {
            "My Location".into()
        } else {
            loc.name.clone()
        };
        let sub = if cur {
            loc.name.to_uppercase()
        } else {
            loc.subtitle().to_uppercase()
        };
        let (tsz, ssz) = if cur { (23.0, 10.5) } else { (26.0, 10.0) };
        let hl = b.daily.first().map_or_else(String::new, |d| {
            format!(
                "H:{}  L:{}",
                self.settings.format_temp(d.temp_max_c),
                self.settings.format_temp(d.temp_min_c)
            )
        });
        let hdr = column![
            t(title, tsz, p.text_primary),
            tb(sub, ssz, p.text_secondary)
        ]
        .spacing(1)
        .align_x(Alignment::Center);
        let tc = self.settings.format_temp(b.current.temp_c);
        let cond = b.current.condition.label();
        column![
            hdr,
            t(tc, 66.0, p.text_primary),
            tm(cond, 15.0, p.text_secondary),
            tm(hl, 13.5, p.text_primary)
        ]
        .spacing(1)
        .align_x(Alignment::Center)
        .into()
    }

    pub(crate) fn view_alerts_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
    ) -> El<'a> {
        let sev = b
            .alerts
            .first()
            .map_or(AlertSeverity::Moderate, |a| a.severity);
        let accent = match sev {
            AlertSeverity::Extreme => rgb8(255, 69, 58),
            AlertSeverity::Severe => rgb8(255, 159, 10),
            AlertSeverity::Moderate => rgb8(255, 214, 10),
            AlertSeverity::Minor => rgb8(100, 210, 255),
        };
        let ic = icon_svg(UiIcon::Alert, 12.5, accent);
        let hd = row![ic, tb("SEVERE WEATHER", 10.0, p.text_muted)]
            .spacing(5)
            .align_y(Alignment::Center);
        let mut rows: Vec<El<'a>> = vec![hd.into()];
        let bc = (
            rgba8(255, 255, 255, 0.14),
            rgba8(255, 255, 255, 0.24),
            p.text_primary,
        );
        for (i, a) in b.alerts.iter().enumerate() {
            let open = self.expanded_alert_idx == Some(i);
            let lbl = tm(if open { "Hide" } else { "Details" }, 11.0, p.text_primary);
            let sub = text(&a.headline).size(11.0).color(p.text_secondary);
            let info = column![tb(&a.event, 13.0, p.text_primary), sub].width(Fill);
            let btn = pbtn(lbl, [3, 8], bc, 6.0, Msg::ToggleAlertDetails(i));
            rows.push(row![info, btn].spacing(8).align_y(Alignment::Center).into());
            if open && !a.description.is_empty() {
                rows.push(
                    text(a.description.trim())
                        .size(11.0)
                        .color(p.text_secondary)
                        .into(),
                );
            }
        }
        container(column(rows).spacing(6))
            .width(w)
            .padding(12)
            .style(alert_card(p, accent))
            .into()
    }

    pub(crate) fn view_minutely_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
    ) -> Option<El<'a>> {
        let now = Utc::now().timestamp();
        let samp: Vec<f64> = b
            .minutely
            .iter()
            .filter(|s| s.timestamp_unix >= now - 600 && s.timestamp_unix <= now + 3900)
            .take(16)
            .map(|s| s.precip_mm_hr)
            .collect();
        if samp.len() < 3 || !samp.iter().any(|&r| r > 0.06) {
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
        let bar = widget_canvas(WidgetCanvas::Minutely(samp, p.is_light), Fill, 48);
        let hd = self.ctx(p).hdr(UiIcon::Umbrella, "NEXT-HOUR PRECIPITATION");
        let col = column![hd, tb(hl, 12.5, p.text_primary), bar, ticks].spacing(6);
        Some(
            container(col)
                .width(w)
                .padding(12)
                .style(frosted_card(p, 14.0))
                .into(),
        )
    }

    pub(crate) fn view_hourly_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        max_n: usize,
    ) -> El<'a> {
        let now = Utc::now().timestamp();
        let loc = self.active_location().unwrap_or(&b.location);
        let h24 = self.settings.use_24h_time;
        let i0 = b
            .hourly
            .iter()
            .position(|h| h.timestamp_unix >= now - 3000)
            .unwrap_or(0);
        let slice = &b.hourly[i0..(i0 + max_n).min(b.hourly.len())];
        let summary = slice.get(2).map_or_else(
            || format!("{} conditions right now.", b.current.condition.label()),
            |h3| {
                let c = h3.condition.label();
                let at = loc.format_local_hour(h3.timestamp_unix, h24);
                let ws = self.settings.format_wind(b.current.wind_speed_mps);
                format!("{c} conditions expected around {at}, with winds near {ws}.")
            },
        );
        let mut sun: Vec<_> = b
            .daily
            .iter()
            .take(2)
            .flat_map(|d| {
                let sr = d.sunrise_unix.map(|t| (t, true, "Sunrise"));
                let ss = d.sunset_unix.map(|t| (t, false, "Sunset"));
                sr.into_iter().chain(ss)
            })
            .collect();
        sun.sort_by_key(|e| e.0);
        let mut cols: Vec<El<'a>> = Vec::new();
        for (i, h) in slice.iter().enumerate() {
            let hr = if i == 0 {
                "Now".into()
            } else {
                loc.format_local_hour(h.timestamp_unix, h24)
            };
            let ic = icon_prob_cell(
                h.condition,
                h.is_day,
                h.precip_prob_pct,
                (20.0, 22.0, 9.5, 4.0, 44.0),
                None,
                p.is_light,
            );
            let tc = self.settings.format_temp(h.temp_c);
            let slot = column![
                tb(hr, 11.5, p.text_primary),
                ic,
                tm(tc, 14.0, p.text_primary)
            ];
            cols.push(slot.spacing(5).width(46).align_x(Alignment::Center).into());
            let next_ts = slice
                .get(i + 1)
                .map_or(h.timestamp_unix + 3600, |nh| nh.timestamp_unix);
            for &(ets, rise, name) in &sun {
                if ets >= h.timestamp_unix && ets < next_ts {
                    let ts = loc.format_local_time(ets, h24);
                    let se = container(sun_event_svg(rise, 22.0)).height(44).center_y(44);
                    let sc = column![
                        tb(ts, 11.0, p.text_primary),
                        se,
                        tm(name, 12.5, p.text_primary)
                    ];
                    cols.push(sc.spacing(5).width(54).align_x(Alignment::Center).into());
                }
            }
        }
        let sb = scrollable::Scrollbar::new().width(0).scroller_width(0);
        let strip = scrollable(row(cols).spacing(6).padding([2, 0]))
            .direction(scrollable::Direction::Horizontal(sb))
            .width(Fill);
        let col = column![
            tm(summary, 11.5, p.text_primary),
            hairline(p.divider),
            strip
        ]
        .spacing(8);
        container(col)
            .width(w)
            .padding(12)
            .style(frosted_card(p, 14.0))
            .into()
    }

    pub(crate) fn view_ten_day_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h_opt: Option<f32>,
    ) -> El<'a> {
        let days = &b.daily[..b.daily.len().min(10)];
        let omin = days
            .iter()
            .map(|d| d.temp_min_c)
            .fold(f64::INFINITY, f64::min);
        let omax = days
            .iter()
            .map(|d| d.temp_max_c)
            .fold(f64::NEG_INFINITY, f64::max);
        let (hbg, abg, dbg) = if p.is_light {
            (
                rgba8(20, 40, 72, 0.06),
                rgba8(10, 122, 235, 0.09),
                rgba8(24, 48, 86, 0.05),
            )
        } else {
            (
                rgba8(255, 255, 255, 0.08),
                rgba8(255, 255, 255, 0.11),
                rgba8(255, 255, 255, 0.06),
            )
        };
        let mut rows: Vec<El<'a>> = vec![self.ctx(p).hdr(UiIcon::Calendar, "10-DAY FORECAST")];
        for (i, d) in days.iter().enumerate() {
            rows.push(hairline(p.divider));
            let lbl = if i == 0 {
                "Today".into()
            } else if let Ok(nd) = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d") {
                nd.weekday().to_string()
            } else {
                d.date.clone()
            };
            let exp = self.selected_day_idx == Some(i);
            let cur = (i == 0).then_some(b.current.temp_c);
            let tb_w =
                WidgetCanvas::TempBar((d.temp_min_c, d.temp_max_c), (omin, omax), cur, p.is_light);
            let bar = widget_canvas(tb_w, Fill, 16);
            let ic = icon_prob_cell(
                d.condition,
                true,
                d.precip_prob_pct,
                (15.0, 18.0, 8.5, 2.5, 29.0),
                Some(28.0),
                p.is_light,
            );
            let lo = container(tm(
                self.settings.format_temp(d.temp_min_c),
                13.0,
                p.text_muted,
            ))
            .width(30)
            .align_x(Alignment::End);
            let hi = container(tb(
                self.settings.format_temp(d.temp_max_c),
                13.0,
                p.text_primary,
            ))
            .width(30)
            .align_x(Alignment::End);
            let dl = container(tb(lbl, 13.0, p.text_primary)).width(46);
            let line = row![dl, ic, lo, bar, hi]
                .spacing(5)
                .height(30.0)
                .align_y(Alignment::Center);
            let bg = if exp { abg } else { Color::TRANSPARENT };
            rows.push(
                pbtn(
                    line,
                    [0, 4],
                    (bg, hbg, p.text_primary),
                    8.0,
                    Msg::ToggleDayDetails(i),
                )
                .width(Fill)
                .into(),
            );
            if exp {
                let h24 = self.settings.use_24h_time;
                let sun = match (d.sunrise_unix, d.sunset_unix) {
                    (Some(sr), Some(ss)) => format!(
                        "{} / {}",
                        b.location.format_local_time(sr, h24),
                        b.location.format_local_time(ss, h24)
                    ),
                    _ => "—".into(),
                };
                let pv = if d.precip_sum_mm >= 0.1 {
                    format!(
                        "{} ({:.0}%)",
                        self.settings.format_precip(d.precip_sum_mm),
                        d.precip_prob_pct
                    )
                } else {
                    format!("{:.0}%", d.precip_prob_pct)
                };
                let uv = format!("UV {:.0} • {}", d.uv_max.round(), uv_category(d.uv_max));
                let wn = format!("Wind {}", self.settings.format_wind(d.wind_max_mps));
                let r1 = row![
                    tb(d.condition.label(), 11.5, p.text_primary),
                    sp_w(Fill),
                    text(uv).size(10.5).color(p.text_secondary)
                ]
                .align_y(Alignment::Center);
                let r2 = row![
                    text(format!("Precip {pv}"))
                        .size(10.5)
                        .color(p.text_secondary),
                    sp_w(Fill),
                    text(wn).size(10.5).color(p.text_secondary),
                    sp_w(Fill),
                    text(sun).size(10.5).color(p.text_muted)
                ]
                .align_y(Alignment::Center);
                rows.push(
                    container(column![r1, r2].spacing(3))
                        .width(Fill)
                        .padding([6, 8])
                        .style(flat_box(dbg, 8.0))
                        .into(),
                );
            }
        }
        let inner: El<'a> = if h_opt.is_some() && self.selected_day_idx.is_some() {
            scrollable(column(rows).spacing(2))
                .height(Fill)
                .style(sleek_scrollable)
                .into()
        } else {
            column(rows).spacing(2).into()
        };
        let mut c = container(inner)
            .width(w)
            .padding(12)
            .clip(true)
            .style(frosted_card(p, 14.0));
        if let Some(h) = h_opt {
            c = c.height(h);
        }
        c.into()
    }

    pub(crate) fn view_aqi_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let c = self.ctx(p);
        if let Some(a) = &b.air_quality {
            let f = (a.us_aqi as f32 / 250.0).clamp(0.02, 0.98);
            let note = format!("{} (PM2.5 {:.1})", a.summary(), a.pm2_5);
            c.spec(
                (w, h),
                (UiIcon::Air, "AIR QUALITY"),
                a.us_aqi.to_string(),
                a.category(),
                f,
                note,
            )
        } else {
            c.simple(
                (w, h),
                UiIcon::Air,
                "AIR QUALITY",
                "--".into(),
                "Air quality feed unavailable.",
            )
        }
    }

    pub(crate) fn view_wind_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let c = self.ctx(p);
        let (sv, su) = c.s.wind_value_and_unit(b.current.wind_speed_mps);
        let (gv, gu) = c.s.wind_value_and_unit(
            b.current
                .wind_gust_mps
                .unwrap_or(b.current.wind_speed_mps * 1.35),
        );
        let stat = |v: String, u: &'static str, l: &'static str| {
            let m = column![
                tbold(u.to_uppercase(), 8.5, p.text_muted),
                tm(l, 10.5, p.text_primary)
            ];
            row![tb(v, 22.0, p.text_primary), m]
                .spacing(6)
                .align_y(Alignment::Center)
        };
        let dsz = if w >= 220.0 { 82.0 } else { 52.0 };
        let left = column![
            stat(sv, su, "Wind"),
            hairline(p.divider),
            stat(gv, gu, "Gusts")
        ]
        .spacing(6)
        .width(Fill);
        let dial = widget_canvas(WidgetCanvas::Compass(b.current.wind_dir_deg, p), dsz, dsz);
        let body = row![left, dial]
            .spacing(8)
            .height(Fill)
            .align_y(Alignment::Center);
        c.bento(
            column![c.hdr(UiIcon::Wind, "WIND"), body].spacing(4),
            (w, h),
            11,
        )
    }

    pub(crate) fn view_map_bento_card(&self, p: SkyPalette, w: f32, h: f32) -> El<'_> {
        let sz = Size::new((w - 20.0).max(80.0), (h - 38.0).max(80.0));
        let m = self.map.view_mini_card(
            sz,
            self.active_location(),
            &self.bundles,
            &self.settings,
            p.is_light,
            Msg::Map(MapMsg::ExpandMap),
        );
        let c = self.ctx(p);
        c.bento(
            column![c.hdr(UiIcon::Umbrella, "PRECIPITATION"), m].spacing(6),
            (w, h),
            10,
        )
    }

    pub(crate) fn view_uv_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let uv = b.current.uv_index;
        let adv = if uv < 3.0 {
            "Low for the rest of the day."
        } else if uv < 6.0 {
            "Use sun protection until late afternoon."
        } else {
            "Seek shade during midday hours."
        };
        let f = (uv as f32 / 11.0).clamp(0.02, 0.98);
        self.ctx(p).spec(
            (w, h),
            (UiIcon::Sun, "UV INDEX"),
            format!("{uv:.0}"),
            uv_category(uv),
            f,
            adv.into(),
        )
    }

    pub(crate) fn view_sun_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let (now, loc, c) = (
            Utc::now().timestamp(),
            self.active_location().unwrap_or(&b.location),
            self.ctx(p),
        );
        let fmt = |t: i64| loc.format_local_time(t, c.s.use_24h_time);
        let r0 = b.daily.first().and_then(|d| d.sunrise_unix);
        let s0 = b.daily.first().and_then(|d| d.sunset_unix);
        let r1 = b.daily.get(1).and_then(|d| d.sunrise_unix);
        let (ic, title, main_t, sub_t) = if s0.is_some_and(|s| now < s) {
            (
                UiIcon::Sunset,
                "SUNSET",
                s0.map(fmt).unwrap_or_else(|| "--:--".into()),
                r0.map(|r| format!("Sunrise: {}", fmt(r)))
                    .unwrap_or_default(),
            )
        } else {
            (
                UiIcon::Sunrise,
                "SUNRISE",
                r1.or(r0).map(fmt).unwrap_or_else(|| "--:--".into()),
                s0.map(|s| format!("Sunset: {}", fmt(s)))
                    .unwrap_or_default(),
            )
        };
        let arc = widget_canvas(WidgetCanvas::SunArc(r0, s0, now, p.is_light), Fill, 36);
        let col = column![
            c.hdr(ic, title),
            tb(main_t, 21.0, p.text_primary),
            arc,
            sp_h(Fill),
            text(sub_t).size(10.0).color(p.text_secondary)
        ]
        .spacing(2)
        .height(Fill);
        c.bento(col, (w, h), 11)
    }

    pub(crate) fn view_feels_like_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let d = b.current.feels_like_c - b.current.temp_c;
        let note = if d <= -1.5 {
            "Wind is making it feel cooler."
        } else if d >= 1.5 {
            "Humidity is making it feel warmer."
        } else {
            "Similar to the actual temperature."
        };
        let val = self.settings.format_temp(b.current.feels_like_c);
        self.ctx(p)
            .simple((w, h), UiIcon::Thermometer, "FEELS LIKE", val, note)
    }

    pub(crate) fn view_precip_summary_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let c = self.ctx(p);
        let today = b
            .daily
            .first()
            .map_or(b.current.precip_mm, |d| d.precip_sum_mm);
        let tmrw = b.daily.get(1).map_or(0.0, |d| d.precip_sum_mm);
        let next = if tmrw > 0.1 {
            format!("{} expected tomorrow.", c.s.format_precip(tmrw))
        } else {
            "Dry conditions ahead.".into()
        };
        let val = c.s.format_precip(today);
        let col = column![
            c.hdr(UiIcon::Droplet, "PRECIPITATION"),
            tb(val, 23.0, p.text_primary),
            tb("Today", 11.5, p.text_primary),
            sp_h(Fill),
            text(next).size(10.0).color(p.text_secondary)
        ]
        .spacing(2)
        .height(Fill);
        c.bento(col, (w, h), 11)
    }

    pub(crate) fn view_moon_card<'a>(&self, p: SkyPalette, w: f32, h: f32) -> El<'a> {
        let (m, c) = (compute_moon_info(Utc::now().timestamp()), self.ctx(p));
        let to_full = (14.765 - m.age_days).rem_euclid(29.53).round() as i32;
        let kv = |l: &'static str, v: String| {
            row![
                tm(l, 10.5, p.text_primary),
                sp_w(Fill),
                text(v).size(10.5).color(p.text_secondary)
            ]
        };
        let st = column![
            kv("Illumination", format!("{:.0}%", m.illumination_pct)),
            hairline(p.divider),
            kv("Lunar Age", format!("{:.0} DAYS", m.age_days)),
            hairline(p.divider),
            kv("Next Full Moon", format!("{to_full} DAYS"))
        ]
        .spacing(5)
        .width(Fill);
        let body = row![st, widget_canvas(WidgetCanvas::Moon(m.phase), 76, 76)]
            .spacing(12)
            .height(Fill)
            .align_y(Alignment::Center);
        c.bento(
            column![c.hdr(UiIcon::Moon, &m.phase_name.to_uppercase()), body].spacing(4),
            (w, h),
            11,
        )
    }

    pub(crate) fn view_humidity_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let note = format!(
            "The dew point is {} right now.",
            self.settings.format_temp(b.current.dew_point_c)
        );
        self.ctx(p).simple(
            (w, h),
            UiIcon::Humidity,
            "HUMIDITY",
            format!("{:.0}%", b.current.humidity_pct),
            &note,
        )
    }

    pub(crate) fn view_visibility_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let v = b.current.visibility_m.unwrap_or(16_000.0);
        let note = if v >= 14_000.0 {
            "Perfectly clear view."
        } else if v >= 7_000.0 {
            "Light haze on horizon."
        } else {
            "Reduced by mist or rain."
        };
        self.ctx(p).simple(
            (w, h),
            UiIcon::Eye,
            "VISIBILITY",
            self.settings.format_visibility(v),
            note,
        )
    }

    pub(crate) fn view_pressure_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let c = self.ctx(p);
        let (val, unit) = match c.s.pressure_unit {
            PressureUnit::HPa => (format!("{:.0}", b.current.pressure_hpa), "hPa"),
            PressureUnit::InHg => (format!("{:.2}", b.current.pressure_hpa * 0.02953), "inHg"),
        };
        let dial = widget_canvas(
            WidgetCanvas::Pressure(b.current.pressure_hpa, val, unit, p),
            Fill,
            Fill,
        );
        c.bento(
            column![c.hdr(UiIcon::Gauge, "PRESSURE"), dial]
                .spacing(2)
                .height(Fill),
            (w, h),
            11,
        )
    }

    pub(crate) fn view_averages_card<'a>(
        &self,
        b: &'a WeatherBundle,
        p: SkyPalette,
        w: f32,
        h: f32,
    ) -> El<'a> {
        let c = self.ctx(p);
        let today = b.daily.first().map_or(b.current.temp_c, |d| d.temp_max_c);
        let mean = if b.daily.is_empty() {
            today
        } else {
            b.daily.iter().map(|d| d.temp_max_c).sum::<f64>() / b.daily.len() as f64
        };
        let delta = (c.s.convert_temp(today) - c.s.convert_temp(mean)).round();
        let hi_row = |l: &'static str, tc: f64| {
            row![
                text(l).size(10.0).color(p.text_muted),
                sp_w(Fill),
                tb(format!("H:{}", c.s.format_temp(tc)), 10.0, p.text_primary)
            ]
        };
        let dtxt = format!("{}{delta:.0}°", if delta > 0.0 { "+" } else { "" });
        let col = column![
            c.hdr(UiIcon::Trend, "AVERAGES"),
            tb(dtxt, 23.0, p.text_primary),
            tm("from 10-day high", 10.0, p.text_secondary),
            sp_h(Fill),
            hi_row("Today", today),
            hi_row("Average", mean)
        ]
        .spacing(1)
        .height(Fill);
        c.bento(col, (w, h), 11)
    }
}

fn icon_prob_cell<'a>(
    cond: WeatherCondition,
    day: bool,
    prob: f64,
    (sz_p, sz_n, fsz, sp, h): (f32, f32, f32, f32, f32),
    w: Option<f32>,
    light: bool,
) -> El<'a> {
    let inner: El<'a> = if prob >= 25.0 {
        let pc = if light {
            rgb8(10, 122, 235)
        } else {
            rgb8(96, 198, 255)
        };
        let pct = tbold(format!("{prob:.0}%"), fsz, pc);
        column![weather_icon_svg(cond, day, sz_p), pct]
            .spacing(sp)
            .align_x(Alignment::Center)
            .into()
    } else {
        weather_icon_svg(cond, day, sz_n)
    };
    let mut c = container(inner).height(h).center_y(h);
    if let Some(w) = w {
        c = c.width(w).center_x(w);
    }
    c.into()
}
