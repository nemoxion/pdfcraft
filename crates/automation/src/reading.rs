//! Reading colours: Acrobat's Replace Document Colors as a tool (#897, AGENTS.md §3).
//!
//! A per-document view setting: solid text and line-art paints remap for readability and the
//! page clears to the setting's background when rendering. The file itself never changes.

use pdfcraft_render::ReadingColors;
use serde_json::{Value, json};

use crate::{Args, Automation, Result, ToolError};

/// The modes `ReadingColors::parse` accepts, for error messages.
const MODES: &str = "off, black-on-white, white-on-black, yellow-on-black, green-on-black, custom";

impl Automation {
    /// Set the document's reading colours. Colours are only meaningful with `mode: "custom"`;
    /// `parse` enforces the rest and its errors are actionable.
    pub(crate) fn view_reading_colors(&mut self, a: &Args) -> Result<Value> {
        let id = self.doc(a)?.id;
        let Some(mode) = a.opt_str("mode")? else {
            return Err(ToolError::InvalidArgs(format!("mode is required (one of: {MODES})")));
        };
        let colors = ReadingColors::parse(mode, a.opt_str("text")?, a.opt_str("background")?, a.opt_bool("only_black")?, a.opt_bool("line_art")?)
            .map_err(ToolError::InvalidArgs)?;
        if self.session.set_reading_colors(id, colors) {
            // Cached renderers baked the old setting into their config; page_render must re-read it.
            self.renderers.remove(&id);
        }
        Ok(json!({
            "mode": colors.mode_name(),
            "text": colors.text_hex(),
            "background": colors.background_hex(),
            "only_black": colors.params.is_some_and(|p| p.only_black),
            "line_art": colors.params.is_some_and(|p| p.line_art),
        }))
    }
}

/// The most paragraphs one `read_aloud_passages` call lists.
const MAX_PASSAGES: usize = 500;

impl Automation {
    /// View ▸ Read Out Loud without the speech: what would be read, in order and in the pieces a
    /// speech server is sent, from `paragraph` (1-based, default 1) of `page` to the end of the
    /// page, or with `to_end` of the document. At most `limit` paragraphs (default and cap
    /// [`MAX_PASSAGES`]); `next` says where to continue. The listing is text an agent can copy,
    /// so unlike reading aloud on screen it needs the copy permission.
    pub(crate) fn read_aloud_passages(&mut self, a: &Args) -> Result<Value> {
        let page = self.page(a)?;
        let doc = self.doc(a)?;
        doc.check_copying().map_err(|e| ToolError::Failed(e.to_string()))?;
        let from = match a.opt_int("paragraph")? {
            None => 0,
            Some(p) if p >= 1 => usize::try_from(p - 1).unwrap_or(usize::MAX),
            Some(p) => return Err(ToolError::InvalidArgs(format!("paragraph {p} is out of range: paragraphs are numbered from 1"))),
        };
        let limit = match a.opt_int("limit")? {
            None => MAX_PASSAGES,
            Some(l) if (1..=MAX_PASSAGES as i64).contains(&l) => l as usize,
            Some(l) => return Err(ToolError::InvalidArgs(format!("limit {l} is out of range: 1 to {MAX_PASSAGES}"))),
        };
        let filters = pdfcraft_engine::read_aloud::Filters {
            page_numbers: a.opt_bool("skip_page_numbers")?.unwrap_or(true),
            bullets: a.opt_bool("skip_bullets")?.unwrap_or(true),
            equations: a.opt_bool("skip_equations")?.unwrap_or(true),
            numbers: a.opt_bool("skip_numbers")?.unwrap_or(true),
            citations: a.opt_bool("skip_citations")?.unwrap_or(true),
            figures: a.opt_bool("skip_figures")?.unwrap_or(true),
            captions: a.opt_bool("skip_captions")?.unwrap_or(false),
            headers: a.opt_bool("skip_headers")?.unwrap_or(true),
        };
        let rules = match a.opt_str("rules")? {
            Some(text) => pdfcraft_engine::read_aloud::Rules::from_toml(text).map_err(|e| ToolError::InvalidArgs(format!("rules: {e}")))?,
            None => pdfcraft_engine::read_aloud::Rules::default(),
        };
        let last = if a.opt_bool("to_end")?.unwrap_or(false) { doc.info.pages.len().saturating_sub(1) } else { page };
        let r = |x: f32| (f64::from(x) * 100.0).round() / 100.0;
        let mut out = Vec::new();
        let mut next = None;
        'pages: for p in page..=last {
            let Some(info) = doc.info.pages.get(p) else { break };
            // Paragraphs are numbered by their place on the page.
            for (k, passage) in doc.read_aloud_passages_with(p, filters, &rules).into_iter().enumerate().filter(|(k, _)| p != page || *k >= from) {
                if out.len() == limit {
                    next = Some(json!({ "page": p + 1, "paragraph": k + 1 }));
                    break 'pages;
                }
                let b = passage.rect;
                let (u, v) = (info.user_to_view(b[0] as f32, b[1] as f32), info.user_to_view(b[2] as f32, b[3] as f32));
                out.push(json!({
                    "page": p + 1,
                    "paragraph": k + 1,
                    "heading": passage.heading,
                    "role": match passage.role {
                        pdfcraft_engine::read_aloud::Role::Text => "text",
                        pdfcraft_engine::read_aloud::Role::Caption => "caption",
                        pdfcraft_engine::read_aloud::Role::FigureText => "figure_text",
                    },
                    "rect": [r(u[0].min(v[0])), r(u[1].min(v[1])), r(u[0].max(v[0])), r(u[1].max(v[1]))],
                    "chunks": pdfcraft_engine::read_aloud::speech_chunks(&passage.text),
                    "text": passage.text,
                }));
            }
        }
        Ok(json!({ "count": out.len(), "passages": out, "next": next }))
    }
}
