"""Tier 2 BVA and negative/corner-case tests for F05-F07 (Core Input, Quanpin, Lexicons)."""

from __future__ import annotations

import json
import time
import unicodedata
from pathlib import Path
from typing import Any

import pytest

from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets

pytestmark = pytest.mark.tier2


def _fixtures_dir(discovered_targets: DiscoveredTargets) -> Path:
    assert discovered_targets.fixtures_dir is not None, "tests/fixtures directory missing"
    return discovered_targets.fixtures_dir


def _type_ascii(target: VitralTarget, text: str) -> dict[str, Any]:
    last: dict[str, Any] = {}
    for ch in text:
        last = target.process_key(ch)
    return last


def _extract_candidates(action: dict[str, Any]) -> list[str]:
    raw = action.get("candidates", [])
    out: list[str] = []
    for item in raw:
        if isinstance(item, str):
            out.append(item)
        elif isinstance(item, dict):
            out.append(str(item.get("text", item.get("word", ""))))
    return out


def _preedit_text(action: dict[str, Any]) -> str:
    raw = action.get("preedit", "")
    if isinstance(raw, dict):
        return str(raw.get("text", ""))
    return str(raw or "")


# ======================================================================
# F05: vitral-core Input Engine (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f05_candidate_page_size_strict_cap_5(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    for schema in ("rime_ice", "double_pinyin_flypy"):
        vitral_target.set_schema(schema)
        vitral_target.process_key("Escape")
        action = _type_ascii(vitral_target, "shi")
        cands = _extract_candidates(action)
        assert 1 <= len(cands) <= 5, f"Schema {schema} page size out of [1,5]: {len(cands)}"
        next_page = vitral_target.process_key("Page_Down")
        next_cands = _extract_candidates(next_page)
        assert 0 <= len(next_cands) <= 5
        if next_cands:
            assert next_cands != cands or len(cands) < 5


@pytest.mark.requires_core
def test_f05_xiaohe_vs_quanpin_key_mapping_divergence(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "double_pinyin_flypy"})
    # Valid Xiaohe Shuangpin sequence: "nihc" -> ni + hao ("ao" -> 'c') ("你好")
    flypy_act = _type_ascii(vitral_target, "nihc")
    flypy_cands = _extract_candidates(flypy_act)
    assert len(flypy_cands) >= 1
    assert "你好" in flypy_cands, f"Expected '你好' for Xiaohe 'nihc', got {flypy_cands}"

    # Switch to Quanpin directly on a dirty composing preedit (without prior Escape); must reset cleanly
    vitral_target.set_schema("rime_ice")
    after_switch = vitral_target.process_key("")
    assert _preedit_text(after_switch) == ""
    assert _extract_candidates(after_switch) == []

    # Under Quanpin, "nihc" is NOT a valid sequence for "你好"
    qp_diverge = _extract_candidates(_type_ascii(vitral_target, "nihc"))
    assert not qp_diverge or qp_diverge[0] != "你好"

    vitral_target.process_key("Escape")
    qp_act = _type_ascii(vitral_target, "xiaoheshuangpin")
    qp_cands = _extract_candidates(qp_act)
    assert len(qp_cands) >= 1
    assert any("小鹤" in c or "双拼" in c for c in qp_cands)

    # Switch back to Xiaohe directly on dirty composing preedit and test "xnheulpb" -> "小鹤双拼"
    vitral_target.set_schema("double_pinyin_flypy")
    after_back = vitral_target.process_key("")
    assert _preedit_text(after_back) == ""
    assert _extract_candidates(after_back) == []
    flypy_sp_cands = _extract_candidates(_type_ascii(vitral_target, "xnheulpb"))
    assert len(flypy_sp_cands) >= 1
    assert any("小鹤" in c or "双拼" in c for c in flypy_sp_cands)


