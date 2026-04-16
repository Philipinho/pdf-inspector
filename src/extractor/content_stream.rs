//! PDF content-stream operator state machine.
//!
//! Walks the page's content stream, tracking the graphics state and text
//! matrix, and emits `TextItem`s and `PdfRect`s.

use crate::text_utils::{
    decode_text_string, effective_font_size, expand_ligatures, is_bold_font, is_italic_font,
};
use crate::tounicode::FontCMaps;
use crate::types::{
    ExtractedImage, ImageFormat, ItemType, PageExtraction, PdfLine, PdfRect, TextItem,
};
use crate::PdfError;
use log::trace;
use lopdf::{Document, Encoding, Object, ObjectId};
use std::collections::HashMap;

use super::fonts::{
    build_font_encodings, build_font_widths, compute_string_width_ts, extract_text_from_operand,
    get_font_file2_obj_num, get_operand_bytes, CMapDecisionCache,
};
use super::xobjects::{extract_form_xobject_text, get_page_xobjects, XObjectType};
use super::{get_number, multiply_matrices};

/// Strip PDF comments (% to end of line) from content stream bytes.
///
/// Some PDF generators (e.g. PD4ML) embed comments in content streams that
/// confuse lopdf's `Content::decode` parser.  Comments inside string literals
/// (parentheses) are NOT stripped — only top-level comments.
fn strip_pdf_comments(data: &[u8]) -> Vec<u8> {
    // Quick check: if no '%' present, return as-is (common case)
    if !data.contains(&b'%') {
        return data.to_vec();
    }

    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    let mut in_string = 0i32; // parenthesis nesting depth
    let mut in_hex_string = false;

    while i < data.len() {
        let b = data[i];
        match b {
            b'(' if !in_hex_string => {
                in_string += 1;
                result.push(b);
            }
            b')' if !in_hex_string && in_string > 0 => {
                in_string -= 1;
                result.push(b);
            }
            b'<' if in_string == 0 && !in_hex_string => {
                in_hex_string = true;
                result.push(b);
            }
            b'>' if in_hex_string => {
                in_hex_string = false;
                result.push(b);
            }
            b'%' if in_string == 0 && !in_hex_string => {
                // Skip until end of line
                while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
                // Replace comment with a space to preserve token separation
                result.push(b' ');
                continue; // Don't increment i again
            }
            _ => {
                result.push(b);
            }
        }
        i += 1;
    }

    result
}

/// Returns `(page_extraction, has_gid_fonts)` where `has_gid_fonts` indicates
/// the page uses fonts with unresolvable gid-encoded glyphs.
pub(crate) fn extract_page_text_items(
    doc: &Document,
    page_id: ObjectId,
    page_num: u32,
    font_cmaps: &FontCMaps,
    include_invisible: bool,
) -> Result<(PageExtraction, Vec<ExtractedImage>, bool, bool), PdfError> {
    extract_page_text_items_impl(doc, page_id, page_num, font_cmaps, include_invisible, false)
}

pub(crate) fn extract_page_text_and_images(
    doc: &Document,
    page_id: ObjectId,
    page_num: u32,
    font_cmaps: &FontCMaps,
    include_invisible: bool,
) -> Result<(PageExtraction, Vec<ExtractedImage>, bool, bool), PdfError> {
    extract_page_text_items_impl(doc, page_id, page_num, font_cmaps, include_invisible, true)
}

