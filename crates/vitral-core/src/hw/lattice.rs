use super::ids_exta::{canonical_radical, find_bipartite_splits, IdcOp, IdsIndex};
use super::onnx_models::{HwOnnxBundle, VisualPrediction};
use super::pure_fallback::{classify_1d_primitive, classify_single_stroke_primitive};
use super::raster::{global_bbox, render_directmap, GlyphGeometry, StrokeStats, DIRECTMAP_LEN};
use super::{StrokePoint, Top8Candidate};
use crate::error::VitralError;
use vitral_tokens::TOKENS;

#[derive(Debug, Clone)]
pub struct SegmentedSpan {
    pub stroke_start: usize,
    pub stroke_end: usize, // inclusive
    pub strokes: Vec<Vec<StrokePoint>>,
}

#[derive(Debug, Clone)]
pub struct LatticeSlot {
    pub slot_idx: usize,
    pub stroke_start: usize,
    pub stroke_end: usize,
    pub locked_by_entropy: bool,
    pub manually_locked: bool,
    pub margin_delta_p: f32,
    pub top8: Vec<Top8Candidate>,
}

/// Dual-regime CASIA-OLHWDB spatiotemporal stroke segmentation:
/// - Superimposed writing: a long pause before restarting in the previous glyph area splits the ink.
/// - Horizontal connected writing: routes pause/wide-gap candidates through a batched
///   visual DAG + `IdsIndex` structural-compound gate (keeping LR single characters like
///   `明`/`好`/`你` unified even across intra-char pauses while splitting horizontal sequences).
pub fn segment_strokes(
    strokes: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
) -> Vec<SegmentedSpan> {
    let valid: Vec<(usize, StrokeStats)> = strokes
        .iter()
        .enumerate()
        .filter_map(|(i, s)| StrokeStats::from_stroke(s).map(|st| (i, st)))
        .collect();
    if valid.is_empty() {
        return Vec::new();
    }
    if valid.len() == 1 {
        return vec![SegmentedSpan {
            stroke_start: valid[0].0,
            stroke_end: valid[0].0,
            strokes: vec![strokes[valid[0].0].clone()],
        }];
    }

    // Compute median intra-stroke pen-lift gap for pause ratio estimation
    let mut dts: Vec<u64> = Vec::with_capacity(valid.len());
    for i in 0..(valid.len() - 1) {
        dts.push(valid[i + 1].1.t_start.saturating_sub(valid[i].1.t_end));
    }
    dts.sort_unstable();
    let median_dt = dts.get(dts.len() / 2).copied().unwrap_or(50).max(20);
    let fade_ms = TOKENS.handwriting.superimposed_ink_fade_out_ms as u64;

    // Collect overlaid restarts and possible horizontal boundaries.
    let mut hard_splits = vec![false; valid.len() - 1];
    let mut horiz_candidates = vec![false; valid.len() - 1];

    let mut cur_min_x = valid[0].1.min_x;
    let mut cur_max_x = valid[0].1.max_x;
    let mut cur_min_y = valid[0].1.min_y;
    let mut cur_max_y = valid[0].1.max_y;
    let mut cur_count = 1usize;

    for i in 0..(valid.len() - 1) {
        let dt = valid[i + 1].1.t_start.saturating_sub(valid[i].1.t_end);
        let next = &valid[i + 1].1;
        let cur_w = (cur_max_x - cur_min_x).max(1e-4);
        let cur_h = (cur_max_y - cur_min_y).max(1e-4);
        let char_scale = cur_h.max(cur_w);
        let gap_x = next.min_x - cur_max_x;
        let paused = dt >= fade_ms;

        // A pause before the right component of a glyph needs the same visual
        // comparison as a short pen lift. An overlaid restart stays a boundary.
        if paused && gap_x < -0.05 * char_scale {
            hard_splits[i] = true;
            cur_min_x = valid[i + 1].1.min_x;
            cur_max_x = valid[i + 1].1.max_x;
            cur_min_y = valid[i + 1].1.min_y;
            cur_max_y = valid[i + 1].1.max_y;
            cur_count = 1;
            continue;
        }

        // Check horizontal non-overlap with the next stroke AND ensure no subsequent stroke
        // before the next hard split jumps back left into [cur_min_x, cur_max_x]
        let mut jumps_back_left = false;
        let mut merged_max_x = cur_max_x.max(next.max_x);
        let mut merged_max_y = cur_max_y.max(next.max_y);
        let mut merged_min_y = cur_min_y.min(next.min_y);
        for j in (i + 1)..valid.len() {
            if j > i + 1 && valid[j].1.t_start.saturating_sub(valid[j - 1].1.t_end) >= fade_ms {
                break;
            }
            if valid[j].1.min_x < cur_max_x - 0.05 * char_scale {
                jumps_back_left = true;
                break;
            }
            // Look ahead to the next cluster (~5 strokes) for merged aspect estimate
            if j - (i + 1) < 5 {
                merged_max_x = merged_max_x.max(valid[j].1.max_x);
                merged_max_y = merged_max_y.max(valid[j].1.max_y);
                merged_min_y = merged_min_y.min(valid[j].1.min_y);
            }
        }
        let merged_w = merged_max_x - cur_min_x;
        let merged_h = (merged_max_y - merged_min_y).max(1e-4);
        let merged_aspect = merged_w / merged_h;

        let pause_signal = paused || (dt >= 135 && (dt as f32) >= 2.2 * (median_dt as f32));
        let wide_gap_signal = gap_x > 0.28 * char_scale && merged_aspect > 1.18;

        // Never hard-split on sub-420ms pauses blindly; route through visual DAG + IDS gate.
        if (paused || gap_x > 0.02 * char_scale)
            && (paused || !jumps_back_left)
            && (paused || cur_count >= 2)
            && (pause_signal || wide_gap_signal)
        {
            horiz_candidates[i] = true;
            cur_min_x = next.min_x;
            cur_max_x = next.max_x;
            cur_min_y = next.min_y;
            cur_max_y = next.max_y;
            cur_count = 1;
        } else {
            cur_min_x = cur_min_x.min(next.min_x);
            cur_max_x = cur_max_x.max(next.max_x);
            cur_min_y = cur_min_y.min(next.min_y);
            cur_max_y = cur_max_y.max(next.max_y);
            cur_count += 1;
        }
    }

    // Step 2: Resolve horizontal candidates via batched visual DAG + IDS compound gating
    if horiz_candidates.iter().any(|&b| b) {
        let gap_at: Vec<f32> = (0..(valid.len() - 1))
            .map(|k| valid[k + 1].1.min_x - valid[k].1.max_x)
            .collect();
        let mut i = 0usize;
        while i < horiz_candidates.len() {
            if horiz_candidates[i] {
                let seg_start = (0..i)
                    .rev()
                    .find(|&k| hard_splits[k])
                    .map(|k| k + 1)
                    .unwrap_or(0);
                let cur_gap = gap_at[i].max(0.0);
                let seg_end = ((i + 1)..hard_splits.len())
                    .find(|&k| {
                        hard_splits[k] || (horiz_candidates[k] && gap_at[k] >= 0.75 * cur_gap)
                    })
                    .unwrap_or(valid.len() - 1);
                let left_s: Vec<Vec<StrokePoint>> = valid[seg_start..=i]
                    .iter()
                    .map(|&(idx, _)| strokes[idx].clone())
                    .collect();
                let right_s: Vec<Vec<StrokePoint>> = valid[(i + 1)..=seg_end]
                    .iter()
                    .map(|&(idx, _)| strokes[idx].clone())
                    .collect();
                let merged_s: Vec<Vec<StrokePoint>> = valid[seg_start..=seg_end]
                    .iter()
                    .map(|&(idx, _)| strokes[idx].clone())
                    .collect();

                let (l_min_x, l_max_x, l_min_y, l_max_y) =
                    global_bbox(&left_s).unwrap_or((0.0, 0.0, 0.0, 0.0));
                let (r_min_x, _r_max_x, _r_min_y, _r_max_y) =
                    global_bbox(&right_s).unwrap_or((0.0, 0.0, 0.0, 0.0));
                let (m_min_x, m_max_x, m_min_y, m_max_y) =
                    global_bbox(&merged_s).unwrap_or((0.0, 0.0, 0.0, 0.0));
                let l_w = (l_max_x - l_min_x).max(1e-4);
                let l_h = (l_max_y - l_min_y).max(1e-4);
                let char_scale = l_h.max(l_w);
                let gap_x = r_min_x - l_max_x;
                let merged_w = (m_max_x - m_min_x).max(1e-4);
                let merged_h = (m_max_y - m_min_y).max(1e-4);
                let merged_aspect = merged_w / merged_h;

                let (preds_l, preds_r, preds_m) =
                    quick_top3_triple(&left_s, &right_s, &merged_s, bundle);
                let p_l = preds_l.first().map(|p| p.prob).unwrap_or(0.0);
                let p_r = preds_r.first().map(|p| p.prob).unwrap_or(0.0);
                let p_m = preds_m.first().map(|p| p.prob).unwrap_or(0.0);

                let compact_single_box = merged_aspect <= 1.35 && gap_x <= 0.28 * char_scale;
                let ids_compound_match = compact_single_box
                    && !ids.is_empty()
                    && !preds_l.is_empty()
                    && !preds_r.is_empty()
                    && preds_m.iter().take(3).any(|pm| {
                        pm.prob >= 0.25
                            && matches!(
                                ids.char_decomposition(pm.ch),
                                Some((IdcOp::LeftRight, a, b))
                                    if canonical_radical(a) == canonical_radical(preds_l[0].ch)
                                        && canonical_radical(b) == canonical_radical(preds_r[0].ch)
                            )
                    });

                let geo_mean = (p_l * p_r).sqrt();
                let should_split = if ids_compound_match {
                    false
                } else if compact_single_box {
                    (geo_mean > p_m * 1.0) || (!bundle.has_hw_onnx() && p_m == 0.0)
                } else {
                    (geo_mean > p_m * 0.80) || (!bundle.has_hw_onnx() && p_m == 0.0)
                };
                if should_split {
                    hard_splits[i] = true;
                }
            }
            i += 1;
        }
    }

    // Build final spans
    let mut spans = Vec::new();
    let mut start = 0usize;
    for i in 0..valid.len() {
        let is_end = i == valid.len() - 1 || hard_splits[i];
        if is_end {
            let chunk: Vec<Vec<StrokePoint>> = valid[start..=i]
                .iter()
                .map(|&(idx, _)| strokes[idx].clone())
                .collect();
            spans.push(SegmentedSpan {
                stroke_start: valid[start].0,
                stroke_end: valid[i].0,
                strokes: chunk,
            });
            start = i + 1;
        }
    }
    spans
}

