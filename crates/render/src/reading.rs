//! The "reading colours" view setting (Acrobat's Replace Document Colors): solid text and
//! line-art paints are remapped for readability while rendering. Display only — the document
//! itself never changes. The setting flows into [`hayro::RenderSettings`] through
//! `RenderConfig`.

/// The reading-colours setting for one document. `params: None` means off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadingColors {
    /// The colours and flags, or `None` when the setting is off.
    pub params: Option<ReadingParams>,
}

/// Colours and flags of the reading-colours mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadingParams {
    /// The sRGB colour that dark paints are mapped towards.
    pub text: [u8; 3],
    /// The sRGB colour that light paints are mapped towards.
    pub background: [u8; 3],
    /// When `true`, only near-black paints are mapped; lighter ones keep their colour.
    pub only_black: bool,
    /// When `true`, line-art paints are mapped in addition to text paints.
    pub line_art: bool,
}

impl ReadingColors {
    /// Black text on a white page.
    pub const BLACK_ON_WHITE: Self =
        Self { params: Some(ReadingParams { text: [0x00, 0x00, 0x00], background: [0xff, 0xff, 0xff], only_black: false, line_art: true }) };
    /// White text on a black page.
    pub const WHITE_ON_BLACK: Self =
        Self { params: Some(ReadingParams { text: [0xff, 0xff, 0xff], background: [0x00, 0x00, 0x00], only_black: false, line_art: true }) };
    /// Yellow text on a black page.
    pub const YELLOW_ON_BLACK: Self =
        Self { params: Some(ReadingParams { text: [0xff, 0xff, 0x00], background: [0x00, 0x00, 0x00], only_black: false, line_art: true }) };
    /// Green text on a black page.
    pub const GREEN_ON_BLACK: Self =
        Self { params: Some(ReadingParams { text: [0x00, 0xff, 0x00], background: [0x00, 0x00, 0x00], only_black: false, line_art: true }) };

    /// Map an unpremultiplied sRGB line-art paint with the same rules used for PDF rendering.
    pub fn map_line_art(self, rgba: [u8; 4]) -> [u8; 4] {
        self.params.map_or(rgba, |params| hayro::ColorReplacement::from(params).map(rgba, false))
    }

    /// Map an unpremultiplied sRGB text paint, including when line-art mapping is disabled.
    pub fn map_text(self, rgba: [u8; 4]) -> [u8; 4] {
        self.params.map_or(rgba, |params| hayro::ColorReplacement::from(params).map(rgba, true))
    }

    /// Parses tool/settings input. `mode` is one of `off | black-on-white | white-on-black |
    /// yellow-on-black | green-on-black | custom`; `custom` needs `text` and `background` as
    /// `#rrggbb`; the flags override the preset defaults when given. `off` ignores colours.
    /// Errors are actionable strings.
    pub fn parse(mode: &str, text: Option<&str>, background: Option<&str>, only_black: Option<bool>, line_art: Option<bool>) -> Result<Self, String> {
        // Every mode accepts the flags: they override the preset defaults, and are the custom
        // mode's own values.
        let with_flags = |mut colors: Self| {
            if let Some(p) = colors.params.as_mut() {
                if let Some(v) = only_black {
                    p.only_black = v;
                }
                if let Some(v) = line_art {
                    p.line_art = v;
                }
            }
            colors
        };
        match mode {
            "off" => Ok(Self::default()),
            "black-on-white" => Ok(with_flags(Self::BLACK_ON_WHITE)),
            "white-on-black" => Ok(with_flags(Self::WHITE_ON_BLACK)),
            "yellow-on-black" => Ok(with_flags(Self::YELLOW_ON_BLACK)),
            "green-on-black" => Ok(with_flags(Self::GREEN_ON_BLACK)),
            "custom" => match (text, background) {
                (None, None) => Err("custom needs text and background as #rrggbb".into()),
                (None, Some(_)) => Err("custom needs text as #rrggbb".into()),
                (Some(_), None) => Err("custom needs background as #rrggbb".into()),
                (Some(text), Some(background)) => Ok(with_flags(Self {
                    params: Some(ReadingParams {
                        text: parse_hex("text", text)?,
                        background: parse_hex("background", background)?,
                        only_black: false,
                        line_art: true,
                    }),
                })),
            },
            other => {
                Err(format!("unknown mode {other:?}; expected one of: off, black-on-white, white-on-black, yellow-on-black, green-on-black, custom"))
            }
        }
    }

