pub mod ids_exta;
pub mod lattice;
pub mod onnx_models;
pub mod pure_fallback;
pub mod raster;
pub mod rerank;

use crate::error::VitralError;
use crate::imp::ImportedLexiconIndex;
use crate::ret::RetentionEngine;
use ids_exta::{compose_rare_candidates, find_bipartite_splits, IdsIndex};
use lattice::{build_confusion_lattice, segment_strokes};
use onnx_models::HwOnnxBundle;
use pure_fallback::{classify_1d_primitive, classify_single_stroke_primitive};
use raster::{render_directmap, GlyphGeometry, StrokeStats, DIRECTMAP_LEN};
use rerank::decode_lattice_3tier;
pub use rerank::{HwDecodeContext, Tier3ElasticSlot, Tier3SlotDisambiguator, Tier3SlotRequest};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use vitral_tokens::TOKENS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HwMode {
    PhoneticLookup,
    ContinuousLattice,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrokePoint {
    pub x: f32,
    pub y: f32,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode1Candidate {
    #[serde(rename = "char")]
    pub ch: String,
    pub toned_pinyin: String,
    pub xiaohe_code: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode1Result {
    pub mode: String,
    pub stroke_count: usize,
    pub candidates: Vec<Mode1Candidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Top8Candidate {
    #[serde(rename = "char")]
    pub ch: String,
    pub visual_prob: f32,
    pub joint_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode2Slot {
    pub slot_idx: usize,
    #[serde(rename = "char")]
    pub ch: String,
    pub locked_by_entropy: bool,
    pub manually_locked: bool,
    pub changed_retroactively: bool,
    pub margin_delta_p: f32,
    pub top8: Vec<Top8Candidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode2Result {
    pub mode: String,
    pub composing_text: String,
    pub slots: Vec<Mode2Slot>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HandwritingPayload {
    Mode1(Mode1Result),
    Mode2(Mode2Result),
}

#[derive(Debug, Default)]
pub struct HandwritingSession {
    pub active_mode: Option<HwMode>,
    pub strokes: Vec<Vec<StrokePoint>>,
    pub current_stroke: Vec<StrokePoint>,
    pub last_slots: Vec<Mode2Slot>,
    pub last_mode1: Vec<Mode1Candidate>,
    pub assets_dir: Option<PathBuf>,
    model_paths: Option<crate::models::ModelPaths>,
    slot_span_bounds: Vec<(usize, usize)>,
    manual_locked_by_span: HashMap<(usize, usize), String>,
    baseline_char_by_span: HashMap<(usize, usize), String>,
    strokes_dirty: bool,
    prev_span_count: usize,
    pub(crate) confirmed_since_edit: bool,
    pub(crate) revision: u64,
}

impl HandwritingSession {
    pub fn with_assets_dir(assets_dir: Option<&Path>) -> Self {
        Self {
            assets_dir: assets_dir.map(PathBuf::from),
            ..Self::default()
        }
    }

    pub fn set_assets_dir(&mut self, assets_dir: Option<&Path>) {
        self.assets_dir = assets_dir.map(PathBuf::from);
        self.model_paths = None;
    }

    pub fn set_model_paths(&mut self, paths: crate::models::ModelPaths) {
        self.model_paths = Some(paths);
        self.clear();
    }

    pub fn begin(&mut self, mode: HwMode) {
        self.revision = self.revision.wrapping_add(1);
        self.active_mode = Some(mode);
        self.strokes.clear();
        self.current_stroke.clear();
        self.last_slots.clear();
        self.last_mode1.clear();
        self.slot_span_bounds.clear();
        self.manual_locked_by_span.clear();
        self.baseline_char_by_span.clear();
        self.strokes_dirty = false;
        self.prev_span_count = 0;
        self.confirmed_since_edit = false;
    }

    pub fn clear(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.strokes.clear();
        self.current_stroke.clear();
        self.last_slots.clear();
        self.last_mode1.clear();
        self.slot_span_bounds.clear();
        self.manual_locked_by_span.clear();
        self.baseline_char_by_span.clear();
        self.strokes_dirty = false;
        self.prev_span_count = 0;
        self.confirmed_since_edit = false;
    }

    pub fn push_stroke(&mut self, points: &[StrokePoint], pen_up: bool) -> Result<(), VitralError> {
        if self.active_mode.is_none() {
            return Err(VitralError::NotInitialized);
        }
        if !points.is_empty() {
        self.revision = self.revision.wrapping_add(1);
            self.current_stroke.extend_from_slice(points);
            self.strokes_dirty = true;
            self.confirmed_since_edit = false;
        }
        if pen_up && !self.current_stroke.is_empty() {
            self.strokes.push(std::mem::take(&mut self.current_stroke));
            self.strokes_dirty = true;
            self.confirmed_since_edit = false;
        }
        Ok(())
    }

    pub fn push_stroke_with_context(
        &mut self,
        points: &[StrokePoint],
        pen_up: bool,
        ctx: &HwDecodeContext<'_>,
    ) -> Result<HandwritingPayload, VitralError> {
        self.push_stroke(points, pen_up)?;
        if pen_up && self.strokes_dirty {
            self.decode_current(ctx)
        } else {
            self.snapshot()
        }
    }

    pub fn end_with_context(
        &mut self,
        ctx: &HwDecodeContext<'_>,
    ) -> Result<HandwritingPayload, VitralError> {
        if self.active_mode.is_none() {
            return Err(VitralError::NotInitialized);
        }
        if !self.current_stroke.is_empty() {
            self.strokes.push(std::mem::take(&mut self.current_stroke));
            self.strokes_dirty = true;
            self.confirmed_since_edit = false;
        }
        if !self.strokes_dirty && (!self.last_slots.is_empty() || !self.last_mode1.is_empty()) {
            return self.snapshot();
        }
        self.decode_current(ctx)
    }

    pub fn snapshot(&self) -> Result<HandwritingPayload, VitralError> {
        let mode = self.active_mode.ok_or(VitralError::NotInitialized)?;
        match mode {
            HwMode::PhoneticLookup => Ok(HandwritingPayload::Mode1(Mode1Result {
                mode: TOKENS.handwriting.modes[0].to_string(),
                stroke_count: self.strokes.len() + usize::from(!self.current_stroke.is_empty()),
                candidates: self.last_mode1.clone(),
            })),
            HwMode::ContinuousLattice => Ok(HandwritingPayload::Mode2(Mode2Result {
                mode: TOKENS.handwriting.modes[1].to_string(),
                composing_text: self.last_slots.iter().map(|s| s.ch.as_str()).collect(),
                slots: self.last_slots.clone(),
            })),
        }
    }

    pub fn override_slot(
        &mut self,
        slot_idx: usize,
        candidate_idx: usize,
    ) -> Result<Mode2Result, VitralError> {
        if self.active_mode != Some(HwMode::ContinuousLattice) {
            return Err(VitralError::InvalidArgument(
                "override_slot requires continuous-contextual-lattice mode".into(),
            ));
        }
        let slot = self
            .last_slots
            .get_mut(slot_idx)
            .ok_or_else(|| VitralError::NotFound(format!("slot {slot_idx} not found")))?;
        let picked = slot
            .top8
            .get(candidate_idx)
            .ok_or_else(|| VitralError::NotFound(format!("candidate {candidate_idx} not found")))?
            .clone();
        slot.ch = picked.ch.clone();
        self.revision = self.revision.wrapping_add(1);
        slot.manually_locked = true;
        slot.changed_retroactively = false;
        self.confirmed_since_edit = false;
        if let Some(&span) = self.slot_span_bounds.get(slot_idx) {
            self.manual_locked_by_span.insert(span, picked.ch);
        }
        Ok(Mode2Result {
            mode: TOKENS.handwriting.modes[1].to_string(),
            composing_text: self.last_slots.iter().map(|s| s.ch.as_str()).collect(),
            slots: self.last_slots.clone(),
        })
    }

    /// Record the confirmed handwriting output into shared `RetentionEngine` Frecency
    /// (idempotent per edit so repeated calls do not double-boost Frecency).
    pub fn confirm_result(&mut self, retention: &mut RetentionEngine) -> Option<String> {
        let mode = self.active_mode?;
        let committed = match mode {
            HwMode::PhoneticLookup => self.last_mode1.first().map(|c| c.ch.clone()),
            HwMode::ContinuousLattice => {
                if self.last_slots.is_empty() {
                    None
                } else {
                    Some(
                        self.last_slots
                            .iter()
                            .map(|s| s.ch.as_str())
                            .collect::<String>(),
                    )
                }
            }
        }?;
        if committed.is_empty() {
            return None;
        }
        if !self.confirmed_since_edit {
            retention.record_selection(&committed, 0.0, 0.001, 1.0);
            if mode == HwMode::ContinuousLattice {
                for slot in &self.last_slots {
                    if !slot.ch.is_empty() {
                        retention.record_selection(&slot.ch, 0.0, 0.001, 0.25);
                    }
                }
            }
            self.confirmed_since_edit = true;
        }
        Some(committed)
    }

    fn decode_current(&mut self, ctx: &HwDecodeContext<'_>) -> Result<HandwritingPayload, VitralError> {
        let mode = self.active_mode.ok_or(VitralError::NotInitialized)?;
        let assets = self
            .assets_dir
            .clone()
            .or_else(crate::imp::polyphone::default_assets_dir_candidates);
        let paths = self.model_paths.clone().unwrap_or_else(|| crate::models::ModelPaths::bundled(assets.as_deref()));
        let bundle = HwOnnxBundle::load_paths(&paths);
        let ids = IdsIndex::load_dir(paths.handwriting.as_deref());

        match mode {
            HwMode::PhoneticLookup => {
                let cands = decode_mode1(&self.strokes, &bundle, &ids, ctx.imported)?;
                self.last_mode1 = cands.clone();
                self.strokes_dirty = false;
                Ok(HandwritingPayload::Mode1(Mode1Result {
                    mode: TOKENS.handwriting.modes[0].to_string(),
                    stroke_count: self.strokes.len()
                        + usize::from(!self.current_stroke.is_empty()),
                    candidates: cands,
                }))
            }
            HwMode::ContinuousLattice => {
                if self.strokes.is_empty() {
                    self.prev_span_count = 0;
                    self.slot_span_bounds.clear();
                    self.last_slots.clear();
                    self.strokes_dirty = false;
                    return Ok(HandwritingPayload::Mode2(Mode2Result {
                        mode: TOKENS.handwriting.modes[1].to_string(),
                        composing_text: String::new(),
                        slots: Vec::new(),
                    }));
                }

                let spans = segment_strokes(&self.strokes, &bundle, &ids);
                let lattice = build_confusion_lattice(&spans, &bundle, &ids)?;

                let span_bounds: Vec<(usize, usize)> = lattice
                    .iter()
                    .map(|s| (s.stroke_start, s.stroke_end))
                    .collect();

                // Gather initial baseline chars per span (populated the first time a span stabilizes)
                let baseline_vec: Vec<String> = span_bounds
                    .iter()
                    .enumerate()
                    .map(|(i, span)| {
                        self.baseline_char_by_span
                            .get(span)
                            .cloned()
                            .unwrap_or_else(|| {
                                lattice[i]
                                    .top8
                                    .first()
                                    .map(|c| c.ch.clone())
                                    .unwrap_or_default()
                            })
                    })
                    .collect();

                let mut slots = decode_lattice_3tier(
                    &lattice,
                    &self.manual_locked_by_span,
                    &baseline_vec,
                    &bundle,
                    ctx,
                );

                // When slot 0 first enters multi-char context (`prev_span_count < 2 && span_bounds.len() == 2`),
                // record its contextual baseline so later trailing flips properly flag `changed_retroactively`.
                if self.prev_span_count < 2 && span_bounds.len() == 2 {
                    for (i, span) in span_bounds.iter().enumerate() {
                        self.baseline_char_by_span.insert(*span, slots[i].ch.clone());
                        slots[i].changed_retroactively = false;
                    }
                } else {
                    for (i, span) in span_bounds.iter().enumerate() {
                        self.baseline_char_by_span
                            .entry(*span)
                            .or_insert_with(|| slots[i].ch.clone());
                    }
                }

                self.prev_span_count = span_bounds.len();
                self.slot_span_bounds = span_bounds;
                self.last_slots = slots.clone();
                self.strokes_dirty = false;
                Ok(HandwritingPayload::Mode2(Mode2Result {
                    mode: TOKENS.handwriting.modes[1].to_string(),
                    composing_text: slots.iter().map(|s| s.ch.as_str()).collect(),
                    slots,
                }))
            }
        }
    }
}

/// Produce deduplicated stroke permutations `[raw, top_left_reordered, reversed]`
/// for stroke-order-invariant classification without redundant ONNX batching.
pub fn unique_stroke_orders(strokes: &[Vec<StrokePoint>]) -> Vec<Vec<Vec<StrokePoint>>> {
    let non_empty: Vec<&Vec<StrokePoint>> = strokes.iter().filter(|s| !s.is_empty()).collect();
    let n = non_empty.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![vec![non_empty[0].clone()]];
    }

    let raw_perm: Vec<usize> = (0..n).collect();

    let mut keyed: Vec<(f32, usize)> = non_empty
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let key = match StrokeStats::from_stroke(s) {
                Some(st) => st.cy * 0.7 + st.cx * 0.3,
                None => idx as f32,
            };
            (key, idx)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let tl_perm: Vec<usize> = keyed.into_iter().map(|(_, idx)| idx).collect();

    let rev_perm: Vec<usize> = (0..n).rev().collect();

    let mut perms: Vec<Vec<usize>> = Vec::with_capacity(3);
    for p in [raw_perm, tl_perm, rev_perm] {
        if !perms.iter().any(|existing| existing == &p) {
            perms.push(p);
        }
    }

    perms
        .into_iter()
        .map(|perm| perm.into_iter().map(|i| non_empty[i].clone()).collect())
        .collect()
}

fn classify_part_variants(
    strokes: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
) -> Result<Vec<(char, f32)>, VitralError> {
    let geom = GlyphGeometry::from_strokes(strokes);
    if let Some(prim) = classify_1d_primitive(&geom) {
        return Ok(prim.into_iter().map(|p| (p.ch, p.prob)).collect());
    }
    let orders = unique_stroke_orders(strokes);
    if orders.is_empty() {
        return Ok(Vec::new());
    }
    let mut flat = Vec::with_capacity(orders.len() * DIRECTMAP_LEN);
    for ord in &orders {
        flat.extend_from_slice(&render_directmap(ord));
    }
    let batch = bundle
        .classify_directmap_batch(&flat, orders.len(), 8)
        .ok_or_else(|| VitralError::ModelUnavailable("hw_model ONNX inference failed".into()))?;
    let mut best: HashMap<char, f32> = HashMap::new();
    for preds in batch {
        for p in preds {
            let ent = best.entry(p.ch).or_insert(0.0);
            if p.prob > *ent {
                *ent = p.prob;
            }
        }
    }
    let mut out: Vec<(char, f32)> = best.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.truncate(8);
    Ok(out)
}

fn decode_mode1(
    strokes: &[Vec<StrokePoint>],
    bundle: &HwOnnxBundle,
    ids: &IdsIndex,
    imported: &ImportedLexiconIndex,
) -> Result<Vec<Mode1Candidate>, VitralError> {
    if strokes.is_empty() {
        return Ok(Vec::new());
    }
    let geom = GlyphGeometry::from_strokes(strokes);
    let top_k = 10usize;

    let mut direct_scores: HashMap<char, f32> = HashMap::new();
    if bundle.has_hw_onnx() {
        if let Some(prim) = classify_1d_primitive(&geom) {
            for p in prim {
                direct_scores.insert(p.ch, p.prob);
            }
        } else {
            // Direct Mode 1: deduplicated triple-order [raw, top_left_reordered, reversed]
            let orders = unique_stroke_orders(strokes);
            let mut flat = Vec::with_capacity(orders.len() * DIRECTMAP_LEN);
            for ord in &orders {
                flat.extend_from_slice(&render_directmap(ord));
            }
            let batch = bundle
                .classify_directmap_batch(&flat, orders.len(), top_k)
                .ok_or_else(|| {
                    VitralError::ModelUnavailable("hw_model ONNX inference failed".into())
                })?;
            for preds in batch {
                for p in preds {
                    let ent = direct_scores.entry(p.ch).or_insert(0.0);
                    if p.prob > *ent {
                        *ent = p.prob;
                    }
                }
            }
        }
    } else if let Some(prim) = classify_single_stroke_primitive(&geom) {
        for p in prim {
            direct_scores.insert(p.ch, p.prob);
        }
    } else {
        return Err(VitralError::ModelUnavailable(
            "尚未下载或已禁用离线手写识别模型，请在模型中心下载并启用".into(),
        ));
    }

    let max_direct = direct_scores.values().copied().fold(0.0f32, f32::max);

    // Compositional Ext-A & Rare-BMP recognition via cjkvi_ids.txt + radical_pinyin.dict.yaml
    // Runs whenever direct CNN confidence is low/uncertain (< 0.70) and strokes >= 4.
    if bundle.has_hw_onnx() && strokes.len() >= 4 && max_direct < 0.70 && !ids.is_empty() {
        let splits = find_bipartite_splits(strokes);
        let class_set = (!bundle.hw_class_set.is_empty()).then_some(&bundle.hw_class_set);

        for sp in splits.iter().take(2) {
            // Batch both halves when neither is 1D; classify_part_variants handles 1D & triple-order.
            let geom_a = GlyphGeometry::from_strokes(&sp.left_or_top);
            let geom_b = GlyphGeometry::from_strokes(&sp.right_or_bottom);
            let (pa, pb) = match (classify_1d_primitive(&geom_a), classify_1d_primitive(&geom_b)) {
                (Some(prim_a), Some(prim_b)) => (
                    prim_a.into_iter().map(|p| (p.ch, p.prob)).collect(),
                    prim_b.into_iter().map(|p| (p.ch, p.prob)).collect(),
                ),
                (None, None) => {
                    let orders_a = unique_stroke_orders(&sp.left_or_top);
                    let orders_b = unique_stroke_orders(&sp.right_or_bottom);
                    let na = orders_a.len();
                    let nb = orders_b.len();
                    let mut flat = Vec::with_capacity((na + nb) * DIRECTMAP_LEN);
                    for ord in &orders_a {
                        flat.extend_from_slice(&render_directmap(ord));
                    }
                    for ord in &orders_b {
                        flat.extend_from_slice(&render_directmap(ord));
                    }
                    let batch = bundle
                        .classify_directmap_batch(&flat, na + nb, 8)
                        .ok_or_else(|| {
                            VitralError::ModelUnavailable("hw_model ONNX inference failed".into())
                        })?;
                    let mut best_a: HashMap<char, f32> = HashMap::new();
                    for preds in batch.iter().take(na) {
                        for p in preds {
                            let ent = best_a.entry(p.ch).or_insert(0.0);
                            if p.prob > *ent {
                                *ent = p.prob;
                            }
                        }
                    }
                    let mut best_b: HashMap<char, f32> = HashMap::new();
                    for preds in batch.iter().skip(na).take(nb) {
                        for p in preds {
                            let ent = best_b.entry(p.ch).or_insert(0.0);
                            if p.prob > *ent {
                                *ent = p.prob;
                            }
                        }
                    }
                    let mut va: Vec<(char, f32)> = best_a.into_iter().collect();
                    va.sort_by(|x, y| y.1.total_cmp(&x.1));
                    va.truncate(8);
                    let mut vb: Vec<(char, f32)> = best_b.into_iter().collect();
                    vb.sort_by(|x, y| y.1.total_cmp(&x.1));
                    vb.truncate(8);
                    (va, vb)
                }
                _ => (
                    classify_part_variants(&sp.left_or_top, bundle)?,
                    classify_part_variants(&sp.right_or_bottom, bundle)?,
                ),
            };

            let composed = compose_rare_candidates(
                ids,
                imported,
                class_set,
                sp,
                &pa,
                &pb,
                geom.stroke_count,
            );
            let mut best_comp = 0.0f32;
            for (ch, comp_score) in composed.into_iter().take(5) {
                best_comp = best_comp.max(comp_score);
                let scaled = if max_direct < 0.35 {
                    (comp_score * 1.15).clamp(0.05, 0.99)
                } else {
                    (comp_score * 0.65).clamp(0.01, 0.85)
                };
                let ent = direct_scores.entry(ch).or_insert(0.0);
                if scaled > *ent {
                    *ent = scaled;
                }
            }
            if best_comp >= 0.70 {
                break;
            }
        }
    }

    let mut ranked: Vec<(char, f32)> = direct_scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(8);

    // Annotate each candidate with `(toned_pinyin, xiaohe_code)` via ImportedLexiconIndex::char_phonetics
    Ok(ranked
        .into_iter()
        .map(|(ch, conf)| {
            let phon = imported.char_phonetics(ch);
            let (toned_pinyin, xiaohe_code) = phon
                .into_iter()
                .next()
                .unwrap_or_else(|| (String::new(), String::new()));
            Mode1Candidate {
                ch: ch.to_string(),
                toned_pinyin,
                xiaohe_code,
                confidence: conf.clamp(0.0, 1.0),
            }
        })
        .collect())
}
