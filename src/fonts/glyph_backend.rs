use super::font_dict::{Encoding, FontInfo};
use super::system_font::load_system_font_face;
use owned_ttf_parser::OwnedFace;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};

static PARSED_FACE_CACHE: LazyLock<Mutex<HashMap<u64, Option<Arc<OwnedFace>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PARSED_CFF_CID_GID_CACHE: LazyLock<Mutex<HashMap<u64, Option<Arc<HashMap<u16, u16>>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PARSED_CFF_GLYPH_NAME_GID_CACHE: LazyLock<
    Mutex<HashMap<u64, Option<Arc<HashMap<String, u16>>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));
static TYPE1_GLYPH_NAME_CACHE: LazyLock<Mutex<HashMap<u64, Arc<HashMap<u8, String>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParsedFaceSource {
    Embedded,
    SystemFallback,
}

fn hash_char_encoding_map<H: Hasher>(map: &HashMap<u8, char>, hasher: &mut H) {
    let mut entries = map.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(code, _)| **code);
    for (code, ch) in entries {
        code.hash(hasher);
        ch.hash(hasher);
    }
}

fn hash_string_encoding_map<H: Hasher>(map: &HashMap<u8, String>, hasher: &mut H) {
    let mut entries = map.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(code, _)| **code);
    for (code, value) in entries {
        code.hash(hasher);
        value.hash(hasher);
    }
}

fn type1_glyph_name_cache_key_from_font(font: &FontInfo) -> Option<u64> {
    if !matches!(font.subtype.as_str(), "Type1" | "MMType1") {
        return None;
    }

    let font_data = font.embedded_font_data.as_ref()?;
    let mut hasher = DefaultHasher::new();
    font.subtype.hash(&mut hasher);
    font_data.hash(&mut hasher);
    match &font.encoding {
        Encoding::Standard(name) => {
            0u8.hash(&mut hasher);
            name.hash(&mut hasher);
        },
        Encoding::Custom(map) => {
            1u8.hash(&mut hasher);
            hash_char_encoding_map(map, &mut hasher);
        },
        Encoding::Identity => {
            2u8.hash(&mut hasher);
        },
    }
    hash_string_encoding_map(&font.multi_char_map, &mut hasher);
    Some(hasher.finish())
}

pub(crate) fn cache_type1_glyph_names(font: &FontInfo, glyph_names: HashMap<u8, String>) {
    let Some(cache_key) = type1_glyph_name_cache_key_from_font(font) else {
        return;
    };

    if let Ok(mut cache) = TYPE1_GLYPH_NAME_CACHE.lock() {
        cache.insert(cache_key, Arc::new(glyph_names));
    }
}

pub(crate) fn type1_glyph_name_for_pdf_char_code(
    font: &FontInfo,
    char_code: u32,
) -> Option<String> {
    let byte = u8::try_from(char_code).ok()?;
    let cache_key = type1_glyph_name_cache_key_from_font(font)?;
    let cache = TYPE1_GLYPH_NAME_CACHE.lock().ok()?;
    cache.get(&cache_key)?.get(&byte).cloned()
}

fn parsed_face_cache_key(font: &FontInfo) -> Option<u64> {
    let font_data = font.embedded_font_data.as_ref()?;
    let mut hasher = DefaultHasher::new();
    font_data.hash(&mut hasher);
    Some(hasher.finish())
}

fn font_data_looks_like_sfnt(data: &[u8]) -> bool {
    data.len() >= 4
        && matches!(
            u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            0x00010000 | 0x4F54544F | 0x74727565
        )
}

#[cfg(test)]
pub(crate) fn parsed_face_with_source(
    font: &FontInfo,
) -> Option<(Arc<OwnedFace>, ParsedFaceSource)> {
    if let Some(face) = embedded_parsed_face(font) {
        return Some((face, ParsedFaceSource::Embedded));
    }

    system_fallback_face(font).map(|face| (face, ParsedFaceSource::SystemFallback))
}

