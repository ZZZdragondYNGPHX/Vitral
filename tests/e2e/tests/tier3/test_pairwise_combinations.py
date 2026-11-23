"""Tier 3 cross-feature pairwise integration tests covering F01-F20."""

from __future__ import annotations

import ctypes
import json
import re
import shutil
import struct
import subprocess
import sys
import wave
import zipfile
from pathlib import Path
from typing import Any

import pytest

from harnesses import AIMockServer, LocalSendMockPeer, WebDAVMockServer
from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets
from vitral_e2e.tokens import assert_english_only_brand, assert_no_toolbar_text_labels

pytestmark = pytest.mark.tier3


# ----------------------------------------------------------------------
# Local Test Helpers (Opaque-Box Only)
# ----------------------------------------------------------------------


def _fixtures_dir(discovered_targets: DiscoveredTargets) -> Path:
    assert discovered_targets.fixtures_dir is not None, "tests/fixtures directory missing"
    return discovered_targets.fixtures_dir


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
            out.append(str(item.get("text", item.get("word", ""))))
    return out


def _preedit_text(action: dict[str, Any]) -> str:
    raw = action.get("preedit", "")
    if isinstance(raw, dict):
        return str(raw.get("text", ""))
    return str(raw or "")


def _stroke_points(stroke: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "x": float(p["x"]),
            "y": float(p["y"]),
            "timestamp_ms": int(p.get("timestamp_ms", p.get("t", 0))),
        }
        for p in stroke
    ]


def _replay_strokes(
    target: VitralTarget, strokes: list[list[dict[str, Any]]]
) -> dict[str, Any]:
    last: dict[str, Any] = {}
    for stroke in strokes:
        last = target.hw_push_stroke(_stroke_points(stroke), pen_up=True)
    return last


def _extract_pushed_id(pushed: dict[str, Any], expected_content: str) -> str:
    ring = pushed.get("ring")
    assert isinstance(ring, list) and ring, f"Expected non-empty clipboard ring: {pushed}"
    entry = next((e for e in ring if isinstance(e, dict) and e.get("content") == expected_content), None)
    assert entry is not None, f"Missing pushed content {expected_content!r} in ring: {ring}"
    assert entry.get("id"), f"ClipboardEntry missing 'id': {entry}"
    return str(entry["id"])


def _qp_content(qp_state: dict[str, Any]) -> str | None:
    assert "quick_paste" in qp_state, f"Missing 'quick_paste' in {qp_state}"
    entry = qp_state["quick_paste"]
    if entry is None:
        return None
    assert isinstance(entry, dict), f"Invalid quick_paste entry: {qp_state}"
    val = entry.get("content", entry.get("text"))
    return None if val is None else str(val)


def _read_wav_pcm(path: Path) -> bytes:
    with wave.open(str(path), "rb") as wf:
        assert wf.getframerate() == 16000
        assert wf.getnchannels() == 1
        assert wf.getsampwidth() == 2
        return wf.readframes(wf.getnframes())


def _read_non_generated_sources(
    directory: Path,
    suffixes: tuple[str, ...],
    *,
    exclude_names: set[str] | None = None,
    exclude_parts: tuple[str, ...] = ("generated", "build", "target", "node_modules"),
) -> tuple[list[Path], str]:
    if not directory.is_dir():
        return [], ""
    excluded = exclude_names or set()
    files: list[Path] = []
    for p in sorted(directory.rglob("*")):
        if not p.is_file() or p.suffix.lower() not in suffixes:
            continue
        if p.name in excluded:
            continue
        rel_parts = p.relative_to(directory).parts
        if any(part in exclude_parts for part in rel_parts):
            continue
        files.append(p)
    return files, "\n".join(p.read_text(encoding="utf-8", errors="replace") for p in files)


def _parse_pe_headers(data: bytes) -> tuple[int, int, int, list[tuple[int, int, int]], int]:
    assert data[:2] == b"MZ", "Missing MZ DOS header"
    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    assert data[pe_off : pe_off + 4] == b"PE\x00\x00", "Missing PE signature"
    machine, num_sections, _, _, _, opt_hdr_size, characteristics = struct.unpack_from(
        "<HHIIIHH", data, pe_off + 4
    )
    opt_off = pe_off + 24
    opt_magic = struct.unpack_from("<H", data, opt_off)[0]
    sec_off = opt_off + opt_hdr_size
    sections: list[tuple[int, int, int]] = []
    for i in range(num_sections):
        s = sec_off + i * 40
        vsize, va, raw_size, raw_ptr = struct.unpack_from("<IIII", data, s + 8)
        sections.append((va, max(vsize, raw_size), raw_ptr))
    return machine, characteristics, opt_magic, sections, opt_off


def _pe_imports(data: bytes) -> list[str]:
    _, _, opt_magic, sections, opt_off = _parse_pe_headers(data)
    assert opt_magic == 0x20B
    import_rva, import_size = struct.unpack_from("<II", data, opt_off + 120)
    if import_rva == 0 or import_size == 0:
        return []
    imp_off = next(
        (raw_ptr + (import_rva - va) for va, span, raw_ptr in sections if va <= import_rva < va + span),
        None,
    )
    assert imp_off is not None
    out: list[str] = []
    while True:
        _, _, _, name_rva, first_thunk = struct.unpack_from("<IIIII", data, imp_off)
        if name_rva == 0 and first_thunk == 0:
            break
        name_off = next(
            (raw_ptr + (name_rva - va) for va, span, raw_ptr in sections if va <= name_rva < va + span),
            None,
        )
        if name_off is not None:
            end = data.index(b"\x00", name_off)
            out.append(data[name_off:end].decode("ascii", errors="replace"))
        imp_off += 20
    return out


_RAW_STR_START_RE = re.compile(r'(?:br|cr|r)(#*)"')
_CFG_TEST_ATTR_RE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def _advance_rust_non_code(text: str, cur: int, n: int) -> int | None:
    ch = text[cur]
    nxt = text[cur + 1] if cur + 1 < n else ""
    if ch == "/" and nxt == "/":
        cur += 2
        while cur < n and text[cur] != "\n":
            cur += 1
        return cur + 1 if cur < n else n
    if ch == "/" and nxt == "*":
        depth = 1
        cur += 2
        while cur < n and depth > 0:
            c0 = text[cur]
            c1 = text[cur + 1] if cur + 1 < n else ""
            if c0 == "/" and c1 == "*":
                depth += 1
                cur += 2
            elif c0 == "*" and c1 == "/":
                depth -= 1
                cur += 2
            else:
                cur += 1
        return cur
    prev = text[cur - 1] if cur > 0 else ""
    if ch in ("r", "b", "c") and not (prev.isalnum() or prev == "_"):
        rm = _RAW_STR_START_RE.match(text, cur)
        if rm is not None:
            raw_hashes = len(rm.group(1))
            end_hashes = "#" * raw_hashes
            cur = rm.end()
            while cur < n:
                if text[cur] == '"' and text.startswith(end_hashes, cur + 1):
                    return cur + 1 + raw_hashes
                cur += 1
            return n
        if ch == "b" and nxt == "'":
            cur += 1
            ch = "'"
            nxt = text[cur + 1] if cur + 1 < n else ""
    if ch == "'":
        if nxt == "\\":
            if cur + 2 >= n:
                return n
            cur += 3
            while cur < n and text[cur] not in ("'", "\n"):
                cur += 1
            return cur + 1 if cur < n and text[cur] == "'" else cur
        if cur + 2 < n and text[cur + 2] == "'" and nxt != "'":
            return cur + 3
        return cur + 1
    if ch == '"':
        cur += 1
        while cur < n:
            c = text[cur]
            if c == "\\":
                cur += 2
            elif c == '"':
                return cur + 1
            else:
                cur += 1
        return n
    return None


