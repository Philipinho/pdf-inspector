//! Markdown conversion with structure detection.
//!
//! Converts extracted text to markdown, detecting:
//! - Headers (by font size)
//! - Lists (bullet points, numbered lists)
//! - Code blocks (monospace fonts, indentation)
//! - Paragraphs

pub(crate) mod analysis;
mod classify;
mod convert;
mod postprocess;
mod preprocess;

pub use convert::to_markdown_from_lines;

use std::collections::{HashMap, HashSet};

use crate::extractor::group_into_lines;
use crate::types::{PdfLine, PdfRect, TextItem};

use analysis::calculate_font_stats_from_items;
use classify::{format_list_item, is_code_like, is_list_item};
use convert::{merge_continuation_tables, to_markdown_from_lines_with_tables_and_images};

/// Detect side-by-side table layout by finding a significant X-position gap.
///
/// Returns X-band boundaries `[(x_min, split_x), (split_x, x_max)]` when a
/// clear vertical gap separates two groups of items, or an empty vec if the
/// page has a single-region layout.
///
/// Evaluates all candidate gaps (≥40pt with ≥20 items on each side) and picks
/// the one with the fewest bounding-box crossings. The crossing count must be
/// under 2% of total items — this prevents splitting paragraphs whose lines
/// extend across the gap.
pub(crate) fn split_side_by_side(items: &[TextItem]) -> Vec<(f32, f32)> {
    if items.len() < 40 {
        return vec![];
    }

    // Sort items by left edge
    let mut xs: Vec<f32> = items.iter().map(|i| i.x).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Find all candidate gaps: ≥40pt with ≥20 items on each side
    let mut candidates: Vec<(f32, usize)> = Vec::new(); // (split_x, gap_pos)
    for i in 1..xs.len() {
        let gap = xs[i] - xs[i - 1];
        let left_count = i;
        let right_count = xs.len() - i;
        if gap >= 40.0 && left_count >= 20 && right_count >= 20 {
            let split_x = (xs[i - 1] + xs[i]) / 2.0;
            candidates.push((split_x, i));
        }
    }

    if candidates.is_empty() {
        return vec![];
    }

    // Pick the candidate with the fewest bounding-box crossings,
    // but only consider balanced splits (smaller side ≥ 30% of total).
    let min_side = items.len() * 3 / 10;
    let mut best_split = 0.0f32;
    let mut best_crossing = usize::MAX;
    for &(split_x, gap_pos) in &candidates {
        let left_count = gap_pos;
        let right_count = xs.len() - gap_pos;
        if left_count.min(right_count) < min_side {
            continue;
        }
        let crossing = items
            .iter()
            .filter(|item| item.x < split_x && (item.x + item.width) > split_x)
            .count();
        if crossing < best_crossing {
            best_crossing = crossing;
            best_split = split_x;
        }
    }

    if best_crossing == usize::MAX {
        return vec![];
    }

    // Crossing items must be < 2% of total (allows a few spanning headers)
    let max_crossing = (items.len() / 50).max(2);
    if best_crossing > max_crossing {
        return vec![];
    }

    let x_min = xs[0];
    let x_max = *xs.last().unwrap();

    vec![(x_min, best_split), (best_split, x_max)]
}

/// Filter rects to those overlapping an X band.
pub(crate) fn filter_rects_to_band(
    rects: &[PdfRect],
    page: u32,
    x_lo: f32,
    x_hi: f32,
) -> Vec<PdfRect> {
    rects
        .iter()
        .filter(|r| {
            r.page == page && {
                let rx_min = if r.width >= 0.0 { r.x } else { r.x + r.width };
                let rx_max = if r.width >= 0.0 { r.x + r.width } else { r.x };
                rx_max > x_lo && rx_min < x_hi
            }
        })
        .cloned()
        .collect()
}

/// A band of items/indices/rects/lines for side-by-side table detection.
type BandSpec = (Vec<TextItem>, Vec<usize>, Vec<PdfRect>, Vec<PdfLine>);

