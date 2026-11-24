//! What the engine says about its own health, as the window shows it: one standing banner above the
//! footer (silent captures, devices held by another app, Bluetooth in phone-call mode) and a
//! per-device dropout indicator. The decisions are plain functions over `EngineStatus`; only `banner`
//! draws.
use crate::setup;
use crate::theme::{Tokens, body, num, radius};
use crate::widgets::{name_button, xmark};
use eframe::egui::{
    self, CornerRadius, Id, Rect, Sense, Stroke, WidgetInfo, accesskit::Role, pos2, vec2,
};
use sr_core::backend::{EngineStatus, HogInfo, ReadAheadState, TccStatus};
use sr_core::config::{SourceKind, VirtualDevice};
use std::collections::{HashMap, HashSet};

/// A dropout counts as "now" for this long after the engine's counter last grew.
pub const RECENT_SECS: f64 = 10.0;
/// The banner's button for `Fix::Privacy`; it opens the Audio Recording pane of System Settings directly.
pub const PRIVACY: &str = "Open Privacy settings";
/// The banner's button for `Fix::Settings`.
pub const SETTINGS: &str = "Settings";
/// Banner height for one line of text; more lines make it taller.
const MIN_H: f32 = 44.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// Open the System Audio Recording pane (`setup::AUDIO_CAPTURE_SETTINGS`).
    Privacy,
    /// Open the Settings sheet.
    Settings,
}

impl Fix {
    pub fn label(self) -> &'static str {
        match self {
            Self::Privacy => PRIVACY,
            Self::Settings => SETTINGS,
        }
    }
}

/// One thing worth telling the user, with the key that dismissing it is remembered by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub key: String,
    pub text: String,
    pub fix: Option<Fix>,
}

/// "Safari", "Safari and Spotify", "Safari, Spotify and Zoom", "Safari, Spotify and 2 more".
fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [a] => (*a).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// Who a silent capture of `dev` is about: its app sources by name, else the device itself.
pub fn subject(dev: &VirtualDevice) -> String {
    let apps: Vec<&str> = dev
        .sources
        .iter()
        .filter_map(|s| match &s.kind {
            SourceKind::App { bundle_id } => Some(if s.name.is_empty() {
                bundle_id.as_str()
            } else {
                s.name.as_str()
            }),
            _ => None,
        })
        .collect();
    if apps.is_empty() {
        dev.name.clone()
    } else {
        join_names(&apps)
    }
}

fn hog_owner(h: &HogInfo) -> String {
    match (&h.process_name, h.pid) {
        (Some(n), _) => n.clone(),
        (None, Some(p)) => format!("another app (process {p})"),
        (None, None) => "another app".to_owned(),
    }
}

/// Everything that is wrong right now, most important first: a capture the system denied, then
/// captures that deliver silence, devices another app holds, Bluetooth in headset mode.
/// `subject_of` names a virtual device id in the first sentence; `app_source` says whether an enabled
/// device captures an app (a denied permission only matters then).
pub fn issues(
    status: &EngineStatus,
    app_source: bool,
    subject_of: impl Fn(&str) -> String,
) -> Vec<Issue> {
    let mut out = Vec::new();
    let denied = app_source && status.tcc == TccStatus::Denied;
    if denied {
        out.push(Issue {
            key: "capture-denied".into(),
            text: "App audio cannot be captured because System Audio Recording is off for SoundRouter."
                .into(),
            fix: Some(Fix::Privacy),
        });
    }
    // A denied permission is why a tap delivers zeros: one banner for the one cause.
    if !denied {
        for id in &status.tap_silent {
            out.push(Issue {
                key: format!("silent:{id}"),
                text: format!(
                    "Audio from {} looks silent. If you expect sound, check that SoundRouter may capture audio.",
                    subject_of(id)
                ),
                fix: Some(Fix::Privacy),
            });
        }
    }
    for h in &status.hogged {
        out.push(Issue {
            key: format!("hogged:{}", h.device_name),
            text: format!(
                "{} is in exclusive use by {}; apps playing to it cannot be routed.",
                h.device_name,
                hog_owner(h)
            ),
            fix: None,
        });
    }
    for name in &status.bluetooth_hfp {
        out.push(Issue {
            key: format!("hfp:{name}"),
            text: format!(
                "{name} switched to a phone-call mode (low quality) because a microphone is open on it."
            ),
            fix: None,
        });
    }
    out
}