def _strip_rust_cfg_test_blocks(text: str) -> str:
    out: list[str] = []
    pos = 0
    chunk_start = 0
    n = len(text)
    while pos < n:
        skipped = _advance_rust_non_code(text, pos, n)
        if skipped is not None:
            pos = skipped
            continue
        if text[pos] == "#":
            m = _CFG_TEST_ATTR_RE.match(text, pos)
            if m is not None:
                out.append(text[chunk_start:pos])
                cur = m.end()
                depth = 0
                paren_depth = 0
                bracket_depth = 0
                saw_brace = False
                while cur < n:
                    inner_skipped = _advance_rust_non_code(text, cur, n)
                    if inner_skipped is not None:
                        cur = inner_skipped
                        continue
                    ch = text[cur]
                    if ch == "{":
                        depth += 1
                        saw_brace = True
                    elif ch == "}":
                        if saw_brace:
                            depth -= 1
                            if depth == 0:
                                cur += 1
                                break
                    elif not saw_brace:
                        if ch == "(":
                            paren_depth += 1
                        elif ch == ")" and paren_depth > 0:
                            paren_depth -= 1
                        elif ch == "[":
                            bracket_depth += 1
                        elif ch == "]" and bracket_depth > 0:
                            bracket_depth -= 1
                        elif ch == ";" and paren_depth == 0 and bracket_depth == 0:
                            cur += 1
                            break
                    cur += 1
                pos = cur
                chunk_start = cur
                continue
        pos += 1
    out.append(text[chunk_start:])
    return "".join(out)


# ======================================================================
# Pairwise Cross-Feature Tests (>= 20 tests across F01-F20)
# ======================================================================


@pytest.mark.requires_core
def test_t3_f07_x_f09_scel_and_unannotated_txt_feed_mode2_handwriting(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F07 x F09: Imported .scel & un-annotated .txt hotwords drive Mode-2 backtracking without overriding clear locks."""
    fx_dir = _fixtures_dir(discovered_targets)
    scel_path = fx_dir / "lexicons" / "sample_hotwords.scel"
    poly_path = fx_dir / "lexicons" / "polyphone_unannotated.txt"
    ambig_fx = json.loads((fx_dir / "handwriting" / "mode2_baoguang_laodeng.json").read_text(encoding="utf-8"))
    clear_fx = json.loads((fx_dir / "handwriting" / "mode2_clear_locked.json").read_text(encoding="utf-8"))

    vitral_target.init({"schema": "rime_ice"})
    scel_rep = vitral_target.import_lexicon(scel_path, "scel")
    assert int(scel_rep.get("imported_count", 0)) >= 12
    txt_rep = vitral_target.import_lexicon(poly_path, "txt")
    assert int(txt_rep.get("imported_count", 0)) == 13

    # Mode-2 retroactive flip enabled by imported .scel hotwords ("爆光", "老登", "金币")
    vitral_target.hw_begin("ContinuousLattice")
    seg1_end = ambig_fx["char_segments"][1]["stroke_range"][1] + 1
    mid = _replay_strokes(vitral_target, ambig_fx["strokes"][:seg1_end])
    assert mid["composing_text"] == "曝光"
    assert mid["slots"][0]["locked_by_entropy"] is False

    _replay_strokes(vitral_target, ambig_fx["strokes"][seg1_end:])
    final_res = vitral_target.hw_end()
    assert final_res["composing_text"] == "爆光老登金币"
    assert final_res["slots"][0]["char"] == "爆"
    assert final_res["slots"][0]["changed_retroactively"] is True
    top8_chars = [item["char"] if isinstance(item, dict) else str(item) for item in final_res["slots"][0]["top8"]]
    assert "曝" in top8_chars and "爆" in top8_chars

    # Clear high-confidence character must stay entropy-locked despite hotword prior
    vitral_target.hw_clear()
    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, clear_fx["strokes"])
    clear_res = vitral_target.hw_end()
    assert clear_res["slots"][0]["locked_by_entropy"] is True
    assert clear_res["slots"][0]["char"] == "曝"
    assert clear_res["slots"][0]["changed_retroactively"] is False
    assert clear_res["composing_text"] == "曝光老登金币"

    # Imported polyphone .txt entries remain recallable in Pinyin alongside handwriting
    vitral_target.process_key("Escape")
    poly_cands = _candidate_texts(_type_keys(vitral_target, "zhongyaojueding"))
    assert "重要决定" in poly_cands


@pytest.mark.requires_core
def test_t3_f05_f09_x_f10_shared_frecency_xiaohe_quanpin_and_mode2_hw(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F05 x F09 x F10: Xiaohe & Quanpin share Frecency with confirmed Mode-2 handwriting."""
    fx_hw = json.loads(
        (_fixtures_dir(discovered_targets) / "handwriting" / "mode2_horizontal_connected.json").read_text(
            encoding="utf-8"
        )
    )
    vitral_target.init({"schema": "double_pinyin_flypy"})

    # Verify Xiaohe core typing ("nihc" -> "你好")
    xiaohe_cands = _candidate_texts(_type_keys(vitral_target, "nihc"))
    assert "你好" in xiaohe_cands, f"Expected Xiaohe 'nihc' -> '你好', got {xiaohe_cands}"

    # Switch to Quanpin and promote a rival candidate on "shouxiezhongwen" to rank 0 first
    vitral_target.set_schema("rime_ice")
    probe_cands = _candidate_texts(_type_keys(vitral_target, "shouxiezhongwen"))
    assert len(probe_cands) >= 2
    rival = next((c for c in probe_cands if c != "手写中文"), None)
    assert rival is not None, f"Expected competing candidate for 'shouxiezhongwen', got {probe_cands}"
    vitral_target.process_key("Escape")

    rival_boosts = 0
    for attempt in range(3):
        cur_cands = _candidate_texts(_type_keys(vitral_target, "shouxiezhongwen"))
        assert rival in cur_cands
        vitral_target.select_candidate(cur_cands.index(rival))
        rival_boosts += 1
        if attempt >= 1:
            chk_cands = _candidate_texts(_type_keys(vitral_target, "shouxiezhongwen"))
            vitral_target.process_key("Escape")
            if chk_cands[0] == rival:
                break

    before_hw = _type_keys(vitral_target, "shouxiezhongwen")
    assert _candidate_texts(before_hw)[0] == rival
    pre_score = next(
        (
            float(it["frecency_score"])
            for it in before_hw.get("candidates", [])
            if isinstance(it, dict) and it.get("text") == "手写中文" and "frecency_score" in it
        ),
        None,
    )
    vitral_target.process_key("Escape")

    # Confirm Mode-2 handwriting ("手写中文") -> shared Frecency reclaims rank 0 in Pinyin
    for _ in range(rival_boosts + 2):
        vitral_target.hw_begin("ContinuousLattice")
        _replay_strokes(vitral_target, fx_hw["strokes"])
        hw_res = vitral_target.hw_end()
        assert hw_res["composing_text"] == "手写中文"
        vitral_target.hw_clear()

    after_hw = _type_keys(vitral_target, "shouxiezhongwen")
    after_cands = _candidate_texts(after_hw)
    assert after_cands[0] == "手写中文", f"Expected '手写中文' to reclaim rank 0, got {after_cands}"
    if pre_score is not None:
        post_score = next(
            (
                float(it["frecency_score"])
                for it in after_hw.get("candidates", [])
                if isinstance(it, dict) and it.get("text") == "手写中文" and "frecency_score" in it
            ),
            None,
        )
        assert post_score is not None and post_score > pre_score


@pytest.mark.requires_core
def test_t3_f05_f10_x_f11_ksb_clipboard_shortcut_both_schemas_and_opencc(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    """F05 x F10 x F11: Pinned 'ksb' shortcut surfaces in both Xiaohe & Quanpin alongside OpenCC conversion."""
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "schema": "double_pinyin_flypy",
        "clipboard_ring_capacity": 6,
    })

    # Pre-pin an unrelated item so Reverse(a.pinned) ring sort places subsequent unpinned pushes after it
    first_push = vitral_target.clipboard_push("PinnedFirstGuard")
    first_id = _extract_pushed_id(first_push, "PinnedFirstGuard")
    vitral_target.clipboard_pin(first_id, "grd")

    target_payload = "https://vitral.dev/ksb-pinned-note"
    pushed = vitral_target.clipboard_push(target_payload)
    target_id = _extract_pushed_id(pushed, target_payload)
    vitral_target.clipboard_pin(target_id, "ksb")

    # Surfaces in Xiaohe (double_pinyin_flypy)
    flypy_act = _type_keys(vitral_target, "ksb")
    flypy_cands = _candidate_texts(flypy_act)
    assert flypy_cands and target_payload in flypy_cands[0], f"Xiaohe ksb failed: {flypy_cands}"
    assert flypy_act["candidates"][0].get("is_clipboard_shortcut") is True

    # Surfaces in Quanpin (rime_ice) and survives OpenCC mode toggle
    vitral_target.set_schema("rime_ice")
    vitral_target.set_opencc_mode("traditional")
    qp_trad_act = _type_keys(vitral_target, "ksb")
    qp_trad_cands = _candidate_texts(qp_trad_act)
    assert qp_trad_cands and target_payload in qp_trad_cands[0], f"Quanpin ksb failed: {qp_trad_cands}"

    vitral_target.process_key("Escape")
    trad_cands = _candidate_texts(_type_keys(vitral_target, "ruanjian"))
    assert any("軟件" in c or "軟體" in c for c in trad_cands), f"Expected OpenCC traditional output: {trad_cands}"

    vitral_target.set_opencc_mode("simplified")
    vitral_target.process_key("Escape")
    simp_cands = _candidate_texts(_type_keys(vitral_target, "ruanjian"))
    assert any("软件" in c for c in simp_cands), f"Expected OpenCC simplified output: {simp_cands}"
    webdav_server.assert_no_live_slot()