    /// The mode name for echoes and display: `"off"`, a preset name, or `"custom"`.
    pub fn mode_name(&self) -> &'static str {
        match self.params {
            None => "off",
            Some(params) => {
                if Some(params) == Self::BLACK_ON_WHITE.params {
                    "black-on-white"
                } else if Some(params) == Self::WHITE_ON_BLACK.params {
                    "white-on-black"
                } else if Some(params) == Self::YELLOW_ON_BLACK.params {
                    "yellow-on-black"
                } else if Some(params) == Self::GREEN_ON_BLACK.params {
                    "green-on-black"
                } else {
                    "custom"
                }
            }
        }
    }

    /// The text colour as lowercase `#rrggbb`, or `None` when off.
    pub fn text_hex(&self) -> Option<String> {
        self.params.map(|p| hex(p.text))
    }

    /// The background colour as lowercase `#rrggbb`, or `None` when off.
    pub fn background_hex(&self) -> Option<String> {
        self.params.map(|p| hex(p.background))
    }
}

impl From<ReadingParams> for hayro::ColorReplacement {
    fn from(p: ReadingParams) -> Self {
        hayro::ColorReplacement { text: p.text, background: p.background, only_black: p.only_black, line_art: p.line_art }
    }
}

/// Parses a colour given as `#rrggbb` — exactly this shape, either case of digits. The error
/// names the argument and the expected shape.
fn parse_hex(name: &str, value: &str) -> Result<[u8; 3], String> {
    let invalid = || format!("invalid {name} colour {value:?}; expected #rrggbb (six hex digits, e.g. #00ff00)");
    let Some(digits) = value.strip_prefix('#') else { return Err(invalid()) };
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let mut rgb = [0u8; 3];
    for (i, channel) in rgb.iter_mut().enumerate() {
        // Length and charset are checked above, so the parse cannot actually fail; the mapping
        // only keeps it total without panicking.
        *channel = u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).map_err(|_| invalid())?;
    }
    Ok(rgb)
}

