use std::sync::Arc;

use freetype::face::LoadFlag;
use freetype::{ffi, Face, Library};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Type1GlyphBox1000 {
    pub(crate) x_min: f32,
    pub(crate) y_min: f32,
    pub(crate) x_max: f32,
    pub(crate) y_max: f32,
}

#[derive(Debug)]
pub(crate) struct Type1FreeTypeFace {
    face: Face,
    units_per_em: u16,
    cff_glyph_name_to_gid: Option<std::collections::HashMap<String, u16>>,
}

impl Type1FreeTypeFace {
    pub(crate) fn from_font_data(font_data: Arc<Vec<u8>>) -> Option<Self> {
        if font_data.is_empty() {
            return None;
        }

        let library = Library::init().ok()?;
        let face = library
            .new_memory_face(font_data.as_ref().clone(), 0)
            .ok()
            .or_else(|| {
                super::cff_encoding::extract_cff_from_opentype(&font_data)
                    .and_then(|cff_data| library.new_memory_face(cff_data.to_vec(), 0).ok())
            })?;
        if !face.is_scalable() {
            return None;
        }
        let units_per_em = match face.em_size() {
            n if n > 0 => n as u16,
            _ => 1000,
        };
        let cff_glyph_name_to_gid =
            super::cff_encoding::parse_cff_glyph_name_gid_mapping(&font_data);
        Some(Self {
            face,
            units_per_em,
            cff_glyph_name_to_gid,
        })
    }

    fn glyph_box_for_index(&self, glyph_index: u32) -> Option<Type1GlyphBox1000> {
        if glyph_index == 0 {
            return None;
        }

        self.face
            .load_glyph(
                glyph_index,
                LoadFlag::NO_SCALE | LoadFlag::NO_HINTING | LoadFlag::NO_BITMAP,
            )
            .ok()?;

        let scale = 1000.0 / self.units_per_em.max(1) as f32;
        let slot = self.face.glyph();

        let bbox = if slot.outline().is_some() {
            let mut bbox = ffi::FT_BBox::default();
            unsafe {
                ffi::FT_Outline_Get_CBox(&slot.raw().outline, &mut bbox);
            }
            bbox
        } else {
            let metrics = slot.metrics();
            ffi::FT_BBox {
                xMin: metrics.horiBearingX,
                yMin: metrics.horiBearingY - metrics.height,
                xMax: metrics.horiBearingX + metrics.width,
                yMax: metrics.horiBearingY,
            }
        };

        Some(Type1GlyphBox1000 {
            x_min: bbox.xMin as f32 * scale,
            y_min: bbox.yMin as f32 * scale,
            x_max: bbox.xMax as f32 * scale,
            y_max: bbox.yMax as f32 * scale,
        })
    }

    pub(crate) fn glyph_box_for_char_code(&self, char_code: u32) -> Option<Type1GlyphBox1000> {
        let glyph_index = self.face.get_char_index(char_code as usize)?;
        self.glyph_box_for_index(glyph_index)
    }

    pub(crate) fn glyph_box_for_name(&self, glyph_name: &str) -> Option<Type1GlyphBox1000> {
        let glyph_index = self
            .face
            .get_name_index(glyph_name)
            .filter(|&glyph_index| glyph_index != 0)
            .or_else(|| {
                self.cff_glyph_name_to_gid
                    .as_ref()
                    .and_then(|map| map.get(glyph_name).copied().map(u32::from))
            })?;
        self.glyph_box_for_index(glyph_index)
    }
}
