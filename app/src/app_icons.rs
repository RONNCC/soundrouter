//! Icons for the "Choose App…" list: every installed app, so far more than the worker's 64-icon
//! snapshot may hold. Only rows on screen (plus a little look-ahead) are resolved, on one small
//! loader thread, into a separate bounded LRU that is dropped when the list closes.
use crate::icons::IconResolver;
use eframe::egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};

/// Textures kept while the list is open (it shows ~13 rows, so scrolling 300 apps never evicts one in view).
pub const TEXTURE_CAP: usize = 256;
/// Apps whose lookup failed, remembered for the session so a bundle without an icon is tried once.
const MISSING_CAP: usize = 4096;
/// Requests sent but not answered yet; more wait for the next frame, so a fast scroll cannot queue 300 lookups.
const IN_FLIGHT: usize = 24;
/// Rows resolved beyond the visible ones in each direction.
pub const LOOK_AHEAD: usize = 6;
/// Bitmap edge handed to the GPU: the list draws 18 pt, so 40 px is crisp at 2x.
pub const PICKER_PX: usize = 40;

/// Least-recently-used map with a hard size limit.
pub struct Lru<V> {
    cap: usize,
    tick: u64,
    map: HashMap<String, (V, u64)>,
}

impl<V> Lru<V> {
    pub fn new(cap: usize) -> Self {
        Lru {
            cap,
            tick: 0,
            map: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// The value, marked as just used.
    pub fn get(&mut self, key: &str) -> Option<&V> {
        self.tick += 1;
        let tick = self.tick;
        let (v, used) = self.map.get_mut(key)?;
        *used = tick;
        Some(v)
    }

    /// Stores `v`, dropping the least recently used entries beyond the cap.
    pub fn insert(&mut self, key: String, v: V) {
        self.tick += 1;
        self.map.insert(key, (v, self.tick));
        while self.map.len() > self.cap {
            let oldest = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.map.remove(&k),
                None => break,
            };
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

/// Rows of a uniform list that intersect the clip: row `i` starts at `top0 + i * pitch`.
pub fn visible_rows(
    top0: f32,
    pitch: f32,
    n: usize,
    clip_top: f32,
    clip_bottom: f32,
) -> Range<usize> {
    if n == 0 || pitch <= 0.0 || clip_bottom <= clip_top {
        return 0..0;
    }
    let at = |y: f32| ((y - top0) / pitch).max(0.0) as usize;
    let first = at(clip_top).min(n);
    let end = (at(clip_bottom) + 1).min(n);
    first..end.max(first)
}

/// Row indexes to resolve, most urgent first: the visible rows, then `look` rows after them, then
/// `look` rows before them (nearest first).
pub fn wanted(visible: &Range<usize>, n: usize, look: usize) -> Vec<usize> {
    if visible.is_empty() {
        return Vec::new();
    }
    let after = visible.end..(visible.end + look).min(n);
    let before = (visible.start.saturating_sub(look)..visible.start).rev();
    visible.clone().chain(after).chain(before).collect()
}

/// Box-filters `img` down to at most `px` on its long edge (premultiplied pixels average correctly).
pub fn downscale(img: &ColorImage, px: usize) -> ColorImage {
    let [w, h] = img.size;
    if w <= px && h <= px {
        return img.clone();
    }
    let (dw, dh) = if w >= h {
        (px, (h * px / w).max(1))
    } else {
        ((w * px / h).max(1), px)
    };
    let span = |i: usize, d: usize, s: usize| (i * s / d, ((i + 1) * s / d).max(i * s / d + 1));
    let mut out = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        let (y0, y1) = span(y, dh, h);
        for x in 0..dw {
            let (x0, x1) = span(x, dw, w);
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                for p in &img.pixels[sy * w + x0..sy * w + x1] {
                    for (s, c) in sum.iter_mut().zip(p.to_array()) {
                        *s += u32::from(c);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let [r, g, b, a] = sum.map(|s| (s / n) as u8);
            out.push(Color32::from_rgba_premultiplied(r, g, b, a));
        }
    }
    ColorImage::new([dw, dh], out)
}

/// What a row draws now.
pub enum Look {
    Icon(TextureHandle),
    /// No icon could be resolved: the letter tile.
    Missing,
    /// Asked for or not yet asked: the neutral tile.
    Loading,
}

struct Loaded {
    epoch: u64,
    key: String,
    image: Option<ColorImage>,
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Vec<Loaded>>,
    /// Bumped when the list closes: answers and queued lookups of an older epoch are dropped.
    epoch: AtomicU64,
}

/// Resolves `key` (if its epoch is still current), shrinks it, and wakes the UI.
fn run_job(
    shared: &Shared,
    resolve: &mut IconResolver,
    ctx: &Context,
    (epoch, key): (u64, String),
) {
    if epoch != shared.epoch.load(Relaxed) {
        return;
    }
    let image = resolve(&key).map(|i| downscale(&i, PICKER_PX));
    shared
        .inbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Loaded { epoch, key, image });
    ctx.request_repaint();
}

enum Sink {
    /// Not started: the thread is spawned by the first request, when a `Context` exists.
    Idle(IconResolver),
    Thread(Sender<(u64, String)>),
    /// Tests: lookups wait here until the test answers them.
    #[cfg(test)]
    Manual(IconResolver, Vec<(u64, String)>),
}

pub struct PickerIcons {
    shared: Arc<Shared>,
    sink: Option<Sink>,
    textures: Lru<TextureHandle>,
    /// Session-long: apps that have no icon.
    missing: Lru<()>,
    pending: HashSet<String>,
    /// The list was drawn this frame; see [`PickerIcons::end_frame`].
    open: bool,
}

impl Default for PickerIcons {
    fn default() -> Self {
        // Tests never touch AppKit: they answer lookups themselves.
        let resolve: IconResolver = if cfg!(test) {
            Box::new(|_| None)
        } else {
            Box::new(crate::icons::resolve)
        };
        PickerIcons {
            shared: Arc::default(),
            sink: Some(Sink::Idle(resolve)),
            textures: Lru::new(TEXTURE_CAP),
            missing: Lru::new(MISSING_CAP),
            pending: HashSet::new(),
            open: false,
        }
    }
}

impl PickerIcons {
    /// Call each frame the list is drawn, before reading rows: uploads what the loader finished.
    pub fn begin(&mut self, ctx: &Context) {
        self.open = true;
        let done = std::mem::take(
            &mut *self
                .shared
                .inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let epoch = self.shared.epoch.load(Relaxed);
        for l in done.into_iter().filter(|l| l.epoch == epoch) {
            self.pending.remove(&l.key);
            match l.image {
                Some(img) => {
                    let tex =
                        ctx.load_texture(format!("picker:{}", l.key), img, TextureOptions::LINEAR);
                    self.textures.insert(l.key, tex);
                }
                None => self.missing.insert(l.key, ()),
            }
        }
    }

    /// What `key`'s row draws; marks a shown icon as in use.
    pub fn look(&mut self, key: &str) -> Look {
        if let Some(t) = self.textures.get(key) {
            Look::Icon(t.clone())
        } else if self.missing.contains(key) {
            Look::Missing
        } else {
            Look::Loading
        }
    }

    /// Asks the loader for `keys` (most urgent first) that are neither cached, failed nor already asked.
    pub fn request<'a>(&mut self, ctx: &Context, keys: impl IntoIterator<Item = &'a str>) {
        let epoch = self.shared.epoch.load(Relaxed);
        for key in keys {
            if self.pending.len() >= IN_FLIGHT {
                break;
            }
            if self.textures.contains(key)
                || self.missing.contains(key)
                || self.pending.contains(key)
            {
                continue;
            }
            let job = (epoch, key.to_string());
            let sink = match self.sink.take() {
                Some(Sink::Idle(mut resolve)) => {
                    let (tx, rx) = mpsc::channel::<(u64, String)>();
                    let (shared, ctx) = (self.shared.clone(), ctx.clone());
                    let spawned = std::thread::Builder::new()
                        .name("picker-icons".into())
                        .spawn(move || {
                            for job in rx {
                                run_job(&shared, &mut resolve, &ctx, job);
                            }
                        });
                    if let Err(e) = spawned {
                        log::warn!("icon loader thread: {e}");
                    }
                    Sink::Thread(tx)
                }
                Some(s) => s,
                None => return,
            };
            // A dead loader (spawn failed) leaves the rows on their placeholders: nothing to retry.
            self.sink = Some(match sink {
                Sink::Thread(tx) => {
                    let _ = tx.send(job);
                    Sink::Thread(tx)
                }
                #[cfg(test)]
                Sink::Manual(r, mut q) => {
                    q.push(job);
                    Sink::Manual(r, q)
                }
                idle => idle,
            });
            self.pending.insert(key.to_string());
        }
    }

    /// Call once per frame after the whole UI: a frame that did not draw the list drops everything
    /// held for it (textures, queued lookups). Failures stay for the session.
    pub fn end_frame(&mut self) {
        if !std::mem::take(&mut self.open) && (self.textures.len() > 0 || !self.pending.is_empty())
        {
            self.shared.epoch.fetch_add(1, Relaxed);
            self.shared
                .inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            self.pending.clear();
            self.textures.clear();
            #[cfg(test)]
            if let Some(Sink::Manual(_, q)) = &mut self.sink {
                q.clear();
            }
        }
    }
}

#[cfg(test)]
impl PickerIcons {
    /// Answers lookups with `resolve` only when `answer` is called.
    pub fn manual(&mut self, resolve: IconResolver) {
        self.sink = Some(Sink::Manual(resolve, Vec::new()));
    }

    /// Keys asked for and not yet answered.
    pub fn queued(&self) -> Vec<String> {
        match &self.sink {
            Some(Sink::Manual(_, q)) => q.iter().map(|(_, k)| k.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Runs every queued lookup, as the loader thread would; the UI is asked to repaint.
    pub fn answer(&mut self, ctx: &Context) {
        if let Some(Sink::Manual(resolve, q)) = &mut self.sink {
            for job in std::mem::take(q) {
                run_job(&self.shared, resolve, ctx, job);
            }
        }
    }

    pub fn textures(&self) -> usize {
        self.textures.len()
    }

    pub fn missing(&self) -> usize {
        self.missing.len()
    }

    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_rows_follow_the_clip_and_scroll_offset() {
        // 28 pt pitch, list scrolled up by 100 pt: rows 3.. are in view of a 0..112 clip.
        assert_eq!(visible_rows(-100.0, 28.0, 300, 0.0, 112.0), 3..8);
        assert_eq!(visible_rows(0.0, 28.0, 300, 0.0, 112.0), 0..5);
        assert_eq!(visible_rows(0.0, 28.0, 4, 0.0, 112.0), 0..4, "short list");
        assert_eq!(visible_rows(0.0, 28.0, 0, 0.0, 112.0), 0..0);
        assert_eq!(visible_rows(0.0, 28.0, 10, 50.0, 50.0), 0..0, "empty clip");
        assert_eq!(
            visible_rows(-9000.0, 28.0, 10, 0.0, 100.0),
            10..10,
            "scrolled past the end"
        );
    }

    #[test]
    fn wanted_lists_visible_rows_then_look_ahead_after_then_before() {
        assert_eq!(wanted(&(10..13), 300, 2), [10, 11, 12, 13, 14, 9, 8]);
        assert_eq!(wanted(&(0..2), 3, 4), [0, 1, 2], "clamped at both ends");
        assert_eq!(wanted(&(5..5), 300, 4), Vec::<usize>::new());
        assert!(wanted(&(0..13), 300, LOOK_AHEAD).len() < 25);
    }

    #[test]
    fn lru_drops_the_least_recently_used_beyond_its_cap() {
        let mut c = Lru::new(3);
        for k in ["a", "b", "c"] {
            c.insert(k.into(), k.len());
        }
        c.get("a"); // a is now newer than b
        c.insert("d".into(), 1);
        assert_eq!(c.len(), 3);
        assert!(c.contains("a") && c.contains("c") && c.contains("d") && !c.contains("b"));
        c.insert("a".into(), 2); // replacing does not grow it
        assert_eq!(c.len(), 3);
        c.clear();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn the_negative_cache_is_bounded_too() {
        let mut m: Lru<()> = Lru::new(MISSING_CAP);
        for i in 0..MISSING_CAP + 10 {
            m.insert(format!("k{i}"), ());
        }
        assert_eq!(m.len(), MISSING_CAP);
        assert!(!m.contains("k0") && m.contains(&format!("k{}", MISSING_CAP + 9)));
    }

    #[test]
    fn downscale_averages_and_keeps_aspect() {
        let big = ColorImage::filled([64, 64], Color32::from_rgb(10, 20, 30));
        let small = downscale(&big, 40);
        assert_eq!(small.size, [40, 40]);
        assert!(
            small
                .pixels
                .iter()
                .all(|p| *p == Color32::from_rgb(10, 20, 30))
        );
        let wide = downscale(&ColorImage::filled([100, 50], Color32::WHITE), 40);
        assert_eq!(wide.size, [40, 20]);
        let tiny = ColorImage::filled([8, 8], Color32::RED);
        assert_eq!(downscale(&tiny, 40).size, [8, 8], "never upscaled");
        // Half transparent, half opaque: the average is half alpha.
        let mut two = ColorImage::filled([2, 1], Color32::TRANSPARENT);
        two.pixels[0] = Color32::from_rgba_premultiplied(100, 100, 100, 200);
        assert_eq!(
            downscale(&two, 1).pixels[0],
            Color32::from_rgba_premultiplied(50, 50, 50, 100)
        );
    }

    fn picker() -> (PickerIcons, Context, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = calls.clone();
        let mut p = PickerIcons::default();
        p.manual(Box::new(move |k| {
            log.lock().unwrap().push(k.to_string());
            (k != "none").then(|| ColorImage::filled([64, 64], Color32::GREEN))
        }));
        (p, Context::default(), calls)
    }

    #[test]
    fn requests_are_deduplicated_bounded_and_negatively_cached() {
        let (mut p, ctx, calls) = picker();
        let keys: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
        let ask = |p: &mut PickerIcons| p.request(&ctx, keys.iter().map(String::as_str));
        ask(&mut p);
        ask(&mut p); // same frame again: nothing new
        assert_eq!(p.queued().len(), IN_FLIGHT);
        p.answer(&ctx);
        p.begin(&ctx);
        assert_eq!((p.textures(), p.in_flight()), (IN_FLIGHT, 0));
        ask(&mut p);
        assert_eq!(p.queued().len(), IN_FLIGHT, "the next batch, not a repeat");
        assert!(
            p.queued()
                .iter()
                .all(|k| !calls.lock().unwrap().contains(k))
        );
        p.answer(&ctx);
        p.begin(&ctx);
        // A failure is remembered and never asked again.
        p.request(&ctx, ["none"]);
        p.answer(&ctx);
        p.begin(&ctx);
        assert!(matches!(p.look("none"), Look::Missing));
        p.request(&ctx, ["none"]);
        assert!(p.queued().is_empty());
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|k| *k == "none")
                .count(),
            1
        );
        assert!(matches!(p.look("k0"), Look::Icon(_)));
        assert!(matches!(p.look("k99"), Look::Loading));
    }

    #[test]
    fn closing_drops_textures_and_late_answers_but_keeps_failures() {
        let (mut p, ctx, _) = picker();
        p.request(&ctx, ["a", "none"]);
        p.answer(&ctx);
        p.begin(&ctx);
        p.end_frame();
        assert_eq!((p.textures(), p.missing()), (1, 1));
        // A frame without the list, with a lookup still out: all of it is dropped.
        p.begin(&ctx);
        p.request(&ctx, ["b"]);
        p.end_frame();
        p.end_frame();
        assert_eq!((p.textures(), p.in_flight(), p.queued().len()), (0, 0, 0));
        assert_eq!(p.missing(), 1);
    }

    #[test]
    fn a_texture_cache_never_passes_its_cap() {
        let (mut p, ctx, _) = picker();
        for i in 0..TEXTURE_CAP + 40 {
            p.request(&ctx, [format!("k{i}").as_str()]);
            p.answer(&ctx);
            p.begin(&ctx);
        }
        assert_eq!(p.textures(), TEXTURE_CAP);
        assert!(matches!(
            p.look(&format!("k{}", TEXTURE_CAP + 39)),
            Look::Icon(_)
        ));
        assert!(matches!(p.look("k0"), Look::Loading));
    }
}