/// Banners the user closed in this session; a closed banner stays closed until the app quits.
#[derive(Default)]
pub struct Dismissed(HashSet<String>);

impl Dismissed {
    pub fn dismiss(&mut self, key: &str) {
        self.0.insert(key.to_owned());
    }

    /// A notice that happens again shows again after it was closed.
    pub fn forget(&mut self, key: &str) {
        self.0.remove(key);
    }

    /// The banner to show, and how many more are waiting behind it.
    pub fn pick<'a>(&self, issues: &'a [Issue]) -> Option<(&'a Issue, usize)> {
        let mut open = issues.iter().filter(|i| !self.0.contains(&i.key));
        let first = open.next()?;
        Some((first, open.count()))
    }
}

/// When each device's dropout counter last grew, to tell "dropouts now" from "dropouts earlier".
#[derive(Default)]
pub struct Dropouts {
    /// Device id -> (count last seen, when it last grew).
    seen: HashMap<String, (u64, Option<f64>)>,
}

impl Dropouts {
    /// Feed the engine's counters. A device seen for the first time sets the baseline (the window may
    /// have been closed while they piled up); a counter that fell was reset by a rebuild, and one that fell
    /// to a nonzero count already dropped out again since.
    pub fn observe(&mut self, xruns: &[(String, u64)], now: f64) {
        self.seen.retain(|id, _| xruns.iter().any(|(x, _)| x == id));
        for (id, n) in xruns {
            match self.seen.get_mut(id) {
                Some((count, grew)) => {
                    if *n > *count || (*n < *count && *n > 0) {
                        *grew = Some(now);
                    }
                    *count = *n;
                }
                None => {
                    self.seen.insert(id.clone(), (*n, None));
                }
            }
        }
    }

    /// The device's dropout count when it grew within the last `RECENT_SECS`.
    pub fn growing(&self, id: &str, now: f64) -> Option<u64> {
        let (n, grew) = self.seen.get(id)?;
        grew.filter(|t| now - t < RECENT_SECS).map(|_| *n)
    }

    pub fn total(&self) -> u64 {
        self.seen.values().map(|(n, _)| n).sum()
    }

    pub fn any_growing(&self, now: f64) -> bool {
        self.seen.keys().any(|id| self.growing(id, now).is_some())
    }
}

/// Key of the banner that says read-ahead turned itself off.
pub const TRIP_KEY: &str = "read-ahead-trip";

/// "Lower delay turned itself off after 3 driver dropouts." Ranked below every engine problem:
/// nothing is wrong now, a feature just stopped. The caller appends it to `issues`.
pub fn trip_issue(dropouts: u64) -> Issue {
    Issue {
        key: TRIP_KEY.into(),
        text: format!(
            "Lower delay turned itself off after {dropouts} driver dropout{}.",
            if dropouts == 1 { "" } else { "s" }
        ),
        fix: Some(Fix::Settings),
    }
}

/// The safety net of "Lower delay" (read-ahead): while the driver reports it On, any growth of a
/// device's driver overload counter or xrun counter between two status polls means the loop ran dry or
/// late, and it must be switched off. The overload counter also counts late writes, so a false alarm
/// is possible; the price is an experimental feature turning itself off, which is acceptable.
/// No clock: every decision compares two polls, so time never enters.
#[derive(Default)]
pub struct ReadAheadGuard {
    overloads: HashMap<String, u64>,
    xruns: HashMap<String, u64>,
    /// Growth seen since read-ahead turned on.
    overload_total: u64,
    xrun_total: u64,
    /// Already fired for this run of "On"; the next poll may still report On.
    fired: bool,
    /// The engine's own flag was seen clear since the user asked for read-ahead, so a flag still set
    /// from an earlier run is not mistaken for a new trip.
    engine_quiet: bool,
}

