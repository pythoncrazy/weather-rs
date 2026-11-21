use crate::model::{WeatherCondition, compass_direction};
use crate::style::{SkyPalette, fw, rgb8, rgba8, subtle_btn};
use iced::advanced::image as adv_image;
use iced::font::Weight;
use iced::widget::canvas::gradient::Linear;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke, Text as CanvasText};
use iced::widget::{button, container, image};
use iced::{Alignment, Color, Element, Length::Fill, Point, Rectangle, Renderer, Size, Theme};
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::sync::{LazyLock, OnceLock, RwLock};

static CLOUD_TEX: LazyLock<adv_image::Handle> = LazyLock::new(generate_volumetric_cloud_tex);

pub fn preload_cloud_tex() {
    let _ = std::thread::Builder::new()
        .name("cloud-tex".into())
        .spawn(|| {
            let _ = &*CLOUD_TEX;
            let _ = &*SUN_EVENT_HANDLES;
        });
}

fn line(f: &mut Frame, a: (f32, f32), b: (f32, f32), w: f32, c: Color) {
    let p = Path::line(Point::new(a.0, a.1), Point::new(b.0, b.1));
    f.stroke(&p, Stroke::default().with_width(w).with_color(c));
}
fn circ(f: &mut Frame, x: f32, y: f32, r: f32, c: Color) {
    f.fill(&Path::circle(Point::new(x, y), r), c);
}
fn rrect(f: &mut Frame, x: f32, y: f32, w: f32, h: f32, r: f32, fill: impl Into<canvas::Fill>) {
    f.fill(
        &Path::rounded_rectangle(Point::new(x, y), Size::new(w, h), r.into()),
        fill,
    );
}
fn dot(f: &mut Frame, x: f32, y: f32) {
    circ(f, x, y, 4.6, rgba8(20, 28, 42, 0.85));
    circ(f, x, y, 3.2, Color::WHITE);
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
fn svg_handle(inner: &str) -> adv_image::Handle {
    const PX: u32 = 48;
    let s = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">{inner}</svg>"#);
    let mut px = resvg::tiny_skia::Pixmap::new(PX, PX).expect("icon pixmap");
    if let Ok(tree) = resvg::usvg::Tree::from_str(&s, &resvg::usvg::Options::default()) {
        let sc = PX as f32 / 24.0;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sc, sc),
            &mut px.as_mut(),
        );
    }
    // Un-premultiply RGBA for iced_wgpu::image (which expects straight alpha).
    let mut rgba = px.take();
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            p[0] = ((p[0] as u32 * 255 + (a >> 1)) / a).min(255) as u8;
            p[1] = ((p[1] as u32 * 255 + (a >> 1)) / a).min(255) as u8;
            p[2] = ((p[2] as u32 * 255 + (a >> 1)) / a).min(255) as u8;
        }
    }
    adv_image::Handle::from_rgba(PX, PX, rgba)
}
fn svg_from_handle<'a, M: 'a>(handle: adv_image::Handle, size: f32) -> Element<'a, M> {
    image(handle).width(size).height(size).into()
}

fn smooth_noise(x: f32, y: f32) -> f32 {
    let h = |ix: i32, iy: i32| {
        let n = ix
            .wrapping_mul(374_761_393)
            .wrapping_add(iy.wrapping_mul(668_265_263));
        let m = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
        ((m ^ (m >> 16)) & 0x7fff) as f32 / 32767.0
    };
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let ab = h(ix, iy) + (h(ix + 1, iy) - h(ix, iy)) * ux;
    let cd = h(ix, iy + 1) + (h(ix + 1, iy + 1) - h(ix, iy + 1)) * ux;
    ab + (cd - ab) * uy
}