/// Filter PDF lines to those overlapping an X band.
pub(crate) fn filter_lines_to_band(
    lines: &[PdfLine],
    page: u32,
    x_lo: f32,
    x_hi: f32,
) -> Vec<PdfLine> {
    lines
        .iter()
        .filter(|l| {
            l.page == page && {
                let lx_min = l.x1.min(l.x2);
                let lx_max = l.x1.max(l.x2);
                lx_max > x_lo && lx_min < x_hi
            }
        })
        .cloned()
        .collect()
}

/// Options for markdown conversion
#[derive(Debug, Clone)]
pub struct MarkdownOptions {
    /// Detect headers by font size
    pub detect_headers: bool,
    /// Detect list items
    pub detect_lists: bool,
    /// Detect code blocks
    pub detect_code: bool,
    /// Base font size for comparison
    pub base_font_size: Option<f32>,
    /// Remove standalone page numbers
    pub remove_page_numbers: bool,
    /// Convert URLs to markdown links
    pub format_urls: bool,
    /// Fix hyphenation (broken words across lines)
    pub fix_hyphenation: bool,
    /// Detect and format bold text from font names
    pub detect_bold: bool,
    /// Detect and format italic text from font names
    pub detect_italic: bool,
    /// Include image placeholders in output
    pub include_images: bool,
    /// Include extracted hyperlinks
    pub include_links: bool,
    /// Insert page break markers (<!-- Page N -->) between pages
    pub include_page_numbers: bool,
    /// Strip repeated headers/footers that appear on many pages
    pub strip_headers_footers: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            detect_headers: true,
            detect_lists: true,
            detect_code: true,
            base_font_size: None,
            remove_page_numbers: true,
            format_urls: true,
            fix_hyphenation: true,
            detect_bold: true,
            detect_italic: true,
            include_images: true,
            include_links: true,
            include_page_numbers: false,
            strip_headers_footers: true,
        }
    }
}

/// Convert plain text to markdown (basic conversion)
pub fn to_markdown(text: &str, options: MarkdownOptions) -> String {
    let mut output = String::new();
    let mut in_list = false;
    let mut in_code_block = false;

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            if in_list {
                in_list = false;
            }
            if in_code_block {
                output.push_str("```\n");
                in_code_block = false;
            }
            output.push('\n');
            continue;
        }

        // Detect list items
        if options.detect_lists && is_list_item(trimmed) {
            let formatted = format_list_item(trimmed);
            output.push_str(&formatted);
            output.push('\n');
            in_list = true;
            continue;
        }

        // Detect code blocks (indented lines)
        if options.detect_code && is_code_like(trimmed) {
            if !in_code_block {
                output.push_str("```\n");
                in_code_block = true;
            }
            output.push_str(trimmed);
            output.push('\n');
            continue;
        } else if in_code_block {
            output.push_str("```\n");
            in_code_block = false;
        }

        // Regular paragraph text
        output.push_str(trimmed);
        output.push('\n');
    }

    if in_code_block {
        output.push_str("```\n");
    }

    output
}

/// Convert positioned text items to markdown with structure detection
pub fn to_markdown_from_items(items: Vec<TextItem>, options: MarkdownOptions) -> String {
    to_markdown_from_items_with_rects(items, options, &[])
}

/// Convert positioned text items to markdown, using rectangle data for table detection
pub fn to_markdown_from_items_with_rects(
    items: Vec<TextItem>,
    options: MarkdownOptions,
    rects: &[crate::types::PdfRect],
) -> String {
    to_markdown_from_items_with_rects_and_lines(items, options, rects, &[])
}