@pytest.mark.requires_core
def test_t3_f12_x_f09_f11_f19_pure_offline_blocks_network_keeps_local_engines(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    localsend_server: LocalSendMockPeer,
    ai_server: AIMockServer,
) -> None:
    """F12 x F09 x F11 x F19: Pure Offline blocks WebDAV/LocalSend/BYOK while local voice, librime, & HW run."""
    fx_dir = _fixtures_dir(discovered_targets)
    pcm = _read_wav_pcm(fx_dir / "audio" / "mandarin_short_16k.wav")
    fx_mao = json.loads((fx_dir / "handwriting" / "mode1_mao.json").read_text(encoding="utf-8"))
    fx_hw = json.loads((fx_dir / "handwriting" / "mode2_horizontal_connected.json").read_text(encoding="utf-8"))

    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "ai_endpoint": ai_server.ollama_url,
        "known_devices": [
            {
                "device_id": localsend_server.fingerprint,
                "device_name": localsend_server.alias,
                "tier": "lan",
                "usage_count": 1,
                "last_used_ms": 1000,
                "localsend_url": localsend_server.base_url,
            }
        ],
    })
    webdav_server.clear_request_log()
    localsend_server.clear_request_log()
    ai_server.clear_request_log()

    vitral_target.set_pure_offline(True)
    assert vitral_target.is_pure_offline() is True

    with pytest.raises(VitralCommandError):
        vitral_target.ai_inline("translate", "纯离线拦截", "en")
    with pytest.raises(VitralCommandError):
        vitral_target.restore_from_koofr({"koofr_endpoint": webdav_server.koofr_endpoint})
    with pytest.raises(VitralCommandError):
        vitral_target.relay_send(localsend_server.fingerprint, "blocked-lan-payload")
    with pytest.raises(VitralCommandError):
        vitral_target.relay_poll_inbox()

    assert webdav_server.request_log == []
    assert localsend_server.request_log == []
    assert ai_server.request_log == []

    # Offline SenseVoice remains operational
    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm)
    v_res = vitral_target.voice_stop()
    assert any(kw in v_res.get("transcript", "") for kw in ("你好", "天气"))

    # Offline librime (Xiaohe & Quanpin) remains operational
    vitral_target.set_schema("double_pinyin_flypy")
    assert "你好" in _candidate_texts(_type_keys(vitral_target, "nihc"))
    vitral_target.set_schema("rime_ice")
    assert "你好" in _candidate_texts(_type_keys(vitral_target, "nihao"))

    # Offline Mode-1 & Tier 1+2 Mode-2 handwriting remain operational
    vitral_target.hw_begin("PhoneticLookup")
    _replay_strokes(vitral_target, fx_mao["strokes"])
    m1_res = vitral_target.hw_end()
    assert any(c["char"] == "耄" for c in m1_res.get("candidates", []))
    vitral_target.hw_clear()

    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx_hw["strokes"])
    m2_res = vitral_target.hw_end()
    assert m2_res["composing_text"] == "手写中文"

    assert webdav_server.request_log == []
    assert localsend_server.request_log == []
    assert ai_server.request_log == []


