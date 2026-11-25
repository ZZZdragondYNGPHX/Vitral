use super::onnx_models::VisualPrediction;
use super::raster::GlyphGeometry;

/// Purely geometric 1D degenerate-aspect guard (`一`, `丨`, `丶`) applied even when
/// `hw_model` ONNX is loaded (because `points_normalized` stretches 1D strokes across 48x48).
pub fn classify_1d_primitive(geom: &GlyphGeometry) -> Option<Vec<VisualPrediction>> {
    if geom.stroke_count != 1 {
        return None;
    }
    let candidates: &[(char, f32)] = if geom.is_single_horizontal {
        &[('一', 0.44), ('-', 0.32), ('丶', 0.16), ('丨', 0.08)]
    } else if geom.is_single_vertical {
        &[('丨', 0.44), ('1', 0.32), ('丿', 0.16), ('丶', 0.08)]
    } else if geom.is_single_dot {
        &[('丶', 0.42), ('、', 0.30), ('.', 0.16), ('一', 0.08), ('丨', 0.04)]
    } else {
        return None;
    };
    Some(
        candidates
            .iter()
            .map(|&(ch, prob)| VisualPrediction {
                ch,
                prob,
                raw_logit: prob.ln(),
            })
            .collect(),
    )
}

/// Purely geometric single-stroke primitive recognizer (`一`, `丨`, `丶`, `丿`, `乀`)
/// used when `hw_model` ONNX is unavailable. Multi-stroke character recognition has NO
/// hardcoded prototype table and requires `hw_model` ONNX (`VitralError::ModelUnavailable`).
pub fn classify_single_stroke_primitive(geom: &GlyphGeometry) -> Option<Vec<VisualPrediction>> {
    if let Some(p) = classify_1d_primitive(geom) {
        return Some(p);
    }
    if geom.stroke_count != 1 {
        return None;
    }
    if geom.is_single_pie {
        return Some(vec![
            VisualPrediction {
                ch: '丿',
                prob: 0.92,
                raw_logit: 4.2,
            },
            VisualPrediction {
                ch: '丨',
                prob: 0.05,
                raw_logit: 1.3,
            },
            VisualPrediction {
                ch: '丶',
                prob: 0.03,
                raw_logit: 0.8,
            },
        ]);
    }
    if geom.is_single_na {
        return Some(vec![
            VisualPrediction {
                ch: '乀',
                prob: 0.92,
                raw_logit: 4.2,
            },
            VisualPrediction {
                ch: '丶',
                prob: 0.05,
                raw_logit: 1.3,
            },
            VisualPrediction {
                ch: '一',
                prob: 0.03,
                raw_logit: 0.8,
            },
        ]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::{lattice::build_lattice_slot_from_predictions, StrokePoint};

    #[test]
    fn single_strokes_keep_digit_and_punctuation_choices() {
        type StrokeCase<'a> = (&'a [(f32, f32)], &'a [char]);
        let cases: &[StrokeCase<'_>] = &[
            (&[(0.5, 0.1), (0.5, 0.9)], &['丨', '1']),
            (&[(0.1, 0.5), (0.9, 0.5)], &['一', '-']),
            (&[(0.5, 0.5)], &['丶', '、', '.']),
        ];
        for &(points, expected) in cases {
            let stroke = points
                .iter()
                .map(|&(x, y)| StrokePoint {
                    x,
                    y,
                    timestamp_ms: 0,
                })
                .collect();
            let geom = GlyphGeometry::from_strokes(&[stroke]);
            let predictions = classify_1d_primitive(&geom).unwrap();
            let slot = build_lattice_slot_from_predictions(0, 0, 0, &predictions, false);
            assert!(!slot.locked_by_entropy);
            let got: Vec<_> = predictions.iter().map(|p| p.ch).collect();
            assert!(got.starts_with(expected));
        }
    }
}
