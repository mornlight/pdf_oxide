//! Text rasterizer - renders PDF text using tiny-skia.
//!
//! Text rendering in PDF is complex because:
//! - Fonts may be embedded or use standard PDF fonts
//! - Character encoding varies (identity-H, MacRoman, custom ToUnicode, etc.)
#![allow(clippy::collapsible_if, clippy::vec_box)]
//! - Glyph positioning is explicit via TJ arrays
//!
//! This module provides a text rendering implementation that:
//! - Uses system fonts as fallback when embedded fonts aren't available
//! - Renders text using rustybuzz for shaping and tiny-skia for drawing glyph paths

use super::create_fill_paint;
use crate::content::operators::TextElement;
use crate::content::GraphicsState;
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::fonts::system_font::load_system_font_data;
use crate::object::Object;
use std::collections::HashMap;
use std::sync::Arc;

use tiny_skia::{Paint, PathBuilder, Pixmap, Transform};
use ttf_parser::OutlineBuilder;

/// Outline builder that converts ttf-parser paths to tiny-skia paths.
struct SkiaOutlineBuilder<'a>(&'a mut PathBuilder);

impl<'a> OutlineBuilder for SkiaOutlineBuilder<'a> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// Returns true if the font's only usable cmap subtable is a byte-indexed
/// (single-byte input) table — typically Macintosh Roman format 0 on
/// producer-stripped TrueType subsets.
fn has_byte_indexed_cmap(font_data: &[u8]) -> bool {
    let face = match ttf_parser::Face::parse(font_data, 0) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let cmap = match face.tables().cmap {
        Some(c) => c,
        None => return false,
    };
    let mut saw_byte_indexed = false;
    let mut saw_unicode = false;
    for sub in cmap.subtables {
        use ttf_parser::PlatformId;
        match sub.platform_id {
            PlatformId::Unicode => saw_unicode = true,
            PlatformId::Windows if sub.encoding_id == 1 || sub.encoding_id == 10 => {
                saw_unicode = true
            },
            PlatformId::Macintosh if sub.encoding_id == 0 => saw_byte_indexed = true,
            _ => {},
        }
    }
    saw_byte_indexed && !saw_unicode
}

/// Resolve a single PDF content byte to a GID by consulting the font's
/// own cmap subtables. Prefers a byte-indexed (Macintosh Roman) subtable
/// when present; falls back to ttf-parser's default Unicode resolution
/// for ASCII-range bytes if no byte-indexed subtable exists.
fn cmap_byte_to_gid(face: &ttf_parser::Face, byte: u8) -> Option<u16> {
    if let Some(cmap) = face.tables().cmap {
        for sub in cmap.subtables {
            use ttf_parser::PlatformId;
            if matches!(sub.platform_id, PlatformId::Macintosh) && sub.encoding_id == 0 {
                if let Some(gid) = sub.glyph_index(byte as u32) {
                    return Some(gid.0);
                }
            }
        }
    }
    face.glyph_index(byte as char).map(|g| g.0)
}

/// Process-wide cache for the system font database.
///
/// `fontdb::Database::load_system_fonts()` walks every font directory on
/// the host and parses each face it finds, which typically takes several
/// seconds on first call. Before this cache was introduced, every
/// `TextRasterizer::new()` (and therefore every `PageRenderer::new()`)
/// paid that cost, and callers who constructed a fresh `PageRenderer`
/// per page — which is the obvious first-draft usage from the Python /
/// CLI surface — hit the scan once per page. A cold-cache ORAFOL 5400
/// render took ~4.1 s on a warm machine for a single page because of
/// this. See issue #331.
///
/// Switching to a process-wide `OnceLock<Arc<fontdb::Database>>` loads
/// the database exactly once per process, and every subsequent
/// `TextRasterizer` constructor takes a cheap `Arc::clone`. Wrapping
/// in `Arc` is important so that the cache is still cheaply shareable
/// across `TextRasterizer` instances in different rendering contexts
/// without re-copying the full parsed font metadata. Callers that want
/// a private / modified database can still construct one by hand and
/// bypass this cache via `TextRasterizer::with_fontdb()`.
static SYSTEM_FONTDB: std::sync::OnceLock<std::sync::Arc<fontdb::Database>> =
    std::sync::OnceLock::new();

fn system_fontdb() -> std::sync::Arc<fontdb::Database> {
    SYSTEM_FONTDB
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            std::sync::Arc::new(db)
        })
        .clone()
}

/// Rasterizer for PDF text operations.
pub struct TextRasterizer {
    /// Font database for system font fallback.
    ///
    /// Shared across rasterizers via a process-wide `OnceLock` cache so
    /// we don't re-scan the system font directories on every new
    /// `PageRenderer`. See the `SYSTEM_FONTDB` docstring for the
    /// measurement that motivated the switch.
    fontdb: std::sync::Arc<fontdb::Database>,
}

