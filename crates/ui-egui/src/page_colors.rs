//! Display colours shared by page textures and live drawing previews.

use egui::Color32;
use pdfcraft_render::{PRINT_TAG, ReadingColors};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageColors {
    Original,
    Dark,
    /// The renderer already applies this preset to raster pixels.
    Reading(ReadingColors),
}

impl PageColors {
    /// An explicit reading-colour preset wins; the saved dark-page choice resumes when it is off.
    pub(crate) fn for_reading(dark: bool, reading: ReadingColors) -> Self {
        if reading.params.is_some() {
            Self::Reading(reading)
        } else if dark {
            Self::Dark
        } else {
            Self::Original
        }
    }

    pub(crate) fn for_request(dark: bool, reading: ReadingColors, tag: u64) -> Self {
        if tag & PRINT_TAG != 0 { Self::Original } else { Self::for_reading(dark, reading) }
    }

    pub(crate) fn raster(self, color: Color32) -> Color32 {
        if self == Self::Dark { dark_page_color(color) } else { color }
    }

    /// Map an unrendered drawing stroke, using the renderer's own preset mapping.
    pub(crate) fn line_art(self, color: Color32) -> Color32 {
        match self {
            Self::Reading(reading) => {
                let [r, g, b, a] = reading.map_line_art(color.to_srgba_unmultiplied());
                Color32::from_rgba_unmultiplied(r, g, b, a)
            }
            _ => self.raster(color),
        }
    }

    /// Map live text independently of the preset's line-art opt-out.
    pub(crate) fn text(self, color: Color32) -> Color32 {
        match self {
            Self::Reading(reading) => {
                let [r, g, b, a] = reading.map_text(color.to_srgba_unmultiplied());
                Color32::from_rgba_unmultiplied(r, g, b, a)
            }
            _ => self.raster(color),
        }
    }

    /// Raw images keep their colours under reading presets; dark pages filter all pixels.
    pub(crate) fn image(self, mut image: egui::ColorImage) -> egui::ColorImage {
        if self == Self::Dark {
            image.pixels.iter_mut().for_each(|pixel| *pixel = self.raster(*pixel));
        }
        image
    }

    /// Editor surfaces follow the page paper even when a preset only replaces black paints.
    pub(crate) fn editor_paper(self, original: Color32) -> Color32 {
        if matches!(self, Self::Reading(_)) {
            let paper = self.paper();
            Color32::from_rgba_unmultiplied(paper.r(), paper.g(), paper.b(), original.a())
        } else {
            self.raster(original)
        }
    }

    pub(crate) fn paper(self) -> Color32 {
        match self {
            Self::Reading(ReadingColors { params: Some(p) }) => Color32::from_rgb(p.background[0], p.background[1], p.background[2]),
            _ => self.raster(Color32::WHITE),
        }
    }

    pub(crate) fn loading_color(self, fallback: Color32) -> Color32 {
        match self {
            Self::Reading(ReadingColors { params: Some(p) }) => Color32::from_rgb(p.text[0], p.text[1], p.text[2]),
            Self::Dark => Color32::from_gray(190),
            _ => fallback,
        }
    }
}

/// Reading filter that keeps hues recognisable while exchanging light and dark luminance.
fn dark_page_color(pixel: Color32) -> Color32 {
    let [r, g, b, a] = pdfcraft_render::dark_page_rgba(pixel.to_srgba_unmultiplied());
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

#[cfg(test)]
pub(crate) fn uploaded_textures(ctx: &egui::Context, textures: &[&egui::TextureHandle]) -> Vec<Vec<Color32>> {
    let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
    let mut uploads = std::mem::take(&mut output.textures_delta.set);
    // Assert only after clearing deltas: egui's Drop asserts too, even during unwinding.
    output.textures_delta.clear();
    textures
        .iter()
        .map(|texture| {
            let deltas = uploads.remove(&texture.id()).expect("texture upload");
            match &deltas.last().expect("image delta").image {
                egui::ImageData::Color(image) => image.pixels.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn uploaded_pixels(ctx: &egui::Context, texture: &egui::TextureHandle) -> Vec<Color32> {
    uploaded_textures(ctx, &[texture]).pop().expect("one requested texture")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_texture_upload_panics_once_without_aborting_cleanup() {
        let ctx = egui::Context::default();
        let old = ctx.load_texture("old", egui::ColorImage::filled([1, 1], Color32::WHITE), egui::TextureOptions::LINEAR);
        assert_eq!(uploaded_pixels(&ctx, &old), [Color32::WHITE]);
        let _pending = ctx.load_texture("pending", egui::ColorImage::filled([1, 1], Color32::BLACK), egui::TextureOptions::LINEAR);
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| uploaded_pixels(&ctx, &old)));
        assert!(failure.is_err());
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
    }

    #[test]
    fn reading_presets_take_precedence_and_prints_always_use_original_colours() {
        for reading in [ReadingColors::WHITE_ON_BLACK, ReadingColors::YELLOW_ON_BLACK, ReadingColors::BLACK_ON_WHITE] {
            let colors = PageColors::for_reading(true, reading);
            assert_eq!(colors.raster(Color32::BLACK), Color32::BLACK, "already rendered pixels are not mapped twice");
            assert_eq!(colors.line_art(Color32::BLACK).to_array()[..3], reading.params.unwrap().text);
            assert_eq!(colors.paper().to_array()[..3], reading.params.unwrap().background);
            assert_eq!(PageColors::for_request(true, reading, PRINT_TAG), PageColors::Original);
        }
        assert_eq!(PageColors::for_reading(true, ReadingColors::default()), PageColors::Dark);
        let flags = ReadingColors::parse("white-on-black", None, None, Some(true), Some(false)).unwrap();
        let colors = PageColors::for_reading(true, flags);
        assert_eq!(colors.line_art(Color32::BLACK), Color32::BLACK, "honour line-art opt-out");
        assert_eq!(colors.text(Color32::BLACK), Color32::WHITE, "text is still mapped");
        assert_eq!(colors.editor_paper(Color32::WHITE), Color32::BLACK, "editor paper follows the preset even with only-black and line-art opt-out");
        let flags = ReadingColors::parse("white-on-black", None, None, Some(true), Some(true)).unwrap();
        assert_eq!(PageColors::for_reading(true, flags).line_art(Color32::RED), Color32::RED, "honour only-black mapping");
    }
}