/// Sum of the increases of `now` over `seen`, which then becomes `now`. A device seen for the first
/// time sets its baseline; a counter that fell was reset by a rebuild and starts over.
fn growth(seen: &mut HashMap<String, u64>, now: &[(String, u64)]) -> u64 {
    seen.retain(|id, _| now.iter().any(|(n, _)| n == id));
    let mut grew = 0;
    for (id, n) in now {
        if let Some(before) = seen.insert(id.clone(), *n) {
            grew += n.saturating_sub(before);
        }
    }
    grew
}

impl ReadAheadGuard {
    /// Feed one status poll. `wanted` is whether the user's settings ask for read-ahead. Returns the
    /// number of dropouts to report when it must be turned off now, once per run of "On".
    pub fn observe(&mut self, status: &EngineStatus, wanted: bool) -> Option<u64> {
        if !wanted {
            self.engine_quiet = false;
        } else if !status.read_ahead_tripped {
            self.engine_quiet = true;
        }
        // The engine has a net of its own; when it fired first, the window follows it.
        let engine_tripped = wanted && self.engine_quiet && status.read_ahead_tripped;
        // Not wanted: the driver may still say On for a poll or two while it is being turned off.
        if !wanted || status.read_ahead != ReadAheadState::On {
            let n = self.since_on().max(1);
            *self = Self {
                engine_quiet: self.engine_quiet,
                ..Self::default()
            };
            return engine_tripped.then_some(n);
        }
        if self.fired {
            return None;
        }
        let overloads = growth(&mut self.overloads, &status.driver_overloads);
        let xruns = growth(&mut self.xruns, &status.xruns);
        self.overload_total += overloads;
        self.xrun_total += xruns;
        if overloads + xruns > 0 || engine_tripped {
            self.fired = true;
            return Some(self.since_on().max(1));
        }
        None
    }

    /// Driver overloads counted since read-ahead turned on ("Driver dropouts since on").
    pub fn overloads_since_on(&self) -> u64 {
        self.overload_total
    }

    fn since_on(&self) -> u64 {
        self.overload_total + self.xrun_total
    }

    /// A guard that has counted `n` overloads, for snapshots of the sheet's "since on" line.
    #[cfg(test)]
    pub fn with_overloads(n: u64) -> Self {
        Self {
            overload_total: n,
            ..Self::default()
        }
    }
}

/// "3 dropouts since start" for the dot's tooltip.
pub fn dropout_tip(n: u64) -> String {
    format!("{n} dropout{} since start", if n == 1 { "" } else { "s" })
}

/// The "Audio health" line of the delay menu, and the hint under it while dropouts keep growing.
/// `larger_exists` is false at the largest delay, where "choose a larger delay" would be wrong.
pub fn health_line(
    total: u64,
    growing: bool,
    larger_exists: bool,
) -> (String, Option<&'static str>) {
    let line = match total {
        0 => "Audio health: no dropouts since start".to_owned(),
        n => format!("Audio health: {}", dropout_tip(n)),
    };
    let hint = match (growing, larger_exists) {
        (false, _) => None,
        (true, true) => Some("Dropouts: choose a larger delay"),
        (true, false) => Some("Dropouts: close busy apps or turn off devices you do not use"),
    };
    (line, hint)
}

/// What a click on the banner asked for.
#[derive(Default)]
pub struct Clicked {
    pub dismiss: bool,
    pub fix: Option<Fix>,
}

