use byteorder::{BigEndian, ReadBytesExt};
/// TrueType cmap table extraction for font character mapping
///
/// This module extracts Unicode mappings from TrueType font cmap tables,
/// providing a fallback for Type0 fonts missing ToUnicode CMaps.
///
/// The cmap table maps character codes to glyph IDs, and Unicode-capable
/// subtables can also be inverted to recover glyph-to-Unicode mappings.
/// We support formats 0, 4 (BMP), 6 (trimmed), and 12 (Unicode full).
use std::collections::HashMap;
use std::io::Cursor;

/// Mac Roman (platform 1, encoding 0) high-half → Unicode mapping.
///
/// Bytes 0x00..0x7F are identical to ASCII and are handled by the caller.
/// This table covers 0x80..0xFF per Apple's Mac Roman → Unicode reference.
#[rustfmt::skip]
const MAC_ROMAN_HIGH: [char; 128] = [
    'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á',
    'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è',
    'ê', 'ë', 'í', 'ì', 'î', 'ï', 'ñ', 'ó',
    'ò', 'ô', 'ö', 'õ', 'ú', 'ù', 'û', 'ü',
    '†', '°', '¢', '£', '§', '•', '¶', 'ß',
    '®', '©', '™', '´', '¨', '≠', 'Æ', 'Ø',
    '∞', '±', '≤', '≥', '¥', 'µ', '∂', '∑',
    '∏', 'π', '∫', 'ª', 'º', 'Ω', 'æ', 'ø',
    '¿', '¡', '¬', '√', 'ƒ', '≈', '∆', '«',
    '»', '…', '\u{00A0}', 'À', 'Ã', 'Õ', 'Œ', 'œ',
    '–', '—', '"', '"', '\'', '\'', '÷', '◊',
    'ÿ', 'Ÿ', '⁄', '€', '‹', '›', 'ﬁ', 'ﬂ',
    '‡', '·', '‚', '„', '‰', 'Â', 'Ê', 'Á',
    'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô',
    '\u{F8FF}', 'Ò', 'Ú', 'Û', 'Ù', 'ı', 'ˆ', '˜',
    '¯', '˘', '˙', '˚', '¸', '˝', '˛', 'ˇ',
];

#[inline]
fn mac_roman_to_unicode(byte: u8) -> char {
    debug_assert!(byte >= 0x80);
    MAC_ROMAN_HIGH[(byte - 0x80) as usize]
}

#[derive(Debug, Default)]
struct ParsedCMapSubtable {
    gid_to_unicode: HashMap<u16, char>,
    char_code_to_gid: HashMap<u16, u16>,
}

/// Represents a TrueType cmap table extracted from an embedded font
#[derive(Debug, Clone)]
pub struct TrueTypeCMap {
    /// Mapping from Glyph ID to Unicode character
    gid_to_unicode: HashMap<u16, char>,
    /// Mapping from font character code to glyph ID.
    ///
    /// This is needed for embedded subset fonts that expose only a simple
    /// non-Unicode cmap (for example format 0 Mac Roman tables).
    char_code_to_gid: HashMap<u16, u16>,
}

