//! Smart PDF detection and text extraction using lopdf
//!
//! This module provides:
//! - Fast detection of scanned vs text-based PDFs without full document load
//! - Direct text extraction from text-based PDFs
//! - Markdown conversion with structure detection

pub mod adobe_korea1;
pub mod detector;
pub mod extractor;
pub mod glyph_names;
pub mod markdown;
pub mod process_mode;
pub mod tables;
pub mod text_utils;
pub mod tounicode;
pub mod types;

pub use detector::{
    detect_pdf_type, detect_pdf_type_mem, detect_pdf_type_mem_with_config,
    detect_pdf_type_with_config, DetectionConfig, PdfType, PdfTypeResult, ScanStrategy,
};
pub use extractor::{extract_text, extract_text_with_positions, extract_text_with_positions_pages};
pub use markdown::{
    to_markdown, to_markdown_from_items, to_markdown_from_items_with_rects, MarkdownOptions,
};
pub use process_mode::ProcessMode;
pub use types::{LayoutComplexity, PdfRect, TextItem};

use std::path::Path;

/// High-level PDF processing result
#[derive(Debug)]
pub struct PdfProcessResult {
    /// The detected PDF type
    pub pdf_type: PdfType,
    /// Extracted text (if text-based PDF)
    pub text: Option<String>,
    /// Markdown output (if text-based PDF)
    pub markdown: Option<String>,
    /// Page count
    pub page_count: u32,
    /// Processing time in milliseconds
    pub processing_time_ms: u64,
    /// 1-indexed page numbers that need OCR.
    pub pages_needing_ocr: Vec<u32>,
    /// Title from PDF metadata (if available)
    pub title: Option<String>,
    /// Detection confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Layout complexity analysis (tables, multi-column detection).
    pub layout: LayoutComplexity,
    /// True when broken font encodings are detected (garbled text, replacement
    /// characters). Clients should fall back to OCR for affected pages.
    pub has_encoding_issues: bool,
}

/// Process a PDF file with smart detection and extraction
///
/// This function will:
/// 1. Quickly detect if the PDF is text-based or scanned
/// 2. If text-based, extract text and convert to markdown
/// 3. If scanned, return early indicating OCR is needed
pub fn process_pdf<P: AsRef<Path>>(path: P) -> Result<PdfProcessResult, PdfError> {
    let start = std::time::Instant::now();

    validate_pdf_file(&path)?;

    // Step 1: Smart detection (fast, no full load)
    let detection = detect_pdf_type(&path)?;
    let page_count = detection.page_count;
    let pdf_type = detection.pdf_type;
    let pages_needing_ocr = detection.pages_needing_ocr;
    let title = detection.title;
    let confidence = detection.confidence;

    let result = match pdf_type {
        PdfType::TextBased => {
            // Step 2: Full extraction with position-aware reading order
            let (items, rects) = extractor::extract_text_with_positions_and_rects(&path, None)?;
            let layout = compute_layout_complexity(&items, &rects);
            let markdown =
                to_markdown_from_items_with_rects(items, MarkdownOptions::default(), &rects);
            let has_encoding_issues = detect_encoding_issues(&markdown);

            PdfProcessResult {
                pdf_type,
                text: None, // We now produce markdown directly
                markdown: Some(markdown),
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout,
                has_encoding_issues,
            }
        }
        PdfType::Scanned | PdfType::ImageBased => {
            // Return early - OCR needed
            PdfProcessResult {
                pdf_type,
                text: None,
                markdown: None,
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout: LayoutComplexity::default(),
                has_encoding_issues: false,
            }
        }
        PdfType::Mixed => {
            // Try to extract what we can with position-aware reading order
            let extracted = extractor::extract_text_with_positions_and_rects(&path, None).ok();
            let (markdown, layout, has_encoding_issues) = match extracted {
                Some((items, rects)) => {
                    let layout = compute_layout_complexity(&items, &rects);
                    let md = to_markdown_from_items_with_rects(
                        items,
                        MarkdownOptions::default(),
                        &rects,
                    );
                    let enc = detect_encoding_issues(&md);
                    (Some(md), layout, enc)
                }
                None => (None, LayoutComplexity::default(), false),
            };

            PdfProcessResult {
                pdf_type,
                text: None,
                markdown,
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout,
                has_encoding_issues,
            }
        }
    };

    Ok(result)
}

