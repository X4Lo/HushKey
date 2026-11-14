//! The on-screen confirmation popup.
//!
//! It must never steal keyboard focus: a focus change would minimise a game running in
//! exclusive fullscreen or interrupt typing. So the winit window is created with
//! `active = false`, as a tool window (no taskbar entry), always-on-top and click-through.
//! Slint creates the native window when the component is constructed, so we flag "the
//! next window is a toast" and the global window-attributes hook (installed in `main`)
//! applies the toast-specific attributes. Window is created lazily and dropped after it
//! fades out, so it costs nothing while idle.
//!
//! Note: games using *exclusive* fullscreen (rare today; most use borderless) draw over
//! every other window, so the toast can't be seen there — the tray flash still happens.

use crate::Toast;
use slint::winit_030::{winit, WinitWindowAccessor};
use slint::ComponentHandle;
use std::cell::Cell;
use std::time::Duration;

thread_local! {
    static NEXT_WINDOW_IS_TOAST: Cell<bool> = const { Cell::new(false) };
}

const VISIBLE_FOR: Duration = Duration::from_millis(1400);
const FADE: Duration = Duration::from_millis(220);
const WIDTH: f64 = 300.0;
const HEIGHT: f64 = 76.0;
const BOTTOM_MARGIN: f64 = 110.0;

/// Window-attribute hook: applied to every winit window Slint creates.
pub fn window_attributes_hook(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    if !NEXT_WINDOW_IS_TOAST.with(|f| f.replace(false)) {
        return attrs;
    }
    let attrs = attrs
        .with_active(false)
        .with_decorations(false)
        .with_resizable(false)
        .with_window_level(winit::window::WindowLevel::AlwaysOnTop);
    #[cfg(target_os = "windows")]
    let attrs = {
        use winit::platform::windows::WindowAttributesExtWindows;
        attrs.with_skip_taskbar(true).with_no_redirection_bitmap(false)
    };
    attrs
}

pub struct ToastPresenter {
    toast: Option<Toast>,
    hide_timer: slint::Timer,
    drop_timer: slint::Timer,
}

impl ToastPresenter {
    pub fn new() -> Self {
        Self { toast: None, hide_timer: slint::Timer::default(), drop_timer: slint::Timer::default() }
    }

    pub fn show(&mut self, dark: bool, headline: &str, detail: &str, muted: bool, error: bool) {
        self.drop_timer.stop();
        if self.toast.is_none() {
            NEXT_WINDOW_IS_TOAST.with(|f| f.set(true));
            let created = Toast::new();
            NEXT_WINDOW_IS_TOAST.with(|f| f.set(false));
            match created {
                Ok(t) => self.toast = Some(t),
                Err(e) => {
                    log::warn!("toast window: {e}");
                    return;
                }
            }
        }
        let Some(t) = &self.toast else { return };
        t.set_dark(dark);
        t.set_headline(headline.into());
        t.set_detail(detail.into());
        t.set_muted(muted);
        t.set_error(error);
        if let Err(e) = t.show() {
            log::warn!("toast show: {e}");
            return;
        }
        t.window().with_winit_window(|w| {
            w.set_cursor_hittest(false).ok();
            position_bottom_center(w);
        });
        // Defer one frame so the fade-in animation runs from the hidden state.
        let weak = t.as_weak();
        slint::Timer::single_shot(Duration::from_millis(16), move || {
            if let Some(t) = weak.upgrade() {
                t.set_shown(true);
            }
        });
        self.hide_timer.start(slint::TimerMode::SingleShot, VISIBLE_FOR, || {
            crate::app::with_toast(|p| p.fade_out());
        });
    }

    fn fade_out(&mut self) {
        if let Some(t) = &self.toast {
            t.set_shown(false);
        }
        self.drop_timer.start(slint::TimerMode::SingleShot, FADE, || {
            crate::app::with_toast(|p| {
                if let Some(t) = p.toast.take() {
                    let _ = t.hide();
                }
            });
        });
    }
}

fn position_bottom_center(w: &winit::window::Window) {
    let Some(monitor) = w.current_monitor().or_else(|| w.primary_monitor()) else { return };
    let scale = monitor.scale_factor();
    let (mx, my) = (monitor.position().x as f64, monitor.position().y as f64);
    let (mw, mh) = (monitor.size().width as f64, monitor.size().height as f64);
    let x = mx + (mw - WIDTH * scale) / 2.0;
    let y = my + mh - (HEIGHT + BOTTOM_MARGIN) * scale;
    w.set_outer_position(winit::dpi::PhysicalPosition::new(x.round() as i32, y.round() as i32));
}
