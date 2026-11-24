use crate::model::{ThemeMode, WeatherCondition};
use iced::advanced::graphics::text::{cosmic_text::fontdb::Source, font_system};
use iced::font::{Family, Font, Weight};
use iced::gradient::Linear;
use iced::widget::{Space, button, container, scrollable, text};
use iced::{Background, Border, Color, Element, Fill, Length, Radians, Shadow, Theme, Vector};
use std::{
    f32::consts::PI,
    io::Read,
    sync::{Arc, OnceLock},
};

const INTER_FONTS: [&[u8]; 5] = [
    // ExtraLight is used only for the hero temperature, matching the thin numerals of Apple Weather.
    include_bytes!("../assets/fonts/Inter-ExtraLight.woff"),
    include_bytes!("../assets/fonts/Inter-Regular.woff"),
    include_bytes!("../assets/fonts/Inter-Medium.woff"),
    include_bytes!("../assets/fonts/Inter-SemiBold.woff"),
    include_bytes!("../assets/fonts/Inter-Bold.woff"),
];
static LOAD_FONTS: OnceLock<()> = OnceLock::new();

pub fn preload_fonts() {
    let _ = std::thread::Builder::new().name("font-preload".into()).spawn(ensure_fonts_loaded);
}

pub fn ensure_fonts_loaded() {
    LOAD_FONTS.get_or_init(|| {
        let sfnts = std::thread::scope(|s| {
            // Decode all WOFF weights in parallel while initializing cosmic-text font_system concurrently!
            let hs: Vec<_> = INTER_FONTS.into_iter().map(|b| s.spawn(move || woff_to_sfnt(b))).collect();
            let _ = font_system();
            hs.into_iter().map(|h| h.join().expect("woff decode")).collect::<Vec<_>>()
        });
        let mut fs = font_system().write().expect("font system");
        for sfnt in sfnts {
            fs.raw().db_mut().load_font_source(Source::Binary(Arc::new(sfnt)));
        }
    });
}

#[cfg(test)]
pub fn bench_woff_decode_all() -> usize {
    std::thread::scope(|s| {
        let hs: Vec<_> = INTER_FONTS.into_iter().map(|b| s.spawn(move || woff_to_sfnt(b).len())).collect();
        hs.into_iter().map(|h| h.join().expect("woff")).sum()
    })
}

fn woff_to_sfnt(woff: &[u8]) -> Vec<u8> {
    assert_eq!(&woff[..4], b"wOFF");
    let be32 = |i: usize| u32::from_be_bytes(woff[i..i + 4].try_into().expect("4B"));
    let n = usize::from(u16::from_be_bytes([woff[12], woff[13]]));
    let tables: Vec<_> = (0..n)
        .map(|i| {
            let e = 44 + 20 * i;
            let (at, pk, full) = (be32(e + 4) as usize, be32(e + 8) as usize, be32(e + 12) as usize);
            let mut data = Vec::with_capacity(full);
            if pk < full {
                flate2::read::ZlibDecoder::new(&woff[at..at + pk]).read_to_end(&mut data).expect("woff");
            } else {
                data.extend_from_slice(&woff[at..at + pk]);
            }
            (&woff[e..e + 4], be32(e + 16), data)
        })
        .collect();
    let total: usize = 12 + 16 * n + tables.iter().map(|(_, _, d)| d.len().next_multiple_of(4)).sum::<usize>();
    let (sh, mut out) = (n.ilog2(), Vec::with_capacity(total));
    let rng = 16u16 << sh;
    out.extend_from_slice(&woff[4..8]);
    for v in [n as u16, rng, sh as u16, n as u16 * 16 - rng] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut at = 12 + 16 * n;
    for (tag, sum, data) in &tables {
        out.extend_from_slice(tag);
        for v in [*sum, at as u32, data.len() as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        at += data.len().next_multiple_of(4);
    }
    for (_, _, data) in tables {
        out.extend_from_slice(&data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

pub const fn fw(weight: Weight) -> Font {
    Font { family: Family::Name("Inter"), weight, ..Font::DEFAULT }
}
pub const fn rgb8(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}
pub const fn rgba8(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color::from_rgba(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a)
}
pub fn txt<'a>(c: impl text::IntoFragment<'a>, sz: f32, w: Weight, col: Color) -> text::Text<'a> {
    text(c).size(sz).font(fw(w)).color(col)
}
pub fn tm<'a>(c: impl text::IntoFragment<'a>, sz: f32, col: Color) -> text::Text<'a> {
    txt(c, sz, Weight::Medium, col)
}
pub fn tb<'a>(c: impl text::IntoFragment<'a>, sz: f32, col: Color) -> text::Text<'a> {
    txt(c, sz, Weight::Semibold, col)
}
pub fn tbold<'a>(c: impl text::IntoFragment<'a>, sz: f32, col: Color) -> text::Text<'a> {
    txt(c, sz, Weight::Bold, col)
}
pub fn sp_w<'a, M: 'a>(w: impl Into<Length>) -> Element<'a, M> {
    Space::new().width(w).into()
}
pub fn sp_h<'a, M: 'a>(h: impl Into<Length>) -> Element<'a, M> {
    Space::new().height(h).into()
}
pub fn hairline<'a, M: 'a>(col: Color) -> Element<'a, M> {
    container(Space::new().width(Fill).height(1)).style(flat_box(col, 0.0)).into()
}
pub fn pbtn<'a, M: Clone + 'a>(
    c: impl Into<Element<'a, M>>, pad: [u16; 2], cols: (Color, Color, Color), r: f32, msg: M,
) -> button::Button<'a, M> {
    button(c).padding(pad).style(subtle_btn(cols.0, cols.1, cols.2, r)).on_press(msg)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyPalette {
    pub sky_top: Color,
    pub sky_mid: Color,
    pub sky_bot: Color,
    pub sidebar_bg: Color,
    pub sidebar_selected: Color,
    pub card_bg: Color,
    pub card_border: Color,
    pub divider: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub is_light: bool,
}

