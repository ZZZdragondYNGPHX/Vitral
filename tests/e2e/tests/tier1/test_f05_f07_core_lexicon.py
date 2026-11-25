"""Tier 1 Core Input & Lexicon Compiler tests for Vitral (F05-F07)."""

from __future__ import annotations

import json
import unicodedata
from pathlib import Path
from typing import Any

import pytest

from harnesses import WebDAVMockServer
from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets


def _type_keys(target: VitralTarget, text: str) -> dict[str, Any]:
    last: dict[str, Any] = {}
    for ch in text:
        last = target.process_key(ch)
    return last


def _candidate_texts(action: dict[str, Any]) -> list[str]:
    out: list[str] = []
    for item in action.get("candidates", []):
        if isinstance(item, str):
            out.append(item)
        elif isinstance(item, dict):
            assert "text" in item, f"CandidateItem missing canonical 'text' field: {item}"
            out.append(str(item["text"]))
    return out


def _strip_pinyin_tones(syllable: str) -> str:
    decomposed = unicodedata.normalize("NFKD", syllable)
    return "".join(ch for ch in decomposed if not unicodedata.combining(ch)).lower()


# ---------------------------------------------------------------------------
# F05: vitral-core Input Engine (librime + rime-ice + Wanxiang + OpenCC) (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f05_xiaohe_flypy_schema_default_and_commit(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("double_pinyin_flypy")

    action = _type_keys(vitral_target, "nihc")
    cands = _candidate_texts(action)
    assert 1 <= len(cands) <= 5
    assert cands[0] == "你好", f"Expected '你好' at rank 1 for Xiaohe 'nihc', got {cands}"

    committed = vitral_target.select_candidate(0)
    assert committed.get("commit_text") == "你好"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f05_quanpin_rime_ice_schema_commit(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    action = _type_keys(vitral_target, "nihao")
    preedit = action.get("preedit")
    if isinstance(preedit, dict):
        assert preedit.get("text"), f"Expected non-empty preedit text, got {preedit}"
    else:
        assert preedit
    cands = _candidate_texts(action)
    assert "你好" in cands[:5]

    idx = cands.index("你好")
    committed = vitral_target.select_candidate(idx)
    assert committed.get("commit_text") == "你好"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f05_one_click_schema_toggle_flypy_and_quanpin(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})

    vitral_target.set_schema("double_pinyin_flypy")
    flypy_action = _type_keys(vitral_target, "nihc")
    flypy_cands = _candidate_texts(flypy_action)
    assert 1 <= len(flypy_cands) <= 5
    assert "你好" in flypy_cands
    vitral_target.select_candidate(0)

    vitral_target.set_schema("rime_ice")
    quanpin_action = _type_keys(vitral_target, "nihao")
    quanpin_cands = _candidate_texts(quanpin_action)
    assert 1 <= len(quanpin_cands) <= 5
    assert "你好" in quanpin_cands
    vitral_target.select_candidate(0)

    vitral_target.set_schema("double_pinyin_flypy")
    flypy_again = _type_keys(vitral_target, "nihc")
    flypy_again_cands = _candidate_texts(flypy_again)
    assert 1 <= len(flypy_again_cands) <= 5
    assert "你好" in flypy_again_cands


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f05_opencc_simplified_to_traditional_conversion(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    vitral_target.set_opencc_mode("traditional")
    tc_action = _type_keys(vitral_target, "kaifashuru")
    tc_cands = _candidate_texts(tc_action)
    assert any(ch in "".join(tc_cands) for ch in ("開發", "輸入")), (
        f"Expected Traditional Chinese output, got {tc_cands}"
    )
    vitral_target.select_candidate(0)

    vitral_target.set_opencc_mode("simplified")
    sc_action = _type_keys(vitral_target, "kaifashuru")
    sc_cands = _candidate_texts(sc_action)
    assert any("开发" in c or "输入" in c for c in sc_cands), (
        f"Expected Simplified Chinese output after reset, got {sc_cands}"
    )


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f05_wanxiang_grammar_prioritizes_natural_phrase(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    action = _type_keys(vitral_target, "jintiantianqizhenhao")
    cands = _candidate_texts(action)
    assert cands, "Expected non-empty candidates for fluent Quanpin sentence"
    assert cands[0] == "今天天气真好", f"Expected grammar rank-1 '今天天气真好', got {cands}"


@pytest.mark.tier1
@pytest.mark.requires_core
@pytest.mark.tokens
def test_f05_candidate_page_size_matches_token_5(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    page_size = design_tokens["ergonomics"]["desktopCandidatePageSize"]
    assert page_size == 5

    action = _type_keys(vitral_target, "zhong")
    cands = action.get("candidates", [])
    assert 1 <= len(cands) <= page_size


# ---------------------------------------------------------------------------
# F06: Long-Context Quanpin Research & Decoder (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
def test_f06_quanpin_research_notes_present_with_algorithm(
    discovered_targets: DiscoveredTargets,
) -> None:
    research_dir = discovered_targets.vitral_root / "docs" / "research"
    assert research_dir.is_dir(), "docs/research directory missing"
    md_files = sorted(research_dir.glob("*.md"))
    assert md_files, "No research markdown files found under docs/research/"

    quanpin_docs = []
    for p in md_files:
        content = p.read_text(encoding="utf-8")
        lowered = content.lower()
        if ("quanpin" in lowered or "全拼" in content) and any(
            kw in lowered for kw in ("viterbi", "beam", "syllable", "segmentation")
        ):
            quanpin_docs.append((p, content))

    assert quanpin_docs, (
        "Expected a dedicated Quanpin long-context decoding research document in docs/research/ "
        "covering Viterbi/beam/lattice segmentation and Wanxiang grammar"
    )
    assert len(quanpin_docs[0][1].splitlines()) >= 30


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f06_quanpin_zero_initial_segmentation_short_cases(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    corpus = json.loads(
        (discovered_targets.fixtures_dir / "quanpin" / "corpus.json").read_text(encoding="utf-8")
    )
    short_targets = {c["id"]: c for c in corpus["cases"] if c["id"] in {"qp_short_01", "qp_short_02", "qp_short_03"}}
    assert len(short_targets) == 3

    vitral_target.init({})
    vitral_target.set_schema("rime_ice")
    for cid in ("qp_short_01", "qp_short_02", "qp_short_03"):
        case = short_targets[cid]
        action = _type_keys(vitral_target, case["pinyin"])
        cands = _candidate_texts(action)
        assert cands and cands[0] == case["expected_text"], (
            f"{cid} ({case['pinyin']}): expected top-1 {case['expected_text']!r}, got {cands}"
        )
        vitral_target.select_candidate(0)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f06_quanpin_medium_sentence_progressive_resolution(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    corpus = json.loads(
        (discovered_targets.fixtures_dir / "quanpin" / "corpus.json").read_text(encoding="utf-8")
    )
    med_cases = [c for c in corpus["cases"] if c["length_bucket"] == "medium"]
    assert len(med_cases) >= 5

    vitral_target.init({})
    vitral_target.set_schema("rime_ice")
    for case in med_cases[:3]:
        action = _type_keys(vitral_target, case["pinyin"])
        cands = _candidate_texts(action)
        assert case["expected_text"] in cands[:3], (
            f"{case['id']} expected {case['expected_text']!r} in top-3, got {cands}"
        )
        vitral_target.select_candidate(0)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f06_quanpin_long_sentence_monotonic_accuracy(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    corpus = json.loads(
        (discovered_targets.fixtures_dir / "quanpin" / "corpus.json").read_text(encoding="utf-8")
    )
    long_cases = [c for c in corpus["cases"] if c["length_bucket"] == "long"]
    assert len(long_cases) >= 5

    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    short_prefix_hits = 0
    full_sentence_hits = 0
    for case in long_cases:
        first_step = case["progression"][0]
        action_short = _type_keys(vitral_target, first_step["prefix"])
        cands_short = _candidate_texts(action_short)
        if cands_short and cands_short[0] == first_step["resolved"]:
            short_prefix_hits += 1
        vitral_target.process_key("Escape")

        action_full = _type_keys(vitral_target, case["pinyin"])
        cands_full = _candidate_texts(action_full)
        assert cands_full and cands_full[0] == case["expected_text"], (
            f"{case['id']} full long sentence expected top-1 {case['expected_text']!r}, got {cands_full}"
        )
        full_sentence_hits += 1
        vitral_target.process_key("Escape")

    assert full_sentence_hits >= short_prefix_hits
    assert full_sentence_hits == len(long_cases)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f06_quanpin_backtrack_disambiguates_ambiguous_prefix(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    corpus = json.loads(
        (discovered_targets.fixtures_dir / "quanpin" / "corpus.json").read_text(encoding="utf-8")
    )
    case = next(c for c in corpus["cases"] if c["id"] == "qp_med_01")  # xianbingmayong -> 西安兵马俑

    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    prog = case["progression"]
    prev_prefix = ""
    last_action: dict[str, Any] = {}
    for step in prog:
        delta = step["prefix"][len(prev_prefix) :]
        prev_prefix = step["prefix"]
        last_action = _type_keys(vitral_target, delta)

    cands = _candidate_texts(last_action)
    assert cands and cands[0] == prog[-1]["resolved"] == "西安兵马俑"


# ---------------------------------------------------------------------------
# F07: Multi-Format Lexicon Compiler (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f07_import_sogou_scel_and_recall_in_pinyin(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"

    vitral_target.init({})
    report = vitral_target.import_lexicon(scel_path, "scel")
    assert report["imported_count"] >= 10
    assert report["skipped_count"] == 0

    vitral_target.set_schema("rime_ice")
    for py, word in (("laodeng", "老登"), ("baojinbi", "爆金币")):
        action = _type_keys(vitral_target, py)
        cands = _candidate_texts(action)
        assert word in cands[:5], f"Imported SCEL word {word!r} missing from {cands}"
        vitral_target.select_candidate(0)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f07_import_rime_dict_yaml_respects_by_weight(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    yaml_path = discovered_targets.fixtures_dir / "lexicons" / "sample_terms.dict.yaml"

    vitral_target.init({})
    report = vitral_target.import_lexicon(yaml_path, "dict_yaml")
    assert report["imported_count"] >= 10
    assert report["skipped_count"] == 0

    vitral_target.set_schema("rime_ice")
    action = _type_keys(vitral_target, "laodeng")
    cands = _candidate_texts(action)
    assert cands and cands[0] == "老登", (
        f"Expected highest-weight '老登' (1200) at rank 1 for 'laodeng', got {cands}"
    )


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f07_import_tsv_3col_lexicon(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    tsv_path = discovered_targets.fixtures_dir / "lexicons" / "sample_pairs.tsv"

    vitral_target.init({})
    report = vitral_target.import_lexicon(tsv_path, "tsv")
    assert report["imported_count"] == 12
    assert report["skipped_count"] == 0

    vitral_target.set_schema("rime_ice")
    action = _type_keys(vitral_target, "lixianyuyin")
    assert "离线语音" in _candidate_texts(action)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f07_import_unannotated_txt_context_aware_polyphones(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    txt_path = discovered_targets.fixtures_dir / "lexicons" / "polyphone_unannotated.txt"
    oracle = json.loads(
        (discovered_targets.fixtures_dir / "lexicons" / "expected_polyphones.json").read_text(
            encoding="utf-8"
        )
    )

    vitral_target.init({"typo_correction_level": "off"})
    report = vitral_target.import_lexicon(txt_path, "txt")
    assert report["imported_count"] == len(oracle["cases"])
    assert report["skipped_count"] == 0

    vitral_target.set_schema("rime_ice")
    for case in oracle["cases"]:
        syllables = case["expected_full_pinyin"].split(" ")
        expected_py = "".join(syllables)
        action = _type_keys(vitral_target, expected_py)
        cands = _candidate_texts(action)
        assert case["text"] in cands[:5], (
            f"Polyphone {case['id']} ({case['text']} via {expected_py}) missing from {cands}"
        )
        vitral_target.select_candidate(0)
        vitral_target.process_key("Escape")

        for raw_disallowed in case.get("disallowed_syllables", []):
            plain_disallowed = _strip_pinyin_tones(raw_disallowed)
            if plain_disallowed == case["expected_syllable"]:
                continue
            bad_syllables = list(syllables)
            bad_syllables[int(case["target_index"])] = plain_disallowed
            bad_py = "".join(bad_syllables)
            bad_action = _type_keys(vitral_target, bad_py)
            bad_cands = _candidate_texts(bad_action)
            assert case["text"] not in bad_cands[:5], (
                f"Polyphone {case['id']} ({case['text']}) leaked on disallowed pinyin {bad_py!r}: {bad_cands}"
            )
            vitral_target.process_key("Escape")


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f07_imported_scel_hotwords_callable_in_flypy_shuangpin(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"

    vitral_target.init({})
    vitral_target.import_lexicon(scel_path, "scel")
    vitral_target.set_schema("double_pinyin_flypy")

    # Xiaohe Flypy: lao=lc, deng=dg -> lcdg; bao=bc, jin=jb, bi=bi -> bcjbbi
    action_ld = _type_keys(vitral_target, "lcdg")
    assert "老登" in _candidate_texts(action_ld)
    vitral_target.select_candidate(0)

    action_bjb = _type_keys(vitral_target, "bcjbbi")
    assert "爆金币" in _candidate_texts(action_bjb)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f07_imported_lexicon_staged_for_koofr_imports_sync(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    tsv_path = discovered_targets.fixtures_dir / "lexicons" / "sample_pairs.tsv"

    vitral_target.init({"koofr_endpoint": webdav_server.koofr_endpoint})
    report = vitral_target.import_lexicon(tsv_path, "tsv")
    assert report["imported_count"] >= 10

    imported_files = webdav_server.list_files("lexicon/imports/")
    assert imported_files, "Expected imported lexicon artifact staged under /Koofr/Vitral/lexicon/imports/"
    webdav_server.assert_no_live_slot()
