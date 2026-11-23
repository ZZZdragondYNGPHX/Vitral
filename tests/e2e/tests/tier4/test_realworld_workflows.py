"""Tier 4 real-world multi-step end-to-end workflow tests for Vitral."""

from __future__ import annotations

import contextlib
import json
import re
import shutil
import struct
import subprocess
import sys
import time
import wave
import zipfile
from pathlib import Path
from typing import Any

import pytest

from harnesses import AIMockServer, LocalSendMockPeer, WebDAVMockServer
from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets
from vitral_e2e.tokens import assert_all_token_invariants, assert_english_only_brand, assert_no_toolbar_text_labels

pytestmark = pytest.mark.tier4


# ----------------------------------------------------------------------
# Local Workflow Helpers (Opaque-Box Only)
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
    assert entry is not None, f"Missing pushed item {expected_content!r} in ring: {ring}"
    assert entry.get("id"), f"ClipboardEntry missing 'id': {entry}"
    return str(entry["id"])


def _qp_content(qp_state: dict[str, Any]) -> str | None:
    assert "quick_paste" in qp_state, f"Missing 'quick_paste' key in {qp_state}"
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


def _rva_to_off(rva: int, sections: list[tuple[int, int, int]]) -> int | None:
    for va, span, raw_ptr in sections:
        if va <= rva < va + span:
            return raw_ptr + (rva - va)
    return None


def _read_cstr(data: bytes, off: int) -> str:
    end = data.index(b"\x00", off)
    return data[off:end].decode("ascii", errors="replace")


def _pe_exports(data: bytes) -> list[str]:
    _, _, opt_magic, sections, opt_off = _parse_pe_headers(data)
    assert opt_magic == 0x20B
    export_rva, export_size = struct.unpack_from("<II", data, opt_off + 112)
    if export_rva == 0 or export_size == 0:
        return []
    exp_off = _rva_to_off(export_rva, sections)
    assert exp_off is not None
    num_names = struct.unpack_from("<I", data, exp_off + 24)[0]
    names_rva = struct.unpack_from("<I", data, exp_off + 32)[0]
    names_off = _rva_to_off(names_rva, sections)
    assert names_off is not None
    out: list[str] = []
    for i in range(num_names):
        name_rva = struct.unpack_from("<I", data, names_off + i * 4)[0]
        name_off = _rva_to_off(name_rva, sections)
        if name_off is not None:
            out.append(_read_cstr(data, name_off))
    return out


def _pe_imports(data: bytes) -> list[str]:
    _, _, opt_magic, sections, opt_off = _parse_pe_headers(data)
    assert opt_magic == 0x20B
    import_rva, import_size = struct.unpack_from("<II", data, opt_off + 120)
    if import_rva == 0 or import_size == 0:
        return []
    imp_off = _rva_to_off(import_rva, sections)
    assert imp_off is not None
    out: list[str] = []
    while True:
        _, _, _, name_rva, first_thunk = struct.unpack_from("<IIIII", data, imp_off)
        if name_rva == 0 and first_thunk == 0:
            break
        name_off = _rva_to_off(name_rva, sections)
        if name_off is not None:
            out.append(_read_cstr(data, name_off))
        imp_off += 20
    return out


# ======================================================================
# Tier 4 Real-World Workflows (10 workflows)
# ======================================================================