fn generate_volumetric_cloud_tex() -> adv_image::Handle {
    let (w, h) = (256_u32, 144_u32);
    let mut rgba = vec![0_u8; (w * h * 4) as usize];
    for py in 0..h {
        let vy = py as f32 / h as f32;
        let env = ((1.0 - (vy - 0.24).abs() * 2.1).clamp(0.0, 1.0)).powf(1.35);
        for px in 0..w {
            let (mut x, mut y) = ((px as f32 / w as f32) * 5.5, vy * 3.8);
            let (mut n, mut amp) = (0.0, 0.52);
            for _ in 0..5 {
                n += amp * smooth_noise(x, y);
                (x, y, amp) = (x * 2.03 + 13.7, y * 2.07 + 7.3, amp * 0.5);
            }
            let d = ((n - 0.36) * 2.2).clamp(0.0, 1.0) * env;
            let s = (212.0 + n * 42.0).clamp(195.0, 255.0) as u8;
            let i = ((py * w + px) * 4) as usize;
            let px4 = [
                s,
                s.saturating_add(3),
                s.saturating_add(8),
                (d * 112.0).round() as u8,
            ];
            rgba[i..i + 4].copy_from_slice(&px4);
        }
    }
    adv_image::Handle::from_rgba(w, h, rgba)
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
}

const UI_SVGS: [&str; 29] = [
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
];

static UI_ICON_CACHE: LazyLock<RwLock<HashMap<(u8, u32), adv_image::Handle>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn icon_svg<'a, M: 'a>(icon: UiIcon, size: f32, color: Color) -> Element<'a, M> {
    let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (r, g, b) = (u(color.r), u(color.g), u(color.b));
    let key = (
        icon as u8,
        ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    );
    if let Ok(cache) = UI_ICON_CACHE.read()
        && let Some(h) = cache.get(&key)
    {
        return svg_from_handle(h.clone(), size);
    }
    let hex = format!("#{r:02X}{g:02X}{b:02X}");
    let h = svg_handle(&UI_SVGS[icon as usize].replace("{c}", &hex));
    if let Ok(mut cache) = UI_ICON_CACHE.write() {
        cache.insert(key, h.clone());
    }
    svg_from_handle(h, size)
}

pub fn icon_btn<'a, M: Clone + 'a>(
    icon: UiIcon,
    isz: f32,
    sz: (u32, u32),
    cols: (Color, Color, Color),
    r: f32,
    msg: M,
) -> Element<'a, M> {
    let ic = container(icon_svg(icon, isz, cols.0))
        .center_x(Fill)
        .center_y(Fill);
    button(ic)
        .width(sz.0)
        .height(sz.1)
        .padding(0)
        .style(subtle_btn(cols.1, cols.2, cols.0, r))
        .on_press(msg)
        .into()
}

static WEATHER_ICON_HANDLES: [OnceLock<adv_image::Handle>; 26] = [const { OnceLock::new() }; 26];