@pytest.mark.requires_core
def test_t3_f05_x_f10_per_app_cn_en_mixed_punct_and_time_macros(
    vitral_target: VitralTarget,
) -> None:
    """F05 x F10: Per-app CN/EN context memory interacts with mixed CN/EN, paired punctuation, & rq/sj macros."""
    vitral_target.init({"schema": "rime_ice"})

    code_state = vitral_target.on_app_focus("code")
    assert str(code_state.get("lang_mode", "")).lower() == "en"

    tg_state = vitral_target.on_app_focus("org.telegram.desktop")
    assert str(tg_state.get("lang_mode", "")).lower() == "zh"

    ff_state = vitral_target.on_app_focus("org.mozilla.firefox")
    assert str(ff_state.get("lang_mode", "")).lower() == "zh"
    toggled = vitral_target.process_key("Shift", modifiers=["lang_toggle"])
    assert str(toggled.get("lang_mode", "")).lower() == "en"

    # Returning to code & firefox remembers EN; switching to telegram restores ZH
    assert str(vitral_target.on_app_focus("code").get("lang_mode", "")).lower() == "en"
    ff_again = vitral_target.on_app_focus("org.mozilla.firefox")
    assert str(ff_again.get("lang_mode", "")).lower() == "en"
    assert ff_again.get("remembered") is True

    tg_again = vitral_target.on_app_focus("org.telegram.desktop")
    assert str(tg_again.get("lang_mode", "")).lower() == "zh"

    # Paired punctuation & time macros work across both schemas in ZH app
    for schema in ("rime_ice", "double_pinyin_flypy"):
        vitral_target.set_schema(schema)
        vitral_target.process_key("Escape")
        punct_act = vitral_target.process_key("(")
        paired = str(
            punct_act.get("paired_punctuation")
            or punct_act.get("commit_text")
            or _preedit_text(punct_act)
        )
        assert "()" in paired or "（）" in paired, f"Expected paired punctuation under {schema}: {punct_act}"
        assert int(punct_act.get("cursor_delta", -1)) <= 0

        vitral_target.process_key("Escape")
        rq_cands = _candidate_texts(_type_keys(vitral_target, "rq"))
        assert any(re.search(r"\d", c) and any(sep in c for sep in ("-", "年", "/")) for c in rq_cands)

        vitral_target.process_key("Escape")
        sj_cands = _candidate_texts(_type_keys(vitral_target, "sj"))
        assert any(re.search(r"\d", c) and any(sep in c for sep in (":", "时")) for c in sj_cands)


@pytest.mark.requires_core
def test_t3_f11_x_f10_quick_paste_ram_lifecycle_localsend_and_webdav_ring(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    localsend_server: LocalSendMockPeer,
    tmp_path: Path,
) -> None:
    """F11 x F10: PROPOSAL v1.2 RAM-only Quick Paste across two devices + WebDAV history_ring + LocalSend."""
    dev_a = vitral_target
    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b_qp")
    try:
        dev_a.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "device_id": "dev-a",
            "clipboard_ring_capacity": 5,
            "known_devices": [
                {
                    "device_id": localsend_server.fingerprint,
                    "device_name": localsend_server.alias,
                    "tier": "lan",
                    "usage_count": 2,
                    "last_used_ms": 2000,
                    "localsend_url": localsend_server.base_url,
                }
            ],
        })
        dev_b.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "device_id": "dev-b",
            "clipboard_ring_capacity": 5,
        })

        # Cold-boot history load MUST NOT trigger Quick Paste
        dev_b.clipboard_ingest_remote("cold-boot-history", is_cold_boot=True)
        assert _qp_content(dev_b.quick_paste_state()) is None
        assert "cold-boot-history" in [e["content"] for e in dev_b.clipboard_state()["ring"]]

        # Copy on Device A -> remote push to Device B -> per-device independent consumption
        dev_a.clipboard_push("qp-shared-item", source_device="dev-a")
        dev_b.clipboard_ingest_remote("qp-shared-item", is_cold_boot=False)
        assert _qp_content(dev_a.quick_paste_state()) == "qp-shared-item"
        assert _qp_content(dev_b.quick_paste_state()) == "qp-shared-item"

        consumed_a = dev_a.consume_quick_paste()
        assert consumed_a["consumed"]["content"] == "qp-shared-item"
        assert _qp_content(dev_a.quick_paste_state()) is None
        assert _qp_content(dev_b.quick_paste_state()) == "qp-shared-item"

        # IME open -> close on Device B clears Quick Paste locally only
        open_b = dev_b.on_ime_open()
        assert open_b["seen_while_ime_open"] is True
        assert _qp_content(open_b) == "qp-shared-item"
        dev_b.on_ime_close()
        assert _qp_content(dev_b.quick_paste_state()) is None
        dev_b.on_ime_open()
        assert _qp_content(dev_b.quick_paste_state()) is None

        # LocalSend v2 transfer alongside persistent WebDAV ring
        tx = dev_a.relay_send(localsend_server.fingerprint, "lan-quick-text")
        assert tx.get("status") in ("ok", "sent", "delivered", "queued")
        assert len(localsend_server.received_transfers) == 1
        assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")
        webdav_server.assert_no_live_slot()
    finally:
        dev_b.close()