fn quick_top3_triple(
    left_s: &[Vec<StrokePoint>],
    right_s: &[Vec<StrokePoint>],
    merged_s: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
) -> (
    Vec<VisualPrediction>,
    Vec<VisualPrediction>,
    Vec<VisualPrediction>,
) {
    if bundle.has_hw_onnx() {
        let mut flat = Vec::with_capacity(3 * DIRECTMAP_LEN);
        flat.extend_from_slice(&render_directmap(left_s));
        flat.extend_from_slice(&render_directmap(right_s));
        flat.extend_from_slice(&render_directmap(merged_s));
        if let Some(mut batch) = bundle.classify_directmap_batch(&flat, 3, 3) {
            if batch.len() == 3 {
                let mut preds_m = batch.pop().unwrap();
                let mut preds_r = batch.pop().unwrap();
                let mut preds_l = batch.pop().unwrap();
                if let Some(p) = classify_1d_primitive(&GlyphGeometry::from_strokes(left_s)) {
                    preds_l = p;
                }
                if let Some(p) = classify_1d_primitive(&GlyphGeometry::from_strokes(right_s)) {
                    preds_r = p;
                }
                if let Some(p) = classify_1d_primitive(&GlyphGeometry::from_strokes(merged_s)) {
                    preds_m = p;
                }
                return (preds_l, preds_r, preds_m);
            }
        }
    }
    (
        classify_single_stroke_primitive(&GlyphGeometry::from_strokes(left_s)).unwrap_or_default(),
        classify_single_stroke_primitive(&GlyphGeometry::from_strokes(right_s)).unwrap_or_default(),
        classify_single_stroke_primitive(&GlyphGeometry::from_strokes(merged_s))
            .unwrap_or_default(),
    )
}

