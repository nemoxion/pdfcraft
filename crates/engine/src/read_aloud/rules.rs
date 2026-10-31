//! The tunable parts of Read Out Loud's text rules: the thresholds that tell paragraphs, tables,
//! figures and running headers apart, and word lists added to the built-in ones. They come from a
//! TOML file the user may edit ([`Rules::from_toml`]), so the rules can be tried out on a document
//! without a rebuild. The file is untrusted: it is size-capped, every value is range-checked, and
//! an unknown key is an error rather than silently ignored, so a misspelt rule is noticed.

/// The most bytes a rules file may have.
pub const MAX_RULES_BYTES: usize = 64 * 1024;
/// The most entries a word list may have, and the most characters an entry may have.
const MAX_WORDS: usize = 256;
const MAX_WORD_CHARS: usize = 64;

/// Read Out Loud's tunable rules. Distances are in font sizes (ems) of the text concerned.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    /// `[paragraphs] gap`: a gap between rows this much wider than the page's typical line gap
    /// starts a paragraph.
    pub paragraph_gap: f64,
    /// `[paragraphs] indent_min`, `indent_max`: a row starting this far right of the row above
    /// is a first-line indent, which starts a paragraph.
    pub indent_min: f64,
    pub indent_max: f64,
    /// `[figures] cell_gap`: a gap between runs on a row this wide sets cells apart.
    pub cell_gap: f64,
    /// `[figures] label_width`: a row's first run this narrow is a label, whose gap after it
    /// doesn't count as a cell gap.
    pub label_width: f64,
    /// `[figures] sparse_density`: a row filled less than this (prose comes to about 1) is laid
    /// out like a figure or table.
    pub sparse_density: f64,
    /// `[figures] prose_density`: a row at least this full that reaches the column's right edge
    /// is prose, which ends a figure's or table's area.
    pub prose_density: f64,
    /// `[figures] margin`: a row this far in from both column edges is laid out like a figure;
    /// a row ending within this of the right edge reaches it.
    pub figure_margin: f64,
    /// `[figures] passed_rows`: how many rows in a row that look like prose a table's area may
    /// pass over (a long cell, a lone axis label).
    pub passed_rows: usize,
    /// `[headers] max_size`: a running header or footer is set no larger than this many times
    /// the body size.
    pub header_size: f64,
    /// `[headers] pages`: how many pages before and after are compared for a repeated row.
    pub header_pages: usize,
    /// `[headers] min_letters`: the fewest letters a repeated row needs.
    pub header_letters: usize,
    /// `[words] captions`: further words that open a caption, like "Figure" and "Table".
    pub captions: Vec<String>,
    /// `[words] compound_ends`: further second halves of compounds that keep their hyphen at a
    /// row's end, like "aware" in "context-aware".
    pub compound_ends: Vec<String>,
    /// `[words] bold_fonts`, `italic_fonts`: further parts of font names (any case) that mark a
    /// bold or italic font.
    pub bold_fonts: Vec<String>,
    pub italic_fonts: Vec<String>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            paragraph_gap: 0.35,
            indent_min: 0.8,
            indent_max: 6.0,
            cell_gap: 1.5,
            label_width: 3.0,
            sparse_density: 0.55,
            prose_density: 0.85,
            figure_margin: 3.0,
            passed_rows: 2,
            header_size: 1.1,
            header_pages: 2,
            header_letters: 3,
            captions: Vec::new(),
            compound_ends: Vec::new(),
            bold_fonts: Vec::new(),
            italic_fonts: Vec::new(),
        }
    }
}

impl Rules {
    /// The rules a TOML file sets, the others at their defaults. An error names the first
    /// problem: a file too large, a syntax error, an unknown section or key, or a value of the
    /// wrong type or out of range.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        if text.len() > MAX_RULES_BYTES {
            return Err(format!("the rules file is larger than {} KiB", MAX_RULES_BYTES / 1024));
        }
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string().trim().to_string())?;
        let mut r = Self::default();
        for (section, value) in &table {
            let Some(keys) = value.as_table() else { return Err(format!("{section} must be a section, [{section}]")) };
            for (key, v) in keys {
                let name = format!("{section}.{key}");
                match (section.as_str(), key.as_str()) {
                    ("paragraphs", "gap") => r.paragraph_gap = number(&name, v, 0.0, 5.0)?,
                    ("paragraphs", "indent_min") => r.indent_min = number(&name, v, 0.0, 20.0)?,
                    ("paragraphs", "indent_max") => r.indent_max = number(&name, v, 0.0, 50.0)?,
                    ("figures", "cell_gap") => r.cell_gap = number(&name, v, 0.1, 50.0)?,
                    ("figures", "label_width") => r.label_width = number(&name, v, 0.0, 50.0)?,
                    ("figures", "sparse_density") => r.sparse_density = number(&name, v, 0.0, 2.0)?,
                    ("figures", "prose_density") => r.prose_density = number(&name, v, 0.0, 2.0)?,
                    ("figures", "margin") => r.figure_margin = number(&name, v, 0.0, 50.0)?,
                    ("figures", "passed_rows") => r.passed_rows = count(&name, v, 0, 20)?,
                    ("headers", "max_size") => r.header_size = number(&name, v, 0.1, 10.0)?,
                    ("headers", "pages") => r.header_pages = count(&name, v, 1, 5)?,
                    ("headers", "min_letters") => r.header_letters = count(&name, v, 1, 200)?,
                    ("words", "captions") => r.captions = words(&name, v)?,
                    ("words", "compound_ends") => r.compound_ends = words(&name, v)?,
                    ("words", "bold_fonts") => r.bold_fonts = words(&name, v)?,
                    ("words", "italic_fonts") => r.italic_fonts = words(&name, v)?,
                    _ => return Err(format!("{name} is not a rule")),
                }
            }
        }
        if r.indent_min > r.indent_max {
            return Err(format!("paragraphs.indent_min ({}) is larger than paragraphs.indent_max ({})", r.indent_min, r.indent_max));
        }
        Ok(r)
    }

    /// A rules file to start from: every rule, commented out at its default.
    pub fn template() -> String {
        let d = Self::default();
        format!(
            "# Read Out Loud rules. Uncomment a line and change its value to try it; the app picks
# the change up within a second. Distances are in font sizes (ems).

[paragraphs]
# A gap between rows this much wider than the page's typical line gap starts a paragraph.
# gap = {}
# A row starting this far right of the row above is a first-line indent (a new paragraph).
# indent_min = {}
# indent_max = {}

[figures]
# A gap this wide between runs on a row sets table cells apart.
# cell_gap = {}
# A row's first run this narrow is a label: the gap after it isn't a cell gap.
# label_width = {}
# A row filled less than this (prose is about 1) is laid out like a figure or table.
# sparse_density = {}
# A row at least this full that reaches the column's right edge is prose: the table ends.
# prose_density = {}
# A row this far in from both column edges is laid out like a figure.
# margin = {}
# How many prose-like rows in a row a table may pass over (a long cell, an axis label).
# passed_rows = {}

[headers]
# A running header or footer is no larger than this many times the body size.
# max_size = {}
# How many pages before and after are compared for a repeated top or bottom row.
# pages = {}
# The fewest letters a repeated row needs.
# min_letters = {}

[words]
# Added to the built-in lists.
# captions = [\"Plate\", \"Diagram\"]
# compound_ends = [\"centric\"]
# bold_fonts = [\"heavy\"]
# italic_fonts = [\"slanted\"]
",
            d.paragraph_gap,
            d.indent_min,
            d.indent_max,
            d.cell_gap,
            d.label_width,
            d.sparse_density,
            d.prose_density,
            d.figure_margin,
            d.passed_rows,
            d.header_size,
            d.header_pages,
            d.header_letters
        )
    }
}

