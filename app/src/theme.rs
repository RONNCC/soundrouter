//! Design tokens (macOS system colours) and font setup.
use eframe::egui::{self, Color32, FontFamily};
use std::sync::Arc;

pub const fn hex(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

#[derive(Clone, Copy)]
pub struct Tokens {
    pub window: Color32,
    pub sidebar: Color32,
    pub row_sel: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub accent: Color32,
    /// Wire colours: left (even) and right (odd) channel.
    pub ch: [Color32; 2],
    /// Meter green, yellow, red.
    pub meter: [Color32; 3],
    pub track: Color32,
    pub shadow: Color32,
    /// Dims the window behind a modal sheet.
    pub scrim: Color32,
    pub switch_off: Color32,
    pub danger: Color32,
    /// Warning text on a card, e.g. "Not connected" (meter yellow is unreadable on white).
    pub warn: Color32,
}

pub fn tokens(dark: bool) -> Tokens {
    if dark {
        Tokens {
            window: hex(0x1E1E1E),
            sidebar: hex(0x2A2A2C),
            row_sel: hex(0x3A3A3D),
            card: hex(0x323234),
            card_stroke: hex(0x444446),
            text: hex(0xF2F2F7),
            text2: hex(0x98989D),
            accent: hex(0x0A84FF),
            ch: [hex(0x0A84FF), hex(0xFF9F0A)],
            meter: [hex(0x30D158), hex(0xFFD60A), hex(0xFF453A)],
            track: hex(0x1C1C1E),
            shadow: Color32::from_black_alpha(90),
            scrim: Color32::from_black_alpha(80),
            switch_off: hex(0x48484A),
            danger: hex(0xFF453A),
            warn: hex(0xFFD60A),
        }
    } else {
        Tokens {
            window: hex(0xF2F2F4),
            sidebar: hex(0xE6E6E9),
            row_sel: hex(0xCFCFD4),
            card: hex(0xFFFFFF),
            card_stroke: hex(0xD8D8DC),
            text: hex(0x1D1D1F),
            text2: hex(0x6E6E73),
            accent: hex(0x007AFF),
            ch: [hex(0x007AFF), hex(0xFF9500)],
            meter: [hex(0x34C759), hex(0xFFCC00), hex(0xFF3B30)],
            track: hex(0xE5E5EA),
            shadow: Color32::from_black_alpha(28),
            scrim: Color32::from_black_alpha(36),
            switch_off: hex(0xC7C7CC),
            danger: hex(0xFF3B30),
            warn: hex(0xB25000),
        }
    }
}

/// SF Pro from the system when present (loaded at runtime, never bundled); egui's default otherwise.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let sf = std::fs::read("/System/Library/Fonts/SFNS.ttf").ok();
    if let Some(bytes) = sf {
        // SFNS is a variable font: pick the weight through the `wght` axis.
        for (name, wght) in [("sf", 400.0), ("sf-semibold", 590.0)] {
            let tweak = egui::FontTweak {
                coords: egui::epaint::text::VariationCoords::new([("wght", wght)]),
                ..Default::default()
            };
            fonts.font_data.insert(
                name.into(),
                Arc::new(egui::FontData::from_owned(bytes.clone()).tweak(tweak)),
            );
        }
        fonts
            .families
            .get_mut(&FontFamily::Proportional)
            .unwrap()
            .insert(0, "sf".into());
        let mut semi = fonts.families[&FontFamily::Proportional].clone();
        semi[0] = "sf-semibold".into();
        fonts.families.insert(semibold(), semi);
    } else {
        let p = fonts.families[&FontFamily::Proportional].clone();
        fonts.families.insert(semibold(), p);
    }
    ctx.set_fonts(fonts);
}

/// Tint for an app/device tile, stable per name.
pub fn tile_tint(name: &str) -> Color32 {
    const PALETTE: [u32; 7] = [
        0x1E90FF, 0xFA2D48, 0x34C759, 0xFF9500, 0xAF52DE, 0x30B0C7, 0x5E5CE6,
    ];
    let h = name
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    hex(PALETTE[h as usize % PALETTE.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tint_is_stable() {
        assert_eq!(tile_tint("Safari"), tile_tint("Safari"));
        assert_eq!(hex(0x0A84FF), Color32::from_rgb(10, 132, 255));
    }
}