/// Build a single `LatticeSlot` from Top-K predictions & radical ambiguity flag.
pub fn build_lattice_slot_from_predictions(
    slot_idx: usize,
    stroke_start: usize,
    stroke_end: usize,
    preds: &[VisualPrediction],
    is_radical_ambig: bool,
) -> LatticeSlot {
    let top_k = TOKENS.handwriting.confusion_top_k as usize;
    let margin_thresh = TOKENS.handwriting.ambiguous_margin_threshold;
    let k_preds: Vec<VisualPrediction> = preds.iter().take(top_k).cloned().collect();

    if k_preds.is_empty() {
        return LatticeSlot {
            slot_idx,
            stroke_start,
            stroke_end,
            locked_by_entropy: false,
            manually_locked: false,
            margin_delta_p: 0.0,
            top8: Vec::new(),
        };
    }

    // Temperature calibration over Top-8 logits:
    // If structural-sibling ambiguity is detected AND the minor radical itself has low
    // visual clarity (< 0.65), soften temperature over Top-8 so sibling remains elastic (Δp < 0.15).
    let temp = if is_radical_ambig && k_preds.len() >= 2 {
        let dz = (k_preds[0].raw_logit - k_preds[1].raw_logit).max(0.1);
        (dz / 0.25).max(3.5)
    } else {
        1.0
    };

    let max_z = k_preds
        .iter()
        .map(|p| p.raw_logit / temp)
        .fold(f32::NEG_INFINITY, f32::max);
    let sum_e: f32 = k_preds
        .iter()
        .map(|p| ((p.raw_logit / temp) - max_z).exp())
        .sum();

    let top8: Vec<Top8Candidate> = k_preds
        .iter()
        .map(|p| {
            let norm_p = (((p.raw_logit / temp) - max_z).exp() / sum_e.max(1e-6)).clamp(0.0, 1.0);
            Top8Candidate {
                ch: p.ch.to_string(),
                visual_prob: norm_p,
                joint_score: norm_p.max(1e-6).ln(),
            }
        })
        .collect();

    let p1 = top8.first().map(|c| c.visual_prob).unwrap_or(0.0);
    let p2 = top8.get(1).map(|c| c.visual_prob).unwrap_or(0.0);
    let margin_delta_p = (p1 - p2).max(0.0);
    let locked_by_entropy = margin_delta_p >= margin_thresh;

    LatticeSlot {
        slot_idx,
        stroke_start,
        stroke_end,
        locked_by_entropy,
        manually_locked: false,
        margin_delta_p,
        top8,
    }
}