fn number(name: &str, v: &toml::Value, min: f64, max: f64) -> Result<f64, String> {
    let n = match v {
        toml::Value::Float(f) => *f,
        // Within the range checked below, so exact as an f64.
        toml::Value::Integer(i) if (-1_000_000..=1_000_000).contains(i) => *i as f64,
        _ => return Err(format!("{name} must be a number from {min} to {max}")),
    };
    if !n.is_finite() || n < min || n > max {
        return Err(format!("{name} must be a number from {min} to {max}"));
    }
    Ok(n)
}

fn count(name: &str, v: &toml::Value, min: usize, max: usize) -> Result<usize, String> {
    v.as_integer()
        .and_then(|i| usize::try_from(i).ok())
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| format!("{name} must be a whole number from {min} to {max}"))
}

fn words(name: &str, v: &toml::Value) -> Result<Vec<String>, String> {
    let Some(list) = v.as_array() else { return Err(format!("{name} must be a list of words, [\"…\", \"…\"]")) };
    if list.len() > MAX_WORDS {
        return Err(format!("{name} has more than {MAX_WORDS} entries"));
    }
    list.iter()
        .map(|w| {
            w.as_str()
                .map(str::trim)
                .filter(|w| !w.is_empty() && w.chars().count() <= MAX_WORD_CHARS)
                .map(str::to_string)
                .ok_or_else(|| format!("{name} entries must be words of 1 to {MAX_WORD_CHARS} characters"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_holds_the_defaults() {
        // Uncommenting every rule line of the template leaves the defaults, apart from the
        // example word lists.
        let template = Rules::template();
        let uncommented: String = template
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rule) if rule.contains(" = ") && !rule.contains('[') => rule,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(Rules::from_toml(&uncommented), Ok(Rules::default()));
        assert_eq!(Rules::from_toml(&template), Ok(Rules::default()));
        assert_eq!(Rules::from_toml(""), Ok(Rules::default()));
    }

    #[test]
    fn rules_are_read_and_bad_ones_are_named() {
        let r = Rules::from_toml("[figures]\ncell_gap = 2\nsparse_density = 0.6\n[words]\ncaptions = [\"Plate\"]\n").unwrap();
        assert_eq!((r.cell_gap, r.sparse_density, r.captions.as_slice()), (2.0, 0.6, ["Plate".to_string()].as_slice()));
        let err = |text: &str| Rules::from_toml(text).unwrap_err();
        assert!(err("[figures]\ncel_gap = 2").contains("figures.cel_gap is not a rule"));
        assert!(err("[figures]\ncell_gap = -1").contains("figures.cell_gap must be"));
        assert!(err("[figures]\ncell_gap = nan").contains("figures.cell_gap must be"));
        assert!(err("[figures]\ncell_gap = \"wide\"").contains("must be a number"));
        assert!(err("[headers]\npages = 1.5").contains("headers.pages must be a whole number"));
        assert!(err("[headers]\npages = -3").contains("headers.pages"));
        assert!(err("figures = 3").contains("must be a section"));
        assert!(err("[words]\ncaptions = [\"\"]").contains("words.captions"));
        assert!(err("[words]\ncaptions = [1]").contains("words.captions"));
        assert!(err("[paragraphs]\nindent_min = 5\nindent_max = 2").contains("larger than"));
        assert!(!err("[figures\n").is_empty(), "a syntax error is reported");
        let many = format!("[words]\ncaptions = [{}]", vec!["\"x\""; MAX_WORDS + 1].join(","));
        assert!(err(&many).contains("more than"));
        assert!(err(&"#".repeat(MAX_RULES_BYTES + 1)).contains("larger than"));
    }
}