fn extract_page_text_items_impl(
    doc: &Document,
    page_id: ObjectId,
    page_num: u32,
    font_cmaps: &FontCMaps,
    include_invisible: bool,
    extract_images_flag: bool,
) -> Result<(PageExtraction, Vec<ExtractedImage>, bool, bool), PdfError> {
    use lopdf::content::Content;

    let mut items = Vec::new();
    let mut images: Vec<ExtractedImage> = Vec::new();
    let mut rects: Vec<PdfRect> = Vec::new();
    let mut clip_rects: Vec<PdfRect> = Vec::new();
    let mut lines: Vec<PdfLine> = Vec::new();

    // Path construction state for m/l/h → S/s line extraction
    let mut path_subpath_start: Option<(f32, f32)> = None;
    let mut path_current: Option<(f32, f32)> = None;
    let mut pending_lines: Vec<(f32, f32, f32, f32)> = Vec::new();
    // Completed subpaths (each a vec of line segments) for f/f* rect extraction
    let mut pending_subpaths: Vec<Vec<(f32, f32, f32, f32)>> = Vec::new();
    let mut fill_rects: Vec<PdfRect> = Vec::new();

    // Get fonts for encoding
    let fonts = doc.get_page_fonts(page_id).unwrap_or_default();

    // Build font encoding maps from Differences arrays
    let (font_encodings, has_gid_fonts) = build_font_encodings(doc, &fonts);

    // Build font width info for accurate text positioning
    let font_widths = build_font_widths(doc, &fonts);

    // Build maps of font resource names to their base font names and ToUnicode object refs
    let mut font_base_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut font_tounicode_refs: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut inline_cmaps: std::collections::HashMap<String, crate::tounicode::CMapEntry> =
        std::collections::HashMap::new();
    for (font_name, font_dict) in &fonts {
        let resource_name = String::from_utf8_lossy(font_name).to_string();
        if let Ok(base_font) = font_dict.get(b"BaseFont") {
            if let Ok(name) = base_font.as_name() {
                let base_name = String::from_utf8_lossy(name).to_string();
                font_base_names.insert(resource_name.clone(), base_name);
            }
        }
        // Track ToUnicode object reference, with FontFile2 fallback for Identity-H/V.
        // Also handle inline ToUnicode streams.
        match font_dict.get(b"ToUnicode") {
            Ok(tounicode) => {
                if let Ok(obj_ref) = tounicode.as_reference() {
                    font_tounicode_refs.insert(resource_name, obj_ref.0);
                } else if let Object::Stream(s) = tounicode {
                    let data = s
                        .decompressed_content()
                        .unwrap_or_else(|_| s.content.clone());
                    if let Some(entry) =
                        crate::tounicode::build_cmap_entry_from_stream(&data, font_dict, doc, 0)
                    {
                        inline_cmaps.insert(resource_name, entry);
                    }
                }
            }
            Err(_) => {
                if let Some(ff2_obj_num) = get_font_file2_obj_num(doc, font_dict) {
                    font_tounicode_refs.insert(resource_name, ff2_obj_num);
                }
            }
        }
    }

    // Cache font encodings from lopdf (once per font, not per text operand).
    // This avoids re-parsing ToUnicode CMap streams for every Tj/TJ operator.
    let mut encoding_cache: HashMap<String, Encoding<'_>> = HashMap::new();
    for (font_name, font_dict) in &fonts {
        let name = String::from_utf8_lossy(font_name).to_string();
        if let Ok(enc) = font_dict.get_font_encoding(doc) {
            encoding_cache.insert(name, enc);
        }
    }

    let mut cmap_decisions = CMapDecisionCache::new();

    // Get XObjects (images) from page resources
    let xobjects = get_page_xobjects(doc, page_id);

    // Get content
    let content_data = doc
        .get_page_content(page_id)
        .map_err(|e| PdfError::Parse(e.to_string()))?;

    // Strip PDF comments (% to end of line) from the content stream.
    // Some PDF generators (e.g. PD4ML) embed comments that confuse lopdf's
    // Content::decode parser, causing it to skip operators like ET and Q.
    let content_data = strip_pdf_comments(&content_data);

    let content = Content::decode(&content_data).map_err(|e| PdfError::Parse(e.to_string()))?;

    const MAX_OPERATIONS: usize = 1_000_000;
    if content.operations.len() > MAX_OPERATIONS {
        log::warn!(
            "page {}: skipping extraction — {} operations exceeds limit ({})",
            page_num,
            content.operations.len(),
            MAX_OPERATIONS
        );
        return Ok((
            (Vec::new(), Vec::new(), Vec::new()),
            Vec::new(),
            false,
            false,
        ));
    }

    // Graphics state tracking
    let mut ctm = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0]; // Current Transformation Matrix
    let mut text_rendering_mode: i32 = 0; // 0=fill, 1=stroke, 2=fill+stroke, 3=invisible
    let mut gstate_stack: Vec<([f32; 6], i32, f32, f32)> = Vec::new();

    // Text state tracking
    let mut current_font = String::new();
    let mut current_font_size: f32 = 12.0;
    let mut text_leading: f32 = 0.0; // TL parameter (in text-space units)
    let mut char_spacing: f32 = 0.0; // Tc parameter (extra spacing per character, unscaled)
    let mut word_spacing: f32 = 0.0; // Tw parameter (extra spacing per space char, unscaled)
    let mut text_matrix = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut line_matrix = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut in_text_block = false;

    // Track text direction votes: (horizontal_count, rotated_count).
    // For each text item, if |combined[0]| > |combined[1]| the text runs
    // horizontally (normal); otherwise it's rotated ~90°.
    let mut rotation_votes = RotationVotes {
        horizontal: 0,
        rotated: 0,
    };

    // Marked content tracking: (ActualText, MCID) per nesting level
    struct MarkedContentEntry {
        actual_text: Option<String>,
        mcid: Option<i64>,
    }
    let mut marked_content_stack: Vec<MarkedContentEntry> = Vec::new();
    let mut suppress_glyph_extraction = false;
    let mut actual_text_start_tm: Option<[f32; 6]> = None; // text matrix at BDC entry
    /// Get the innermost MCID from the marked content stack.
    fn current_mcid(stack: &[MarkedContentEntry]) -> Option<i64> {
        stack.iter().rev().find_map(|e| e.mcid)
    }

    for op in &content.operations {
        trace!("{} {:?}", op.operator, op.operands);
        match op.operator.as_str() {
            "q" => {
                // Save graphics state
                gstate_stack.push((ctm, text_rendering_mode, char_spacing, word_spacing));
            }
            "Q" => {
                // Restore graphics state
                if let Some((saved_ctm, saved_tr, saved_tc, saved_tw)) = gstate_stack.pop() {
                    ctm = saved_ctm;
                    text_rendering_mode = saved_tr;
                    char_spacing = saved_tc;
                    word_spacing = saved_tw;
                }
            }
            "cm" => {
                // Concatenate matrix to CTM
                if op.operands.len() >= 6 {
                    let new_matrix = [
                        get_number(&op.operands[0]).unwrap_or(1.0),
                        get_number(&op.operands[1]).unwrap_or(0.0),
                        get_number(&op.operands[2]).unwrap_or(0.0),
                        get_number(&op.operands[3]).unwrap_or(1.0),
                        get_number(&op.operands[4]).unwrap_or(0.0),
                        get_number(&op.operands[5]).unwrap_or(0.0),
                    ];
                    ctm = multiply_matrices(&new_matrix, &ctm);
                }
            }
            "BT" => {
                // Begin text block
                in_text_block = true;
                text_matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
                line_matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
                text_rendering_mode = 0;
            }
            "ET" => {
                // End text block
                in_text_block = false;
            }
            "Tf" => {
                // Set font and size
                if op.operands.len() >= 2 {
                    if let Ok(name) = op.operands[0].as_name() {
                        current_font = String::from_utf8_lossy(name).to_string();
                    }
                    if let Ok(size) = op.operands[1].as_f32() {
                        current_font_size = size;
                    } else if let Ok(size) = op.operands[1].as_i64() {
                        current_font_size = size as f32;
                    }
                }
            }
            "TL" => {
                // Set text leading (used by T*, ', and " operators)
                if let Some(tl) = op.operands.first().and_then(get_number) {
                    text_leading = tl;
                }
            }
            "Tr" => {
                // Set text rendering mode (3 = invisible / OCR overlay)
                if let Some(mode) = op.operands.first().and_then(get_number) {
                    text_rendering_mode = mode as i32;
                }
            }
            "Tc" => {
                // Set character spacing (extra space added after each character)
                if let Some(tc) = op.operands.first().and_then(get_number) {
                    char_spacing = tc;
                }
            }
            "Tw" => {
                // Set word spacing (extra space added for each space character)
                if let Some(tw) = op.operands.first().and_then(get_number) {
                    word_spacing = tw;
                }
            }
            "Td" | "TD" => {
                // Move text position: TLM = T(tx,ty) × TLM; Tm = TLM
                // tx,ty are in text space — must be scaled by the text line matrix
                if op.operands.len() >= 2 {
                    let tx = get_number(&op.operands[0]).unwrap_or(0.0);
                    let ty = get_number(&op.operands[1]).unwrap_or(0.0);
                    line_matrix[4] += tx * line_matrix[0] + ty * line_matrix[2];
                    line_matrix[5] += tx * line_matrix[1] + ty * line_matrix[3];
                    text_matrix = line_matrix;
                    if op.operator == "TD" {
                        text_leading = -ty;
                    }
                }
            }
            "Tm" => {
                // Set text matrix
                if op.operands.len() >= 6 {
                    for (i, operand) in op.operands.iter().take(6).enumerate() {
                        text_matrix[i] =
                            get_number(operand).unwrap_or(if i == 0 || i == 3 { 1.0 } else { 0.0 });
                    }
                    line_matrix = text_matrix;
                }
            }
            "T*" => {
                // Move to start of next line: equivalent to 0 -TL Td
                let tl = if text_leading != 0.0 {
                    text_leading
                } else {
                    current_font_size * 1.2
                };
                line_matrix[4] += (-tl) * line_matrix[2]; // Usually 0 for non-rotated text
                line_matrix[5] += (-tl) * line_matrix[3];
                text_matrix = line_matrix;
            }
            "Tj" => {
                // Show text string
                if in_text_block && !op.operands.is_empty() {
                    // Advance text matrix regardless of visibility
                    let w_ts_opt = font_widths.get(&current_font).and_then(|fi| {
                        get_operand_bytes(&op.operands[0]).map(|raw| {
                            compute_string_width_ts(
                                raw,
                                fi,
                                current_font_size,
                                char_spacing,
                                word_spacing,
                            )
                        })
                    });
                    // ActualText: suppress glyph extraction, just advance text matrix
                    if suppress_glyph_extraction {
                        if let Some(w_ts) = w_ts_opt {
                            text_matrix[4] += w_ts * text_matrix[0];
                            text_matrix[5] += w_ts * text_matrix[1];
                        }
                        continue;
                    }
                    // Skip invisible (Tr=3) text but still advance text matrix.
                    // For Mixed/template PDFs, include_invisible=true extracts
                    // the OCR text layer that sits behind scanned images.
                    if text_rendering_mode == 3 && !include_invisible {
                        if let Some(w_ts) = w_ts_opt {
                            text_matrix[4] += w_ts * text_matrix[0];
                            text_matrix[5] += w_ts * text_matrix[1];
                        }
                        continue;
                    }
                    if let Some(text) = extract_text_from_operand(
                        &op.operands[0],
                        &current_font,
                        font_base_names.get(&current_font).map(|s| s.as_str()),
                        font_cmaps,
                        &font_tounicode_refs,
                        &inline_cmaps,
                        &font_encodings,
                        &encoding_cache,
                        &mut cmap_decisions,
                    ) {
                        let combined = multiply_matrices(&text_matrix, &ctm);
                        let rendered_size = effective_font_size(current_font_size, &combined);
                        let (x, y) = (combined[4], combined[5]);
                        if combined[0].abs() >= combined[1].abs() {
                            rotation_votes.horizontal += 1;
                        } else {
                            rotation_votes.rotated += 1;
                        }
                        let width = if let Some(w_ts) = w_ts_opt {
                            text_matrix[4] += w_ts * text_matrix[0];
                            text_matrix[5] += w_ts * text_matrix[1];
                            (w_ts * (text_matrix[0] * ctm[0] + text_matrix[1] * ctm[2])).abs()
                        } else {
                            0.0
                        };
                        // Only create text item for non-whitespace; whitespace
                        // still advances the text matrix above so gap detection works
                        if !text.trim().is_empty() {
                            let base_font = font_base_names
                                .get(&current_font)
                                .map(|s| s.as_str())
                                .unwrap_or(&current_font);
                            items.push(TextItem {
                                text: expand_ligatures(&text),
                                x,
                                y,
                                width,
                                height: rendered_size,
                                font: current_font.clone(),
                                font_size: rendered_size,
                                page: page_num,
                                is_bold: is_bold_font(base_font),
                                is_italic: is_italic_font(base_font),
                                item_type: ItemType::Text,
                                mcid: current_mcid(&marked_content_stack),
                            });
                        }
                    }
                }
            }
            "TJ" => {
                // Show text with positioning — split at column-sized gaps
                if in_text_block && !op.operands.is_empty() {
                    if let Ok(array) = op.operands[0].as_array() {
                        let font_info = font_widths.get(&current_font);
                        let is_invisible = (text_rendering_mode == 3 && !include_invisible)
                            || suppress_glyph_extraction;

                        // Compute space threshold based on font metrics when available
                        let space_threshold = if let Some(font_info) = font_info {
                            let space_em = font_info.space_width as f32 * font_info.units_scale;
                            let threshold = space_em * 1000.0 * 0.4;
                            threshold.max(80.0)
                        } else {
                            120.0
                        };
                        let column_gap_threshold = space_threshold * 4.0;

                        // Track sub-items for column-gap splitting:
                        // (text, start_width_ts, end_width_ts)
                        let mut sub_items: Vec<(String, f32, f32)> = Vec::new();
                        let mut current_text = String::new();
                        let mut sub_start_width_ts: f32 = 0.0;
                        let mut total_width_ts: f32 = 0.0;
                        for element in array {
                            match element {
                                Object::Integer(n) => {
                                    let n_val = *n as f32;
                                    let displacement = -n_val / 1000.0 * current_font_size;
                                    if !is_invisible
                                        && n_val < -column_gap_threshold
                                        && !current_text.is_empty()
                                    {
                                        // Column gap: flush current segment
                                        sub_items.push((
                                            std::mem::take(&mut current_text),
                                            sub_start_width_ts,
                                            total_width_ts,
                                        ));
                                        total_width_ts += displacement;
                                        sub_start_width_ts = total_width_ts;
                                    } else {
                                        total_width_ts += displacement;
                                        if !is_invisible
                                            && n_val < -space_threshold
                                            && !current_text.is_empty()
                                            && !current_text.ends_with(' ')
                                        {
                                            current_text.push(' ');
                                        }
                                    }
                                    continue;
                                }
                                Object::Real(n) => {
                                    let n_val = *n;
                                    let displacement = -n_val / 1000.0 * current_font_size;
                                    if !is_invisible
                                        && n_val < -column_gap_threshold
                                        && !current_text.is_empty()
                                    {
                                        sub_items.push((
                                            std::mem::take(&mut current_text),
                                            sub_start_width_ts,
                                            total_width_ts,
                                        ));
                                        total_width_ts += displacement;
                                        sub_start_width_ts = total_width_ts;
                                    } else {
                                        total_width_ts += displacement;
                                        if !is_invisible
                                            && n_val < -space_threshold
                                            && !current_text.is_empty()
                                            && !current_text.ends_with(' ')
                                        {
                                            current_text.push(' ');
                                        }
                                    }
                                    continue;
                                }
                                _ => {}
                            }
                            if let Some(fi) = font_info {
                                if let Some(raw_bytes) = get_operand_bytes(element) {
                                    total_width_ts += compute_string_width_ts(
                                        raw_bytes,
                                        fi,
                                        current_font_size,
                                        char_spacing,
                                        word_spacing,
                                    );
                                }
                            }
                            if !is_invisible {
                                if let Some(text) = extract_text_from_operand(
                                    element,
                                    &current_font,
                                    font_base_names.get(&current_font).map(|s| s.as_str()),
                                    font_cmaps,
                                    &font_tounicode_refs,
                                    &inline_cmaps,
                                    &font_encodings,
                                    &encoding_cache,
                                    &mut cmap_decisions,
                                ) {
                                    current_text.push_str(&text);
                                }
                            }
                        }
                        // Flush remaining text
                        if !is_invisible && !current_text.trim().is_empty() {
                            sub_items.push((current_text, sub_start_width_ts, total_width_ts));
                        }
                        // Emit one TextItem per sub-item
                        if !sub_items.is_empty() {
                            let combined = multiply_matrices(&text_matrix, &ctm);
                            if combined[0].abs() >= combined[1].abs() {
                                rotation_votes.horizontal += 1;
                            } else {
                                rotation_votes.rotated += 1;
                            }
                            let rendered_size = effective_font_size(current_font_size, &combined);
                            let base_font = font_base_names
                                .get(&current_font)
                                .map(|s| s.as_str())
                                .unwrap_or(&current_font);
                            let scale_x = text_matrix[0] * ctm[0] + text_matrix[1] * ctm[2];
                            for (text, start_w, end_w) in &sub_items {
                                let offset_tm = [
                                    text_matrix[0],
                                    text_matrix[1],
                                    text_matrix[2],
                                    text_matrix[3],
                                    text_matrix[4] + start_w * text_matrix[0],
                                    text_matrix[5] + start_w * text_matrix[1],
                                ];
                                let combined = multiply_matrices(&offset_tm, &ctm);
                                let (x, y) = (combined[4], combined[5]);
                                let width = if font_info.is_some() {
                                    ((end_w - start_w) * scale_x).abs()
                                } else {
                                    0.0
                                };
                                items.push(TextItem {
                                    text: expand_ligatures(text),
                                    x,
                                    y,
                                    width,
                                    height: rendered_size,
                                    font: current_font.clone(),
                                    font_size: rendered_size,
                                    page: page_num,
                                    is_bold: is_bold_font(base_font),
                                    is_italic: is_italic_font(base_font),
                                    item_type: ItemType::Text,
                                    mcid: current_mcid(&marked_content_stack),
                                });
                            }
                        }
                        // Always advance text matrix by total width
                        if font_info.is_some() {
                            text_matrix[4] += total_width_ts * text_matrix[0];
                            text_matrix[5] += total_width_ts * text_matrix[1];
                        }
                    }
                }
            }
            "'" => {
                // Move to next line and show text (equivalent to T* then Tj)
                let tl = if text_leading != 0.0 {
                    text_leading
                } else {
                    current_font_size * 1.2
                };
                line_matrix[4] += (-tl) * line_matrix[2];
                line_matrix[5] += (-tl) * line_matrix[3];
                text_matrix = line_matrix;
                if !((text_rendering_mode == 3 && !include_invisible)
                    || suppress_glyph_extraction
                    || op.operands.is_empty())
                {
                    if let Some(text) = extract_text_from_operand(
                        &op.operands[0],
                        &current_font,
                        font_base_names.get(&current_font).map(|s| s.as_str()),
                        font_cmaps,
                        &font_tounicode_refs,
                        &inline_cmaps,
                        &font_encodings,
                        &encoding_cache,
                        &mut cmap_decisions,
                    ) {
                        if !text.trim().is_empty() {
                            let combined = multiply_matrices(&text_matrix, &ctm);
                            if combined[0].abs() >= combined[1].abs() {
                                rotation_votes.horizontal += 1;
                            } else {
                                rotation_votes.rotated += 1;
                            }
                            let rendered_size = effective_font_size(current_font_size, &combined);
                            let (x, y) = (combined[4], combined[5]);
                            let base_font = font_base_names
                                .get(&current_font)
                                .map(|s| s.as_str())
                                .unwrap_or(&current_font);
                            items.push(TextItem {
                                text: expand_ligatures(&text),
                                x,
                                y,
                                width: 0.0,
                                height: rendered_size,
                                font: current_font.clone(),
                                font_size: rendered_size,
                                page: page_num,
                                is_bold: is_bold_font(base_font),
                                is_italic: is_italic_font(base_font),
                                item_type: ItemType::Text,
                                mcid: current_mcid(&marked_content_stack),
                            });
                        }
                    }
                }
            }
            "Do" => {
                // XObject invocation - could be an image or form
                if !op.operands.is_empty() {
                    if let Ok(name) = op.operands[0].as_name() {
                        let xobj_name = String::from_utf8_lossy(name).to_string();

                        if let Some(xobj_type) = xobjects.get(&xobj_name) {
                            match xobj_type {
                                XObjectType::Image(obj_id) => {
                                    if extract_images_flag {
                                        if let Some(img) =
                                            extract_image_xobject(doc, *obj_id, page_num, &ctm)
                                        {
                                            images.push(img);
                                        }
                                    }
                                }
                                XObjectType::Form(form_id) => {
                                    // Extract text from Form XObject
                                    let form_items = extract_form_xobject_text(
                                        doc,
                                        *form_id,
                                        page_num,
                                        font_cmaps,
                                        &ctm,
                                        &mut cmap_decisions,
                                    );
                                    items.extend(form_items);
                                }
                            }
                        }
                    }
                }
            }
            "BMC" => {
                // Begin Marked Content (no properties)
                marked_content_stack.push(MarkedContentEntry {
                    actual_text: None,
                    mcid: None,
                });
            }
            "BDC" => {
                // Begin Marked Content with properties — extract ActualText and MCID
                let mut actual_text: Option<String> = None;
                let mut mcid: Option<i64> = None;
                if op.operands.len() >= 2 {
                    let dict = match &op.operands[1] {
                        Object::Dictionary(d) => Some(d.clone()),
                        Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
                        _ => None,
                    };
                    if let Some(d) = dict {
                        if let Ok(val) = d.get(b"ActualText") {
                            actual_text = match val {
                                Object::String(bytes, _) => Some(decode_text_string(bytes)),
                                _ => None,
                            };
                        }
                        if let Ok(Object::Integer(id)) = d.get(b"MCID") {
                            mcid = Some(*id);
                        }
                    }
                }
                if actual_text.is_some() {
                    suppress_glyph_extraction = true;
                    actual_text_start_tm = Some(text_matrix);
                }
                marked_content_stack.push(MarkedContentEntry { actual_text, mcid });
            }
            "EMC" => {
                // End Marked Content — emit ActualText item with correct width
                if let Some(entry) = marked_content_stack.pop() {
                    if let Some(at) = entry.actual_text {
                        // Compute width from text matrix advancement during BDC..EMC
                        if let Some(start_tm) = actual_text_start_tm.take() {
                            let combined = multiply_matrices(&start_tm, &ctm);
                            if combined[0].abs() >= combined[1].abs() {
                                rotation_votes.horizontal += 1;
                            } else {
                                rotation_votes.rotated += 1;
                            }
                            let rendered_size = effective_font_size(current_font_size, &combined);
                            let (x, y) = (combined[4], combined[5]);
                            // Width in device space from text matrix delta
                            let delta_ts = text_matrix[4] - start_tm[4];
                            let scale_x = start_tm[0] * ctm[0] + start_tm[1] * ctm[2];
                            let width = (delta_ts * scale_x).abs();
                            if !at.trim().is_empty() {
                                let base_font = font_base_names
                                    .get(&current_font)
                                    .map(|s| s.as_str())
                                    .unwrap_or(&current_font);
                                items.push(TextItem {
                                    text: expand_ligatures(&at),
                                    x,
                                    y,
                                    width,
                                    height: rendered_size,
                                    font: current_font.clone(),
                                    font_size: rendered_size,
                                    page: page_num,
                                    is_bold: is_bold_font(base_font),
                                    is_italic: is_italic_font(base_font),
                                    item_type: ItemType::Text,
                                    mcid: entry
                                        .mcid
                                        .or_else(|| current_mcid(&marked_content_stack)),
                                });
                            }
                        }
                        suppress_glyph_extraction =
                            marked_content_stack.iter().any(|e| e.actual_text.is_some());
                    }
                }
            }
            "re" => {
                // Rectangle operator: collect for table-grid detection
                if op.operands.len() >= 4 {
                    let rx = get_number(&op.operands[0]).unwrap_or(0.0);
                    let ry = get_number(&op.operands[1]).unwrap_or(0.0);
                    let rw = get_number(&op.operands[2]).unwrap_or(0.0);
                    let rh = get_number(&op.operands[3]).unwrap_or(0.0);
                    // Transform origin to device space
                    let x_dev = rx * ctm[0] + ry * ctm[2] + ctm[4];
                    let y_dev = rx * ctm[1] + ry * ctm[3] + ctm[5];
                    let w_dev = rw * ctm[0];
                    let h_dev = rh * ctm[3];
                    rects.push(PdfRect {
                        x: x_dev,
                        y: y_dev,
                        width: w_dev,
                        height: h_dev,
                        page: page_num,
                    });
                }
            }
            // ── Path construction operators ──────────────────────
            "m" => {
                // moveto: start a new subpath
                if op.operands.len() >= 2 {
                    let px = get_number(&op.operands[0]).unwrap_or(0.0);
                    let py = get_number(&op.operands[1]).unwrap_or(0.0);
                    path_subpath_start = Some((px, py));
                    path_current = Some((px, py));
                }
            }
            "l" => {
                // lineto: add segment from current point
                if op.operands.len() >= 2 {
                    if let Some((cx, cy)) = path_current {
                        let px = get_number(&op.operands[0]).unwrap_or(0.0);
                        let py = get_number(&op.operands[1]).unwrap_or(0.0);
                        pending_lines.push((cx, cy, px, py));
                        path_current = Some((px, py));
                    }
                }
            }
            "h" => {
                // closepath: segment back to subpath start
                if let (Some((cx, cy)), Some((sx, sy))) = (path_current, path_subpath_start) {
                    if (cx - sx).abs() > 0.01 || (cy - sy).abs() > 0.01 {
                        pending_lines.push((cx, cy, sx, sy));
                    }
                    path_current = path_subpath_start;
                }
                // Save completed subpath for f/f* rect extraction and clear pending_lines.
                // The W/W* handler reads from pending_subpaths (last entry) instead.
                if !pending_lines.is_empty() {
                    pending_subpaths.push(std::mem::take(&mut pending_lines));
                }
            }
            // ── Path painting operators ──────────────────────────
            "S" | "s" => {
                // stroke / close-and-stroke: emit pending lines
                if op.operator == "s" {
                    // close first
                    if let (Some((cx, cy)), Some((sx, sy))) = (path_current, path_subpath_start) {
                        if (cx - sx).abs() > 0.01 || (cy - sy).abs() > 0.01 {
                            pending_lines.push((cx, cy, sx, sy));
                        }
                    }
                }
                for (x1, y1, x2, y2) in pending_lines.drain(..) {
                    let x1d = x1 * ctm[0] + y1 * ctm[2] + ctm[4];
                    let y1d = x1 * ctm[1] + y1 * ctm[3] + ctm[5];
                    let x2d = x2 * ctm[0] + y2 * ctm[2] + ctm[4];
                    let y2d = x2 * ctm[1] + y2 * ctm[3] + ctm[5];
                    lines.push(PdfLine {
                        x1: x1d,
                        y1: y1d,
                        x2: x2d,
                        y2: y2d,
                        page: page_num,
                    });
                }
                pending_subpaths.clear();
                path_subpath_start = None;
                path_current = None;
            }
            "B" | "B*" | "b" | "b*" => {
                // fill+stroke: emit lines AND clear state
                if op.operator == "b" || op.operator == "b*" {
                    // close first
                    if let (Some((cx, cy)), Some((sx, sy))) = (path_current, path_subpath_start) {
                        if (cx - sx).abs() > 0.01 || (cy - sy).abs() > 0.01 {
                            pending_lines.push((cx, cy, sx, sy));
                        }
                    }
                }
                for (x1, y1, x2, y2) in pending_lines.drain(..) {
                    let x1d = x1 * ctm[0] + y1 * ctm[2] + ctm[4];
                    let y1d = x1 * ctm[1] + y1 * ctm[3] + ctm[5];
                    let x2d = x2 * ctm[0] + y2 * ctm[2] + ctm[4];
                    let y2d = x2 * ctm[1] + y2 * ctm[3] + ctm[5];
                    lines.push(PdfLine {
                        x1: x1d,
                        y1: y1d,
                        x2: x2d,
                        y2: y2d,
                        page: page_num,
                    });
                }
                pending_subpaths.clear();
                path_subpath_start = None;
                path_current = None;
            }
            "f" | "F" | "f*" => {
                // fill-only: extract axis-aligned rects from completed subpaths
                // Also check any un-closed segments still in pending_lines
                if !pending_lines.is_empty() {
                    pending_subpaths.push(std::mem::take(&mut pending_lines));
                }
                for subpath in pending_subpaths.drain(..) {
                    // Synthesize closing segment if only 3 segments
                    let mut segs = subpath;
                    if segs.len() == 3 {
                        let (x0, y0, _, _) = segs[0];
                        let (_, _, ex, ey) = segs[2];
                        if (ex - x0).abs() > 0.01 || (ey - y0).abs() > 0.01 {
                            segs.push((ex, ey, x0, y0));
                        }
                    }
                    if segs.len() == 4 {
                        let mut xs = Vec::with_capacity(8);
                        let mut ys = Vec::with_capacity(8);
                        for &(x1, y1, x2, y2) in &segs {
                            xs.push(x1);
                            xs.push(x2);
                            ys.push(y1);
                            ys.push(y2);
                        }
                        let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
                        let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                        let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
                        let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                        let w = max_x - min_x;
                        let h = max_y - min_y;
                        let eps: f32 = 0.5;
                        let axis_aligned = xs
                            .iter()
                            .all(|&x| (x - min_x).abs() < eps || (x - max_x).abs() < eps)
                            && ys
                                .iter()
                                .all(|&y| (y - min_y).abs() < eps || (y - max_y).abs() < eps);
                        if axis_aligned && w > 1.0 && h > 1.0 {
                            let x_dev = min_x * ctm[0] + min_y * ctm[2] + ctm[4];
                            let y_dev = min_x * ctm[1] + min_y * ctm[3] + ctm[5];
                            let w_dev = w * ctm[0];
                            let h_dev = h * ctm[3];
                            fill_rects.push(PdfRect {
                                x: x_dev,
                                y: y_dev,
                                width: w_dev,
                                height: h_dev,
                                page: page_num,
                            });
                        }
                    }
                }
                pending_lines.clear();
                path_subpath_start = None;
                path_current = None;
            }
            "W" | "W*" => {
                // Clip operator: check if pending path forms an axis-aligned rectangle.
                // Many PDFs define table cells as clipping paths instead of stroked rects.
                // After `h` closes a subpath, pending_lines is cleared and the subpath
                // is saved to pending_subpaths. Read from the last subpath entry.
                let mut segs: Vec<(f32, f32, f32, f32)> = if pending_lines.is_empty() {
                    pending_subpaths.last().cloned().unwrap_or_default()
                } else {
                    pending_lines.clone()
                };
                // If only 3 segments, synthesize closing segment back to subpath start
                if segs.len() == 3 {
                    if let Some((sx, sy)) = path_subpath_start {
                        let (_, _, ex, ey) = segs[2];
                        if (ex - sx).abs() > 0.01 || (ey - sy).abs() > 0.01 {
                            segs.push((ex, ey, sx, sy));
                        }
                    }
                }
                if segs.len() == 4 {
                    // Collect all endpoints and compute bounding box
                    let mut xs = Vec::with_capacity(8);
                    let mut ys = Vec::with_capacity(8);
                    for &(x1, y1, x2, y2) in &segs {
                        xs.push(x1);
                        xs.push(x2);
                        ys.push(y1);
                        ys.push(y2);
                    }
                    let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
                    let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                    let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
                    let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                    let w = max_x - min_x;
                    let h = max_y - min_y;
                    // Verify all points lie on bounding box edges (axis-aligned rectangle)
                    let eps: f32 = 0.5;
                    let axis_aligned = xs
                        .iter()
                        .all(|&x| (x - min_x).abs() < eps || (x - max_x).abs() < eps)
                        && ys
                            .iter()
                            .all(|&y| (y - min_y).abs() < eps || (y - max_y).abs() < eps);
                    if axis_aligned && w > 1.0 && h > 1.0 {
                        // Transform to device space using CTM (same as `re` handler)
                        let x_dev = min_x * ctm[0] + min_y * ctm[2] + ctm[4];
                        let y_dev = min_x * ctm[1] + min_y * ctm[3] + ctm[5];
                        let w_dev = w * ctm[0];
                        let h_dev = h * ctm[3];
                        clip_rects.push(PdfRect {
                            x: x_dev,
                            y: y_dev,
                            width: w_dev,
                            height: h_dev,
                            page: page_num,
                        });
                    }
                }
                // Do NOT clear pending_lines — the following `n` does that
            }
            "n" => {
                // end path (no-op): discard
                pending_lines.clear();
                pending_subpaths.clear();
                path_subpath_start = None;
                path_current = None;
            }
            _ => {}
        }
    }

    // Only use clip/fill rects when no `re` rects exist on this page.
    // Clip rects take priority over fill rects, but first we deduplicate
    // them: some PDFs wrap every text block in a full-page W* clip path,
    // producing thousands of identical rects that yield a degenerate grid.
    // After dedup, if too few unique clip rects remain we fall through to
    // fill rects (explicitly drawn visible rectangles).
    if rects.is_empty() {
        dedup_rects(&mut clip_rects);
        if clip_rects.len() >= 4 {
            rects = clip_rects;
        } else if !fill_rects.is_empty() {
            rects = fill_rects;
        } else if !clip_rects.is_empty() {
            rects = clip_rects;
        }
    }

    // Detect dominant text rotation and transform coordinates if needed.
    // Some PDFs embed landscape content in portrait pages using a rotated text
    // matrix (e.g. [0, b, -b, 0, tx, ty] for 90° CCW).  The layout engine
    // assumes x=horizontal, y=vertical — so we swap coordinates to match.
    let (items, rects, lines, coords_rotated) =
        correct_rotated_page(items, rects, lines, &rotation_votes);

    let items = super::merge_text_items(items);
    let items = super::merge_subscript_items(items);
    Ok(((items, rects, lines), images, has_gid_fonts, coords_rotated))
}

