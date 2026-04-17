use owned_ttf_parser::OwnedFace;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

static SYSTEM_FONT_DB: LazyLock<fontdb::Database> = LazyLock::new(|| {
    let mut fontdb = fontdb::Database::new();
    fontdb.load_system_fonts();
    fontdb
});

static SYSTEM_FONT_FACE_CACHE: LazyLock<Mutex<HashMap<String, Option<Arc<OwnedFace>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn load_system_font_data(pdf_font_name: &str) -> Option<(Vec<u8>, u32)> {
    let clean_name = if let Some(plus_idx) = pdf_font_name.find('+') {
        &pdf_font_name[plus_idx + 1..]
    } else {
        pdf_font_name
    };

    let is_cjk_probability = clean_name.contains("GB2312")
        || clean_name.contains("Identity")
        || clean_name.contains("楷体")
        || clean_name.contains("æ¥·ä½")
        || clean_name.contains("宋体")
        || clean_name.contains("å®\u{008b}ä½")
        || clean_name.contains("黑体")
        || clean_name.contains("é»\u{0091}ä½")
        || clean_name.contains("FangSong")
        || clean_name.contains("SimSun")
        || clean_name.contains("SimHei")
        || clean_name.contains("KaiTi")
        || pdf_font_name == "F1";

    let final_name = if clean_name.contains("楷体")
        || clean_name.contains("æ¥·ä½")
        || clean_name.contains("KaiTi")
    {
        "KaiTi"
    } else if clean_name.contains("宋体")
        || clean_name.contains("å®\u{008b}ä½")
        || clean_name.contains("SimSun")
    {
        "SimSun"
    } else if clean_name.contains("黑体")
        || clean_name.contains("é»\u{0091}ä½")
        || clean_name.contains("SimHei")
    {
        "SimHei"
    } else {
        clean_name
    };

    let mut variants = vec![final_name.to_string()];

    if clean_name.contains("URWPalladioL") || clean_name.contains("Palatino") {
        variants.insert(0, "P052".to_string());
        variants.push("Palatino Linotype".to_string());
        variants.push("TeX Gyre Pagella".to_string());
    } else if clean_name.contains("NimbusRomNo9L") || clean_name.contains("NimbusRoman") {
        variants.insert(0, "Nimbus Roman".to_string());
        variants.push("Times New Roman".to_string());
    } else if clean_name.contains("NimbusSanL") || clean_name.contains("NimbusSans") {
        variants.insert(0, "Nimbus Sans".to_string());
        variants.push("Arial".to_string());
    } else if clean_name.contains("NimbusMonL") || clean_name.contains("NimbusMono") {
        variants.insert(0, "Nimbus Mono PS".to_string());
        variants.push("Courier New".to_string());
    } else if clean_name.contains("CMSS")
        || clean_name.contains("CMR")
        || clean_name.contains("CMBX")
    {
        variants.push("Latin Modern Roman".to_string());
        variants.push("Computer Modern".to_string());
    } else if clean_name.contains("URWBookmanL") || clean_name.contains("Bookman") {
        variants.insert(0, "Bookman URW".to_string());
    } else if clean_name.contains("CenturySchL") || clean_name.contains("NewCentury") {
        variants.insert(0, "C059".to_string());
    } else if clean_name.contains("URWChanceryL") || clean_name.contains("Chancery") {
        variants.insert(0, "Z003".to_string());
    }

    if is_cjk_probability {
        variants.push("Noto Sans CJK SC".to_string());
        variants.push("Noto Serif CJK SC".to_string());
        variants.push("WenQuanYi Micro Hei".to_string());
        variants.push("Droid Sans Fallback".to_string());
    }

    let is_serif = clean_name.contains("Roman")
        || clean_name.contains("Serif")
        || clean_name.contains("Times")
        || clean_name.contains("Palladio")
        || clean_name.contains("Palatino")
        || clean_name.contains("Bookman")
        || clean_name.contains("Garamond")
        || clean_name.contains("Century")
        || clean_name.contains("Georgia")
        || clean_name.contains("CMR")
        || clean_name.contains("CMBX")
        || clean_name.contains("CMTI");
    if is_serif {
        variants.push("Times New Roman".to_string());
        variants.push("Liberation Serif".to_string());
        variants.push("DejaVu Serif".to_string());
    }
    variants.push("Arial".to_string());
    variants.push("Helvetica".to_string());
    variants.push("Liberation Sans".to_string());
    variants.push("DejaVu Sans".to_string());
    variants.push("Noto Sans".to_string());
    variants.push("FreeSans".to_string());

    let weight = if pdf_font_name.contains("Bold") || pdf_font_name.contains("Black") {
        fontdb::Weight::BOLD
    } else {
        fontdb::Weight::NORMAL
    };

    let style = if pdf_font_name.contains("Italic") || pdf_font_name.contains("Oblique") {
        fontdb::Style::Italic
    } else {
        fontdb::Style::Normal
    };

    for variant in variants {
        let families = [
            fontdb::Family::Name(&variant),
            fontdb::Family::Serif,
            fontdb::Family::SansSerif,
        ];
        let query = fontdb::Query {
            families: &families,
            weight,
            stretch: fontdb::Stretch::Normal,
            style,
        };

        if let Some(id) = SYSTEM_FONT_DB.query(&query) {
            let mut data = None;
            SYSTEM_FONT_DB.with_face_data(id, |face_data, index| {
                data = Some((face_data.to_vec(), index));
            });
            if data.is_some() {
                return data;
            }
        }
    }

    None
}

pub(crate) fn load_system_font_face(pdf_font_name: &str) -> Option<Arc<OwnedFace>> {
    if let Ok(cache) = SYSTEM_FONT_FACE_CACHE.lock() {
        if let Some(cached) = cache.get(pdf_font_name) {
            return cached.clone();
        }
    }

    let parsed = load_system_font_data(pdf_font_name)
        .and_then(|(font_data, index)| OwnedFace::from_vec(font_data, index).ok())
        .map(Arc::new);

    if let Ok(mut cache) = SYSTEM_FONT_FACE_CACHE.lock() {
        cache.insert(pdf_font_name.to_string(), parsed.clone());
    }

    parsed
}
