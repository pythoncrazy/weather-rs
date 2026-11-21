#![forbid(unsafe_op_in_unsafe_fn)]

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
use std::env;

#[cfg(target_os = "linux")]
mod linux_display {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::path::PathBuf;
    use std::time::Duration;

    type Open = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    type Query = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_int,
        *mut c_int,
        *mut c_int,
    ) -> c_int;
    type Close = unsafe extern "C" fn(*mut c_void) -> c_int;

    unsafe extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
    }

    pub fn wayland_live() -> bool {
        let Some(d) = std::env::var_os("WAYLAND_DISPLAY") else {
            return false;
        };
        let p = PathBuf::from(&d);
        let sock = if p.is_absolute() {
            p
        } else {
            let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
                return false;
            };
            PathBuf::from(dir).join(p)
        };
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    }

    pub fn x11_has_dri3(display: &str) -> bool {
        let Ok(dpy) = CString::new(display) else {
            return false;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: NUL-terminated static C symbols, null-checked before calls, display & lib closed.
            let _ = tx.send(unsafe { probe_x11_dri3(&dpy) });
        });
        rx.recv_timeout(Duration::from_millis(250)).unwrap_or(false)
    }

    unsafe fn probe_x11_dri3(dpy: &CString) -> bool {
        // SAFETY: Caller guarantees dpy is a valid NUL-terminated X11 display string.
        unsafe {
            let lib = dlopen(c"libX11.so.6".as_ptr(), 1);
            if lib.is_null() {
                return false;
            }
            let po = dlsym(lib, c"XOpenDisplay".as_ptr());
            let pq = dlsym(lib, c"XQueryExtension".as_ptr());
            let pc = dlsym(lib, c"XCloseDisplay".as_ptr());
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
        if env::var_os("NO_AT_BRIDGE").is_none() {
            // SAFETY: single-threaded startup before Iced/winit or GTK preload threads spawn.
            // Prevents offscreen GTK3 titlebar rendering from registering an unpumped AT-SPI bridge.
            unsafe { env::set_var("NO_AT_BRIDGE", "1") };
        }
        if let Some(tok) = env::var("XDG_ACTIVATION_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                env::var("DESKTOP_STARTUP_ID")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
        {
            sctk_adwaita::set_activation_token(tok);
            // SAFETY: single-threaded startup before Iced/winit or GTK preload threads spawn.
            // Prevents libgtk-3 from swallowing the token without mapping a GTK window.
            unsafe {
                env::remove_var("XDG_ACTIVATION_TOKEN");
                env::remove_var("DESKTOP_STARTUP_ID");
            }
        }
        if env::var_os("WAYLAND_DISPLAY").is_some() && !linux_display::wayland_live() {
            // SAFETY: single-threaded startup before Iced/winit or worker threads spawn.
            unsafe { env::remove_var("WAYLAND_DISPLAY") };
        }
        if env::var_os("WAYLAND_DISPLAY").is_none()
            && env::var("DISPLAY").is_ok_and(|d| !d.is_empty() && !linux_display::x11_has_dri3(&d))
            && env::var_os("WGPU_BACKEND").is_none()
        {
            // SAFETY: single-threaded startup before Iced/wgpu initializes.
            unsafe {
                env::set_var("WGPU_BACKEND", "gl");
                if env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
                    env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
                }
            }
        }
    }
}

fn build_app_icon() -> Option<window::Icon> {
    const N: usize = 64;
    let mut px = vec![0u8; N * N * 4];
    let blend = |dst: &mut [f32; 3], src: [f32; 3], a: f32| {
        for i in 0..3 {
            dst[i] = dst[i] * (1.0 - a) + src[i] * a;
        }
    };
    for y in 0..N {
        let t = y as f32 / (N - 1) as f32;
        for x in 0..N {
            let (fx, fy) = (x as f32, y as f32);
            let qx = ((fx - 31.5).abs() - 18.5).max(0.0);
            let qy = ((fy - 31.5).abs() - 18.5).max(0.0);
            let mask = (13.0 - qx.hypot(qy)).clamp(0.0, 1.0);
            if mask <= 0.0 {
                continue;
            }
            let mut c = [24.0 + 48.0 * t, 114.0 + 58.0 * t, 216.0 + 28.0 * t];
            let s =
                (1.0 - ((fx - 26.0).hypot(fy - 24.0) - 10.5).clamp(0.0, 1.5) / 1.5).clamp(0.0, 1.0);
            if s > 0.0 {
                blend(&mut c, [255.0, 214.0, 64.0], s);
            }
            let cl = [(26.0, 39.0, 9.5), (36.0, 36.0, 11.5), (45.0, 40.0, 8.5)]
                .into_iter()
                .map(|(cx, cy, r)| (r - (fx - cx).hypot(fy - cy)).clamp(0.0, 1.5) / 1.5)
                .fold(0.0_f32, f32::max);
            if cl > 0.0 {
                blend(&mut c, [248.0, 252.0, 255.0], cl);
            }
            let i = (y * N + x) * 4;
            px[i..i + 4].copy_from_slice(&[
                c[0] as u8,
                c[1] as u8,
                c[2] as u8,
                (mask * 255.0) as u8,
            ]);
        }
    }
    window::icon::from_rgba(px, N as u32, N as u32).ok()
}

fn main() -> iced::Result {
    configure_display();
    #[cfg(target_os = "linux")]
    sctk_adwaita::preload();
    style::preload_fonts();
    icons::preload_cloud_tex();
    iced::application(
        app::WeatherApp::boot,
        app::WeatherApp::update,
        app::WeatherApp::view,
    )
    .title(app::WeatherApp::title)
    .theme(app::WeatherApp::theme)
    .subscription(app::WeatherApp::subscription)
    .default_font(style::fw(iced::font::Weight::Normal))
    .window(window::Settings {
        size: Size::new(1180.0, 760.0),
        min_size: Some(Size::new(720.0, 520.0)),
        exit_on_close_request: false,
        icon: build_app_icon(),
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "weather-rs".into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
}
