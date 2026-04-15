//! Probe extract_formulas_in_regions_mem against a regions list.
//! Usage: probe-formulas <pdf-path> <regions-file>
//! regions-file format: one region per line, "<page_0idx> <x1> <y1> <x2> <y2> [label]"

use pdf_inspector::extract_formulas_in_regions_mem;
use std::collections::HashMap;
use std::env;
use std::fs;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <pdf-path> <regions-file>", args[0]);
        std::process::exit(1);
    }
    let pdf_path = &args[1];
    let regions_path = &args[2];

    let pdf_bytes = fs::read(pdf_path).expect("read pdf");
    let regions_text = fs::read_to_string(regions_path).expect("read regions");

    // Parse regions and group by page
    let mut by_page: HashMap<u32, Vec<[f32; 4]>> = HashMap::new();
    let mut labels_by_page: HashMap<u32, Vec<String>> = HashMap::new();

    for (line_no, line) in regions_text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            eprintln!("line {} skipped: not enough tokens", line_no + 1);
            continue;
        }
        let page: u32 = parts[0].parse().expect("page idx");
        let x1: f32 = parts[1].parse().expect("x1");
        let y1: f32 = parts[2].parse().expect("y1");
        let x2: f32 = parts[3].parse().expect("x2");
        let y2: f32 = parts[4].parse().expect("y2");
        let label = if parts.len() >= 6 {
            parts[5].to_string()
        } else {
            "?".to_string()
        };
        by_page.entry(page).or_default().push([x1, y1, x2, y2]);
        labels_by_page.entry(page).or_default().push(label);
    }

    let mut page_regions: Vec<(u32, Vec<[f32; 4]>)> = by_page.into_iter().collect();
    page_regions.sort_by_key(|(p, _)| *p);

    let total: usize = page_regions.iter().map(|(_, r)| r.len()).sum();
    if total == 0 {
        println!("No regions to probe.");
        return;
    }

    let results = extract_formulas_in_regions_mem(&pdf_bytes, &page_regions).expect("extract");

    let mut total_ok = 0;
    let mut total_needs_ocr = 0;
    let mut total_empty = 0;
    let mut samples: Vec<(u32, String, String)> = Vec::new();

    for page_result in &results {
        let labels = labels_by_page
            .get(&page_result.page)
            .cloned()
            .unwrap_or_default();
        for (idx, region) in page_result.regions.iter().enumerate() {
            let label = labels.get(idx).cloned().unwrap_or_else(|| "?".into());
            if region.text.trim().is_empty() {
                total_empty += 1;
                total_needs_ocr += 1;
                if samples
                    .iter()
                    .filter(|(p, s, _)| *p == page_result.page && s.starts_with("EMPTY"))
                    .count()
                    < 1
                {
                    samples.push((page_result.page, format!("EMPTY[{}]", label), String::new()));
                }
            } else if region.needs_ocr {
                total_needs_ocr += 1;
                if samples
                    .iter()
                    .filter(|(p, s, _)| *p == page_result.page && s.starts_with("BAD"))
                    .count()
                    < 2
                {
                    samples.push((
                        page_result.page,
                        format!("BAD[{}]", label),
                        region.text.chars().take(80).collect(),
                    ));
                }
            } else {
                total_ok += 1;
                if samples
                    .iter()
                    .filter(|(p, s, _)| *p == page_result.page && s.starts_with("OK"))
                    .count()
                    < 3
                {
                    samples.push((
                        page_result.page,
                        format!("OK[{}]", label),
                        region.text.chars().take(80).collect(),
                    ));
                }
            }
        }
    }

    println!(
        "=== {} ===",
        pdf_path.split('/').next_back().unwrap_or(pdf_path)
    );
    println!("Formula regions: {}", total);
    println!(
        "  native_ok:  {}  ({:.1}%)",
        total_ok,
        total_ok as f64 * 100.0 / total as f64
    );
    println!(
        "  needs_ocr:  {}  ({:.1}%)  [empty: {}]",
        total_needs_ocr,
        total_needs_ocr as f64 * 100.0 / total as f64,
        total_empty
    );
    println!("Samples:");
    for (page, status, text) in samples.iter().take(10) {
        if text.is_empty() {
            println!("  p{} {}", page, status);
        } else {
            println!("  p{} {} → {:?}", page, status, text);
        }
    }
}