fn build_weather_svg_handle(cond: WeatherCondition, is_day: bool) -> adv_image::Handle {
    const SUN: &str = r##"<circle cx="12" cy="12" r="5.2" fill="#FFCC33"/><g stroke="#FFCC33" stroke-width="1.8" stroke-linecap="round"><line x1="12" y1="3" x2="12" y2="4.8"/><line x1="12" y1="19.2" x2="12" y2="21"/><line x1="3" y1="12" x2="4.8" y2="12"/><line x1="19.2" y1="12" x2="21" y2="12"/><line x1="5.6" y1="5.6" x2="6.9" y2="6.9"/><line x1="17.1" y1="17.1" x2="18.4" y2="18.4"/><line x1="18.4" y1="5.6" x2="17.1" y2="6.9"/><line x1="6.9" y1="17.1" x2="5.6" y2="18.4"/></g>"##;
    const MOON: &str = r##"<path d="M17.8 14.6A6.5 6.5 0 0 1 9.4 6.2a6.5 6.5 0 1 0 8.4 8.4Z" fill="#CFE0FA" stroke="#7B98C4" stroke-width="0.9" stroke-linejoin="round"/>"##;
    const SSUN: &str = r##"<circle cx="8.8" cy="9" r="4" fill="#FFCC33"/><g stroke="#FFCC33" stroke-width="1.5" stroke-linecap="round"><line x1="8.8" y1="2.8" x2="8.8" y2="4"/><line x1="2.6" y1="9" x2="3.8" y2="9"/><line x1="4.4" y1="4.6" x2="5.3" y2="5.5"/><line x1="13.2" y1="4.6" x2="12.3" y2="5.5"/></g>"##;
    const SMOON: &str = r##"<path d="M12.2 9.8A4.2 4.2 0 0 1 6.8 4.4a4.2 4.2 0 1 0 5.4 5.4Z" fill="#CFE0FA" stroke="#7B98C4" stroke-width="0.85" stroke-linejoin="round"/>"##;
    const CLD: &str = r##"<path d="M7.5 18.5h9.8a3.7 3.7 0 0 0 .4-7.4A5.2 5.2 0 0 0 7.7 9.6 4.3 4.3 0 0 0 7.5 18.5Z" fill="#EEF3FA" stroke="#8CA4C4" stroke-width="0.85" stroke-linejoin="round"/>"##;
    const BCK: &str = r##"<path d="M5.5 15.5h8.5a3 3 0 0 0 .3-6 4.2 4.2 0 0 0-8-1.2 3.5 3.5 0 0 0-.8 7.2Z" fill="#B0C2DA" stroke="#7F97B8" stroke-width="0.75" stroke-linejoin="round"/>"##;
    const STM: &str = r##"<path d="M6.8 14.5h10.2a3.6 3.6 0 0 0 .3-7.2 5 5 0 0 0-9.7-1.4 4.1 4.1 0 0 0-.8 8.6Z" fill="#C6D4E6" stroke="#7E96B8" stroke-width="0.85" stroke-linejoin="round"/>"##;
    use WeatherCondition as W;
    let inner: String = match cond {
        W::Clear => (if is_day { SUN } else { MOON }).into(),
        W::MostlyClear | W::PartlyCloudy => format!("{}{CLD}", if is_day { SSUN } else { SMOON }),
        W::Overcast => format!("{BCK}{CLD}"),
        W::Fog => format!(
            r##"{STM}<g stroke="#C8D6EA" stroke-width="1.7" stroke-linecap="round"><line x1="5.5" y1="17.2" x2="18.5" y2="17.2"/><line x1="7.5" y1="19.8" x2="16.5" y2="19.8"/></g>"##
        ),
        W::Drizzle => format!(
            r##"{STM}<g stroke="#5AC8FA" stroke-width="1.8" stroke-linecap="round"><line x1="9.5" y1="15.8" x2="8.7" y2="18.4"/><line x1="14.5" y1="15.8" x2="13.7" y2="18.4"/></g>"##
        ),
        W::Rain | W::HeavyRain | W::FreezingRain => format!(
            r##"{STM}<g stroke="#47B8FF" stroke-width="1.85" stroke-linecap="round"><line x1="8.2" y1="15.8" x2="7.2" y2="18.7"/><line x1="12.2" y1="15.8" x2="11.2" y2="18.7"/><line x1="16.2" y1="15.8" x2="15.2" y2="18.7"/></g>"##
        ),
        W::Snow | W::HeavySnow | W::Sleet => format!(
            r##"{STM}<g fill="#9ED4FA" stroke="#6AA9DC" stroke-width="0.5"><circle cx="8.5" cy="17.6" r="1.25"/><circle cx="12.2" cy="18.5" r="1.25"/><circle cx="15.8" cy="17.6" r="1.25"/></g>"##
        ),
        W::Thunderstorm => format!(
            r##"{STM}<polygon points="12.8,13.6 9.5,17.8 12.2,17.8 10.9,21.0 15.3,16.6 12.5,16.6" fill="#FFD43B"/>"##
        ),
    };
    svg_handle(&inner)
}

