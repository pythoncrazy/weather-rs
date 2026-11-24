#![forbid(unsafe_op_in_unsafe_fn)]
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod app;
mod astro;
mod cards;
mod icons;
mod map;
mod model;
mod providers;
mod storage;
mod style;

use iced::{Size, window};
#[cfg(target_os = "linux")]
use std::env;

#[cfg(target_os = "linux")]
mod linux_display {
    use std::{
        ffi::{CString, c_char, c_int, c_void},
        path::PathBuf,
        time::Duration,
    };
    type Open = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    type Query = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_int, *mut c_int, *mut c_int) -> c_int;
    type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
    unsafe extern "C" {
        fn dlopen(f: *const c_char, fl: c_int) -> *mut c_void;
        fn dlsym(h: *mut c_void, s: *const c_char) -> *mut c_void;
        fn dlclose(h: *mut c_void) -> c_int;
    }

    pub fn wayland_live() -> bool {
        let Some(p) = std::env::var_os("WAYLAND_DISPLAY").map(PathBuf::from) else { return false };
        let sock = if p.is_absolute() {
            p
        } else {
            let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") else { return false };
            PathBuf::from(d).join(p)
        };
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    }

    pub fn x11_has_dri3(display: &str) -> bool {
        let Ok(dpy) = CString::new(display) else { return false };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(unsafe { probe_x11_dri3(&dpy) });
        });
        rx.recv_timeout(Duration::from_millis(250)).unwrap_or(false)
    }

    unsafe fn probe_x11_dri3(dpy: &CString) -> bool {
        // SAFETY: dpy is NUL-terminated; static C symbols & lib pointer are null-checked before calls and closed.
        unsafe {
            let lib = dlopen(c"libX11.so.6".as_ptr(), 1);
            if lib.is_null() {
                return false;
            }
            let (po, pq, pc) =
                (dlsym(lib, c"XOpenDisplay".as_ptr()), dlsym(lib, c"XQueryExtension".as_ptr()), dlsym(lib, c"XCloseDisplay".as_ptr()));
            let mut ok = false;
            if !po.is_null() && !pq.is_null() && !pc.is_null() {
                let open = std::mem::transmute::<*mut c_void, Open>(po);
                let query = std::mem::transmute::<*mut c_void, Query>(pq);
                let close = std::mem::transmute::<*mut c_void, Close>(pc);
                let conn = open(dpy.as_ptr());
                if !conn.is_null() {
                    let (mut a, mut b, mut c) = (0, 0, 0);
                    ok = query(conn, c"DRI3".as_ptr(), &mut a, &mut b, &mut c) != 0;
                    close(conn);
                }
            }
            dlclose(lib);
            ok
        }
    }
}

fn configure_display() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: single-threaded startup before Iced/winit or worker threads spawn.
        if env::var_os("NO_AT_BRIDGE").is_none() {
            unsafe { env::set_var("NO_AT_BRIDGE", "1") };
        }
        if let Some(tok) = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"].iter().find_map(|k| env::var(k).ok().filter(|s| !s.is_empty())) {
            sctk_adwaita::set_activation_token(tok);
            unsafe {
                env::remove_var("XDG_ACTIVATION_TOKEN");
                env::remove_var("DESKTOP_STARTUP_ID");
            }
        }
        if env::var_os("WAYLAND_DISPLAY").is_some() && !linux_display::wayland_live() {
            unsafe { env::remove_var("WAYLAND_DISPLAY") };
        }
        // An X11 display without DRI3 (Xvfb, VNC / remote X, some nested servers) cannot present a GPU surface. Forcing
        // wgpu onto software OpenGL there aborts at startup ("Surface::configure: Invalid surface"), so pick iced's
        // software renderer outright — unless the user chose a backend themselves.
        let no_gpu_present = env::var_os("WAYLAND_DISPLAY").is_none()
            && env::var_os("WGPU_BACKEND").is_none()
            && env::var_os("ICED_BACKEND").is_none()
            && env::var("DISPLAY").is_ok_and(|d| !d.is_empty() && !linux_display::x11_has_dri3(&d));
        if no_gpu_present {
            unsafe { env::set_var("ICED_BACKEND", "tiny-skia") };
        }
    }
}

fn main() -> iced::Result {
    configure_display();
    #[cfg(target_os = "linux")]
    sctk_adwaita::preload();
    style::preload_fonts();
    icons::preload_cloud_tex();
    // Single source of truth for the app icon: the same SVG the packaged launcher icons are rendered from.
    const ICON_SVG: &str = include_str!("../assets/icons/weather-rs.svg");
    let icon = window::icon::from_rgba(icons::render_svg_doc_rgba(ICON_SVG, 64, 64.0), 64, 64).ok();
    let win = window::Settings {
        size: Size::new(1180.0, 760.0),
        min_size: Some(Size::new(720.0, 520.0)),
        exit_on_close_request: false,
        icon,
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific { application_id: "weather-rs".into(), ..Default::default() },
        #[cfg(target_os = "macos")]
        platform_specific: window::settings::PlatformSpecific {
            title_hidden: true,
            titlebar_transparent: true,
            fullsize_content_view: true,
        },
        #[cfg(target_os = "windows")]
        platform_specific: window::settings::PlatformSpecific {
            corner_preference: window::settings::platform::CornerPreference::Round,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = iced::application(app::WeatherApp::boot, app::WeatherApp::update, app::WeatherApp::view)
        .title(app::WeatherApp::title)
        .theme(app::WeatherApp::theme)
        .subscription(app::WeatherApp::subscription);
    app.default_font(style::fw(iced::font::Weight::Normal)).window(win).run()
}