/// Process a PDF file with custom detection and markdown configuration
pub fn process_pdf_with_config<P: AsRef<Path>>(
    path: P,
    config: DetectionConfig,
    markdown_options: MarkdownOptions,
) -> Result<PdfProcessResult, PdfError> {
    process_pdf_with_config_pages(path, config, markdown_options, None)
}

/// Process a PDF file with custom configuration and optional page filter.
///
/// `page_filter` limits extraction to the given 1-indexed page numbers.
/// When `None`, all pages are processed.
pub fn process_pdf_with_config_pages<P: AsRef<Path>>(
    path: P,
    config: DetectionConfig,
    markdown_options: MarkdownOptions,
    page_filter: Option<&std::collections::HashSet<u32>>,
) -> Result<PdfProcessResult, PdfError> {
    let start = std::time::Instant::now();

    validate_pdf_file(&path)?;

    let detection = detect_pdf_type_with_config(&path, config)?;
    let page_count = detection.page_count;
    let pdf_type = detection.pdf_type;
    let pages_needing_ocr = detection.pages_needing_ocr;
    let title = detection.title;
    let confidence = detection.confidence;

    // DetectOnly: return immediately after detection
    if markdown_options.process_mode == ProcessMode::DetectOnly {
        return Ok(PdfProcessResult {
            pdf_type,
            text: None,
            markdown: None,
            page_count,
            processing_time_ms: start.elapsed().as_millis() as u64,
            pages_needing_ocr,
            title,
            confidence,
            layout: LayoutComplexity::default(),
            has_encoding_issues: false,
        });
    }

    let result = match pdf_type {
        PdfType::TextBased => {
            let (items, rects) =
                extractor::extract_text_with_positions_and_rects(&path, page_filter)?;
            let layout = compute_layout_complexity(&items, &rects);

            let markdown = if markdown_options.process_mode == ProcessMode::Analyze {
                None
            } else {
                Some(to_markdown_from_items_with_rects(
                    items,
                    markdown_options,
                    &rects,
                ))
            };
            let has_encoding_issues = markdown
                .as_ref()
                .is_some_and(|md| detect_encoding_issues(md));

            PdfProcessResult {
                pdf_type,
                text: None,
                markdown,
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout,
                has_encoding_issues,
            }
        }
        PdfType::Scanned | PdfType::ImageBased => PdfProcessResult {
            pdf_type,
            text: None,
            markdown: None,
            page_count,
            processing_time_ms: start.elapsed().as_millis() as u64,
            pages_needing_ocr,
            title,
            confidence,
            layout: LayoutComplexity::default(),
            has_encoding_issues: false,
        },
        PdfType::Mixed => {
            let extracted =
                extractor::extract_text_with_positions_and_rects(&path, page_filter).ok();
            let (markdown, layout, has_encoding_issues) = match extracted {
                Some((items, rects)) => {
                    let layout = compute_layout_complexity(&items, &rects);
                    let md = if markdown_options.process_mode == ProcessMode::Analyze {
                        None
                    } else {
                        Some(to_markdown_from_items_with_rects(
                            items,
                            markdown_options.clone(),
                            &rects,
                        ))
                    };
                    let enc = md.as_ref().is_some_and(|m| detect_encoding_issues(m));
                    (md, layout, enc)
                }
                None => (None, LayoutComplexity::default(), false),
            };

            PdfProcessResult {
                pdf_type,
                text: None,
                markdown,
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout,
                has_encoding_issues,
            }
        }
    };

    Ok(result)
}

/// Process PDF from memory buffer
pub fn process_pdf_mem(buffer: &[u8]) -> Result<PdfProcessResult, PdfError> {
    process_pdf_mem_with_config(
        buffer,
        DetectionConfig::default(),
        MarkdownOptions::default(),
    )
}

