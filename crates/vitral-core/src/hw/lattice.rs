use super::ids_exta::{canonical_radical, find_bipartite_splits, IdcOp, IdsIndex};
use super::onnx_models::{HwOnnxBundle, VisualPrediction};
use super::pure_fallback::{classify_1d_primitive, classify_single_stroke_primitive};
use super::raster::{global_bbox, render_directmap, GlyphGeometry, StrokeStats};
use super::{StrokePoint, Top8Candidate};
use crate::error::VitralError;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Weak};
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

type StrokeSpan = (usize, usize);

#[derive(Debug)]
struct SegmentationVisual {
    bounds: (f32, f32, f32, f32),
    geometry: GlyphGeometry,
    predictions: Option<Vec<VisualPrediction>>,
}

#[derive(Debug, Default)]
pub(super) struct VisualCache {
    bundle: Weak<HwOnnxBundle>,
    strokes: Vec<(Vec<StrokePoint>, Option<StrokeStats>)>,
    segmentation: HashMap<StrokeSpan, SegmentationVisual>,
    lattice: HashMap<StrokeSpan, (usize, LatticeSlot)>,
    #[cfg(all(test, feature = "native-onnx", target_os = "linux"))]
    pub(super) lattice_cache_hits: usize,
}

impl VisualCache {
    pub(super) fn update(&mut self, strokes: &[Vec<StrokePoint>], bundle: &Arc<HwOnnxBundle>) {
        let bundle = Arc::downgrade(bundle);
        if !self.bundle.ptr_eq(&bundle) {
            self.segmentation.clear();
            self.lattice.clear();
            self.bundle = bundle;
        }
        self.update_strokes(strokes);
    }

    fn update_strokes(&mut self, strokes: &[Vec<StrokePoint>]) {
        // HandwritingSession exposes the strokes to Rust callers, including edits to earlier ink.
        let unchanged = self
            .strokes
            .iter()
            .zip(strokes)
            .take_while(|((old, _), new)| {
                old.len() == new.len()
                    && old.iter().zip(*new).all(|(a, b)| {
                        a.x.to_bits() == b.x.to_bits()
                            && a.y.to_bits() == b.y.to_bits()
                            && a.timestamp_ms == b.timestamp_ms
                    })
            })
            .count();
        if unchanged < self.strokes.len() {
            self.strokes.truncate(unchanged);
            self.segmentation.retain(|&(_, end), _| end < unchanged);
            self.lattice.retain(|&(_, end), _| end < unchanged);
        }
        self.strokes.extend(
            strokes[unchanged..]
                .iter()
                .map(|s| (s.clone(), StrokeStats::from_stroke(s))),
        );
    }

    #[cfg(all(test, feature = "native-onnx", target_os = "linux"))]
    pub(super) fn reset_visuals(&mut self) {
        self.segmentation.clear();
        self.lattice.clear();
    }
}

pub(super) fn segment_strokes_with_locks(
    strokes: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
    locked: impl Iterator<Item = (usize, usize)>,
    cache: &mut VisualCache,
) -> Vec<SegmentedSpan> {
    let mut locked: Vec<_> = locked.collect();
    locked.sort_unstable();
    let mut spans = Vec::new();
    let mut offset = 0;
    for (start, end) in locked {
        if start < offset || end >= strokes.len() || start > end {
            continue;
        }
        spans.extend(segment_range(strokes, offset..start, bundle, ids, cache));
        spans.push(SegmentedSpan {
            stroke_start: start,
            stroke_end: end,
            strokes: strokes[start..=end].to_vec(),
        });
        offset = end + 1;
    }
    spans.extend(segment_range(
        strokes,
        offset..strokes.len(),
        bundle,
        ids,
        cache,
    ));
    spans
}

/// Dual-regime CASIA-OLHWDB spatiotemporal stroke segmentation:
/// - Superimposed writing: a long pause before restarting in the previous glyph area splits the ink.
/// - Horizontal connected writing: routes pause/wide-gap candidates through a
///   visual DAG + `IdsIndex` structural-compound gate (keeping LR single characters like
///   `明`/`好`/`你` unified even across intra-char pauses while splitting horizontal sequences).
pub fn segment_strokes(
    strokes: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
) -> Vec<SegmentedSpan> {
    let mut cache = VisualCache::default();
    cache.update_strokes(strokes);
    segment_range(strokes, 0..strokes.len(), bundle, ids, &mut cache)
}