pub fn palette_for(cond: WeatherCondition, is_day: bool, mode: ThemeMode) -> SkyPalette {
    use WeatherCondition as W;
    // Authentic macOS Sequoia / iOS 18 Apple Weather sky & vibrancy glass tints:
    // [sky_top, sky_mid, sky_bot, sidebar_bg, card_tint], card_alpha
    const P: [([[u8; 3]; 5], f32); 9] = [
        // 0: Dark mode
        ([[18, 25, 38], [14, 20, 32], [11, 15, 24], [18, 23, 33], [30, 40, 56]], 0.66),
        // 1: Clear / Mostly Clear Day (Apple luminous cerulean)
        ([[24, 102, 196], [52, 138, 222], [96, 174, 238], [28, 70, 122], [16, 70, 142]], 0.36),
        // 2: Partly Cloudy Day (Airy sky blue + soft haze)
        ([[52, 104, 166], [84, 134, 188], [108, 144, 182], [44, 70, 102], [32, 62, 98]], 0.42),
        // 3: Overcast / Fog Day (Refined silver-slate)
        ([[86, 102, 120], [108, 122, 138], [80, 94, 110], [66, 78, 92], [48, 60, 74]], 0.50),
        // 4: Rain / Drizzle (Moody slate blue)
        ([[40, 58, 82], [58, 76, 100], [42, 56, 76], [36, 48, 66], [24, 36, 54]], 0.52),
        // 5: Snow / Sleet Day (Crisp glacial slate)
        ([[82, 104, 132], [110, 130, 156], [86, 106, 132], [58, 74, 96], [40, 54, 74]], 0.48),
        // 6: Thunderstorm (Deep storm indigo)
        ([[26, 32, 54], [42, 50, 76], [30, 36, 58], [24, 30, 48], [18, 24, 42]], 0.56),
        // 7: Clear / Cloudy Night (Apple deep midnight indigo)
        ([[11, 18, 40], [22, 34, 66], [30, 46, 82], [16, 22, 38], [14, 22, 44]], 0.50),
        // 8: Light mode (macOS soft frost)
        ([[206, 226, 248], [224, 238, 252], [242, 247, 253], [228, 234, 242], [255, 255, 255]], 0.76),
    ];
    let (light, dark) = (mode == ThemeMode::Light, mode == ThemeMode::Dark);
    let idx = if light {
        8
    } else if dark {
        0
    } else {
        match (cond, is_day) {
            (W::Clear | W::MostlyClear, true) => 1,
            (W::PartlyCloudy, true) => 2,
            (W::Overcast | W::Fog, true) => 3,
            (W::Drizzle | W::Rain | W::HeavyRain | W::FreezingRain, _) => 4,
            (W::Snow | W::HeavySnow | W::Sleet, true) => 5,
            (W::Thunderstorm, _) => 6,
            (_, false) => 7,
        }
    };
    let ((s, ca), c3) = (P[idx], |[r, g, b]: [u8; 3]| rgb8(r, g, b));
    // Translucent macOS Sequoia / Tahoe sidebar vibrancy: lets sky gradients & volumetric clouds glow softly through!
    let sb_a = if light {
        0.74
    } else if dark {
        0.82
    } else {
        0.62
    };
    SkyPalette {
        sky_top: c3(s[0]),
        sky_mid: c3(s[1]),
        sky_bot: c3(s[2]),
        sidebar_bg: rgba8(s[3][0], s[3][1], s[3][2], sb_a),
        card_bg: rgba8(s[4][0], s[4][1], s[4][2], ca),
        is_light: light,
        sidebar_selected: rgba8(
            255,
            255,
            255,
            if light {
                0.78
            } else if dark {
                0.13
            } else {
                0.18
            },
        ),
        card_border: if light { rgba8(255, 255, 255, 0.78) } else { rgba8(255, 255, 255, if dark { 0.11 } else { 0.18 }) },
        divider: if light { rgba8(20, 35, 60, 0.09) } else { rgba8(255, 255, 255, if dark { 0.09 } else { 0.14 }) },
        text_primary: if light { rgb8(18, 26, 38) } else { Color::WHITE },
        text_secondary: if light { rgb8(66, 82, 106) } else { rgba8(238, 245, 255, if dark { 0.84 } else { 0.90 }) },
        text_muted: if light { rgb8(102, 116, 138) } else { rgba8(224, 236, 255, if dark { 0.58 } else { 0.68 }) },
        accent: if light { rgb8(0, 122, 255) } else { rgb8(90, 194, 255) },
    }
}