impl TextRasterizer {
    /// Create a new text rasterizer using the cached system font database.
    pub fn new() -> Self {
        Self {
            fontdb: system_fontdb(),
        }
    }

    /// Construct with a caller-supplied font database. Bypasses the
    /// process-wide cache — useful for tests or callers that need to
    /// pre-populate the database with non-system fonts.
    #[allow(dead_code)]
    pub fn with_fontdb(fontdb: std::sync::Arc<fontdb::Database>) -> Self {
        Self { fontdb }
    }

    /// Render a text string (Tj operator).
    /// Returns the total horizontal advance in PDF points.
    #[allow(unused_variables)]
    pub fn render_text(
        &self,
        pixmap: &mut Pixmap,
        text: &[u8],
        base_transform: Transform,
        gs: &GraphicsState,
        _resources: &Object,
        doc: &mut PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
        font_cache: &HashMap<String, Arc<crate::fonts::FontInfo>>,
    ) -> Result<f32> {
        // Get font info from cache
        let font_info = if let Some(font_name) = &gs.font_name {
            font_cache.get(font_name).cloned()
        } else {
            None
        };

        // Convert raw PDF bytes to Unicode string using font encoding
        let unicode_text = self.decode_text_to_unicode(text, font_info.as_deref());
        log::debug!("Decoded text: '{}' (font={:?})", unicode_text, gs.font_name);

        // Create paint from fill color
        let mut paint = create_fill_paint(gs, "Normal");
        // Text rendering mode 3 = invisible text (used for searchable OCR layers)
        if gs.render_mode == 3 {
            paint.set_color(tiny_skia::Color::from_rgba(0.0, 0.0, 0.0, 0.0).unwrap());
        }

        // Find and load font - prioritize embedded font data
        let pdf_font_name = gs.font_name.as_deref().unwrap_or("Helvetica");
        let font_data_and_index: Option<(Vec<u8>, u32, bool)> = if let Some(ref info) = font_info {
            if let Some(ref embedded) = info.embedded_font_data {
                // Simple (non-Type0) TrueType subsets whose sole cmap subtable
                // is a byte-indexed table must be rendered by feeding the raw
                // PDF content bytes to the embedded cmap directly — the PDF
                // byte is the cmap input under the font's declared encoding
                // (ISO 32000-1 §9.6.6.4). Unicode shaping against these fonts
                // is unreliable: even if a space or punctuation happens to
                // share a codepoint with a cmap key, shaping for letters
                // resolves to .notdef and the system-font fallback picks up
                // unrelated glyphs. Bypass the Unicode shaping path entirely
                // for this subtype so the byte→GID route is taken for every
                // `Tj` / `TJ` call, not just the ones whose decoded Unicode
                // happens to miss the cmap.
                if info.subtype != "Type0" && has_byte_indexed_cmap(embedded) {
                    log::debug!(
                        "Using embedded font '{}' with byte-indexed cmap (simple TrueType subset)",
                        info.base_font
                    );
                    return self.render_cid_direct(
                        pixmap,
                        text,
                        info,
                        embedded,
                        0,
                        &paint,
                        base_transform,
                        gs,
                        clip_mask,
                    );
                }

                // Validate embedded font: check if rustybuzz can find real glyphs (not .notdef)
                // CID subset fonts often lack standard Unicode cmap tables, so shaping
                // produces gid=0 for every character.
                let usable = if let Some(face) = rustybuzz::Face::from_slice(embedded, 0) {
                    let mut buf = rustybuzz::UnicodeBuffer::new();
                    buf.push_str(&unicode_text);
                    buf.set_direction(rustybuzz::Direction::LeftToRight);
                    let shaped = rustybuzz::shape(&face, &[], buf);
                    let infos = shaped.glyph_infos();
                    infos.iter().any(|g| g.glyph_id != 0)
                } else {
                    false
                };
                if usable {
                    log::debug!("Using embedded font data for '{}'", info.base_font);
                    Some((embedded.to_vec(), 0, false))
                } else if info.subtype == "Type0"
                    && info.cid_to_gid_map.is_some()
                    && info.cid_font_type.as_deref() == Some("CIDFontType2")
                {
                    // CIDFontType2 (TrueType) with CIDToGIDMap — use direct GID rendering
                    // Note: CIDFontType0 (CFF) requires a CFF parser which ttf-parser doesn't handle
                    // for raw CFF data (FontFile3), so those fall back to system fonts.
                    log::debug!(
                        "Using embedded font '{}' with CIDToGIDMap (CIDFontType2)",
                        info.base_font
                    );
                    Some((embedded.to_vec(), 0, true))
                } else if info.cff_gid_map.is_some() {
                    // CFF font with byte→GID mapping — use direct rendering
                    log::debug!(
                        "Using embedded CFF font '{}' with direct GID mapping",
                        info.base_font
                    );
                    Some((embedded.to_vec(), 0, true))
                } else {
                    log::debug!(
                        "Embedded font '{}' lacks usable cmap, falling back to system font",
                        info.base_font
                    );
                    self.load_font_data(&info.base_font)
                        .map(|(d, i)| (d, i, false))
                }
            } else {
                self.load_font_data(&info.base_font)
                    .map(|(d, i)| (d, i, false))
            }
        } else {
            self.load_font_data(pdf_font_name)
                .map(|(d, i)| (d, i, false))
        };

        if let Some((font_data, index, use_cid_to_gid)) = font_data_and_index {
            if use_cid_to_gid {
                // Direct CIDToGIDMap/CFF rendering — bypass rustybuzz, use ttf-parser for glyph outlines
                match self.render_cid_direct(
                    pixmap,
                    text,
                    font_info.as_deref().unwrap(),
                    &font_data,
                    index,
                    &paint,
                    base_transform,
                    gs,
                    clip_mask,
                ) {
                    Ok(advance) => return Ok(advance),
                    Err(e) => {
                        // Fall back to system font if embedded parsing fails
                        log::warn!(
                            "Direct CID/CFF rendering failed: {}, falling back to system font",
                            e
                        );
                        if let Some((fallback_data, fallback_idx)) =
                            self.load_font_data(pdf_font_name)
                        {
                            return self.render_unicode_text(
                                pixmap,
                                &unicode_text,
                                text,
                                font_info.as_deref(),
                                &fallback_data,
                                fallback_idx,
                                &paint,
                                base_transform,
                                gs,
                                clip_mask,
                                pdf_font_name,
                                false,
                            );
                        }
                    },
                }
            }
            Ok(self.render_unicode_text(
                pixmap,
                &unicode_text,
                text, // raw bytes
                font_info.as_deref(),
                &font_data,
                index,
                &paint,
                base_transform,
                gs,
                clip_mask,
                pdf_font_name,
                true, // allow_fallback
            )?)
        } else {
            let font_name = font_info
                .as_ref()
                .map(|i| i.base_font.as_str())
                .unwrap_or("unknown");
            log::warn!(
                "No font found for '{}', text may render incorrectly. \
                 Install common fonts (e.g., liberation-fonts, dejavu-fonts, or noto-fonts).",
                font_name
            );
            // Fallback to simple rendering if font not found
            Ok(self.render_text_fallback(
                pixmap,
                &unicode_text,
                &paint,
                base_transform,
                gs,
                clip_mask,
            )?)
        }
    }