impl TrueTypeCMap {
    /// Parse TrueType cmap table from font data
    ///
    /// The TrueType sfnt structure contains a directory of tables.
    /// We locate the 'cmap' table and parse the best available subtable.
    ///
    /// Priority for Unicode-capable subtables:
    /// 1. Platform 3 (Windows), Encoding 10 (Unicode full repertoire)
    /// 2. Platform 3 (Windows), Encoding 1 (Unicode BMP)
    /// 3. Platform 0 (Unicode), any encoding
    ///
    /// If no Unicode-capable subtable exists, we still keep the best available
    /// character-code map for glyph-id recovery.
    pub fn from_font_data(data: &[u8]) -> Result<Self, String> {
        let mut cursor = Cursor::new(data);

        // Parse sfnt header to locate table directory
        let (num_tables, search_range, entry_selector, range_shift) =
            Self::parse_sfnt_header(&mut cursor)?;

        // Find cmap table entry in the directory
        let cmap_offset = Self::find_cmap_table(
            &mut cursor,
            num_tables,
            search_range,
            entry_selector,
            range_shift,
        )?;

        // Parse cmap table and find the best subtable
        cursor.set_position(cmap_offset as u64);
        let cmap_version = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read cmap version: {}", e))?;

        if cmap_version != 0 {
            return Err(format!("Unsupported cmap table version: {}", cmap_version));
        }

        let num_subtables = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read cmap subtable count: {}", e))?;

        let mut subtables = Vec::with_capacity(num_subtables as usize);

        for _ in 0..num_subtables {
            let platform_id = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read platform ID: {}", e))?;
            let encoding_id = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read encoding ID: {}", e))?;
            let offset = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read subtable offset: {}", e))?;
            subtables.push((platform_id, encoding_id, offset));
        }

        let best_unicode_subtable = subtables
            .iter()
            .copied()
            .max_by_key(|(platform_id, encoding_id, _)| {
                Self::unicode_subtable_priority(*platform_id, *encoding_id)
            })
            .filter(|(platform_id, encoding_id, _)| {
                Self::unicode_subtable_priority(*platform_id, *encoding_id) >= 0
            });

        let best_charcode_subtable = subtables
            .iter()
            .copied()
            .max_by_key(|(platform_id, encoding_id, _)| {
                Self::charcode_subtable_priority(*platform_id, *encoding_id)
            })
            .filter(|(platform_id, encoding_id, _)| {
                Self::charcode_subtable_priority(*platform_id, *encoding_id) >= 0
            });

        let Some((_, _, charcode_offset)) = best_charcode_subtable else {
            return Err("No suitable cmap subtable found".to_string());
        };

        cursor.set_position((cmap_offset + charcode_offset) as u64);
        let parsed_charcode_map = Self::parse_cmap_subtable(&mut cursor)?;

        let gid_to_unicode =
            if let Some((platform_id, encoding_id, subtable_offset)) = best_unicode_subtable {
                log::debug!(
                    "TrueType cmap: selected unicode platform={} encoding={} offset={}",
                    platform_id,
                    encoding_id,
                    subtable_offset
                );
                cursor.set_position((cmap_offset + subtable_offset) as u64);
                Self::parse_cmap_subtable(&mut cursor)?.gid_to_unicode
            } else {
                parsed_charcode_map.gid_to_unicode.clone()
            };

        Ok(TrueTypeCMap {
            gid_to_unicode,
            char_code_to_gid: parsed_charcode_map.char_code_to_gid,
        })
    }

    /// Get Unicode character for a glyph ID
    pub fn get_unicode(&self, gid: u16) -> Option<char> {
        self.gid_to_unicode.get(&gid).copied()
    }

    /// Get glyph ID for a font character code.
    pub(crate) fn get_gid_for_char_code(&self, char_code: u16) -> Option<u16> {
        self.char_code_to_gid.get(&char_code).copied()
    }

    /// Get the number of glyph mappings
    pub fn len(&self) -> usize {
        self.gid_to_unicode.len().max(self.char_code_to_gid.len())
    }

    /// Check if cmap is empty
    pub fn is_empty(&self) -> bool {
        self.gid_to_unicode.is_empty() && self.char_code_to_gid.is_empty()
    }

    // ==================================================================================
    // Private Helper Methods
    // ==================================================================================

    fn parse_sfnt_header(cursor: &mut Cursor<&[u8]>) -> Result<(u16, u16, u16, u16), String> {
        // Read sfnt version (4 bytes - can be 0x00010000 for TrueType or "OTTO" for OpenType)
        let version = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read sfnt version: {}", e))?;

        // 0x00010000 = TrueType, 0x4F54544F = OpenType (OTTO), 0x74727565 = Apple TrueType ("true")
        if version != 0x00010000 && version != 0x4F54544F && version != 0x74727565 {
            // 0x4F54544F = "OTTO" (OpenType)
            return Err(format!("Invalid sfnt version: 0x{:08X}", version));
        }

        let num_tables = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read table count: {}", e))?;
        let search_range = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read search range: {}", e))?;
        let entry_selector = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read entry selector: {}", e))?;
        let range_shift = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read range shift: {}", e))?;

        Ok((num_tables, search_range, entry_selector, range_shift))
    }

    fn find_cmap_table(
        cursor: &mut Cursor<&[u8]>,
        num_tables: u16,
        _search_range: u16,
        _entry_selector: u16,
        _range_shift: u16,
    ) -> Result<u32, String> {
        // Linear search through table directory for 'cmap' tag (0x636D6170)
        const CMAP_TAG: u32 = 0x636D6170;

        for _ in 0..num_tables {
            let tag = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table tag: {}", e))?;
            let _checksum = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table checksum: {}", e))?;
            let offset = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table offset: {}", e))?;
            let _length = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table length: {}", e))?;

            if tag == CMAP_TAG {
                return Ok(offset);
            }
        }

        Err("cmap table not found in font".to_string())
    }

