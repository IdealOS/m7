//! The UI font, built into the binary.
//!
//! Inter (variable, so every weight from one file) is the sans-serif face on every
//! target, so text looks the same hosted on the Mac, in QEMU and on the Pi, and the
//! compositor doesn't depend on the image's font packages. System fonts are still
//! loaded as fallbacks for scripts Inter doesn't cover.
//!
//! Inter is under the SIL Open Font License 1.1: `fonts/Inter-LICENSE.txt`.

use std::sync::Arc;

use glyphon::FontSystem;
use glyphon::fontdb::{Database, Source};

const INTER: &[u8] = include_bytes!("../fonts/InterVariable.ttf");
/// The family name inside the font file.
pub const UI_FAMILY: &str = "Inter Variable";

/// A font system with the system fonts plus Inter as the sans-serif family.
pub fn system() -> FontSystem {
    let mut fonts = FontSystem::new();
    add_ui_font(fonts.db_mut());
    fonts
}

/// Load Inter into `db` and make it what `Family::SansSerif` resolves to.
fn add_ui_font(db: &mut Database) {
    // A borrowed static slice, so the 880 KB font isn't copied.
    db.load_font_source(Source::Binary(Arc::new(INTER)));
    db.set_sans_serif_family(UI_FAMILY);
}

#[cfg(test)]
mod tests {
    use glyphon::{Attrs, Buffer, Family, Metrics, Shaping, Weight};

    use super::*;

    /// The family of every glyph in `text`, shaped as sans-serif at `weight`.
    fn families(fonts: &mut FontSystem, text: &str, weight: Weight) -> Vec<String> {
        let mut buffer = Buffer::new(fonts, Metrics::new(16.0, 20.0));
        buffer.set_size(Some(400.0), None);
        let attrs = Attrs::new().family(Family::SansSerif).weight(weight);
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(fonts, false);
        let ids: Vec<_> = buffer.layout_runs().flat_map(|run| run.glyphs.iter().map(|g| g.font_id)).collect();
        ids.iter().map(|id| fonts.db().face(*id).map_or("?".into(), |f| f.families[0].0.clone())).collect()
    }

    #[test]
    fn sans_serif_is_inter_at_the_weights_the_renderer_uses() {
        let mut fonts = system();
        for weight in [Weight::NORMAL, Weight::SEMIBOLD] {
            let found = families(&mut fonts, "Mail · 3 unread", weight);
            assert!(!found.is_empty());
            assert!(found.iter().all(|f| f == UI_FAMILY), "weight {}: {found:?}", weight.0);
        }
    }

    /// Regression: with no fonts installed (a bare image), text used to have nothing
    /// to shape with. The bundled font alone must be enough.
    #[test]
    fn text_shapes_with_no_system_fonts() {
        let mut db = Database::new();
        add_ui_font(&mut db);
        let mut fonts = FontSystem::new_with_locale_and_db("en-US".into(), db);
        let found = families(&mut fonts, "Settings", Weight::NORMAL);
        assert_eq!(found.len(), "Settings".len());
        assert!(found.iter().all(|f| f == UI_FAMILY), "{found:?}");
    }
}