pub fn weather_icon_svg<'a, M: 'a>(
    cond: WeatherCondition,
    is_day: bool,
    size: f32,
) -> Element<'a, M> {
    let h = WEATHER_ICON_HANDLES[(cond as usize) * 2 + usize::from(is_day)]
        .get_or_init(|| build_weather_svg_handle(cond, is_day));
    svg_from_handle(h.clone(), size)
}

static SUN_EVENT_HANDLES: LazyLock<[adv_image::Handle; 2]> = LazyLock::new(|| {
    let build = |arr: &str| {
        svg_handle(&format!(
            r##"{arr}<path d="M6 16a6 6 0 0 1 12 0" fill="#FFC83D"/><line x1="3" y1="16.5" x2="21" y2="16.5" stroke="#FFE394" stroke-width="1.8" stroke-linecap="round"/><line x1="6.5" y1="19.5" x2="17.5" y2="19.5" stroke="#FFFFFF" stroke-opacity="0.55" stroke-width="1.5" stroke-linecap="round"/>"##
        ))
    };
    [
        build(
            r##"<polyline points="9.5,5.5 12,8 14.5,5.5" fill="none" stroke="#FFB347" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"##,
        ),
        build(
            r##"<polyline points="9.5,7.5 12,5 14.5,7.5" fill="none" stroke="#FFD452" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"##,
        ),
    ]
});

pub fn sun_event_svg<'a, M: 'a>(is_sunrise: bool, size: f32) -> Element<'a, M> {
    svg_from_handle(SUN_EVENT_HANDLES[usize::from(is_sunrise)].clone(), size)
}

pub enum WidgetCanvas {
    SkyBackdrop(SkyPalette, WeatherCondition),
    SkyParticles(bool, WeatherCondition, bool, f32),
    TempBar((f64, f64), (f64, f64), Option<f64>, bool),
    Spectrum(f32),
    Compass(f64, SkyPalette),
    SunArc(Option<i64>, Option<i64>, i64, bool),
    Pressure(f64, String, &'static str, SkyPalette),
    Moon(f64),
    Minutely(Vec<f64>, bool),
}

pub fn widget_canvas<'a, M: 'a>(
    kind: WidgetCanvas,
    w: impl Into<iced::Length>,
    h: impl Into<iced::Length>,
) -> Element<'a, M> {
    Canvas::new(kind).width(w).height(h).into()
}

fn draw_sky_backdrop(f: &mut Frame, (bw, bh): (f32, f32), pal: SkyPalette, cond: WeatherCondition) {
    use WeatherCondition as W;
    let g = Linear::new(Point::ORIGIN, Point::new(0.0, bh))
        .add_stop(0.0, pal.sky_top)
        .add_stop(0.42, pal.sky_mid)
        .add_stop(1.0, pal.sky_bot);
    f.fill_rectangle(Point::ORIGIN, Size::new(bw, bh), g);
    if pal.is_light {
        return;
    }
    let op: f32 = match cond {
        W::PartlyCloudy => 0.55,
        W::Overcast | W::Fog => 0.85,
        W::Drizzle | W::Rain | W::HeavyRain => 0.65,
        _ => 0.0,
    };
    if op > 0.0 {
        let r = Rectangle {
            x: 0.0,
            y: 0.0,
            width: bw,
            height: (bh * 0.62).min(520.0),
        };
        f.draw_image(r, adv_image::Image::new(CLOUD_TEX.clone()).opacity(op));
    }
}

