//! Real application icons from NSWorkspace plus painted device glyphs.
//! `resolve` is slow and AppKit-bound: call it only from the worker's refresh, never the UI thread.
use eframe::egui::{
    Color32, ColorImage, Context, Painter, Pos2, Rect, Shape, Stroke, TextureHandle,
    TextureOptions, pos2, vec2,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Edge of the bitmap handed to the UI; drawn at 36 px, so 64 stays crisp on Retina.
pub const ICON_PX: usize = 64;

/// Maps a source key (a bundle id, or `path:<exe>` for CLI players) to its icon.
pub type IconResolver = Box<dyn FnMut(&str) -> Option<ColorImage> + Send>;

/// Icon for `key`, or `None` when the app is unknown or the platform has no icons.
pub fn resolve(key: &str) -> Option<ColorImage> {
    #[cfg(target_os = "macos")]
    {
        mac::resolve(key)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::ICON_PX;
    use eframe::egui::ColorImage;
    use objc2::AnyThread;
    use objc2_app_kit::{
        NSBitmapImageRep, NSCalibratedRGBColorSpace, NSCompositingOperation, NSGraphicsContext,
        NSImage, NSWorkspace,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use std::ptr::null_mut;

    pub fn resolve(key: &str) -> Option<ColorImage> {
        let ws = NSWorkspace::sharedWorkspace();
        let path = match key.strip_prefix("path:") {
            Some(exe) => NSString::from_str(exe),
            None => {
                let url = ws.URLForApplicationWithBundleIdentifier(&NSString::from_str(key))?;
                url.path()?
            }
        };
        // For a plain executable, iconForFile returns the generic exec icon.
        rasterize(&ws.iconForFile(&path))
    }

    fn rasterize(image: &NSImage) -> Option<ColorImage> {
        let px = ICON_PX as isize;
        // SAFETY: plain AppKit calls on objects created here; the rep owns its pixel buffer and is
        // read only while `rep` is alive and not drawn into.
        unsafe {
            let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                null_mut(),
                px,
                px,
                8,
                4,
                true,
                false,
                NSCalibratedRGBColorSpace,
                0,
                0,
            )?;
            let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
            NSGraphicsContext::saveGraphicsState_class();
            NSGraphicsContext::setCurrentContext(Some(&ctx));
            let full = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(px as f64, px as f64));
            image.drawInRect_fromRect_operation_fraction(
                full,
                NSRect::ZERO,
                NSCompositingOperation::SourceOver,
                1.0,
            );
            ctx.flushGraphics();
            NSGraphicsContext::restoreGraphicsState_class();
            let stride = rep.bytesPerRow() as usize;
            let data = rep.bitmapData();
            if data.is_null() || stride < ICON_PX * 4 {
                return None;
            }
            let mut rgba = Vec::with_capacity(ICON_PX * ICON_PX * 4);
            for row in 0..ICON_PX {
                rgba.extend_from_slice(std::slice::from_raw_parts(
                    data.add(row * stride),
                    ICON_PX * 4,
                ));
            }
            // Calibrated RGB with alpha is premultiplied.
            Some(ColorImage::from_rgba_premultiplied(
                [ICON_PX, ICON_PX],
                &rgba,
            ))
        }
    }
}

/// GPU copies of the worker's icons, uploaded on the UI thread the first time a key shows up.
#[derive(Default)]
pub struct IconTextures(HashMap<String, TextureHandle>);

impl IconTextures {
    pub fn sync(&mut self, ctx: &Context, images: &HashMap<String, Arc<ColorImage>>) {
        for (key, img) in images {
            self.0.entry(key.clone()).or_insert_with(|| {
                ctx.load_texture(format!("icon:{key}"), img.clone(), TextureOptions::LINEAR)
            });
        }
    }

    pub fn get(&self, key: &str) -> Option<&TextureHandle> {
        self.0.get(key)
    }
}

/// Microphone glyph (input devices) inside `r`.
pub fn mic(p: &Painter, r: Rect, color: Color32) {
    let w = r.width();
    let s = Stroke::new(w * 0.07, color);
    let body = Rect::from_center_size(r.center() + vec2(0.0, -w * 0.08), vec2(w * 0.26, w * 0.44));
    p.rect_filled(body, w * 0.13, color);
    // Pickup cradle: a U under the capsule, then stem and foot.
    let cx = r.center().x;
    let top = body.center().y;
    let bottom = body.bottom() + w * 0.1;
    let rad = w * 0.21;
    let u: Vec<Pos2> = (0..=12)
        .map(|i| {
            let a = std::f32::consts::PI * i as f32 / 12.0;
            pos2(
                cx - rad * a.cos(),
                top + (bottom - top - rad).max(0.0) + rad * a.sin(),
            )
        })
        .collect();
    p.add(Shape::line(u, s));
    let stem_top = top + (bottom - top - rad).max(0.0) + rad;
    let foot = stem_top + w * 0.1;
    p.line_segment([pos2(cx, stem_top), pos2(cx, foot)], s);
    p.line_segment([pos2(cx - w * 0.12, foot), pos2(cx + w * 0.12, foot)], s);
}

/// Loudspeaker glyph (output devices) inside `r`.
pub fn speaker(p: &Painter, r: Rect, color: Color32) {
    let w = r.width();
    let c = r.center();
    let x0 = c.x - w * 0.26;
    let cone = vec![
        pos2(x0, c.y - w * 0.11),
        pos2(x0 + w * 0.14, c.y - w * 0.11),
        pos2(x0 + w * 0.34, c.y - w * 0.27),
        pos2(x0 + w * 0.34, c.y + w * 0.27),
        pos2(x0 + w * 0.14, c.y + w * 0.11),
        pos2(x0, c.y + w * 0.11),
    ];
    p.add(Shape::convex_polygon(cone, color, Stroke::NONE));
    let s = Stroke::new(w * 0.07, color);
    for (k, a) in [(0.0f32, 0.55f32), (1.0, 0.9)] {
        let rad = w * (0.17 + 0.12 * k);
        let pts: Vec<Pos2> = (0..=8)
            .map(|i| {
                let t = -a + 2.0 * a * i as f32 / 8.0;
                pos2(x0 + w * 0.4 + rad * t.cos() * 0.8, c.y + rad * t.sin())
            })
            .collect();
        p.add(Shape::line(pts, s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_bundle_has_no_icon() {
        assert!(resolve("com.example.definitely.not.installed").is_none());
    }

    #[test]
    #[ignore = "needs a real macOS desktop session with Safari installed"]
    fn safari_icon_is_a_64px_bitmap() {
        let img = resolve("com.apple.Safari").expect("Safari icon");
        assert_eq!(img.size, [ICON_PX, ICON_PX]);
        assert!(img.pixels.iter().any(|p| p.a() > 0), "icon is blank");
        let corner = img.pixels[0];
        assert_eq!(corner.a(), 0, "squircle corner must be transparent");
    }

    #[test]
    #[ignore = "needs a real macOS desktop session"]
    fn cli_executable_gets_the_generic_icon() {
        let img = resolve("path:/usr/bin/afplay").expect("exec icon");
        assert_eq!(img.size, [ICON_PX, ICON_PX]);
    }
}
