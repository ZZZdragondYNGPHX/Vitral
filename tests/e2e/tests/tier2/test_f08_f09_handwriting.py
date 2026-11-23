"""Tier 2 BVA and negative/corner-case tests for F08-F09 (Dual-Mode Online Handwriting)."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets

pytestmark = pytest.mark.tier2


def _hw_dir(discovered_targets: DiscoveredTargets) -> Path:
    assert discovered_targets.fixtures_dir is not None, "tests/fixtures directory missing"
    return discovered_targets.fixtures_dir / "handwriting"


def _load_hw_fixture(discovered_targets: DiscoveredTargets, filename: str) -> dict[str, Any]:
    return json.loads((_hw_dir(discovered_targets) / filename).read_text(encoding="utf-8"))


def _replay_strokes(target: VitralTarget, strokes: list[list[dict[str, Any]]]) -> dict[str, Any]:
    last: dict[str, Any] = {}
    for stroke in strokes:
        pts = [{"x": float(p["x"]), "y": float(p["y"]), "timestamp_ms": int(p["t"])} for p in stroke]
        last = target.hw_push_stroke(pts, pen_up=True)
    return last


# ======================================================================
# F08: Mode 1 Single-Character Phonetic Lookup (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f08_mode1_slow_reversed_no_auto_clear(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    fx = _load_hw_fixture(discovered_targets, "mode1_slow_reversed.json")
    assert fx["expected"]["total_duration_ms"] > 5000
    assert fx["expected"]["reversed_stroke_order"] is True
    assert fx["expected"]["auto_clear_forbidden"] is True

    vitral_target.init({"schema": "double_pinyin_flypy"})
    vitral_target.hw_begin("PhoneticLookup")
    _replay_strokes(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    candidates = res.get("candidates", res.get("results", []))
    assert len(candidates) >= 1, f"Expected Mode 1 candidates for slow reversed 耄, got {res}"
    top_chars = [c["char"] for c in candidates]
    assert "耄" in top_chars
    mao = next(c for c in candidates if c["char"] == "耄")
    assert mao["toned_pinyin"] == "mào"
    assert mao["xiaohe_code"] == "mc"


@pytest.mark.requires_core
def test_f08_mode1_rare_gb18030_toned_and_xiaohe_codes(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.init({"schema": "double_pinyin_flypy"})
    for fname in ("mode1_mao.json", "mode1_di.json"):
        fx = _load_hw_fixture(discovered_targets, fname)
        vitral_target.hw_begin("PhoneticLookup")
        _replay_strokes(vitral_target, fx["strokes"])
        res = vitral_target.hw_end()
        candidates = res.get("candidates", res.get("results", []))
        assert len(candidates) >= 1
        match = next((c for c in candidates if c["char"] == fx["expected"]["char"]), None)
        assert match is not None, f"Missing expected GB18030 char {fx['expected']['char']} in {candidates}"
        assert match["toned_pinyin"] == fx["expected"]["toned_pinyin"]
        assert match["xiaohe_code"] == fx["expected"]["xiaohe_code"]
        assert len(match["xiaohe_code"]) == 2
        vitral_target.hw_clear()


@pytest.mark.requires_core
def test_f08_mode1_degenerate_and_oob_stroke_coordinates(vitral_target: VitralTarget) -> None:
    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")

    for degenerate in (
        [],
        [{"x": 0.5, "y": 0.5, "timestamp_ms": 0}],
        [{"x": -0.5, "y": 2.5, "timestamp_ms": 10}, {"x": 1.8, "y": -0.2, "timestamp_ms": 30}],
    ):
        try:
            res = vitral_target.hw_push_stroke(degenerate, pen_up=True)
            assert isinstance(res, dict)
        except VitralCommandError:
            pass

    try:
        end_res = vitral_target.hw_end()
        assert isinstance(end_res, dict)
    except VitralCommandError:
        pass


@pytest.mark.requires_core
def test_f08_mode1_clear_isolation_between_characters(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    fx_mao = _load_hw_fixture(discovered_targets, "mode1_mao.json")
    fx_di = _load_hw_fixture(discovered_targets, "mode1_di.json")

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _replay_strokes(vitral_target, fx_mao["strokes"][:5])
    vitral_target.hw_clear()

    _replay_strokes(vitral_target, fx_di["strokes"])
    res = vitral_target.hw_end()
    candidates = res.get("candidates", res.get("results", []))
    assert len(candidates) >= 1
    assert candidates[0]["char"] == "頔"


@pytest.mark.requires_core
def test_f08_mode1_lifecycle_negative_without_begin(vitral_target: VitralTarget) -> None:
    vitral_target.init({})
    with pytest.raises(VitralCommandError):
        vitral_target.hw_push_stroke(
            [{"x": 0.1, "y": 0.1, "timestamp_ms": 0}, {"x": 0.5, "y": 0.5, "timestamp_ms": 20}],
            pen_up=True,
        )
    with pytest.raises(VitralCommandError):
        vitral_target.hw_end()


# ======================================================================
# F09: Mode 2 Continuous Handwriting, Top-8 Lattice, Entropy Gate (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f09_entropy_gate_margin_boundary_0149_vs_0151(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    ambig_fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")
    clear_fx = _load_hw_fixture(discovered_targets, "mode2_clear_locked.json")
    assert ambig_fx["design_tokens_ref"]["ambiguousMarginThreshold"] == 0.15

    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    slot0_end = ambig_fx["char_segments"][0]["stroke_range"][1] + 1
    _replay_strokes(vitral_target, ambig_fx["strokes"][:slot0_end])
    ambig_res = vitral_target.hw_end()
    ambig_slot0 = ambig_res["slots"][0]
    assert ambig_slot0["locked_by_entropy"] is False
    assert "margin_delta_p" in ambig_slot0, f"Mode2Slot missing 'margin_delta_p': {ambig_slot0}"
    assert float(ambig_slot0["margin_delta_p"]) < 0.15
    vitral_target.hw_clear()

    vitral_target.hw_begin("ContinuousLattice")
    clear_slot0_end = clear_fx["char_segments"][0]["stroke_range"][1] + 1
    _replay_strokes(vitral_target, clear_fx["strokes"][:clear_slot0_end])
    clear_res = vitral_target.hw_end()
    clear_slot0 = clear_res["slots"][0]
    assert clear_slot0["locked_by_entropy"] is True
    assert "margin_delta_p" in clear_slot0, f"Mode2Slot missing 'margin_delta_p': {clear_slot0}"
    assert float(clear_slot0["margin_delta_p"]) >= 0.15


@pytest.mark.requires_core
def test_f09_baoguang_retroactive_flip_and_top8_cap(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    assert discovered_targets.fixtures_dir is not None
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"
    fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")
    vitral_target.init({})
    vitral_target.import_lexicon(scel_path, "scel")
    vitral_target.hw_begin("ContinuousLattice")

    seg1_end = fx["char_segments"][1]["stroke_range"][1] + 1
    mid_res = _replay_strokes(vitral_target, fx["strokes"][:seg1_end])
    assert mid_res["composing_text"] == "曝光"
    assert 1 <= len(mid_res["slots"][0]["top8"]) <= 8

    _replay_strokes(vitral_target, fx["strokes"][seg1_end:])
    final_res = vitral_target.hw_end()
    assert final_res["composing_text"] == "爆光老登金币"
    assert final_res["slots"][0]["char"] == "爆"
    assert final_res["slots"][0]["changed_retroactively"] is True


@pytest.mark.requires_core
def test_f09_clear_locked_high_confidence_non_mutation(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    fx = _load_hw_fixture(discovered_targets, "mode2_clear_locked.json")
    assert fx["expected"]["final_text"] == "曝光老登金币"
    assert fx["expected"]["slot0_locked_by_entropy"] is True

    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()

    slot0 = res["slots"][0]
    assert slot0["locked_by_entropy"] is True
    assert "margin_delta_p" in slot0, f"Mode2Slot missing 'margin_delta_p': {slot0}"
    assert float(slot0["margin_delta_p"]) >= 0.15
    assert slot0["char"] == "曝"
    assert slot0["changed_retroactively"] is False
    assert res["composing_text"] == "曝光老登金币"


@pytest.mark.requires_core
def test_f09_click_to_override_and_post_override_lock(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    assert discovered_targets.fixtures_dir is not None
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"
    fx = _load_hw_fixture(discovered_targets, "mode2_baoguang_laodeng.json")
    vitral_target.init({})
    vitral_target.import_lexicon(scel_path, "scel")
    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()
    assert res["composing_text"] == "爆光老登金币"

    top8 = res["slots"][0]["top8"]
    override_idx = next(
        (idx for idx, item in enumerate(top8) if (item["char"] if isinstance(item, dict) else item) == "曝"),
        None,
    )
    assert override_idx is not None, f"Expected '曝' retained in slot 0 top8: {top8}"

    after_ovr = vitral_target.hw_override_slot(0, override_idx)
    assert after_ovr["composing_text"] == "曝光老登金币"
    slot0 = after_ovr["slots"][0]
    assert slot0["char"] == "曝"
    assert (
        slot0.get("manually_locked") is True
        or slot0.get("locked_by_entropy") is True
    ), f"Expected slot 0 locked after override, got {slot0}"
    assert slot0.get("changed_retroactively") is False


@pytest.mark.requires_core
def test_f09_horizontal_connected_and_invalid_override_oob(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    fx = _load_hw_fixture(discovered_targets, "mode2_horizontal_connected.json")
    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx["strokes"])
    res = vitral_target.hw_end()
    assert res["composing_text"] == "手写中文"

    with pytest.raises(VitralCommandError):
        vitral_target.hw_override_slot(999, 0)
    with pytest.raises(VitralCommandError):
        vitral_target.hw_override_slot(0, 99)


@pytest.mark.requires_core
def test_f09_pause_between_components_of_ni(vitral_target: VitralTarget) -> None:
    strokes = [
        [(0.33, 0.13), (0.25, 0.35), (0.12, 0.51)],
        [(0.25, 0.36), (0.25, 0.86)],
        [(0.54, 0.13), (0.46, 0.34), (0.37, 0.45)],
        [(0.49, 0.31), (0.84, 0.31), (0.75, 0.45)],
        [(0.59, 0.36), (0.59, 0.80), (0.57, 0.86), (0.50, 0.80)],
        [(0.47, 0.53), (0.41, 0.65), (0.35, 0.74)],
        [(0.72, 0.53), (0.78, 0.63), (0.84, 0.74)],
    ]
    vitral_target.init({})
    vitral_target.hw_begin("ContinuousLattice")
    timestamp = 0
    for i, stroke in enumerate(strokes):
        if i == 2:
            timestamp += 800
        points = []
        for x, y in stroke:
            points.append({"x": x, "y": y, "timestamp_ms": timestamp})
            timestamp += 25
        vitral_target.hw_push_stroke(points, pen_up=True)
        timestamp += 45
    result = vitral_target.hw_end()
    assert [slot["char"] for slot in result["slots"]] == ["你"]