/// Build the Top-8 Confusion Lattice & evaluate the Visual-Entropy Gate (`Δp = p1 - p2 >= 0.15`).
pub fn build_confusion_lattice(
    spans: &[SegmentedSpan],
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
) -> Result<Vec<LatticeSlot>, VitralError> {
    if spans.is_empty() {
        return Ok(Vec::new());
    }
    let top_k = TOKENS.handwriting.confusion_top_k as usize;

    let mut minor_top_prob: Vec<Option<f32>> = vec![None; spans.len()];
    let raw_preds: Vec<Vec<VisualPrediction>> = if bundle.has_hw_onnx() {
        let mut flat = Vec::with_capacity(spans.len() * DIRECTMAP_LEN);
        for sp in spans {
            flat.extend_from_slice(&render_directmap(&sp.strokes));
        }
        let mut batch_res = bundle
            .classify_directmap_batch(&flat, spans.len(), top_k.max(8))
            .ok_or_else(|| {
                VitralError::ModelUnavailable("hw_model ONNX inference failed".into())
            })?;
        // Apply 1D single-stroke primitive override (`一`, `丨`, `丶`)
        for (idx, sp) in spans.iter().enumerate() {
            let geom = GlyphGeometry::from_strokes(&sp.strokes);
            if let Some(prim) = classify_1d_primitive(&geom) {
                batch_res[idx] = prim;
            }
        }

        // Detect structural sibling ambiguity for multi-stroke compound spans (e.g. 曝/爆)
        // where the major right/bottom component dominates the 48x48 CNN while the minor left/top
        // radical was written ambiguously/cursively.
        let mut minor_checks: Vec<(usize, Vec<Vec<StrokePoint>>)> = Vec::new();
        for (idx, sp) in spans.iter().enumerate() {
            let preds = &batch_res[idx];
            if sp.strokes.len() >= 6 && preds.len() >= 2 {
                let top1 = &preds[0];
                let has_sibling = preds
                    .iter()
                    .skip(1)
                    .take(4)
                    .any(|p| ids.are_structural_siblings(top1.ch, p.ch));
                if has_sibling && top1.raw_logit < 2.5 {
                    let splits = find_bipartite_splits(&sp.strokes);
                    if let Some(best_split) = splits.first() {
                        minor_checks.push((idx, best_split.left_or_top.clone()));
                    } else {
                        let prefix_n = 4.min(sp.strokes.len() / 2);
                        minor_checks.push((idx, sp.strokes[..prefix_n].to_vec()));
                    }
                }
            }
        }

        if !minor_checks.is_empty() {
            let mut mflat = Vec::with_capacity(minor_checks.len() * DIRECTMAP_LEN);
            for (_, st) in &minor_checks {
                mflat.extend_from_slice(&render_directmap(st));
            }
            if let Some(res) = bundle.classify_directmap_batch(&mflat, minor_checks.len(), 3) {
                for ((slot_i, _), preds) in minor_checks.into_iter().zip(res) {
                    let p_sum: f32 = if preds.len() >= 2
                        && canonical_radical(preds[0].ch) == canonical_radical(preds[1].ch)
                    {
                        preds[0].prob + preds[1].prob
                    } else {
                        preds.first().map(|p| p.prob).unwrap_or(0.0)
                    };
                    minor_top_prob[slot_i] = Some(p_sum);
                }
            }
        }

        batch_res
    } else {
        let mut out = Vec::with_capacity(spans.len());
        for sp in spans {
            let geom = GlyphGeometry::from_strokes(&sp.strokes);
            let prim = classify_single_stroke_primitive(&geom).ok_or_else(|| {
                VitralError::ModelUnavailable(
                    "handwriting ONNX model (hw_model.onnx / hw_model.int8.onnx) is unavailable for multi-stroke recognition".into(),
                )
            })?;
            out.push(prim);
        }
        out
    };

    let slots = spans
        .iter()
        .enumerate()
        .map(|(idx, sp)| {
            let is_radical_ambig = minor_top_prob[idx].is_some_and(|p| p < 0.65);
            build_lattice_slot_from_predictions(
                idx,
                sp.stroke_start,
                sp.stroke_end,
                &raw_preds[idx],
                is_radical_ambig,
            )
        })
        .collect();
    Ok(slots)
}
