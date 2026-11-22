//! The device summary card: one full-width layer between the device title and the graph. It ties
//! the sidebar row to the graph with what the selected device is doing right now: its state, the
//! level of its output channels, a master fader with mute, its delay and what is wired to it.
//! Everything comes from the config, `EngineStatus` and the meters; the card only emits `Edit`s.
use crate::graph::text_left;
use crate::health;
use crate::layout::SIDE;
use crate::meter::{PeakHold, level};
use crate::model::{
    Edit, MIN_DB, apps_readout, db_to_pct, delay_readout, delay_readout_tip, pct_to_db,
};
use crate::theme::{Tokens, body, label, num, radius};
use crate::widgets::{chevron, fader, halo, hit as hit_target, meter, speaker};
use eframe::egui::{
    self, Align2, CornerRadius, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, WidgetInfo,
    accesskit::Role, pos2, text::LayoutJob, text::TextFormat, vec2,
};
use sr_core::backend::{DeviceState, MeterReading};
use sr_core::config::VirtualDevice;
use std::collections::HashMap;
use std::time::Duration;

/// Card height with both rows, and as a one-line strip.
const FULL_H: f32 = 76.0;
const STRIP_H: f32 = 40.0;
/// Distance from the title row to the card, and from the card to the graph.
const TOP_GAP: f32 = 48.0;
const BOTTOM_GAP: f32 = 6.0;
/// Output meters: at most this many channel pairs, each between these widths.
const MAX_PAIRS: usize = 4;
const PAIR_MIN_W: f32 = 44.0;
const PAIR_MAX_W: f32 = 200.0;
const PAIR_GAP: f32 = 10.0;
/// Room for the "+N" after the last pair shown.
const MORE_W: f32 = 26.0;
/// Master fader, dB readout and the space they keep to the card's right edge.
const FADER_W: f32 = 120.0;
const FADER_MIN_W: f32 = 72.0;
const READOUT_W: f32 = 62.0;
const PAD: f32 = 16.0;
/// Left edge of the chip and of the second row, from the card's left edge.
const TEXT_X: f32 = 42.0;

/// What belongs to the card between frames; per session only, nothing is saved.
#[derive(Default)]
pub struct State {
    /// Shown as the one-line strip.
    pub collapsed: bool,
    /// Peak-hold markers by output channel, for the device named in `dev`.
    peaks: HashMap<usize, PeakHold>,
    dev: String,
}

/// What the card reads about the selected device.
pub struct Facts<'a> {
    pub idx: usize,
    pub dev: &'a VirtualDevice,
    pub state: &'a DeviceState,
    /// The engine's reading of the main path and of captured apps, in ms.
    pub delay_ms: Option<f64>,
    pub apps_ms: Option<f64>,
    pub reading: Option<&'a MeterReading>,
    /// Dropout count while it grew within the last few seconds.
    pub dropouts: Option<u64>,
    /// The synthetic engine is in use (the chip says so, whatever it reports).
    pub demo: bool,
}

/// Where the card sits in the main pane `full`.
pub fn rect(full: Rect, collapsed: bool) -> Rect {
    let h = if collapsed { STRIP_H } else { FULL_H };
    Rect::from_min_size(
        pos2(full.left() + SIDE, full.top() + TOP_GAP),
        vec2(full.width() - 2.0 * SIDE, h),
    )
}

/// Top of the graph: just under the card.
pub fn graph_top(card: Rect) -> f32 {
    card.bottom() + BOTTOM_GAP
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Quiet,
    /// Demo mode: neutral, not a state of real audio.
    Demo,
}

/// The status chip: the words always say it, the colour only agrees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub text: &'static str,
    pub tone: Tone,
    pub tip: String,
}

pub const DEMO_TIP: &str = "Demo mode: audio is synthetic. Nothing here touches your real audio.";

pub fn chip(enabled: bool, state: &DeviceState, demo: bool) -> Chip {
    let (text, tone, tip) = match (enabled, state) {
        (false, _) => (
            "Off",
            Tone::Quiet,
            "Switched off in the sidebar.".to_owned(),
        ),
        (true, _) if demo => ("Demo", Tone::Demo, DEMO_TIP.to_owned()),
        (true, DeviceState::Running) => ("Running", Tone::Ok, "Audio is being routed.".to_owned()),
        (true, DeviceState::Starting) => ("Starting", Tone::Warn, "Starting up.".to_owned()),
        (true, DeviceState::WaitingForPermission) => (
            "Waiting for permission",
            Tone::Warn,
            "Allow System Audio Recording in System Settings.".to_owned(),
        ),
        (true, DeviceState::Failed(e)) => ("Error", Tone::Bad, format!("Failed: {e}")),
        (true, DeviceState::Idle) => (
            "Not connected",
            Tone::Quiet,
            "The audio engine is not running this device.".to_owned(),
        ),
    };
    Chip { text, tone, tip }
}

