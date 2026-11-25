"""Tier 2 BVA and negative/corner-case tests for F10-F12 (Retention, Sync/Clipboard/Quick Paste, Voice/Offline)."""

from __future__ import annotations

import json
import re
import time
import wave
from pathlib import Path
from typing import Any

import pytest

from harnesses import AIMockServer, LocalSendMockPeer, WebDAVMockServer
from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets

pytestmark = pytest.mark.tier2


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


def _qp_entry_text(state: dict[str, Any]) -> str | None:
    qp = state.get("quick_paste")
    if isinstance(qp, dict):
        val = qp.get("content", qp.get("text"))
        return None if val is None else str(val)
    if state.get("present") is True:
        val = state.get("text", state.get("content"))
        return None if val is None else str(val)
    return None


def _consumed_text(res: dict[str, Any]) -> str | None:
    consumed = res.get("consumed")
    if isinstance(consumed, dict):
        val = consumed.get("content", consumed.get("text"))
        return None if val is None else str(val)
    val = res.get("text", res.get("pasted", res.get("content")))
    return None if val is None else str(val)


def _ring_items(state: dict[str, Any]) -> list[dict[str, Any]]:
    raw = state.get("ring")
    if raw is None:
        raw = state.get("entries", state.get("items", []))
    return [it for it in raw if isinstance(it, dict)]


def _item_text(item: dict[str, Any]) -> str:
    return str(item.get("content", item.get("text", item.get("payload", ""))))


def _extract_pushed_id(pushed: dict[str, Any], expected_content: str | None = None) -> str:
    if pushed.get("id") or pushed.get("entry_id"):
        if expected_content is not None and any(k in pushed for k in ("content", "text", "payload")):
            assert _item_text(pushed) == expected_content, (
                f"clipboard_push returned entry with content {_item_text(pushed)!r}, expected {expected_content!r}"
            )
        return str(pushed.get("id") or pushed.get("entry_id"))
    items = _ring_items(pushed)
    assert items, f"clipboard_push returned no entry id and empty ring: {pushed}"
    if expected_content is not None:
        entry = next((it for it in items if _item_text(it) == expected_content), None)
        assert entry is not None, (
            f"Expected pushed item {expected_content!r} in clipboard ring, got {items}"
        )
    else:
        entry = items[0]
    assert entry.get("id"), f"Missing 'id' in ClipboardEntry: {entry}"
    return str(entry["id"])


def _audio_dir(discovered_targets: DiscoveredTargets) -> Path:
    assert discovered_targets.fixtures_dir is not None, "tests/fixtures directory missing"
    return discovered_targets.fixtures_dir / "audio"


def _hw_dir(discovered_targets: DiscoveredTargets) -> Path:
    assert discovered_targets.fixtures_dir is not None, "tests/fixtures directory missing"
    return discovered_targets.fixtures_dir / "handwriting"


def _read_wav_pcm(path: Path) -> bytes:
    with wave.open(str(path), "rb") as wf:
        assert wf.getnchannels() == 1
        assert wf.getsampwidth() == 2
        assert wf.getframerate() == 16000
        return wf.readframes(wf.getnframes())


