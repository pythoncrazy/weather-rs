use crate::model::{WeatherCondition, compass_direction};
use crate::style::{SkyPalette, fw, rgb8, rgba8, subtle_btn};
use iced::advanced::image as adv_image;
use iced::font::Weight;
use iced::widget::canvas::{self, Cache, Canvas, Frame, Geometry, Path, Stroke, Text as CanvasText, gradient::Linear};
use iced::widget::{button, container, image};
use iced::{Alignment, Color, Element, Length::Fill, Point, Rectangle, Renderer, Size, Theme};
use std::{
    collections::HashMap,
    f32::consts::{PI, TAU},
    sync::{LazyLock, OnceLock, RwLock},
};

static CLOUD_TEX: LazyLock<adv_image::Handle> = LazyLock::new(generate_volumetric_cloud_tex);
pub fn preload_cloud_tex() {
    let _ = std::thread::Builder::new().name("cloud-tex".into()).spawn(|| {
        let _ = (&*CLOUD_TEX, &*SUN_EVENT_HANDLES);
    });
}

fn line(f: &mut Frame, a: (f32, f32), b: (f32, f32), w: f32, c: Color) {
    f.stroke(&Path::line(Point::new(a.0, a.1), Point::new(b.0, b.1)), Stroke::default().with_width(w).with_color(c));
}
fn circ(f: &mut Frame, x: f32, y: f32, r: f32, c: Color) {
    f.fill(&Path::circle(Point::new(x, y), r), c);
}
fn rrect(f: &mut Frame, x: f32, y: f32, w: f32, h: f32, r: f32, fill: impl Into<canvas::Fill>) {
    f.fill(&Path::rounded_rectangle(Point::new(x, y), Size::new(w, h), r.into()), fill);
}
fn dot(f: &mut Frame, x: f32, y: f32) {
    circ(f, x, y, 5.4, rgba8(16, 24, 38, 0.55));
    circ(f, x, y, 3.6, Color::WHITE);
}
fn label(f: &mut Frame, s: impl Into<String>, x: f32, y: f32, c: Color, sz: f32, w: Weight) {
    f.fill_text(CanvasText {
        content: s.into(),
        position: Point::new(x, y),
        color: c,
        size: sz.into(),
        font: fw(w),
        align_x: Alignment::Center.into(),
        align_y: Alignment::Center.into(),
        ..CanvasText::default()
    });
}
pub fn render_svg_rgba(inner: &str, px_sz: u32, vb: f32) -> Vec<u8> {
    let s = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {vb} {vb}">{inner}</svg>"#);
    let mut px = resvg::tiny_skia::Pixmap::new(px_sz, px_sz).expect("pixmap");
    if let Ok(tree) = resvg::usvg::Tree::from_str(&s, &resvg::usvg::Options::default()) {
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(px_sz as f32 / vb, px_sz as f32 / vb), &mut px.as_mut());
    }
    let mut rgba = px.take();
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for ch in &mut p[..3] {
                *ch = ((*ch as u32 * 255 + (a >> 1)) / a).min(255) as u8;
            }
        }
    }
    rgba
}
fn svg_handle(inner: &str) -> adv_image::Handle {
    adv_image::Handle::from_rgba(48, 48, render_svg_rgba(inner, 48, 24.0))
}

fn generate_volumetric_cloud_tex() -> adv_image::Handle {
    let h = |ix: i32, iy: i32| {
        let v = ix.wrapping_mul(374_761_393).wrapping_add(iy.wrapping_mul(668_265_263));
        let m = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
        ((m ^ (m >> 16)) & 0x7fff) as f32 / 32767.0
    };
    let noise = |x: f32, y: f32| {
        let ((ix, iy), (fx, fy)) = ((x.floor() as i32, y.floor() as i32), (x - x.floor(), y - y.floor()));
        let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let (ab, cd) = (h(ix, iy) + (h(ix + 1, iy) - h(ix, iy)) * ux, h(ix, iy + 1) + (h(ix + 1, iy + 1) - h(ix, iy + 1)) * ux);
        ab + (cd - ab) * uy
    };
    let (w, ht, mut rgba) = (192_u32, 108_u32, vec![0_u8; 192 * 108 * 4]);
    let (inv_w, inv_h) = (1.0 / w as f32, 1.0 / ht as f32);
    for py in 0..ht {
        let vy = py as f32 * inv_h;
        let env = ((1.0 - (vy - 0.24).abs() * 2.1).clamp(0.0, 1.0)).powf(1.35);
        if env <= 0.004 {
            continue;
        }
        let env_112 = env * 112.0;
        let row = (py * w) as usize;
        for px in 0..w {
            let (mut x, mut y, mut n, mut amp) = (px as f32 * inv_w * 5.5, vy * 3.8, 0.0_f32, 0.54_f32);
            for _ in 0..4 {
                n += amp * noise(x, y);
                (x, y, amp) = (x * 2.03 + 13.7, y * 2.07 + 7.3, amp * 0.5);
            }
            let d = ((n - 0.36) * 2.2).clamp(0.0, 1.0);
            if d > 0.004 {
                let (s, i) = ((212.0 + n * 42.0).clamp(195.0, 255.0) as u8, (row + px as usize) * 4);
                rgba[i..i + 4].copy_from_slice(&[s, s.saturating_add(3), s.saturating_add(8), (d * env_112) as u8]);
            }
        }
    }
    adv_image::Handle::from_rgba(w, ht, rgba)
}