/// Formats sRGB bytes as lowercase `#rrggbb`.
fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// Display-only dark-page filter on one unpremultiplied sRGBA pixel.
/// Exchanges light and dark luminance while keeping hues recognisable and alpha unchanged.
/// This is shared by the GUI and headless previews; never apply it to saved or printed PDFs.
pub fn dark_page_rgba([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    let invert = |channel: u8| 0.9 - 0.8 * (f32::from(channel) / 255.0);
    let [r, g, b] = [invert(r), invert(g), invert(b)];
    // CSS hue-rotate(180deg), using the filter function's luminance coefficients.
    let rotated = [-0.574 * r + 1.430 * g + 0.144 * b, 0.426 * r + 0.430 * g + 0.144 * b, 0.426 * r + 1.430 * g - 0.856 * b];
    let gray = 0.213 * rotated[0] + 0.715 * rotated[1] + 0.072 * rotated[2];
    let byte = |channel: f32| ((0.4 * channel + 0.6 * gray).clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(rotated[0]), byte(rotated[1]), byte(rotated[2]), a]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_every_mode() {
        let off = ReadingColors::parse("off", None, None, None, None).unwrap();
        assert_eq!(off, ReadingColors::default());
        assert_eq!(off.mode_name(), "off");
        assert_eq!(off.text_hex(), None);
        assert_eq!(off.background_hex(), None);
        // `off` ignores colour and flag arguments.
        assert_eq!(ReadingColors::parse("off", Some("zzz"), Some("#000000"), Some(true), Some(false)).unwrap(), off);

        for (mode, text, background) in [
            ("black-on-white", "#000000", "#ffffff"),
            ("white-on-black", "#ffffff", "#000000"),
            ("yellow-on-black", "#ffff00", "#000000"),
            ("green-on-black", "#00ff00", "#000000"),
        ] {
            let colors = ReadingColors::parse(mode, None, None, None, None).unwrap();
            assert_eq!(colors.mode_name(), mode, "{mode}");
            assert_eq!(colors.text_hex().as_deref(), Some(text), "{mode}");
            assert_eq!(colors.background_hex().as_deref(), Some(background), "{mode}");
            let params = colors.params.unwrap();
            assert!(!params.only_black && params.line_art, "{mode}");
        }

        // The flags override the preset defaults; a changed preset is no longer one.
        let overridden = ReadingColors::parse("white-on-black", None, None, Some(true), Some(false)).unwrap();
        let params = overridden.params.unwrap();
        assert!(params.only_black && !params.line_art);
        assert_eq!(overridden.mode_name(), "custom");

        // `custom` takes either case of hex and echoes lowercase.
        let custom = ReadingColors::parse("custom", Some("#AaBbCc"), Some("#123456"), Some(true), None).unwrap();
        assert_eq!(custom.mode_name(), "custom");
        assert_eq!(custom.text_hex().as_deref(), Some("#aabbcc"));
        assert_eq!(custom.background_hex().as_deref(), Some("#123456"));
        let params = custom.params.unwrap();
        assert!(params.only_black && params.line_art);
    }

    #[test]
    fn parse_rejects_unknown_mode() {
        assert_eq!(
            ReadingColors::parse("nope", None, None, None, None),
            Err("unknown mode \"nope\"; expected one of: off, black-on-white, white-on-black, yellow-on-black, green-on-black, custom".to_string())
        );
    }

    #[test]
    fn parse_rejects_malformed_hex() {
        for bad in ["zzz", "000000", "#000", "#0000000", "#00g000", ""] {
            assert_eq!(
                ReadingColors::parse("custom", Some(bad), Some("#000000"), None, None),
                Err(format!("invalid text colour {bad:?}; expected #rrggbb (six hex digits, e.g. #00ff00)")),
                "{bad}"
            );
        }
        assert_eq!(
            ReadingColors::parse("custom", Some("#000000"), Some("ffffff"), None, None),
            Err("invalid background colour \"ffffff\"; expected #rrggbb (six hex digits, e.g. #00ff00)".to_string())
        );
    }

    #[test]
    fn parse_rejects_custom_missing_colours() {
        assert_eq!(ReadingColors::parse("custom", None, None, None, None), Err("custom needs text and background as #rrggbb".to_string()));
        assert_eq!(ReadingColors::parse("custom", None, Some("#000000"), None, None), Err("custom needs text as #rrggbb".to_string()));
        assert_eq!(ReadingColors::parse("custom", Some("#000000"), None, None, None), Err("custom needs background as #rrggbb".to_string()));
    }

    #[test]
    fn dark_page_filter_exchanges_luminance_keeps_hue_and_alpha() {
        let [r, g, b, a] = dark_page_rgba([0xff, 0xff, 0xff, 0xff]);
        assert!(r < 40 && r == g && g == b && a == 0xff, "white paper turns dark gray: {r} {g} {b} {a}");
        let [r, g, b, _] = dark_page_rgba([0x00, 0x00, 0x00, 0xff]);
        assert!(r > 215 && r == g && g == b, "black ink turns light gray: {r} {g} {b}");
        assert_eq!(dark_page_rgba([0xff, 0xff, 0xff, 0x00])[3], 0, "alpha is unchanged");
        assert_eq!(dark_page_rgba([0x00, 0x00, 0x00, 0x80])[3], 0x80, "alpha is unchanged");
        let [r, g, b, _] = dark_page_rgba([0xff, 0x00, 0x00, 0xff]);
        assert!(r > g && r > b, "hue rotation keeps red recognisable: {r} {g} {b}");
    }

    #[test]
    fn paint_mapping_follows_the_preset_and_its_flags() {
        let black = [0x00, 0x00, 0x00, 0xff];
        let red = [0xff, 0x00, 0x00, 0xff];
        let off = ReadingColors::default();
        assert_eq!(off.map_text(black), black, "off leaves text alone");
        assert_eq!(off.map_line_art(red), red, "off leaves line art alone");
        let dark = ReadingColors::WHITE_ON_BLACK;
        assert_eq!(dark.map_text(black), [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(dark.map_line_art(black), [0xff, 0xff, 0xff, 0xff]);
        let no_line_art = ReadingColors::parse("white-on-black", None, None, Some(false), Some(false)).unwrap();
        assert_eq!(no_line_art.map_line_art(black), black, "line-art opt-out");
        assert_eq!(no_line_art.map_text(black), [0xff, 0xff, 0xff, 0xff], "text is still mapped without line art");
        let only_black = ReadingColors::parse("white-on-black", None, None, Some(true), Some(true)).unwrap();
        assert_eq!(only_black.map_line_art(red), red, "only-black keeps coloured paints");
        assert_eq!(only_black.map_text(black), [0xff, 0xff, 0xff, 0xff]);
    }
}
