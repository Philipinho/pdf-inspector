//! Formula/math region detection via font names, Unicode ranges, and sub/super density.

use crate::types::{BBox, ItemType, LayoutRegion, RegionType, TextItem};

/// Known math font prefixes (case-insensitive matching).
const MATH_FONT_PREFIXES: &[&str] = &[
    "cmmi",
    "cmsy",
    "cmr",
    "cmex",
    "cmbx",
    "cmti", // Computer Modern
    "stix",
    "stixmath", // STIX
    "mathjax",  // MathJax
    "cambria math",
    "cambriamath", // Cambria Math
    "asana-math",
    "asanamath",         // Asana Math
    "latin modern math", // Latin Modern
    "xits math",
    "xitsmath", // XITS
    "mt extra", // MT Extra
];

/// Check if a font name indicates a math font.
fn is_math_font(font_name: &str) -> bool {
    let lower = font_name.to_ascii_lowercase();
    // Check known prefixes
    if MATH_FONT_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    // Generic: font name contains "math" (but not just "Mathilda" etc.)
    if lower.contains("math") {
        return true;
    }
    false
}

/// Check if a character is in a math-related Unicode range.
fn is_math_char(c: char) -> bool {
    matches!(c,
        // Mathematical Operators
        '\u{2200}'..='\u{22FF}' |
        // Supplemental Math Operators
        '\u{2A00}'..='\u{2AFF}' |
        // Miscellaneous Mathematical Symbols A & B
        '\u{27C0}'..='\u{27EF}' |
        '\u{2980}'..='\u{29FF}' |
        // Mathematical Alphanumeric Symbols
        '\u{1D400}'..='\u{1D7FF}' |
        // Arrows (often in math)
        '\u{2190}'..='\u{21FF}' |
        // Superscript/subscript block
        '\u{2070}'..='\u{209F}' |
        // Standalone math symbols not covered by ranges above
        '\u{00B1}' | // ±
        '\u{00D7}' | // ×
        '\u{00F7}' | // ÷
        // Greek letters (often used as math variables)
        '\u{0391}'..='\u{03C9}'
    )
}

/// Returns true if the item has math signals (font or content).
fn has_math_signal(item: &TextItem) -> bool {
    if is_math_font(&item.font) {
        return true;
    }
    // Check if a significant portion of characters are math
    let total = item.text.chars().count();
    if total == 0 {
        return false;
    }
    let math_count = item.text.chars().filter(|&c| is_math_char(c)).count();
    // For short items, even 1 math char counts. For longer, require 30%+.
    if total <= 3 {
        math_count >= 1
    } else {
        math_count as f32 / total as f32 >= 0.3
    }
}