// ---------------------------------------------------------------------------
// Image extraction helpers
// ---------------------------------------------------------------------------

/// Extract an image from an XObject stream, returning an `ExtractedImage`
/// if the filter is supported (DCTDecode / FlateDecode).
fn extract_image_xobject(
    doc: &Document,
    obj_id: ObjectId,
    page_num: u32,
    ctm: &[f32; 6],
) -> Option<ExtractedImage> {
    let stream = match doc.get_object(obj_id) {
        Ok(Object::Stream(s)) => s,
        _ => return None,
    };

    // Filter can be a name (/FlateDecode) or array ([/FlateDecode])
    let filter = stream.dict.get(b"Filter").ok().and_then(|f| {
        if let Ok(name) = f.as_name() {
            Some(name.to_vec())
        } else if let Ok(arr) = f.as_array() {
            if arr.len() == 1 {
                arr[0].as_name().ok().map(|n| n.to_vec())
            } else {
                // Chained filters like [/ASCII85Decode /FlateDecode]
                arr.last()
                    .and_then(|v| v.as_name().ok())
                    .map(|n| n.to_vec())
            }
        } else {
            None
        }
    });

    let width = stream
        .dict
        .get(b"Width")
        .ok()
        .and_then(|w| w.as_i64().ok())
        .unwrap_or(0) as u32;
    let height = stream
        .dict
        .get(b"Height")
        .ok()
        .and_then(|h| h.as_i64().ok())
        .unwrap_or(0) as u32;

    if width == 0 || height == 0 {
        return None;
    }

    match filter.as_deref() {
        Some(b"DCTDecode") => {
            // JPEG — raw stream content is the JPEG file
            Some(ExtractedImage {
                page: page_num,
                x: ctm[4],
                y: ctm[5],
                width,
                height,
                format: ImageFormat::Jpeg,
                data: stream.content.clone(),
            })
        }
        Some(b"FlateDecode") => encode_flatedecode_to_png(stream, width, height, page_num, ctm),
        _ => None,
    }
}

