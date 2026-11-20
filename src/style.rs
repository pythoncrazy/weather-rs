use crate::model::{ThemeMode, WeatherCondition};
use iced::advanced::graphics::text::cosmic_text::fontdb::Source;
use iced::advanced::graphics::text::font_system;
use iced::font::{Family, Font, Weight};
use iced::widget::{Space, button, container, scrollable, text};
use iced::{Background, Border, Color, Element, Fill, Length, Shadow, Theme, Vector};
use std::io::Read;
use std::sync::{Arc, Once};

const INTER_FONTS: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/Inter-Regular.woff"),
    include_bytes!("../assets/fonts/Inter-Medium.woff"),
    include_bytes!("../assets/fonts/Inter-SemiBold.woff"),
    include_bytes!("../assets/fonts/Inter-Bold.woff"),
];
static LOAD_FONTS: Once = Once::new();

pub fn ensure_fonts_loaded() {
    use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
    LOAD_FONTS.call_once(|| {
        let sfnts: Vec<_> = INTER_FONTS.par_iter().map(|w| woff_to_sfnt(w)).collect();
        let mut fs = font_system().write().expect("font system");
        for sfnt in sfnts {
            fs.raw()
                .db_mut()
                .load_font_source(Source::Binary(Arc::new(sfnt)));
        }
    });
}

fn woff_to_sfnt(woff: &[u8]) -> Vec<u8> {
    assert_eq!(&woff[..4], b"wOFF");
    let be32 = |i: usize| u32::from_be_bytes(woff[i..i + 4].try_into().expect("4B"));
    let n = usize::from(u16::from_be_bytes([woff[12], woff[13]]));
    let tables: Vec<_> = (0..n)
        .map(|i| {
            let e = 44 + 20 * i;
            let (at, pk, full) = (
                be32(e + 4) as usize,
                be32(e + 8) as usize,
                be32(e + 12) as usize,
            );
            let mut data = Vec::with_capacity(full);
            if pk < full {
                let mut dec = flate2::read::ZlibDecoder::new(&woff[at..at + pk]);
                dec.read_to_end(&mut data).expect("woff");
            } else {
                data.extend_from_slice(&woff[at..at + pk]);
            }
            (&woff[e..e + 4], be32(e + 16), data)
        })
        .collect();
    let (sh, mut out) = (n.ilog2(), Vec::new());
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
    Font {
        family: Family::Name("Inter"),
        weight,
        ..Font::DEFAULT
    }
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
pub fn t<'a>(c: impl text::IntoFragment<'a>, sz: f32, col: Color) -> text::Text<'a> {
    txt(c, sz, Weight::Normal, col)
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
    container(Space::new().width(Fill).height(1))
        .style(flat_box(col, 0.0))
        .into()
}
pub fn pbtn<'a, M: Clone + 'a>(
    c: impl Into<Element<'a, M>>,
    pad: [u16; 2],
    cols: (Color, Color, Color),
    r: f32,
    msg: M,
) -> button::Button<'a, M> {
    button(c)
        .padding(pad)
        .style(subtle_btn(cols.0, cols.1, cols.2, r))
        .on_press(msg)
}

#[derive(Debug, Clone, Copy)]
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

fn c3([r, g, b]: [u8; 3]) -> Color {
    rgb8(r, g, b)
}