@pytest.mark.requires_core
def test_f05_opencc_all_4_modes_and_invalid_mode(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    outputs: dict[str, list[str]] = {}
    for mode in ("simplified", "traditional", "taiwan_standard", "hong_kong"):
        vitral_target.set_opencc_mode(mode)
        vitral_target.process_key("Escape")
        act = _type_ascii(vitral_target, "ruanjian")
        outputs[mode] = _extract_candidates(act)
        assert len(outputs[mode]) >= 1

    assert any("软件" in c for c in outputs["simplified"])
    assert any("軟體" in c or "軟件" in c for c in outputs["taiwan_standard"] + outputs["traditional"])

    with pytest.raises(VitralCommandError):
        vitral_target.set_opencc_mode("nonexistent_opencc_mode_99")


@pytest.mark.requires_core
def test_f05_non_alpha_empty_and_escape_key_boundaries(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    # Backspace / Escape / unknown F15 / empty key on empty preedit must not panic
    for key in ("", "BackSpace", "Escape", "F15"):
        act = vitral_target.process_key(key)
        assert isinstance(act, dict)
        assert _preedit_text(act) == ""

    _type_ascii(vitral_target, "nihao")
    cleared = vitral_target.process_key("Escape")
    assert _preedit_text(cleared) == ""
    assert _extract_candidates(cleared) == []


@pytest.mark.requires_core
def test_f05_long_preedit_120_chars_stability(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    sequence = ("womenxuyaojiejue" * 8)[:120]
    assert len(sequence) == 120
    act = _type_ascii(vitral_target, sequence)
    cands = _extract_candidates(act)
    assert 1 <= len(cands) <= 5
    assert len(_preedit_text(act)) > 0


# ======================================================================
# F06: Long-Context Quanpin Research & Decoder (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f06_quanpin_monotonic_progression_corpus(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    corpus_path = _fixtures_dir(discovered_targets) / "quanpin" / "corpus.json"
    corpus = json.loads(corpus_path.read_text(encoding="utf-8"))
    vitral_target.init({"schema": "rime_ice"})

    bucket_hits: dict[str, list[bool]] = {"short": [], "medium": [], "long": []}
    for case in corpus["cases"]:
        vitral_target.process_key("Escape")
        prev_len = 0
        final_cands: list[str] = []
        for step in case["progression"]:
            prefix = step["prefix"]
            delta = prefix[prev_len:]
            prev_len = len(prefix)
            act = _type_ascii(vitral_target, delta)
            final_cands = _extract_candidates(act)
            assert 1 <= len(final_cands) <= 5

        top1 = final_cands[0] if final_cands else ""
        bucket_hits[case["length_bucket"]].append(top1 == case["expected_text"])
        assert top1 == case["expected_text"], (
            f"Corpus case {case['id']} ({case['pinyin']}) top-1={top1!r} != {case['expected_text']!r}"
        )

    short_acc = sum(bucket_hits["short"]) / len(bucket_hits["short"])
    medium_acc = sum(bucket_hits["medium"]) / len(bucket_hits["medium"])
    long_acc = sum(bucket_hits["long"]) / len(bucket_hits["long"])
    assert long_acc >= short_acc and medium_acc >= short_acc


@pytest.mark.requires_core
def test_f06_quanpin_zero_initial_apostrophe_boundaries(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    pairs = [
        ("fangan", "方案"),
        ("changan", "长安"),
        ("xi'an", "西安"),
    ]
    for py, expected in pairs:
        vitral_target.process_key("Escape")
        act = _type_ascii(vitral_target, py)
        cands = _extract_candidates(act)
        assert expected in cands, f"Expected {expected!r} in candidates for {py!r}, got {cands}"

    vitral_target.process_key("Escape")
    xian_cands = _extract_candidates(_type_ascii(vitral_target, "xian"))
    assert len(xian_cands) >= 1 and any(c in ("先", "线", "现", "西安") for c in xian_cands)


@pytest.mark.requires_core
def test_f06_quanpin_ambiguous_trailing_nasal_shift(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    act_fan = _type_ascii(vitral_target, "fan")
    cands_fan = _extract_candidates(act_fan)
    assert any(c in ("反", "饭", "翻", "范") for c in cands_fan)

    act_fangan = _type_ascii(vitral_target, "gan")
    cands_fangan = _extract_candidates(act_fangan)
    assert cands_fangan[0] == "方案", f"Expected trailing 'gan' to shift to '方案', got {cands_fangan}"


@pytest.mark.requires_core
def test_f06_quanpin_impossible_syllable_fallback(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    for bad_seq in ("zzzzzz", "vvvvv", "xbxbxb"):
        vitral_target.process_key("Escape")
        act = _type_ascii(vitral_target, bad_seq)
        assert isinstance(act, dict)
        assert len(_extract_candidates(act)) <= 5


@pytest.mark.requires_core
def test_f06_quanpin_determinism_and_latency_bound(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    corpus_path = _fixtures_dir(discovered_targets) / "quanpin" / "corpus.json"
    corpus = json.loads(corpus_path.read_text(encoding="utf-8"))
    longest_cases = [c for c in corpus["cases"] if c["length_bucket"] == "long"][-2:]
    vitral_target.init({"schema": "rime_ice"})

    for case in longest_cases:
        runs: list[list[str]] = []
        for _ in range(2):
            vitral_target.process_key("Escape")
            t0 = time.perf_counter()
            act = _type_ascii(vitral_target, case["pinyin"])
            elapsed = time.perf_counter() - t0
            assert elapsed < 2.0, f"Latency exceeded bound ({elapsed:.3f}s) for {case['id']}"
            runs.append(_extract_candidates(act))
        assert runs[0] == runs[1], f"Non-deterministic Quanpin decoding for {case['id']}"


# ======================================================================
# F07: Multi-Format Lexicon Compiler (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f07_corrupt_scel_graceful_rejection(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    corrupt_scel = _fixtures_dir(discovered_targets) / "lexicons" / "corrupt.scel"
    vitral_target.init({"schema": "rime_ice"})

    with pytest.raises(VitralCommandError):
        vitral_target.import_lexicon(corrupt_scel, format_hint="scel")

    # Engine must remain healthy and responsive after corrupt import rejection
    act = _type_ascii(vitral_target, "nihao")
    assert len(_extract_candidates(act)) >= 1


@pytest.mark.requires_core
def test_f07_malformed_edge_txt_resilience(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    malformed = _fixtures_dir(discovered_targets) / "lexicons" / "malformed_edge.txt"
    vitral_target.init({"schema": "rime_ice"})

    res = vitral_target.import_lexicon(malformed, format_hint="txt")
    assert isinstance(res, dict)
    skipped = int(res.get("skipped_count", 0))
    warnings = res.get("warnings", [])
    assert skipped >= 1 or len(warnings) >= 1, (
        f"Expected malformed_edge.txt lines to produce skips/warnings, got {res}"
    )


@pytest.mark.requires_core
def test_f07_unannotated_polyphone_disambiguation_and_tones(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    lex_dir = _fixtures_dir(discovered_targets) / "lexicons"
    unannotated = lex_dir / "polyphone_unannotated.txt"
    oracle = json.loads((lex_dir / "expected_polyphones.json").read_text(encoding="utf-8"))

    vitral_target.init({"schema": "rime_ice"})
    res = vitral_target.import_lexicon(unannotated, format_hint="txt")
    # PROJECT.md:126 contract: { imported_count, skipped_count, warnings }
    assert int(res.get("imported_count", 0)) == len(oracle["cases"])
    assert int(res.get("skipped_count", 0)) == 0
    assert isinstance(res.get("warnings", []), list)

    # Support optional entries/imported_entries if provided by core/CLI
    annotated_entries = res.get("entries", res.get("imported_entries", []))
    if annotated_entries:
        by_text = {str(e.get("text", e.get("word", ""))): e for e in annotated_entries if isinstance(e, dict)}
        for case in oracle["cases"]:
            phrase = case["text"]
            assert phrase in by_text, f"Imported entries missing polyphone phrase {phrase!r}"
            entry = by_text[phrase]
            toned = str(entry.get("toned_pinyin", entry.get("pinyin_toned", "")))
            plain = str(entry.get("pinyin", entry.get("code", "")))
            idx = int(case["target_index"])
            if toned:
                toned_tokens = toned.split()
                assert len(toned_tokens) > idx
                assert toned_tokens[idx] == case["expected_toned_syllable"]
                for bad in case["disallowed_syllables"]:
                    assert toned_tokens[idx] != bad
            if plain:
                plain_tokens = plain.split()
                if len(plain_tokens) > idx:
                    assert plain_tokens[idx] == case["expected_syllable"]

    # Verify context-aware polyphone auto-pinyin recall across 重/行/长/乐/得/发 (including fā vs fà)
    vitral_target.set_schema("rime_ice")
    for case in oracle["cases"]:
        vitral_target.process_key("Escape")
        expected_py = case["expected_full_pinyin"].replace(" ", "")
        act = _type_ascii(vitral_target, expected_py)
        cands = _extract_candidates(act)
        assert case["text"] in cands, (
            f"{case['id']} ({case['text']}) not recalled under {expected_py!r}: {cands}"
        )
        # Catch swapped tones (e.g. fā vs fà) whenever candidate item surfaces toned_pinyin or toned comment
        raw_cands = act.get("candidates", [])
        cand_item = next(
            (c for c in raw_cands if isinstance(c, dict) and c.get("text", c.get("word")) == case["text"]),
            None,
        )
        if cand_item is not None:
            surfaced_toned = str(cand_item.get("toned_pinyin", cand_item.get("comment", ""))).strip()
            has_tone_mark = any(
                unicodedata.combining(ch) for ch in unicodedata.normalize("NFKD", surfaced_toned)
            )
            if surfaced_toned and ("toned_pinyin" in cand_item or has_tone_mark):
                idx = int(case["target_index"])
                toned_toks = surfaced_toned.split()
                if len(toned_toks) > idx:
                    assert toned_toks[idx] == case["expected_toned_syllable"], (
                        f"Tone mismatch for {case['id']} ({case['text']}): "
                        f"got {toned_toks[idx]!r}, expected {case['expected_toned_syllable']!r}"
                    )
                    for bad in case["disallowed_syllables"]:
                        assert toned_toks[idx] != bad

    # Negative check: disallowed polyphone readings must not recall those phrases
    disallowed_probes = [
        ("zhongfujianshe", "重复建设"),
        ("yinxingyingye", "银行营业"),
        ("zhuozhuangchengchang", "茁壮成长"),
        ("celiangzhangdu", "测量长度"),
        ("gudianyinle", "古典音乐"),
        ("feideruci", "非得如此"),
    ]
    for bad_py, phrase in disallowed_probes:
        vitral_target.process_key("Escape")
        bad_cands = _extract_candidates(_type_ascii(vitral_target, bad_py))
        assert phrase not in bad_cands, f"Phrase {phrase!r} leaked under disallowed reading {bad_py!r}"


@pytest.mark.requires_core
def test_f07_scel_dict_yaml_tsv_weight_ordering(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    lex_dir = _fixtures_dir(discovered_targets) / "lexicons"
    vitral_target.init({"schema": "rime_ice"})

    res_scel = vitral_target.import_lexicon(lex_dir / "sample_hotwords.scel", format_hint="scel")
    assert int(res_scel.get("imported_count", 0)) >= 12
    res_yaml = vitral_target.import_lexicon(lex_dir / "sample_terms.dict.yaml", format_hint="dict_yaml")
    assert int(res_yaml.get("imported_count", 0)) >= 12
    res_tsv = vitral_target.import_lexicon(lex_dir / "sample_pairs.tsv", format_hint="tsv")
    assert int(res_tsv.get("imported_count", 0)) >= 12

    # In sample_hotwords.scel & sample_pairs.tsv, '爆光' (500/520) outweighs '曝光' (300/410)
    act = _type_ascii(vitral_target, "baoguang")
    cands = _extract_candidates(act)
    assert "爆光" in cands and "曝光" in cands
    assert cands.index("爆光") < cands.index("曝光")


@pytest.mark.requires_core
def test_f07_nonexistent_empty_and_mismatched_format_hint(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    vitral_target.init({"schema": "rime_ice"})
    missing_path = tmp_path / "does_not_exist.dict.yaml"
    with pytest.raises(VitralCommandError):
        vitral_target.import_lexicon(missing_path, format_hint="dict_yaml")

    empty_file = tmp_path / "empty.tsv"
    empty_file.write_bytes(b"")
    try:
        res_empty = vitral_target.import_lexicon(empty_file, format_hint="tsv")
        assert int(res_empty.get("imported_count", 0)) == 0
    except VitralCommandError:
        pass

    scel_path = _fixtures_dir(discovered_targets) / "lexicons" / "sample_hotwords.scel"
    try:
        res_mismatch = vitral_target.import_lexicon(scel_path, format_hint="tsv")
        assert int(res_mismatch.get("imported_count", 0)) == 0
    except VitralCommandError:
        pass
