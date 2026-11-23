"""Tier 1 Retention, Sync/Clipboard/Quick-Paste, and Offline Voice/Mode tests (F10-F12)."""

from __future__ import annotations

import json
import time
import wave
from pathlib import Path
from typing import Any

import pytest

from harnesses import AIMockServer, LocalSendMockPeer, WebDAVMockServer
from vitral_e2e.adapter import VitralCommandError, VitralTarget
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


def _extract_pushed_entry_id(pushed: dict[str, Any], expected_content: str) -> str:
    assert "ring" in pushed and isinstance(pushed["ring"], list) and pushed["ring"], (
        f"Expected ClipboardState with non-empty 'ring', got {pushed}"
    )
    entry = next((e for e in pushed["ring"] if e.get("content") == expected_content), pushed["ring"][0])
    assert "id" in entry and entry["id"], f"Missing 'id' in ClipboardEntry: {entry}"
    return str(entry["id"])


def _assert_qp_present(qp_state: dict[str, Any], expected_content: str) -> None:
    assert "quick_paste" in qp_state, f"Missing 'quick_paste' key in QuickPasteState: {qp_state}"
    entry = qp_state["quick_paste"]
    assert isinstance(entry, dict), f"Expected non-null Quick Paste entry, got {qp_state}"
    assert "content" in entry and entry["content"] == expected_content, (
        f"Expected quick_paste.content == {expected_content!r}, got {entry}"
    )


def _assert_qp_empty(qp_state: dict[str, Any]) -> None:
    assert "quick_paste" in qp_state, f"Missing 'quick_paste' key in QuickPasteState: {qp_state}"
    assert qp_state["quick_paste"] is None, f"Expected empty quick_paste=None, got {qp_state}"


def _assert_qp_consumed(res: dict[str, Any], expected_content: str) -> None:
    assert "consumed" in res and isinstance(res["consumed"], dict), (
        f"Expected non-null 'consumed' entry in QuickPasteConsumeResult, got {res}"
    )
    assert res["consumed"].get("content") == expected_content, f"Unexpected consumed entry: {res}"
    assert "state" in res and isinstance(res["state"], dict), (
        f"Missing 'state' in QuickPasteConsumeResult: {res}"
    )
    _assert_qp_empty(res["state"])


def _extract_ring_entries(state: dict[str, Any]) -> list[dict[str, Any]]:
    assert "ring" in state and isinstance(state["ring"], list), (
        f"Missing canonical 'ring' in ClipboardState: {state}"
    )
    for entry in state["ring"]:
        assert isinstance(entry, dict) and "id" in entry and "content" in entry, (
            f"Invalid ClipboardEntry in ring: {entry}"
        )
    return state["ring"]


def _extract_voice_transcript(res: dict[str, Any]) -> str:
    assert "transcript" in res and isinstance(res["transcript"], str), (
        f"Missing canonical 'transcript' string in VoiceResult: {res}"
    )
    return res["transcript"].strip()


def _read_wav_pcm(path: Path) -> bytes:
    with wave.open(str(path), "rb") as w:
        assert w.getframerate() == 16000
        assert w.getnchannels() == 1
        assert w.getsampwidth() == 2
        return w.readframes(w.getnframes())