/// Color space info resolved from a PDF image dictionary.
struct ImageColorInfo {
    png_color_type: png::ColorType,
    channels: u8,
    is_cmyk: bool,
}

/// Decompress a FlateDecode image stream and encode it as PNG.
///
/// Handles DeviceRGB, DeviceGray, DeviceCMYK color spaces.
/// Handles PNG predictor filters (Predictor >= 10 in DecodeParms).
/// Skips unsupported color spaces (Indexed, etc.) instead of crashing.
fn encode_flatedecode_to_png(
    stream: &lopdf::Stream,
    width: u32,
    height: u32,
    page_num: u32,
    ctm: &[f32; 6],
) -> Option<ExtractedImage> {
    // Decompress the stream
    let raw = stream.decompressed_content().ok()?;

    let bpc = stream
        .dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|b| b.as_i64().ok())
        .unwrap_or(8) as u8;

    let color_info = resolve_color_space(&stream.dict)?;

    // Check for PNG predictor in DecodeParms
    let has_png_predictor = stream
        .dict
        .get(b"DecodeParms")
        .ok()
        .and_then(|dp| dp.as_dict().ok())
        .and_then(|dp| dp.get(b"Predictor").ok())
        .and_then(|p| p.as_i64().ok())
        .map(|p| p >= 10)
        .unwrap_or(false);

    // Calculate expected row size (bytes per row of pixel data, no filter byte)
    let bits_per_row = width as usize * color_info.channels as usize * bpc as usize;
    let bytes_per_row = bits_per_row.div_ceil(8);

    // Prepare pixel data: strip PNG predictor filter bytes if present
    let pixel_data = if has_png_predictor {
        let stride = bytes_per_row + 1; // +1 for filter byte
        if raw.len() < stride * height as usize {
            return None;
        }
        unfilter_png_rows(
            &raw,
            width,
            height,
            bytes_per_row,
            color_info.channels as usize,
            bpc,
        )
    } else {
        if raw.len() < bytes_per_row * height as usize {
            return None;
        }
        // Truncate to exact expected size to avoid PNG encoder errors
        raw[..bytes_per_row * height as usize].to_vec()
    };

    // Handle CMYK → RGB conversion
    let (final_data, final_color_type) = if color_info.is_cmyk {
        let rgb = cmyk_to_rgb(&pixel_data, width, height);
        (rgb, png::ColorType::Rgb)
    } else {
        (pixel_data, color_info.png_color_type)
    };

    // Encode to PNG
    let mut png_buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, width, height);
        encoder.set_color(final_color_type);
        encoder.set_depth(match bpc {
            1 => png::BitDepth::One,
            2 => png::BitDepth::Two,
            4 => png::BitDepth::Four,
            16 => png::BitDepth::Sixteen,
            _ => png::BitDepth::Eight,
        });
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&final_data).ok()?;
    }

    Some(ExtractedImage {
        page: page_num,
        x: ctm[4],
        y: ctm[5],
        width,
        height,
        format: ImageFormat::Png,
        data: png_buf,
    })
}

