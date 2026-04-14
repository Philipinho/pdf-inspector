//! Paragraph and heading region detection from remaining text items.

use std::collections::HashSet;

use crate::extractor::group_into_lines;
use crate::markdown::analysis::{
    calculate_font_stats_from_items, compute_heading_tiers, detect_header_level,
};
use crate::types::{BBox, ItemType, LayoutRegion, RegionType, TextItem};

/// Detect paragraph and heading regions from text items, excluding items
/// already claimed by table/formula/image regions.
///
/// `excluded_indices` are indices into `page_items` that belong to other regions.
pub(crate) fn detect_text_block_regions(
    page_items: &[TextItem],
    page: u32,
    excluded_indices: &HashSet<usize>,
) -> Vec<LayoutRegion> {
    // Filter to unclaimed text items
    let remaining: Vec<TextItem> = page_items
        .iter()
        .enumerate()
        .filter(|(i, item)| {
            !excluded_indices.contains(i) && matches!(item.item_type, ItemType::Text)
        })
        .map(|(_, item)| item.clone())
        .collect();

    if remaining.is_empty() {
        return Vec::new();
    }

    // Compute font stats and heading tiers
    let font_stats = calculate_font_stats_from_items(&remaining);
    let base_size = font_stats.most_common_size;
    let lines = group_into_lines(remaining);
    let heading_tiers = compute_heading_tiers(&lines, base_size);

    // Classify each line as heading or paragraph, then group into blocks
    let mut regions = Vec::new();
    let mut current_type: Option<RegionType> = None;
    let mut block_x_min = f32::MAX;
    let mut block_x_max = f32::MIN;
    let mut block_y_min = f32::MAX;
    let mut block_y_max = f32::MIN;
    let mut prev_y: Option<f32> = None;

    for line in &lines {
        let first = match line.items.first() {
            Some(f) => f,
            None => continue,
        };

        // Classify this line
        let heading_level = detect_header_level(first.font_size, base_size, &heading_tiers);
        let line_type = if let Some(level) = heading_level {
            RegionType::Heading { level: level as u8 }
        } else {
            RegionType::Paragraph
        };

        // Compute line bbox
        let line_x_min = line.items.iter().map(|i| i.x).fold(f32::MAX, f32::min);
        let line_x_max = line
            .items
            .iter()
            .map(|i| i.x + i.width)
            .fold(f32::MIN, f32::max);
        let line_y = first.y;
        let line_height = first.height;

        // Check if we should start a new block:
        // 1. Type changed (heading vs paragraph, or different heading level)
        // 2. Large vertical gap (> 1.5x base size)
        let should_break = match (&current_type, &line_type) {
            (None, _) => false, // first line, no break
            (Some(cur), new) if cur != new => true,
            (Some(_), _) => {
                // Same type — check vertical gap
                if let Some(py) = prev_y {
                    (py - line_y).abs() > base_size * 1.5
                } else {
                    false
                }
            }
        };

        if should_break {
            // Emit current block
            if current_type.is_some() && block_x_max > block_x_min && block_y_max > block_y_min {
                regions.push(LayoutRegion {
                    bbox: BBox {
                        x_min: block_x_min,
                        y_min: block_y_min,
                        x_max: block_x_max,
                        y_max: block_y_max,
                    },
                    region_type: current_type.take().unwrap(),
                    page,
                    confidence: 0.85,
                    needs_ocr: false,
                });
            }
            // Reset
            block_x_min = f32::MAX;
            block_x_max = f32::MIN;
            block_y_min = f32::MAX;
            block_y_max = f32::MIN;
        }

        // Accumulate into current block
        current_type = Some(line_type);
        block_x_min = block_x_min.min(line_x_min);
        block_x_max = block_x_max.max(line_x_max);
        block_y_min = block_y_min.min(line_y);
        block_y_max = block_y_max.max(line_y + line_height);
        prev_y = Some(line_y);
    }

    // Emit final block
    if let Some(rt) = current_type {
        if block_x_max > block_x_min && block_y_max > block_y_min {
            regions.push(LayoutRegion {
                bbox: BBox {
                    x_min: block_x_min,
                    y_min: block_y_min,
                    x_max: block_x_max,
                    y_max: block_y_max,
                },
                region_type: rt,
                page,
                confidence: 0.85,
                needs_ocr: false,
            });
        }
    }

    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(text: &str, x: f32, y: f32, fs: f32) -> TextItem {
        TextItem {
            text: text.into(),
            x,
            y,
            width: text.len() as f32 * fs * 0.5,
            height: fs,
            font: "Helvetica".into(),
            font_size: fs,
            page: 1,
            is_bold: false,
            is_italic: false,
            item_type: ItemType::Text,
            mcid: None,
        }
    }

    #[test]
    fn heading_then_paragraph() {
        let items = vec![
            // Heading (larger font)
            make_item("Introduction", 72.0, 700.0, 20.0),
            // Body text (base size = 10pt)
            make_item("This is the first paragraph of text.", 72.0, 670.0, 10.0),
            make_item("It continues on this line here.", 72.0, 658.0, 10.0),
            make_item("And this is the third line.", 72.0, 646.0, 10.0),
        ];
        let regions = detect_text_block_regions(&items, 1, &HashSet::new());
        assert!(regions.len() >= 2, "got {:?}", regions);
        assert!(
            regions
                .iter()
                .any(|r| matches!(r.region_type, RegionType::Heading { .. })),
            "expected a heading region"
        );
        assert!(
            regions
                .iter()
                .any(|r| r.region_type == RegionType::Paragraph),
            "expected a paragraph region"
        );
    }

    #[test]
    fn excluded_indices_skip_items() {
        let items = vec![
            make_item("Hello", 72.0, 700.0, 10.0),
            make_item("Table content", 72.0, 680.0, 10.0),
            make_item("World", 72.0, 660.0, 10.0),
        ];
        let mut excluded = HashSet::new();
        excluded.insert(1); // table content excluded
        let regions = detect_text_block_regions(&items, 1, &excluded);
        // Should have regions, but the table item is excluded
        assert!(!regions.is_empty());
    }
}