/// The banner row, to be shown inside a panel above the footer: a status dot, the sentence, an
/// optional action, a "+N" badge when more are waiting, and a close button. Wraps to more lines
/// rather than clipping, so it never hides under the buttons at the minimum window width.
pub fn banner(ui: &mut egui::Ui, t: &Tokens, issue: &Issue, more: usize) -> Clicked {
    let mut out = Clicked::default();
    let full = ui.available_width();
    const PAD: f32 = 18.0;
    const TEXT_X: f32 = PAD + 20.0;
    const BTN_H: f32 = 26.0;
    // Right edge, built from the right: close, "+N" badge, action.
    let mut right = full - PAD;
    let close_w = 24.0;
    right -= close_w;
    let badge = (more > 0).then(|| {
        let text = format!("+{more}");
        let g = ui.painter().layout_no_wrap(text, num(11.0), t.muted);
        (g.size().x + 16.0, g)
    });
    if let Some((w, _)) = &badge {
        right -= w + 8.0;
    }
    let fix = issue.fix.map(|f| {
        let w = setup::footer_width(ui, f.label());
        right -= w + 8.0;
        (f, w)
    });
    let wrap = (right - TEXT_X - 8.0).max(120.0);
    let galley = ui.painter().layout(issue.text.clone(), body(), t.ink, wrap);
    let h = (galley.size().y + 22.0).max(MIN_H);
    let (r, _) = ui.allocate_exact_size(vec2(full, h), Sense::hover());
    let x = |dx: f32| r.left() + dx;
    let p = ui.painter();
    p.hline(
        r.x_range(),
        r.top() + 0.5,
        Stroke::new(1.0, t.hairline.gamma_multiply(0.6)),
    );
    p.circle_filled(pos2(x(PAD + 5.0), r.center().y), 5.0, t.warn);
    let text_rect = Rect::from_min_size(
        pos2(x(TEXT_X), r.center().y - galley.size().y / 2.0),
        galley.size(),
    );
    p.galley(text_rect.min, galley, t.ink);
    let text = ui.interact(text_rect, Id::unique("health_text"), Sense::hover());
    text.widget_info(|| WidgetInfo::labeled(Role::Label, true, &issue.text));

    let mut at = r.right() - PAD;
    let close = Rect::from_center_size(
        pos2(at - close_w / 2.0, r.center().y),
        vec2(close_w, close_w),
    );
    at -= close_w;
    let resp = crate::widgets::hit(ui, close, Id::unique("health_close"));
    name_button(&resp, "Dismiss notice");
    crate::widgets::halo(ui, &resp, t);
    xmark(
        ui.painter(),
        close.center(),
        if resp.hovered() { t.ink } else { t.muted },
    );
    out.dismiss = resp.on_hover_text("Dismiss").clicked();

    if let Some((w, g)) = badge {
        let pill = Rect::from_center_size(pos2(at - 8.0 - w / 2.0, r.center().y), vec2(w, 20.0));
        at -= w + 8.0;
        ui.painter()
            .rect_filled(pill, CornerRadius::same(radius::PILL), t.hairline);
        ui.painter()
            .galley(pill.center() - g.size() / 2.0, g, t.muted);
        let tip = format!("{more} more notice{}", if more == 1 { "" } else { "s" });
        let hit = ui.interact(pill, Id::unique("health_more"), Sense::hover());
        hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, &tip));
        hit.on_hover_text(tip);
    }
    if let Some((f, w)) = fix {
        let btn = Rect::from_center_size(pos2(at - 8.0 - w / 2.0, r.center().y), vec2(w, BTN_H));
        if setup::footer_button(ui, btn, "health_fix", f.label(), t).clicked() {
            out.fix = Some(f);
        }
    }
    out
}