/// Resolve PDF ColorSpace to PNG color type and channel count.
///
/// Returns `None` for unsupported color spaces (Indexed, patterns, etc.)
/// to skip the image rather than crash.
fn resolve_color_space(dict: &lopdf::Dictionary) -> Option<ImageColorInfo> {
    let cs = dict.get(b"ColorSpace").ok().and_then(|cs| {
        if let Ok(name) = cs.as_name() {
            Some(name.to_vec())
        } else if let Ok(arr) = cs.as_array() {
            arr.first()
                .and_then(|v| v.as_name().ok())
                .map(|n| n.to_vec())
        } else {
            None
        }
    });

    match cs.as_deref() {
        Some(b"DeviceRGB") | Some(b"CalRGB") => Some(ImageColorInfo {
            png_color_type: png::ColorType::Rgb,
            channels: 3,
            is_cmyk: false,
        }),
        Some(b"DeviceGray") | Some(b"CalGray") => Some(ImageColorInfo {
            png_color_type: png::ColorType::Grayscale,
            channels: 1,
            is_cmyk: false,
        }),
        Some(b"DeviceCMYK") => Some(ImageColorInfo {
            png_color_type: png::ColorType::Rgb, // will be converted
            channels: 4,
            is_cmyk: true,
        }),
        Some(b"ICCBased") => {
            // ICCBased: [/ICCBased stream_ref]. Infer channels from BPC + data,
            // or fall back to RGB. Safe to guess RGB(3) for most ICC profiles.
            Some(ImageColorInfo {
                png_color_type: png::ColorType::Rgb,
                channels: 3,
                is_cmyk: false,
            })
        }
        Some(b"Indexed") => {
            // Indexed requires a PLTE chunk which we don't extract.
            // Skip rather than crash.
            None
        }
        None => {
            // No ColorSpace specified — default to RGB
            Some(ImageColorInfo {
                png_color_type: png::ColorType::Rgb,
                channels: 3,
                is_cmyk: false,
            })
        }
        _ => None, // Unknown color space — skip
    }
}