fn draw_sky_particles(
    f: &mut Frame,
    (bw, bh): (f32, f32),
    is_light: bool,
    cond: WeatherCondition,
    day: bool,
    ph: f32,
) {
    use WeatherCondition as W;
    if is_light {
        return;
    }
    if !day && matches!(cond, W::Clear | W::MostlyClear | W::PartlyCloudy) {
        for i in 0..42 {
            let fx = ((i as f32 * 127.1).sin() * 43_758.547).fract().abs();
            let fy = ((i as f32 * 311.7).sin() * 19_583.21).fract().abs() * 0.48;
            let tw = 0.45 + 0.55 * (0.5 + 0.5 * (ph * 1.25 + i as f32 * 1.7).sin());
            let r = if i % 5 == 0 { 1.4 } else { 0.9 };
            circ(f, fx * bw, fy * bh, r, rgba8(255, 255, 255, tw * 0.55));
        }
    }
    let snow = matches!(cond, W::Snow | W::HeavySnow | W::Sleet);
    let rain = matches!(
        cond,
        W::Drizzle | W::Rain | W::HeavyRain | W::FreezingRain | W::Thunderstorm
    );
    if !(snow || rain) {
        return;
    }
    let heavy = matches!(cond, W::HeavyRain | W::Thunderstorm);
    let count = if snow { 56 } else { 52 + 20 * i32::from(heavy) };
    for i in 0..count {
        let d = (i % 3) as f32;
        let h1 = ((i as f32 * 91.7).sin() * 24_819.3).fract().abs();
        let h2 = ((i as f32 * 213.3).cos() * 15_731.9).fract().abs();
        let spd = if snow {
            34.0 + d * 18.0 + h2 * 24.0
        } else {
            290.0 + d * 125.0 + h2 * 120.0
        };
        let y = (h2 * bh + ph * spd).rem_euclid(bh);
        let fade = ((y / (bh * 0.11)).min(1.0) * ((bh - y) / (bh * 0.13)).min(1.0)).clamp(0.0, 1.0);
        if snow {
            let sw = (ph * (0.95 + d * 0.22) + i as f32 * 0.85).sin() * (10.0 + d * 4.5)
                + (ph * 1.65 + i as f32 * 1.9).cos() * 3.5;
            let r = 1.05 + d * 0.55 + if i % 7 == 0 { 0.35 } else { 0.0 };
            let col = rgba8(242, 249, 255, (0.16 + d * 0.10) * fade);
            circ(f, (h1 * bw + sw).rem_euclid(bw), y, r, col);
        } else {
            let sl = 0.095 + d * 0.015;
            let len = 10.0 + d * 6.5 + if heavy { 4.0 } else { 0.0 };
            let x = (h1 * bw - y * sl).rem_euclid(bw);
            let col = rgba8(205, 232, 255, (0.09 + d * 0.055) * fade);
            line(f, (x, y), (x - len * sl, y + len), 0.85 + d * 0.22, col);
        }
    }
}

fn draw_compass(f: &mut Frame, (bw, bh): (f32, f32), deg: f64, pal: SkyPalette) {
    let (cx, cy, sm) = (bw * 0.5, bh * 0.5, bw < 66.0);
    let rad = (bw.min(bh) * 0.5 - 4.0).max(20.0);
    let pt = |a: f32, r: f32| (cx + a.cos() * r, cy + a.sin() * r);
    let needle = if pal.is_light {
        rgb8(24, 34, 50)
    } else {
        Color::WHITE
    };
    for i in 0..72 {
        let d = i as f32 * 5.0;
        let skip = [0.0_f32, 90.0, 180.0, 270.0, 360.0]
            .iter()
            .any(|x| (d - x).abs() < 6.0);
        if skip {
            continue;
        }
        let a = (d - 90.0).to_radians();
        let al = if i % 6 == 0 { 0.45 } else { 0.21 };
        let col = if pal.is_light {
            rgba8(28, 42, 68, al)
        } else {
            rgba8(255, 255, 255, al)
        };
        let r0 = rad - if i % 6 == 0 { 5.5 } else { 3.5 };
        line(f, pt(a, r0), pt(a, rad), 1.0, col);
    }
    for (d, l) in [(0.0_f32, "N"), (90.0, "E"), (180.0, "S"), (270.0, "W")] {
        let p = pt((d - 90.0).to_radians(), rad - if sm { 4.8 } else { 6.0 });
        let sz = if sm { 7.5 } else { 9.5 };
        label(f, l, p.0, p.1, pal.text_muted, sz, Weight::Bold);
    }
    let to = (deg as f32 + 90.0).to_radians();
    let (tip, tail) = (pt(to, rad), pt(to + PI, rad - 2.0));
    let (gap, wing) = if sm { (11.5, 5.0) } else { (15.0, 6.5) };
    line(f, tail, pt(to + PI, gap), 1.6, needle);
    line(f, pt(to, gap), tip, 1.6, needle);
    circ(f, tail.0, tail.1, if sm { 2.0 } else { 2.6 }, needle);
    let head = Path::new(|b| {
        b.move_to(Point::new(tip.0, tip.1));
        let (a1, a2) = (to - 0.45, to + 0.45);
        b.line_to(Point::new(tip.0 - a1.cos() * wing, tip.1 - a1.sin() * wing));
        b.line_to(Point::new(tip.0 - a2.cos() * wing, tip.1 - a2.sin() * wing));
        b.close();
    });
    f.fill(&head, needle);
    let csz = if sm { 8.5 } else { 11.0 };
    label(
        f,
        compass_direction(deg),
        cx,
        cy,
        pal.text_primary,
        csz,
        Weight::Bold,
    );
}