@pytest.mark.requires_core
def test_w01_multi_length_quanpin_sentence_accuracy_progression(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """Workflow 1: Short -> medium -> long ambiguous Quanpin chains improve/maintain accuracy + research backing."""
    root = discovered_targets.vitral_root
    research_dir = root / "docs" / "research"
    assert research_dir.is_dir(), f"Missing docs/research: {research_dir}"
    research_docs = sorted(research_dir.glob("*.md"))
    assert len(research_docs) >= 2
    research_text = "\n".join(d.read_text(encoding="utf-8", errors="replace").lower() for d in research_docs)
    assert "quanpin" in research_text and any(k in research_text for k in ("viterbi", "beam", "lattice", "gram"))

    corpus_path = _fixtures_dir(discovered_targets) / "quanpin" / "corpus.json"
    corpus = json.loads(corpus_path.read_text(encoding="utf-8"))
    assert len(corpus["cases"]) == 18

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
            act = _type_keys(vitral_target, delta)
            final_cands = _candidate_texts(act)
            assert 1 <= len(final_cands) <= 5
            if step.get("resolved") and prefix == case["pinyin"]:
                assert final_cands[0] == step["resolved"]

        top1 = final_cands[0] if final_cands else ""
        bucket_hits[case["length_bucket"]].append(top1 == case["expected_text"])
        assert top1 == case["expected_text"], (
            f"Quanpin {case['id']} ({case['pinyin']}) top1={top1!r} != {case['expected_text']!r}"
        )

    short_acc = sum(bucket_hits["short"]) / len(bucket_hits["short"])
    medium_acc = sum(bucket_hits["medium"]) / len(bucket_hits["medium"])
    long_acc = sum(bucket_hits["long"]) / len(bucket_hits["long"])
    assert short_acc == 1.0 and medium_acc == 1.0 and long_acc == 1.0
    assert long_acc >= short_acc and medium_acc >= short_acc


@pytest.mark.requires_core
@pytest.mark.tokens
def test_w02_continuous_handwriting_baoguang_override_and_clear_lock(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """Workflow 2: Continuous Mode-2 曝光 -> 爆光老登金币 retroactive preedit, override lock, & p1>=0.95 lock."""
    hw_tokens = design_tokens["handwriting"]
    assert float(hw_tokens["ambiguousMarginThreshold"]) == pytest.approx(0.15)
    assert hw_tokens["confusionTopK"] == 8
    assert hw_tokens["retroactiveHighlightDurationMs"] == 600

    fx_dir = _fixtures_dir(discovered_targets)
    scel_path = fx_dir / "lexicons" / "sample_hotwords.scel"
    fx_ambig = json.loads((fx_dir / "handwriting" / "mode2_baoguang_laodeng.json").read_text(encoding="utf-8"))
    fx_clear = json.loads((fx_dir / "handwriting" / "mode2_clear_locked.json").read_text(encoding="utf-8"))

    vitral_target.init({})
    vitral_target.import_lexicon(scel_path, "scel")

    # Step 1: Progressive segment-by-segment Mode 2 input ("曝光" -> "爆光老登金币")
    vitral_target.hw_begin("ContinuousLattice")
    seg0_end = fx_ambig["char_segments"][0]["stroke_range"][1] + 1
    seg1_end = fx_ambig["char_segments"][1]["stroke_range"][1] + 1

    after_seg0 = _replay_strokes(vitral_target, fx_ambig["strokes"][:seg0_end])
    slot0_mid = after_seg0["slots"][0]
    assert slot0_mid["locked_by_entropy"] is False
    assert float(slot0_mid["margin_delta_p"]) < 0.15
    assert 1 <= len(slot0_mid["top8"]) <= 8

    after_seg1 = _replay_strokes(vitral_target, fx_ambig["strokes"][seg0_end:seg1_end])
    assert after_seg1["composing_text"] == "曝光"

    _replay_strokes(vitral_target, fx_ambig["strokes"][seg1_end:])
    final_res = vitral_target.hw_end()
    assert final_res["composing_text"] == "爆光老登金币"
    assert final_res["slots"][0]["char"] == "爆"
    assert final_res["slots"][0]["changed_retroactively"] is True

    # Step 2: Click-to-override manual lock on slot 0 ("曝")
    top8 = final_res["slots"][0]["top8"]
    pu_idx = next(
        (i for i, it in enumerate(top8) if (it["char"] if isinstance(it, dict) else str(it)) == "曝"),
        None,
    )
    assert pu_idx is not None, f"Expected '曝' in slot 0 top8: {top8}"
    overridden = vitral_target.hw_override_slot(0, pu_idx)
    assert overridden["composing_text"] == "曝光老登金币"
    ovr_slot0 = overridden["slots"][0]
    assert ovr_slot0["char"] == "曝"
    assert ovr_slot0.get("manually_locked") is True or ovr_slot0.get("locked_by_entropy") is True
    assert ovr_slot0.get("changed_retroactively") is False

    # Step 3: Clear high-confidence char (p1 >= 0.95) non-mutation invariant
    vitral_target.hw_clear()
    vitral_target.hw_begin("ContinuousLattice")
    _replay_strokes(vitral_target, fx_clear["strokes"])
    clear_res = vitral_target.hw_end()
    clear_slot0 = clear_res["slots"][0]
    assert clear_slot0["locked_by_entropy"] is True
    assert float(clear_slot0["margin_delta_p"]) >= 0.15
    assert clear_slot0["char"] == "曝"
    assert clear_slot0["changed_retroactively"] is False
    assert clear_res["composing_text"] == "曝光老登金币"


@pytest.mark.requires_core
def test_w03_offline_sensevoice_p2_recorder_summary_and_inline_ai(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    ai_server: AIMockServer,
) -> None:
    """Workflow 3: Offline SenseVoice-Small WAV clips -> P2 micro-capsule summary -> inline translate & polish."""
    audio_dir = _fixtures_dir(discovered_targets) / "audio"
    manifest = json.loads((audio_dir / "audio_manifest.json").read_text(encoding="utf-8"))
    clips = {c["id"]: c for c in manifest["clips"]}

    vitral_target.init({"ai_endpoint": ai_server.ollama_url, "ai_model": "qwen2.5:7b"})
    vitral_target.set_pure_offline(True)
    ai_server.clear_request_log()

    transcripts: dict[str, str] = {}
    for cid in ("mandarin_short", "mandarin_mixed_en", "long_20s"):
        clip = clips[cid]
        pcm = _read_wav_pcm(audio_dir / clip["file"])
        vitral_target.voice_start()
        for off in range(0, len(pcm), 32_000):
            vitral_target.voice_push_pcm(pcm[off : off + 32_000])
        res = vitral_target.voice_stop()
        txt = res["transcript"].strip()
        assert txt, f"Empty transcript for {cid}"
        assert any(kw.lower() in txt.lower() for kw in clip["expected_keywords"]), (
            f"Expected one of {clip['expected_keywords']} in {txt!r}"
        )
        transcripts[cid] = txt

    # Silence clip must not hallucinate
    silence_pcm = _read_wav_pcm(audio_dir / clips["silence_noise"]["file"])
    vitral_target.voice_start()
    vitral_target.voice_push_pcm(silence_pcm)
    sil_res = vitral_target.voice_stop()
    assert sil_res["transcript"].strip() == ""
    assert ai_server.request_log == []

    # Exit offline mode -> P2 micro-capsule summary + inline translate + spoken polish
    vitral_target.set_pure_offline(False)
    sum_res = vitral_target.ai_inline("summary", transcripts["long_20s"])
    assert "1." in sum_res["output_text"] and "2." in sum_res["output_text"]

    tr_res = vitral_target.ai_inline("translate", transcripts["mandarin_short"], "en")
    assert tr_res["output_text"].strip()

    pol_res = vitral_target.ai_inline("polish", "那个就是说今天下午三点然后开会啊", "zh")
    assert pol_res["output_text"] == "今天下午三点开会。"
    assert len(ai_server.request_log) >= 3


@pytest.mark.requires_core
def test_w04_koofr_webdav_sync_v12_quick_paste_relay_and_full_restore(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    """Workflow 4: Full Koofr WebDAV sync, all 5 v1.2 Quick Paste rules, N-ring, /relay/ + 24h sweep, & restore."""
    dev_a = vitral_target
    with contextlib.ExitStack() as stack:
        dev_b = stack.enter_context(
            contextlib.closing(VitralTarget(targets=discovered_targets, state_dir=tmp_path / "w04_dev_b"))
        )
        dev_c = stack.enter_context(
            contextlib.closing(VitralTarget(targets=discovered_targets, state_dir=tmp_path / "w04_dev_c_restore"))
        )
        cfg_a = {
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-w04-sync-key-32bytes-001",
            "device_id": "w04-dev-a",
            "clipboard_ring_capacity": 4,
        }
        cfg_b = {
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-w04-sync-key-32bytes-001",
            "device_id": "w04-dev-b",
            "clipboard_ring_capacity": 4,
        }
        dev_a.init(cfg_a)
        dev_b.init(cfg_b)

        # (1) Device A copy -> Device B overwrite
        p1 = dev_a.clipboard_push("qp-sync-alpha", source_device="w04-dev-a")
        alpha_id = _extract_pushed_id(p1, "qp-sync-alpha")
        dev_a.clipboard_pin(alpha_id, "alp")
        dev_b.clipboard_ingest_remote("qp-sync-alpha", is_cold_boot=False)
        assert _qp_content(dev_a.quick_paste_state()) == "qp-sync-alpha"
        assert _qp_content(dev_b.quick_paste_state()) == "qp-sync-alpha"

        # (2) Device A consume_quick_paste clears Device A ONLY; Device B STILL retains & pastes
        c_a = dev_a.consume_quick_paste()
        assert c_a["consumed"]["content"] == "qp-sync-alpha"
        assert _qp_content(dev_a.quick_paste_state()) is None
        assert _qp_content(dev_b.quick_paste_state()) == "qp-sync-alpha"
        c_b = dev_b.consume_quick_paste()
        assert c_b["consumed"]["content"] == "qp-sync-alpha"
        assert _qp_content(dev_b.quick_paste_state()) is None

        # (3) on_ime_open -> on_ime_close clears locally only
        dev_a.clipboard_push("qp-ime-cycle", source_device="w04-dev-a")
        dev_b.clipboard_ingest_remote("qp-ime-cycle", is_cold_boot=False)
        open_a = dev_a.on_ime_open()
        assert open_a["seen_while_ime_open"] is True
        assert _qp_content(open_a) == "qp-ime-cycle"
        dev_a.on_ime_close()
        assert _qp_content(dev_a.quick_paste_state()) is None
        dev_a.on_ime_open()
        assert _qp_content(dev_a.quick_paste_state()) is None
        assert _qp_content(dev_b.quick_paste_state()) == "qp-ime-cycle"

        # (4) Newer copy overwrites Quick Paste
        dev_a.clipboard_push("qp-overwrite-1", source_device="w04-dev-a")
        dev_a.clipboard_push("qp-overwrite-2", source_device="w04-dev-a")
        assert _qp_content(dev_a.quick_paste_state()) == "qp-overwrite-2"

        # (5) ZERO live_slot.enc on WebDAV while N-item history_ring.jsonl.enc persists + clear unpinned
        webdav_server.assert_no_live_slot()
        assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")
        cleared = dev_a.clipboard_clear_unpinned()
        assert [e["content"] for e in cleared["ring"]] == ["qp-sync-alpha"]

        # Koofr /relay/ SHA-256 + immediate DELETE
        dev_a.relay_send("w04-dev-b", "w04-relay-secret-sha256")
        assert any(p.endswith(".enc") for p in webdav_server.list_files("relay/"))
        polled = dev_b.relay_poll_inbox()
        assert len(polled) == 1
        item0 = polled[0] if isinstance(polled[0], str) else polled[0].get("content", polled[0].get("text"))
        assert item0 == "w04-relay-secret-sha256"
        assert webdav_server.list_files("relay/") == []

        # 24h orphan sweep using set_mtime (no wall-clock sleep)
        now = time.time()
        webdav_server.write_file_bytes("relay/fresh_23h59m.enc", b"keep")
        webdav_server.write_file_bytes("relay/fresh_23h59m.meta.json", b"{}")
        webdav_server.set_mtime("relay/fresh_23h59m.enc", now - (23 * 3600 + 59 * 60))
        webdav_server.set_mtime("relay/fresh_23h59m.meta.json", now - (23 * 3600 + 59 * 60))

        webdav_server.write_file_bytes("relay/stale_24h01m.enc", b"purge")
        webdav_server.write_file_bytes("relay/stale_24h01m.meta.json", b"{}")
        webdav_server.set_mtime("relay/stale_24h01m.enc", now - (24 * 3600 + 60))
        webdav_server.set_mtime("relay/stale_24h01m.meta.json", now - (24 * 3600 + 60))

        dev_a.relay_cleanup_orphans()
        assert webdav_server.file_exists("relay/fresh_23h59m.enc")
        assert not webdav_server.file_exists("relay/stale_24h01m.enc")

        # Full device restore onto Device C
        dev_c.init({"device_id": "w04-dev-c"})
        res_restore = dev_c.restore_from_koofr({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-w04-sync-key-32bytes-001",
        })
        assert res_restore.get("restored") is True
        ring_c = [e["content"] for e in dev_c.clipboard_state()["ring"]]
        assert "qp-sync-alpha" in ring_c
        webdav_server.assert_no_live_slot()


@pytest.mark.requires_core
def test_w05_lan_localsend_v2_p2p_transfer_and_mru_reordering(
    vitral_target: VitralTarget,
    localsend_server: LocalSendMockPeer,
    tmp_path: Path,
) -> None:
    """Workflow 5: LAN LocalSend v2 P2P text + file transfer and MRU device priority reordering."""
    vitral_target.init({
        "known_devices": [
            {
                "device_id": "offline-node",
                "device_name": "Offline Node",
                "tier": "offline",
                "usage_count": 99,
                "last_used_ms": 9999,
            },
            {
                "device_id": "wan-node",
                "device_name": "WAN Node",
                "tier": "wan",
                "usage_count": 50,
                "last_used_ms": 9000,
            },
            {
                "device_id": "lan-rival",
                "device_name": "LAN Rival",
                "tier": "lan",
                "usage_count": 2,
                "last_used_ms": 2000,
            },
            {
                "device_id": localsend_server.fingerprint,
                "device_name": localsend_server.alias,
                "tier": "lan",
                "usage_count": 1,
                "last_used_ms": 1000,
                "localsend_url": localsend_server.base_url,
            },
        ]
    })

    initial_mru = [d["device_id"] for d in vitral_target.relay_devices_mru()]
    assert initial_mru == ["lan-rival", localsend_server.fingerprint, "wan-node", "offline-node"]

    # Text transfer via LocalSend v2
    tx_text = vitral_target.relay_send(localsend_server.fingerprint, "Vitral P2P text transfer")
    assert tx_text.get("status") in ("ok", "sent", "delivered", "queued")

    # File transfer via LocalSend v2
    sample_file = tmp_path / "vitral_p2p_note.txt"
    sample_bytes = b"Vitral LAN LocalSend v2 file payload\nline2"
    sample_file.write_bytes(sample_bytes)
    tx_file = vitral_target.relay_send(localsend_server.fingerprint, str(sample_file))
    assert tx_file.get("status") in ("ok", "sent", "delivered", "queued")

    assert len(localsend_server.received_transfers) == 2
    assert localsend_server.received_transfers[0]["data"].decode("utf-8") == "Vitral P2P text transfer"
    assert localsend_server.received_transfers[1]["data"] in (sample_bytes, str(sample_file).encode("utf-8"))
    for rec in localsend_server.received_transfers:
        assert rec["sha256_actual"]

    # After 2 transfers, localsend_server has higher usage_count & newer last_used_ms -> climbs to rank 0
    updated_mru = [d["device_id"] for d in vitral_target.relay_devices_mru()]
    assert updated_mru[0] == localsend_server.fingerprint
    assert updated_mru.index("lan-rival") < updated_mru.index("wan-node") < updated_mru.index("offline-node")


@pytest.mark.requires_core
@pytest.mark.requires_linux
@pytest.mark.requires_android
@pytest.mark.requires_windows
def test_w06_cross_platform_build_artifacts_and_asset_checksums(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    """Workflow 6: Linux Fcitx5+Tauri, Android APK+JNI, Windows TSF DLL+Tauri exe, & asset SHA-256 verification."""
    # Core & FFI
    vitral_target.require_core()
    header_text = vitral_target.require_ffi_header().read_text(encoding="utf-8")
    assert "#define VITRAL_ABI_VERSION 1" in header_text

    # Linux Fcitx5 + Tauri
    fcitx_so, tauri_linux = vitral_target.require_linux()
    assert fcitx_so.read_bytes()[:4] == b"\x7fELF"
    assert tauri_linux.read_bytes()[:4] == b"\x7fELF"

    # Android APK + JNI
    apk_path = vitral_target.require_android_apk()
    jni_libs = vitral_target.require_android_jni()
    assert len(jni_libs) >= 1
    with zipfile.ZipFile(apk_path, "r") as zf:
        apk_names = set(zf.namelist())
        assert "AndroidManifest.xml" in apk_names
        assert any(n.endswith(".dex") for n in apk_names)
        assert any(n.endswith("libvitral_jni.so") for n in apk_names)

    # Windows TSF DLL + Tauri exe (PE32+ x86_64)
    tsf_dll, tauri_win = vitral_target.require_windows()
    dll_bytes = tsf_dll.read_bytes()
    exe_bytes = tauri_win.read_bytes()
    m_d, c_d, opt_d, _, _ = _parse_pe_headers(dll_bytes)
    m_e, c_e, opt_e, _, _ = _parse_pe_headers(exe_bytes)
    assert m_d == 0x8664 and opt_d == 0x20B and (c_d & 0x2000)
    assert m_e == 0x8664 and opt_e == 0x20B and not (c_e & 0x2000)
    assert {"DllGetClassObject", "DllCanUnloadNow", "DllRegisterServer", "DllUnregisterServer"} <= set(
        _pe_exports(dll_bytes)
    )
    for imp in _pe_imports(dll_bytes) + _pe_imports(exe_bytes):
        assert ".so" not in imp.lower()

    # Asset manifest + fetch-assets.sh --verify
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
    expected_count = len(
        json.loads(
            (discovered_targets.vitral_root / "scripts" / "assets-manifest.json").read_text(encoding="utf-8")
        )["assets"]
    )
    assert f"All {expected_count} asset entries verified cleanly" in proc.stdout


@pytest.mark.tokens
@pytest.mark.requires_linux
@pytest.mark.requires_android
@pytest.mark.requires_windows
def test_w07_cross_platform_design_token_v11_conformance(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """Workflow 7: Cross-platform Design Token v1.1.0 conformance across Linux, Android, Windows, & Tauri."""
    assert_all_token_invariants(design_tokens)
    assert_english_only_brand(design_tokens["brand"]["name"], strict_ascii_only=True)
    assert_no_toolbar_text_labels([
        {"id": item, "showLabel": False, "strokeWidthPx": 1.75}
        for item in design_tokens["ergonomics"]["desktopInitialToolbarItems"]
    ])

    vitral_target.require_linux()
    vitral_target.require_android_apk()
    vitral_target.require_windows()

    root = discovered_targets.vitral_root
    gen_script = discovered_targets.gen_tokens_script
    assert gen_script is not None and gen_script.is_file()
    proc = subprocess.run(
        [sys.executable, str(gen_script), "--check", "--root", str(root)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"gen-tokens.py --check failed: {proc.stderr}\n{proc.stdout}"

    # Verify non-generated platform sources consume generated tokens and avoid draft literals / bloat
    _, fcitx_src = _read_non_generated_sources(root / "linux" / "fcitx5-vitral" / "src", (".cpp", ".cc", ".h", ".hpp"))
    _, tauri_ui = _read_non_generated_sources(
        root / "apps" / "desktop-tauri" / "src", (".ts", ".tsx", ".js", ".jsx", ".css", ".html")
    )
    _, android_src = _read_non_generated_sources(
        root / "apps" / "android" / "app" / "src", (".kt", ".java"), exclude_names={"VitralTokens.kt"}
    )
    _, win_src = _read_non_generated_sources(root / "crates" / "vitral-tsf" / "src", (".rs",))

    assert "vitral_tokens.hpp" in fcitx_src
    assert "tokens.css" in tauri_ui or "tokens.ts" in tauri_ui or "--vitral-" in tauri_ui
    assert "VitralTokens" in android_src
    assert "vitral_tokens" in win_src or "TOKENS" in win_src

    for bad_draft in ("900ms", "0.32f", "230.dp", "175.dp", 'max="19"', "max={19}", "live_slot.enc", "PureIME"):
        assert bad_draft not in fcitx_src
        assert bad_draft not in tauri_ui
        assert bad_draft not in android_src
        assert bad_draft not in win_src
    for src_name, src in (
        ("fcitx_src", fcitx_src),
        ("tauri_ui", tauri_ui),
        ("android_src", android_src),
        ("win_src", win_src),
    ):
        assert not re.search(r"(?<![\d.])0\.32f?\b", src), f"Draft opacity 0.32/0.32f found in {src_name}"


@pytest.mark.requires_core
def test_w08_ide_to_chat_app_context_mixed_cn_en_punct_macros_and_1s_pin_erase(
    vitral_target: VitralTarget,
) -> None:
    """Workflow 8: IDE (code/alacritty) -> Chat (telegram/wechat) per-app CN/EN, punct, rq/sj macros, & 1s pin/erase."""
    vitral_target.init({"schema": "rime_ice"})

    # IDE & terminal default to EN
    assert str(vitral_target.on_app_focus("code").get("lang_mode", "")).lower() == "en"
    assert str(vitral_target.on_app_focus("alacritty").get("lang_mode", "")).lower() == "en"

    # Chat defaults to ZH
    assert str(vitral_target.on_app_focus("org.telegram.desktop").get("lang_mode", "")).lower() == "zh"

    # Return to code remembers EN
    code_again = vitral_target.on_app_focus("code")
    assert str(code_again.get("lang_mode", "")).lower() == "en"

    # Switch to WeChat (ZH) -> paired punctuation + rq/sj macros + 1s pin/erase
    assert str(vitral_target.on_app_focus("wechat").get("lang_mode", "")).lower() == "zh"

    punct = vitral_target.process_key("(")
    paired = str(punct.get("paired_punctuation") or punct.get("commit_text") or _preedit_text(punct))
    assert "()" in paired or "（）" in paired
    assert int(punct.get("cursor_delta", -1)) <= 0

    vitral_target.process_key("Escape")
    rq_cands = _candidate_texts(_type_keys(vitral_target, "rq"))
    assert any(re.search(r"\d", c) and any(sep in c for sep in ("-", "年", "/")) for c in rq_cands)

    vitral_target.process_key("Escape")
    sj_cands = _candidate_texts(_type_keys(vitral_target, "sj"))
    assert any(re.search(r"\d", c) and any(sep in c for sep in (":", "时")) for c in sj_cands)

    vitral_target.process_key("Escape")
    cands = _candidate_texts(_type_keys(vitral_target, "ceshi"))
    assert len(cands) >= 2
    pinned = cands[1]
    pin_res = vitral_target.pin_candidate(1)
    assert _candidate_texts(pin_res)[0] == pinned
    erase_res = vitral_target.erase_candidate(0)
    assert pinned not in _candidate_texts(erase_res)


@pytest.mark.requires_core
def test_w09_lexicon_import_webdav_sync_restore_pinyin_and_mode2_recall(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    """Workflow 9: Import .scel + polyphone .txt -> WebDAV sync -> full restore -> Pinyin & Mode-2 HW recall."""
    fx_dir = _fixtures_dir(discovered_targets)
    scel_path = fx_dir / "lexicons" / "sample_hotwords.scel"
    poly_path = fx_dir / "lexicons" / "polyphone_unannotated.txt"
    oracle = json.loads((fx_dir / "lexicons" / "expected_polyphones.json").read_text(encoding="utf-8"))
    fx_hw = json.loads((fx_dir / "handwriting" / "mode2_baoguang_laodeng.json").read_text(encoding="utf-8"))

    dev_a = vitral_target
    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "w09_dev_b")
    try:
        dev_a.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-w09-lexicon-sync-key-32b",
            "device_id": "w09-dev-a",
        })
        rep_scel = dev_a.import_lexicon(scel_path, "scel")
        rep_poly = dev_a.import_lexicon(poly_path, "txt")
        assert int(rep_scel.get("imported_count", 0)) >= 12
        assert int(rep_poly.get("imported_count", 0)) == len(oracle["cases"])
        dev_a.reload_assets()

        assert any(p.startswith("lexicon/") for p in webdav_server.list_files())
        webdav_server.assert_no_live_slot()

        # Restore onto fresh Device B
        dev_b.init({"device_id": "w09-dev-b"})
        restored = dev_b.restore_from_koofr({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-w09-lexicon-sync-key-32b",
        })
        assert restored.get("restored") is True

        # Verify restored polyphone Pinyin recall + negative reading exclusion
        dev_b.set_schema("rime_ice")
        for case in oracle["cases"]:
            dev_b.process_key("Escape")
            py = case["expected_full_pinyin"].replace(" ", "")
            cands = _candidate_texts(_type_keys(dev_b, py))
            assert case["text"] in cands, f"Restored device missing {case['text']} under {py!r}"

        dev_b.process_key("Escape")
        assert "银行营业" not in _candidate_texts(_type_keys(dev_b, "yinxingyingye"))

        # Verify restored hotword lexicon drives Mode-2 continuous handwriting backtracking
        dev_b.hw_begin("ContinuousLattice")
        _replay_strokes(dev_b, fx_hw["strokes"])
        hw_res = dev_b.hw_end()
        assert hw_res["composing_text"] == "爆光老登金币"
        webdav_server.assert_no_live_slot()
    finally:
        dev_b.close()


@pytest.mark.requires_core
@pytest.mark.requires_android
@pytest.mark.tokens
def test_w10_mobile_docked_floating_layout_and_spacebar_backspace_gestures(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    """Workflow 10: Android 1/4 docked (4-row Xiaohe vs 5-row Quanpin), floating >=220x165dp, & swipe gestures."""
    vitral_target.require_android_apk()
    ergo = design_tokens["ergonomics"]
    assert float(ergo["mobileDockedScreenRatio"]) == pytest.approx(0.26)
    assert ergo["mobileTopHierarchy"] == ["icon-toolbar", "candidate-bar", "key-grid"]
    assert ergo["mobileFloatingWindow"] == {"freeDrag": True, "freeResize": True, "minWidthDp": 220, "minHeightDp": 165}
    assert ergo["spacebarVoiceHoldThresholdMs"] == 350
    assert ergo["spacebarVoiceVisual"] == "in-place-waveform"
    assert ergo["schemaKeycaps"]["double_pinyin_flypy"]["rows"] == 4
    assert ergo["schemaKeycaps"]["rime_ice_quanpin"]["rows"] == 5
    assert ergo["schemaKeycaps"]["rime_ice_quanpin"]["dedicatedTopNumberRow"] is True

    root = discovered_targets.vitral_root
    kt_tokens = (
        root / "apps" / "android" / "app" / "src" / "main" / "java" / "dev" / "vitral" / "ime" / "theme" / "VitralTokens.kt"
    ).read_text(encoding="utf-8")
    assert "mobileDockedScreenRatio: Float = 0.26f" in kt_tokens
    assert "floatingMinWidthDp: Int = 220" in kt_tokens
    assert "floatingMinHeightDp: Int = 165" in kt_tokens

    kt_files, kt_src = _read_non_generated_sources(
        root / "apps" / "android" / "app" / "src", (".kt", ".java"), exclude_names={"VitralTokens.kt"}
    )
    assert len(kt_files) >= 1
    assert "flypyRows" in kt_src and "quanpinRows" in kt_src and "123" in kt_src
    assert "floatingMinWidth" in kt_src and "floatingMinHeight" in kt_src
    assert "spacebarVoiceHoldThresholdMs" in kt_src

    # Verify core gesture hooks across both Xiaohe & Quanpin schemas
    vitral_target.init({"schema": "double_pinyin_flypy"})
    for schema in ("double_pinyin_flypy", "rime_ice"):
        vitral_target.set_schema(schema)
        for swipe in ("left", "right", "up"):
            sp_res = vitral_target.process_key("space", swipe_dir=swipe)
            assert isinstance(sp_res, dict)
        bs_res = vitral_target.process_key("BackSpace", swipe_dir="left")
        assert isinstance(bs_res, dict)
