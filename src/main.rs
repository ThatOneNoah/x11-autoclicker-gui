use std::{
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use eframe::egui;
use spin_sleep::SpinSleeper;

use x11::xlib::*;
use x11::xtest::*;

// ---------- Shared state ----------
#[derive(Clone)]
struct Settings {
    cps: f64,            // clicks per second (decimal)
    duty: f64,           // percent 0..100 (decimal)
    button_name: String, // "left" | "middle" | "right" | "1..9"
    hotkey: String,      // X11 keysym string, e.g., "F6", "F8", "q"
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cps: 24.32345237573,
            duty: 36.836218324712,
            button_name: "left".to_string(),
            hotkey: "F6".to_string(),
        }
    }
}

fn parse_button(name: &str) -> Result<u32> {
    let b = name.to_lowercase();
    let v = match b.as_str() {
        "left" => 1,
        "middle" => 2,
        "right" => 3,
        _ => b.parse::<u32>().context("button must be left|middle|right|1..9")?,
    };
    if !(1..=9).contains(&v) {
        bail!("button must be in 1..=9");
    }
    Ok(v)
}

fn keysym_to_keycode(display: *mut Display, name: &str) -> Result<u32> {
    let c = std::ffi::CString::new(name)?;
    unsafe {
        let ks = XStringToKeysym(c.as_ptr());
        if ks == 0 {
            bail!("Unknown hotkey keysym '{}'", name);
        }
        let kc = XKeysymToKeycode(display, ks);
        if kc == 0 {
            bail!("No keycode for keysym '{}'", name);
        }
        Ok(kc as u32)
    }
}

// Mod combinations to handle NumLock/CapsLock variations
const MOD_VARIANTS: [u32; 8] = [
    0,
    LockMask,
    Mod2Mask,
    Mod5Mask,
    LockMask | Mod2Mask,
    LockMask | Mod5Mask,
    Mod2Mask | Mod5Mask,
    LockMask | Mod2Mask | Mod5Mask,
];

