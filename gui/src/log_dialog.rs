//! Log window: recent runtime log lines, a live traffic chart and statistics.
//!
//! Reachable from the tray menu.  The window mirrors what ss-win shows: a
//! scrollback of the proxy's log plus a traffic graph and byte totals.

use crate::logging::{self, LogLine};
use crate::traffic::{self, Speed};
use crate::widgets;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, fill, point, px, size, App, Bounds, ClickEvent, Context, ElementId, FontWeight,
    ScrollHandle, SharedString, Window, WindowBounds, WindowOptions,
};
use rust_i18n::t;
use std::time::Duration;
use tracing::Level;

/// Height of the traffic chart.
const CHART_HEIGHT: f32 = 130.;
/// How often the window checks for new log lines and traffic samples.
const REFRESH: Duration = Duration::from_millis(400);
/// Colour of the upload series (download uses the system accent).
const UPLOAD_COLOR: u32 = 0x22c55e;

/// Open the log window as its own window.
pub fn open(cx: &mut App) {
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(720.), px(560.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&t!("log_dialog.title"));
                window.set_app_id("io.juicity.gui");
                let dialog = cx.new(LogDialog::new);
                cx.new(|cx| gpui_kit::base::Root::new(dialog, window, cx))
            },
        )
        .ok();

    if let Some(handle) = handle {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .ok();
    }
}

/// State of the log window.
struct LogDialog {
    /// Scroll position of the log list, so it can follow new lines.
    scroll: ScrollHandle,
    /// Whether the view sticks to the newest line.
    follow: bool,
    /// Last rendered log-buffer / traffic-monitor versions.
    log_version: u64,
    traffic_version: u64,
}

impl LogDialog {
    fn new(cx: &mut Context<Self>) -> Self {
        // Re-render whenever new log lines or traffic samples arrive.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH).await;
            let updated = this.update(cx, |dialog, cx| {
                let mut dirty = false;
                let logs = logging::buffer();
                if logs.version() != dialog.log_version {
                    dialog.log_version = logs.version();
                    if dialog.follow {
                        dialog.scroll.scroll_to_bottom();
                    }
                    dirty = true;
                }
                let monitor = traffic::monitor();
                if monitor.version() != dialog.traffic_version {
                    dialog.traffic_version = monitor.version();
                    dirty = true;
                }
                if dirty {
                    cx.notify();
                }
            });
            if updated.is_err() {
                break;
            }
        })
        .detach();

        Self {
            scroll: ScrollHandle::new(),
            follow: true,
            log_version: logging::buffer().version(),
            traffic_version: traffic::monitor().version(),
        }
    }

    fn toggle_follow(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.follow = follow;
        if follow {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }
}

impl Render for LogDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = widgets::palette(cx);
        let snapshot = traffic::monitor().snapshot();
        let logs = logging::buffer();
        let lines = logs.snapshot();
        let dropped = logs.dropped();
        // The log area is monospaced, so columns line up and long lines can be
        // read (and scrolled) without wrapping.
        let mono_font = gpui_kit::component::Theme::global(cx).mono_font_family.clone();
        let this = cx.weak_entity();
        let peak = snapshot.peak_down.max(snapshot.peak_up);
        let samples = snapshot.samples.clone();
        let down_speed = traffic::format_speed(snapshot.current.down);
        let up_speed = traffic::format_speed(snapshot.current.up);
        let down_total = traffic::format_bytes(snapshot.total_down);
        let up_total = traffic::format_bytes(snapshot.total_up);

        let clear_button = btn(
            "log-clear",
            t!("log_dialog.clear").to_string(),
            {
                let this = this.clone();
                move |_e, _w, cx| {
                    logging::buffer().clear();
                    this.update(cx, |_dialog, cx| cx.notify()).ok();
                }
            },
        );
        let follow_toggle = chk(
            "log-follow",
            t!("log_dialog.follow").to_string(),
            self.follow,
            {
                let this = this.clone();
                move |checked, _w, cx| {
                    this.update(cx, |dialog, cx| dialog.toggle_follow(*checked, cx))
                        .ok();
                }
            },
        );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            // ── Statistics ────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_8()
                    .px_4()
                    .py_3()
                    .child(stat(
                        colors,
                        t!("log_dialog.download_speed").to_string(),
                        down_speed,
                        colors.accent,
                    ))
                    .child(stat(
                        colors,
                        t!("log_dialog.upload_speed").to_string(),
                        up_speed,
                        gpui_kit::rgb(UPLOAD_COLOR).into(),
                    ))
                    .child(stat(
                        colors,
                        t!("log_dialog.download_total").to_string(),
                        down_total,
                        colors.accent,
                    ))
                    .child(stat(
                        colors,
                        t!("log_dialog.upload_total").to_string(),
                        up_total,
                        gpui_kit::rgb(UPLOAD_COLOR).into(),
                    )),
            )
            // ── Traffic chart ─────────────────────────────────────────────
            .child(
                div()
                    .px_4()
                    .child(traffic_chart(colors, samples, peak)),
            )
            // ── Toolbar ───────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .child(clear_button)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child(if dropped > 0 {
                                        t!("log_dialog.dropped", n = dropped).to_string()
                                    } else {
                                        String::new()
                                    }),
                            ),
                    )
                    .child(follow_toggle),
            )
            .child(div().h(px(1.)).w_full().bg(colors.border))
            // ── Log list ──────────────────────────────────────────────────
            .child(
                div()
                    .relative()
                    .flex_grow(1.)
                    .min_h_0()
                    .child(
                        div()
                            .id("log-list")
                            .size_full()
                            .track_scroll(&self.scroll)
                            .overflow_scroll()
                            .font_family(mono_font)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w_auto()
                                    .min_w_full()
                                    .py_1()
                                    .children(lines.into_iter().map(|line| log_row(colors, line))),
                            ),
                    )
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::new(&self.scroll)
                                .id("log-scrollbar")
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                    ),
            )
    }
}