    fn unicode_subtable_priority(platform_id: u16, encoding_id: u16) -> i32 {
        match (platform_id, encoding_id) {
            (3, 10) => 50,
            (3, 1) => 40,
            (0, _) => 30,
            _ => -1,
        }
    }

    fn charcode_subtable_priority(platform_id: u16, encoding_id: u16) -> i32 {
        match (platform_id, encoding_id) {
            (3, 10) => 60,
            (3, 1) => 50,
            (0, _) => 40,
            (3, 0) => 30,
            (1, 0) => 20,
            _ => -1,
        }
    }

    fn parse_cmap_subtable(cursor: &mut Cursor<&[u8]>) -> Result<ParsedCMapSubtable, String> {
        let format = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read cmap format: {}", e))?;

        match format {
            0 => Self::parse_cmap_format0(cursor),
            4 => Self::parse_unicode_cmap(cursor, Self::parse_cmap_format4),
            6 => Self::parse_unicode_cmap(cursor, Self::parse_cmap_format6),
            12 => Self::parse_unicode_cmap(cursor, Self::parse_cmap_format12),
            _ => Err(format!("Unsupported cmap format: {}", format)),
        }
    }

    fn parse_unicode_cmap(
        cursor: &mut Cursor<&[u8]>,
        parser: fn(&mut Cursor<&[u8]>) -> Result<HashMap<u32, u16>, String>,
    ) -> Result<ParsedCMapSubtable, String> {
        let mut parsed = ParsedCMapSubtable::default();
        for (char_code, gid) in parser(cursor)? {
            if let Ok(char_code_u16) = u16::try_from(char_code) {
                parsed.char_code_to_gid.insert(char_code_u16, gid);
            }
            if let Some(ch) = char::from_u32(char_code) {
                parsed.gid_to_unicode.insert(gid, ch);
            }
        }
        Ok(parsed)
    }

    /// Parse cmap format 0 (legacy 1-byte indexed, Mac Roman era).
    fn parse_cmap_format0(cursor: &mut Cursor<&[u8]>) -> Result<ParsedCMapSubtable, String> {
        let length = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 0 length: {}", e))?;
        let _language = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 0 language: {}", e))?;

        if length < 262 {
            return Err(format!("Invalid format 0 length: {}", length));
        }

        let mut glyph_ids = [0u8; 256];
        std::io::Read::read_exact(cursor, &mut glyph_ids)
            .map_err(|e| format!("Failed to read format 0 glyphIdArray: {}", e))?;

        let mut parsed = ParsedCMapSubtable::default();
        for (byte, &gid) in glyph_ids.iter().enumerate() {
            if gid != 0 {
                let gid = gid as u16;
                parsed.char_code_to_gid.insert(byte as u16, gid);
                let ch = if byte < 0x80 {
                    char::from_u32(byte as u32)
                } else {
                    Some(mac_roman_to_unicode(byte as u8))
                };
                if let Some(ch) = ch {
                    parsed.gid_to_unicode.insert(gid, ch);
                }
            }
        }

        Ok(parsed)
    }