#[cfg(test)]
pub fn bench_cloud_tex() {
    let _ = std::hint::black_box(generate_volumetric_cloud_tex());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UiIcon {
    Sidebar,
    Search,
    Locate,
    Refresh,
    Sliders,
    Close,
    Layers,
    Play,
    Pause,
    Plus,
    Minus,
    Calendar,
    Air,
    Wind,
    Umbrella,
    Sun,
    Sunrise,
    Sunset,
    Thermometer,
    Droplet,
    Humidity,
    Eye,
    Gauge,
    Trend,
    Moon,
    Alert,
    Check,
    Cloud,
    Radar,
    Expand,
    Compress,
    Globe,
    Pin,
    Copy,
}

const UI_SVGS: [&str; 34] = [
    r#"<rect x="2.5" y="3.5" width="19" height="17" rx="3" fill="none" stroke="{c}" stroke-width="1.7"/><line x1="8.5" y1="3.5" x2="8.5" y2="20.5" stroke="{c}" stroke-width="1.7"/>"#,
    r#"<circle cx="10.5" cy="10.5" r="6.5" fill="none" stroke="{c}" stroke-width="1.8"/><line x1="15.5" y1="15.5" x2="20.5" y2="20.5" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<polygon points="3,11 21,3 13,21 11,13" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/>"#,
    r#"<path d="M20 12a8 8 0 1 1-2.3-5.6" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><polyline points="20 3.5 20 7.5 16 7.5" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
    r#"<line x1="4" y1="7" x2="20" y2="7" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/><circle cx="9" cy="7" r="2.2" fill="{c}"/><line x1="4" y1="12" x2="20" y2="12" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/><circle cx="15" cy="12" r="2.2" fill="{c}"/><line x1="4" y1="17" x2="20" y2="17" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/><circle cx="11" cy="17" r="2.2" fill="{c}"/>"#,
    r#"<line x1="6.5" y1="6.5" x2="17.5" y2="17.5" stroke="{c}" stroke-width="2" stroke-linecap="round"/><line x1="17.5" y1="6.5" x2="6.5" y2="17.5" stroke="{c}" stroke-width="2" stroke-linecap="round"/>"#,
    r#"<polygon points="12,3 21,8 12,13 3,8" fill="none" stroke="{c}" stroke-width="1.7" stroke-linejoin="round"/><polyline points="3,12.5 12,17.5 21,12.5" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><polyline points="3,16.5 12,21.5 21,16.5" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"#,
    r#"<polygon points="7,5 19,12 7,19" fill="{c}"/>"#,
    r#"<rect x="6.5" y="5" width="4" height="14" rx="1.2" fill="{c}"/><rect x="13.5" y="5" width="4" height="14" rx="1.2" fill="{c}"/>"#,
    r#"<line x1="12" y1="5" x2="12" y2="19" stroke="{c}" stroke-width="2" stroke-linecap="round"/><line x1="5" y1="12" x2="19" y2="12" stroke="{c}" stroke-width="2" stroke-linecap="round"/>"#,
    r#"<line x1="5" y1="12" x2="19" y2="12" stroke="{c}" stroke-width="2" stroke-linecap="round"/>"#,
    r#"<rect x="3" y="4.5" width="18" height="16.5" rx="2.5" fill="none" stroke="{c}" stroke-width="1.8"/><line x1="3" y1="9.5" x2="21" y2="9.5" stroke="{c}" stroke-width="1.8"/><line x1="8" y1="2.5" x2="8" y2="6" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="16" y1="2.5" x2="16" y2="6" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<circle cx="6" cy="8" r="1.4" fill="{c}"/><circle cx="12" cy="6" r="1.4" fill="{c}"/><circle cx="18" cy="9" r="1.4" fill="{c}"/><path d="M4 14c3-2 6 2 9 0s5-2 7 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><path d="M6 18c3-1.5 5 1.5 8 0s4-1.5 5 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<path d="M3 9h11.5a2.5 2.5 0 1 0-2.5-2.5" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><path d="M3 13h15.5a2.5 2.5 0 1 1-2.5 2.5" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><path d="M3 17h8.5a2 2 0 1 1-2 2" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<path d="M3 12a9 9 0 0 1 18 0Z" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/><path d="M12 12v6.5a2 2 0 0 1-4 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<circle cx="12" cy="12" r="4.5" fill="none" stroke="{c}" stroke-width="1.8"/><line x1="12" y1="2.5" x2="12" y2="5" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="12" y1="19" x2="12" y2="21.5" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="2.5" y1="12" x2="5" y2="12" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="19" y1="12" x2="21.5" y2="12" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="5.3" y1="5.3" x2="7.1" y2="7.1" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="16.9" y1="16.9" x2="18.7" y2="18.7" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="18.7" y1="5.3" x2="16.9" y2="7.1" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="7.1" y1="16.9" x2="5.3" y2="18.7" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<path d="M5 17a7 7 0 0 1 14 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="2.5" y1="17" x2="21.5" y2="17" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="12" y1="3.5" x2="12" y2="8.5" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><polyline points="9.2,6.2 12,3.4 14.8,6.2" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
    r#"<path d="M5 17a7 7 0 0 1 14 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="2.5" y1="17" x2="21.5" y2="17" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="12" y1="3.5" x2="12" y2="8.5" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><polyline points="9.2,5.8 12,8.6 14.8,5.8" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
    r#"<path d="M14 14.8V5a2 2 0 1 0-4 0v9.8a4 4 0 1 0 4 0Z" fill="none" stroke="{c}" stroke-width="1.8"/>"#,
    r#"<path d="M12 3.5C12 3.5 6 10.2 6 14.5a6 6 0 0 0 12 0c0-4.3-6-11-6-11Z" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/>"#,
    r#"<path d="M4 9c2.5-2 5 2 7.5 0s5-2 8.5 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><path d="M4 14c2.5-2 5 2 7.5 0s5-2 8.5 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><path d="M4 19c2.5-2 5 2 7.5 0s5-2 8.5 0" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<path d="M2.5 12S6 6 12 6s9.5 6 9.5 6-3.5 6-9.5 6-9.5-6-9.5-6Z" fill="none" stroke="{c}" stroke-width="1.8"/><circle cx="12" cy="12" r="2.8" fill="none" stroke="{c}" stroke-width="1.8"/>"#,
    r#"<circle cx="12" cy="12" r="9" fill="none" stroke="{c}" stroke-width="1.8"/><line x1="12" y1="12" x2="16" y2="8" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<polyline points="3,17 9,11 13,14 21,6" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/><polyline points="16,6 21,6 21,11" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
    r#"<path d="M19 14.5A7.5 7.5 0 0 1 9.5 5a7.5 7.5 0 1 0 9.5 9.5Z" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/>"#,
    r#"<path d="M12 3.5L2.8 19.5h18.4L12 3.5Z" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/><line x1="12" y1="9.5" x2="12" y2="14" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><circle cx="12" cy="17" r="1" fill="{c}"/>"#,
    r#"<polyline points="4.5,12.5 9.5,17.5 19.5,6.5" fill="none" stroke="{c}" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    r#"<path d="M7 18h10a4 4 0 0 0 .5-8 5.4 5.4 0 0 0-10.3-1.4A4.2 4.2 0 0 0 7 18Z" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/>"#,
    r#"<circle cx="12" cy="12" r="2.2" fill="{c}"/><path d="M7.8 7.8a6 6 0 0 0 0 8.4M16.2 7.8a6 6 0 0 1 0 8.4M4.8 4.8a10.2 10.2 0 0 0 0 14.4M19.2 4.8a10.2 10.2 0 0 1 0 14.4" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/>"#,
    r#"<polyline points="15 3 21 3 21 9" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/><polyline points="9 21 3 21 3 15" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/><line x1="21" y1="3" x2="14" y2="10" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="3" y1="21" x2="10" y2="14" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<polyline points="4 14 10 14 10 20" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/><polyline points="20 10 14 10 14 4" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/><line x1="14" y1="10" x2="21" y2="3" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/><line x1="3" y1="21" x2="10" y2="14" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"#,
    r#"<circle cx="12" cy="12" r="9" fill="none" stroke="{c}" stroke-width="1.7"/><ellipse cx="12" cy="12" rx="4.2" ry="9" fill="none" stroke="{c}" stroke-width="1.5"/><line x1="3.3" y1="9" x2="20.7" y2="9" stroke="{c}" stroke-width="1.5"/><line x1="3.3" y1="15" x2="20.7" y2="15" stroke="{c}" stroke-width="1.5"/>"#,
    r#"<path d="M12 21s-6.2-5.6-6.2-10.6a6.2 6.2 0 1 1 12.4 0C18.2 15.4 12 21 12 21Z" fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round"/><circle cx="12" cy="10.4" r="2.2" fill="{c}"/>"#,
    r#"<rect x="9" y="9" width="11" height="11" rx="2.2" fill="none" stroke="{c}" stroke-width="1.7"/><path d="M5 15H4.5A2.5 2.5 0 0 1 2 12.5v-8A2.5 2.5 0 0 1 4.5 2h8A2.5 2.5 0 0 1 15 4.5V5" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/>"#,
];

static UI_ICON_CACHE: LazyLock<RwLock<HashMap<(u8, u32), adv_image::Handle>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn icon_svg<'a, M: 'a>(icon: UiIcon, size: f32, color: Color) -> Element<'a, M> {
    let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (r, g, b) = (u(color.r), u(color.g), u(color.b));
    let key = (icon as u8, ((r as u32) << 16) | ((g as u32) << 8) | b as u32);
    if let Ok(c) = UI_ICON_CACHE.read()
        && let Some(h) = c.get(&key)
    {
        return image(h.clone()).width(size).height(size).into();
    }
    let h = svg_handle(&UI_SVGS[icon as usize].replace("{c}", &format!("#{r:02X}{g:02X}{b:02X}")));
    let _ = UI_ICON_CACHE.write().map(|mut c| c.insert(key, h.clone()));
    image(h).width(size).height(size).into()
}