/// Apply PNG un-filtering to rows with predictor filter bytes.
///
/// PDF spec §7.4.4.4: Predictor values 10–15 correspond to PNG filter types
/// (None=0, Sub=1, Up=2, Average=3, Paeth=4).
fn unfilter_png_rows(
    data: &[u8],
    _width: u32,
    height: u32,
    bytes_per_row: usize,
    channels: usize,
    bpc: u8,
) -> Vec<u8> {
    let stride = bytes_per_row + 1; // row data + filter byte
    let bpp = std::cmp::max(1, (channels * bpc as usize) / 8); // bytes per pixel
    let mut result = vec![0u8; bytes_per_row * height as usize];
    let mut prev_row = vec![0u8; bytes_per_row];

    for row in 0..height as usize {
        let src_offset = row * stride;
        if src_offset >= data.len() {
            break;
        }
        let filter_type = data[src_offset];
        let src_start = src_offset + 1;
        let src_end = std::cmp::min(src_start + bytes_per_row, data.len());
        let dst_start = row * bytes_per_row;

        let row_data = &data[src_start..src_end];
        let dst = &mut result[dst_start..dst_start + bytes_per_row];

        match filter_type {
            0 => {
                // None
                dst[..row_data.len()].copy_from_slice(row_data);
            }
            1 => {
                // Sub: each byte is added to the byte `bpp` positions to the left
                for i in 0..row_data.len() {
                    let left = if i >= bpp { dst[i - bpp] } else { 0 };
                    dst[i] = row_data[i].wrapping_add(left);
                }
            }
            2 => {
                // Up: each byte is added to the byte directly above
                for i in 0..row_data.len() {
                    dst[i] = row_data[i].wrapping_add(prev_row[i]);
                }
            }
            3 => {
                // Average: each byte uses the average of left and above
                for i in 0..row_data.len() {
                    let left = if i >= bpp { dst[i - bpp] as u16 } else { 0 };
                    let above = prev_row[i] as u16;
                    dst[i] = row_data[i].wrapping_add(((left + above) / 2) as u8);
                }
            }
            4 => {
                // Paeth: each byte uses the Paeth predictor of left, above, upper-left
                for i in 0..row_data.len() {
                    let left = if i >= bpp { dst[i - bpp] as i32 } else { 0 };
                    let above = prev_row[i] as i32;
                    let upper_left = if i >= bpp {
                        prev_row[i - bpp] as i32
                    } else {
                        0
                    };
                    dst[i] = row_data[i].wrapping_add(paeth_predictor(left, above, upper_left));
                }
            }
            _ => {
                // Unknown filter — treat as None
                dst[..row_data.len()].copy_from_slice(row_data);
            }
        }

        prev_row[..bytes_per_row].copy_from_slice(dst);
    }

    result
}