fn segment_range(
    strokes: &[Vec<StrokePoint>],
    range: Range<usize>,
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
    cache: &mut VisualCache,
) -> Vec<SegmentedSpan> {
    let offset = range.start;
    let valid: Vec<(usize, StrokeStats)> = cache.strokes[range]
        .iter()
        .enumerate()
        .filter_map(|(i, (_, stats))| stats.clone().map(|st| (offset + i, st)))
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

        // A paused stroke that returns deep into the left half starts overlaid ink.
        // Shallow overlap near the right edge still needs a visual comparison.
        let overlaid_restart =
            next.cx <= cur_min_x + 0.5 * cur_w && cur_max_x - next.cx >= 0.25 * char_scale;
        if paused && overlaid_restart {
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

    // Resolve horizontal candidates via the visual DAG and IDS compound gate.
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
                let [left, right, merged] = [
                    &valid[seg_start..=i],
                    &valid[(i + 1)..=seg_end],
                    &valid[seg_start..=seg_end],
                ];
                let bounds = |s: &[(usize, StrokeStats)]| (s[0].0, s[s.len() - 1].0);
                let keys = [bounds(left), bounds(right), bounds(merged)];
                let [preds_l, preds_r, preds_m] =
                    segmentation_predictions(strokes, [left, right, merged], keys, bundle, cache);

                let (l_min_x, l_max_x, l_min_y, l_max_y) = cache.segmentation[&keys[0]].bounds;
                let (r_min_x, _, _, _) = cache.segmentation[&keys[1]].bounds;
                let (m_min_x, m_max_x, m_min_y, m_max_y) = cache.segmentation[&keys[2]].bounds;
                let l_w = (l_max_x - l_min_x).max(1e-4);
                let l_h = (l_max_y - l_min_y).max(1e-4);
                let char_scale = l_h.max(l_w);
                let gap_x = r_min_x - l_max_x;
                let merged_w = (m_max_x - m_min_x).max(1e-4);
                let merged_h = (m_max_y - m_min_y).max(1e-4);
                let merged_aspect = merged_w / merged_h;

                let p_l = preds_l.first().map(|p| p.prob).unwrap_or(0.0);
                let p_r = preds_r.first().map(|p| p.prob).unwrap_or(0.0);
                let p_m = preds_m.first().map(|p| p.prob).unwrap_or(0.0);

                let compact_single_box = merged_aspect <= 1.55 && gap_x <= 0.33 * char_scale;
                let ids_compound_match =
                    compact_single_box && matches_ids_compound(ids, &preds_l, &preds_r, &preds_m);
                let geo_mean = (p_l * p_r).sqrt();
                let merged_margin = if compact_single_box { 1.0 } else { 0.80 };
                let separated_without_model = p_m == 0.0 && gap_x >= 0.0 && !bundle.has_hw_onnx();
                let should_split = !ids_compound_match
                    && (geo_mean > p_m * merged_margin || separated_without_model);
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

fn matches_ids_compound(
    ids: &IdsIndex,
    left: &[VisualPrediction],
    right: &[VisualPrediction],
    merged: &[VisualPrediction],
) -> bool {
    merged.iter().take(3).filter(|p| p.prob >= 0.25).any(|p| {
        let Some((IdcOp::LeftRight, a, b)) = ids.char_decomposition(p.ch) else {
            return false;
        };
        let (a, b) = (canonical_radical(a), canonical_radical(b));
        left.iter().take(3).any(|p| canonical_radical(p.ch) == a)
            && right.iter().take(3).any(|p| canonical_radical(p.ch) == b)
    })
}

fn segmentation_predictions(
    strokes: &[Vec<StrokePoint>],
    spans: [&[(usize, StrokeStats)]; 3],
    keys: [StrokeSpan; 3],
    bundle: &HwOnnxBundle,
    cache: &mut VisualCache,
) -> [Vec<VisualPrediction>; 3] {
    let mut inference_failed = false;
    for (span, key) in spans.into_iter().zip(keys) {
        if cache
            .segmentation
            .get(&key)
            .is_some_and(|vis| vis.predictions.is_some())
        {
            continue;
        }
        let ink: Vec<_> = span
            .iter()
            .map(|(index, _)| strokes[*index].clone())
            .collect();
        let vis = cache
            .segmentation
            .entry(key)
            .or_insert_with(|| SegmentationVisual {
                bounds: global_bbox(&ink).unwrap_or_default(),
                geometry: GlyphGeometry::from_strokes(&ink),
                predictions: None,
            });
        if !bundle.has_hw_onnx() {
            vis.predictions =
                Some(classify_single_stroke_primitive(&vis.geometry).unwrap_or_default());
        } else if let Some(preds) = classify_span(&ink, bundle, 3) {
            vis.predictions = Some(classify_1d_primitive(&vis.geometry).unwrap_or(preds));
        } else {
            inference_failed = true;
        }
    }
    keys.map(|key| {
        let vis = &cache.segmentation[&key];
        if inference_failed {
            // Keep the geometry fallback for this comparison and retry failed inference later.
            classify_single_stroke_primitive(&vis.geometry).unwrap_or_default()
        } else {
            vis.predictions.clone().unwrap_or_default()
        }
    })
}

fn classify_span(
    ink: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
    top_k: usize,
) -> Option<Vec<VisualPrediction>> {
    // The int8 classifier derives an activation scale from its whole batch.
    // A single-span batch keeps predictions independent of other ink.
    bundle
        .classify_directmap_batch(&render_directmap(ink), 1, top_k)?
        .pop()
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
    spans
        .iter()
        .enumerate()
        .map(|(i, span)| {
            let (mut slot, _) = build_span_slot(span, bundle, ids)?;
            slot.slot_idx = i;
            Ok(slot)
        })
        .collect()
}

pub(super) fn build_cached_lattice(
    spans: &[SegmentedSpan],
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
    cache: &mut VisualCache,
) -> Result<Vec<LatticeSlot>, VitralError> {
    spans
        .iter()
        .enumerate()
        .map(|(i, span)| {
            let key = (span.stroke_start, span.stroke_end);
            // Locked ranges include empty/invalid strokes that normal segmentation filters out.
            let count = span.strokes.len();
            let mut slot =
                if let Some((_, slot)) = cache.lattice.get(&key).filter(|(n, _)| *n == count) {
                    #[cfg(all(test, feature = "native-onnx", target_os = "linux"))]
                    {
                        cache.lattice_cache_hits += 1;
                    }
                    slot.clone()
                } else {
                    let (slot, reusable) = build_span_slot(span, bundle, ids)?;
                    if reusable {
                        cache.lattice.insert(key, (count, slot.clone()));
                    }
                    slot
                };
            slot.slot_idx = i;
            Ok(slot)
        })
        .collect()
}

fn build_span_slot(
    span: &SegmentedSpan,
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
) -> Result<(LatticeSlot, bool), VitralError> {
    let geom = GlyphGeometry::from_strokes(&span.strokes);
    let (preds, minor_top_prob, reusable) = if bundle.has_hw_onnx() {
        let top_k = TOKENS.handwriting.confusion_top_k as usize;
        let raw_preds = classify_span(&span.strokes, bundle, top_k.max(8)).ok_or_else(|| {
            VitralError::ModelUnavailable("hw_model ONNX inference failed".into())
        })?;
        let preds = classify_1d_primitive(&geom).unwrap_or(raw_preds);

        // Soften structural siblings (e.g. 曝/爆) when the minor radical is unclear.
        let mut minor_top_prob = None;
        let mut reusable = true;
        if span.strokes.len() >= 6
            && preds.len() >= 2
            && preds[0].raw_logit < 2.5
            && preds
                .iter()
                .skip(1)
                .take(4)
                .any(|p| ids.are_structural_siblings(preds[0].ch, p.ch))
        {
            let splits = find_bipartite_splits(&span.strokes);
            let minor = splits.first().map_or_else(
                || &span.strokes[..4.min(span.strokes.len() / 2)],
                |split| split.left_or_top.as_slice(),
            );
            if let Some(minor_preds) = classify_span(minor, bundle, 3) {
                minor_top_prob = Some(match minor_preds.as_slice() {
                    [a, b, ..] if canonical_radical(a.ch) == canonical_radical(b.ch) => {
                        a.prob + b.prob
                    }
                    [a, ..] => a.prob,
                    [] => 0.0,
                });
            } else {
                reusable = false;
            }
        }
        (preds, minor_top_prob, reusable)
    } else {
        let preds = classify_single_stroke_primitive(&geom).ok_or_else(|| {
            VitralError::ModelUnavailable(
                "handwriting ONNX model (hw_model.onnx / hw_model.int8.onnx) is unavailable for multi-stroke recognition".into(),
            )
        })?;
        (preds, None, true)
    };
    let slot = build_lattice_slot_from_predictions(
        0,
        span.stroke_start,
        span.stroke_end,
        &preds,
        minor_top_prob.is_some_and(|p| p < 0.65),
    );
    Ok((slot, reusable))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(points: &[(f32, f32)], start: u64) -> Vec<StrokePoint> {
        points
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| StrokePoint {
                x,
                y,
                timestamp_ms: start + i as u64 * 10,
            })
            .collect()
    }

    #[test]
    fn long_pause_only_hard_splits_deep_left_restarts() {
        let bundle = HwOnnxBundle::load_paths(&crate::models::ModelPaths::default());
        let ids = IdsIndex::default();
        let first = stroke(&[(0.10, 0.10), (0.75, 0.10), (0.75, 0.80)], 0);
        let shallow_overlap = stroke(&[(0.68, 0.40), (0.85, 0.80)], 600);
        let restart = stroke(&[(0.25, 0.20), (0.25, 0.70)], 600);
        assert_eq!(
            segment_strokes(&[first.clone(), shallow_overlap], &bundle, &ids).len(),
            1
        );
        assert_eq!(segment_strokes(&[first, restart], &bundle, &ids).len(), 2);
    }

    #[test]
    fn locked_span_keeps_both_boundaries_and_blocks_internal_splits() {
        let bundle = HwOnnxBundle::load_paths(&crate::models::ModelPaths::default());
        let ids = IdsIndex::default();
        let strokes = [
            stroke(&[(0.1, 0.1), (0.1, 0.8)], 0),
            stroke(&[(0.1, 0.3), (0.8, 0.3)], 30),
            stroke(&[(0.2, 0.1), (0.2, 0.8)], 600),
            stroke(&[(0.1, 0.5), (0.9, 0.5)], 630),
        ];
        let natural = segment_strokes(&strokes, &bundle, &ids);
        assert_eq!((natural[0].stroke_start, natural[0].stroke_end), (0, 1));
        let mut cache = VisualCache::default();
        cache.update(&strokes, &bundle);
        let locked = segment_strokes_with_locks(
            &strokes,
            &bundle,
            &ids,
            [(3, 4), (2, 3), (1, 2), (0, usize::MAX), (3, 2)].into_iter(),
            &mut cache,
        );
        let bounds: Vec<_> = locked
            .iter()
            .map(|span| (span.stroke_start, span.stroke_end))
            .collect();
        assert_eq!(bounds, [(0, 0), (1, 2), (3, 3)]);
        assert_eq!(locked[1].strokes, strokes[1..=2]);
    }

    #[test]
    fn ids_checks_top_three_on_both_sides_of_a_compound() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let ids = IdsIndex::load_cached(Some(&assets));
        let prediction = |ch, prob: f32| VisualPrediction {
            ch,
            prob,
            raw_logit: prob.ln(),
        };
        let left = [
            prediction('小', 0.6),
            prediction('水', 0.2),
            prediction('川', 0.1),
        ];
        let right = [
            prediction('司', 0.5),
            prediction('丁', 0.3),
            prediction('可', 0.15),
        ];
        let merged = [prediction('何', 0.5), prediction('河', 0.3)];
        assert!(matches_ids_compound(&ids, &left, &right, &merged));
        assert!(!matches_ids_compound(&ids, &left[..1], &right, &merged));
        assert!(!matches_ids_compound(&ids, &left, &right[..2], &merged));
    }
}
