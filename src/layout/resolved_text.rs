//! Resolved text structures for precise PDF-native extraction.
//!
//! These types intentionally model a different contract from
//! [`crate::layout::TextSpan`] and [`crate::layout::TextChar`]:
//! - order follows the PDF content stream's native text-run order
//! - span boxes are rebuilt from resolved character boxes
//! - no reading-order reflow is implied by this data model

use crate::geometry::Rect;
use crate::layout::text_block::{Color, FontWeight, TextChar};

/// A resolved character with precise text and bounding box data.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "wasm", serde(rename_all = "camelCase"))]
pub struct ResolvedChar {
    /// The resolved Unicode scalar value for this character.
    pub text: char,
    /// Character bounding box in PDF user space.
    pub bbox: Rect,
    /// Baseline origin X coordinate in PDF user space.
    pub origin_x: f32,
    /// Baseline origin Y coordinate in PDF user space.
    pub origin_y: f32,
    /// Horizontal advance width in PDF user space.
    pub advance_width: f32,
    /// Optional rendered rotation in degrees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<f32>,
}

impl ResolvedChar {
    pub(crate) fn from_text_char(text_char: &TextChar) -> Self {
        Self {
            text: text_char.char,
            bbox: text_char.bbox,
            origin_x: text_char.origin_x,
            origin_y: text_char.origin_y,
            advance_width: text_char.advance_width,
            rotation_degrees: Some(text_char.rotation_degrees),
        }
    }
}

/// Shared style attributes for a resolved span.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "wasm", serde(rename_all = "camelCase"))]
pub struct ResolvedStyle {
    /// Font name or family.
    pub font_name: String,
    /// Font size in points.
    pub font_size: f32,
    /// Font weight.
    pub font_weight: FontWeight,
    /// Whether the font is italic.
    pub is_italic: bool,
    /// Whether the font is monospace.
    pub is_monospace: bool,
    /// Text color.
    pub color: Color,
    /// Character spacing (`Tc`).
    pub char_spacing: f32,
    /// Word spacing (`Tw`).
    pub word_spacing: f32,
    /// Horizontal scaling (`Tz`).
    pub horizontal_scaling: f32,
}

/// A resolved PDF-native text span preserving PDF-native run order.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "wasm", serde(rename_all = "camelCase"))]
pub struct ResolvedSpan {
    /// Concatenated text from `chars`.
    pub text: String,
    /// Union bounding box of `chars`.
    pub bbox: Rect,
    /// PDF-native emission sequence for this span.
    ///
    /// This is assigned in the order runs are accepted by the resolved
    /// extraction pipeline, before any reading-order sorting.
    pub sequence: usize,
    /// Marked content ID when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcid: Option<u32>,
    /// Shared style attributes for this span.
    pub style: ResolvedStyle,
    /// Characters in PDF-native order.
    pub chars: Vec<ResolvedChar>,
}

impl ResolvedSpan {
    /// Build a resolved span from already-resolved characters.
    ///
    /// The resulting span text is the concatenation of `chars`, and the span
    /// bounding box is the union of all char boxes. Empty spans keep the
    /// default rectangle.
    pub fn from_parts(
        sequence: usize,
        mcid: Option<u32>,
        style: ResolvedStyle,
        chars: Vec<ResolvedChar>,
    ) -> Self {
        let text: String = chars.iter().map(|ch| ch.text).collect();
        let bbox = chars
            .iter()
            .map(|ch| ch.bbox)
            .reduce(|acc, rect| acc.union(&rect))
            .unwrap_or_default();

        Self {
            text,
            bbox,
            sequence,
            mcid,
            style,
            chars,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolvedChar, ResolvedSpan, ResolvedStyle};
    use crate::geometry::Rect;
    use crate::layout::{Color, FontWeight, TextChar};

    fn style() -> ResolvedStyle {
        ResolvedStyle {
            font_name: "Helvetica".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            is_italic: false,
            is_monospace: false,
            color: Color::black(),
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
        }
    }

    #[test]
    fn resolved_span_bbox_is_union_of_chars() {
        let span = ResolvedSpan::from_parts(
            7,
            Some(3),
            style(),
            vec![
                ResolvedChar {
                    text: 'A',
                    bbox: Rect::new(10.0, 20.0, 5.0, 8.0),
                    origin_x: 10.0,
                    origin_y: 20.0,
                    advance_width: 5.0,
                    rotation_degrees: Some(0.0),
                },
                ResolvedChar {
                    text: 'B',
                    bbox: Rect::new(16.0, 18.0, 4.0, 10.0),
                    origin_x: 16.0,
                    origin_y: 18.0,
                    advance_width: 4.0,
                    rotation_degrees: Some(0.0),
                },
            ],
        );

        assert_eq!(span.text, "AB");
        assert_eq!(span.sequence, 7);
        assert_eq!(span.mcid, Some(3));
        assert_eq!(span.bbox, Rect::new(10.0, 18.0, 10.0, 10.0));
    }

    #[test]
    fn resolved_char_preserves_text_origin_box_separately_from_glyph_bbox() {
        let text_char = TextChar {
            char: 'g',
            bbox: Rect::new(11.0, 16.0, 4.0, 9.0),
            font_name: "Helvetica".to_string(),
            font_size: 12.0,
            font_weight: FontWeight::Normal,
            is_italic: false,
            is_monospace: false,
            color: Color::black(),
            mcid: None,
            origin_x: 10.0,
            origin_y: 20.0,
            rotation_degrees: 0.0,
            advance_width: 6.0,
            matrix: None,
        };

        let resolved = ResolvedChar::from_text_char(&text_char);

        assert_eq!(resolved.bbox, Rect::new(11.0, 16.0, 4.0, 9.0));
        assert_eq!(resolved.origin_x, 10.0);
        assert_eq!(resolved.origin_y, 20.0);
        assert_eq!(resolved.advance_width, 6.0);
    }

    #[test]
    fn resolved_span_empty_chars_uses_default_bbox() {
        let span = ResolvedSpan::from_parts(0, None, style(), Vec::new());
        assert!(span.text.is_empty());
        assert_eq!(span.bbox, Rect::default());
    }
}