pub fn icon_btn<'a, M: Clone + 'a>(icon: UiIcon, isz: f32, sz: (u32, u32), cols: (Color, Color, Color), r: f32, msg: M) -> Element<'a, M> {
    let b = button(container(icon_svg(icon, isz, cols.0)).center_x(Fill).center_y(Fill)).width(sz.0).height(sz.1).padding(0);
    b.style(subtle_btn(cols.1, cols.2, cols.0, r)).on_press(msg).into()
}

static WEATHER_ICON_HANDLES: [OnceLock<adv_image::Handle>; 26] = [const { OnceLock::new() }; 26];

pub fn weather_icon_svg<'a, M: 'a>(cond: WeatherCondition, is_day: bool, size: f32) -> Element<'a, M> {
    let h = WEATHER_ICON_HANDLES[(cond as usize) * 2 + usize::from(is_day)].get_or_init(|| {
        // Multi-stop gradient Apple Weather SF-Symbol-inspired icons (64x64 retina, cached once).
        const DEFS: &str = r##"<defs><radialGradient id="sg" cx="38%" cy="35%" r="65%"><stop offset="0%" stop-color="#FFF27A"/><stop offset="55%" stop-color="#FFCA28"/><stop offset="100%" stop-color="#FF9500"/></radialGradient><linearGradient id="mg" x1="0" y1="0" x2="1" y2="1"><stop offset="0%" stop-color="#EBF3FF"/><stop offset="100%" stop-color="#89ABD9"/></linearGradient><linearGradient id="cg" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#F7FAFF"/><stop offset="55%" stop-color="#D6E3F4"/><stop offset="100%" stop-color="#A6BDD8"/></linearGradient><linearGradient id="bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#C6D5E8"/><stop offset="100%" stop-color="#8EA5C4"/></linearGradient><linearGradient id="dg" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#D4E0F0"/><stop offset="100%" stop-color="#92AAC8"/></linearGradient><linearGradient id="rg" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#6DD2FF"/><stop offset="100%" stop-color="#0A84FF"/></linearGradient><linearGradient id="lg" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#FFE96B"/><stop offset="100%" stop-color="#FF9F0A"/></linearGradient></defs>"##;
        const SUN: &str = r##"<circle cx="12" cy="12" r="5.3" fill="url(#sg)"/><g stroke="#FFCA28" stroke-width="1.9" stroke-linecap="round"><line x1="12" y1="2.7" x2="12" y2="4.7"/><line x1="12" y1="19.3" x2="12" y2="21.3"/><line x1="2.7" y1="12" x2="4.7" y2="12"/><line x1="19.3" y1="12" x2="21.3" y2="12"/><line x1="5.4" y1="5.4" x2="6.8" y2="6.8"/><line x1="17.2" y1="17.2" x2="18.6" y2="18.6"/><line x1="18.6" y1="5.4" x2="17.2" y2="6.8"/><line x1="6.8" y1="17.2" x2="5.4" y2="18.6"/></g>"##;
        const MOON: &str = r##"<path d="M18.2 14.8A6.8 6.8 0 0 1 9.2 5.8a6.8 6.8 0 1 0 9.0 9.0Z" fill="url(#mg)" stroke="#6E8DB8" stroke-opacity="0.45" stroke-width="0.65"/>"##;
        const SSUN: &str = r##"<circle cx="8.6" cy="9.2" r="4.2" fill="url(#sg)"/><g stroke="#FFCA28" stroke-width="1.5" stroke-linecap="round"><line x1="8.6" y1="2.6" x2="8.6" y2="3.9"/><line x1="2.1" y1="9.2" x2="3.4" y2="9.2"/><line x1="4.0" y1="4.6" x2="4.9" y2="5.5"/><line x1="13.2" y1="4.6" x2="12.3" y2="5.5"/></g>"##;
        const SMOON: &str = r##"<path d="M12.6 9.8A4.4 4.4 0 0 1 6.8 4.0a4.4 4.4 0 1 0 5.8 5.8Z" fill="url(#mg)" stroke="#6E8DB8" stroke-opacity="0.45" stroke-width="0.6"/>"##;
        const CLD: &str = r##"<path d="M7.2 18.8h10.2a3.8 3.8 0 0 0 .4-7.6A5.4 5.4 0 0 0 7.5 9.6 4.4 4.4 0 0 0 7.2 18.8Z" fill="url(#cg)" stroke="#6C87AB" stroke-opacity="0.45" stroke-width="0.7" stroke-linejoin="round"/>"##;
        const BCK: &str = r##"<path d="M5.2 15.6h8.8a3.1 3.1 0 0 0 .3-6.2 4.3 4.3 0 0 0-8.2-1.2 3.6 3.6 0 0 0-.9 7.4Z" fill="url(#bg)" stroke="#6680A3" stroke-opacity="0.42" stroke-width="0.65" stroke-linejoin="round"/>"##;
        const STM: &str = r##"<path d="M6.6 14.7h10.6a3.7 3.7 0 0 0 .3-7.4 5.1 5.1 0 0 0-9.9-1.4 4.2 4.2 0 0 0-1.0 8.8Z" fill="url(#dg)" stroke="#657FA3" stroke-opacity="0.45" stroke-width="0.7" stroke-linejoin="round"/>"##;
        use WeatherCondition as W;
        let inner = match cond {
            W::Clear => format!("{DEFS}{}", if is_day { SUN } else { MOON }),
            W::MostlyClear | W::PartlyCloudy => format!("{DEFS}{}{CLD}", if is_day { SSUN } else { SMOON }),
            W::Overcast => format!("{DEFS}{BCK}{CLD}"),
            W::Fog => format!(r##"{DEFS}{STM}<g stroke="#D6E4F5" stroke-width="1.8" stroke-linecap="round"><line x1="5.2" y1="17.3" x2="18.8" y2="17.3"/><line x1="7.4" y1="20.1" x2="16.6" y2="20.1"/></g>"##),
            W::Drizzle => format!(r##"{DEFS}{STM}<g stroke="url(#rg)" stroke-width="1.9" stroke-linecap="round"><line x1="9.6" y1="16.0" x2="8.6" y2="18.8"/><line x1="14.6" y1="16.0" x2="13.6" y2="18.8"/></g>"##),
            W::Rain | W::HeavyRain | W::FreezingRain => format!(r##"{DEFS}{STM}<g stroke="url(#rg)" stroke-width="1.95" stroke-linecap="round"><line x1="8.2" y1="16.0" x2="7.0" y2="19.2"/><line x1="12.2" y1="16.0" x2="11.0" y2="19.2"/><line x1="16.2" y1="16.0" x2="15.0" y2="19.2"/></g>"##),
            W::Snow | W::HeavySnow | W::Sleet => format!(r##"{DEFS}{STM}<g fill="#CBE9FF"><circle cx="8.4" cy="17.8" r="1.45"/><circle cx="12.2" cy="19.0" r="1.45"/><circle cx="16.0" cy="17.8" r="1.45"/></g>"##),
            W::Thunderstorm => format!(r##"{DEFS}{STM}<polygon points="13.0,13.6 9.2,18.2 12.1,18.2 10.8,21.6 15.6,16.8 12.6,16.8" fill="url(#lg)"/>"##),
        };
        adv_image::Handle::from_rgba(64, 64, render_svg_rgba(&inner, 64, 24.0))
    });
    image(h.clone()).width(size).height(size).into()
}

static SUN_EVENT_HANDLES: LazyLock<[adv_image::Handle; 2]> = LazyLock::new(|| {
    let b = |pts: &str, col: &str| {
        let inner = format!(
            r##"<defs><radialGradient id="hg" cx="50%" cy="75%" r="65%"><stop offset="0%" stop-color="#FFF080"/><stop offset="100%" stop-color="#FFA600"/></radialGradient></defs><polyline points="{pts}" fill="none" stroke="{col}" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/><path d="M5.8 16.2a6.2 6.2 0 0 1 12.4 0" fill="url(#hg)"/><line x1="2.8" y1="16.8" x2="21.2" y2="16.8" stroke="#FFE394" stroke-width="1.9" stroke-linecap="round"/><line x1="6.4" y1="19.8" x2="17.6" y2="19.8" stroke="#FFFFFF" stroke-opacity="0.58" stroke-width="1.5" stroke-linecap="round"/>"##
        );
        adv_image::Handle::from_rgba(64, 64, render_svg_rgba(&inner, 64, 24.0))
    };
    [b("9.4,5.4 12,8.2 14.6,5.4", "#FFB347"), b("9.4,7.6 12,4.8 14.6,7.6", "#FFD858")]
});
pub fn sun_event_svg<'a, M: 'a>(is_sunrise: bool, size: f32) -> Element<'a, M> {
    image(SUN_EVENT_HANDLES[usize::from(is_sunrise)].clone()).width(size).height(size).into()
}

/// GPU-cached Apple Weather atmospheric sky backdrop (zero per-frame tessellation when unchanged).
static SUN_GLOW_TEX: LazyLock<adv_image::Handle> = LazyLock::new(|| {
    const N: usize = 128;
    let mut rgba = vec![0_u8; N * N * 4];
    let inv = 2.0 / (N as f32 - 1.0);
    for y in 0..N {
        let dy = y as f32 * inv - 1.0;
        for x in 0..N {
            let dx = x as f32 * inv - 1.0;
            let r = (dx * dx + dy * dy).sqrt();
            if r < 1.0 {
                // Smooth atmospheric Mie-scattering solar halo: bright soft core + wide feathered veil, zero ring steps!
                let outer = (1.0 - r).powf(2.6) * 0.30;
                let inner = (1.0 - (r * 2.2).min(1.0)).powf(2.0) * 0.28;
                let a = ((outer + inner).clamp(0.0, 1.0) * 255.0).round() as u8;
                let o = (y * N + x) * 4;
                rgba[o..o + 4].copy_from_slice(&[255, 249, 228, a]);
            }
        }
    }
    adv_image::Handle::from_rgba(N as u32, N as u32, rgba)
});

struct CachedSky<'a> {
    cache: &'a Cache,
    pal: SkyPalette,
    cond: WeatherCondition,
    is_day: bool,
}
impl<M> canvas::Program<M> for CachedSky<'_> {
    type State = ();
    fn draw(&self, _: &(), r: &Renderer, _: &Theme, b: Rectangle, _: iced::mouse::Cursor) -> Vec<Geometry> {
        let geom = self.cache.draw(r, b.size(), |f| {
            use WeatherCondition as W;
            let (bw, bh) = (b.width.max(1.0), b.height.max(1.0));
            let g = Linear::new(Point::ORIGIN, Point::new(0.0, bh))
                .add_stop(0.0, self.pal.sky_top)
                .add_stop(0.45, self.pal.sky_mid)
                .add_stop(1.0, self.pal.sky_bot);
            f.fill_rectangle(Point::ORIGIN, Size::new(bw, bh), g);
            if !self.pal.is_light {
                // Silky atmospheric Mie-scattering solar halo on clear/partly cloudy day, or subtle horizon sheen at night.
                if self.is_day && matches!(self.cond, W::Clear | W::MostlyClear | W::PartlyCloudy) {
                    let (sx, sy, rad) = (bw * 0.78, bh * 0.10, 340.0_f32);
                    let intensity: f32 = if self.cond == W::PartlyCloudy { 0.55 } else { 1.0 };
                    let rect = Rectangle { x: sx - rad, y: sy - rad, width: rad * 2.0, height: rad * 2.0 };
                    f.draw_image(rect, adv_image::Image::new(SUN_GLOW_TEX.clone()).opacity(intensity));
                } else if !self.is_day && matches!(self.cond, W::Clear | W::MostlyClear) {
                    let glow = Linear::new(Point::new(0.0, bh * 0.62), Point::new(0.0, bh))
                        .add_stop(0.0, rgba8(80, 128, 210, 0.0))
                        .add_stop(1.0, rgba8(72, 118, 198, 0.14));
                    f.fill_rectangle(Point::new(0.0, bh * 0.62), Size::new(bw, bh * 0.38), glow);
                }
                let op: f32 = match self.cond {
                    W::PartlyCloudy => 0.58,
                    W::Overcast | W::Fog => 0.86,
                    W::Drizzle | W::Rain | W::HeavyRain | W::Thunderstorm => 0.62,
                    _ => 0.0,
                };
                if op > 0.0 {
                    let rect = Rectangle { x: 0.0, y: 0.0, width: bw, height: (bh * 0.62).min(520.0) };
                    f.draw_image(rect, adv_image::Image::new(CLOUD_TEX.clone()).opacity(op));
                }
            }
        });
        vec![geom]
    }
}
pub fn sky_backdrop_canvas<'a, M: 'a>(cache: &'a Cache, pal: SkyPalette, cond: WeatherCondition, is_day: bool) -> Element<'a, M> {
    Canvas::new(CachedSky { cache, pal, cond, is_day }).width(Fill).height(Fill).into()
}

/// GPU-cached Bento card canvas pool sharing ONE `iced_wgpu` text atlas (PR #2382 + #2415).
pub enum WidgetCanvas {
    SkyParticles(bool, WeatherCondition, bool, f32),
    TempBar((f64, f64), (f64, f64), Option<f64>, bool),
    Spectrum(f32),
    Compass(f64, String, &'static str, SkyPalette),
    SunArc(Option<i64>, Option<i64>, i64, bool),
    Pressure(f64, i8, String, &'static str, SkyPalette),
    Moon(f64),
    Minutely(Vec<f64>, bool),
}
struct CachedWidget<'a> {
    cache: &'a Cache,
    kind: WidgetCanvas,
}
impl<M> canvas::Program<M> for CachedWidget<'_> {
    type State = ();
    fn draw(&self, _: &(), r: &Renderer, _: &Theme, b: Rectangle, _: iced::mouse::Cursor) -> Vec<Geometry> {
        vec![self.cache.draw(r, b.size(), |f| self.kind.draw_into(f, b.width.max(1.0), b.height.max(1.0)))]
    }
}
pub fn cached_widget_canvas<'a, M: 'a>(
    cache: &'a Cache, kind: WidgetCanvas, w: impl Into<iced::Length>, h: impl Into<iced::Length>,
) -> Element<'a, M> {
    Canvas::new(CachedWidget { cache, kind }).width(w).height(h).into()
}

#[derive(Default)]
struct OwnedWidgetState {
    cache: Cache,
    sig: std::cell::Cell<u64>,
}
struct OwnedCachedWidget {
    sig: u64,
    kind: WidgetCanvas,
}
impl<M> canvas::Program<M> for OwnedCachedWidget {
    type State = OwnedWidgetState;
    fn draw(&self, st: &OwnedWidgetState, r: &Renderer, _: &Theme, b: Rectangle, _: iced::mouse::Cursor) -> Vec<Geometry> {
        let sig = self.sig ^ ((b.width.to_bits() as u64) << 32) ^ (b.height.to_bits() as u64);
        if st.sig.get() != sig {
            st.cache.clear();
            st.sig.set(sig);
        }
        vec![st.cache.draw(r, b.size(), |f| self.kind.draw_into(f, b.width.max(1.0), b.height.max(1.0)))]
    }
}
pub fn owned_widget_canvas<M: 'static>(
    sig: u64, kind: WidgetCanvas, w: impl Into<iced::Length>, h: impl Into<iced::Length>,
) -> Element<'static, M> {
    Canvas::new(OwnedCachedWidget { sig, kind }).width(w).height(h).into()
}
impl WidgetCanvas {
    fn draw_into(&self, f: &mut Frame, bw: f32, bh: f32) {
        match self {
            Self::SkyParticles(light, cond, day, ph) if !light => {
                use WeatherCondition as W;
                // Batched into 3 depth-tier paths instead of 42–72 individual draw calls per 60fps frame.
                if !day && matches!(cond, W::Clear | W::MostlyClear | W::PartlyCloudy) {
                    for tier in 0..3_i32 {
                        let tw = 0.55 + 0.45 * (0.5 + 0.5 * (ph * (0.9 + tier as f32 * 0.35) + tier as f32 * 2.1).sin());
                        let (rad, al) = match tier {
                            0 => (0.85, 0.30 * tw),
                            1 => (1.15, 0.48 * tw),
                            _ => (1.55, 0.72 * tw),
                        };
                        let path = Path::new(|pb| {
                            for i in (0..54).filter(|&i| i % 3 == tier) {
                                let (fx, fy) = (
                                    ((i as f32 * 127.1).sin() * 43_758.547).fract().abs(),
                                    ((i as f32 * 311.7).sin() * 19_583.21).fract().abs() * 0.52,
                                );
                                pb.circle(Point::new(fx * bw, fy * bh), rad);
                            }
                        });
                        f.fill(&path, rgba8(244, 249, 255, al));
                    }
                }
                let snow = matches!(cond, W::Snow | W::HeavySnow | W::Sleet);
                let rain = matches!(cond, W::Drizzle | W::Rain | W::HeavyRain | W::FreezingRain | W::Thunderstorm);
                if snow || rain {
                    let heavy = matches!(cond, W::HeavyRain | W::Thunderstorm);
                    let total = if snow { 57 } else { 54 + 21 * i32::from(heavy) };
                    for tier in 0..3_i32 {
                        let d = tier as f32;
                        let path = Path::new(|pb| {
                            for i in (0..total).filter(|&i| i % 3 == tier) {
                                let (h1, h2) = (
                                    ((i as f32 * 91.7).sin() * 24_819.3).fract().abs(),
                                    ((i as f32 * 213.3).cos() * 15_731.9).fract().abs(),
                                );
                                let spd = if snow { 34.0 + d * 18.0 + h2 * 24.0 } else { 290.0 + d * 125.0 + h2 * 120.0 };
                                let y = (h2 * bh + ph * spd).rem_euclid(bh);
                                if snow {
                                    let sw = (ph * (0.95 + d * 0.22) + i as f32 * 0.85).sin() * (10.0 + d * 4.5)
                                        + (ph * 1.65 + i as f32 * 1.9).cos() * 3.5;
                                    pb.circle(Point::new((h1 * bw + sw).rem_euclid(bw), y), 1.05 + d * 0.55);
                                } else {
                                    let (sl, len) = (0.095 + d * 0.015, 10.0 + d * 6.5 + if heavy { 4.0 } else { 0.0 });
                                    let x = (h1 * bw - y * sl).rem_euclid(bw);
                                    pb.move_to(Point::new(x, y));
                                    pb.line_to(Point::new(x - len * sl, y + len));
                                }
                            }
                        });
                        if snow {
                            f.fill(&path, rgba8(242, 249, 255, 0.18 + d * 0.11));
                        } else {
                            f.stroke(
                                &path,
                                Stroke::default().with_width(0.85 + d * 0.22).with_color(rgba8(205, 232, 255, 0.10 + d * 0.06)),
                            );
                        }
                    }
                }
            }
            Self::SkyParticles(..) => {}
            Self::Compass(deg, speed_val, speed_unit, pal) => {
                let ((cx, cy, sm), rad) = ((bw * 0.5, bh * 0.5, bw < 66.0), (bw.min(bh) * 0.5 - 3.5).max(20.0));
                let pt = |a: f32, r: f32| Point::new(cx + a.cos() * r, cy + a.sin() * r);
                let needle = if pal.is_light { rgb8(24, 34, 50) } else { Color::WHITE };
                // Authentic macOS Weather translucent compass dial with crisp speed+unit center badge.
                circ(f, cx, cy, rad, if pal.is_light { rgba8(24, 42, 72, 0.05) } else { rgba8(255, 255, 255, 0.06) });
                for (maj, al, len) in [(false, 0.22, 3.5), (true, 0.48, 5.5)] {
                    let path = Path::new(|pb| {
                        for i in (0..72).filter(|&i| (i % 6 == 0) == maj) {
                            let d = i as f32 * 5.0;
                            if [0.0_f32, 90.0, 180.0, 270.0, 360.0].iter().any(|x| (d - x).abs() < 6.0) {
                                continue;
                            }
                            let a = (d - 90.0).to_radians();
                            pb.move_to(pt(a, rad - len));
                            pb.line_to(pt(a, rad));
                        }
                    });
                    let tc = if pal.is_light { rgba8(28, 42, 68, al) } else { rgba8(255, 255, 255, al) };
                    f.stroke(&path, Stroke::default().with_width(1.0).with_color(tc));
                }
                for (d, l) in [(0.0_f32, "N"), (90.0, "E"), (180.0, "S"), (270.0, "W")] {
                    let p = pt((d - 90.0).to_radians(), rad - if sm { 3.8 } else { 6.2 });
                    let col = if d == 0.0 { pal.text_primary } else { pal.text_muted };
                    label(f, l, p.x, p.y, col, if sm { 6.4 } else { 9.5 }, Weight::Bold);
                }
                let (to, (gap, wing)) = ((*deg as f32 + 90.0).to_radians(), if sm { (10.5, 4.2) } else { (17.5, 6.2) });
                let (tip, tail) = (pt(to, rad), pt(to + PI, rad - 2.0));
                let shaft = Path::new(|pb| {
                    pb.move_to(tail);
                    pb.line_to(pt(to + PI, gap));
                    pb.move_to(pt(to, gap));
                    pb.line_to(tip);
                });
                f.stroke(&shaft, Stroke::default().with_width(if sm { 1.3 } else { 1.7 }).with_color(needle));
                circ(f, tail.x, tail.y, if sm { 1.8 } else { 2.6 }, needle);
                let head = Path::new(|pb| {
                    pb.move_to(tip);
                    for a in [to - 0.45, to + 0.45] {
                        pb.line_to(Point::new(tip.x - a.cos() * wing, tip.y - a.sin() * wing));
                    }
                    pb.close();
                });
                f.fill(&head, needle);
                if sm {
                    label(f, compass_direction(*deg), cx, cy, pal.text_primary, 7.0, Weight::Bold);
                } else {
                    label(f, speed_val, cx, cy - 4.5, pal.text_primary, 13.5, Weight::Bold);
                    label(f, *speed_unit, cx, cy + 7.5, pal.text_muted, 8.0, Weight::Semibold);
                }
            }
            Self::Pressure(hpa, trend, val, unit, pal) => {
                let (cx, cy, rad) = (bw * 0.5, bh * 0.49, (bw.min(bh) * 0.44).max(22.0));
                let active = (((*hpa - 980.0) / 60.0).clamp(0.0, 1.0) * 36.0).round() as i32;
                let dim = if pal.is_light { rgba8(28, 42, 68, 0.24) } else { rgba8(255, 255, 255, 0.28) };
                for (is_act, w, r0, col) in [(false, 1.1, rad - 4.8, dim), (true, 2.4, rad - 8.0, pal.text_primary)] {
                    let path = Path::new(|pb| {
                        for i in (0..=36).filter(|&i| ((i - active).abs() <= 1) == is_act) {
                            let a = (135.0 + (i as f32 / 36.0) * 270.0).to_radians();
                            pb.move_to(Point::new(cx + a.cos() * r0, cy + a.sin() * r0));
                            pb.line_to(Point::new(cx + a.cos() * rad, cy + a.sin() * rad));
                        }
                    });
                    f.stroke(&path, Stroke::default().with_width(w).with_color(col));
                }
                // Apple Weather center pressure trend arrow (↑ rising / → steady / ↓ falling).
                let ay = cy - 16.0;
                let arrow = Path::new(|pb| match *trend {
                    1 => {
                        pb.move_to(Point::new(cx, ay + 4.5));
                        pb.line_to(Point::new(cx, ay - 3.5));
                        pb.move_to(Point::new(cx - 3.2, ay - 0.5));
                        pb.line_to(Point::new(cx, ay - 3.7));
                        pb.line_to(Point::new(cx + 3.2, ay - 0.5));
                    }
                    -1 => {
                        pb.move_to(Point::new(cx, ay - 3.5));
                        pb.line_to(Point::new(cx, ay + 4.5));
                        pb.move_to(Point::new(cx - 3.2, ay + 1.3));
                        pb.line_to(Point::new(cx, ay + 4.5));
                        pb.line_to(Point::new(cx + 3.2, ay + 1.3));
                    }
                    _ => {
                        pb.move_to(Point::new(cx - 4.5, ay + 0.5));
                        pb.line_to(Point::new(cx + 4.5, ay + 0.5));
                        pb.move_to(Point::new(cx + 1.5, ay - 2.3));
                        pb.line_to(Point::new(cx + 4.5, ay + 0.5));
                        pb.line_to(Point::new(cx + 1.5, ay + 3.3));
                    }
                });
                f.stroke(&arrow, Stroke::default().with_width(1.6).with_color(pal.text_primary));
                label(f, val, cx, cy + 1.0, pal.text_primary, 14.5, Weight::Bold);
                label(f, *unit, cx, cy + 14.5, pal.text_muted, 9.5, Weight::Medium);
                label(f, "Low", cx - rad * 0.62, bh - 6.0, pal.text_muted, 9.5, Weight::Semibold);
                label(f, "High", cx + rad * 0.62, bh - 6.0, pal.text_muted, 9.5, Weight::Semibold);
            }
            Self::TempBar((dmin, dmax), (omin, omax), cur, light) => {
                let (cy, h, pad, w, span) = (bh * 0.5, 4.8_f32, 4.0_f32, (bw - 8.0).max(12.0), (omax - omin).max(1.0));
                rrect(f, pad, cy - h * 0.5, w, h, h * 0.5, if *light { rgba8(24, 42, 72, 0.14) } else { rgba8(10, 16, 28, 0.38) });
                let px = |t: f64| pad + ((t - omin) / span).clamp(0.0, 1.0) as f32 * w;
                let (x0, x1) = (px(*dmin), px(*dmax).max(px(*dmin) + 7.0).min(pad + w));
                let g = Linear::new(Point::new(x0, cy), Point::new(x1, cy))
                    .add_stop(0.0, temp_to_color(*dmin, 1.0))
                    .add_stop(0.5, temp_to_color((*dmin + *dmax) * 0.5, 1.0))
                    .add_stop(1.0, temp_to_color(*dmax, 1.0));
                rrect(f, x0, cy - h * 0.5, x1 - x0, h, h * 0.5, g);
                let _ = cur.map(|c| dot(f, px(c).clamp(x0, x1), cy));
            }
            Self::Spectrum(frac) => {
                let (cy, h, w) = (bh * 0.5, 4.8_f32, (bw - 6.0).max(10.0));
                let mut g = Linear::new(Point::new(3.0, cy), Point::new(3.0 + w, cy));
                for (p, c) in [
                    (0.0, rgb8(52, 199, 89)),
                    (0.25, rgb8(255, 204, 0)),
                    (0.5, rgb8(255, 149, 0)),
                    (0.75, rgb8(255, 59, 48)),
                    (1.0, rgb8(175, 82, 222)),
                ] {
                    g = g.add_stop(p, c);
                }
                rrect(f, 3.0, cy - h * 0.5, w, h, h * 0.5, g);
                dot(f, 3.0 + frac.clamp(0.02, 0.98) * w, cy);
            }
            Self::SunArc(rise, set, now, light) => {
                let (hy, amp) = (bh * 0.62, bh * 0.34);
                line(f, (0.0, hy), (bw, hy), 1.0, if *light { rgba8(30, 48, 76, 0.24) } else { rgba8(255, 255, 255, 0.28) });
                let prog = match (*rise, *set) {
                    (Some(r), Some(s)) if s > r => ((*now - r) as f32 / (s - r) as f32).clamp(-0.2, 1.2),
                    _ => 0.5,
                };
                let ay = |u: f32| hy - (u * PI * 1.4 - PI * 0.2).sin() * amp;
                let cols = if *light {
                    (rgba8(240, 166, 26, 0.92), rgba8(90, 110, 140, 0.32))
                } else {
                    (rgba8(245, 228, 178, 0.88), rgba8(136, 158, 192, 0.36))
                };
                for (above, col) in [(false, cols.1), (true, cols.0)] {
                    let path = Path::new(|pb| {
                        for i in 0..44 {
                            let (u0, u1) = (i as f32 / 44.0, (i + 1) as f32 / 44.0);
                            let (y0, y1) = (ay(u0), ay(u1));
                            if ((y0 + y1) * 0.5 <= hy) == above {
                                pb.move_to(Point::new(u0 * bw, y0));
                                pb.line_to(Point::new(u1 * bw, y1));
                            }
                        }
                    });
                    f.stroke(&path, Stroke::default().with_width(2.2).with_color(col));
                }
                let cu = (0.143 + prog * 0.714).clamp(0.05, 0.95);
                let sy = ay(cu);
                let above_hor = sy <= hy;
                let (gc, cc) = if above_hor {
                    (rgba8(255, 210, 82, 0.34), if *light { rgb8(255, 176, 18) } else { rgb8(255, 246, 210) })
                } else {
                    (rgba8(180, 200, 232, 0.24), if *light { rgb8(88, 108, 140) } else { rgba8(216, 228, 248, 0.85) })
                };
                circ(f, cu * bw, sy, 8.2, gc);
                circ(f, cu * bw, sy, 4.0, cc);
            }
            Self::Moon(phase) => {
                let ((cx, cy, p), rad) = ((bw * 0.5, bh * 0.5, phase.clamp(0.0, 1.0) as f32), (bw.min(bh) * 0.45).max(16.0));
                circ(f, cx, cy, rad + 3.0, rgba8(210, 224, 248, 0.08));
                circ(f, cx, cy, rad, rgb8(50, 60, 78));
                let k = (p * TAU).cos();
                let lit = Path::new(|pb| {
                    const N: usize = 32;
                    for i in 0..=N {
                        let v = (i as f32 / N as f32) * 2.0 - 1.0;
                        let x = (1.0 - v * v).max(0.0).sqrt() * rad * if p <= 0.5 { 1.0 } else { -1.0 };
                        let pt = Point::new(cx + x, cy + v * rad);
                        if i == 0 {
                            pb.move_to(pt);
                        } else {
                            pb.line_to(pt);
                        }
                    }
                    for i in (0..=N).rev() {
                        let v = (i as f32 / N as f32) * 2.0 - 1.0;
                        let x = (1.0 - v * v).max(0.0).sqrt() * rad * k * if p <= 0.5 { 1.0 } else { -1.0 };
                        pb.line_to(Point::new(cx + x, cy + v * rad));
                    }
                    pb.close();
                });
                f.fill(&lit, rgb8(224, 232, 244));
                for (dx, dy, cr, a) in [
                    (-0.22, -0.25, 0.22, 0.16),
                    (0.18, -0.14, 0.18, 0.14),
                    (0.08, 0.24, 0.24, 0.15),
                    (-0.28, 0.18, 0.14, 0.12),
                    (0.32, 0.12, 0.12, 0.13),
                ] {
                    circ(f, cx + dx * rad, cy + dy * rad, cr * rad, rgba8(108, 120, 140, a));
                }
                f.stroke(&Path::circle(Point::new(cx, cy), rad), Stroke::default().with_width(1.0).with_color(rgba8(255, 255, 255, 0.24)));
            }
            Self::Minutely(rates, light) => {
                let (n, by, ch, gap) = (rates.len().max(1), bh - 4.0, (bh - 10.0).max(16.0), 3.0_f32);
                let (guide, dry) = if *light {
                    (rgba8(28, 44, 72, 0.14), rgba8(30, 50, 80, 0.15))
                } else {
                    (rgba8(255, 255, 255, 0.14), rgba8(255, 255, 255, 0.18))
                };
                for fr in [0.0_f32, 0.5, 1.0] {
                    line(f, (0.0, by - fr * ch), (bw, by - fr * ch), 1.0, guide);
                }
                let gaps = gap * (n.saturating_sub(1) as f32);
                let bar_w = ((bw - gaps) / n as f32).max(3.0);
                let sx = ((bw - (bar_w * n as f32 + gaps)) * 0.5).max(0.0);
                for (i, &r) in rates.iter().enumerate() {
                    let h = ((r.sqrt() / 10.0_f64.sqrt()).clamp(0.04, 1.0) as f32 * ch).max(2.5);
                    let col = if r > 5.0 {
                        rgb8(56, 152, 255)
                    } else if r > 1.2 {
                        rgb8(82, 184, 255)
                    } else if r > 0.05 {
                        rgba8(116, 206, 255, 0.88)
                    } else {
                        dry
                    };
                    rrect(f, sx + i as f32 * (bar_w + gap), by - h, bar_w, h, (bar_w * 0.4).min(3.5), col);
                }
            }
        }
    }
}