@pytest.mark.requires_core
def test_t3_f08_x_f05_mode1_phonetic_lookup_teaches_xiaohe_typing(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F08 x F05: Mode-1 phonetic lookup outputs Xiaohe code that directly recalls the character in double_pinyin_flypy."""
    fx_dir = _fixtures_dir(discovered_targets) / "handwriting"
    vitral_target.init({"schema": "double_pinyin_flypy"})

    for fname, expected_char, expected_tone, expected_code in (
        ("mode1_mao.json", "耄", "mào", "mc"),
        ("mode1_di.json", "頔", "dí", "di"),
    ):
        fx = json.loads((fx_dir / fname).read_text(encoding="utf-8"))
        vitral_target.hw_begin("PhoneticLookup")
        _replay_strokes(vitral_target, fx["strokes"])
        res = vitral_target.hw_end()
        cands = res.get("candidates", res.get("results", []))
        match = next((c for c in cands if c["char"] == expected_char), None)
        assert match is not None, f"Expected {expected_char} from Mode-1 {fname}, got {cands}"
        assert match["toned_pinyin"] == expected_tone
        assert match["xiaohe_code"] == expected_code
        vitral_target.hw_clear()

        # Use the Mode-1 taught Xiaohe short code directly in double_pinyin_flypy
        vitral_target.process_key("Escape")
        act = _type_keys(vitral_target, match["xiaohe_code"])
        collected = _candidate_texts(act)
        cur_act = act
        for _ in range(60):
            if expected_char in collected:
                break
            page_info = cur_act.get("page")
            if isinstance(page_info, dict) and page_info.get("has_next") is False:
                break
            next_act = vitral_target.process_key("Page_Down")
            page_cands = _candidate_texts(next_act)
            if not page_cands or page_cands == collected[-len(page_cands) :]:
                break
            collected.extend(page_cands)
            cur_act = next_act
        assert expected_char in collected, (
            f"Xiaohe code {match['xiaohe_code']!r} taught by Mode 1 failed to recall {expected_char!r}: {collected}"
        )


@pytest.mark.requires_core
def test_t3_f07_x_f10_1s_pin_erase_with_imported_dict_yaml_tsv_and_frecency(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F07 x F10: Imported .dict.yaml/.tsv lexicons integrate with 1s candidate pin/erase and Frecency."""
    lex_dir = _fixtures_dir(discovered_targets) / "lexicons"
    vitral_target.init({"schema": "rime_ice"})
    rep_yaml = vitral_target.import_lexicon(lex_dir / "sample_terms.dict.yaml", "dict_yaml")
    rep_tsv = vitral_target.import_lexicon(lex_dir / "sample_pairs.tsv", "tsv")
    assert int(rep_yaml.get("imported_count", 0)) >= 12
    assert int(rep_tsv.get("imported_count", 0)) >= 12

    # Pin and then erase a candidate on "maoboli"
    act = _type_keys(vitral_target, "maoboli")
    cands = _candidate_texts(act)
    assert "毛玻璃" in cands, f"Imported term '毛玻璃' missing: {cands}"
    assert len(cands) >= 2
    target_idx = 1 if len(cands) > 1 else 0
    pinned_word = cands[target_idx]
    pinned_act = vitral_target.pin_candidate(target_idx)
    assert _candidate_texts(pinned_act)[0] == pinned_word

    erased_act = vitral_target.erase_candidate(0)
    assert pinned_word not in _candidate_texts(erased_act)

    # Frecency boosts another imported term ("雾面玻璃") across repeated selections
    vitral_target.process_key("Escape")
    for _ in range(3):
        cur = _candidate_texts(_type_keys(vitral_target, "wumianboli"))
        assert "雾面玻璃" in cur
        vitral_target.select_candidate(cur.index("雾面玻璃"))
    after_cands = _candidate_texts(_type_keys(vitral_target, "wumianboli"))
    assert after_cands[0] == "雾面玻璃"


@pytest.mark.requires_core
def test_t3_f12_x_f19_offline_sensevoice_feeds_byok_polish_and_translate(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    ai_server: AIMockServer,
) -> None:
    """F12 x F19: Offline SenseVoice transcription feeds BYOK/Ollama spoken polish & inline translate."""
    audio_dir = _fixtures_dir(discovered_targets) / "audio"
    pcm_short = _read_wav_pcm(audio_dir / "mandarin_short_16k.wav")
    pcm_mixed = _read_wav_pcm(audio_dir / "mandarin_mixed_en_16k.wav")

    vitral_target.init({"ai_endpoint": ai_server.ollama_url, "ai_model": "qwen2.5:7b"})
    vitral_target.set_pure_offline(True)

    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm_short)
    short_res = vitral_target.voice_stop()
    short_txt = short_res["transcript"].strip()
    assert any(kw in short_txt for kw in ("你好", "天气")), f"Unexpected short transcript: {short_txt!r}"

    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm_mixed)
    mixed_res = vitral_target.voice_stop()
    mixed_txt = mixed_res["transcript"].strip()
    assert any(kw in mixed_txt for kw in ("Vitral", "测试", "使用")), f"Unexpected mixed transcript: {mixed_txt!r}"

    # Re-enable online AI and process transcribed speech through polish + translate
    vitral_target.set_pure_offline(False)
    ai_server.clear_request_log()
    polished = vitral_target.ai_inline("polish", short_txt, "zh")
    assert polished["output_text"].strip()
    translated = vitral_target.ai_inline("translate", mixed_txt, "en")
    assert translated["output_text"].strip()
    assert len(ai_server.request_log) >= 2


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_t3_f02_x_f13_f14_linux_fcitx5_and_tauri_shared_tokens_and_brand(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F02 x F13 x F14: Linux Fcitx5 & Tauri v2 share v1.1.0 Design Tokens & English-only Vitral brand."""
    fcitx_so, tauri_bin = vitral_target.require_linux()
    assert fcitx_so.read_bytes()[:4] == b"\x7fELF"
    assert tauri_bin.read_bytes()[:4] == b"\x7fELF"

    root = discovered_targets.vitral_root
    hpp = (root / "linux" / "fcitx5-vitral" / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")
    css = (root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.css").read_text(encoding="utf-8")
    tokens_ts = (root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.ts").read_text(encoding="utf-8")

    assert f"kBackdropStandardPx = {design_tokens['blur']['backdropStandardPx']};" in hpp
    assert f"kDesktopCandidateWindowPx = {design_tokens['radius']['desktopCandidateWindowPx']};" in hpp
    assert f"kDesktopStatusCapsulePx = {design_tokens['radius']['desktopStatusCapsulePx']};" in hpp
    assert f"kDesktopCandidatePageSize = {design_tokens['ergonomics']['desktopCandidatePageSize']};" in hpp
    assert "kToolbarTextLabelsAllowed = false;" in hpp
    assert "--vitral-blur-standard: 18px;" in css
    assert "--vitral-idle-opacity: 0.35;" in css
    assert '"candidateFontSizeRangePx": [\n      13,\n      22\n    ]' in tokens_ts

    fcitx_files, fcitx_src = _read_non_generated_sources(
        root / "linux" / "fcitx5-vitral" / "src", (".cpp", ".cc", ".h", ".hpp")
    )
    tauri_files, tauri_src = _read_non_generated_sources(
        root / "apps" / "desktop-tauri" / "src", (".ts", ".tsx", ".js", ".jsx", ".css", ".html")
    )
    assert len(fcitx_files) >= 1 and len(tauri_files) >= 1
    assert "vitral_tokens.hpp" in fcitx_src
    assert "tokens.css" in tauri_src or "tokens.ts" in tauri_src or "--vitral-" in tauri_src
    assert "live_slot.enc" not in (fcitx_src + tauri_src)
    assert "PureIME" not in (fcitx_src + tauri_src)
    assert_no_toolbar_text_labels([
        {"id": item, "showLabel": False, "strokeWidthPx": 1.75}
        for item in design_tokens["ergonomics"]["desktopInitialToolbarItems"]
    ])


@pytest.mark.requires_android
@pytest.mark.tokens
def test_t3_f02_x_f15_f16_android_keyboard_handwriting_tokens_and_apk(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F02 x F15 x F16: Android 120Hz keyboard & dual-mode handwriting canvas consume VitralTokens.kt."""
    apk_path = vitral_target.require_android_apk()
    vitral_target.require_android_jni()

    root = discovered_targets.vitral_root
    kt_tokens = (
        root / "apps" / "android" / "app" / "src" / "main" / "java" / "dev" / "vitral" / "ime" / "theme" / "VitralTokens.kt"
    ).read_text(encoding="utf-8")
    assert "mobileDockedScreenRatio: Float = 0.26f" in kt_tokens
    assert "floatingMinWidthDp: Int = 220" in kt_tokens
    assert "floatingMinHeightDp: Int = 165" in kt_tokens
    assert "backdropFloatingWindowPx: Int = 20" in kt_tokens
    assert "mobileKeycapPx: Int = 10" in kt_tokens
    assert "superimposedInkFadeOutMs: Int = 420" in kt_tokens
    assert "retroactiveHighlightDurationMs: Int = 600" in kt_tokens
    assert "strokeWidthPx: Float = 3.50f" in kt_tokens
    assert "flypyRows: Int = 4" in kt_tokens
    assert "quanpinRows: Int = 5" in kt_tokens
    assert "spacebarVoiceHoldThresholdMs: Int = 350" in kt_tokens
    assert "toolbarTextLabelsAllowed: Boolean = false" in kt_tokens
    assert design_tokens["ergonomics"]["mobileFloatingWindow"]["minWidthDp"] == 220

    android_files, android_src = _read_non_generated_sources(
        root / "apps" / "android" / "app" / "src", (".kt", ".java"), exclude_names={"VitralTokens.kt"}
    )
    assert len(android_files) >= 1
    assert "VitralTokens" in android_src
    assert "mobileDockedScreenRatio" in android_src
    assert "floatingMinWidth" in android_src and "floatingMinHeight" in android_src
    assert "superimposedInkFadeOutMs" in android_src and "retroactiveHighlightDurationMs" in android_src
    assert "live_slot.enc" not in android_src

    with zipfile.ZipFile(apk_path, "r") as zf:
        names = zf.namelist()
        assert "AndroidManifest.xml" in names
        assert any(n.endswith("libvitral_jni.so") for n in names)


@pytest.mark.requires_windows
@pytest.mark.tokens
def test_t3_f02_x_f17_f18_windows_tsf_and_tauri_shared_tokens_and_pe(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F02 x F17 x F18: Windows TSF DLL & Tauri exe share v1.1.0 tokens and clean PE32+ artifacts."""
    tsf_dll, tauri_exe = vitral_target.require_windows()
    root = discovered_targets.vitral_root

    linux_hpp = (root / "linux" / "fcitx5-vitral" / "include" / "vitral_tokens.hpp").read_bytes()
    win_hpp = (root / "crates" / "vitral-tsf" / "include" / "vitral_tokens.hpp").read_bytes()
    assert linux_hpp == win_hpp

    tsf_bytes = tsf_dll.read_bytes()
    exe_bytes = tauri_exe.read_bytes()
    m_dll, ch_dll, magic_dll, _, _ = _parse_pe_headers(tsf_bytes)
    m_exe, ch_exe, magic_exe, _, _ = _parse_pe_headers(exe_bytes)
    assert m_dll == 0x8664 and magic_dll == 0x20B and (ch_dll & 0x2000)
    assert m_exe == 0x8664 and magic_exe == 0x20B and not (ch_exe & 0x2000)

    for imp in _pe_imports(tsf_bytes) + _pe_imports(exe_bytes):
        assert ".so" not in imp.lower()
    assert b"Vitral" in tsf_bytes and b"Vitral" in exe_bytes
    assert b"PureIME" not in tsf_bytes and b"PureIME" not in exe_bytes
    assert design_tokens["radius"]["desktopCandidateWindowPx"] == 16


@pytest.mark.requires_core
@pytest.mark.requires_android
def test_t3_f01_x_f04_toolchain_ffi_jni_abi_symbol_parity(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F01 x F04: Cross-target toolchains produce ABI-v1 C-ABI (40 symbols) & JNI (38 camelCase methods)."""
    header_path = vitral_target.require_ffi_header()
    header = header_path.read_text(encoding="utf-8")
    assert "#define VITRAL_ABI_VERSION 1" in header

    cdll = vitral_target.load_ffi_cdll()
    cdll.vitral_abi_version.restype = ctypes.c_uint32
    assert cdll.vitral_abi_version() == 1

    m = re.search(r'extern "C" \{(.*?)#ifdef __cplusplus', header, re.S)
    assert m is not None
    declared_ffi = re.findall(r"\b(vitral_\w+)\s*\(", m.group(1))
    assert len(declared_ffi) == 40
    for sym in declared_ffi:
        assert hasattr(cdll, sym), f"Missing C-ABI symbol {sym} in libvitral_ffi.so"

    jni_libs = vitral_target.require_android_jni()
    nm_bin = shutil.which("nm") or shutil.which("llvm-nm")
    assert nm_bin is not None
    jni_rs = (discovered_targets.vitral_root / "crates" / "vitral-jni" / "src" / "lib.rs").read_text(encoding="utf-8")
    expected_jni = set(re.findall(r'pub extern "system" fn (Java_dev_vitral_ime_core_VitralNative_\w+)', jni_rs))
    assert len(expected_jni) == 38

    for lib_path in jni_libs:
        proc = subprocess.run(
            [nm_bin, "-D", "--defined-only", str(lib_path)],
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        )
        exported = {line.split()[-1] for line in proc.stdout.splitlines() if line.strip()}
        assert not (expected_jni - exported), f"Missing JNI exports in {lib_path}"


@pytest.mark.requires_core
def test_t3_f03_x_f05_asset_manifest_checksums_and_core_reload_assets(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F03 x F05: Verified staged assets (14 entries across 6 groups) feed vitral_reload_assets & both schemas."""
    fetch_script = vitral_target.require_fetch_assets_script()
    proc = subprocess.run(
        ["bash", str(fetch_script), "--verify"],
        cwd=discovered_targets.vitral_root,
        capture_output=True,
        text=True,
        timeout=25,
        check=False,
    )
    assert proc.returncode == 0, f"fetch-assets.sh --verify failed: {proc.stderr}\n{proc.stdout}"
    assert "verified cleanly" in proc.stdout

    vitral_target.init({"schema": "rime_ice"})
    rep = vitral_target.reload_assets()
    assert rep.get("reloaded") is True

    assert "你好" in _candidate_texts(_type_keys(vitral_target, "nihao"))
    vitral_target.set_schema("double_pinyin_flypy")
    assert "你好" in _candidate_texts(_type_keys(vitral_target, "nihc"))


@pytest.mark.requires_core
def test_t3_f06_x_f07_long_context_quanpin_with_polyphone_txt_lexicon(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """F06 x F07: Un-annotated polyphone .txt lexicon integrates without degrading long-context Quanpin decoding."""
    fx_dir = _fixtures_dir(discovered_targets)
    poly_txt = fx_dir / "lexicons" / "polyphone_unannotated.txt"
    oracle = json.loads((fx_dir / "lexicons" / "expected_polyphones.json").read_text(encoding="utf-8"))
    corpus = json.loads((fx_dir / "quanpin" / "corpus.json").read_text(encoding="utf-8"))

    vitral_target.init({"schema": "rime_ice"})
    rep = vitral_target.import_lexicon(poly_txt, "txt")
    assert int(rep.get("imported_count", 0)) == len(oracle["cases"])
    assert int(rep.get("skipped_count", 0)) == 0

    # Long-context Quanpin cases remain 100% accurate
    for case in [c for c in corpus["cases"] if c["length_bucket"] == "long"]:
        vitral_target.process_key("Escape")
        cands = _candidate_texts(_type_keys(vitral_target, case["pinyin"]))
        assert cands and cands[0] == case["expected_text"], (
            f"Long Quanpin {case['id']} top1={cands[0] if cands else None!r} != {case['expected_text']!r}"
        )

    # All polyphone phrases recall under expected context reading
    for case in oracle["cases"]:
        vitral_target.process_key("Escape")
        py = case["expected_full_pinyin"].replace(" ", "")
        cands = _candidate_texts(_type_keys(vitral_target, py))
        assert case["text"] in cands, f"Missing {case['text']} under {py!r}: {cands}"


@pytest.mark.requires_core
def test_t3_f09_x_f19_mode2_tier3_byok_disambiguation_vs_entropy_lock(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    ai_server: AIMockServer,
) -> None:
    """F09 x F19: Tier-3 BYOK slot disambiguator resolves ambiguous slots while respecting visual-entropy locks."""
    fx_dir = _fixtures_dir(discovered_targets)
    scel_path = fx_dir / "lexicons" / "sample_hotwords.scel"
    fx_ambig = json.loads((fx_dir / "handwriting" / "mode2_baoguang_laodeng.json").read_text(encoding="utf-8"))
    fx_locked = json.loads((fx_dir / "handwriting" / "mode2_clear_locked.json").read_text(encoding="utf-8"))

    ai_server.set_slot_choice(1)
    ai_server.clear_request_log()
    vitral_target.init({
        "ai_endpoint": ai_server.openai_url,
        "ai_tier3_enabled": True,
        "enable_tier3_ai": True,
    })
    vitral_target.import_lexicon(scel_path, "scel")

    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx_ambig["strokes"])
    ambig_res = vitral_target.hw_end()
    assert ambig_res["composing_text"] == "爆光老登金币"
    assert len(ai_server.request_log) >= 1

    # High-confidence locked slot must never be mutated by BYOK choice
    vitral_target.hw_clear()
    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx_locked["strokes"])
    locked_res = vitral_target.hw_end()
    assert locked_res["slots"][0]["locked_by_entropy"] is True
    assert float(locked_res["slots"][0]["margin_delta_p"]) >= 0.15
    assert locked_res["slots"][0]["char"] == "曝"
    assert locked_res["composing_text"] == "曝光老登金币"


@pytest.mark.requires_core
def test_t3_f10_x_f11_mru_device_sorting_with_localsend_and_webdav_fallback(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    localsend_server: LocalSendMockPeer,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    """F10 x F11: MRU device sorting prioritizes LAN > WAN > Offline and supports LocalSend + WebDAV /relay/."""
    sender = vitral_target
    wan_receiver = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "wan_receiver")
    try:
        sender.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "device_id": "sender-main",
            "known_devices": [
                {
                    "device_id": "dev-offline",
                    "device_name": "Offline Box",
                    "tier": "offline",
                    "usage_count": 90,
                    "last_used_ms": 9000,
                },
                {
                    "device_id": "wan-peer",
                    "device_name": "WAN Peer",
                    "tier": "wan",
                    "usage_count": 40,
                    "last_used_ms": 8000,
                },
                {
                    "device_id": localsend_server.fingerprint,
                    "device_name": localsend_server.alias,
                    "tier": "lan",
                    "usage_count": 5,
                    "last_used_ms": 5000,
                    "localsend_url": localsend_server.base_url,
                },
            ],
        })
        wan_receiver.init({"koofr_endpoint": webdav_server.koofr_endpoint, "device_id": "wan-peer"})

        mru = sender.relay_devices_mru()
        ids = [d["device_id"] for d in mru]
        assert ids == [localsend_server.fingerprint, "wan-peer", "dev-offline"]

        # LAN peer transfer hits LocalSend v2
        lan_tx = sender.relay_send(localsend_server.fingerprint, "lan-p2p-message")
        assert lan_tx.get("status") in ("ok", "sent", "delivered", "queued")
        assert len(localsend_server.received_transfers) == 1
        assert localsend_server.received_transfers[0]["data"].decode("utf-8") == "lan-p2p-message"

        # WAN peer transfer stages in Koofr /relay/ and deletes immediately on poll
        sender.relay_send("wan-peer", "wan-relay-message")
        assert any(p.endswith(".enc") for p in webdav_server.list_files("relay/"))
        polled = wan_receiver.relay_poll_inbox()
        assert len(polled) == 1
        val = polled[0] if isinstance(polled[0], str) else polled[0].get("content", polled[0].get("text"))
        assert val == "wan-relay-message"
        assert webdav_server.list_files("relay/") == []
        webdav_server.assert_no_live_slot()
    finally:
        wan_receiver.close()


@pytest.mark.requires_core
@pytest.mark.tokens
def test_t3_f02_x_f09_handwriting_token_invariants_and_mode2_lattice(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F02 x F09: Handwriting tokens (topK=8, margin=0.15, 420ms, 600ms, Xiaohe lookup) match core payloads."""
    hw_tokens = design_tokens["handwriting"]
    assert hw_tokens["confusionTopK"] == 8
    assert float(hw_tokens["ambiguousMarginThreshold"]) == pytest.approx(0.15)
    assert hw_tokens["superimposedInkFadeOutMs"] == 420
    assert hw_tokens["retroactiveHighlightDurationMs"] == 600
    assert hw_tokens["showXiaoheCodeInLookupMode"] is True

    fx_dir = _fixtures_dir(discovered_targets)
    fx_ambig = json.loads((fx_dir / "handwriting" / "mode2_baoguang_laodeng.json").read_text(encoding="utf-8"))
    fx_clear = json.loads((fx_dir / "handwriting" / "mode2_clear_locked.json").read_text(encoding="utf-8"))
    fx_m1 = json.loads((fx_dir / "handwriting" / "mode1_mao.json").read_text(encoding="utf-8"))

    vitral_target.init({})
    vitral_target.hw_begin("PhoneticLookup")
    _replay_strokes(vitral_target, fx_m1["strokes"])
    m1_res = vitral_target.hw_end()
    mao = next(c for c in m1_res["candidates"] if c["char"] == "耄")
    assert mao["xiaohe_code"] == "mc"
    vitral_target.hw_clear()

    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx_ambig["strokes"])
    ambig_res = vitral_target.hw_end()
    slot0_ambig = ambig_res["slots"][0]
    assert 1 <= len(slot0_ambig["top8"]) <= hw_tokens["confusionTopK"]
    assert float(slot0_ambig["margin_delta_p"]) < float(hw_tokens["ambiguousMarginThreshold"])
    assert slot0_ambig["locked_by_entropy"] is False
    vitral_target.hw_clear()

    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx_clear["strokes"])
    clear_res = vitral_target.hw_end()
    slot0_clear = clear_res["slots"][0]
    assert float(slot0_clear["margin_delta_p"]) >= float(hw_tokens["ambiguousMarginThreshold"])
    assert slot0_clear["locked_by_entropy"] is True


@pytest.mark.requires_core
def test_t3_f11_x_f19_crdt_sync_backup_and_full_device_restore_integrity(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    """F11 x F19: Koofr 3-tier sync persists lexicon + pinned clipboard ring and restores cleanly onto new device."""
    scel_path = _fixtures_dir(discovered_targets) / "lexicons" / "sample_hotwords.scel"
    dev_a = vitral_target
    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b_crdt_restore")
    try:
        dev_a.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-crdt-restore-test-key-32b",
            "device_id": "dev-a-sync",
        })
        dev_a.import_lexicon(scel_path, "scel")
        pushed = dev_a.clipboard_push("crdt-restored-pin")
        pid = _extract_pushed_id(pushed, "crdt-restored-pin")
        dev_a.clipboard_pin(pid, "crp")
        dev_a.reload_assets()

        files = webdav_server.list_files()
        assert any(p.startswith(("lexicon/", "profile/")) for p in files)
        assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")
        webdav_server.assert_no_live_slot()

        dev_b.init({"device_id": "dev-b-restore"})
        restore_res = dev_b.restore_from_koofr({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-crdt-restore-test-key-32b",
        })
        assert restore_res.get("restored") is True
        assert int(restore_res.get("snapshots_applied", 0)) >= 1

        ring_b = [e["content"] for e in dev_b.clipboard_state()["ring"]]
        assert "crdt-restored-pin" in ring_b
        assert "crdt-restored-pin" in _candidate_texts(_type_keys(dev_b, "crp"))[0]
        dev_b.process_key("Escape")
        dev_b.set_schema("rime_ice")
        assert "老登金币" in _candidate_texts(_type_keys(dev_b, "laodengjinbi"))
        webdav_server.assert_no_live_slot()
    finally:
        dev_b.close()


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_t3_f13_x_f14_linux_fcitx5_and_tauri_shared_config_and_zero_bloat(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F13 x F14: Linux Fcitx5 addon & Tauri management center share config keys and reject bloat/live_slot."""
    assert_english_only_brand(design_tokens["brand"]["name"], strict_ascii_only=True)
    fcitx_so, tauri_bin = vitral_target.require_linux()
    nm_bin = shutil.which("nm") or shutil.which("llvm-nm")
    assert nm_bin is not None
    nm_out = subprocess.run([nm_bin, "-D", str(fcitx_so)], capture_output=True, text=True, check=True).stdout
    assert "fcitx" in nm_out.lower()
    assert tauri_bin.is_file() and tauri_bin.stat().st_size > 0

    root = discovered_targets.vitral_root
    _, fcitx_src = _read_non_generated_sources(root / "linux" / "fcitx5-vitral" / "src", (".cpp", ".cc", ".h", ".hpp"))
    _, tauri_ui = _read_non_generated_sources(
        root / "apps" / "desktop-tauri" / "src", (".ts", ".tsx", ".js", ".jsx", ".css", ".html")
    )
    _, tauri_rs = _read_non_generated_sources(root / "apps" / "desktop-tauri" / "src-tauri" / "src", (".rs",))
    combined = fcitx_src + "\n" + tauri_ui + "\n" + tauri_rs
    combined_lower = combined.lower()

    for forbidden in ("斗图", "书影音", "花体字", "问 ai", "问ai", "ask-ai", "live_slot.enc", "pureime"):
        assert forbidden not in combined_lower, f"Forbidden string {forbidden!r} in Linux sources"
    assert "koofr" in combined_lower or "webdav" in combined_lower
    assert "pure_offline" in tauri_rs or "pureoffline" in combined_lower


@pytest.mark.requires_android
@pytest.mark.tokens
def test_t3_f15_x_f16_android_1_4_layout_and_handwriting_canvas_integration(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F15 x F16: Android 1/4 docked/floating keyboard integrates seamlessly with dual-mode HW & Compose settings."""
    vitral_target.require_android_apk()
    root = discovered_targets.vitral_root
    kt_files, kt_src = _read_non_generated_sources(
        root / "apps" / "android" / "app" / "src", (".kt", ".java"), exclude_names={"VitralTokens.kt"}
    )
    assert len(kt_files) >= 1
    assert "flypyRows" in kt_src and "quanpinRows" in kt_src
    assert "123" in kt_src
    assert "hwOverrideSlot" in kt_src or "overrideSlot" in kt_src
    assert "quickPaste" in kt_src or "consumeQuickPaste" in kt_src
    assert "13" in kt_src and "22" in kt_src
    assert float(design_tokens["ergonomics"]["mobileDockedScreenRatio"]) == pytest.approx(0.26)
    assert design_tokens["ergonomics"]["mobileTopHierarchy"] == ["icon-toolbar", "candidate-bar", "key-grid"]


@pytest.mark.tokens
def test_t3_f02_x_f20_research_docs_and_english_brand_across_platforms(
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """F02 x F20: Research notes, gen-tokens.py --check, & English-only Vitral brand hold across codebase."""
    root = discovered_targets.vitral_root
    assert_english_only_brand(design_tokens["brand"]["name"], strict_ascii_only=True)

    gen_script = discovered_targets.gen_tokens_script
    assert gen_script is not None and gen_script.is_file()
    proc = subprocess.run(
        [sys.executable, str(gen_script), "--check", "--root", str(root)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"Token drift detected:\n{proc.stderr}\n{proc.stdout}"

    research_dir = root / "docs" / "research"
    docs = sorted(research_dir.glob("*.md"))
    assert len(docs) >= 2
    docs_lower = "\n".join(d.read_text(encoding="utf-8", errors="replace").lower() for d in docs)
    for topic in ("quanpin", "handwriting", "frecency", "webdav"):
        assert topic in docs_lower

    exts = {".rs", ".ts", ".tsx", ".js", ".jsx", ".css", ".html", ".kt", ".java", ".cpp", ".hpp", ".h", ".json"}
    for base in (root / "crates", root / "apps", root / "linux", root / "tokens"):
        if not base.is_dir():
            continue
        for p in base.rglob("*"):
            if not p.is_file() or p.suffix.lower() not in exts:
                continue
            rel_parts = p.relative_to(base).parts
            if any(part in ("target", "node_modules", "build", "tests") for part in rel_parts):
                continue
            txt = p.read_text(encoding="utf-8", errors="replace")
            if p.suffix.lower() == ".rs":
                txt = _strip_rust_cfg_test_blocks(txt)
            assert "pureime" not in txt.lower(), f"Legacy PureIME found in {p}"
            assert not re.search(r"Vitral\s*[\u4e00-\u9fff]*输入法", txt), f"Chinese brand alias in {p}"
