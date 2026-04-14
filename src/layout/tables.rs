//! Table region detection — wraps existing 3-strategy cascade and emits bounding boxes.

use crate::markdown;
use crate::tables;
use crate::types::{BBox, LayoutRegion, PdfLine, PdfRect, RegionType, TextItem};

/// Detection strategy used, determines confidence score.
enum Strategy {
    Rect,
    Line,
    Heuristic,
}

/// Detect table regions on a single page.
///
/// Runs the rect → line → heuristic cascade, including side-by-side band
/// splitting (same logic as `compute_layout_complexity`).
pub(crate) fn detect_table_regions(
    page_items: &[TextItem],
    rects: &[PdfRect],
    lines: &[PdfLine],
    page: u32,
    base_font_size: f32,
) -> Vec<LayoutRegion> {
    let bands = markdown::split_side_by_side(page_items);

    let band_ranges: Vec<(f32, f32)> = if bands.is_empty() {
        vec![(f32::MIN, f32::MAX)]
    } else {
        bands
    };

    let mut regions = Vec::new();

    for &(x_lo, x_hi) in &band_ranges {
        let is_full = x_lo == f32::MIN;
        let margin = 2.0;

        let band_items: Vec<TextItem> = page_items
            .iter()
            .filter(|item| is_full || (item.x >= x_lo - margin && item.x < x_hi + margin))
            .cloned()
            .collect();

        let band_rects: Vec<PdfRect> = if is_full {
            rects.iter().filter(|r| r.page == page).cloned().collect()
        } else {
            markdown::filter_rects_to_band(rects, page, x_lo, x_hi)
        };

        let band_lines: Vec<PdfLine> = if is_full {
            lines.iter().filter(|l| l.page == page).cloned().collect()
        } else {
            markdown::filter_lines_to_band(lines, page, x_lo, x_hi)
        };

        // Rect-based
        let (rect_tables, _) = tables::detect_tables_from_rects(&band_items, &band_rects, page);
        if !rect_tables.is_empty() {
            for t in &rect_tables {
                if let Some(r) = table_to_region(t, page, Strategy::Rect) {
                    regions.push(r);
                }
            }
            continue;
        }

        // Line-based
        let line_tables = tables::detect_tables_from_lines(&band_items, &band_lines, page);
        if !line_tables.is_empty() {
            for t in &line_tables {
                if let Some(r) = table_to_region(t, page, Strategy::Line) {
                    regions.push(r);
                }
            }
            continue;
        }

        // Heuristic
        let heuristic_tables = tables::detect_tables(&band_items, base_font_size, false);
        for t in &heuristic_tables {
            if let Some(r) = table_to_region(t, page, Strategy::Heuristic) {
                regions.push(r);
            }
        }
    }

    regions
}

/// Convert a `Table` to a `LayoutRegion` by deriving its bounding box.
fn table_to_region(table: &tables::Table, page: u32, strategy: Strategy) -> Option<LayoutRegion> {
    let x_min = table.columns.first().copied()?;
    let x_max = table.columns.last().copied()?;
    // rows are in descending y order
    let y_max = table.rows.first().copied()?;
    let y_min = table.rows.last().copied()?;

    let confidence = match strategy {
        Strategy::Rect => 0.9,
        Strategy::Line => 0.85,
        Strategy::Heuristic => 0.7,
    };

    // Small padding to encompass text slightly outside grid boundaries
    let pad = 2.0;
    Some(LayoutRegion {
        bbox: BBox {
            x_min: x_min - pad,
            y_min: y_min - pad,
            x_max: x_max + pad,
            y_max: y_max + pad,
        },
        region_type: RegionType::Table,
        page,
        confidence,
        needs_ocr: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemType;

    fn make_item(text: &str, x: f32, y: f32, fs: f32) -> TextItem {
        TextItem {
            text: text.into(),
            x,
            y,
            width: 40.0,
            height: fs,
            font: "F1".into(),
            font_size: fs,
            page: 1,
            is_bold: false,
            is_italic: false,
            item_type: ItemType::Text,
            mcid: None,
        }
    }

    #[test]
    fn table_to_region_derives_bbox() {
        let table = tables::Table {
            columns: vec![100.0, 200.0, 300.0],
            rows: vec![500.0, 480.0, 460.0],
            cells: vec![vec!["A".into(), "B".into()], vec!["C".into(), "D".into()]],
            item_indices: vec![0, 1, 2, 3],
        };
        let region = table_to_region(&table, 1, Strategy::Rect).unwrap();
        assert_eq!(region.region_type, RegionType::Table);
        assert!(region.needs_ocr);
        assert!((region.confidence - 0.9).abs() < 0.01);
        assert!(region.bbox.x_min < 100.0); // padded
        assert!(region.bbox.x_max > 300.0);
        assert!(region.bbox.y_min < 460.0);
        assert!(region.bbox.y_max > 500.0);
    }

    #[test]
    fn detect_table_regions_with_rect_borders() {
        let items = vec![
            make_item("A", 100.0, 500.0, 10.0),
            make_item("B", 200.0, 500.0, 10.0),
            make_item("C", 100.0, 480.0, 10.0),
            make_item("D", 200.0, 480.0, 10.0),
        ];
        let rects = vec![
            PdfRect {
                x: 95.0,
                y: 475.0,
                width: 55.0,
                height: 30.0,
                page: 1,
            },
            PdfRect {
                x: 150.0,
                y: 475.0,
                width: 55.0,
                height: 30.0,
                page: 1,
            },
            PdfRect {
                x: 195.0,
                y: 475.0,
                width: 55.0,
                height: 30.0,
                page: 1,
            },
            PdfRect {
                x: 95.0,
                y: 495.0,
                width: 55.0,
                height: 30.0,
                page: 1,
            },
            PdfRect {
                x: 150.0,
                y: 495.0,
                width: 55.0,
                height: 30.0,
                page: 1,
            },
            PdfRect {
                x: 195.0,
                y: 495.0,
                width: 55.0,
                height: 30.0,
                page: 1,
            },
        ];
        let regions = detect_table_regions(&items, &rects, &[], 1, 10.0);
        // Should find at least one table region (rect-based)
        assert!(
            regions.iter().any(|r| r.region_type == RegionType::Table),
            "expected at least one table region, got {:?}",
            regions
        );
    }
}