#[inline]
fn paeth_predictor(a: i32, b: i32, c: i32) -> u8 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Convert CMYK pixel data to RGB.
///
/// Uses the standard formula: R = 255 × (1-C) × (1-K), etc.
fn cmyk_to_rgb(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixel_count = (width * height) as usize;
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for i in 0..pixel_count {
        let offset = i * 4;
        if offset + 3 >= data.len() {
            break;
        }
        let c = data[offset] as f32 / 255.0;
        let m = data[offset + 1] as f32 / 255.0;
        let y = data[offset + 2] as f32 / 255.0;
        let k = data[offset + 3] as f32 / 255.0;
        rgb.push((255.0 * (1.0 - c) * (1.0 - k)) as u8);
        rgb.push((255.0 * (1.0 - m) * (1.0 - k)) as u8);
        rgb.push((255.0 * (1.0 - y) * (1.0 - k)) as u8);
    }
    rgb
}

/// Counts of text operators with horizontal vs rotated combined matrices.
struct RotationVotes {
    horizontal: u32,
    rotated: u32,
}

/// Detect if most text items on a page are rotated 90° or 270°, and if so,
/// swap x↔y coordinates (plus widths/heights) so the layout engine sees
/// them as horizontal text on a landscape page.
fn correct_rotated_page(
    mut items: Vec<TextItem>,
    mut rects: Vec<PdfRect>,
    mut lines: Vec<PdfLine>,
    votes: &RotationVotes,
) -> (Vec<TextItem>, Vec<PdfRect>, Vec<PdfLine>, bool) {
    if items.len() < 2 {
        return (items, rects, lines, false);
    }

    // Use the combined-matrix direction votes collected during extraction.
    // For normal text, combined[0] (the x-component of the text x-axis) is
    // large; for 90° rotated text, combined[1] dominates instead.
    let total_votes = votes.horizontal + votes.rotated;
    if total_votes == 0 || votes.rotated * 3 < total_votes * 2 {
        // Less than ~67% of text operators are rotated → not a rotated page
        return (items, rects, lines, false);
    }

    log::debug!(
        "detected rotated page text: {}/{} text ops are rotated — swapping coordinates",
        votes.rotated,
        total_votes
    );

    // For 90° CCW rotation (the common case: Tm = [0, b, -b, 0, tx, ty]):
    //   device x increases = visual "down"   → negate when mapping to y
    //   device y increases = visual "right"   → use directly as x
    // The layout engine sorts by y descending (highest = top of page), so
    // we negate old_x so that visual-top (low device x) gets high new_y.
    for item in &mut items {
        let new_x = item.y;
        let new_y = -item.x;
        item.x = new_x;
        item.y = new_y;
        // For rotated text, the "width" along the reading direction was
        // lost (computed as 0 due to scale_x ≈ 0).  Estimate from text
        // length × approximate char width.  font_size is the rendered
        // height in device space, which for 90° rotation corresponds to
        // the horizontal extent of one em.
        if item.width < 0.5 {
            let char_count = item.text.chars().count() as f32;
            item.width = char_count * item.font_size * 0.5;
        }
    }

    // Transform rectangles
    for rect in &mut rects {
        let new_x = rect.y;
        let new_y = -(rect.x + rect.width.abs());
        rect.x = new_x;
        rect.y = new_y;
        std::mem::swap(&mut rect.width, &mut rect.height);
    }

    // Transform lines
    for line in &mut lines {
        let new_x1 = line.y1;
        let new_y1 = -line.x1;
        let new_x2 = line.y2;
        let new_y2 = -line.x2;
        line.x1 = new_x1;
        line.y1 = new_y1;
        line.x2 = new_x2;
        line.y2 = new_y2;
    }

    (items, rects, lines, true)
}