/// The small "DEMO" pill beside the window title, so synthetic audio is never taken for real.
pub fn demo_pill(ui: &mut Ui, t: &Tokens, left_center: Pos2) {
    let (signal, ink) = tone_colors(Tone::Demo, t);
    let p = ui.painter();
    let g = p.layout_no_wrap("DEMO".to_owned(), label(), ink);
    let pill = Rect::from_min_size(
        pos2(left_center.x, left_center.y - 10.0),
        vec2(g.size().x + 16.0, 20.0),
    );
    p.rect(
        pill,
        CornerRadius::same(radius::PILL),
        signal.gamma_multiply(0.14),
        Stroke::new(1.0, signal.gamma_multiply(0.4)),
        StrokeKind::Inside,
    );
    p.galley(
        pos2(pill.left() + 8.0, pill.center().y - g.size().y / 2.0),
        g,
        ink,
    );
    let hit = ui.interact(pill, Id::new("demo_pill"), Sense::hover());
    hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, "Demo mode"));
    hit.on_hover_text(DEMO_TIP);
}

/// How many channel pairs the meter shows in `room` px, and how many are left out ("+N").
pub fn visible_pairs(total: usize, room: f32) -> (usize, usize) {
    let fit = |room: f32| {
        (((room + PAIR_GAP) / (PAIR_MIN_W + PAIR_GAP))
            .floor()
            .max(1.0) as usize)
            .min(MAX_PAIRS)
    };
    if total <= MAX_PAIRS && fit(room) >= total {
        return (total, 0);
    }
    let shown = fit(room - MORE_W).min(total);
    (shown, total - shown)
}

/// "-6.0 dB"; the bottom of the fader is "-inf dB".
pub fn db_readout(db: f32) -> String {
    if db <= MIN_DB {
        "-inf dB".into()
    } else if db.abs() < 0.05 {
        "0.0 dB".into()
    } else {
        format!("{db:.1} dB")
    }
}