fn draw_pressure(
    f: &mut Frame,
    (bw, bh): (f32, f32),
    hpa: f64,
    val: &str,
    unit: &str,
    pal: SkyPalette,
) {
    let (cx, cy) = (bw * 0.5, bh * 0.48);
    let rad = (bw.min(bh) * 0.44).max(22.0);
    let active = (((hpa - 980.0) / 60.0).clamp(0.0, 1.0) * 36.0).round() as i32;
    let dim = if pal.is_light {
        rgba8(28, 42, 68, 0.24)
    } else {
        rgba8(255, 255, 255, 0.28)
    };
    for i in 0..=36 {
        let a = (135.0 + (i as f32 / 36.0) * 270.0).to_radians();
        let act = (i - active).abs() <= 1;
        let r0 = rad - if act { 7.5 } else { 4.8 };
        let col = if act { pal.text_primary } else { dim };
        let (p0, p1) = (
            (cx + a.cos() * r0, cy + a.sin() * r0),
            (cx + a.cos() * rad, cy + a.sin() * rad),
        );
        line(f, p0, p1, if act { 2.2 } else { 1.1 }, col);
    }
    label(f, val, cx, cy - 2.0, pal.text_primary, 14.0, Weight::Bold);
    label(f, unit, cx, cy + 12.0, pal.text_muted, 9.5, Weight::Medium);
    label(
        f,
        "Low",
        cx - rad * 0.62,
        bh - 7.0,
        pal.text_muted,
        9.5,
        Weight::Semibold,
    );
    label(
        f,
        "High",
        cx + rad * 0.62,
        bh - 7.0,
        pal.text_muted,
        9.5,
        Weight::Semibold,
    );
}

impl<M> canvas::Program<M> for WidgetCanvas {
    type State = ();

