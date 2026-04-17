use crate::layout::{ResolvedChar, ResolvedSpan, ResolvedStyle};

#[derive(Debug, Default)]
pub(crate) struct ResolvedCollector {
    spans: Vec<ResolvedSpan>,
    pending_sequence: Option<usize>,
    pending_mcid: Option<u32>,
    pending_style: Option<ResolvedStyle>,
    pending_chars: Vec<ResolvedChar>,
}

impl ResolvedCollector {
    pub(crate) fn begin_run(&mut self, sequence: usize, mcid: Option<u32>, style: ResolvedStyle) {
        self.finish_run();
        self.pending_sequence = Some(sequence);
        self.pending_mcid = mcid;
        self.pending_style = Some(style);
    }

    pub(crate) fn push_char(&mut self, ch: ResolvedChar) {
        self.pending_chars.push(ch);
    }

    pub(crate) fn finish_run(&mut self) -> bool {
        let Some(sequence) = self.pending_sequence.take() else {
            return false;
        };

        let style = self
            .pending_style
            .take()
            .expect("begin_run sets pending_style");
        let chars = std::mem::take(&mut self.pending_chars);
        let mcid = self.pending_mcid.take();
        if chars.is_empty() {
            return false;
        }
        self.spans
            .push(ResolvedSpan::from_parts(sequence, mcid, style, chars));
        true
    }

    pub(crate) fn into_spans(mut self) -> Vec<ResolvedSpan> {
        let _ = self.finish_run();
        self.spans
    }
}

#[cfg(test)]
mod tests {
    use super::ResolvedCollector;
    use crate::geometry::Rect;
    use crate::layout::{Color, FontWeight, ResolvedChar, ResolvedStyle};

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
    fn resolved_collector_finishes_runs_in_pdf_order() {
        let mut collector = ResolvedCollector::default();
        collector.begin_run(0, Some(1), style());
        collector.push_char(ResolvedChar {
            text: 'A',
            bbox: Rect::new(10.0, 20.0, 5.0, 8.0),
            rotation_degrees: Some(0.0),
        });
        collector.finish_run();

        collector.begin_run(1, Some(2), style());
        collector.push_char(ResolvedChar {
            text: 'B',
            bbox: Rect::new(40.0, 20.0, 5.0, 8.0),
            rotation_degrees: Some(0.0),
        });
        collector.finish_run();

        let spans = collector.into_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].sequence, 0);
        assert_eq!(spans[0].text, "A");
        assert_eq!(spans[1].sequence, 1);
        assert_eq!(spans[1].text, "B");
    }
}