pub fn palette_for(cond: WeatherCondition, is_day: bool, mode: ThemeMode) -> SkyPalette {
    use WeatherCondition as W;
    if mode == ThemeMode::Light {
        return SkyPalette {
            sky_top: rgb8(208, 226, 246),
            sky_mid: rgb8(226, 238, 250),
            sky_bot: rgb8(242, 246, 252),
            sidebar_bg: rgb8(226, 232, 240),
            sidebar_selected: rgba8(255, 255, 255, 0.72),
            card_bg: rgba8(255, 255, 255, 0.72),
            card_border: rgba8(30, 50, 80, 0.10),
            divider: rgba8(20, 35, 60, 0.10),
            text_primary: rgb8(18, 26, 38),
            text_secondary: rgb8(68, 84, 108),
            text_muted: rgb8(108, 122, 144),
            accent: rgb8(10, 122, 255),
            is_light: true,
        };
    }
    const PRESETS: [([[u8; 3]; 5], f32); 8] = [
        (
            [
                [24, 32, 46],
                [18, 24, 36],
                [14, 18, 28],
                [20, 26, 36],
                [34, 44, 60],
            ],
            0.68,
        ),
        (
            [
                [28, 114, 198],
                [58, 142, 218],
                [86, 162, 228],
                [36, 78, 126],
                [22, 82, 150],
            ],
            0.44,
        ),
        (
            [
                [58, 108, 164],
                [88, 132, 182],
                [96, 126, 162],
                [54, 76, 104],
                [38, 66, 98],
            ],
            0.48,
        ),
        (
            [
                [94, 108, 124],
                [116, 128, 142],
                [84, 96, 110],
                [76, 86, 98],
                [54, 66, 80],
            ],
            0.56,
        ),
        (
            [
                [46, 62, 84],
                [62, 78, 100],
                [44, 58, 78],
                [42, 54, 70],
                [28, 40, 56],
            ],
            0.56,
        ),
        (
            [
                [88, 108, 134],
                [114, 132, 156],
                [86, 104, 128],
                [64, 80, 100],
                [44, 58, 78],
            ],
            0.52,
        ),
        (
            [
                [32, 38, 58],
                [48, 56, 80],
                [34, 40, 62],
                [30, 36, 52],
                [20, 26, 42],
            ],
            0.60,
        ),
        (
            [
                [18, 26, 50],
                [30, 42, 74],
                [22, 32, 58],
                [20, 28, 46],
                [14, 20, 38],
            ],
            0.56,
        ),
    ];
    let dark = mode == ThemeMode::Dark;
    let idx = if dark {
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
    let (s, ca) = PRESETS[idx];
    SkyPalette {
        sky_top: c3(s[0]),
        sky_mid: c3(s[1]),
        sky_bot: c3(s[2]),
        sidebar_bg: c3(s[3]),
        card_bg: rgba8(s[4][0], s[4][1], s[4][2], ca),
        sidebar_selected: rgba8(255, 255, 255, if dark { 0.12 } else { 0.16 }),
        card_border: rgba8(255, 255, 255, if dark { 0.09 } else { 0.11 }),
        divider: rgba8(255, 255, 255, if dark { 0.10 } else { 0.13 }),
        text_primary: Color::WHITE,
        text_secondary: rgba8(236, 243, 254, if dark { 0.82 } else { 0.84 }),
        text_muted: rgba8(214, 226, 244, if dark { 0.56 } else { 0.58 }),
        accent: if dark {
            rgb8(88, 182, 255)
        } else {
            rgb8(96, 194, 255)
        },
        is_light: false,
    }
}

fn box_style(
    bg: Color,
    bc: Color,
    bw: f32,
    r: f32,
    sa: f32,
    blur: f32,
) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let shadow = if sa > 0.0 {
            Shadow {
                color: rgba8(0, 0, 0, sa),
                offset: Vector::new(0.0, 3.5),
                blur_radius: blur,
            }
        } else {
            Shadow::default()
        };
        let border = Border {
            color: bc,
            width: bw,
            radius: r.into(),
        };
        container::Style {
            background: Some(Background::Color(bg)),
            border,
            shadow,
            ..Default::default()
        }
    }
}

pub fn frosted_card(pal: SkyPalette, r: f32) -> impl Fn(&Theme) -> container::Style {
    box_style(pal.card_bg, pal.card_border, 1.0, r, 0.12, 14.0)
}
pub fn alert_card(pal: SkyPalette, col: Color) -> impl Fn(&Theme) -> container::Style {
    box_style(pal.card_bg, col, 1.2, 14.0, 0.0, 0.0)
}
pub fn flat_box(bg: Color, r: f32) -> impl Fn(&Theme) -> container::Style {
    box_style(bg, Color::TRANSPARENT, 0.0, r, 0.0, 0.0)
}
pub fn hud_pill(bg: Color, bcol: Color, r: f32) -> impl Fn(&Theme) -> container::Style {
    box_style(bg, bcol, 1.0, r, 0.22, 12.0)
}

pub fn subtle_btn(
    normal: Color,
    hover: Color,
    text_color: Color,
    r: f32,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, st| {
        let bg = if matches!(st, button::Status::Hovered | button::Status::Pressed) {
            hover
        } else {
            normal
        };
        let border = Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: r.into(),
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color,
            border,
            ..Default::default()
        }
    }
}

pub fn sleek_scrollable(theme: &Theme, st: scrollable::Status) -> scrollable::Style {
    let hot = matches!(
        st,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    let a = if hot { 0.42 } else { 0.08 };
    let col = if *theme == Theme::Light {
        rgba8(30, 45, 72, a)
    } else {
        rgba8(255, 255, 255, a)
    };
    let border = Border {
        color: Color::TRANSPARENT,
        width: 0.0,
        radius: 4.0.into(),
    };
    let scroller = scrollable::Scroller {
        background: Background::Color(col),
        border,
    };
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller,
    };
    let auto_scroll = scrollable::AutoScroll {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        shadow: Shadow::default(),
        icon: Color::TRANSPARENT,
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll,
    }
}
