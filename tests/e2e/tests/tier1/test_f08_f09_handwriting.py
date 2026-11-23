"""Tier 1 Dual-Mode Handwriting Engine tests for Vitral (F08-F09)."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets


def _load_hw_fixture(discovered_targets: DiscoveredTargets, name: str) -> dict[str, Any]:
    assert discovered_targets.fixtures_dir is not None
    path = discovered_targets.fixtures_dir / "handwriting" / name
    return json.loads(path.read_text(encoding="utf-8"))


def _push_stroke_list(target: VitralTarget, strokes: list[list[dict[str, Any]]]) -> dict[str, Any]:
    last: dict[str, Any] = {}
    for stroke in strokes:
        pts = [
            {
                "x": float(p["x"]),
                "y": float(p["y"]),
                "timestamp_ms": int(p.get("timestamp_ms", p.get("t", 0))),
            }
            for p in stroke
        ]
        last = target.hw_push_stroke(pts, pen_up=True)
    return last


def _extract_mode1_candidates(res: dict[str, Any]) -> list[dict[str, Any]]:
    cands = res.get("candidates", res.get("items", []))
    assert isinstance(cands, list)
    return cands


# ---------------------------------------------------------------------------
# F08: Mode 1 Single-Character Phonetic Lookup (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f08_mode1_lookup_mao_returns_toned_pinyin_and_xiaohe(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode1_mao.json")

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    cands = _extract_mode1_candidates(res)
    mao = next((c for c in cands if c.get("char") == "耄"), None)
    assert mao is not None, f"Expected '耄' in Mode 1 candidates, got {cands}"
    assert mao["toned_pinyin"] == "mào"
    assert mao["xiaohe_code"] == "mc"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f08_mode1_lookup_di_rare_char_gb18030_annotation(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode1_di.json")

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    cands = _extract_mode1_candidates(res)
    di_item = next((c for c in cands if c.get("char") == "頔"), None)
    assert di_item is not None, f"Expected GB18030 char '頔' in Mode 1 candidates, got {cands}"
    assert di_item["toned_pinyin"] == "dí"
    assert di_item["xiaohe_code"] == "di"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f08_mode1_slow_and_reversed_stroke_order_no_autoclear(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode1_slow_reversed.json")
    assert fx["expected"]["auto_clear_forbidden"] is True
    assert fx["strokes"][-1][-1]["t"] > 5000

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    cands = _extract_mode1_candidates(res)
    mao = next((c for c in cands if c.get("char") == "耄"), None)
    assert mao is not None, f"Slow/reversed strokes should not auto-clear '耄', got {cands}"
    assert mao["toned_pinyin"] == "mào"
    assert mao["xiaohe_code"] == "mc"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f08_mode1_hw_clear_resets_ink_buffer_cleanly(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    mao_fx = _load_hw_fixture(discovered_targets, "mode1_mao.json")
    di_fx = _load_hw_fixture(discovered_targets, "mode1_di.json")

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _push_stroke_list(vitral_target, mao_fx["strokes"][:5])
    vitral_target.hw_clear()

    _push_stroke_list(vitral_target, di_fx["strokes"])
    res = vitral_target.hw_end()
    cands = _extract_mode1_candidates(res)
    chars = [c.get("char") for c in cands]
    assert "頔" in chars
    assert chars[0] != "耄"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f08_mode1_candidate_payload_schema_and_confidence_ordering(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode1_mao.json")

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    cands = _extract_mode1_candidates(res)
    assert len(cands) >= 1
    confs: list[float] = []
    for item in cands:
        assert isinstance(item.get("char"), str) and len(item["char"]) >= 1
        assert isinstance(item.get("toned_pinyin"), str) and item["toned_pinyin"]
        assert isinstance(item.get("xiaohe_code"), str) and item["xiaohe_code"]
        conf = float(item["confidence"])
        assert 0.0 <= conf <= 1.0
        confs.append(conf)
    assert confs == sorted(confs, reverse=True)


@pytest.mark.tier1
@pytest.mark.tokens
def test_f08_mode1_tokens_show_xiaohe_code_enabled(design_tokens: dict[str, Any]) -> None:
    hw = design_tokens["handwriting"]
    assert hw["modes"][0] == "phonetic-lookup-single"
    assert hw["showXiaoheCodeInLookupMode"] is True


# ---------------------------------------------------------------------------
# F09: Mode 2 Continuous Handwriting, Lattice, Entropy Gate & Backtracking (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
@pytest.mark.tokens
def test_f09_mode2_top8_confusion_lattice_retained(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")
    s0_start, s0_end = fx["char_segments"][0]["stroke_range"]
    seg0_strokes = fx["strokes"][s0_start : s0_end + 1]

    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    _push_stroke_list(vitral_target, seg0_strokes)
    res = vitral_target.hw_end()

    slots = res["slots"]
    assert len(slots) >= 1
    top8 = slots[0]["top8"]
    top_chars = [c["char"] if isinstance(c, dict) else str(c) for c in top8]
    assert 1 <= len(top8) <= design_tokens["handwriting"]["confusionTopK"] == 8
    assert "曝" in top_chars and "爆" in top_chars


@pytest.mark.tier1
@pytest.mark.requires_core
@pytest.mark.tokens
def test_f09_mode2_visual_entropy_gate_marks_ambiguous_slot_elastic(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_core()
    assert design_tokens["handwriting"]["ambiguousMarginThreshold"] == 0.15
    fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")
    s0_start, s0_end = fx["char_segments"][0]["stroke_range"]
    seg0_strokes = fx["strokes"][s0_start : s0_end + 1]

    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    _push_stroke_list(vitral_target, seg0_strokes)
    res = vitral_target.hw_end()

    assert res["slots"][0]["locked_by_entropy"] is False


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f09_mode2_visual_entropy_gate_locks_clear_character_non_mutation(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode2_clear_locked.json")

    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    assert res["composing_text"] == "曝光老登金币"
    slot0 = res["slots"][0]
    assert slot0["char"] == "曝"
    assert slot0["locked_by_entropy"] is True
    assert slot0["changed_retroactively"] is False


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f09_mode2_baoguang_laodeng_retroactive_correction_benchmark(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"
    fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")

    vitral_target.init({})
    vitral_target.import_lexicon(scel_path, "scel")
    vitral_target.hw_begin("ContinuousLattice")

    segs = fx["char_segments"]
    # Phase 1: slots 0..1 ("曝光")
    s1_end = segs[1]["stroke_range"][1] + 1
    phase1 = _push_stroke_list(vitral_target, fx["strokes"][:s1_end])
    assert "composing_text" in phase1 and phase1["composing_text"].startswith("曝"), (
        f"Expected phase1 composing_text starting with '曝', got {phase1}"
    )

    # Phase 2: slots 2..5 ("老登金币") -> retroactive flip to "爆光老登金币"
    _push_stroke_list(vitral_target, fx["strokes"][s1_end:])
    final_res = vitral_target.hw_end()

    assert final_res["composing_text"] == "爆光老登金币"
    assert final_res["slots"][0]["char"] == "爆"
    assert final_res["slots"][0]["changed_retroactively"] is True


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f09_mode2_horizontal_connected_writing_segmentation(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    fx = _load_hw_fixture(discovered_targets, "mode2_horizontal_connected.json")

    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    assert res["composing_text"] == "手写中文"
    assert len(res["slots"]) == 4


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f09_mode2_click_to_override_locks_selected_candidate(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"
    fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")

    vitral_target.init({})
    vitral_target.import_lexicon(scel_path, "scel")
    vitral_target.hw_begin("ContinuousLattice")
    _push_stroke_list(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    top8 = res["slots"][0]["top8"]
    top_chars = [c["char"] if isinstance(c, dict) else str(c) for c in top8]
    assert "曝" in top_chars
    idx_pu = top_chars.index("曝")

    overridden = vitral_target.hw_override_slot(0, idx_pu)
    assert overridden["composing_text"] == "曝光老登金币"
    slot0 = overridden["slots"][0]
    assert slot0["char"] == "曝"
    assert (
        slot0.get("manually_locked") is True
        or slot0.get("locked_by_entropy") is True
        or slot0.get("locked") is True
    ), f"Expected slot 0 to be locked after override, got {slot0}"
    assert slot0.get("changed_retroactively") is False