fn noun(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// "2 sources, 2 outputs, 1 monitor" as (count, word) parts.
pub fn counts(dev: &VirtualDevice) -> [(usize, &'static str); 3] {
    let (s, o, m) = (
        dev.sources.len(),
        usize::from(dev.channels),
        dev.monitors.len(),
    );
    [
        (s, noun(s, "source", "sources")),
        (o, noun(o, "output", "outputs")),
        (m, noun(m, "monitor", "monitors")),
    ]
}

fn pair_title(p: usize) -> String {
    format!("Channels {} & {}", p * 2 + 1, p * 2 + 2)
}

fn tone_colors(tone: Tone, t: &Tokens) -> (egui::Color32, egui::Color32) {
    match tone {
        Tone::Ok => (t.ok, t.ok_ink),
        Tone::Warn => (t.warn, t.warn_ink),
        Tone::Bad => (t.bad, t.bad_ink),
        Tone::Quiet => (t.muted, t.muted),
        Tone::Demo => (t.channels[7], t.ink), // violet: no state colour, so never read as one
    }
}

pub fn show(ui: &mut Ui, card: Rect, t: &Tokens, st: &mut State, f: &Facts, edits: &mut Vec<Edit>) {
    let dev = f.dev;
    if st.dev != dev.id {
        st.dev.clone_from(&dev.id);
        st.peaks.clear();
    }
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    let p = ui.painter().clone();
    p.rect(
        card,
        CornerRadius::same(radius::STRIP),
        t.raised,
        Stroke::new(1.0, t.hairline),
        StrokeKind::Inside,
    );
    let cy = if st.collapsed {
        card.center().y
    } else {
        card.top() + 24.0
    };

    // Chevron: full card or one-line strip.
    let hit = Rect::from_center_size(pos2(card.left() + 22.0, cy), vec2(28.0, 28.0));
    let toggle = hit_target(ui, hit, Id::unique("summary_toggle"));
    halo(ui, &toggle, t);
    let name = if st.collapsed {
        "Expand device summary"
    } else {
        "Collapse device summary"
    };
    toggle.widget_info(|| WidgetInfo::labeled(Role::Button, true, name));
    if toggle.has_focus() {
        p.rect_stroke(
            hit,
            CornerRadius::same(8),
            Stroke::new(2.0, t.focus),
            StrokeKind::Outside,
        );
    }
    chevron(
        &p,
        hit.center(),
        !st.collapsed,
        if toggle.hovered() { t.ink } else { t.muted },
    );
    if toggle.clicked() {
        st.collapsed = !st.collapsed;
    }
    toggle.on_hover_text(name);

    // Status chip.
    let chip = chip(dev.enabled, f.state, f.demo);
    let (signal, ink) = tone_colors(chip.tone, t);
    let g = p.layout_no_wrap(chip.text.to_owned(), label(), ink);
    let pill = Rect::from_min_size(
        pos2(card.left() + TEXT_X, cy - 12.0),
        vec2(12.0 + 8.0 + 6.0 + g.size().x + 12.0, 24.0),
    );
    let (fill, edge) = match chip.tone {
        Tone::Quiet => (t.canvas, t.hairline),
        _ => (signal.gamma_multiply(0.14), signal.gamma_multiply(0.4)),
    };
    p.rect(
        pill,
        CornerRadius::same(radius::PILL),
        fill,
        Stroke::new(1.0, edge),
        StrokeKind::Inside,
    );
    let dot = pos2(pill.left() + 12.0 + 4.0, cy);
    if chip.tone == Tone::Quiet {
        p.circle_stroke(dot, 3.5, Stroke::new(1.4, signal));
    } else {
        p.circle_filled(dot, 4.0, signal);
    }
    p.galley(pos2(dot.x + 4.0 + 6.0, cy - g.size().y / 2.0), g, ink);
    let chip_hit = ui.interact(pill, Id::unique("summary_state"), Sense::hover());
    let chip_label = format!("Device state: {}", chip.text);
    chip_hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, &chip_label));
    chip_hit.on_hover_text(&chip.tip);
    let mut left = pill.right();
    if let Some(n) = f.dropouts {
        let at = pos2(left + 16.0, cy);
        health::dropout_dot(
            ui,
            at,
            t.raised,
            t,
            &format!("summary:{}", dev.id),
            "This device",
            n,
        );
        left = at.x + 8.0;
    }

    // Master fader, readout and mute, from the right edge inwards.
    let right = card.right() - PAD;
    let readout = pos2(right, cy);
    let muted = dev.muted;
    p.text(
        readout,
        Align2::RIGHT_CENTER,
        db_readout(dev.volume_db),
        num(12.0),
        if muted { t.muted } else { t.ink },
    );
    let fader_right = right - READOUT_W - 8.0;
    let meters_from = left + 20.0;
    let mut fader_w = FADER_W;
    // Meters shrink last: the fader gives way first, down to a stub.
    let need = PAIR_MIN_W + 16.0 + 22.0 + 8.0;
    let room = fader_right - fader_w - meters_from;
    if room < need {
        fader_w = (fader_w - (need - room)).max(FADER_MIN_W);
    }
    let slider_rect = Rect::from_x_y_ranges(
        (fader_right - fader_w)..=fader_right,
        (cy - 7.5)..=(cy + 7.5),
    );
    let mut pct = db_to_pct(dev.volume_db).clamp(0.0, 100.0);
    let before = pct;
    let slider = fader(
        ui,
        slider_rect,
        Id::unique("summary_fader"),
        "Master volume",
        &mut pct,
        t,
    );
    slider.on_hover_text("Master volume");
    if (pct - before).abs() > 1e-3 {
        edits.push(Edit::Volume(f.idx, pct_to_db(pct)));
    }
    let mute_at = pos2(slider_rect.left() - 8.0 - 11.0, cy);
    let mute = speaker(
        ui,
        mute_at,
        Id::unique("summary_mute"),
        muted,
        t,
        "Mute device",
    );
    if mute.clicked() {
        edits.push(Edit::Mute(f.idx, !muted));
    }
    mute.on_hover_text(if muted {
        "Unmute device"
    } else {
        "Mute device"
    });

    // Output level: one stereo ladder per channel pair.
    let room = (mute_at.x - 11.0 - 16.0) - meters_from;
    let total = usize::from(dev.channels / 2).max(1);
    let (shown, more) = visible_pairs(total, room);
    let pair_w = ((room - if more > 0 { MORE_W } else { 0.0 } - PAIR_GAP * (shown as f32 - 1.0))
        / shown as f32)
        .clamp(PAIR_MIN_W, PAIR_MAX_W);
    let lit = dev.enabled;
    for pair in 0..shown {
        let x = meters_from + pair as f32 * (pair_w + PAIR_GAP);
        for side in 0..2 {
            let ch = pair * 2 + side;
            let lin = f
                .reading
                .filter(|_| lit)
                .and_then(|r| r.channels.get(ch))
                .copied()
                .unwrap_or(0.0);
            let lv = level(lin);
            let pk = st.peaks.entry(ch).or_default().update(lv, dt);
            if pk > lv {
                // The peak marker is still holding or falling, and no meter change will wake the UI.
                ui.request_repaint_after(Duration::from_millis(33));
            }
            let y = cy - 7.0 + side as f32 * 8.0;
            meter(
                &p,
                Rect::from_x_y_ranges(x..=(x + pair_w), y..=(y + 6.0)),
                lv,
                pk,
                t,
                1.0,
            );
        }
        let area = Rect::from_x_y_ranges(x..=(x + pair_w), (cy - 9.0)..=(cy + 9.0));
        let tip = ui.interact(area, Id::unique(("summary_pair", pair)), Sense::hover());
        let title = pair_title(pair);
        let name = format!("Output level, {}", title.to_lowercase());
        tip.widget_info(|| WidgetInfo::labeled(Role::Label, true, &name));
        tip.on_hover_text(title);
    }
    if more > 0 {
        let at = pos2(meters_from + shown as f32 * (pair_w + PAIR_GAP) - 4.0, cy);
        let r = p.text(
            at,
            Align2::LEFT_CENTER,
            format!("+{more}"),
            num(11.0),
            t.muted,
        );
        let tip = ui.interact(r.expand(4.0), Id::unique("summary_more"), Sense::hover());
        let name = format!(
            "{more} more output channel pair{}",
            if more == 1 { "" } else { "s" }
        );
        tip.widget_info(|| WidgetInfo::labeled(Role::Label, true, &name));
        tip.on_hover_text(name);
    }

    if st.collapsed {
        return;
    }

    // Second row: what the engine measured, and what the device is made of.
    let cy2 = card.top() + 54.0;
    let mut job = LayoutJob::default();
    let nums = TextFormat::simple(num(12.0), t.ink);
    let words = TextFormat::simple(body(), t.muted);
    let list = counts(dev);
    for (i, (n, w)) in list.iter().enumerate() {
        if i > 0 {
            job.append(", ", 0.0, words.clone());
        }
        job.append(&n.to_string(), 0.0, nums.clone());
        job.append(&format!(" {w}"), 0.0, words.clone());
    }
    let summary = list
        .iter()
        .map(|(n, w)| format!("{n} {w}"))
        .collect::<Vec<_>>()
        .join(", ");
    let g = p.layout_job(job);
    let at = Rect::from_min_size(pos2(right - g.size().x, cy2 - g.size().y / 2.0), g.size());
    p.galley(at.min, g, t.muted);
    let sum_hit = ui.interact(at.expand(2.0), Id::unique("summary_counts"), Sense::hover());
    sum_hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, &summary));

    let x = card.left() + TEXT_X;
    let max_w = (at.left() - 16.0 - x).max(0.0);
    let (text, tip) = match f.state {
        DeviceState::Failed(e) if dev.enabled => (e.clone(), e.clone()),
        _ => match f.delay_ms {
            Some(ms) => {
                let mut text = delay_readout(ms);
                if let Some(a) = f.apps_ms {
                    text = format!("{text}  {}", apps_readout(a));
                }
                (text, delay_readout_tip(f.apps_ms))
            }
            None => (
                "No delay reading yet".to_owned(),
                "Shown once audio runs through this device.".to_owned(),
            ),
        },
    };
    let failed = matches!(f.state, DeviceState::Failed(_)) && dev.enabled;
    let (font, color) = match (failed, f.delay_ms) {
        (true, _) => (body(), t.bad_ink),
        (false, Some(_)) => (num(12.0), t.ink),
        (false, None) => (body(), t.muted),
    };
    text_left(ui, x, cy2, &text, font, color, max_w);
    let hit = ui.interact(
        Rect::from_x_y_ranges(x..=(x + max_w), (cy2 - 10.0)..=(cy2 + 10.0)),
        Id::unique("summary_delay"),
        Sense::hover(),
    );
    hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, &text));
    hit.on_hover_text(tip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::demo_config;

    #[test]
    fn chip_says_the_state_in_words() {
        let on = |s: &DeviceState| chip(true, s, false);
        assert_eq!(
            (
                on(&DeviceState::Running).text,
                on(&DeviceState::Running).tone
            ),
            ("Running", Tone::Ok)
        );
        assert_eq!(
            (
                on(&DeviceState::Starting).text,
                on(&DeviceState::Starting).tone
            ),
            ("Starting", Tone::Warn)
        );
        let w = on(&DeviceState::WaitingForPermission);
        assert_eq!((w.text, w.tone), ("Waiting for permission", Tone::Warn));
        let e = on(&DeviceState::Failed("no driver".into()));
        assert_eq!((e.text, e.tone), ("Error", Tone::Bad));
        assert_eq!(e.tip, "Failed: no driver");
        let idle = on(&DeviceState::Idle);
        assert_eq!((idle.text, idle.tone), ("Not connected", Tone::Quiet));
    }

    #[test]
    fn demo_chip_says_demo_whatever_the_engine_reports() {
        for s in [
            DeviceState::Idle,
            DeviceState::Running,
            DeviceState::Starting,
            DeviceState::Failed("x".into()),
        ] {
            let c = chip(true, &s, true);
            assert_eq!((c.text, c.tone), ("Demo", Tone::Demo));
            assert_eq!(c.tip, DEMO_TIP);
        }
        // a device that is switched off is still off
        assert_eq!(chip(false, &DeviceState::Idle, true).text, "Off");
        // outside demo mode Idle keeps its words
        assert_eq!(chip(true, &DeviceState::Idle, false).text, "Not connected");
    }

    #[test]
    fn a_device_that_is_off_is_off_whatever_the_engine_last_said() {
        for s in [
            DeviceState::Idle,
            DeviceState::Running,
            DeviceState::Starting,
            DeviceState::Failed("x".into()),
        ] {
            let c = chip(false, &s, false);
            assert_eq!((c.text, c.tone), ("Off", Tone::Quiet));
        }
    }

    #[test]
    fn one_to_four_pairs_fit_without_a_plus() {
        for n in 1..=MAX_PAIRS {
            let room = n as f32 * (PAIR_MIN_W + PAIR_GAP);
            assert_eq!(visible_pairs(n, room), (n, 0), "{n}");
        }
    }

    #[test]
    fn more_than_four_pairs_show_four_and_count_the_rest() {
        assert_eq!(visible_pairs(5, 1000.0), (4, 1));
        assert_eq!(visible_pairs(32, 1000.0), (4, 28));
    }

    #[test]
    fn a_narrow_room_drops_pairs_and_keeps_at_least_one() {
        // 2 pairs + gap need 98 px; 90 leaves one pair and a "+1".
        assert_eq!(visible_pairs(2, 90.0), (1, 1));
        assert_eq!(visible_pairs(3, 0.0), (1, 2));
        assert_eq!(visible_pairs(1, 0.0), (1, 0));
        // room for three pairs but not the "+N" beside them: three shown
        let room = 3.0 * PAIR_MIN_W + 2.0 * PAIR_GAP + MORE_W;
        assert_eq!(visible_pairs(4, room), (3, 1));
    }

    #[test]
    fn db_readout_reads_like_the_fader() {
        assert_eq!(db_readout(0.0), "0.0 dB");
        assert_eq!(db_readout(-0.01), "0.0 dB");
        assert_eq!(db_readout(-6.04), "-6.0 dB");
        assert_eq!(db_readout(MIN_DB), "-inf dB");
        assert_eq!(db_readout(-200.0), "-inf dB");
    }

    #[test]
    fn counts_agree_with_the_graph_columns() {
        let cfg = demo_config();
        let words = |d: &VirtualDevice| {
            counts(d)
                .iter()
                .map(|(n, w)| format!("{n} {w}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        assert_eq!(words(&cfg.devices[0]), "3 sources, 2 outputs, 1 monitor");
        assert_eq!(words(&cfg.devices[1]), "2 sources, 2 outputs, 0 monitors");
    }

    #[test]
    fn the_card_is_shorter_as_a_strip_and_the_graph_starts_below_it() {
        let full = Rect::from_min_size(pos2(250.0, 0.0), vec2(750.0, 700.0));
        let (a, b) = (rect(full, false), rect(full, true));
        assert!(b.height() < a.height());
        assert_eq!((a.left(), a.right()), (250.0 + SIDE, 1000.0 - SIDE));
        assert!(graph_top(a) > a.bottom() && graph_top(b) < graph_top(a));
    }
}