    /// Decode raw PDF text bytes to a Unicode string based on font type.
    fn decode_text_to_unicode(
        &self,
        bytes: &[u8],
        font: Option<&crate::fonts::FontInfo>,
    ) -> String {
        let raw_result = if let Some(font) = font {
            let mut result = String::new();
            // Use pre-computed lookup table for performance if it's a simple font
            if font.subtype != "Type0" {
                let table = font.get_byte_to_char_table();
                for &byte in bytes {
                    let c = table[byte as usize];
                    if c != '\0' {
                        result.push(c);
                    } else {
                        // Fallback: multi-char mapping or unmapped byte
                        let char_str = font
                            .char_to_unicode(byte as u32)
                            .unwrap_or_else(|| fallback_char_to_unicode(byte as u32));
                        if char_str != "\u{FFFD}" {
                            result.push_str(&char_str);
                        }
                    }
                }
            } else {
                // Complex font: use unified iterator for robust multi-byte decoding
                for (char_code, _) in TextCharIter::new(bytes, Some(font)) {
                    let char_str = font
                        .char_to_unicode(char_code as u32)
                        .unwrap_or_else(|| fallback_char_to_unicode(char_code as u32));

                    if char_str != "\u{FFFD}" {
                        result.push_str(&char_str);
                    }
                }
            }
            result
        } else {
            // No font - fallback to Latin-1 (ISO 8859-1) encoding
            bytes.iter().map(|&b| char::from(b)).collect()
        };

        // Filter control characters from failed encoding resolution,
        // and expand presentation-form ligature code points (fi, fl, ffi,
        // ffl, st, ct, …) into their component letters so the shaper
        // passes the cluster through as ordinary glyphs instead of
        // dropping it or producing a lone box. `extract_text` already
        // does this on the extraction path via
        // `ligature_processor::get_ligature_components`; without the
        // same decomposition on the render path, words like
        // "Efficient" rasterize as "Effi  ert" because the shaper can't
        // resolve the ligature cluster against the fallback system
        // font. See issue #331 (R2).
        let mut filtered = String::with_capacity(raw_result.len());
        for c in raw_result.chars() {
            if c < '\x20' && c != '\t' && c != '\n' && c != '\r' {
                continue;
            }
            if let Some(components) = crate::text::ligature_processor::get_ligature_components(c) {
                filtered.push_str(components);
            } else {
                filtered.push(c);
            }
        }
        filtered
    }

