//! Evaluate LaTeX formula reconstruction against GPU layout output.
//!
//! Usage: probe-formulas-latex <pdf-path> <gpu-layout-json>
//!
//! Reads the GPU layout JSON (from PP-DocLayoutV3), extracts formula bboxes,
//! converts pixel coordinates to PDF points, then calls both
//! `extract_formulas_in_regions_mem` (raw text) and
//! `extract_formulas_in_regions_as_latex` (LaTeX reconstruction).
//!
//! Prints side-by-side comparison and aggregate confidence statistics.

use pdf_inspector::{extract_formulas_in_regions_as_latex, extract_formulas_in_regions_mem};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;

#[derive(Deserialize)]
struct GpuLayout {
    #[allow(dead_code)]
    pdf: Option<String>,
    pages: Vec<GpuPage>,
}

#[derive(Deserialize)]
struct GpuPage {
    page: u32,
    pix_w: f64,
    pix_h: f64,
    pt_w: f64,
    pt_h: f64,
    regions: Vec<GpuRegion>,
    #[allow(dead_code)]
    inference_ms: Option<u64>,
}

#[derive(Deserialize)]
struct GpuRegion {
    label: String,
    #[allow(dead_code)]
    task_type: Option<String>,
    score: f64,
    bbox_pixel: [f64; 4],
    #[allow(dead_code)]
    reading_order: Option<u32>,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <pdf-path> <gpu-layout-json>", args[0]);
        std::process::exit(1);
    }
    let pdf_path = &args[1];
    let layout_path = &args[2];

    let pdf_bytes = fs::read(pdf_path).expect("read pdf");
    let layout_json = fs::read_to_string(layout_path).expect("read layout json");
    let layout: GpuLayout = serde_json::from_str(&layout_json).expect("parse layout json");

    // Collect formula bboxes per page, converting pixel coords to PDF points
    let mut page_regions_map: HashMap<u32, Vec<[f32; 4]>> = HashMap::new();
    let mut region_meta: Vec<(u32, usize, f64)> = Vec::new(); // (page, region_idx_within_page, score)

    for page in &layout.pages {
        let scale_x = page.pt_w / page.pix_w;
        let scale_y = page.pt_h / page.pix_h;

        let formulas: Vec<&GpuRegion> = page
            .regions
            .iter()
            .filter(|r| r.label == "formula")
            .collect();

        for formula in &formulas {
            let [px1, py1, px2, py2] = formula.bbox_pixel;
            let bbox: [f32; 4] = [
                (px1 * scale_x) as f32,
                (py1 * scale_y) as f32,
                (px2 * scale_x) as f32,
                (py2 * scale_y) as f32,
            ];
            let idx = page_regions_map.entry(page.page).or_default().len();
            page_regions_map.entry(page.page).or_default().push(bbox);
            region_meta.push((page.page, idx, formula.score));
        }
    }

    let mut page_regions: Vec<(u32, Vec<[f32; 4]>)> = page_regions_map.into_iter().collect();
    page_regions.sort_by_key(|(p, _)| *p);

    let total: usize = page_regions.iter().map(|(_, r)| r.len()).sum();
    if total == 0 {
        println!("No formula regions found in layout JSON.");
        return;
    }

    // Call both extraction functions
    let raw_results =
        extract_formulas_in_regions_mem(&pdf_bytes, &page_regions).expect("raw extract");
    let latex_results =
        extract_formulas_in_regions_as_latex(&pdf_bytes, &page_regions).expect("latex extract");

    // Print header
    println!(
        "=== {} ===",
        pdf_path.split('/').next_back().unwrap_or(pdf_path)
    );
    println!("Formula regions: {}", total);
    println!();

    // Track confidence distribution
    let mut high_confidence = 0; // > 0.85
    let mut mid_confidence = 0; // 0.5 - 0.85
    let mut low_confidence = 0; // < 0.5
    let mut needs_ocr_count = 0;
    let mut total_confidence: f64 = 0.0;

    // Print per-region results
    for (raw_page, latex_page) in raw_results.iter().zip(latex_results.iter()) {
        for (idx, (raw_region, latex_region)) in raw_page
            .regions
            .iter()
            .zip(latex_page.regions.iter())
            .enumerate()
        {
            let raw_text = raw_region.text.replace('\n', " \\n ");
            let latex_text = &latex_region.latex;
            let confidence = latex_region.confidence;
            let needs_ocr = latex_region.needs_ocr;

            total_confidence += confidence as f64;

            if needs_ocr {
                needs_ocr_count += 1;
            }
            if confidence > 0.85 {
                high_confidence += 1;
            } else if confidence >= 0.5 {
                mid_confidence += 1;
            } else {
                low_confidence += 1;
            }

            // Confidence indicator
            let conf_indicator = if confidence > 0.85 {
                "HIGH"
            } else if confidence >= 0.5 {
                "MID "
            } else {
                "LOW "
            };

            let ocr_tag = if needs_ocr { " [OCR]" } else { "" };

            let breakdown = &latex_region.confidence_breakdown;
            let breakdown_str = if breakdown.is_empty() {
                String::new()
            } else {
                format!("\n  penalties: {}", breakdown.join("; "))
            };

            println!(
                "p{} r{:02} [{} {:.2}]{}\n  raw:   {:?}\n  latex: {:?}{}\n",
                raw_page.page,
                idx,
                conf_indicator,
                confidence,
                ocr_tag,
                truncate(&raw_text, 120),
                truncate(latex_text, 120),
                breakdown_str,
            );
        }
    }

    // Aggregate stats
    println!("=== AGGREGATE ===");
    println!("Total formulas:    {}", total);
    println!(
        "High confidence:   {} ({:.1}%)  [> 0.85]",
        high_confidence,
        high_confidence as f64 * 100.0 / total as f64
    );
    println!(
        "Mid confidence:    {} ({:.1}%)  [0.5 - 0.85]",
        mid_confidence,
        mid_confidence as f64 * 100.0 / total as f64
    );
    println!(
        "Low confidence:    {} ({:.1}%)  [< 0.5]",
        low_confidence,
        low_confidence as f64 * 100.0 / total as f64
    );
    println!(
        "Needs OCR:         {} ({:.1}%)",
        needs_ocr_count,
        needs_ocr_count as f64 * 100.0 / total as f64
    );
    println!("Mean confidence:   {:.3}", total_confidence / total as f64);
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