// ---------- Hotkey thread ----------
fn hotkey_thread(
    running: Arc<AtomicBool>,
    should_exit: Arc<AtomicBool>,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    unsafe { XInitThreads() };
    unsafe {
        let dpy = XOpenDisplay(ptr::null());
        if dpy.is_null() {
            bail!("Hotkey thread: failed to open X display (X11 only)");
        }
        let screen = XDefaultScreen(dpy);
        let root = XRootWindow(dpy, screen);
        XSelectInput(dpy, root, KeyPressMask);

        // Initial grab + last_keycode
        let s0 = settings.lock().unwrap().clone();
        let mut last_keycode = keysym_to_keycode(dpy, &s0.hotkey)?;
        for m in MOD_VARIANTS {
            XGrabKey(dpy, last_keycode as i32, m, root, True, GrabModeAsync, GrabModeAsync);
        }
        XFlush(dpy);

        // Event loop
        let mut event: XEvent = std::mem::zeroed();

        while !should_exit.load(Ordering::SeqCst) {
            // Re-grab if hotkey changed
            if let Some(nk) = {
                let s = settings.lock().unwrap().clone();
                keysym_to_keycode(dpy, &s.hotkey).ok()
            } {
                if nk != last_keycode {
                    for m in MOD_VARIANTS {
                        XUngrabKey(dpy, last_keycode as i32, m, root);
                    }
                    for m in MOD_VARIANTS {
                        XGrabKey(dpy, nk as i32, m, root, True, GrabModeAsync, GrabModeAsync);
                    }
                    last_keycode = nk;
                    XFlush(dpy);
                    eprintln!("[hotkey] rebound");
                }
            }

            // Handle events
            if XPending(dpy) > 0 {
                XNextEvent(dpy, &mut event);
                if event.get_type() == KeyPress {
                    let xkey: XKeyEvent = event.key;
                    if xkey.keycode as u32 == last_keycode {
                        let new_state = !running.load(Ordering::SeqCst);
                        running.store(new_state, Ordering::SeqCst);
                        eprintln!("[hotkey] {}", if new_state { "START" } else { "STOP" });
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        // Cleanup
        for m in MOD_VARIANTS {
            XUngrabKey(dpy, last_keycode as i32, m, root);
        }
        XFlush(dpy);
        XCloseDisplay(dpy);
    }
    Ok(())
}

// ---------- Click thread ----------
fn click_thread(
    running: Arc<AtomicBool>,
    should_exit: Arc<AtomicBool>,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    unsafe { XInitThreads() };
    unsafe {
        let dpy = XOpenDisplay(ptr::null());
        if dpy.is_null() {
            bail!("Click thread: failed to open X display (X11 only)");
        }

        // High-resolution sleep without explicit SpinStrategy variant
        let sleeper = SpinSleeper::new(1_000_000);

        // Ensure button is released on exit
        let mut last_button: u32 = 1;

        while !should_exit.load(Ordering::SeqCst) {
            if running.load(Ordering::SeqCst) {
                // Snapshot settings
                let s = settings.lock().unwrap().clone();

                let cps = if s.cps > 0.0 { s.cps } else { 0.1 };
                let duty = (s.duty / 100.0).clamp(0.0, 1.0);
                let button = parse_button(&s.button_name).unwrap_or(1);
                last_button = button;

                let period = 1.0 / cps;
                let min_press = 0.001_f64; // 1 ms
                let on_time = (period * duty).max(min_press).min(period);
                let off_time = (period - on_time).max(0.0);

                // Press
                XTestFakeButtonEvent(dpy, button, True, CurrentTime);
                XFlush(dpy);
                sleeper.sleep(Duration::from_secs_f64(on_time));

                // Release
                XTestFakeButtonEvent(dpy, button, False, CurrentTime);
                XFlush(dpy);

                // Idle
                if off_time > 0.0 {
                    sleeper.sleep(Duration::from_secs_f64(off_time));
                }
            } else {
                sleeper.sleep(Duration::from_millis(5));
            }
        }

        // Safety: ensure released
        XTestFakeButtonEvent(dpy, last_button, False, CurrentTime);
        XFlush(dpy);
        XCloseDisplay(dpy);
    }
    Ok(())
}

// ---------- GUI app ----------
struct GuiApp {
    settings: Arc<Mutex<Settings>>,
    running: Arc<AtomicBool>,
    should_exit: Arc<AtomicBool>,
    last_err: Option<String>,
}

impl GuiApp {
    fn new() -> Self {
        Self {
            settings: Arc::new(Mutex::new(Settings::default())),
            running: Arc::new(AtomicBool::new(false)),
            should_exit: Arc::new(AtomicBool::new(false)),
            last_err: None,
        }
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        self.should_exit.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("X11 Autoclicker (decimal CPS & duty)");

            // Editable controls
            {
                let mut s = self.settings.lock().unwrap();

                ui.horizontal(|ui| {
                    ui.label("Clicks per second:");
                    let drag = egui::DragValue::new(&mut s.cps)
                        .speed(0.1)
                        .clamp_range(0.001..=10000.0)
                        .fixed_decimals(12);
                    ui.add(drag);
                });

                ui.horizontal(|ui| {
                    ui.label("Duty cycle (%):");
                    let drag = egui::DragValue::new(&mut s.duty)
                        .speed(0.1)
                        .clamp_range(0.0..=100.0)
                        .fixed_decimals(12);
                    ui.add(drag);
                });

                ui.horizontal(|ui| {
                    ui.label("Mouse button:");
                    egui::ComboBox::from_id_source("btn_combo")
                        .selected_text(s.button_name.clone())
                        .show_ui(ui, |ui| {
                            for b in ["left", "middle", "right"] {
                                ui.selectable_value(&mut s.button_name, b.to_string(), b);
                            }
                            for n in 4..=9 {
                                let t = n.to_string();
                                ui.selectable_value(&mut s.button_name, t.clone(), &t);
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("Toggle hotkey (X11 keysym):");
                    ui.text_edit_singleline(&mut s.hotkey);
                });
            }

            ui.separator();

            // Live timing
            let (cps, duty) = {
                let s = self.settings.lock().unwrap().clone();
                (s.cps, s.duty)
            };
            if cps > 0.0 {
                let period_ms = 1000.0 / cps;
                let on_ms = (period_ms * (duty / 100.0)).max(1.0_f64.min(period_ms));
                let off_ms = (period_ms - on_ms).max(0.0);
                ui.label(format!(
                    "Period: {:.6} ms   |   Press (on): {:.6} ms   |   Release (off): {:.6} ms",
                    period_ms, on_ms, off_ms
                ));
            }

            ui.separator();

            // Start / Stop
            ui.horizontal(|ui| {
                let running = self.running.load(Ordering::SeqCst);
                if !running {
                    if ui.button("▶ Start (or press hotkey)").clicked() {
                        self.running.store(true, Ordering::SeqCst);
                    }
                } else {
                    if ui.button("⏹ Stop").clicked() {
                        self.running.store(false, Ordering::SeqCst);
                    }
                }
                ui.label(if running { "Status: RUNNING" } else { "Status: idle" });
            });

            if let Some(err) = &self.last_err {
                ui.colored_label(egui::Color32::RED, format!("Error: {err}"));
            }

            ui.separator();
            ui.small("Tip: Works on X11 only. Hover over the target window and press the hotkey (default F6) to toggle.");
        });

        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

fn main() -> Result<()> {
    // Create app + spawn threads
    let app = GuiApp::new();

    {
        let running = app.running.clone();
        let should_exit = app.should_exit.clone();
        let settings = app.settings.clone();
        thread::spawn(move || {
            if let Err(e) = hotkey_thread(running, should_exit, settings) {
                eprintln!("hotkey thread error: {e}");
            }
        });
    }
    {
        let running = app.running.clone();
        let should_exit = app.should_exit.clone();
        let settings = app.settings.clone();
        thread::spawn(move || {
            if let Err(e) = click_thread(running, should_exit, settings) {
                eprintln!("click thread error: {e}");
            }
        });
    }

    // Launch window (eframe 0.27)
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(520.0, 260.0)),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "X11 Autoclicker (GUI)",
        options,
        Box::new(|_| Box::new(app)),
    ) {
        eprintln!("GUI error: {e}");
    }
    Ok(())
}