    /// Render a TJ array (text with positioning adjustments).
    /// Returns the total horizontal advance in PDF points.
    pub fn render_tj_array(
        &self,
        pixmap: &mut Pixmap,
        array: &[TextElement],
        base_transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &mut PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
        font_cache: &HashMap<String, Arc<crate::fonts::FontInfo>>,
    ) -> Result<f32> {
        let mut current_gs = gs.clone();
        let mut total_advance: f32 = 0.0;

        for element in array {
            match element {
                TextElement::String(text) => {
                    let advance = self.render_text(
                        pixmap,
                        text,
                        base_transform,
                        &current_gs,
                        resources,
                        doc,
                        clip_mask,
                        font_cache,
                    )?;

                    // Advance text position in text space: Tm' = T(advance, 0) * Tm
                    let advance_matrix = crate::content::Matrix::translation(advance, 0.0);
                    current_gs.text_matrix = advance_matrix.multiply(&current_gs.text_matrix);
                    total_advance += advance;
                },
                TextElement::Offset(offset) => {
                    // PDF offsets are in 1/1000th of a unit, and positive shifts text to the left
                    let shift = (-offset / 1000.0) * current_gs.font_size;
                    let advance_matrix = crate::content::Matrix::translation(shift, 0.0);
                    current_gs.text_matrix = advance_matrix.multiply(&current_gs.text_matrix);
                    total_advance += shift;
                },
            }
        }
        Ok(total_advance)
    }

    /// Get font info for a specific font name from resources.
    #[allow(dead_code)]
    fn get_font_info(
        &self,
        doc: &mut PdfDocument,
        resources: &Object,
        font_name: &str,
    ) -> Result<crate::fonts::FontInfo> {
        if let Object::Dictionary(res_dict) = resources {
            if let Some(Object::Dictionary(fonts)) = res_dict.get("Font") {
                if let Some(font_ref) = fonts.get(font_name) {
                    let font_obj = doc.resolve_object(font_ref)?;
                    let info = crate::fonts::FontInfo::from_dict(&font_obj, doc)?;
                    log::debug!("Resolved font '{}': subtype={}, encoding={:?}, has_to_unicode={}, has_embedded={}", 
                        info.base_font, info.subtype, info.encoding, info.to_unicode.is_some(), info.embedded_font_data.is_some());
                    return Ok(info);
                }
            }
        }
        Err(Error::InvalidPdf(format!("Font {} not found", font_name)))
    }

    /// Find and load font data from system.
    fn load_font_data(&self, pdf_font_name: &str) -> Option<(Vec<u8>, u32)> {
        load_system_font_data(pdf_font_name)
    }

    /// Access the font database.
    fn font_db(&self) -> &fontdb::Database {
        &self.fontdb
    }

    /// Render Unicode text using shaped glyphs.
    /// Returns the total horizontal advance in PDF points.
    fn render_unicode_text(
        &self,
        pixmap: &mut Pixmap,
        text: &str,
        bytes: &[u8],
        font_info: Option<&crate::fonts::FontInfo>,
        font_data: &[u8],
        index: u32,
        paint: &Paint,
        base_transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
        pdf_font_name: &str,
        allow_fallback: bool,
    ) -> Result<f32> {
        let font_size = gs.font_size;
        let h_scale = gs.horizontal_scaling / 100.0;

        // 1. Create rustybuzz face and buffer
        let rb_face_opt = rustybuzz::Face::from_slice(font_data, index);

        if rb_face_opt.is_none() {
            if allow_fallback {
                log::warn!("Failed to create rustybuzz face from embedded data for '{}', falling back to system font", pdf_font_name);
                if let Some((fallback_data, fallback_index)) = self.load_font_data(pdf_font_name) {
                    return self.render_unicode_text(
                        pixmap,
                        text,
                        bytes,
                        font_info,
                        &fallback_data,
                        fallback_index,
                        paint,
                        base_transform,
                        gs,
                        clip_mask,
                        pdf_font_name,
                        false, // don't allow infinite fallback
                    );
                }
            }
            return self.render_text_fallback(pixmap, text, paint, base_transform, gs, clip_mask);
        }
        let rb_face = rb_face_opt.unwrap();
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);

        // Explicitly set script and direction for better CJK shaping
        if text
            .chars()
            .any(|c| (c as u32) >= 0x4E00 && (c as u32) <= 0x9FFF)
        {
            if let Some(script) = rustybuzz::Script::from_iso15924_tag(
                rustybuzz::ttf_parser::Tag::from_bytes(b"Hani"),
            ) {
                buffer.set_script(script);
            }
        }
        buffer.set_direction(rustybuzz::Direction::LeftToRight);