# ======================================================================
# F10: Seven Adaptive Retention Mechanisms (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f10_frecency_decay_and_cross_modal_hw_sharing(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    fx_hw = json.loads(
        (_hw_dir(discovered_targets) / "mode2_horizontal_connected.json").read_text(encoding="utf-8")
    )
    vitral_target.init({"schema": "rime_ice", "frecency_half_life_sec": 1})

    # Non-vacuous cross-modal check: first promote a competing candidate for "shouxiezhongwen" to rank 0
    hw_probe = _type_ascii(vitral_target, "shouxiezhongwen")
    probe_cands = _extract_candidates(hw_probe)
    assert len(probe_cands) >= 2, f"Expected >=2 candidates for 'shouxiezhongwen', got {probe_cands}"
    rival = next((t for t in probe_cands if t != "手写中文"), None)
    assert rival is not None, f"Expected competing candidate for 'shouxiezhongwen', got {probe_cands}"
    vitral_target.process_key("Escape")

    rival_boosts = 0
    for attempt in range(3):
        cur = _type_ascii(vitral_target, "shouxiezhongwen")
        cur_cands = _extract_candidates(cur)
        assert rival in cur_cands, f"Competing candidate {rival!r} missing from {cur_cands}"
        vitral_target.select_candidate(cur_cands.index(rival))
        rival_boosts += 1
        if attempt >= 1:
            chk = _type_ascii(vitral_target, "shouxiezhongwen")
            chk_cands = _extract_candidates(chk)
            vitral_target.process_key("Escape")
            if chk_cands[0] == rival:
                break

    before_hw = _type_ascii(vitral_target, "shouxiezhongwen")
    cands_before_hw = _extract_candidates(before_hw)
    assert cands_before_hw[0] == rival and cands_before_hw[0] != "手写中文", (
        f"Expected rival {rival!r} at rank 0 before HW confirmation, got {cands_before_hw}"
    )
    pre_hw_frecency = next(
        (
            float(item["frecency_score"])
            for item in before_hw.get("candidates", [])
            if isinstance(item, dict) and item.get("text") == "手写中文" and "frecency_score" in item
        ),
        None,
    )
    vitral_target.process_key("Escape")

    # Confirm "手写中文" via Mode 2 ContinuousLattice hw_end() -> shared frecency must boost pinyin recall
    hw_boosts = rival_boosts + 2
    for _ in range(hw_boosts):
        vitral_target.hw_begin("ContinuousLattice")
        for stroke in fx_hw["strokes"]:
            pts = [
                {"x": float(p["x"]), "y": float(p["y"]), "timestamp_ms": int(p.get("timestamp_ms", p.get("t", 0)))}
                for p in stroke
            ]
            vitral_target.hw_push_stroke(pts, pen_up=True)
        hw_res = vitral_target.hw_end()
        assert hw_res["composing_text"] == "手写中文", f"Expected '手写中文' from Mode 2 hw_end, got {hw_res}"
        vitral_target.hw_clear()

    hw_pinyin = _type_ascii(vitral_target, "shouxiezhongwen")
    hw_cands = _extract_candidates(hw_pinyin)
    assert hw_cands[0] == "手写中文", (
        f"Expected Mode 2 handwriting confirmation of '手写中文' to reclaim rank 0 ahead of {rival!r}, got {hw_cands}"
    )
    if pre_hw_frecency is not None:
        post_hw_frecency = next(
            (
                float(item["frecency_score"])
                for item in hw_pinyin.get("candidates", [])
                if isinstance(item, dict) and item.get("text") == "手写中文" and "frecency_score" in item
            ),
            None,
        )
        assert post_hw_frecency is not None and post_hw_frecency > pre_hw_frecency, (
            f"Expected '手写中文' frecency_score to increase after HW confirmation "
            f"(before={pre_hw_frecency}, after={post_hw_frecency})"
        )

    # Select a lower-ranked pinyin candidate repeatedly by tracking its text; verify strict rank promotion
    vitral_target.process_key("Escape")
    act1 = _type_ascii(vitral_target, "mao")
    cands1 = _extract_candidates(act1)
    assert len(cands1) >= 3
    target_idx = min(2, len(cands1) - 1)
    chosen = cands1[target_idx]
    for _ in range(3):
        vitral_target.process_key("Escape")
        cur_cands = _extract_candidates(_type_ascii(vitral_target, "mao"))
        assert chosen in cur_cands, f"Candidate {chosen!r} missing from {cur_cands}"
        vitral_target.select_candidate(cur_cands.index(chosen))

    vitral_target.process_key("Escape")
    cands2 = _extract_candidates(_type_ascii(vitral_target, "mao"))
    assert chosen in cands2 and cands2.index(chosen) < target_idx, (
        f"Expected strict rank gain (< {target_idx}) for {chosen!r}, got {cands2}"
    )

    # Deterministic frecency overtaking without wall-clock sleep or conditional guard
    other_word = next(c for c in cands2 if c != chosen)
    for _ in range(5):
        vitral_target.process_key("Escape")
        cur_cands = _extract_candidates(_type_ascii(vitral_target, "mao"))
        assert other_word in cur_cands, f"Candidate {other_word!r} missing from {cur_cands}"
        vitral_target.select_candidate(cur_cands.index(other_word))

    vitral_target.process_key("Escape")
    after_other = _extract_candidates(_type_ascii(vitral_target, "mao"))
    assert after_other[0] == other_word, (
        f"Expected 5x selected {other_word!r} to overtake 3x selected {chosen!r}, got {after_other}"
    )

    # Verify high frecency_half_life_sec boundary preserves strict rank promotion cleanly
    high_hl = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "high_half_life")
    try:
        high_hl.init({"schema": "rime_ice", "frecency_half_life_sec": 86400})
        base_hi = _extract_candidates(_type_ascii(high_hl, "mao"))
        assert len(base_hi) >= 3
        hi_idx = min(2, len(base_hi) - 1)
        hi_chosen = base_hi[hi_idx]
        for _ in range(3):
            high_hl.process_key("Escape")
            cur_hi = _extract_candidates(_type_ascii(high_hl, "mao"))
            assert hi_chosen in cur_hi
            high_hl.select_candidate(cur_hi.index(hi_chosen))
        high_hl.process_key("Escape")
        promoted_hi = _extract_candidates(_type_ascii(high_hl, "mao"))
        assert hi_chosen in promoted_hi and promoted_hi.index(hi_chosen) < hi_idx
    finally:
        high_hl.close()


