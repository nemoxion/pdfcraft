//! Cached page/image layers: a signature follows each pointer frame without editing the PDF.

use crate::preview_raster::PreviewRaster;
use egui::{ColorImage, TextureHandle, TextureOptions};
use pdfcraft_engine::{Document, Edit};

use crate::QuickTool;
use crate::comments::{CommentView, Gesture, PageCx};

#[derive(Default)]
pub(crate) struct SignatureDrag {
    key: Option<(u64, usize, usize)>,
    colors: Option<crate::page_colors::PageColors>,
    aspect_ratio: Option<f32>,
    layers: Option<Layers>,
}

struct Layers {
    image: TextureHandle,
    source: pdfcraft_engine::SignatureImage,
    opacity: f32,
    /// The page /Rotate the PDF draws the image turned back by.
    turn: i64,
    background: PreviewRaster,
    /// Keep the final position visible until the normal page raster catches up.
    settling: bool,
}

impl SignatureDrag {
    pub fn prepare(&mut self, ctx: &egui::Context, doc: &Document, selected: Option<(usize, usize)>, scale: f32, dark: bool) {
        let key = selected.map(|(p, i)| (doc.edit_generation(), p, i));
        let colors = crate::page_colors::PageColors::for_reading(dark, doc.renderer.reading_colors());
        if self.key != key {
            self.colors = Some(colors);
            self.key = key;
            self.aspect_ratio = None;
            self.layers = None;
            if let Some((_, page, index)) = key
                && doc.info.annotations.iter().any(|a| a.page == page && a.index == index && a.subtype == "Stamp" && !a.locked)
                && let Ok(Some(preview)) = doc.image_signature_preview(page, index)
            {
                let [width, height] = preview.image.size();
                let ratio = width as f32 / height as f32;
                // As displayed, the image is turned by the page's rotation less the turn it is drawn back by.
                let shown = doc.info.pages.get(page).map_or(0, |p| i64::from(p.rotation)) - preview.turn;
                self.aspect_ratio = Some(if shown.rem_euclid(180) == 0 { ratio } else { ratio.recip() });
                self.layers = Some(Layers {
                    opacity: preview.opacity,
                    turn: preview.turn,
                    image: ctx.load_texture(
                        "signature-drag-image",
                        colors.image(ColorImage::from_rgba_unmultiplied(preview.image.size(), preview.image.rgba())),
                        TextureOptions::LINEAR,
                    ),
                    background: PreviewRaster::new(preview.renderer()),
                    source: preview.image,
                    settling: false,
                });
            }
        }
        let recolor = self.colors != Some(colors);
        self.colors = Some(colors);
        let (Some((_, page, _)), Some(layers)) = (self.key, self.layers.as_mut()) else { return };
        if recolor {
            layers.image = ctx.load_texture(
                "signature-drag-image",
                colors.image(ColorImage::from_rgba_unmultiplied(layers.source.size(), layers.source.rgba())),
                TextureOptions::LINEAR,
            );
        }
        let Some(p) = doc.info.pages.get(page) else { return };
        let max_side = ctx.input(|i| i.max_texture_side) as f32;
        let scale = scale.min(crate::canvas::BASE_SIDE.min(max_side) / p.width.max(p.height).max(1.0));
        layers.background.prepare(ctx, page, scale, colors, true);
    }

    pub fn pending(&self) -> bool {
        self.layers.as_ref().is_some_and(|layers| layers.background.pending())
    }

    pub fn contains(&self, page: usize, index: usize) -> bool {
        self.key.is_some_and(|(_, p, i)| (p, i) == (page, index)) && self.layers.is_some()
    }

    /// Use the embedded image's proportions, even after an edge handle stretched its rectangle: its
    /// width over its height as the page is displayed (before the view's own rotation).
    pub fn aspect_ratio(&self, page: usize, index: usize) -> Option<f32> {
        self.key.filter(|(_, p, i)| (*p, *i) == (page, index)).and(self.aspect_ratio)
    }

    /// Movement/resizing only change this signature's geometry, so its background is reusable.
    pub fn committed(&mut self, edit: &Edit, generation: u64) {
        if let Edit::MoveAnnotation { page, index, .. } | Edit::ResizeAnnotation { page, index, .. } = edit
            && self.contains(*page, *index)
        {
            self.key = Some((generation, *page, *index));
            if let Some(layers) = &mut self.layers {
                layers.settling = true;
            }
        } else {
            *self = Self::default();
        }
    }

    pub fn page_received(&mut self, page: usize) {
        if self.key.is_some_and(|(_, p, _)| p == page)
            && let Some(layers) = &mut self.layers
        {
            layers.settling = false;
        }
    }

