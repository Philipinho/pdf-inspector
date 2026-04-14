//! CLI tool for PDF to Markdown conversion

use pdf_inspector::{
    detect_layout, process_pdf_with_options, LayoutComplexity, PdfOptions, PdfType, ProcessMode,
    RegionType,
};
use std::collections::HashSet;
use std::env;
use std::fmt::Write;
use std::fs;
use std::process;

/// Escape a string for embedding in a JSON string value.
///
/// Handles all characters that the JSON spec requires to be escaped:
/// backslash, double-quote, and control characters U+0000..U+001F.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            c if c < '\x20' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Parse a page specification like "1,3,5-10,20" into a HashSet of page numbers.
fn parse_page_spec(spec: &str) -> Result<HashSet<u32>, String> {
    let mut pages = HashSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            let start: u32 = start
                .trim()
                .parse()
                .map_err(|_| format!("invalid page number: {}", start.trim()))?;
            let end: u32 = end
                .trim()
                .parse()
                .map_err(|_| format!("invalid page number: {}", end.trim()))?;
            if start == 0 || end == 0 {
                return Err("page numbers are 1-indexed".to_string());
            }
            if start > end {
                return Err(format!("invalid range: {}-{}", start, end));
            }
            for p in start..=end {
                pages.insert(p);
            }
        } else {
            let p: u32 = part
                .parse()
                .map_err(|_| format!("invalid page number: {}", part))?;
            if p == 0 {
                return Err("page numbers are 1-indexed".to_string());
            }
            pages.insert(p);
        }
    }
    Ok(pages)
}

fn print_layout_info(layout: &LayoutComplexity) {
    if layout.is_complex {
        eprintln!("Layout: COMPLEX");
        if !layout.pages_with_tables.is_empty() {
            eprintln!("  Pages with tables: {:?}", layout.pages_with_tables);
        }
        if !layout.pages_with_columns.is_empty() {
            eprintln!("  Pages with columns: {:?}", layout.pages_with_columns);
        }
    } else {
        eprintln!("Layout: simple");
    }
}