pub fn temp_to_color(c: f64, a: f32) -> Color {
    const S: [(f64, [u8; 3]); 9] = [
        (-30.0, [94, 53, 177]),
        (-15.0, [41, 121, 255]),
        (0.0, [0, 188, 212]),
        (10.0, [76, 196, 130]),
        (16.0, [212, 225, 87]),
        (22.0, [255, 193, 7]),
        (28.0, [255, 138, 36]),
        (35.0, [244, 67, 54]),
        (44.0, [183, 28, 88]),
    ];
    ramp(c, &S, a)
}
pub fn temp_badge_color(c: f64) -> Color {
    const S: [(f64, [u8; 3]); 9] = [
        (-25.0, [84, 58, 178]),
        (-10.0, [34, 108, 224]),
        (0.0, [22, 134, 214]),
        (8.0, [26, 146, 186]),
        (15.0, [40, 158, 118]),
        (21.0, [186, 146, 26]),
        (27.0, [224, 112, 24]),
        (33.0, [218, 62, 46]),
        (42.0, [168, 28, 72]),
    ];
    ramp(c, &S, 1.0)
}
pub fn wind_to_color(mps: f64, a: f32) -> Color {
    const S: [(f64, [u8; 3]); 5] =
        [(0.0, [145, 210, 255]), (5.0, [185, 232, 255]), (12.0, [120, 240, 215]), (20.0, [255, 214, 92]), (30.0, [255, 112, 82])];
    ramp(mps, &S, a)
}
pub fn pressure_to_color(hpa: f64, a: f32) -> Color {
    const S: [(f64, [u8; 3]); 5] =
        [(980.0, [123, 31, 162]), (998.0, [30, 136, 229]), (1013.25, [102, 187, 106]), (1026.0, [255, 193, 7]), (1042.0, [229, 57, 53])];
    ramp(hpa, &S, a)
}
pub fn aqi_to_color(aqi: i32) -> Color {
    match aqi {
        ..=50 => rgb8(52, 199, 89),
        51..=100 => rgb8(255, 204, 0),
        101..=150 => rgb8(255, 149, 0),
        151..=200 => rgb8(255, 59, 48),
        201..=300 => rgb8(175, 82, 222),
        _ => rgb8(162, 33, 72),
    }
}
pub(crate) fn ramp(val: f64, stops: &[(f64, [u8; 3])], alpha: f32) -> Color {
    if val <= stops[0].0 {
        let [r, g, b] = stops[0].1;
        return rgba8(r, g, b, alpha);
    }
    for w in stops.windows(2) {
        if val <= w[1].0 {
            let t = ((val - w[0].0) / (w[1].0 - w[0].0)).clamp(0.0, 1.0) as f32;
            let (c0, c1, l) = (w[0].1, w[1].1, |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8);
            return rgba8(l(c0[0], c1[0]), l(c0[1], c1[1]), l(c0[2], c1[2]), alpha);
        }
    }
    let [r, g, b] = stops[stops.len() - 1].1;
    rgba8(r, g, b, alpha)
}