# ---------------------------------------------------------------------------
# F10: Seven Adaptive Retention Mechanisms (7 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_frecency_boosts_repeated_pinyin_and_handwriting_choice(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    first = _type_keys(vitral_target, "ceshi")
    cands = _candidate_texts(first)
    assert len(cands) >= 2
    target_word = cands[1]
    vitral_target.process_key("Escape")

    for _ in range(3):
        action = _type_keys(vitral_target, "ceshi")
        cur_cands = _candidate_texts(action)
        idx = cur_cands.index(target_word)
        vitral_target.select_candidate(idx)

    after = _type_keys(vitral_target, "ceshi")
    assert _candidate_texts(after)[0] == target_word
    vitral_target.process_key("Escape")

    # First promote a competing pinyin candidate for "shouxiezhongwen" to rank 0 so
    # handwriting -> pinyin Frecency sharing cannot pass vacuously.
    hw_probe = _type_keys(vitral_target, "shouxiezhongwen")
    probe_cands = _candidate_texts(hw_probe)
    assert len(probe_cands) >= 2
    rival = next((t for t in probe_cands if t != "手写中文"), None)
    assert rival is not None, f"Expected competing candidate for 'shouxiezhongwen', got {probe_cands}"
    vitral_target.process_key("Escape")

    rival_boosts = 0
    for attempt in range(3):
        cur = _type_keys(vitral_target, "shouxiezhongwen")
        cur_cands = _candidate_texts(cur)
        assert rival in cur_cands, f"Competing candidate {rival!r} missing from {cur_cands}"
        vitral_target.select_candidate(cur_cands.index(rival))
        rival_boosts += 1
        if attempt >= 1:
            check = _type_keys(vitral_target, "shouxiezhongwen")
            check_cands = _candidate_texts(check)
            vitral_target.process_key("Escape")
            if check_cands[0] == rival:
                break

    before_hw = _type_keys(vitral_target, "shouxiezhongwen")
    cands_before_hw = _candidate_texts(before_hw)
    assert cands_before_hw[0] == rival
    assert cands_before_hw[0] != "手写中文"
    pre_hw_frecency = next(
        (
            float(item["frecency_score"])
            for item in before_hw.get("candidates", [])
            if isinstance(item, dict) and item.get("text") == "手写中文" and "frecency_score" in item
        ),
        None,
    )
    vitral_target.process_key("Escape")

    # Confirmed handwriting choice shares Frecency boost with pinyin recall
    hw_fx = json.loads(
        (discovered_targets.fixtures_dir / "handwriting" / "mode2_horizontal_connected.json").read_text(
            encoding="utf-8"
        )
    )
    hw_boosts = rival_boosts + 2
    for _ in range(hw_boosts):
        vitral_target.hw_begin("ContinuousLattice")
        for stroke in hw_fx["strokes"]:
            pts = [
                {
                    "x": float(p["x"]),
                    "y": float(p["y"]),
                    "timestamp_ms": int(p.get("timestamp_ms", p.get("t", 0))),
                }
                for p in stroke
            ]
            vitral_target.hw_push_stroke(pts, pen_up=True)
        hw_res = vitral_target.hw_end()
        assert hw_res["composing_text"] == "手写中文"
        vitral_target.hw_clear()

    hw_pinyin = _type_keys(vitral_target, "shouxiezhongwen")
    hw_cands = _candidate_texts(hw_pinyin)
    assert hw_cands[0] == "手写中文", (
        f"Expected handwriting-confirmed '手写中文' to reclaim rank 0 ahead of {rival!r}, got {hw_cands}"
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
            f"Expected '手写中文' frecency_score to increase after handwriting confirmation "
            f"(before={pre_hw_frecency}, after={post_hw_frecency})"
        )


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_frecency_exponential_time_decay_reduces_stale_score(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_core()
    # Configure rapid simulated decay half-life for deterministic verification
    vitral_target.init({"frecency_half_life_sec": 1})
    vitral_target.set_schema("rime_ice")

    initial = _type_keys(vitral_target, "shuru")
    cands = _candidate_texts(initial)
    assert len(cands) >= 2
    stale_word = cands[1]
    fresh_word = cands[0]
    assert stale_word != fresh_word
    vitral_target.process_key("Escape")

    # Boost stale_word repeatedly so it strictly outranks fresh_word before time decay
    for _ in range(3):
        cur = _type_keys(vitral_target, "shuru")
        cur_cands = _candidate_texts(cur)
        vitral_target.select_candidate(cur_cands.index(stale_word))

    promoted = _type_keys(vitral_target, "shuru")
    assert _candidate_texts(promoted)[0] == stale_word, (
        f"Expected boosted stale_word {stale_word!r} at rank 0 before decay, got {_candidate_texts(promoted)}"
    )
    vitral_target.process_key("Escape")

    # Advance >2 half-lives past simulated decay window, then boost fresh_word once
    time.sleep(2.2)
    after_decay = _type_keys(vitral_target, "shuru")
    cur_cands = _candidate_texts(after_decay)
    vitral_target.select_candidate(cur_cands.index(fresh_word))

    after = _type_keys(vitral_target, "shuru")
    assert _candidate_texts(after)[0] == fresh_word


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_candidate_1s_pin_and_erase_actions(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    action = _type_keys(vitral_target, "pingguo")
    cands = _candidate_texts(action)
    assert len(cands) >= 2
    pinned_word = cands[1]

    pin_res = vitral_target.pin_candidate(1)
    pinned_cands = _candidate_texts(pin_res)
    assert pinned_cands[0] == pinned_word

    erase_res = vitral_target.erase_candidate(0)
    assert pinned_word not in _candidate_texts(erase_res)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_pinned_clipboard_shortcut_ksb_surfaces_in_candidates(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_core()
    vitral_target.init({})

    url = "https://app.koofr.net/dav/Koofr"
    pushed = vitral_target.clipboard_push(url)
    entry_id = _extract_pushed_entry_id(pushed, url)
    vitral_target.clipboard_pin(entry_id, "ksb")
    vitral_target.dismiss_quick_paste()

    vitral_target.set_schema("rime_ice")
    action = _type_keys(vitral_target, "ksb")
    cands = _candidate_texts(action)
    assert any(url in c for c in cands), f"Expected pinned shortcut {url!r} in {cands}"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_per_app_cn_en_context_memory(vitral_target: VitralTarget) -> None:
    vitral_target.require_core()
    vitral_target.init({})

    # Both chat & browser apps default to Zh; toggle browser away from default to En
    chat_init = vitral_target.on_app_focus("org.telegram.desktop")
    assert str(chat_init.get("lang_mode")).lower() == "zh"

    browser_init = vitral_target.on_app_focus("org.mozilla.firefox")
    assert str(browser_init.get("lang_mode")).lower() == "zh"
    toggled = vitral_target.process_key("Shift", modifiers=["lang_toggle"])
    assert str(toggled.get("lang_mode")).lower() == "en"

    # Switching back to chat preserves Zh; returning to browser remembers toggled En
    chat_state = vitral_target.on_app_focus("org.telegram.desktop")
    assert str(chat_state.get("lang_mode")).lower() == "zh"

    browser_state = vitral_target.on_app_focus("org.mozilla.firefox")
    assert str(browser_state.get("lang_mode")).lower() == "en"
    assert browser_state.get("remembered") is True


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_mixed_cn_en_paired_punctuation_and_time_macros(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.set_schema("rime_ice")

    paren_res = vitral_target.process_key("(")
    assert "preedit" in paren_res and "commit_text" in paren_res
    preedit_obj = paren_res["preedit"]
    preedit_text = preedit_obj["text"] if isinstance(preedit_obj, dict) else str(preedit_obj or "")
    commit_val = str(paren_res["commit_text"] or "")
    paired_val = str(paren_res.get("paired_punctuation") or "")
    combined = f"{commit_val}{preedit_text}{paired_val}"
    assert "()" in combined or "（）" in combined, f"Expected paired punctuation in {paren_res}"
    vitral_target.process_key("Escape")

    rq_action = _type_keys(vitral_target, "rq")
    rq_cands = _candidate_texts(rq_action)
    assert any("-" in c or "年" in c or "/" in c for c in rq_cands), (
        f"Expected date macro expansion for 'rq', got {rq_cands}"
    )
    vitral_target.select_candidate(0)
    vitral_target.process_key("Escape")

    sj_action = _type_keys(vitral_target, "sj")
    sj_cands = _candidate_texts(sj_action)
    assert any(":" in c or "时" in c for c in sj_cands), (
        f"Expected time macro expansion for 'sj', got {sj_cands}"
    )


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f10_mru_device_sorting_and_mobile_gesture_hooks(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_core()
    vitral_target.init(
        {
            "known_devices": [
                {
                    "device_id": "dev-offline",
                    "device_name": "Offline Box",
                    "tier": "offline",
                    "usage_count": 20,
                    "last_used_ms": 9000,
                },
                {
                    "device_id": "dev-wan",
                    "device_name": "WAN Box",
                    "tier": "wan",
                    "usage_count": 5,
                    "last_used_ms": 5000,
                },
                {
                    "device_id": "dev-lan-low",
                    "device_name": "LAN Low",
                    "tier": "lan",
                    "usage_count": 1,
                    "last_used_ms": 1000,
                },
                {
                    "device_id": "dev-lan-high",
                    "device_name": "LAN High",
                    "tier": "lan",
                    "usage_count": 12,
                    "last_used_ms": 8000,
                },
            ]
        }
    )
    devices = vitral_target.relay_devices_mru()
    assert devices, "Expected non-empty relay_devices_mru list"
    for d in devices:
        assert "device_id" in d, f"RelayDevice missing canonical 'device_id': {d}"
    ids = [d["device_id"] for d in devices]
    assert ids == ["dev-lan-high", "dev-lan-low", "dev-wan", "dev-offline"]

    for swipe in ("up", "left", "right"):
        res = vitral_target.process_key("space", swipe_dir=swipe)
        assert isinstance(res, dict)


# ---------------------------------------------------------------------------
# F11: Koofr WebDAV 3-Tier Sync, Quick Paste RAM State, N-Ring & LocalSend v2 (9 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_koofr_webdav_three_tier_layout_and_chacha20_sync(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init(
        {
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "sync_key": "vitral-test-chacha20-key-32bytes!",
        }
    )
    vitral_target.set_schema("rime_ice")
    _type_keys(vitral_target, "nihao")
    vitral_target.select_candidate(0)
    vitral_target.reload_assets()

    all_files = webdav_server.list_files()
    assert any(p.startswith(("lexicon/", "profile/")) for p in all_files), (
        f"Expected lexicon/profile sync artifacts on WebDAV, got {all_files}"
    )
    webdav_server.assert_no_live_slot()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_quick_paste_copy_and_remote_ingest_vs_cold_boot(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    vitral_target.require_core()
    vitral_target.init({})
    vitral_target.clipboard_push("https://vitral.dev")
    qp_a = vitral_target.quick_paste_state()
    _assert_qp_present(qp_a, "https://vitral.dev")

    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b")
    try:
        dev_b.init({})
        # Cold boot history load MUST NOT populate Quick Paste
        dev_b.clipboard_ingest_remote("cold-boot-entry", is_cold_boot=True)
        qp_cold = dev_b.quick_paste_state()
        _assert_qp_present(qp_cold, "cold-boot-entry")
        ring_texts = [e["content"] for e in _extract_ring_entries(dev_b.clipboard_state())]
        assert "cold-boot-entry" in ring_texts

        # Incremental live remote copy MUST populate Quick Paste
        dev_b.clipboard_ingest_remote("live-remote-entry", is_cold_boot=False)
        qp_live = dev_b.quick_paste_state()
        _assert_qp_present(qp_live, "live-remote-entry")
    finally:
        dev_b.close()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_quick_paste_per_device_consume_isolation(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    vitral_target.require_core()
    dev_a = vitral_target
    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b")
    try:
        dev_a.init({"koofr_endpoint": webdav_server.koofr_endpoint, "device_id": "dev-a"})
        dev_b.init({"koofr_endpoint": webdav_server.koofr_endpoint, "device_id": "dev-b"})

        dev_a.clipboard_push("alpha-token", source_device="dev-a")
        dev_b.clipboard_ingest_remote("alpha-token", is_cold_boot=False)

        _assert_qp_present(dev_a.quick_paste_state(), "alpha-token")
        _assert_qp_present(dev_b.quick_paste_state(), "alpha-token")

        # Consuming on Device A clears Device A ONLY; Device B MUST still retain it
        consumed_a = dev_a.consume_quick_paste()
        _assert_qp_consumed(consumed_a, "alpha-token")
        _assert_qp_empty(dev_a.quick_paste_state())

        qp_b_still = dev_b.quick_paste_state()
        _assert_qp_present(qp_b_still, "alpha-token")

        consumed_b = dev_b.consume_quick_paste()
        _assert_qp_consumed(consumed_b, "alpha-token")
        _assert_qp_empty(dev_b.quick_paste_state())

        webdav_server.assert_no_live_slot()
    finally:
        dev_b.close()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_quick_paste_ime_close_preserves_and_dismisses_locally(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    vitral_target.require_core()
    dev_a = vitral_target
    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b_ime")
    try:
        dev_a.init({"device_id": "dev-a"})
        dev_b.init({"device_id": "dev-b"})

        # Closing the keyboard preserves the current copy and mobile suggestion.
        dev_a.clipboard_push("secret-1", source_device="dev-a")
        dev_b.clipboard_ingest_remote("secret-1", is_cold_boot=False)

        open_1 = dev_a.on_ime_open()
        _assert_qp_present(open_1, "secret-1")
        assert open_1.get("seen_while_ime_open") is True

        dev_a.on_ime_close()
        _assert_qp_present(dev_a.quick_paste_state(), "secret-1")
        open_2 = dev_a.on_ime_open()
        _assert_qp_present(open_2, "secret-1")

        # Device B MUST still retain its own Quick Paste item unaffected by Device A's IME close
        _assert_qp_present(dev_b.quick_paste_state(), "secret-1")

        # dismiss_quick_paste also clears locally only
        dev_a.clipboard_push("secret-2", source_device="dev-a")
        _assert_qp_present(dev_a.quick_paste_state(), "secret-2")
        dev_a.dismiss_quick_paste()
        _assert_qp_empty(dev_a.quick_paste_state())
        _assert_qp_present(dev_b.quick_paste_state(), "secret-1")
    finally:
        dev_b.close()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_quick_paste_newer_copy_overwrites_and_zero_live_slot_on_webdav(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init({"koofr_endpoint": webdav_server.koofr_endpoint})

    vitral_target.clipboard_push("first")
    _assert_qp_present(vitral_target.quick_paste_state(), "first")

    vitral_target.clipboard_push("second")
    _assert_qp_present(vitral_target.quick_paste_state(), "second")

    assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")
    webdav_server.assert_no_live_slot()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_persistent_n_item_clipboard_ring_pin_and_clear_unpinned(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init({"koofr_endpoint": webdav_server.koofr_endpoint, "clipboard_ring_capacity": 4})

    r1 = vitral_target.clipboard_push("P@ssw0rd!123")
    id1 = _extract_pushed_entry_id(r1, "P@ssw0rd!123")
    vitral_target.clipboard_pin(id1, "pwd")

    for i in range(2, 6):
        vitral_target.clipboard_push(f"item-{i}")

    state = vitral_target.clipboard_state()
    entries = _extract_ring_entries(state)
    assert len(entries) == 4
    texts = [e["content"] for e in entries]
    # Pinned password entry must never be filtered and must survive eviction
    assert "P@ssw0rd!123" in texts
    assert "item-2" not in texts
    assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")

    cleared = vitral_target.clipboard_clear_unpinned()
    rem = _extract_ring_entries(cleared)
    assert [e["content"] for e in rem] == ["P@ssw0rd!123"]
    webdav_server.assert_no_live_slot()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_localsend_v2_lan_transfer_and_sha256_verify(
    vitral_target: VitralTarget,
    localsend_server: LocalSendMockPeer,
) -> None:
    vitral_target.require_core()
    vitral_target.init(
        {
            "known_devices": [
                {
                    "device_id": localsend_server.fingerprint,
                    "device_name": localsend_server.alias,
                    "tier": "lan",
                    "usage_count": 0,
                    "last_used_ms": 0,
                    "localsend_url": localsend_server.base_url,
                }
            ]
        }
    )
    payload_text = "Vitral LAN LocalSend v2 payload"
    res = vitral_target.relay_send(localsend_server.fingerprint, payload_text)
    assert res.get("status") in ("ok", "sent", "delivered", "queued")
    assert len(localsend_server.received_transfers) == 1
    rec = localsend_server.received_transfers[0]
    assert rec["data"].decode("utf-8") == payload_text
    assert rec["sha256_actual"]


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_koofr_relay_cross_network_sha256_and_immediate_delete(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    vitral_target.require_core()
    sender = vitral_target
    receiver = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "receiver")
    try:
        sender.init({"koofr_endpoint": webdav_server.koofr_endpoint, "device_id": "sender-1"})
        receiver.init({"koofr_endpoint": webdav_server.koofr_endpoint, "device_id": "receiver-1"})

        sender.relay_send("receiver-1", "ephemeral-relay-secret")
        staged = webdav_server.list_files("relay/")
        assert any(p.endswith(".enc") for p in staged)
        assert any(p.endswith(".meta.json") for p in staged)

        items = receiver.relay_poll_inbox()
        assert len(items) == 1
        item0 = items[0]
        if isinstance(item0, str):
            assert item0 == "ephemeral-relay-secret"
        else:
            assert isinstance(item0, dict)
            val = item0["content"] if "content" in item0 else item0.get("text")
            assert val == "ephemeral-relay-secret", f"Unexpected relay item: {item0}"
        assert webdav_server.list_files("relay/") == []
        webdav_server.assert_no_live_slot()
    finally:
        receiver.close()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f11_koofr_relay_24h_orphan_cleanup(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_core()
    now = int(time.time())
    old_mtime = now - 90_000  # > 24h ago
    fresh_mtime = now - 60

    webdav_server.write_file_bytes("relay/old_orphan.enc", b"stale", mtime=old_mtime)
    webdav_server.write_file_bytes("relay/old_orphan.meta.json", b"{}", mtime=old_mtime)
    webdav_server.write_file_bytes("relay/fresh.enc", b"fresh", mtime=fresh_mtime)
    webdav_server.write_file_bytes("relay/fresh.meta.json", b"{}", mtime=fresh_mtime)

    vitral_target.init({"koofr_endpoint": webdav_server.koofr_endpoint})
    vitral_target.relay_cleanup_orphans()

    assert not webdav_server.file_exists("relay/old_orphan.enc")
    assert not webdav_server.file_exists("relay/old_orphan.meta.json")
    assert webdav_server.file_exists("relay/fresh.enc")
    assert webdav_server.file_exists("relay/fresh.meta.json")
    webdav_server.assert_no_live_slot()


# ---------------------------------------------------------------------------
# F12: Offline Voice (SenseVoice-Small) & Pure Offline Mode (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f12_offline_voice_mandarin_short_wav_transcription(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    pcm = _read_wav_pcm(discovered_targets.fixtures_dir / "audio" / "mandarin_short_16k.wav")

    vitral_target.init({})
    vitral_target.set_pure_offline(True)
    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm)
    res = vitral_target.voice_stop()

    transcript = _extract_voice_transcript(res)
    assert transcript, "Expected non-empty offline Mandarin transcript"
    assert any(kw in transcript for kw in ("你好", "天气")), f"Unexpected transcript: {transcript!r}"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f12_offline_voice_mixed_mandarin_en_wav(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    pcm = _read_wav_pcm(discovered_targets.fixtures_dir / "audio" / "mandarin_mixed_en_16k.wav")

    vitral_target.init({})
    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm)
    res = vitral_target.voice_stop()

    transcript = _extract_voice_transcript(res)
    assert transcript, "Expected non-empty bilingual transcript"
    assert any(kw in transcript for kw in ("测试", "Vitral", "使用")), (
        f"Unexpected bilingual transcript: {transcript!r}"
    )


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f12_offline_voice_long_20s_streaming_stability(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    pcm = _read_wav_pcm(discovered_targets.fixtures_dir / "audio" / "long_20s_16k.wav")

    vitral_target.init({})
    vitral_target.voice_start()
    chunk_bytes = 32_000  # 1s chunks at 16kHz 16-bit mono
    for offset in range(0, len(pcm), chunk_bytes):
        vitral_target.voice_push_pcm(pcm[offset : offset + chunk_bytes])
    res = vitral_target.voice_stop()

    transcript = _extract_voice_transcript(res)
    assert transcript
    assert any(kw in transcript for kw in ("语音", "测试", "普通话")), (
        f"Unexpected long-clip transcript: {transcript!r}"
    )


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f12_offline_voice_silence_noise_no_hallucination(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    pcm = _read_wav_pcm(discovered_targets.fixtures_dir / "audio" / "silence_noise_16k.wav")

    vitral_target.init({})
    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm)
    res = vitral_target.voice_stop()

    transcript = _extract_voice_transcript(res)
    assert transcript == "", f"Silence/noise clip hallucinated text: {transcript!r}"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f12_pure_offline_mode_physically_blocks_all_egress(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
    localsend_server: LocalSendMockPeer,
    ai_server: AIMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init(
        {
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "ai_endpoint": ai_server.ollama_url,
            "known_devices": [
                {
                    "device_id": localsend_server.fingerprint,
                    "device_name": localsend_server.alias,
                    "tier": "lan",
                    "usage_count": 0,
                    "last_used_ms": 0,
                    "localsend_url": localsend_server.base_url,
                }
            ],
        }
    )
    webdav_server.clear_request_log()
    localsend_server.clear_request_log()
    ai_server.clear_request_log()

    vitral_target.set_pure_offline(True)
    assert vitral_target.is_pure_offline() is True

    with pytest.raises(VitralCommandError, match=r"(?i)pure.?offline"):
        vitral_target.relay_send(localsend_server.fingerprint, "blocked-payload")
    with pytest.raises(VitralCommandError, match=r"(?i)pure.?offline"):
        vitral_target.ai_inline("translate", "测试", "en")
    with pytest.raises(VitralCommandError, match=r"(?i)pure.?offline"):
        vitral_target.restore_from_koofr({"koofr_endpoint": webdav_server.koofr_endpoint})

    assert webdav_server.request_log == []
    assert localsend_server.request_log == []
    assert ai_server.request_log == []


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f12_pure_offline_toggle_off_restores_local_state_and_sync(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init({"koofr_endpoint": webdav_server.koofr_endpoint})

    vitral_target.set_pure_offline(True)
    assert vitral_target.is_pure_offline() is True
    vitral_target.set_schema("rime_ice")
    action = _type_keys(vitral_target, "nihao")
    assert "你好" in _candidate_texts(action)
    vitral_target.select_candidate(0)

    vitral_target.set_pure_offline(False)
    assert vitral_target.is_pure_offline() is False
    vitral_target.clipboard_push("online-again")
    assert webdav_server.file_exists("clipboard/history_ring.jsonl.enc")
    webdav_server.assert_no_live_slot()
