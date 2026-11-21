use super::onnx_models::VisualPrediction;
use super::raster::GlyphGeometry;

/// Purely geometric 1D degenerate-aspect guard (`一`, `丨`, `丶`) applied even when
/// `hw_model` ONNX is loaded (because `points_normalized` stretches 1D strokes across 48x48).
pub fn classify_1d_primitive(geom: &GlyphGeometry) -> Option<Vec<VisualPrediction>> {
    if geom.stroke_count != 1 {
        return None;
    }
    if geom.is_single_horizontal {
        return Some(vec![
            VisualPrediction {
                ch: '一',
                prob: 0.94,
                raw_logit: 4.5,
            },
            VisualPrediction {
                ch: '丶',
                prob: 0.03,
                raw_logit: 1.0,
            },
            VisualPrediction {
                ch: '丨',
                prob: 0.03,
                raw_logit: 1.0,
            },
        ]);
    }
    if geom.is_single_vertical {
        return Some(vec![
            VisualPrediction {
                ch: '丨',
                prob: 0.94,
                raw_logit: 4.5,
            },
            VisualPrediction {
                ch: '丿',
                prob: 0.03,
                raw_logit: 1.0,
            },
            VisualPrediction {
                ch: '丶',
                prob: 0.03,
                raw_logit: 1.0,
            },
        ]);
    }
    if geom.is_single_dot {
        return Some(vec![
            VisualPrediction {
                ch: '丶',
                prob: 0.94,
                raw_logit: 4.5,
            },
            VisualPrediction {
                ch: '一',
                prob: 0.03,
                raw_logit: 1.0,
            },
            VisualPrediction {
                ch: '丨',
                prob: 0.03,
                raw_logit: 1.0,
            },
        ]);
    }
    None
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