/// One labelled statistic with a coloured marker.
fn stat(
    colors: widgets::Palette,
    label: String,
    value: String,
    marker: gpui_kit::Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(marker))
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(label),
                ),
        )
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.foreground)
                .child(value),
        )
}

/// Download bars above the centre line, upload bars below it.
fn traffic_chart(colors: widgets::Palette, samples: Vec<Speed>, peak: f64) -> impl IntoElement {
    let down = colors.accent;
    let up = gpui_kit::rgb(UPLOAD_COLOR);
    let grid = colors.border;
    let scale = peak.max(1024.0);
    let has_samples = !samples.is_empty();

    div()
        .relative()
        .h(px(CHART_HEIGHT))
        .w_full()
        .border_1()
        .border_color(colors.border)
        .child(
            canvas(
                move |_bounds, _window, _cx| {},
                move |bounds, _state, window, _cx| {
                    let width = f32::from(bounds.size.width);
                    let height = f32::from(bounds.size.height);
                    let origin = bounds.origin;
                    let mid = height / 2.0;

                    // Centre line plus the two quarter lines.
                    for offset in [0.0, mid * 0.5, mid, mid * 1.5] {
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(origin.x, origin.y + px(offset)),
                                size: size(px(width), px(1.)),
                            },
                            grid,
                        ));
                    }

                    if samples.is_empty() {
                        return;
                    }

                    let slot = width / traffic::HISTORY as f32;
                    let bar = (slot * 0.72).max(1.0);
                    let offset = traffic::HISTORY.saturating_sub(samples.len());
                    for (i, sample) in samples.iter().enumerate() {
                        let x = origin.x + px((offset + i) as f32 * slot);
                        let down_h = ((sample.down / scale) * f64::from(mid)) as f32;
                        let up_h = ((sample.up / scale) * f64::from(mid)) as f32;
                        if down_h > 0.5 {
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(x, origin.y + px(mid - down_h)),
                                    size: size(px(bar), px(down_h)),
                                },
                                down,
                            ));
                        }
                        if up_h > 0.5 {
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(x, origin.y + px(mid)),
                                    size: size(px(bar), px(up_h)),
                                },
                                up,
                            ));
                        }
                    }
                },
            )
            .size_full(),
        )
        .when(!has_samples, |chart| {
            chart.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child(t!("log_dialog.no_traffic").to_string()),
            )
        })
}

/// One log line: time, level, message.
fn log_row(colors: widgets::Palette, line: LogLine) -> impl IntoElement {
    let level_color: gpui_kit::Hsla = match line.level {
        Level::ERROR => gpui_kit::rgb(0xef4444).into(),
        Level::WARN => gpui_kit::rgb(0xf59e0b).into(),
        Level::INFO => colors.muted_foreground,
        Level::DEBUG | Level::TRACE => colors.muted_foreground,
    };
    let level = match line.level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    };
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        .px_4()
        .py_0p5()
        .text_xs()
        .whitespace_nowrap()
        .child(
            div()
                .flex_none()
                .text_color(colors.muted_foreground)
                .child(line.time),
        )
        .child(
            div()
                .flex_none()
                .w(px(44.))
                .text_color(level_color)
                .child(level),
        )
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_color(colors.foreground)
                .child(line.message),
        )
}

/// Build a gpui-kit `Button`.
fn btn(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id).label(label).on_click(on_click)
}

/// Build a gpui-kit `Checkbox`.
fn chk(
    id: impl Into<ElementId>,
    label: impl Into<gpui_kit::component::text::Text>,
    checked: bool,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Checkbox {
    Checkbox::new(id).label(label).checked(checked).on_click(on_click)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    /// The window must build and render without panicking — this covers the
    /// monospace font lookup, the auto-sized log content and the scrollbar
    /// overlay.
    #[gpui_kit::test]
    fn window_renders_with_log_lines(cx: &mut TestAppContext) {
        logging::init("info");
        tracing::warn!(answer = 42, "a fairly long log line that should not wrap");

        // The window needs the component theme to be installed.
        cx.update(gpui_kit::init);

        cx.add_window(|window, cx| {
            let dialog = cx.new(LogDialog::new);
            gpui_kit::base::Root::new(dialog, window, cx)
        });
        cx.run_until_parked();

        assert!(!logging::buffer().snapshot().is_empty());
    }
}