@pytest.mark.requires_core
def test_f10_candidate_pin_erase_and_oob_index_error(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    act = _type_ascii(vitral_target, "shi")
    cands = _extract_candidates(act)
    assert len(cands) >= 3

    pinned_word = cands[2]
    vitral_target.pin_candidate(2)
    vitral_target.process_key("Escape")
    cands_after_pin = _extract_candidates(_type_ascii(vitral_target, "shi"))
    assert cands_after_pin[0] == pinned_word

    vitral_target.erase_candidate(0)

    with pytest.raises(VitralCommandError):
        vitral_target.pin_candidate(99)
    with pytest.raises(VitralCommandError):
        vitral_target.erase_candidate(99)


@pytest.mark.requires_core
def test_f10_pinned_clipboard_shortcut_ksb_lifecycle(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    pushed = vitral_target.clipboard_push("VitralShortcutPayload-2026")
    entry_id = _extract_pushed_id(pushed, "VitralShortcutPayload-2026")
    vitral_target.clipboard_pin(entry_id, "ksb")
    vitral_target.dismiss_quick_paste()

    act = _type_ascii(vitral_target, "ksb")
    raw_cands = act.get("candidates", [])
    cands = _extract_candidates(act)
    assert len(cands) >= 1 and "VitralShortcutPayload-2026" in cands[0]
    if isinstance(raw_cands[0], dict):
        comment_or_badge = str(
            raw_cands[0].get("comment", raw_cands[0].get("badge", raw_cands[0].get("icon", "")))
        )
        assert (
            raw_cands[0].get("is_clipboard_shortcut") is True
            or raw_cands[0].get("pinned") is True
            or raw_cands[0].get("pinned_clipboard") is True
            or "📌" in comment_or_badge
        )

    # Clearing/removing pinned shortcut restores normal pinyin candidates ('看书吧' etc.)
    vitral_target.process_key("Escape")
    vitral_target.clipboard_pin(entry_id, "")
    act_after = _type_ascii(vitral_target, "ksb")
    assert "VitralShortcutPayload-2026" not in _extract_candidates(act_after)


@pytest.mark.requires_core
def test_f10_per_app_cn_en_context_and_paired_punct(vitral_target: VitralTarget) -> None:
    vitral_target.init({"schema": "rime_ice"})
    en_state = vitral_target.on_app_focus("kitty")
    assert str(en_state.get("lang_mode", "")).lower() in ("en", "english")

    tg_state = vitral_target.on_app_focus("telegram")
    assert str(tg_state.get("lang_mode", "")).lower() in ("zh", "cn", "chinese")

    # Switching back to kitty must remember En mode
    en_remembered = vitral_target.on_app_focus("kitty")
    assert str(en_remembered.get("lang_mode", "")).lower() in ("en", "english")

    # Empty app_id must return InvalidArgument (VitralCommandError) per eng.rs:189-191
    with pytest.raises(VitralCommandError):
        vitral_target.on_app_focus("")

    # Return to Chinese app context for macros and paired punctuation
    tg_again = vitral_target.on_app_focus("telegram")
    assert str(tg_again.get("lang_mode", "")).lower() in ("zh", "cn", "chinese")

    # Paired punctuation & rq/sj time macros
    vitral_target.process_key("Escape")
    rq_cands = _extract_candidates(_type_ascii(vitral_target, "rq"))
    assert any(re.search(r"\d", c) and any(sep in c for sep in ("-", "年", "/")) for c in rq_cands), (
        f"Expected date macro expansion for 'rq', got {rq_cands}"
    )

    vitral_target.process_key("Escape")
    sj_cands = _extract_candidates(_type_ascii(vitral_target, "sj"))
    assert any(re.search(r"\d", c) and any(sep in c for sep in (":", "时")) for c in sj_cands), (
        f"Expected time macro expansion for 'sj', got {sj_cands}"
    )

    vitral_target.process_key("Escape")
    punct_act = vitral_target.process_key("(")
    paired = str(
        punct_act.get("paired_punctuation")
        or punct_act.get("commit_text")
        or _preedit_text(punct_act)
    )
    assert "()" in paired or "（）" in paired, f"Expected paired punctuation, got {punct_act}"
    if "cursor_offset" in punct_act:
        assert int(punct_act["cursor_offset"]) < 0
    elif "cursor_delta" in punct_act:
        assert int(punct_act["cursor_delta"]) <= 0


@pytest.mark.requires_core
def test_f10_mru_device_tier_and_mobile_swipe_boundaries(
    vitral_target: VitralTarget,
    localsend_server: LocalSendMockPeer,
) -> None:
    vitral_target.init({
        "localsend_port": localsend_server.port,
        "known_devices": [
            {
                "id": "offline-dev",
                "device_id": "offline-dev",
                "device_name": "Offline Dev",
                "tier": "offline",
                "lan_online": False,
                "wan_online": False,
                "transfer_count": 50,
                "usage_count": 50,
                "last_used_ms": 5000,
            },
            {
                "id": "wan-high",
                "device_id": "wan-high",
                "device_name": "WAN High",
                "tier": "wan",
                "lan_online": False,
                "wan_online": True,
                "transfer_count": 20,
                "usage_count": 20,
                "last_used_ms": 4000,
            },
            {
                "id": "lan-low",
                "device_id": "lan-low",
                "device_name": "LAN Low",
                "tier": "lan",
                "lan_online": True,
                "wan_online": True,
                "transfer_count": 1,
                "usage_count": 1,
                "last_used_ms": 1000,
            },
            {
                "id": "lan-high",
                "device_id": "lan-high",
                "device_name": "LAN High",
                "tier": "lan",
                "lan_online": True,
                "wan_online": True,
                "transfer_count": 10,
                "usage_count": 10,
                "last_used_ms": 3000,
            },
        ],
    })
    devices = vitral_target.relay_devices_mru()
    ids = [str(d.get("device_id", d.get("id", ""))) for d in devices]
    assert len(ids) >= 4, f"Expected 4 MRU devices, got {devices}"
    assert ids.index("lan-high") < ids.index("lan-low") < ids.index("wan-high") < ids.index("offline-dev")

    for swipe in ("up", "down", "left", "right"):
        act = vitral_target.process_key("space", swipe_dir=swipe)
        assert isinstance(act, dict)


# ======================================================================
# F11: Koofr WebDAV, RAM Quick Paste, N-Item Ring & Relay (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f11_quick_paste_per_device_consume_and_overwrite(
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    dev_a = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_a")
    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b")
    try:
        dev_a.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "webdav_url": webdav_server.koofr_endpoint,
            "device_id": "dev-a",
        })
        dev_b.init({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "webdav_url": webdav_server.koofr_endpoint,
            "device_id": "dev-b",
        })

        dev_a.clipboard_push("copy-one-from-A", source_device="dev-a")
        dev_b.clipboard_ingest_remote("copy-one-from-A", is_cold_boot=False)

        assert _qp_entry_text(dev_a.quick_paste_state()) == "copy-one-from-A"
        assert _qp_entry_text(dev_b.quick_paste_state()) == "copy-one-from-A"

        # Clicking Quick Paste on Device A consumes locally ONLY; Device B MUST keep it!
        consumed_a = dev_a.consume_quick_paste()
        assert _consumed_text(consumed_a) == "copy-one-from-A"
        assert _qp_entry_text(dev_a.quick_paste_state()) is None
        assert _qp_entry_text(dev_b.quick_paste_state()) == "copy-one-from-A"

        # Newer copy overwrites Quick Paste on both devices
        copied = dev_b.clipboard_push("copy-two-overwrite", source_device="dev-b")
        dev_a.clipboard_ingest_remote(copied["ring"][0], is_cold_boot=False)
        assert _qp_entry_text(dev_a.quick_paste_state()) == "copy-two-overwrite"
        assert _qp_entry_text(dev_b.quick_paste_state()) == "copy-two-overwrite"

        webdav_server.assert_no_live_slot()
    finally:
        dev_a.close()
        dev_b.close()