    pub fn paint(&self, painter: &egui::Painter, cx: &PageCx<'_>, cv: &CommentView, pending: Option<&Edit>) {
        let (Some((_, page, index)), Some(layers)) = (self.key, &self.layers) else { return };
        if page != cx.page || !matches!(cx.tool, QuickTool::Select | QuickTool::Fill(_)) || cx.hidden {
            return;
        }
        let dragging = matches!(cv.gesture, Some(Gesture::Move { page: p, index: i, .. } | Gesture::Resize { page: p, index: i, .. }) if (p, i) == (page, index));
        let released = matches!(pending, Some(Edit::MoveAnnotation { page: p, index: i, .. } | Edit::ResizeAnnotation { page: p, index: i, .. }) if (*p, *i) == (page, index));
        if !(dragging || released || layers.settling) {
            return;
        }
        let (Some(background), Some(a), Some(p)) = (layers.background.texture(), cx.get(index), cx.info.pages.get(page)) else { return };
        let (pointer, shift) = painter.ctx().input(|i| (i.pointer.hover_pos(), i.modifiers.shift));
        let rect = cx.adjusted_rect(a, cv.gesture.as_ref(), pointer, shift, pending);
        let rect = cx.rect_to_user(rect);
        let painter = painter.with_clip_rect(painter.clip_rect().intersect(cx.xf.rect));
        cx.xf.paint_image(&painter, background.id(), 0.0, 0.0, 1.0, 1.0);
        cx.xf.paint_user_image(
            &painter,
            layers.image.id(),
            p,
            rect,
            layers.turn,
            egui::Color32::from_white_alpha((layers.opacity * 255.0).round() as u8),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_colors::PageColors;

    #[test]
    fn dark_pages_signature_layers_refresh_without_document_edits() {
        let ctx = egui::Context::default();
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(12, 4, image::Rgba([0, 0, 0, 255])).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let image = pdfcraft_engine::SignatureImage::from_bytes(png.get_ref()).unwrap();
        let mut session = pdfcraft_engine::Session::new();
        let blank = session.create_blank(100.0, 100.0, 1).unwrap();
        let id = session.open_new("signature.pdf", blank).unwrap();
        let page = session.get(id).unwrap().info.pages[0].clone();
        session.apply(id, image.edit(0, &page, [10.0, 50.0], false, "Test").unwrap()).unwrap();
        let doc = session.get(id).unwrap();
        let source = doc.bytes.clone();
        let generation = doc.edit_generation();
        let mut preview = SignatureDrag::default();
        preview.prepare(&ctx, doc, Some((0, 0)), 1.0, false);
        let decoded = preview.layers.as_ref().unwrap().source.clone();
        for (requests, dark) in [false, true, false, true].into_iter().enumerate() {
            for _ in 0..100 {
                preview.prepare(&ctx, doc, Some((0, 0)), 1.0, dark);
                if !preview.pending() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let layers = preview.layers.as_ref().unwrap();
            assert_eq!(layers.source.rgba().as_ptr(), decoded.rgba().as_ptr(), "dark toggles reuse decoded image bytes");
            assert_eq!(layers.background.stats().render_requests, requests as u64 + 1, "background renderer survives toggles");
            let colors = PageColors::for_reading(dark, Default::default());
            let images = crate::page_colors::uploaded_textures(&ctx, &[&layers.image, layers.background.texture().unwrap()]);
            assert!(images[0].iter().all(|pixel| *pixel == colors.raster(egui::Color32::BLACK)));
            assert!(images[1].iter().all(|pixel| *pixel == colors.paper()));
            assert_eq!(doc.bytes, source);
            assert_eq!(doc.edit_generation(), generation);
            assert_eq!(doc.can_undo(), Some("Add signature"));
        }
        let edit = Edit::MoveAnnotation { page: 0, index: 0, dx: 5.0, dy: 0.0 };
        session.apply(id, edit.clone()).unwrap();
        let doc = session.get(id).unwrap();
        preview.committed(&edit, doc.edit_generation());
        preview.prepare(&ctx, doc, Some((0, 0)), 1.0, false);
        let layers = preview.layers.as_ref().unwrap();
        assert!(layers.settling, "a dark toggle preserves the released gesture until the main page arrives");
        assert_eq!(layers.source.rgba().as_ptr(), decoded.rgba().as_ptr());
        preview.page_received(0);
        assert!(!preview.layers.as_ref().unwrap().settling);
        preview.prepare(&ctx, doc, None, 1.0, false);
        assert!(preview.layers.is_none(), "deselection releases prepared layers");
    }
}