/// A small amber badge on a device's icon: it dropped audio in the last few seconds.
pub fn dropout_dot(
    ui: &mut egui::Ui,
    at: egui::Pos2,
    ring: egui::Color32,
    t: &Tokens,
    id: &str,
    name: &str,
    count: u64,
) {
    ui.painter().circle_filled(at, 6.5, ring);
    ui.painter().circle_filled(at, 4.5, t.warn);
    let tip = dropout_tip(count);
    let hit = ui.interact(
        Rect::from_center_size(at, vec2(14.0, 14.0)),
        Id::unique(("dropouts", id)),
        Sense::hover(),
    );
    let label = format!("{name}: {tip}");
    hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
    hit.on_hover_text(tip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_core::backend::HogInfo;
    use sr_core::config::{Source, SourceKind};

    fn hog(device: &str, name: Option<&str>, pid: Option<i32>) -> HogInfo {
        HogInfo {
            device_name: device.into(),
            pid,
            process_name: name.map(Into::into),
        }
    }

    fn counters(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(id, n)| ((*id).into(), *n)).collect()
    }

    /// A status poll: the driver's read-ahead state, its overload counters and the xruns.
    fn poll(
        state: ReadAheadState,
        overloads: &[(&str, u64)],
        xruns: &[(&str, u64)],
    ) -> EngineStatus {
        EngineStatus {
            read_ahead: state,
            driver_overloads: counters(overloads),
            xruns: counters(xruns),
            ..EngineStatus::default()
        }
    }

    const ON: ReadAheadState = ReadAheadState::On;
    const OFF: ReadAheadState = ReadAheadState::Off;

    /// (polls, expected result of each) for one run of the guard with read-ahead wanted.
    #[test]
    fn the_guard_trips_on_growth_between_two_polls_while_on_and_never_otherwise() {
        type Poll = (
            ReadAheadState,
            Vec<(&'static str, u64)>,
            Vec<(&'static str, u64)>,
        );
        let p = |s, o: &[(&'static str, u64)], x: &[(&'static str, u64)]| -> Poll {
            (s, o.to_vec(), x.to_vec())
        };
        type Case = (&'static str, Vec<(Poll, Option<u64>)>);
        let table: Vec<Case> = vec![
            (
                "flat counters never trip, however high the baseline",
                vec![
                    (p(ON, &[("v1", 40)], &[("v1", 9)]), None),
                    (p(ON, &[("v1", 40)], &[("v1", 9)]), None),
                    (p(ON, &[("v1", 40)], &[("v1", 9)]), None),
                ],
            ),
            (
                "an overload between two polls trips with its size",
                vec![
                    (p(ON, &[("v1", 0)], &[]), None),
                    (p(ON, &[("v1", 3)], &[]), Some(3)),
                ],
            ),
            (
                "an xrun alone trips",
                vec![
                    (p(ON, &[("v1", 0)], &[("v1", 5)]), None),
                    (p(ON, &[("v1", 0)], &[("v1", 6)]), Some(1)),
                ],
            ),
            (
                "any device counts, and both counters add up",
                vec![
                    (p(ON, &[("v1", 0), ("v2", 0)], &[("v2", 1)]), None),
                    (p(ON, &[("v1", 0), ("v2", 2)], &[("v2", 4)]), Some(5)),
                ],
            ),
            (
                "growth while read-ahead is off is nobody's business",
                vec![
                    (p(OFF, &[("v1", 0)], &[("v1", 0)]), None),
                    (p(OFF, &[("v1", 50)], &[("v1", 50)]), None),
                    (p(ReadAheadState::Unsupported, &[], &[]), None),
                ],
            ),
            (
                "growth before it turned on is not counted: the first On poll is the baseline",
                vec![
                    (p(OFF, &[("v1", 0)], &[("v1", 0)]), None),
                    (p(ON, &[("v1", 7)], &[("v1", 7)]), None),
                    (p(ON, &[("v1", 7)], &[("v1", 7)]), None),
                    (p(ON, &[("v1", 8)], &[("v1", 7)]), Some(1)),
                ],
            ),
            (
                "a rebuilt device starts its counters over without a trip",
                vec![
                    (p(ON, &[("v1", 9)], &[("v1", 9)]), None),
                    (p(ON, &[("v1", 0)], &[("v1", 0)]), None),
                    (p(ON, &[("v1", 1)], &[("v1", 0)]), Some(1)),
                ],
            ),
            (
                "a device that appears while on sets its baseline",
                vec![
                    (p(ON, &[("v1", 0)], &[]), None),
                    (p(ON, &[("v1", 0), ("v2", 12)], &[("v2", 3)]), None),
                    (p(ON, &[("v1", 0), ("v2", 12)], &[("v2", 3)]), None),
                ],
            ),
            (
                "it fires once per run of On, and again after On, Off, On",
                vec![
                    (p(ON, &[("v1", 0)], &[]), None),
                    (p(ON, &[("v1", 1)], &[]), Some(1)),
                    (p(ON, &[("v1", 5)], &[]), None),
                    (p(OFF, &[("v1", 5)], &[]), None),
                    (p(ON, &[("v1", 5)], &[]), None),
                    (p(ON, &[("v1", 6)], &[]), Some(1)),
                ],
            ),
        ];
        for (name, polls) in table {
            let mut g = ReadAheadGuard::default();
            for (i, ((state, o, x), want)) in polls.into_iter().enumerate() {
                let status = poll(state, &o, &x);
                assert_eq!(g.observe(&status, true), want, "{name}: poll {i}");
            }
        }
    }

    #[test]
    fn the_guard_counts_overloads_since_on_for_the_sheet() {
        let mut g = ReadAheadGuard::default();
        assert_eq!(g.observe(&poll(ON, &[("v1", 2)], &[]), true), None);
        assert_eq!(g.overloads_since_on(), 0);
        assert_eq!(g.observe(&poll(ON, &[("v1", 4)], &[]), true), Some(2));
        assert_eq!(g.overloads_since_on(), 2);
        g.observe(&poll(OFF, &[("v1", 4)], &[]), true);
        assert_eq!(
            g.overloads_since_on(),
            0,
            "turning off starts the count over"
        );
    }

    #[test]
    fn the_guard_ignores_a_driver_that_is_still_on_after_the_user_turned_it_off() {
        let mut g = ReadAheadGuard::default();
        g.observe(&poll(ON, &[("v1", 0)], &[]), true);
        assert_eq!(g.observe(&poll(ON, &[("v1", 9)], &[]), false), None);
    }

    #[test]
    fn the_guard_follows_the_engines_own_trip_but_not_a_stale_one() {
        let tripped = |state| EngineStatus {
            read_ahead_tripped: true,
            ..poll(state, &[], &[])
        };
        // A flag left from an earlier run, set when the user asks again: not a new trip.
        let mut g = ReadAheadGuard::default();
        assert_eq!(g.observe(&tripped(OFF), true), None);
        assert_eq!(g.observe(&tripped(ON), true), None);
        // Once the engine cleared it, a new trip counts, even though the driver already says Off.
        let mut g = ReadAheadGuard::default();
        assert_eq!(g.observe(&poll(ON, &[], &[]), true), None);
        assert_eq!(g.observe(&tripped(OFF), true), Some(1));
        // Not wanted: nothing to turn off.
        assert_eq!(g.observe(&tripped(OFF), false), None);
    }

    #[test]
    fn the_trip_banner_names_the_dropouts_and_leads_to_settings() {
        let one = trip_issue(1);
        assert_eq!(
            one.text,
            "Lower delay turned itself off after 1 driver dropout."
        );
        assert_eq!(one.fix, Some(Fix::Settings));
        assert_eq!(Fix::Settings.label(), "Settings");
        assert_eq!(
            trip_issue(7).text,
            "Lower delay turned itself off after 7 driver dropouts."
        );
        // Ranked below every engine problem: the app appends it after `issues`, so a silent capture
        // or a denied permission is the banner picked first.
        let mut list = issues(
            &EngineStatus {
                tcc: TccStatus::Denied,
                ..EngineStatus::default()
            },
            true,
            |_| String::new(),
        );
        list.push(trip_issue(2));
        let mut d = Dismissed::default();
        assert_eq!(d.pick(&list).unwrap().0.key, "capture-denied");
        d.dismiss("capture-denied");
        assert_eq!(d.pick(&list).unwrap().0.key, TRIP_KEY);
        d.dismiss(TRIP_KEY);
        assert!(d.pick(&list).is_none());
        d.forget(TRIP_KEY);
        assert_eq!(d.pick(&list).unwrap().0.key, TRIP_KEY);
    }

    fn all_four() -> EngineStatus {
        EngineStatus {
            tap_silent: vec!["v1".into()],
            hogged: vec![hog("USB DAC", Some("Logic Pro"), Some(41))],
            bluetooth_hfp: vec!["AirPods".into()],
            tcc: TccStatus::Granted,
            ..EngineStatus::default()
        }
    }

    #[test]
    fn healthy_engine_has_nothing_to_say() {
        assert!(issues(&EngineStatus::default(), true, |_| "x".into()).is_empty());
    }

    #[test]
    fn issues_come_most_important_first_with_plain_sentences() {
        let got = issues(&all_four(), true, |id| format!("app on {id}"));
        let texts: Vec<&str> = got.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Audio from app on v1 looks silent. If you expect sound, check that SoundRouter may capture audio.",
                "USB DAC is in exclusive use by Logic Pro; apps playing to it cannot be routed.",
                "AirPods switched to a phone-call mode (low quality) because a microphone is open on it.",
            ]
        );
        assert_eq!(got[0].fix, Some(Fix::Privacy));
        assert_eq!(got[1].fix, None);
    }

    #[test]
    fn a_denied_capture_replaces_the_silent_taps_it_explains() {
        let status = EngineStatus {
            tcc: TccStatus::Denied,
            ..all_four()
        };
        let got = issues(&status, true, |_| "x".into());
        assert_eq!(got[0].key, "capture-denied");
        assert!(got.iter().all(|i| !i.key.starts_with("silent:")));
        assert_eq!(got.len(), 3);
        // Denied without any app source is nobody's problem.
        assert!(
            issues(&status, false, |_| "x".into())
                .iter()
                .all(|i| i.key != "capture-denied")
        );
    }

    #[test]
    fn a_hog_without_a_process_name_is_still_named() {
        let status = |h| EngineStatus {
            hogged: vec![h],
            ..EngineStatus::default()
        };
        let text = |h| issues(&status(h), false, |_| String::new())[0].text.clone();
        assert!(text(hog("DAC", None, Some(7))).contains("another app (process 7);"));
        assert!(text(hog("DAC", None, None)).contains("another app;"));
    }

    #[test]
    fn the_first_banner_shows_with_a_count_and_closing_it_reveals_the_next() {
        let got = issues(&all_four(), true, |_| "x".into());
        let mut d = Dismissed::default();
        let (first, more) = d.pick(&got).unwrap();
        assert_eq!((first.key.as_str(), more), ("silent:v1", 2));
        d.dismiss("silent:v1");
        let (second, more) = d.pick(&got).unwrap();
        assert_eq!((second.key.as_str(), more), ("hogged:USB DAC", 1));
        d.dismiss("hogged:USB DAC");
        d.dismiss("hfp:AirPods");
        assert!(d.pick(&got).is_none());
    }

    #[test]
    fn a_closed_banner_does_not_hide_a_different_problem_of_the_same_kind() {
        let mut d = Dismissed::default();
        d.dismiss("hogged:USB DAC");
        let other = [Issue {
            key: "hogged:Scarlett".into(),
            text: String::new(),
            fix: None,
        }];
        assert!(d.pick(&other).is_some());
    }

    #[test]
    fn subject_names_the_apps_then_falls_back_to_the_device() {
        let mut dev = crate::model::default_config().devices.remove(0);
        dev.name = "Stream Mix".into();
        dev.sources.clear();
        assert_eq!(subject(&dev), "Stream Mix");
        let src = |id: &str, name: &str, bundle: &str| Source {
            id: id.into(),
            kind: SourceKind::App {
                bundle_id: bundle.into(),
            },
            channels: 2,
            volume_db: 0.0,
            muted: false,
            mute_original: false,
            name: name.into(),
        };
        dev.sources = vec![
            src("s1", "Safari", "com.apple.Safari"),
            src("s2", "", "org.example.Unnamed"),
        ];
        assert_eq!(subject(&dev), "Safari and org.example.Unnamed");
        dev.sources.push(src("s3", "Zoom", "us.zoom.xos"));
        dev.sources.push(src("s4", "Music", "com.apple.Music"));
        assert_eq!(subject(&dev), "Safari, org.example.Unnamed and 2 more");
    }

    fn xr(n: u64) -> Vec<(String, u64)> {
        vec![("v1".into(), n)]
    }

    #[test]
    fn dropouts_show_only_while_the_counter_grew_in_the_last_ten_seconds() {
        let mut d = Dropouts::default();
        d.observe(&xr(5), 0.0); // seen late: a baseline, not news
        assert_eq!((d.growing("v1", 0.0), d.total()), (None, 5));
        d.observe(&xr(5), 1.0);
        assert_eq!(d.growing("v1", 1.0), None);
        d.observe(&xr(7), 2.0);
        assert_eq!(d.growing("v1", 2.0), Some(7));
        assert_eq!(d.growing("v1", 11.9), Some(7));
        assert_eq!(d.growing("v1", 12.0), None);
        assert!(d.any_growing(3.0) && !d.any_growing(13.0));
    }

    #[test]
    fn a_rebuilt_device_starts_over_and_a_gone_one_is_forgotten() {
        let mut d = Dropouts::default();
        d.observe(&xr(2), 0.0);
        d.observe(&xr(4), 1.0);
        d.observe(&xr(0), 30.0); // rebuilt: the counter fell
        assert_eq!((d.growing("v1", 30.0), d.total()), (None, 0));
        d.observe(&xr(1), 31.0);
        assert_eq!(d.growing("v1", 31.0), Some(1));
        d.observe(&[], 32.0);
        assert_eq!(d.total(), 0);
        d.observe(&xr(9), 33.0); // comes back: a new baseline
        assert_eq!(d.growing("v1", 33.0), None);
    }

    #[test]
    fn a_rebuilt_device_that_drops_out_again_before_the_next_look_is_news() {
        let mut d = Dropouts::default();
        d.observe(&xr(4), 0.0);
        d.observe(&xr(2), 1.0); // rebuilt, and already 2 dropouts
        assert_eq!(d.growing("v1", 1.0), Some(2));
        let mut d = Dropouts::default();
        d.observe(&xr(4), 0.0);
        d.observe(&xr(0), 1.0); // rebuilt, clean
        assert_eq!(d.growing("v1", 1.0), None);
    }

    #[test]
    fn tooltips_and_menu_line_pluralise_and_advise_only_while_growing() {
        assert_eq!(dropout_tip(1), "1 dropout since start");
        assert_eq!(dropout_tip(12), "12 dropouts since start");
        assert_eq!(
            health_line(0, false, true),
            ("Audio health: no dropouts since start".into(), None)
        );
        assert_eq!(
            health_line(3, false, true),
            ("Audio health: 3 dropouts since start".into(), None)
        );
        assert_eq!(
            health_line(3, true, true).1,
            Some("Dropouts: choose a larger delay")
        );
        assert!(
            health_line(3, true, false)
                .1
                .unwrap()
                .contains("close busy apps")
        );
    }
}