@pytest.mark.requires_core
def test_f11_quick_paste_cold_boot_and_ime_open_close_edge(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "webdav_url": webdav_server.koofr_endpoint,
        "device_id": "dev-edge",
    })

    # 1. A new device restores its current copy.
    vitral_target.clipboard_ingest_remote("historical-cold-boot-item", is_cold_boot=True)
    assert _qp_entry_text(vitral_target.quick_paste_state()) == "historical-cold-boot-item"

    # 2. Remote copy arriving while ime_open == False must survive on_ime_close() before first open
    vitral_target.clipboard_ingest_remote("fresh-copy-while-closed", is_cold_boot=False)
    vitral_target.on_ime_close()
    qp_before_open = vitral_target.quick_paste_state()
    assert _qp_entry_text(qp_before_open) == "fresh-copy-while-closed"

    # 3. Closing the keyboard preserves the suggestion.
    vitral_target.on_ime_open()
    assert _qp_entry_text(vitral_target.quick_paste_state()) == "fresh-copy-while-closed"
    vitral_target.on_ime_close()
    assert _qp_entry_text(vitral_target.quick_paste_state()) == "fresh-copy-while-closed"
    vitral_target.dismiss_quick_paste()

    # 4. Re-opening IME does NOT resurrect explicitly cleared Quick Paste
    vitral_target.on_ime_open()
    assert _qp_entry_text(vitral_target.quick_paste_state()) is None
    webdav_server.assert_no_live_slot()