    /// Parse cmap format 4 (BMP - supports characters U+0000 to U+FFFF)
    fn parse_cmap_format4(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u32, u16>, String> {
        let _length = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 4 length: {}", e))?
            as u32;
        let _language = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 4 language: {}", e))?;

        let seg_count_x2 = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read segCountX2: {}", e))?
            as usize;
        let seg_count = seg_count_x2 / 2;

        // Skip binary search parameters
        let _search_range = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read searchRange: {}", e))?;
        let _entry_selector = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read entrySelector: {}", e))?;
        let _range_shift = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read rangeShift: {}", e))?;

        // Read segment arrays
        let mut end_codes = vec![0u16; seg_count];
        for i in 0..seg_count {
            end_codes[i] = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read endCode[{}]: {}", i, e))?;
        }

        // Reserved pad
        let _reserved = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read reserved pad: {}", e))?;

        let mut start_codes = vec![0u16; seg_count];
        for i in 0..seg_count {
            start_codes[i] = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read startCode[{}]: {}", i, e))?;
        }

        let mut id_deltas = vec![0i16; seg_count];
        for i in 0..seg_count {
            id_deltas[i] = cursor
                .read_i16::<BigEndian>()
                .map_err(|e| format!("Failed to read idDelta[{}]: {}", i, e))?;
        }

        // id_range_offsets require special parsing - just read as array
        let mut id_range_offsets = vec![0u16; seg_count];
        for i in 0..seg_count {
            id_range_offsets[i] = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read idRangeOffset[{}]: {}", i, e))?;
        }

        // Read remaining bytes as glyphIdArray (used when idRangeOffset != 0)
        let mut glyph_id_array = Vec::new();
        while let Ok(val) = cursor.read_u16::<BigEndian>() {
            glyph_id_array.push(val);
        }

        let mut code_to_gid = HashMap::new();

        for seg in 0..seg_count {
            let start = start_codes[seg] as u32;
            let end = end_codes[seg] as u32;
            let id_delta = id_deltas[seg] as i32;

            for char_code in start..=end {
                if char_code == 0xFFFF {
                    break; // End segment marker
                }

                let gid = if id_range_offsets[seg] == 0 {
                    // Simple formula: GID = charCode + idDelta
                    (char_code as i32 + id_delta) as u16
                } else {
                    // Per TrueType spec: index into glyphIdArray
                    // offset = idRangeOffset[i]/2 + (charCode - startCode[i]) + i - segCount
                    let offset = (id_range_offsets[seg] as usize) / 2
                        + (char_code as usize - start as usize)
                        + seg
                        - seg_count;
                    if offset < glyph_id_array.len() {
                        let raw = glyph_id_array[offset];
                        if raw != 0 {
                            (raw as i32 + id_delta) as u16
                        } else {
                            0
                        }
                    } else {
                        0
                    }
                };

                if gid != 0 {
                    code_to_gid.insert(char_code, gid);
                }
            }
        }

        Ok(code_to_gid)
    }

    /// Parse cmap format 6 (trimmed table)
    fn parse_cmap_format6(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u32, u16>, String> {
        let _length = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 6 length: {}", e))?;
        let _language = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 6 language: {}", e))?;

        let first_code = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read firstCode: {}", e))?;
        let count = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read entryCount: {}", e))? as usize;

        let mut code_to_gid = HashMap::new();

        for i in 0..count {
            let gid = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read glyphId[{}]: {}", i, e))?;

            let char_code = first_code as u32 + i as u32;
            code_to_gid.insert(char_code, gid);
        }

        Ok(code_to_gid)
    }

    /// Parse cmap format 12 (segmented coverage - supports full Unicode)
    fn parse_cmap_format12(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u32, u16>, String> {
        // Skip reserved bytes
        let _reserved = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read reserved: {}", e))?;

        let _length = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read format 12 length: {}", e))?;
        let _language = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read format 12 language: {}", e))?;

        let num_groups = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read numGroups: {}", e))?
            as usize;

        let mut code_to_gid = HashMap::new();

        for _ in 0..num_groups {
            let start_char_code = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read startCharCode: {}", e))?;
            let end_char_code = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read endCharCode: {}", e))?;
            let start_gid = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read startGlyphId: {}", e))?;

            for (offset, char_code) in (start_char_code..=end_char_code).enumerate() {
                let gid = (start_gid + offset as u32) as u16;
                code_to_gid.insert(char_code, gid);
            }
        }

        Ok(code_to_gid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::{BigEndian, WriteBytesExt};

    /// Build a minimal TrueType font with a cmap format 4 table.
    fn build_truetype_with_cmap_format4(mappings: &[(u16, u16)]) -> Vec<u8> {
        // We need: sfnt header + table directory (1 table: cmap) + cmap table
        let mut data = Vec::new();

        // ---- sfnt header ----
        data.write_u32::<BigEndian>(0x00010000).unwrap(); // TrueType version
        data.write_u16::<BigEndian>(1).unwrap(); // numTables = 1
        data.write_u16::<BigEndian>(16).unwrap(); // searchRange
        data.write_u16::<BigEndian>(0).unwrap(); // entrySelector
        data.write_u16::<BigEndian>(0).unwrap(); // rangeShift

        // ---- table directory (1 entry) ----
        let cmap_offset: u32 = 12 + 16; // sfnt header (12) + 1 table record (16)
        data.write_u32::<BigEndian>(0x636D6170).unwrap(); // 'cmap' tag
        data.write_u32::<BigEndian>(0).unwrap(); // checksum (unused)
        data.write_u32::<BigEndian>(cmap_offset).unwrap(); // offset
        data.write_u32::<BigEndian>(0).unwrap(); // length (unused)

        // ---- cmap table header ----
        data.write_u16::<BigEndian>(0).unwrap(); // version
        data.write_u16::<BigEndian>(1).unwrap(); // numSubtables = 1

        // subtable record: platform=3 (Windows), encoding=1 (Unicode BMP)
        let subtable_offset: u32 = 4 + 8; // cmap header (4) + 1 record (8)
        data.write_u16::<BigEndian>(3).unwrap(); // platformID
        data.write_u16::<BigEndian>(1).unwrap(); // encodingID
        data.write_u32::<BigEndian>(subtable_offset).unwrap();

        // ---- cmap format 4 subtable ----
        // Build segments from mappings. Each mapping is (charCode, gid).
        // For simplicity, create one segment per mapping + the sentinel 0xFFFF segment.
        let mut segments: Vec<(u16, u16, i16)> = Vec::new(); // (start, end, delta)
        for &(char_code, gid) in mappings {
            let delta = gid as i16 - char_code as i16;
            segments.push((char_code, char_code, delta));
        }
        segments.push((0xFFFF, 0xFFFF, 1)); // sentinel

        let seg_count = segments.len();
        let seg_count_x2 = (seg_count * 2) as u16;

        data.write_u16::<BigEndian>(4).unwrap(); // format
                                                 // length placeholder (we'll fill in later)
        let length_pos = data.len();
        data.write_u16::<BigEndian>(0).unwrap(); // length
        data.write_u16::<BigEndian>(0).unwrap(); // language

        data.write_u16::<BigEndian>(seg_count_x2).unwrap();
        data.write_u16::<BigEndian>(0).unwrap(); // searchRange
        data.write_u16::<BigEndian>(0).unwrap(); // entrySelector
        data.write_u16::<BigEndian>(0).unwrap(); // rangeShift

        // endCode array
        for seg in &segments {
            data.write_u16::<BigEndian>(seg.1).unwrap();
        }
        // reserved pad
        data.write_u16::<BigEndian>(0).unwrap();
        // startCode array
        for seg in &segments {
            data.write_u16::<BigEndian>(seg.0).unwrap();
        }
        // idDelta array
        for seg in &segments {
            data.write_i16::<BigEndian>(seg.2).unwrap();
        }
        // idRangeOffset array (all zeros = use delta formula)
        for _ in &segments {
            data.write_u16::<BigEndian>(0).unwrap();
        }

        // Fill in format 4 length
        let fmt4_start = length_pos - 2; // format field
        let fmt4_len = data.len() - fmt4_start;
        let len_bytes = (fmt4_len as u16).to_be_bytes();
        data[length_pos] = len_bytes[0];
        data[length_pos + 1] = len_bytes[1];

        data
    }

    /// Build a minimal TrueType font with a cmap format 0 table.
    /// `glyph_ids` is the 256-entry byte → glyph id array.
    fn build_truetype_with_cmap_format0(glyph_ids: [u8; 256]) -> Vec<u8> {
        let mut data = Vec::new();

        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap(); // numTables
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();

        // table directory
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap(); // 'cmap'
        data.write_u32::<BigEndian>(0).unwrap(); // checksum
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap(); // length

        // cmap table header
        data.write_u16::<BigEndian>(0).unwrap(); // version
        data.write_u16::<BigEndian>(1).unwrap(); // numSubtables = 1

        // Use platform (1, 0) — Macintosh / Roman — so the parser routes
        // directly through the format-0 path.
        let subtable_offset: u32 = 4 + 8;
        data.write_u16::<BigEndian>(1).unwrap(); // platformID = Macintosh
        data.write_u16::<BigEndian>(0).unwrap(); // encodingID = Roman
        data.write_u32::<BigEndian>(subtable_offset).unwrap();

        // ---- cmap format 0 subtable ----
        data.write_u16::<BigEndian>(0).unwrap(); // format
        data.write_u16::<BigEndian>(262).unwrap(); // length (fixed for format 0)
        data.write_u16::<BigEndian>(0).unwrap(); // language
        data.extend_from_slice(&glyph_ids);

        data
    }

    /// Build a minimal TrueType font with a cmap format 6 table.
    fn build_truetype_with_cmap_format6(first_code: u16, gids: &[u16]) -> Vec<u8> {
        let mut data = Vec::new();

        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();

        // table directory
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();

        // cmap header
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(3).unwrap(); // platform 3
        data.write_u16::<BigEndian>(1).unwrap(); // encoding 1
        data.write_u32::<BigEndian>(4 + 8).unwrap();

        // format 6
        data.write_u16::<BigEndian>(6).unwrap(); // format
        data.write_u16::<BigEndian>((10 + gids.len() * 2) as u16)
            .unwrap(); // length
        data.write_u16::<BigEndian>(0).unwrap(); // language
        data.write_u16::<BigEndian>(first_code).unwrap();
        data.write_u16::<BigEndian>(gids.len() as u16).unwrap();
        for &gid in gids {
            data.write_u16::<BigEndian>(gid).unwrap();
        }

        data
    }

    /// Build a minimal TrueType font with a cmap format 12 table.
    fn build_truetype_with_cmap_format12(groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut data = Vec::new();

        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();

        // table directory
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();

        // cmap header
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(3).unwrap(); // platform 3
        data.write_u16::<BigEndian>(10).unwrap(); // encoding 10 (full repertoire)
        data.write_u32::<BigEndian>(4 + 8).unwrap();

        // format 12
        data.write_u16::<BigEndian>(12).unwrap(); // format
        data.write_u16::<BigEndian>(0).unwrap(); // reserved
        data.write_u32::<BigEndian>((16 + groups.len() * 12) as u32)
            .unwrap(); // length
        data.write_u32::<BigEndian>(0).unwrap(); // language
        data.write_u32::<BigEndian>(groups.len() as u32).unwrap();
        for &(start, end, start_gid) in groups {
            data.write_u32::<BigEndian>(start).unwrap();
            data.write_u32::<BigEndian>(end).unwrap();
            data.write_u32::<BigEndian>(start_gid).unwrap();
        }

        data
    }

    #[test]
    fn test_sfnt_header_parsing() {
        // Valid TrueType with empty cmap format 4
        let data = build_truetype_with_cmap_format4(&[]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert!(cmap.is_empty());
    }

    #[test]
    fn test_invalid_sfnt_version() {
        let mut data = vec![0u8; 100];
        // Invalid version bytes
        data[0] = 0xFF;
        data[1] = 0xFF;
        data[2] = 0xFF;
        data[3] = 0xFF;
        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid sfnt version"));
    }

    #[test]
    fn test_opentype_version_accepted() {
        // Build data with OTTO version
        let mut data = build_truetype_with_cmap_format4(&[(65, 1)]); // 'A' -> gid 1
                                                                     // Replace version with OTTO (0x4F54544F)
        data[0] = 0x4F;
        data[1] = 0x54;
        data[2] = 0x54;
        data[3] = 0x4F;
        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_ok());
    }

    #[test]
    fn test_apple_truetype_version_accepted() {
        let mut data = build_truetype_with_cmap_format4(&[(65, 1)]);
        // Replace version with "true" (0x74727565)
        data[0] = 0x74;
        data[1] = 0x72;
        data[2] = 0x75;
        data[3] = 0x65;
        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_ok());
    }

    #[test]
    fn test_no_cmap_table() {
        let mut data = Vec::new();
        // sfnt header with 1 table but NOT cmap
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        // table record for 'head' (not 'cmap')
        data.write_u32::<BigEndian>(0x68656164).unwrap(); // 'head'
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(28).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();

        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cmap table not found"));
    }

    #[test]
    fn test_format4_basic_ascii() {
        // Map A(65)->1, B(66)->2, C(67)->3
        let data = build_truetype_with_cmap_format4(&[(65, 1), (66, 2), (67, 3)]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.len(), 3);
        assert_eq!(cmap.get_unicode(1), Some('A'));
        assert_eq!(cmap.get_unicode(2), Some('B'));
        assert_eq!(cmap.get_unicode(3), Some('C'));
        assert_eq!(cmap.get_unicode(4), None);
    }

    #[test]
    fn test_format4_extended_unicode() {
        // Map some non-ASCII: é(233)->10, ñ(241)->11
        let data = build_truetype_with_cmap_format4(&[(233, 10), (241, 11)]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.get_unicode(10), Some('é'));
        assert_eq!(cmap.get_unicode(11), Some('ñ'));
    }

    #[test]
    fn test_format6_basic() {
        // Format 6: first_code=65, gids=[1, 2, 3] -> maps A->gid1, B->gid2, C->gid3
        let data = build_truetype_with_cmap_format6(65, &[1, 2, 3]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.len(), 3);
        assert_eq!(cmap.get_unicode(1), Some('A'));
        assert_eq!(cmap.get_unicode(2), Some('B'));
        assert_eq!(cmap.get_unicode(3), Some('C'));
    }

    #[test]
    fn test_format6_non_zero_first_code() {
        // Start at code 48 ('0') for digits
        let data = build_truetype_with_cmap_format6(48, &[10, 11, 12]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.get_unicode(10), Some('0'));
        assert_eq!(cmap.get_unicode(11), Some('1'));
        assert_eq!(cmap.get_unicode(12), Some('2'));
    }

    #[test]
    fn test_format0_exposes_char_code_to_gid_mapping() {
        let mut gids = [0u8; 256];
        gids[0x63] = 99;
        gids[0x64] = 100;
        let data = build_truetype_with_cmap_format0(gids);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.get_gid_for_char_code(0x63), Some(99));
        assert_eq!(cmap.get_gid_for_char_code(0x64), Some(100));
        assert_eq!(cmap.get_unicode(99), Some('c'));
        assert_eq!(cmap.get_unicode(100), Some('d'));
        assert!(!cmap.is_empty());
    }

    #[test]
    fn test_format12_basic() {
        // One group: chars 65-67 -> gids 1-3
        let data = build_truetype_with_cmap_format12(&[(65, 67, 1)]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.len(), 3);
        assert_eq!(cmap.get_unicode(1), Some('A'));
        assert_eq!(cmap.get_unicode(2), Some('B'));
        assert_eq!(cmap.get_unicode(3), Some('C'));
    }

    #[test]
    fn test_format12_multiple_groups() {
        let data = build_truetype_with_cmap_format12(&[
            (65, 67, 1),  // A-C -> gids 1-3
            (48, 50, 10), // 0-2 -> gids 10-12
        ]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.len(), 6);
        assert_eq!(cmap.get_unicode(1), Some('A'));
        assert_eq!(cmap.get_unicode(10), Some('0'));
        assert_eq!(cmap.get_unicode(12), Some('2'));
    }

    #[test]
    fn test_get_unicode_missing() {
        let data = build_truetype_with_cmap_format4(&[(65, 1)]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.get_unicode(999), None);
    }

    #[test]
    fn test_len_and_is_empty() {
        let data_empty = build_truetype_with_cmap_format4(&[]);
        let cmap_empty = TrueTypeCMap::from_font_data(&data_empty).unwrap();
        assert_eq!(cmap_empty.len(), 0);
        assert!(cmap_empty.is_empty());

        let data_one = build_truetype_with_cmap_format4(&[(65, 1)]);
        let cmap_one = TrueTypeCMap::from_font_data(&data_one).unwrap();
        assert_eq!(cmap_one.len(), 1);
        assert!(!cmap_one.is_empty());
    }

    #[test]
    fn test_cmap_format0_byte_indexed() {
        // Build a format-0 cmap where byte code 0x41 ('A') maps to gid 10,
        // 0x42 ('B') to gid 11, and everything else is 0 (.notdef).
        let mut gids = [0u8; 256];
        gids[0x41] = 10;
        gids[0x42] = 11;
        gids[0x7A] = 50;
        let data = build_truetype_with_cmap_format0(gids);
        let cmap = TrueTypeCMap::from_font_data(&data).expect("format 0 parse");
        assert_eq!(cmap.get_gid_for_char_code(0x41), Some(10));
        assert_eq!(cmap.get_gid_for_char_code(0x42), Some(11));
        assert_eq!(cmap.get_gid_for_char_code(0x7A), Some(50));
        assert_eq!(cmap.get_unicode(10), Some('A'));
        assert_eq!(cmap.get_unicode(11), Some('B'));
        assert_eq!(cmap.get_unicode(50), Some('z'));
    }

    #[test]
    fn test_cmap_format0_mac_roman_high_half() {
        // High-half Macintosh Roman bytes should resolve to both Unicode and glyph ids.
        let mut gids = [0u8; 256];
        gids[0x41] = 10; // 'A' — ASCII pass-through
        gids[0x8A] = 20; // 'ä' via Mac Roman table
        gids[0xA5] = 30; // '•' bullet via Mac Roman
        let data = build_truetype_with_cmap_format0(gids);
        let cmap = TrueTypeCMap::from_font_data(&data).expect("format 0 parse");
        assert_eq!(cmap.get_gid_for_char_code(0x41), Some(10));
        assert_eq!(cmap.get_gid_for_char_code(0x8A), Some(20));
        assert_eq!(cmap.get_gid_for_char_code(0xA5), Some(30));
        assert_eq!(cmap.get_unicode(10), Some('A'));
        assert_eq!(cmap.get_unicode(20), Some('ä'));
        assert_eq!(cmap.get_unicode(30), Some('•'));
    }

    #[test]
    fn test_cmap_format0_rejects_truncated() {
        // 256-byte array but declared length wrong → truncated.
        let mut data = Vec::new();
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(4 + 8).unwrap();
        data.write_u16::<BigEndian>(0).unwrap(); // format
                                                 // Declare the correct length (262) but only append 8 bytes of
                                                 // glyphIdArray instead of 256 — parser should detect the
                                                 // truncation via read_exact.
        data.write_u16::<BigEndian>(262).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.extend_from_slice(&[0u8; 8]);
        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_unsupported_cmap_format() {
        let mut data = Vec::new();
        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        // cmap table directory entry
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        // cmap header
        data.write_u16::<BigEndian>(0).unwrap(); // version
        data.write_u16::<BigEndian>(1).unwrap(); // 1 subtable
        data.write_u16::<BigEndian>(3).unwrap(); // platform 3
        data.write_u16::<BigEndian>(1).unwrap(); // encoding 1
        data.write_u32::<BigEndian>(4 + 8).unwrap(); // subtable offset
                                                     // format 2 (unsupported)
        data.write_u16::<BigEndian>(2).unwrap();

        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unsupported cmap format"));
    }

    #[test]
    fn test_unsupported_cmap_version() {
        let mut data = Vec::new();
        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        // cmap table directory entry
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        // cmap header with invalid version
        data.write_u16::<BigEndian>(99).unwrap(); // version != 0

        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Unsupported cmap table version"));
    }

    #[test]
    fn test_no_suitable_subtable() {
        let mut data = Vec::new();
        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        // cmap table directory entry
        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        // cmap header with 0 subtables
        data.write_u16::<BigEndian>(0).unwrap(); // version
        data.write_u16::<BigEndian>(0).unwrap(); // 0 subtables

        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No suitable cmap subtable"));
    }

    #[test]
    fn test_truncated_data() {
        // Just a few bytes - not even a valid header
        let data = vec![0u8; 4];
        let result = TrueTypeCMap::from_font_data(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_clone_and_debug() {
        let data = build_truetype_with_cmap_format4(&[(65, 1)]);
        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        let cloned = cmap.clone();
        assert_eq!(cloned.get_unicode(1), Some('A'));
        let debug = format!("{:?}", cmap);
        assert!(debug.contains("TrueTypeCMap"));
    }

    #[test]
    fn test_platform_priority_windows_full_over_bmp() {
        // Build a font with 2 subtables: platform 3/encoding 1 and 3/10
        // The 3/10 (full) should be preferred
        let mut data = Vec::new();

        // sfnt header
        data.write_u32::<BigEndian>(0x00010000).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u16::<BigEndian>(16).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();
        data.write_u16::<BigEndian>(0).unwrap();

        let cmap_offset: u32 = 12 + 16;
        data.write_u32::<BigEndian>(0x636D6170).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();
        data.write_u32::<BigEndian>(cmap_offset).unwrap();
        data.write_u32::<BigEndian>(0).unwrap();

        // cmap header with 2 subtables
        data.write_u16::<BigEndian>(0).unwrap(); // version
        data.write_u16::<BigEndian>(2).unwrap(); // 2 subtables

        // Both point to same subtable (format 12) for simplicity
        let subtable_off: u32 = 4 + 8 * 2; // cmap header + 2 records
                                           // Record 1: platform 3, encoding 1
        data.write_u16::<BigEndian>(3).unwrap();
        data.write_u16::<BigEndian>(1).unwrap();
        data.write_u32::<BigEndian>(subtable_off).unwrap();
        // Record 2: platform 3, encoding 10 (higher priority)
        data.write_u16::<BigEndian>(3).unwrap();
        data.write_u16::<BigEndian>(10).unwrap();
        data.write_u32::<BigEndian>(subtable_off).unwrap();

        // format 12 subtable: one group: A(65)->gid1
        data.write_u16::<BigEndian>(12).unwrap();
        data.write_u16::<BigEndian>(0).unwrap(); // reserved
        data.write_u32::<BigEndian>(28).unwrap(); // length
        data.write_u32::<BigEndian>(0).unwrap(); // language
        data.write_u32::<BigEndian>(1).unwrap(); // 1 group
        data.write_u32::<BigEndian>(65).unwrap();
        data.write_u32::<BigEndian>(65).unwrap();
        data.write_u32::<BigEndian>(1).unwrap();

        let cmap = TrueTypeCMap::from_font_data(&data).unwrap();
        assert_eq!(cmap.get_unicode(1), Some('A'));
    }
}