/// Remove near-duplicate rects (same coordinates within 0.5 pt tolerance).
/// Some PDFs emit a full-page clip path for every text block, producing
/// thousands of identical rects. After dedup these collapse to one rect,
/// which is too few for table detection and gets naturally skipped.
fn dedup_rects(rects: &mut Vec<PdfRect>) {
    if rects.len() <= 1 {
        return;
    }
    // Round to 0.5-pt grid for tolerance, then sort and dedup.
    rects.sort_by(|a, b| {
        let ak = (
            a.page,
            (a.x * 2.0) as i32,
            (a.y * 2.0) as i32,
            (a.width * 2.0) as i32,
            (a.height * 2.0) as i32,
        );
        let bk = (
            b.page,
            (b.x * 2.0) as i32,
            (b.y * 2.0) as i32,
            (b.width * 2.0) as i32,
            (b.height * 2.0) as i32,
        );
        ak.cmp(&bk)
    });
    rects.dedup_by(|a, b| {
        a.page == b.page
            && ((a.x - b.x).abs() < 0.5)
            && ((a.y - b.y).abs() < 0.5)
            && ((a.width - b.width).abs() < 0.5)
            && ((a.height - b.height).abs() < 0.5)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32, page: u32) -> PdfRect {
        PdfRect {
            x,
            y,
            width: w,
            height: h,
            page,
        }
    }

    #[test]
    fn test_dedup_rects_identical() {
        let mut rects = vec![rect(0.0, 0.0, 612.0, 792.0, 1); 3759];
        dedup_rects(&mut rects);
        assert_eq!(rects.len(), 1);
    }

    #[test]
    fn test_dedup_rects_within_tolerance() {
        let mut rects = vec![
            rect(10.0, 20.0, 100.0, 50.0, 1),
            rect(10.2, 20.1, 100.3, 50.4, 1),
        ];
        dedup_rects(&mut rects);
        assert_eq!(rects.len(), 1);
    }

    #[test]
    fn test_dedup_rects_distinct_kept() {
        let mut rects = vec![
            rect(10.0, 20.0, 100.0, 50.0, 1),
            rect(120.0, 20.0, 100.0, 50.0, 1),
            rect(10.0, 80.0, 100.0, 50.0, 1),
        ];
        dedup_rects(&mut rects);
        assert_eq!(rects.len(), 3);
    }

    #[test]
    fn test_dedup_rects_different_pages_kept() {
        let mut rects = vec![
            rect(0.0, 0.0, 612.0, 792.0, 1),
            rect(0.0, 0.0, 612.0, 792.0, 2),
        ];
        dedup_rects(&mut rects);
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn test_dedup_rects_empty_and_single() {
        let mut empty: Vec<PdfRect> = vec![];
        dedup_rects(&mut empty);
        assert!(empty.is_empty());

        let mut single = vec![rect(1.0, 2.0, 3.0, 4.0, 1)];
        dedup_rects(&mut single);
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn test_skip_excessive_operations() {
        use crate::tounicode::FontCMaps;
        use lopdf::{dictionary, Object, Stream};

        let mut doc = lopdf::Document::new();

        // "0 0 m\n" = 6 bytes per op, 1_100_000 ops → ~6.6 MB content stream
        let ops_bytes = "0 0 m\n".repeat(1_100_000).into_bytes();
        let stream = Stream::new(dictionary! {}, ops_bytes);
        let content_id = doc.add_object(Object::Stream(stream));

        let page_dict = dictionary! {
            "Type" => "Page",
            "Contents" => Object::Reference(content_id),
            "Resources" => dictionary! {},
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        let page_id = doc.add_object(page_dict);

        // Register the page so get_page_content can find it
        let pages_dict = dictionary! {
            "Type" => "Pages",
            "Count" => Object::Integer(1),
            "Kids" => vec![Object::Reference(page_id)],
        };
        let pages_id = doc.add_object(pages_dict);
        let catalog = dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        };
        doc.add_object(catalog);

        let font_cmaps = FontCMaps::from_doc(&doc);
        let result = extract_page_text_items(&doc, page_id, 1, &font_cmaps, false).unwrap();
        let ((items, rects, lines), _images, _has_gid, _coords_rotated) = result;
        assert!(items.is_empty());
        assert!(rects.is_empty());
        assert!(lines.is_empty());
    }

    #[test]
    fn test_strip_pdf_comments() {
        // Basic comment stripping
        let input = b"BT\n% comment\nTj\nET\n";
        let output = strip_pdf_comments(input);
        assert_eq!(output, b"BT\n \nTj\nET\n");

        // No comments = unchanged
        let input = b"BT\nTj\nET\n";
        let output = strip_pdf_comments(input);
        assert_eq!(output, input.to_vec());

        // Don't strip inside string literals
        let input = b"(text with % not a comment)\n% real comment\n";
        let output = strip_pdf_comments(input);
        assert_eq!(output, b"(text with % not a comment)\n \n");

        // Don't strip inside hex strings
        let input = b"<0033% not a comment>\n% real comment\n";
        let output = strip_pdf_comments(input);
        assert_eq!(output, b"<0033% not a comment>\n \n");

        // PD4ML style: comment between Tj and ET
        let input = b"<0033> Tj\n\t% Mission Statement\n\tET\n";
        let output = strip_pdf_comments(input);
        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("ET"),
            "ET should be preserved after comment stripping"
        );
    }
}