@pytest.mark.requires_core
def test_f11_clipboard_ring_full_pin_eviction_halt_and_no_live_slot(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "webdav_url": webdav_server.koofr_endpoint,
        "clipboard_ring_capacity": 4,
        "clipboard_capacity": 4,
    })

    pinned_ids: list[str] = []
    # Includes unfiltered secret-like string (no password filtering allowed)
    secret_payload = "sk-live-secret-unfiltered-token-999"
    payloads = [secret_payload, "pin-item-2", "pin-item-3", "pin-item-4"]
    codes = ("pa", "pb", "pc", "pd")
    for idx, text in enumerate(payloads):
        res = vitral_target.clipboard_push(text)
        eid = _extract_pushed_id(res, text)
        vitral_target.clipboard_pin(eid, codes[idx])
        pinned_ids.append(eid)

    # Push 5th unpinned item when all 4 slots are pinned -> ZERO pinned items may be dropped
    vitral_target.clipboard_push("fifth-unpinned-item")
    state = vitral_target.clipboard_state()
    items = _ring_items(state)
    texts = [_item_text(it) for it in items]
    for p_text in payloads:
        assert p_text in texts, f"Pinned item {p_text!r} was evicted: {texts}"

    # Clear unpinned leaves all 4 pinned items intact
    vitral_target.clipboard_clear_unpinned()
    after_clear = vitral_target.clipboard_state()
    after_texts = [_item_text(it) for it in _ring_items(after_clear)]
    assert set(payloads) <= set(after_texts)
    assert "fifth-unpinned-item" not in after_texts

    webdav_server.assert_no_live_slot()
    assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")