/// Process PDF from memory buffer with custom detection and markdown configuration
pub fn process_pdf_mem_with_config(
    buffer: &[u8],
    config: DetectionConfig,
    markdown_options: MarkdownOptions,
) -> Result<PdfProcessResult, PdfError> {
    let start = std::time::Instant::now();

    validate_pdf_bytes(buffer)?;

    let detection = detector::detect_pdf_type_mem_with_config(buffer, config)?;
    let page_count = detection.page_count;
    let pdf_type = detection.pdf_type;
    let pages_needing_ocr = detection.pages_needing_ocr;
    let title = detection.title;
    let confidence = detection.confidence;

    // DetectOnly: return immediately after detection
    if markdown_options.process_mode == ProcessMode::DetectOnly {
        return Ok(PdfProcessResult {
            pdf_type,
            text: None,
            markdown: None,
            page_count,
            processing_time_ms: start.elapsed().as_millis() as u64,
            pages_needing_ocr,
            title,
            confidence,
            layout: LayoutComplexity::default(),
            has_encoding_issues: false,
        });
    }

    let result = match pdf_type {
        PdfType::TextBased => {
            let (items, rects) =
                extractor::extract_text_with_positions_mem_and_rects(buffer, None)?;
            let layout = compute_layout_complexity(&items, &rects);

            let markdown = if markdown_options.process_mode == ProcessMode::Analyze {
                None
            } else {
                Some(to_markdown_from_items_with_rects(
                    items,
                    markdown_options,
                    &rects,
                ))
            };
            let has_encoding_issues = markdown
                .as_ref()
                .is_some_and(|md| detect_encoding_issues(md));

            PdfProcessResult {
                pdf_type,
                text: None,
                markdown,
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout,
                has_encoding_issues,
            }
        }
        PdfType::Scanned | PdfType::ImageBased => PdfProcessResult {
            pdf_type,
            text: None,
            markdown: None,
            page_count,
            processing_time_ms: start.elapsed().as_millis() as u64,
            pages_needing_ocr,
            title,
            confidence,
            layout: LayoutComplexity::default(),
            has_encoding_issues: false,
        },
        PdfType::Mixed => {
            let extracted = extractor::extract_text_with_positions_mem_and_rects(buffer, None).ok();
            let (markdown, layout, has_encoding_issues) = match extracted {
                Some((items, rects)) => {
                    let layout = compute_layout_complexity(&items, &rects);
                    let md = if markdown_options.process_mode == ProcessMode::Analyze {
                        None
                    } else {
                        Some(to_markdown_from_items_with_rects(
                            items,
                            markdown_options.clone(),
                            &rects,
                        ))
                    };
                    let enc = md.as_ref().is_some_and(|m| detect_encoding_issues(m));
                    (md, layout, enc)
                }
                None => (None, LayoutComplexity::default(), false),
            };

            PdfProcessResult {
                pdf_type,
                text: None,
                markdown,
                page_count,
                processing_time_ms: start.elapsed().as_millis() as u64,
                pages_needing_ocr,
                title,
                confidence,
                layout,
                has_encoding_issues,
            }
        }
    };

    Ok(result)
}

/// Detect broken font encodings in extracted markdown text.
///
/// Two heuristics:
/// 1. **U+FFFD**: Any replacement character indicates decode failures.
/// 2. **Dollar-as-space**: Pattern like `Word$Word$Word` where `$` is used as a
///    word separator due to broken ToUnicode CMaps. Triggers when either:
///    - More than 50% of `$` are between letters (clear substitution pattern), OR
///    - More than 20 letter-dollar-letter occurrences (even if some `$` are also
///      used as trailing/leading separators, 20+ is far beyond normal financial text).
fn detect_encoding_issues(markdown: &str) -> bool {
    // Heuristic 1: U+FFFD replacement characters
    if markdown.contains('\u{FFFD}') {
        return true;
    }

    // Heuristic 2: dollar-as-space pattern
    let total_dollars = markdown.matches('$').count();
    if total_dollars > 10 {
        let bytes = markdown.as_bytes();
        let mut letter_dollar_letter = 0usize;
        for i in 1..bytes.len().saturating_sub(1) {
            if bytes[i] == b'$'
                && bytes[i - 1].is_ascii_alphabetic()
                && bytes[i + 1].is_ascii_alphabetic()
            {
                letter_dollar_letter += 1;
            }
        }
        if letter_dollar_letter > 20 || letter_dollar_letter * 2 > total_dollars {
            return true;
        }
    }

    false
}