pub(crate) fn embedded_parsed_face(font: &FontInfo) -> Option<Arc<OwnedFace>> {
    let font_data = font.embedded_font_data.as_ref()?;
    let has_embedded_sfnt =
        !font_data.is_empty() && (font.is_truetype_font || font_data_looks_like_sfnt(font_data));
    if !has_embedded_sfnt {
        return None;
    }

    let cache_key = parsed_face_cache_key(font)?;

    if let Ok(cache) = PARSED_FACE_CACHE.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone();
        }
    }

    let parsed = match OwnedFace::from_vec(font_data.as_ref().to_vec(), 0) {
        Ok(face) => Some(Arc::new(face)),
        Err(e) => {
            log::warn!("Font '{}': embedded face parse failed: {}", font.base_font, e);
            None
        },
    };

    if let Ok(mut cache) = PARSED_FACE_CACHE.lock() {
        cache.insert(cache_key, parsed.clone());
    }

    parsed
}

pub(crate) fn system_fallback_face(font: &FontInfo) -> Option<Arc<OwnedFace>> {
    load_system_font_face(&font.base_font)
}

#[cfg(test)]
pub(crate) fn parsed_face_source(font: &FontInfo) -> Option<ParsedFaceSource> {
    parsed_face_with_source(font).map(|(_, source)| source)
}

#[cfg(test)]
pub(crate) fn parsed_face(font: &FontInfo) -> Option<Arc<OwnedFace>> {
    parsed_face_with_source(font).map(|(face, _)| face)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::font_dict::{CIDToGIDMap, Encoding, FontInfo};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn embedded_face_loader_does_not_use_system_fallback_for_missing_embedded_data() {
        let font = FontInfo {
            base_font: "DefinitelyMissingSystemFallbackProbe".to_string(),
            subtype: "Type1".to_string(),
            encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
            to_unicode: None,
            font_weight: None,
            flags: None,
            stem_v: None,
            embedded_font_data: None,
            truetype_cmap: std::sync::OnceLock::new(),
            is_truetype_font: false,
            cid_to_gid_map: None,
            cid_system_info: None,
            cid_font_type: None,
            widths: None,
            first_char: None,
            last_char: None,
            default_width: 500.0,
            cid_widths: None,
            cid_default_width: 1000.0,
            multi_char_map: HashMap::new(),
            cff_gid_map: None,
            byte_to_char_table: std::sync::OnceLock::new(),
            byte_to_width_table: std::sync::OnceLock::new(),
        };

        let _unused_arc: Option<Arc<OwnedFace>> = None;
        let _unused_cid: Option<CIDToGIDMap> = None;
        assert!(embedded_parsed_face(&font).is_none());
    }
}

pub(crate) fn cff_cid_gid_map(font: &FontInfo) -> Option<Arc<HashMap<u16, u16>>> {
    if font.subtype != "Type0" || font.cid_font_type.as_deref() != Some("CIDFontType0") {
        return None;
    }

    let font_data = font.embedded_font_data.as_ref()?;
    if font_data.is_empty() {
        return None;
    }

    let cache_key = parsed_face_cache_key(font)?;
    if let Ok(cache) = PARSED_CFF_CID_GID_CACHE.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone();
        }
    }

    let parsed = super::cff_encoding::parse_cff_cid_gid_mapping(font_data).map(Arc::new);
    if let Ok(mut cache) = PARSED_CFF_CID_GID_CACHE.lock() {
        cache.insert(cache_key, parsed.clone());
    }
    parsed
}

pub(crate) fn cff_glyph_name_gid_map(font: &FontInfo) -> Option<Arc<HashMap<String, u16>>> {
    if font.subtype == "Type0" {
        return None;
    }

    let font_data = font.embedded_font_data.as_ref()?;
    if font_data.is_empty() {
        return None;
    }

    let cache_key = parsed_face_cache_key(font)?;
    if let Ok(cache) = PARSED_CFF_GLYPH_NAME_GID_CACHE.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone();
        }
    }

    let parsed = super::cff_encoding::parse_cff_glyph_name_gid_mapping(font_data).map(Arc::new);
    if let Ok(mut cache) = PARSED_CFF_GLYPH_NAME_GID_CACHE.lock() {
        cache.insert(cache_key, parsed.clone());
    }
    parsed
}