@pytest.mark.requires_core
def test_f11_relay_sha256_mismatch_abort(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "webdav_url": webdav_server.koofr_endpoint,
        "device_id": "receiver",
    })
    transfer_id = "bad-sha-transfer-01"
    enc_bytes = b"authentic-encrypted-relay-payload"
    corrupt_bytes = b"tampered!-encrypted-relay-payload"
    meta = {
        "transfer_id": transfer_id,
        "sha256": "0" * 64,  # guaranteed mismatch
        "created_at_utc": "2026-11-20T15:00:00Z",
        "target_device_id": "receiver",
        "size": len(enc_bytes),
    }
    webdav_server.write_file_bytes(f"relay/{transfer_id}.enc", corrupt_bytes)
    webdav_server.write_file_bytes(f"relay/{transfer_id}.meta.json", json.dumps(meta).encode("utf-8"))

    with pytest.raises(VitralCommandError):
        vitral_target.relay_poll_inbox()


@pytest.mark.requires_core
def test_f11_relay_24h_orphan_boundary_23h59m_vs_24h01m(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "webdav_url": webdav_server.koofr_endpoint,
        "device_id": "sweeper",
    })
    now = time.time()
    keep_mtime = now - (23 * 3600 + 59 * 60)
    purge_mtime = now - (24 * 3600 + 60)

    webdav_server.write_file_bytes("relay/keep_23h59m.enc", b"keep-me")
    webdav_server.write_file_bytes("relay/keep_23h59m.meta.json", b'{"transfer_id":"keep_23h59m"}')
    webdav_server.set_mtime("relay/keep_23h59m.enc", keep_mtime)
    webdav_server.set_mtime("relay/keep_23h59m.meta.json", keep_mtime)

    webdav_server.write_file_bytes("relay/purge_24h01m.enc", b"purge-me")
    webdav_server.write_file_bytes("relay/purge_24h01m.meta.json", b'{"transfer_id":"purge_24h01m"}')
    webdav_server.set_mtime("relay/purge_24h01m.enc", purge_mtime)
    webdav_server.set_mtime("relay/purge_24h01m.meta.json", purge_mtime)

    vitral_target.relay_cleanup_orphans()

    assert webdav_server.file_exists("relay/keep_23h59m.enc")
    assert webdav_server.file_exists("relay/keep_23h59m.meta.json")
    assert not webdav_server.file_exists("relay/purge_24h01m.enc")
    assert not webdav_server.file_exists("relay/purge_24h01m.meta.json")