/// Analyse extracted items and rects for layout complexity.
fn compute_layout_complexity(
    items: &[types::TextItem],
    rects: &[types::PdfRect],
) -> LayoutComplexity {
    use std::collections::HashMap;

    // --- Tables: count significant rects per page (w>=5, h>=5), flag pages with >6 ---
    let mut rect_counts: HashMap<u32, usize> = HashMap::new();
    for r in rects {
        if r.width.abs() >= 5.0 && r.height.abs() >= 5.0 {
            *rect_counts.entry(r.page).or_default() += 1;
        }
    }
    let mut pages_with_tables: Vec<u32> = rect_counts
        .into_iter()
        .filter(|&(_, count)| count > 6)
        .map(|(page, _)| page)
        .collect();
    pages_with_tables.sort();

    // --- Columns: run detect_columns() per page, flag pages with 2+ columns ---
    let mut seen_pages: Vec<u32> = items.iter().map(|i| i.page).collect();
    seen_pages.sort();
    seen_pages.dedup();

    let mut pages_with_columns: Vec<u32> = Vec::new();
    for page in seen_pages {
        let cols = extractor::detect_columns(items, page);
        if cols.len() >= 2 {
            pages_with_columns.push(page);
        }
    }

    let is_complex = !pages_with_tables.is_empty() || !pages_with_columns.is_empty();

    LayoutComplexity {
        is_complex,
        pages_with_tables,
        pages_with_columns,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PDF parsing error: {0}")]
    Parse(String),
    #[error("PDF is encrypted")]
    Encrypted,
    #[error("Invalid PDF structure")]
    InvalidStructure,
    #[error("Not a PDF: {0}")]
    NotAPdf(String),
}

impl From<lopdf::Error> for PdfError {
    fn from(e: lopdf::Error) -> Self {
        match e {
            lopdf::Error::IO(io_err) => PdfError::Io(io_err),
            lopdf::Error::Decryption(_)
            | lopdf::Error::InvalidPassword
            | lopdf::Error::AlreadyEncrypted
            | lopdf::Error::UnsupportedSecurityHandler(_) => PdfError::Encrypted,
            lopdf::Error::Unimplemented(msg) if msg.contains("encrypted") => PdfError::Encrypted,
            lopdf::Error::Parse(ref pe) if pe.to_string().contains("invalid file header") => {
                PdfError::NotAPdf("invalid PDF file header".to_string())
            }
            lopdf::Error::MissingXrefEntry
            | lopdf::Error::Xref(_)
            | lopdf::Error::IndirectObject { .. }
            | lopdf::Error::ObjectIdMismatch
            | lopdf::Error::InvalidObjectStream(_)
            | lopdf::Error::InvalidOffset(_) => PdfError::InvalidStructure,
            other => PdfError::Parse(other.to_string()),
        }
    }
}

/// Check whether a `lopdf::Error` represents an encryption-related failure.
pub(crate) fn is_encrypted_lopdf_error(e: &lopdf::Error) -> bool {
    matches!(
        e,
        lopdf::Error::Decryption(_)
            | lopdf::Error::InvalidPassword
            | lopdf::Error::AlreadyEncrypted
            | lopdf::Error::UnsupportedSecurityHandler(_)
    ) || matches!(e, lopdf::Error::Unimplemented(msg) if msg.contains("encrypted"))
}

// ---------------------------------------------------------------------------
// PDF validation helpers
// ---------------------------------------------------------------------------

/// Strip UTF-8 BOM and leading ASCII whitespace from a byte slice.
fn strip_bom_and_whitespace(bytes: &[u8]) -> &[u8] {
    let b = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    };
    let start = b
        .iter()
        .position(|&c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    &b[start..]
}

/// Case-insensitive prefix check on byte slices.
fn starts_with_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if haystack.len() < needle.len() {
        return false;
    }
    haystack[..needle.len()]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Try to identify what kind of file the bytes represent.
fn detect_file_type_hint(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "file is empty".to_string();
    }

    let trimmed = strip_bom_and_whitespace(bytes);

    // HTML
    if starts_with_ci(trimmed, b"<!doctype html")
        || starts_with_ci(trimmed, b"<html")
        || starts_with_ci(trimmed, b"<head")
        || starts_with_ci(trimmed, b"<body")
    {
        return "file appears to be HTML".to_string();
    }

    // XML (but not HTML)
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<") {
        // Distinguish generic XML from HTML-like XML
        if starts_with_ci(trimmed, b"<?xml") {
            return "file appears to be XML".to_string();
        }
        // Other tags that look like XML
        if trimmed.starts_with(b"<") && !trimmed.starts_with(b"<%") {
            return "file appears to be XML".to_string();
        }
    }

    // JSON
    if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
        return "file appears to be JSON".to_string();
    }

    // PNG
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return "file appears to be a PNG image".to_string();
    }

    // JPEG
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "file appears to be a JPEG image".to_string();
    }

    // ZIP / Office documents
    if bytes.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
        return "file appears to be a ZIP archive (possibly an Office document)".to_string();
    }

    // If it looks like mostly printable ASCII/UTF-8, call it plain text
    let sample = &bytes[..bytes.len().min(512)];
    let printable = sample
        .iter()
        .filter(|&&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
        .count();
    if printable > sample.len() * 3 / 4 {
        return "file appears to be plain text".to_string();
    }

    "file is not a PDF".to_string()
}