    fn draw(
        &self,
        _: &(),
        r: &Renderer,
        _: &Theme,
        b: Rectangle,
        _: iced::mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut f = Frame::new(r, b.size());
        let (bw, bh) = (b.width.max(1.0), b.height.max(1.0));
        match self {
            Self::SkyBackdrop(pal, cond) => draw_sky_backdrop(&mut f, (bw, bh), *pal, *cond),
            Self::SkyParticles(light, cond, day, ph) => {
                draw_sky_particles(&mut f, (bw, bh), *light, *cond, *day, *ph)
            }
            Self::Compass(deg, pal) => draw_compass(&mut f, (bw, bh), *deg, *pal),
            Self::Pressure(hpa, val, unit, pal) => {
                draw_pressure(&mut f, (bw, bh), *hpa, val, unit, *pal)
            }
            Self::TempBar((dmin, dmax), (omin, omax), cur, light) => {
                let (cy, h, pad, w) = (bh * 0.5, 4.5_f32, 4.0_f32, (bw - 8.0).max(12.0));
                let bg = if *light {
                    rgba8(24, 42, 72, 0.14)
                } else {
                    rgba8(12, 18, 28, 0.36)
                };
                rrect(&mut f, pad, cy - h * 0.5, w, h, h * 0.5, bg);
                let span = (omax - omin).max(1.0);
                let px = |t: f64| pad + ((t - omin) / span).clamp(0.0, 1.0) as f32 * w;
                let x0 = px(*dmin);
                let x1 = px(*dmax).max(x0 + 7.0).min(pad + w);
                let g = Linear::new(Point::new(x0, cy), Point::new(x1, cy))
                    .add_stop(0.0, temp_to_color(*dmin, 1.0))
                    .add_stop(0.5, temp_to_color((*dmin + *dmax) * 0.5, 1.0))
                    .add_stop(1.0, temp_to_color(*dmax, 1.0));
                rrect(&mut f, x0, cy - h * 0.5, x1 - x0, h, h * 0.5, g);
                if let Some(c) = cur {
                    dot(&mut f, px(*c).clamp(x0, x1), cy);
                }
            }
            Self::Spectrum(frac) => {
                let (cy, h, w) = (bh * 0.5, 4.5_f32, (bw - 6.0).max(10.0));
                let g = Linear::new(Point::new(3.0, cy), Point::new(3.0 + w, cy))
                    .add_stop(0.0, rgb8(52, 199, 89))
                    .add_stop(0.25, rgb8(255, 204, 0))
                    .add_stop(0.50, rgb8(255, 149, 0))
                    .add_stop(0.75, rgb8(255, 59, 48))
                    .add_stop(1.0, rgb8(175, 82, 222));
                rrect(&mut f, 3.0, cy - h * 0.5, w, h, h * 0.5, g);
                dot(&mut f, 3.0 + frac.clamp(0.02, 0.98) * w, cy);
            }
            Self::SunArc(rise, set, now, light) => {
                let (hy, amp) = (bh * 0.62, bh * 0.34);
                let hc = if *light {
                    rgba8(30, 48, 76, 0.26)
                } else {
                    rgba8(255, 255, 255, 0.32)
                };
                line(&mut f, (0.0, hy), (bw, hy), 1.0, hc);
                let prog = match (*rise, *set) {
                    (Some(r), Some(s)) if s > r => {
                        ((*now - r) as f32 / (s - r) as f32).clamp(-0.2, 1.2)
                    }
                    _ => 0.5,
                };
                let ay = |u: f32| hy - (u * PI * 1.4 - PI * 0.2).sin() * amp;
                for i in 0..44 {
                    let (u0, u1) = (i as f32 / 44.0, (i + 1) as f32 / 44.0);
                    let (y0, y1) = (ay(u0), ay(u1));
                    let col = match ((y0 + y1) * 0.5 <= hy, *light) {
                        (true, true) => rgba8(52, 82, 128, 0.72),
                        (true, false) => rgba8(225, 236, 252, 0.85),
                        (false, true) => rgba8(90, 110, 140, 0.34),
                        (false, false) => rgba8(140, 160, 190, 0.42),
                    };
                    line(&mut f, (u0 * bw, y0), (u1 * bw, y1), 2.0, col);
                }
                let cu = (0.143 + prog * 0.714).clamp(0.05, 0.95);
                let (sx, sy) = (cu * bw, ay(cu));
                let (gc, cc) = if *light {
                    (rgba8(255, 190, 32, 0.32), rgb8(255, 178, 24))
                } else {
                    (rgba8(255, 255, 255, 0.28), Color::WHITE)
                };
                circ(&mut f, sx, sy, if *light { 7.0 } else { 6.8 }, gc);
                circ(&mut f, sx, sy, if *light { 4.0 } else { 3.6 }, cc);
            }
            Self::Moon(phase) => {
                let (cx, cy, p) = (bw * 0.5, bh * 0.5, phase.clamp(0.0, 1.0) as f32);
                let rad = (bw.min(bh) * 0.45).max(16.0);
                circ(&mut f, cx, cy, rad, rgb8(56, 66, 82));
                for row in 0..40 {
                    let v = (row as f32 + 0.5) / 40.0 * 2.0 - 1.0;
                    let hx = (1.0 - v * v).max(0.0).sqrt() * rad;
                    let term = (p * TAU).cos() * hx;
                    let (x0, x1) = if p <= 0.5 { (term, hx) } else { (-hx, -term) };
                    if x1 > x0 {
                        let y = cy + v * rad;
                        line(
                            &mut f,
                            (cx + x0, y),
                            (cx + x1, y),
                            rad * 2.0 / 40.0 + 0.9,
                            rgb8(216, 224, 236),
                        );
                    }
                }
                const CRATERS: [(f32, f32, f32, f32); 5] = [
                    (-0.22, -0.25, 0.22, 0.16),
                    (0.18, -0.14, 0.18, 0.14),
                    (0.08, 0.24, 0.24, 0.15),
                    (-0.28, 0.18, 0.14, 0.12),
                    (0.32, 0.12, 0.12, 0.13),
                ];
                for (dx, dy, cr, a) in CRATERS {
                    circ(
                        &mut f,
                        cx + dx * rad,
                        cy + dy * rad,
                        cr * rad,
                        rgba8(110, 122, 140, a),
                    );
                }
                let rim = Stroke::default()
                    .with_width(1.0)
                    .with_color(rgba8(255, 255, 255, 0.22));
                f.stroke(&Path::circle(Point::new(cx, cy), rad), rim);
            }
            Self::Minutely(rates, light) => {
                let (n, by, ch, gap) =
                    (rates.len().max(1), bh - 4.0, (bh - 10.0).max(16.0), 3.0_f32);
                let guide = if *light {
                    rgba8(28, 44, 72, 0.14)
                } else {
                    rgba8(255, 255, 255, 0.14)
                };
                for fr in [0.0_f32, 0.5, 1.0] {
                    line(&mut f, (0.0, by - fr * ch), (bw, by - fr * ch), 1.0, guide);
                }
                let gaps = gap * (n.saturating_sub(1) as f32);
                let bar_w = ((bw - gaps) / n as f32).max(3.0);
                let sx = ((bw - (bar_w * n as f32 + gaps)) * 0.5).max(0.0);
                let dry = if *light {
                    rgba8(30, 50, 80, 0.15)
                } else {
                    rgba8(255, 255, 255, 0.18)
                };
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
                    rrect(
                        &mut f,
                        sx + i as f32 * (bar_w + gap),
                        by - h,
                        bar_w,
                        h,
                        (bar_w * 0.4).min(3.5),
                        col,
                    );
                }
            }
        }
        vec![f.into_geometry()]
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
    // Rich, high-contrast Apple Weather pin fills calibrated for crisp white numerals.
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
    const S: [(f64, [u8; 3]); 5] = [
        (0.0, [145, 210, 255]),
        (5.0, [185, 232, 255]),
        (12.0, [120, 240, 215]),
        (20.0, [255, 214, 92]),
        (30.0, [255, 112, 82]),
    ];
    ramp(mps, &S, a)
}
pub fn pressure_to_color(hpa: f64, a: f32) -> Color {
    const S: [(f64, [u8; 3]); 5] = [
        (980.0, [123, 31, 162]),
        (998.0, [30, 136, 229]),
        (1013.25, [102, 187, 106]),
        (1026.0, [255, 193, 7]),
        (1042.0, [229, 57, 53]),
    ];
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
            let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            let (c0, c1) = (w[0].1, w[1].1);
            return rgba8(l(c0[0], c1[0]), l(c0[1], c1[1]), l(c0[2], c1[2]), alpha);
        }
    }
    let [r, g, b] = stops[stops.len() - 1].1;
    rgba8(r, g, b, alpha)
}