fn main() {
    env_logger::init();
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <pdf_file> [output_file]", args[0]);
        eprintln!("       {} <pdf_file> --json", args[0]);
        eprintln!("       {} <pdf_file> --raw", args[0]);
        eprintln!();
        eprintln!("Converts PDF to Markdown with smart type detection.");
        eprintln!("Returns early if PDF is scanned (OCR needed).");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --json              Output result as JSON");
        eprintln!("  --raw               Output only markdown (no headers)");
        eprintln!("  --pages             Insert page break markers (<!-- Page N -->)");
        eprintln!("  --select-pages N    Only process specified pages (e.g. 1,3,5-10)");
        eprintln!("  --detect-only       Only detect PDF type (no extraction)");
        eprintln!("  --analyze           Detect + extract + layout analysis (no markdown)");
        eprintln!("  --layout            Detect layout regions (tables, formulas, images, text)");
        process::exit(1);
    }

    let pdf_path = &args[1];
    let json_output = args.iter().any(|a| a == "--json");
    let raw_output = args.iter().any(|a| a == "--raw");
    let page_numbers = args.iter().any(|a| a == "--pages");
    let detect_only = args.iter().any(|a| a == "--detect-only");
    let analyze = args.iter().any(|a| a == "--analyze");
    let layout_mode = args.iter().any(|a| a == "--layout");

    // Parse --select-pages value
    let page_filter = args
        .iter()
        .position(|a| a == "--select-pages")
        .map(|i| {
            args.get(i + 1)
                .unwrap_or_else(|| {
                    eprintln!("Error: --select-pages requires a value (e.g. 1,3,5-10)");
                    process::exit(1);
                })
                .as_str()
        })
        .map(|spec| {
            parse_page_spec(spec).unwrap_or_else(|e| {
                eprintln!("Error: invalid --select-pages value: {}", e);
                process::exit(1);
            })
        });

    let output_file = args
        .get(2)
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.as_str());

    let process_mode = if detect_only {
        ProcessMode::DetectOnly
    } else if analyze {
        ProcessMode::Analyze
    } else {
        ProcessMode::Full
    };

    let mut options = PdfOptions::new().mode(process_mode);
    options.markdown.include_page_numbers = page_numbers;
    if let Some(pages) = page_filter {
        options.page_filter = Some(pages);
    }

    // Layout region detection mode
    if layout_mode {
        match detect_layout(pdf_path) {
            Ok(pages) => {
                if json_output {
                    print_layout_json(&pages);
                } else {
                    print_layout_text(&pages);
                }
            }
            Err(e) => {
                if json_output {
                    println!(r#"{{"error":"{}"}}"#, e);
                } else {
                    eprintln!("Error: {}", e);
                }
                process::exit(1);
            }
        }
        return;
    }

    match process_pdf_with_options(pdf_path, options) {
        Ok(result) => {
            if detect_only || analyze {
                // Non-full modes: output detection/analysis info
                let pdf_type_str = match result.pdf_type {
                    PdfType::TextBased => "text_based",
                    PdfType::Scanned => "scanned",
                    PdfType::ImageBased => "image_based",
                    PdfType::Mixed => "mixed",
                };

                if json_output {
                    let ocr_pages: Vec<String> = result
                        .pages_needing_ocr
                        .iter()
                        .map(|p| p.to_string())
                        .collect();
                    let table_pages: Vec<String> = result
                        .layout
                        .pages_with_tables
                        .iter()
                        .map(|p| p.to_string())
                        .collect();
                    let col_pages: Vec<String> = result
                        .layout
                        .pages_with_columns
                        .iter()
                        .map(|p| p.to_string())
                        .collect();
                    println!(
                        r#"{{"pdf_type":"{}","page_count":{},"processing_time_ms":{},"pages_needing_ocr":[{}],"is_complex":{},"pages_with_tables":[{}],"pages_with_columns":[{}],"has_encoding_issues":{}}}"#,
                        pdf_type_str,
                        result.page_count,
                        result.processing_time_ms,
                        ocr_pages.join(","),
                        result.layout.is_complex,
                        table_pages.join(","),
                        col_pages.join(","),
                        result.has_encoding_issues,
                    );
                } else {
                    eprintln!("Type: {}", pdf_type_str);
                    eprintln!("Pages: {}", result.page_count);
                    eprintln!("Processing time: {}ms", result.processing_time_ms);
                    if !result.pages_needing_ocr.is_empty() {
                        eprintln!("Pages needing OCR: {:?}", result.pages_needing_ocr);
                    }
                    if analyze {
                        print_layout_info(&result.layout);
                    }
                }
            } else if json_output {
                let md_escaped = result
                    .markdown
                    .as_ref()
                    .map(|m| json_escape(m))
                    .unwrap_or_default();

                let ocr_pages: Vec<String> = result
                    .pages_needing_ocr
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                let table_pages: Vec<String> = result
                    .layout
                    .pages_with_tables
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                let col_pages: Vec<String> = result
                    .layout
                    .pages_with_columns
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                println!(
                    r#"{{"pdf_type":"{}","page_count":{},"has_text":{},"processing_time_ms":{},"markdown_length":{},"pages_needing_ocr":[{}],"is_complex":{},"pages_with_tables":[{}],"pages_with_columns":[{}],"has_encoding_issues":{},"markdown":"{}"}}"#,
                    match result.pdf_type {
                        PdfType::TextBased => "text_based",
                        PdfType::Scanned => "scanned",
                        PdfType::ImageBased => "image_based",
                        PdfType::Mixed => "mixed",
                    },
                    result.page_count,
                    result.markdown.is_some(),
                    result.processing_time_ms,
                    result.markdown.as_ref().map(|m| m.len()).unwrap_or(0),
                    ocr_pages.join(","),
                    result.layout.is_complex,
                    table_pages.join(","),
                    col_pages.join(","),
                    result.has_encoding_issues,
                    md_escaped
                );
            } else if raw_output {
                // Raw output - just the markdown, no headers
                match result.pdf_type {
                    PdfType::TextBased | PdfType::Mixed => {
                        if let Some(markdown) = &result.markdown {
                            print!("{}", markdown);
                        }
                    }
                    PdfType::Scanned | PdfType::ImageBased => {
                        eprintln!("Error: PDF requires OCR (type: {:?})", result.pdf_type);
                        process::exit(2);
                    }
                }
            } else {
                // Verbose output with headers
                eprintln!("PDF to Markdown Conversion");
                eprintln!("==========================");
                eprintln!("File: {}", pdf_path);
                eprintln!();

                match result.pdf_type {
                    PdfType::TextBased => {
                        eprintln!("Type: TEXT-BASED (direct extraction)");
                        eprintln!("Pages: {}", result.page_count);
                        eprintln!("Processing time: {}ms", result.processing_time_ms);
                        print_layout_info(&result.layout);
                        if !result.pages_needing_ocr.is_empty() {
                            eprintln!("Pages needing OCR: {:?}", result.pages_needing_ocr);
                        }

                        if let Some(markdown) = &result.markdown {
                            if let Some(output) = output_file {
                                fs::write(output, markdown).expect("Failed to write output file");
                                eprintln!();
                                eprintln!("Markdown written to: {}", output);
                                eprintln!("Length: {} characters", markdown.len());
                            } else {
                                eprintln!();
                                eprintln!("--- Markdown Output ---");
                                eprintln!();
                                println!("{}", markdown);
                            }
                        }
                    }
                    PdfType::Scanned | PdfType::ImageBased => {
                        eprintln!(
                            "Type: {} (OCR required)",
                            if result.pdf_type == PdfType::Scanned {
                                "SCANNED"
                            } else {
                                "IMAGE-BASED"
                            }
                        );
                        eprintln!("Pages: {}", result.page_count);
                        eprintln!("Processing time: {}ms", result.processing_time_ms);
                        eprintln!();
                        eprintln!("This PDF requires OCR for text extraction.");
                        eprintln!("Consider using MinerU or similar OCR tool.");
                        process::exit(2);
                    }
                    PdfType::Mixed => {
                        eprintln!("Type: MIXED (partial text extraction)");
                        eprintln!("Pages: {}", result.page_count);
                        eprintln!("Processing time: {}ms", result.processing_time_ms);
                        print_layout_info(&result.layout);

                        if let Some(markdown) = &result.markdown {
                            eprintln!();
                            if result.pages_needing_ocr.is_empty() {
                                eprintln!("Note: Some pages may contain images that require OCR.");
                            } else {
                                eprintln!("Pages needing OCR: {:?}", result.pages_needing_ocr);
                            }
                            eprintln!();

                            if let Some(output) = output_file {
                                fs::write(output, markdown).expect("Failed to write output file");
                                eprintln!("Markdown written to: {}", output);
                                eprintln!("Length: {} characters", markdown.len());
                            } else {
                                eprintln!("--- Markdown Output ---");
                                eprintln!();
                                println!("{}", markdown);
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            if json_output {
                println!(r#"{{"error":"{}"}}"#, e);
            } else {
                eprintln!("Error: {}", e);
            }
            process::exit(1);
        }
    }
}

fn region_type_str(rt: &RegionType) -> &'static str {
    match rt {
        RegionType::Table => "table",
        RegionType::Formula => "formula",
        RegionType::Image => "image",
        RegionType::Paragraph => "paragraph",
        RegionType::Heading { .. } => "heading",
    }
}

fn print_layout_json(pages: &[pdf_inspector::PageLayout]) {
    let mut out = String::from(r#"{"pages":["#);
    for (pi, page) in pages.iter().enumerate() {
        if pi > 0 {
            out.push(',');
        }
        let _ = write!(out, r#"{{"page":{},"regions":["#, page.page);
        for (ri, r) in page.regions.iter().enumerate() {
            if ri > 0 {
                out.push(',');
            }
            let type_str = region_type_str(&r.region_type);
            let _ = write!(out, r#"{{"type":"{}""#, type_str,);
            if let RegionType::Heading { level } = r.region_type {
                let _ = write!(out, r#","level":{}"#, level);
            }
            let _ = write!(
                out,
                r#","bbox":{{"x_min":{:.1},"y_min":{:.1},"x_max":{:.1},"y_max":{:.1}}},"confidence":{:.2},"needs_ocr":{}}}"#,
                r.bbox.x_min, r.bbox.y_min, r.bbox.x_max, r.bbox.y_max, r.confidence, r.needs_ocr,
            );
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    println!("{}", out);
}

fn print_layout_text(pages: &[pdf_inspector::PageLayout]) {
    for page in pages {
        eprintln!("Page {}:", page.page);
        for r in &page.regions {
            let type_str = region_type_str(&r.region_type);
            let level_suffix = if let RegionType::Heading { level } = r.region_type {
                format!(" (H{})", level)
            } else {
                String::new()
            };
            eprintln!(
                "  {}{} [{:.0},{:.0} - {:.0},{:.0}] conf={:.2} ocr={}",
                type_str,
                level_suffix,
                r.bbox.x_min,
                r.bbox.y_min,
                r.bbox.x_max,
                r.bbox.y_max,
                r.confidence,
                r.needs_ocr,
            );
        }
    }
}