# ======================================================================
# F12: Offline Voice (SenseVoice-Small) & Pure Offline Mode (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f12_pure_offline_physical_egress_block(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
    ai_server: AIMockServer,
) -> None:
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "webdav_url": webdav_server.koofr_endpoint,
        "ai_endpoint": ai_server.ollama_url,
        "pure_offline": True,
    })
    vitral_target.set_pure_offline(True)
    assert vitral_target.is_pure_offline() is True
    webdav_server.clear_request_log()
    ai_server.clear_request_log()

    with pytest.raises(VitralCommandError) as exc_ai:
        vitral_target.ai_inline("translate", "你好世界", "en")
    assert "offline" in str(exc_ai.value).lower() or "5" in str(exc_ai.value)

    with pytest.raises(VitralCommandError):
        vitral_target.restore_from_koofr({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "webdav_url": webdav_server.koofr_endpoint,
        })

    with pytest.raises(VitralCommandError):
        vitral_target.relay_send("remote-wan-dev", "blocked payload")

    assert len(webdav_server.request_log) == 0, f"Unexpected WebDAV egress in pure_offline: {webdav_server.request_log}"
    assert len(ai_server.request_log) == 0, f"Unexpected AI egress in pure_offline: {ai_server.request_log}"


@pytest.mark.requires_core
def test_f12_pure_offline_toggle_recovery(
    vitral_target: VitralTarget,
    ai_server: AIMockServer,
) -> None:
    vitral_target.init({"ai_endpoint": ai_server.ollama_url, "pure_offline": True})
    assert vitral_target.is_pure_offline() is True

    with pytest.raises(VitralCommandError):
        vitral_target.ai_inline("polish", "今天天气不错呀", "zh")
    assert len(ai_server.request_log) == 0

    vitral_target.set_pure_offline(False)
    assert vitral_target.is_pure_offline() is False
    res = vitral_target.ai_inline("polish", "今天天气不错呀", "zh")
    assert isinstance(res, dict)
    out_text = str(res.get("output_text", res.get("text", res.get("result", "")))).strip()
    assert len(out_text) > 0, f"Expected non-empty AI inline output_text, got {res}"
    assert len(ai_server.request_log) >= 1


@pytest.mark.requires_core
def test_f12_voice_silence_noise_no_hallucination(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    audio_dir = _audio_dir(discovered_targets)
    pcm = _read_wav_pcm(audio_dir / "silence_noise_16k.wav")
    vitral_target.init({"pure_offline": True})

    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm)
    res = vitral_target.voice_stop()
    transcript = str(res.get("transcript", res.get("text", ""))).strip()
    assert transcript == "", f"Silence noise should not hallucinate words, got {transcript!r}"


@pytest.mark.requires_core
def test_f12_voice_long_20s_and_mixed_en_offline(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    audio_dir = _audio_dir(discovered_targets)
    manifest = json.loads((audio_dir / "audio_manifest.json").read_text(encoding="utf-8"))
    clips_by_id = {c["id"]: c for c in manifest["clips"]}
    vitral_target.init({"pure_offline": True})

    for clip_id in ("mandarin_mixed_en", "long_20s"):
        clip = clips_by_id[clip_id]
        pcm = _read_wav_pcm(audio_dir / clip["file"])
        vitral_target.voice_start()
        chunk_bytes = 32000  # 1s chunks
        for offset in range(0, len(pcm), chunk_bytes):
            vitral_target.voice_push_pcm(pcm[offset : offset + chunk_bytes])
        res = vitral_target.voice_stop()
        transcript = str(res.get("transcript", res.get("text", ""))).strip()
        assert len(transcript) > 0, f"Empty offline transcript for {clip_id}"
        assert any(kw.lower() in transcript.lower() for kw in clip["expected_keywords"]), (
            f"Expected one of {clip['expected_keywords']} in {transcript!r}"
        )


@pytest.mark.requires_core
def test_f12_voice_empty_unstarted_and_unaligned_pcm(vitral_target: VitralTarget) -> None:
    vitral_target.init({"pure_offline": True})
    with pytest.raises(VitralCommandError):
        vitral_target.voice_stop()

    vitral_target.voice_start()
    try:
        vitral_target.voice_push_pcm(b"")
    except VitralCommandError:
        pass
    try:
        vitral_target.voice_push_pcm(b"\x01\x02\x03")  # odd byte length (not 16-bit aligned)
    except VitralCommandError:
        pass
    try:
        res = vitral_target.voice_stop()
        assert isinstance(res, dict)
    except VitralCommandError:
        pass