/// Validate that a byte buffer looks like a PDF (has `%PDF-` magic).
///
/// Scans the first 1024 bytes, allowing for a UTF-8 BOM and leading whitespace.
pub(crate) fn validate_pdf_bytes(buffer: &[u8]) -> Result<(), PdfError> {
    if buffer.is_empty() {
        return Err(PdfError::NotAPdf(detect_file_type_hint(buffer)));
    }

    let header = &buffer[..buffer.len().min(1024)];
    let trimmed = strip_bom_and_whitespace(header);

    if trimmed.starts_with(b"%PDF-") {
        Ok(())
    } else {
        Err(PdfError::NotAPdf(detect_file_type_hint(buffer)))
    }
}

/// Validate that a file on disk looks like a PDF.
///
/// Reads only the first 1024 bytes and delegates to [`validate_pdf_bytes`].
pub(crate) fn validate_pdf_file<P: AsRef<Path>>(path: P) -> Result<(), PdfError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 1024];
    let n = file.read(&mut buf)?;
    validate_pdf_bytes(&buf[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_encoding_issues_fffd() {
        assert!(detect_encoding_issues(
            "Some text with \u{FFFD} replacement"
        ));
    }

    #[test]
    fn test_detect_encoding_issues_dollar_as_space() {
        // Simulates broken CMap: "$Workshop$on$Chest$Wall$Deformities$and$..."
        let garbled = "Last$advanced$Book$Programm$3th$Workshop$on$Chest$Wall$Deformities$and$More";
        assert!(detect_encoding_issues(garbled));
    }

    #[test]
    fn test_detect_encoding_issues_financial_text() {
        // Legitimate dollar signs in financial text should NOT trigger
        let financial = "Revenue was $100M in Q1, up from $90M. Costs: $50M, $30M, $20M, $15M, $12M, $8M, $5M, $3M, $2M, $1M, $500K.";
        assert!(!detect_encoding_issues(financial));
    }

    #[test]
    fn test_detect_encoding_issues_clean_text() {
        assert!(!detect_encoding_issues(
            "Normal markdown text with no issues."
        ));
    }

    #[test]
    fn test_detect_encoding_issues_few_dollars() {
        // Under threshold of 10 total dollars — should not trigger
        let text = "a$b c$d e$f";
        assert!(!detect_encoding_issues(text));
    }
}