fn box_style(bg: Color, bc: Color, bw: f32, r: f32, sa: f32, blur: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(bg)),
        border: Border { color: bc, width: bw, radius: r.into() },
        shadow: if sa > 0.0 {
            Shadow { color: rgba8(0, 0, 0, sa), offset: Vector::new(0.0, 4.0), blur_radius: blur }
        } else {
            Shadow::default()
        },
        ..Default::default()
    }
}

/// Apple Weather mini-sky gradient card for sidebar cities.
pub fn sky_mini_card(top: Color, bot: Color, selected: bool, light_chrome: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let g = Linear::new(Radians(PI)).add_stop(0.0, top).add_stop(1.0, bot);
        let bc =
            if selected { if light_chrome { rgb8(0, 122, 255) } else { rgba8(255, 255, 255, 0.72) } } else { rgba8(255, 255, 255, 0.16) };
        container::Style {
            background: Some(Background::Gradient(g.into())),
            border: Border { color: bc, width: if selected { 1.5 } else { 0.8 }, radius: 13.0.into() },
            shadow: Shadow {
                color: rgba8(0, 0, 0, if selected { 0.28 } else { 0.12 }),
                offset: Vector::new(0.0, if selected { 3.0 } else { 1.5 }),
                blur_radius: if selected { 12.0 } else { 6.0 },
            },
            ..Default::default()
        }
    }
}

pub fn frosted_card(p: SkyPalette, r: f32) -> impl Fn(&Theme) -> container::Style {
    box_style(p.card_bg, p.card_border, if p.is_light { 1.0 } else { 0.9 }, r, if p.is_light { 0.08 } else { 0.15 }, 18.0)
}
pub fn frosted_card_btn(p: SkyPalette, r: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, st| {
        let hov = matches!(st, button::Status::Hovered | button::Status::Pressed);
        let bg = if hov {
            if p.is_light {
                Color { a: (p.card_bg.a + 0.12).min(0.96), ..p.card_bg }
            } else {
                Color {
                    r: (p.card_bg.r + 0.05).min(1.0),
                    g: (p.card_bg.g + 0.06).min(1.0),
                    b: (p.card_bg.b + 0.07).min(1.0),
                    a: (p.card_bg.a + 0.08).min(0.88),
                }
            }
        } else {
            p.card_bg
        };
        let bc = if hov { if p.is_light { rgba8(0, 122, 255, 0.28) } else { rgba8(255, 255, 255, 0.30) } } else { p.card_border };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: p.text_primary,
            border: Border { color: bc, width: if p.is_light { 1.0 } else { 0.9 }, radius: r.into() },
            shadow: Shadow {
                color: rgba8(0, 0, 0, if p.is_light { 0.08 } else { 0.15 }),
                offset: Vector::new(0.0, 4.0),
                blur_radius: 18.0,
            },
            snap: true,
        }
    }
}
pub fn alert_card(p: SkyPalette, c: Color) -> impl Fn(&Theme) -> container::Style {
    box_style(p.card_bg, c, 1.3, 15.0, 0.12, 14.0)
}
pub fn flat_box(bg: Color, r: f32) -> impl Fn(&Theme) -> container::Style {
    box_style(bg, Color::TRANSPARENT, 0.0, r, 0.0, 0.0)
}
pub fn hud_pill(bg: Color, bc: Color, r: f32) -> impl Fn(&Theme) -> container::Style {
    box_style(bg, bc, 1.0, r, 0.24, 14.0)
}

pub fn subtle_btn(normal: Color, hover: Color, text_color: Color, r: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, st| button::Style {
        background: Some(Background::Color(if matches!(st, button::Status::Hovered | button::Status::Pressed) { hover } else { normal })),
        text_color,
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: r.into() },
        ..Default::default()
    }
}

/// Authentic macOS overlay scrollbar: invisible when idle (zero grey clutter under hourly temps), appears smoothly on hover/drag.
pub fn sleek_scrollable(theme: &Theme, st: scrollable::Status) -> scrollable::Style {
    let a = if matches!(st, scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }) { 0.30 } else { 0.0 };
    let col = if *theme == Theme::Light { rgba8(30, 45, 72, a) } else { rgba8(255, 255, 255, a) };
    let scroller = scrollable::Scroller {
        background: Background::Color(col),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 4.0.into() },
    };
    let rail = scrollable::Rail { background: None, border: Border::default(), scroller };
    let auto_scroll = scrollable::AutoScroll {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        shadow: Shadow::default(),
        icon: Color::TRANSPARENT,
    };
    scrollable::Style { container: container::Style::default(), vertical_rail: rail, horizontal_rail: rail, gap: None, auto_scroll }
}
