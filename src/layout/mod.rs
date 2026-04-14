//! Heuristic layout region detection for PDF pages.
//!
//! Detects tables, formulas, images, paragraphs, and headings from
//! extracted text items, rectangles, and line segments — no ML model needed.
//!
//! Primary entry point: [`detect_layout_regions`].

mod formula;
mod tables;
mod text_blocks;

use std::collections::HashSet;

use crate::markdown::analysis::calculate_font_stats_from_items;
use crate::types::{
    BBox, ItemType, LayoutRegion, PageLayout, PdfLine, PdfRect, RegionType, TextItem,
};

/// Detect layout regions for all pages from pre-extracted PDF data.
///
/// Runs table, formula, image, and text-block detection per page and returns
/// a sorted list of [`PageLayout`] results. Orchestrators can inspect each
/// region's [`LayoutRegion::needs_ocr`] flag to decide routing.
pub fn detect_layout_regions(
    items: &[TextItem],
    rects: &[PdfRect],
    lines: &[PdfLine],
) -> Vec<PageLayout> {
    let font_stats = calculate_font_stats_from_items(items);
    let base_size = font_stats.most_common_size;

    // Collect unique pages
    let mut pages: Vec<u32> = items.iter().map(|i| i.page).collect();
    pages.sort_unstable();
    pages.dedup();

    pages
        .into_iter()
        .map(|page| {
            let page_items: Vec<TextItem> =
                items.iter().filter(|i| i.page == page).cloned().collect();

            let regions = detect_page_regions(&page_items, rects, lines, page, base_size);
            PageLayout { page, regions }
        })
        .collect()
}

/// Detect all region types for a single page.
fn detect_page_regions(
    page_items: &[TextItem],
    rects: &[PdfRect],
    lines: &[PdfLine],
    page: u32,
    base_size: f32,
) -> Vec<LayoutRegion> {
    let mut regions = Vec::new();
    let mut claimed_indices: HashSet<usize> = HashSet::new();

    // 1. Tables (highest priority — claim items first)
    let table_regions = tables::detect_table_regions(page_items, rects, lines, page, base_size);
    for tr in &table_regions {
        // Mark items inside the table bbox as claimed
        for (i, item) in page_items.iter().enumerate() {
            if bbox_contains(&tr.bbox, item.x, item.y) {
                claimed_indices.insert(i);
            }
        }
    }
    regions.extend(table_regions);

    // 2. Images
    for (i, item) in page_items.iter().enumerate() {
        if matches!(item.item_type, ItemType::Image) {
            claimed_indices.insert(i);
            regions.push(LayoutRegion {
                bbox: BBox {
                    x_min: item.x,
                    y_min: item.y,
                    x_max: item.x + item.width,
                    y_max: item.y + item.height,
                },
                region_type: RegionType::Image,
                page,
                confidence: 1.0,
                needs_ocr: false,
            });
        }
    }

    // 3. Formulas (from unclaimed text items)
    let unclaimed_items: Vec<TextItem> = page_items
        .iter()
        .enumerate()
        .filter(|(i, _)| !claimed_indices.contains(i))
        .map(|(_, item)| item.clone())
        .collect();
    let formula_regions = formula::detect_formula_regions(&unclaimed_items, page);
    for fr in &formula_regions {
        // Mark items inside formula bbox as claimed
        for (i, item) in page_items.iter().enumerate() {
            if !claimed_indices.contains(&i) && bbox_contains(&fr.bbox, item.x, item.y) {
                claimed_indices.insert(i);
            }
        }
    }
    regions.extend(formula_regions);

    // 4. Paragraphs and headings (from remaining unclaimed items)
    let text_regions = text_blocks::detect_text_block_regions(page_items, page, &claimed_indices);
    regions.extend(text_regions);

    // Sort: top-to-bottom (descending y in PDF coords), then left-to-right
    regions.sort_by(|a, b| {
        b.bbox
            .y_max
            .total_cmp(&a.bbox.y_max)
            .then(a.bbox.x_min.total_cmp(&b.bbox.x_min))
    });

    regions
}

/// Check if a point falls within a bounding box.
fn bbox_contains(bbox: &BBox, x: f32, y: f32) -> bool {
    x >= bbox.x_min && x <= bbox.x_max && y >= bbox.y_min && y <= bbox.y_max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemType;

    fn make_text(text: &str, x: f32, y: f32, fs: f32) -> TextItem {
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

    fn make_image(x: f32, y: f32, w: f32, h: f32) -> TextItem {
        TextItem {
            text: String::new(),
            x,
            y,
            width: w,
            height: h,
            font: String::new(),
            font_size: 0.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            item_type: ItemType::Image,
            mcid: None,
        }
    }

    #[test]
    fn mixed_page_detects_multiple_region_types() {
        let items = vec![
            // Heading
            make_text("Title", 72.0, 750.0, 24.0),
            // Paragraph
            make_text("Some body text here.", 72.0, 720.0, 12.0),
            make_text("More body text follows.", 72.0, 706.0, 12.0),
            // Image
            make_image(72.0, 400.0, 200.0, 150.0),
        ];

        let pages = detect_layout_regions(&items, &[], &[]);
        assert_eq!(pages.len(), 1);
        let regions = &pages[0].regions;

        assert!(
            regions.iter().any(|r| r.region_type == RegionType::Image),
            "expected image region"
        );
        // Should have heading + paragraph (or at least some text regions)
        let text_region_count = regions
            .iter()
            .filter(|r| {
                matches!(
                    r.region_type,
                    RegionType::Paragraph | RegionType::Heading { .. }
                )
            })
            .count();
        assert!(
            text_region_count >= 1,
            "expected text regions, got {regions:?}"
        );
    }

    #[test]
    fn empty_page_produces_no_regions() {
        let pages = detect_layout_regions(&[], &[], &[]);
        assert!(pages.is_empty());
    }

    #[test]
    fn bbox_contains_works() {
        let bbox = BBox {
            x_min: 10.0,
            y_min: 10.0,
            x_max: 100.0,
            y_max: 100.0,
        };
        assert!(bbox_contains(&bbox, 50.0, 50.0));
        assert!(bbox_contains(&bbox, 10.0, 10.0)); // edge
        assert!(!bbox_contains(&bbox, 5.0, 50.0)); // outside left
        assert!(!bbox_contains(&bbox, 50.0, 105.0)); // outside top
    }
}