/// Detect formula regions on a single page.
///
/// Groups adjacent items with math signals (font, Unicode, sub/super) into
/// formula regions. Requires a cluster of 3+ items to avoid false positives
/// from isolated math symbols in prose.
pub(crate) fn detect_formula_regions(page_items: &[TextItem], page: u32) -> Vec<LayoutRegion> {
    // Score each item for math signals
    let mut math_items: Vec<(usize, &TextItem)> = Vec::new();
    for (i, item) in page_items.iter().enumerate() {
        if !matches!(item.item_type, ItemType::Text) {
            continue;
        }
        if has_math_signal(item) {
            math_items.push((i, item));
        }
    }

    if math_items.is_empty() {
        return Vec::new();
    }

    // Detect sub/super density: items near math items with smaller font + y offset
    let sub_super_indices: Vec<usize> = page_items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            if !matches!(item.item_type, ItemType::Text) {
                return None;
            }
            // Check if this item is sub/super relative to any neighbor
            for (_, math_item) in &math_items {
                let font_ratio = item.font_size / math_item.font_size;
                let y_diff = (item.y - math_item.y).abs();
                let x_near = (item.x - (math_item.x + math_item.width)).abs()
                    < math_item.font_size * 2.0
                    || (math_item.x - (item.x + item.width)).abs() < math_item.font_size * 2.0;
                if font_ratio < 0.85 && y_diff > 1.0 && y_diff < math_item.font_size && x_near {
                    return Some(i);
                }
            }
            None
        })
        .collect();

    // Merge math item indices + sub/super indices
    let mut all_math_indices: Vec<usize> = math_items.iter().map(|(i, _)| *i).collect();
    all_math_indices.extend(sub_super_indices);
    all_math_indices.sort_unstable();
    all_math_indices.dedup();

    // Cluster into contiguous regions by y-proximity
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut current_cluster: Vec<usize> = Vec::new();

    for &idx in &all_math_indices {
        let item = &page_items[idx];
        if current_cluster.is_empty() {
            current_cluster.push(idx);
            continue;
        }

        let last_item = &page_items[*current_cluster.last().unwrap()];
        let line_height = last_item.font_size.max(item.font_size) * 2.0;
        let y_gap = (item.y - last_item.y).abs();
        let x_gap = (item.x - (last_item.x + last_item.width)).abs();

        // Same cluster if vertically close (within 2x line height)
        // and not too far horizontally (within page width / 3)
        if y_gap <= line_height && x_gap < 300.0 {
            current_cluster.push(idx);
        } else {
            if current_cluster.len() >= 3 {
                clusters.push(current_cluster.clone());
            }
            current_cluster = vec![idx];
        }
    }
    if current_cluster.len() >= 3 {
        clusters.push(current_cluster);
    }

    // Convert clusters to regions
    clusters
        .iter()
        .filter_map(|cluster| {
            let items: Vec<&TextItem> = cluster.iter().map(|&i| &page_items[i]).collect();
            let x_min = items.iter().map(|i| i.x).fold(f32::MAX, f32::min);
            let x_max = items.iter().map(|i| i.x + i.width).fold(f32::MIN, f32::max);
            let y_min = items.iter().map(|i| i.y).fold(f32::MAX, f32::min);
            let y_max = items
                .iter()
                .map(|i| i.y + i.height)
                .fold(f32::MIN, f32::max);

            if x_max <= x_min || y_max <= y_min {
                return None;
            }

            // Count how many signals: font-based vs content-based
            let font_signal_count = items.iter().filter(|i| is_math_font(&i.font)).count();
            let confidence = if font_signal_count > items.len() / 2 {
                0.85
            } else {
                0.7
            };

            Some(LayoutRegion {
                bbox: BBox {
                    x_min,
                    y_min,
                    x_max,
                    y_max,
                },
                region_type: RegionType::Formula,
                page,
                confidence,
                needs_ocr: true,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemType;

    fn make_item(text: &str, x: f32, y: f32, fs: f32, font: &str) -> TextItem {
        TextItem {
            text: text.into(),
            x,
            y,
            width: text.len() as f32 * fs * 0.5,
            height: fs,
            font: font.into(),
            font_size: fs,
            page: 1,
            is_bold: false,
            is_italic: false,
            item_type: ItemType::Text,
            mcid: None,
        }
    }

    #[test]
    fn math_font_detection() {
        assert!(is_math_font("CMMI10"));
        assert!(is_math_font("CMSY8"));
        assert!(is_math_font("CMR12"));
        assert!(is_math_font("STIXMath-Regular"));
        assert!(is_math_font("Cambria Math"));
        assert!(is_math_font("MathJax_Main"));
        assert!(!is_math_font("Helvetica"));
        assert!(!is_math_font("TimesNewRoman"));
    }

    #[test]
    fn math_unicode_detection() {
        assert!(is_math_char('∑'));
        assert!(is_math_char('∫'));
        assert!(is_math_char('α'));
        assert!(is_math_char('√'));
        assert!(is_math_char('≤'));
        assert!(!is_math_char('A'));
        assert!(!is_math_char('1'));
    }

    #[test]
    fn formula_cluster_detection() {
        // Simulate a simple equation: y = αx + β
        let items = vec![
            make_item("y", 100.0, 500.0, 12.0, "CMMI10"),
            make_item("=", 115.0, 500.0, 12.0, "CMR10"),
            make_item("α", 130.0, 500.0, 12.0, "CMMI10"),
            make_item("x", 145.0, 500.0, 12.0, "CMMI10"),
            make_item("+", 160.0, 500.0, 12.0, "CMR10"),
            make_item("β", 175.0, 500.0, 12.0, "CMMI10"),
        ];
        let regions = detect_formula_regions(&items, 1);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].region_type, RegionType::Formula);
        assert!(regions[0].needs_ocr);
    }

    #[test]
    fn isolated_math_symbol_not_detected() {
        // A single "+" in running text should not create a formula region
        let items = vec![
            make_item("The", 100.0, 500.0, 12.0, "Helvetica"),
            make_item("+", 140.0, 500.0, 12.0, "Helvetica"),
            make_item("operator", 155.0, 500.0, 12.0, "Helvetica"),
        ];
        let regions = detect_formula_regions(&items, 1);
        assert!(regions.is_empty());
    }
}