        // 2. Shape the text
        let glyphs = rustybuzz::shape(&rb_face, &[], buffer);
        let info = glyphs.glyph_infos();
        let pos = glyphs.glyph_positions();

        // 3. Load ttf-parser face for outlines
        let ttf_face = ttf_parser::Face::parse(font_data, index)
            .map_err(|e| Error::InvalidPdf(format!("Failed to parse font: {}", e)))?;

        let units_per_em = ttf_face.units_per_em() as f32;
        let scale = font_size / units_per_em;
        log::debug!(
            "render_unicode_text: pdf_font={}, units_per_em={}, font_size={}, scale={}",
            pdf_font_name,
            units_per_em,
            font_size,
            scale
        );

        // 4. Transform setup - include full text matrix [Tm]
        let text_transform = Transform::from_row(
            gs.text_matrix.a,
            gs.text_matrix.b,
            gs.text_matrix.c,
            gs.text_matrix.d,
            gs.text_matrix.e,
            gs.text_matrix.f,
        );
        // Transform from text space to pixel space: P_pixel = base_transform * text_transform * P_text
        let combined_base = base_transform.pre_concat(text_transform);

        let mut x_cursor: f32 = 0.0; // In text space units
        let mut last_fallback_cluster: Option<usize> = None;

        // Pre-resolve CIDs for Type0 fonts using our iterator
        let cids: Vec<u16> = if let Some(info) = font_info {
            if info.subtype == "Type0" {
                TextCharIter::new(bytes, Some(info))
                    .map(|(cid, _)| cid)
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Build mapping from Unicode byte offset → character index for correct CID lookup.
        // Rustybuzz clusters are byte offsets into the Unicode string, but we need
        // the character index to map to the corresponding CID.
        let cluster_to_char_idx: HashMap<usize, usize> = text
            .char_indices()
            .enumerate()
            .map(|(char_idx, (byte_offset, _))| (byte_offset, char_idx))
            .collect();

        // 5. Iterate through shaped glyphs
        for i in 0..info.len() {
            let glyph_id = info[i].glyph_id;
            let cluster = info[i].cluster as usize;

            // Get character at this cluster (byte offset)
            let char_at_pos = text[cluster..].chars().next().unwrap_or(' ');

            // Map cluster (Unicode byte offset) to character index
            let char_idx = cluster_to_char_idx.get(&cluster).copied().unwrap_or(0);

            // Determine how many *source* characters this glyph represents.
            // For normal 1:1 glyphs, cluster_chars == 1. For shaped
            // ligatures like the "ffi" glyph (#331 R2), one glyph covers
            // multiple characters and rustybuzz reports them with the
            // same cluster index on every glyph of the cluster. Since we
            // advance the output cursor by the sum of the PDF-declared
            // widths of the *source* characters (per PDF §9.2.4 text-
            // showing advance), we must add the widths of every source
            // character in the ligature cluster to the cursor, not just
            // the first character's width. Otherwise a ligature glyph
            // draws wide but only advances by one character's worth, and
            // subsequent glyphs overwrite the tail of the ligature —
            // exactly the `Efficient` → `Effi ert` symptom reported in
            // #331 on arxiv-style LaTeX-embedded fonts.
            let next_cluster_byte: usize = info
                .get(i + 1)
                .map(|n| n.cluster as usize)
                .unwrap_or(text.len());
            let cluster_chars: usize = text[cluster..next_cluster_byte.min(text.len())]
                .chars()
                .count()
                .max(1);

            // PDF Spec: tx = ((w0 * Tfs) + Tc + Tw) * Th
            // Priority:
            // 1. Explicit /W or /DW from FontInfo (in 1000ths of em),
            //    summed across every source character in the cluster
            //    so ligatures advance by the full cluster's width.
            // 2. Shaped advance from rustybuzz (fallback, already
            //    reflects the ligature's real width because it comes
            //    from the font's horizontal metrics table).
            let pdf_width = if let Some(font_info_ref) = font_info {
                let mut sum = 0.0_f32;
                for k in 0..cluster_chars {
                    let idx = char_idx + k;
                    let char_code = if font_info_ref.subtype == "Type0" {
                        *cids.get(idx).unwrap_or(&0)
                    } else {
                        *bytes.get(idx).unwrap_or(&0) as u16
                    };
                    sum += font_info_ref.get_glyph_width(char_code);
                }
                sum
            } else {
                // No FontInfo, use shaped advance
                pos[i].x_advance as f32 / font_size * 1000.0
            };

            let x_advance = pdf_width * font_size / 1000.0;
            let x_offset = pos[i].x_offset as f32 / units_per_em * font_size;
            let y_offset = pos[i].y_offset as f32 / units_per_em * font_size;

            let mut x_advance_override: Option<f32> = None;

            // Try to get glyph from primary font
            let mut pb = PathBuilder::new();
            let mut builder = SkiaOutlineBuilder(&mut pb);
            let mut has_outline = ttf_face
                .outline_glyph(ttf_parser::GlyphId(glyph_id as u16), &mut builder)
                .is_some();

            if has_outline && glyph_id != 0 {
                if let Some(path) = pb.finish() {
                    let glyph_transform = combined_base
                        .pre_translate((x_cursor + x_offset) * h_scale, y_offset + gs.text_rise)
                        .pre_scale(scale, scale);

                    pixmap.fill_path(
                        &path,
                        paint,
                        tiny_skia::FillRule::Winding,
                        glyph_transform,
                        clip_mask,
                    );
                }
            } else {
                // FALLBACK PATH: If primary font fails, use the cluster offset to find the original character
                // char_at_pos already retrieved above using byte offset

                // Skip empty glyphs for spaces
                if char_at_pos.is_whitespace() {
                    x_cursor += x_advance;
                    x_cursor += gs.char_space;
                    if char_at_pos == ' ' {
                        x_cursor += gs.word_space;
                    }
                    continue;
                }

                // IMPORTANT: Only render fallback character ONCE per cluster
                if last_fallback_cluster == Some(cluster) {
                    x_cursor += x_advance;
                    continue;
                }
                last_fallback_cluster = Some(cluster);

                // Try to find character in fallback CJK fonts
                if let Some((cjk_data, cjk_index)) = self.load_cjk_fallback() {
                    if let Ok(cjk_face) = ttf_parser::Face::parse(&cjk_data, cjk_index) {
                        if let Some(cjk_glyph_id) = cjk_face.glyph_index(char_at_pos) {
                            let mut cjk_pb = PathBuilder::new();
                            let mut cjk_builder = SkiaOutlineBuilder(&mut cjk_pb);
                            if cjk_face
                                .outline_glyph(cjk_glyph_id, &mut cjk_builder)
                                .is_some()
                            {
                                if let Some(cjk_path) = cjk_pb.finish() {
                                    let cjk_scale = font_size / cjk_face.units_per_em() as f32;
                                    let cjk_transform = combined_base
                                        .pre_translate(
                                            (x_cursor + x_offset) * h_scale,
                                            y_offset + gs.text_rise,
                                        )
                                        .pre_scale(cjk_scale, -cjk_scale);
                                    pixmap.fill_path(
                                        &cjk_path,
                                        paint,
                                        tiny_skia::FillRule::Winding,
                                        cjk_transform,
                                        clip_mask,
                                    );
                                    has_outline = true;

                                    // Set override advance from fallback font
                                    if let Some(adv) = cjk_face.glyph_hor_advance(cjk_glyph_id) {
                                        x_advance_override = Some(
                                            adv as f32 / cjk_face.units_per_em() as f32 * font_size,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                if !has_outline {
                    log::debug!(
                        "No glyph outline found for char='{}' (0x{:X})",
                        char_at_pos,
                        char_at_pos as u32
                    );
                }
            }

            // Advance cursor in text space
            // PDF spec: tx = ((w0 * Tfs) + Tc + Tw) * Th
            // Note: x_advance already includes w0 * Tfs
            x_cursor += x_advance_override.unwrap_or(x_advance);

            // Add character spacing (Tc)
            x_cursor += gs.char_space;

            if char_at_pos == ' ' {
                // Add word spacing (Tw) for space characters
                x_cursor += gs.word_space;
            }
        }

        Ok(x_cursor)
    }
    /// Render text using direct CID-to-GID mapping, bypassing rustybuzz shaping.
    /// Used for CID subset fonts that have embedded data but no usable Unicode cmap.
    /// Per PDF spec section 9.7.4, CIDToGIDMap maps CIDs to glyph indices in the TrueType font.
    fn render_cid_direct(
        &self,
        pixmap: &mut Pixmap,
        bytes: &[u8],
        font_info: &crate::fonts::FontInfo,
        font_data: &[u8],
        index: u32,
        paint: &Paint,
        base_transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Result<f32> {
        let font_size = gs.font_size;
        let h_scale = gs.horizontal_scaling / 100.0;

        let ttf_face = ttf_parser::Face::parse(font_data, index)
            .map_err(|e| Error::InvalidPdf(format!("Failed to parse embedded font: {}", e)))?;
        let units_per_em = ttf_face.units_per_em() as f32;
        let scale = font_size / units_per_em;

        let text_transform = Transform::from_row(
            gs.text_matrix.a,
            gs.text_matrix.b,
            gs.text_matrix.c,
            gs.text_matrix.d,
            gs.text_matrix.e,
            gs.text_matrix.f,
        );
        let combined_base = base_transform.pre_concat(text_transform);

        let mut x_cursor: f32 = 0.0;

        // Iterate over character codes from the raw bytes
        for (char_code, _bytes_consumed) in TextCharIter::new(bytes, Some(font_info)) {
            // Map character code to GID based on font type:
            // - CIDFontType2: CIDToGIDMap maps CID → GID
            // - CFF simple font: cff_gid_map maps byte → GID
            // - Simple TrueType: consult the embedded font's cmap directly
            //   (the PDF content byte is the cmap input under the font's
            //   declared encoding; ISO 32000-1 §9.6.6.4).
            // - Default: identity mapping
            let gid = if let Some(cff_map) = &font_info.cff_gid_map {
                *cff_map.get(&(char_code as u8)).unwrap_or(&0)
            } else if font_info.subtype != "Type0" && font_info.cid_to_gid_map.is_none() {
                cmap_byte_to_gid(&ttf_face, char_code as u8).unwrap_or(0)
            } else {
                match &font_info.cid_to_gid_map {
                    Some(crate::fonts::CIDToGIDMap::Identity) => char_code,
                    Some(crate::fonts::CIDToGIDMap::Explicit(map)) => {
                        *map.get(char_code as usize).unwrap_or(&0)
                    },
                    None => char_code,
                }
            };
            let cid = char_code; // For width lookup

            // Get width from PDF metrics
            let pdf_width = font_info.get_glyph_width(cid);
            let x_advance = pdf_width * font_size / 1000.0;

            // Get Unicode character for space detection
            let char_str = font_info.char_to_unicode(cid as u32).unwrap_or_default();
            let char_at_pos = char_str.chars().next().unwrap_or(' ');

            // Draw glyph outline
            if gid != 0 || char_at_pos.is_whitespace() {
                if !char_at_pos.is_whitespace() {
                    let mut pb = PathBuilder::new();
                    let mut builder = SkiaOutlineBuilder(&mut pb);
                    if ttf_face
                        .outline_glyph(ttf_parser::GlyphId(gid), &mut builder)
                        .is_some()
                    {
                        if let Some(path) = pb.finish() {
                            let glyph_transform = combined_base
                                .pre_translate(x_cursor * h_scale, gs.text_rise)
                                .pre_scale(scale, scale);
                            pixmap.fill_path(
                                &path,
                                paint,
                                tiny_skia::FillRule::Winding,
                                glyph_transform,
                                clip_mask,
                            );
                        }
                    }
                }
            }

            x_cursor += x_advance;
            x_cursor += gs.char_space;
            if char_at_pos == ' ' {
                x_cursor += gs.word_space;
            }
        }

        Ok(x_cursor)
    }

    /// Load a dedicated CJK fallback font.
    fn load_cjk_fallback(&self) -> Option<(Vec<u8>, u32)> {
        // Prioritize Simplified Chinese (SC) variants first
        let prioritized_variants = [
            "Noto Sans CJK SC",
            "Noto Serif CJK SC",
            "Droid Sans Fallback",
            "SimSun",
            "WenQuanYi Micro Hei",
            "Noto Sans CJK JP",
            "Noto Serif CJK JP",
        ];

        for variant in prioritized_variants {
            let families = [fontdb::Family::Name(variant)];
            let query = fontdb::Query {
                families: &families,
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };

            if let Some(id) = self.font_db().query(&query) {
                let mut data = None;
                self.font_db().with_face_data(id, |face_data, index| {
                    log::debug!(
                        "CJK Fallback matched variant '{}': index={}, size={} bytes",
                        variant,
                        index,
                        face_data.len()
                    );
                    data = Some((face_data.to_vec(), index));
                });
                if data.is_some() {
                    return data;
                }
            }
        }

        // Generic fallback if no specific CJK font found
        let query = fontdb::Query {
            families: &[fontdb::Family::SansSerif],
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };

        if let Some(id) = self.font_db().query(&query) {
            let mut data = None;
            self.font_db().with_face_data(id, |face_data, index| {
                data = Some((face_data.to_vec(), index));
            });
            data
        } else {
            None
        }
    }

    /// Fallback simple rendering if no font found.
    /// Returns the total horizontal advance in PDF points.
    fn render_text_fallback(
        &self,
        pixmap: &mut Pixmap,
        text: &str,
        paint: &Paint,
        base_transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Result<f32> {
        // Just draw rectangles for now as very last resort
        let font_size = gs.font_size;
        let char_width = font_size * 0.6;
        let mut x_cursor: f32 = 0.0;
        let h_scale = gs.horizontal_scaling / 100.0;

        let text_transform = Transform::from_row(
            gs.text_matrix.a,
            gs.text_matrix.b,
            gs.text_matrix.c,
            gs.text_matrix.d,
            gs.text_matrix.e,
            gs.text_matrix.f,
        );
        let transform = base_transform.pre_concat(text_transform);

        for c in text.chars() {
            if !c.is_whitespace() {
                let mut pb = PathBuilder::new();
                if let Some(rect) = tiny_skia::Rect::from_xywh(
                    x_cursor * h_scale,
                    0.0,
                    char_width * 0.8,
                    font_size * 0.8,
                ) {
                    pb.push_rect(rect);
                    if let Some(path) = pb.finish() {
                        pixmap.fill_path(
                            &path,
                            paint,
                            tiny_skia::FillRule::Winding,
                            transform,
                            clip_mask,
                        );
                    }
                }
            }

            x_cursor += (char_width + gs.char_space) / h_scale;
            if c == ' ' {
                x_cursor += gs.word_space / h_scale;
            }
        }

        Ok(x_cursor * h_scale)
    }
}

/// Byte grouping mode for CID font character code decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteMode {
    /// Single-byte codes (simple fonts, some predefined CMaps)
    OneByte,
    /// Always 2-byte codes (Identity-H/V, UCS2)
    TwoByte,
    /// Shift-JIS variable-width (1 or 2 bytes depending on lead byte)
    ShiftJIS,
}

/// Get byte grouping mode for a font.
fn get_byte_mode(font: Option<&crate::fonts::FontInfo>) -> ByteMode {
    if let Some(font) = font {
        if font.subtype == "Type0" {
            match &font.encoding {
                crate::fonts::Encoding::Identity => ByteMode::TwoByte,
                crate::fonts::Encoding::Standard(name) => {
                    if (name.contains("Identity") && !name.contains("OneByteIdentity"))
                        || name.contains("UCS2")
                        || name.contains("UTF16")
                    {
                        ByteMode::TwoByte
                    } else if name.contains("RKSJ") {
                        ByteMode::ShiftJIS
                    } else if name.contains("EUC")
                        || name.contains("GBK")
                        || name.contains("GBpc")
                        || name.contains("GB-")
                        || name.contains("CNS")
                        || name.contains("B5")
                        || name.contains("KSC")
                        || name.contains("KSCms")
                    {
                        ByteMode::TwoByte
                    } else {
                        ByteMode::OneByte
                    }
                },
                _ => ByteMode::OneByte,
            }
        } else {
            ByteMode::OneByte
        }
    } else {
        ByteMode::OneByte
    }
}

/// Iterator over characters in a PDF string based on font encoding.
struct TextCharIter<'a> {
    bytes: &'a [u8],
    byte_mode: ByteMode,
    index: usize,
}

impl<'a> TextCharIter<'a> {
    fn new(bytes: &'a [u8], font: Option<&crate::fonts::FontInfo>) -> Self {
        Self {
            bytes,
            byte_mode: get_byte_mode(font),
            index: 0,
        }
    }
}

impl<'a> Iterator for TextCharIter<'a> {
    type Item = (u16, usize); // (char_code, bytes_consumed)

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.bytes.len() {
            return None;
        }

        let (char_code, bytes_consumed) = match self.byte_mode {
            ByteMode::TwoByte if self.index + 1 < self.bytes.len() => {
                (((self.bytes[self.index] as u16) << 8) | (self.bytes[self.index + 1] as u16), 2)
            },
            ByteMode::ShiftJIS => {
                let b = self.bytes[self.index];
                let is_lead = (0x81..=0x9F).contains(&b) || (0xE0..=0xFC).contains(&b);
                if is_lead && self.index + 1 < self.bytes.len() {
                    (((b as u16) << 8) | (self.bytes[self.index + 1] as u16), 2)
                } else {
                    (b as u16, 1)
                }
            },
            _ => (self.bytes[self.index] as u16, 1),
        };

        self.index += bytes_consumed;
        Some((char_code, bytes_consumed))
    }
}

/// Fallback function to map common character codes to Unicode when ToUnicode CMap fails.
fn fallback_char_to_unicode(char_code: u32) -> String {
    match char_code {
        0x2014 => "—".to_string(),
        0x2013 => "–".to_string(),
        0x2018 => "\u{2018}".to_string(),
        0x2019 => "\u{2019}".to_string(),
        0x201C => "\u{201C}".to_string(),
        0x201D => "\u{201D}".to_string(),
        0x2022 => "•".to_string(),
        0x2026 => "…".to_string(),
        0x00B0 => "°".to_string(),
        0x00B1 => "±".to_string(),
        0x00D7 => "×".to_string(),
        0x00F7 => "÷".to_string(),
        0x2202 => "∂".to_string(),
        0x2207 => "∇".to_string(),
        0x220F => "∏".to_string(),
        0x2211 => "∑".to_string(),
        0x221A => "√".to_string(),
        0x221E => "∞".to_string(),
        0x2260 => "≠".to_string(),
        0x2261 => "≡".to_string(),
        0x2264 => "≤".to_string(),
        0x2265 => "≥".to_string(),
        code => {
            if let Some(ch) = char::from_u32(code) {
                ch.to_string()
            } else {
                "\u{FFFD}".to_string()
            }
        },
    }
}

impl Default for TextRasterizer {
    fn default() -> Self {
        Self::new()
    }
}