/// Convert positioned text items to markdown, using rectangles and line segments for table detection.
///
/// Line-based detection runs first (strongest structural evidence), then rect-based,
/// then heuristic fallback on unclaimed items.
pub(crate) fn to_markdown_from_items_with_rects_and_lines(
    items: Vec<TextItem>,
    options: MarkdownOptions,
    rects: &[crate::types::PdfRect],
    pdf_lines: &[crate::types::PdfLine],
) -> String {
    use crate::tables::{
        detect_tables, detect_tables_from_lines, detect_tables_from_rects, table_to_markdown,
    };
    use crate::types::ItemType;

    if items.is_empty() {
        return String::new();
    }

    // Separate images and links from text items
    let mut images: Vec<TextItem> = Vec::new();
    let mut links: Vec<TextItem> = Vec::new();
    let mut text_items: Vec<TextItem> = Vec::new();

    for item in items {
        match &item.item_type {
            ItemType::Image => {
                if options.include_images {
                    images.push(item);
                }
            }
            ItemType::Link(_) => {
                if options.include_links {
                    links.push(item);
                }
            }
            ItemType::Text | ItemType::FormField => {
                text_items.push(item);
            }
        }
    }

    // Calculate base font size for table detection
    let font_stats = calculate_font_stats_from_items(&text_items);
    let base_size = options
        .base_font_size
        .unwrap_or(font_stats.most_common_size);

    // Detect tables on each page
    let mut table_items: HashSet<usize> = HashSet::new();
    let mut page_tables: HashMap<u32, Vec<(f32, String)>> = HashMap::new();

    // Store images by page and Y position for insertion
    let mut page_images: HashMap<u32, Vec<(f32, String)>> = HashMap::new();

    for img in &images {
        // Extract image name from "[Image: Im0]" format
        let img_name = img
            .text
            .strip_prefix("[Image: ")
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(&img.text);
        let img_md = format!("![Image: {}](image)\n", img_name);
        page_images
            .entry(img.page)
            .or_default()
            .push((img.y, img_md));
    }

    // Pre-group items by page with their global indices (O(n) instead of O(pages*n))
    let mut page_groups: HashMap<u32, Vec<(usize, &TextItem)>> = HashMap::new();
    for (global_idx, item) in text_items.iter().enumerate() {
        page_groups
            .entry(item.page)
            .or_default()
            .push((global_idx, item));
    }

    let mut pages: Vec<u32> = page_groups.keys().copied().collect();
    pages.sort();
    let page_count = pages.last().copied().unwrap_or(0) + 1;

    for page in pages {
        let group = page_groups.get(&page).unwrap();
        let page_items: Vec<TextItem> = group.iter().map(|(_, item)| (*item).clone()).collect();

        // Check for side-by-side layout (e.g. two tables placed left and right)
        let bands = split_side_by_side(&page_items);

        // Build list of (band_items, band_index_map, band_rects, band_lines).
        // band_index_map[local_band_idx] → page_items index.
        let band_specs: Vec<BandSpec> = if bands.is_empty() {
            // Single-region page — use all items/rects/lines as-is
            let identity: Vec<usize> = (0..page_items.len()).collect();
            vec![(
                page_items.clone(),
                identity,
                rects.iter().filter(|r| r.page == page).cloned().collect(),
                pdf_lines
                    .iter()
                    .filter(|l| l.page == page)
                    .cloned()
                    .collect(),
            )]
        } else {
            bands
                .iter()
                .map(|&(x_lo, x_hi)| {
                    let margin = 2.0; // small margin to avoid clipping edge items
                    let (items_in_band, idx_map): (Vec<TextItem>, Vec<usize>) = page_items
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| item.x >= x_lo - margin && item.x < x_hi + margin)
                        .map(|(idx, item)| (item.clone(), idx))
                        .unzip();
                    let band_rects = filter_rects_to_band(rects, page, x_lo, x_hi);
                    let band_lines = filter_lines_to_band(pdf_lines, page, x_lo, x_hi);
                    (items_in_band, idx_map, band_rects, band_lines)
                })
                .collect()
        };

        for (band_items, band_index_map, band_rects, band_lines) in &band_specs {
            if band_items.is_empty() {
                continue;
            }

            // Track which band-local indices are claimed by structural detection
            let mut rect_claimed: HashSet<usize> = HashSet::new();

            // 1. Rect-based detection first (well-tested, high precision)
            let (rect_tables, hint_regions) =
                detect_tables_from_rects(band_items, band_rects, page);
            for table in &rect_tables {
                for &idx in &table.item_indices {
                    rect_claimed.insert(idx);
                    if let Some(&page_idx) = band_index_map.get(idx) {
                        if let Some(&(global_idx, _)) = group.get(page_idx) {
                            table_items.insert(global_idx);
                        }
                    }
                }
                let table_y = table.rows.first().copied().unwrap_or(0.0);
                let table_md = table_to_markdown(table);
                page_tables
                    .entry(page)
                    .or_default()
                    .push((table_y, table_md));
            }

            // 2. Line-based detection on unclaimed items (when rects didn't find tables)
            if rect_claimed.is_empty() {
                let line_tables = detect_tables_from_lines(band_items, band_lines, page);
                for table in &line_tables {
                    for &idx in &table.item_indices {
                        rect_claimed.insert(idx);
                        if let Some(&page_idx) = band_index_map.get(idx) {
                            if let Some(&(global_idx, _)) = group.get(page_idx) {
                                table_items.insert(global_idx);
                            }
                        }
                    }
                    let table_y = table.rows.first().copied().unwrap_or(0.0);
                    let table_md = table_to_markdown(table);
                    page_tables
                        .entry(page)
                        .or_default()
                        .push((table_y, table_md));
                }
            }

            // 3. Heuristic fallback on unclaimed items
            let mut run_heuristic =
                |subset_items: &[TextItem], index_map: &[usize], min_items: usize| {
                    if subset_items.len() < min_items {
                        return;
                    }
                    let tables = detect_tables(subset_items, base_size, false);
                    for table in tables {
                        for &idx in &table.item_indices {
                            if let Some(&band_idx) = index_map.get(idx) {
                                if let Some(&page_idx) = band_index_map.get(band_idx) {
                                    if let Some(&(global_idx, _)) = group.get(page_idx) {
                                        table_items.insert(global_idx);
                                    }
                                }
                            }
                        }
                        let table_y = table.rows.first().copied().unwrap_or(0.0);
                        let table_md = table_to_markdown(&table);
                        page_tables
                            .entry(page)
                            .or_default()
                            .push((table_y, table_md));
                    }
                };

            // Run heuristic detection on unclaimed items
            if rect_claimed.is_empty() && hint_regions.is_empty() {
                // No rect tables or hints — run heuristic on all band items
                let identity_map: Vec<usize> = (0..band_items.len()).collect();
                run_heuristic(band_items, &identity_map, 6);
            } else if rect_claimed.is_empty() && !hint_regions.is_empty() {
                // No rect tables but hint regions exist — run heuristic separately
                // on items inside each hint region and on items outside all hints.
                let padding = 15.0;
                for hint in &hint_regions {
                    let (inside_items, inside_map): (Vec<TextItem>, Vec<usize>) = band_items
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| {
                            item.y >= hint.y_bottom - padding && item.y <= hint.y_top + padding
                        })
                        .map(|(idx, item)| (item.clone(), idx))
                        .unzip();
                    run_heuristic(&inside_items, &inside_map, 6);
                    for &band_idx in &inside_map {
                        rect_claimed.insert(band_idx);
                    }
                }
                let (outside_items, outside_map): (Vec<TextItem>, Vec<usize>) = band_items
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| !rect_claimed.contains(idx))
                    .map(|(idx, item)| (item.clone(), idx))
                    .unzip();
                run_heuristic(&outside_items, &outside_map, 6);
            } else {
                // Rect tables found — run heuristic on unclaimed items
                let (unclaimed_items, unclaimed_map): (Vec<TextItem>, Vec<usize>) = band_items
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| !rect_claimed.contains(idx))
                    .map(|(idx, item)| (item.clone(), idx))
                    .unzip();
                run_heuristic(&unclaimed_items, &unclaimed_map, 6);
            }
        }
    }

    // Filter out table items and process the rest
    let non_table_items: Vec<TextItem> = text_items
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !table_items.contains(idx))
        .map(|(_, item)| item)
        .collect();

    // Find pages that are table-only (no remaining non-table text)
    let table_only_pages: HashSet<u32> = {
        let pages_with_text: HashSet<u32> = non_table_items.iter().map(|i| i.page).collect();
        page_tables
            .keys()
            .filter(|p| !pages_with_text.contains(p))
            .copied()
            .collect()
    };

    // Merge continuation tables across page breaks, but only for table-only pages
    merge_continuation_tables(&mut page_tables, &table_only_pages);

    let lines = group_into_lines(non_table_items);

    // Strip repeated headers/footers before conversion
    let lines = if options.strip_headers_footers {
        preprocess::strip_repeated_lines(lines, page_count)
    } else {
        lines
    };

    // Convert to markdown, inserting tables and images at appropriate positions
    to_markdown_from_lines_with_tables_and_images(lines, options, page_tables, page_images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analysis::detect_header_level;
    use classify::{is_code_like, is_list_item};

    #[test]
    fn test_is_list_item() {
        assert!(is_list_item("• Item one"));
        assert!(is_list_item("- Item two"));
        assert!(is_list_item("* Item three"));
        assert!(is_list_item("1. First"));
        assert!(is_list_item("2) Second"));
        assert!(is_list_item("a. Letter item"));
        assert!(!is_list_item("Regular text"));
    }

    #[test]
    fn test_format_list_item() {
        assert_eq!(format_list_item("• Item"), "- Item");
        assert_eq!(format_list_item("- Item"), "- Item");
        assert_eq!(format_list_item("1. First"), "1. First");
    }

    #[test]
    fn test_is_code_like() {
        assert!(is_code_like("const x = 5;"));
        assert!(is_code_like("function foo() {"));
        assert!(is_code_like("import React from 'react'"));
        assert!(!is_code_like("This is regular text."));
    }

    #[test]
    fn test_detect_header_level() {
        // With three tiers: 24→H1, 18→H2, 15→H3, 12→None
        let tiers = vec![24.0, 18.0, 15.0];
        assert_eq!(detect_header_level(24.0, 12.0, &tiers), Some(1));
        assert_eq!(detect_header_level(18.0, 12.0, &tiers), Some(2));
        assert_eq!(detect_header_level(15.0, 12.0, &tiers), Some(3));
        assert_eq!(detect_header_level(12.0, 12.0, &tiers), None);

        // Single tier: 15→H1 (ratio 1.25 ≥ 1.2), 14→None (ratio 1.17 < 1.2)
        let tiers = vec![15.0];
        assert_eq!(detect_header_level(15.0, 12.0, &tiers), Some(1));
        assert_eq!(detect_header_level(14.0, 12.0, &tiers), None);
        assert_eq!(detect_header_level(12.0, 12.0, &tiers), None);

        // No tiers (empty): falls back to ratio thresholds
        let tiers: Vec<f32> = vec![];
        assert_eq!(detect_header_level(24.0, 12.0, &tiers), Some(1));
        assert_eq!(detect_header_level(18.0, 12.0, &tiers), Some(2));
        assert_eq!(detect_header_level(15.0, 12.0, &tiers), Some(3));
        assert_eq!(detect_header_level(14.5, 12.0, &tiers), Some(4));
        assert_eq!(detect_header_level(14.0, 12.0, &tiers), None);
        assert_eq!(detect_header_level(12.0, 12.0, &tiers), None);

        // Body text excluded when tiers exist: 13pt (ratio 1.08) → None
        let tiers = vec![20.0];
        assert_eq!(detect_header_level(13.0, 12.0, &tiers), None);
    }

    #[test]
    fn test_to_markdown() {
        let text = "• First item\n• Second item\n\nRegular paragraph.";
        let md = to_markdown(text, MarkdownOptions::default());
        assert!(md.contains("- First item"));
        assert!(md.contains("- Second item"));
    }
}
