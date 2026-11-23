"""Harness and runner smoke tests for E2E-M1."""

from __future__ import annotations

import email.utils
import hashlib
import json
import socket
import time
import xml.etree.ElementTree as ET
from pathlib import Path

import httpx
import pytest
import requests

from harnesses import AIMockServer, LocalSendMockPeer, WebDAVMockServer
from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import discover_targets
from vitral_e2e.tokens import (
    assert_all_token_invariants,
    assert_english_only_brand,
    assert_no_toolbar_text_labels,
    assert_prototype_reconciled_values,
    load_design_tokens,
)


@pytest.mark.harness
def test_webdav_preseeded_koofr_layout(webdav_server: WebDAVMockServer) -> None:
    assert webdav_server.root_path == "/dav/Koofr/Vitral"
    assert webdav_server.koofr_endpoint == f"{webdav_server.base_url}/dav/Koofr"
    assert webdav_server.webdav_url == f"{webdav_server.base_url}/dav/Koofr"
    assert webdav_server.vitral_root_url == f"{webdav_server.base_url}/dav/Koofr/Vitral"

    with httpx.Client(timeout=5.0) as client:
        opt = client.request("OPTIONS", webdav_server.vitral_root_url)
        assert opt.status_code == 200
        assert "1" in opt.headers.get("DAV", "")
        for method in ("PROPFIND", "GET", "HEAD", "PUT", "DELETE", "MKCOL", "MOVE", "COPY"):
            assert method in opt.headers.get("Allow", "")

        propfind_body = (
            '<?xml version="1.0" encoding="utf-8" ?>'
            '<D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>'
        )
        res = client.request(
            "PROPFIND",
            f"{webdav_server.vitral_root_url}/",
            headers={"Depth": "1", "Content-Type": "application/xml"},
            content=propfind_body,
        )
        assert res.status_code == 207

        root = ET.fromstring(res.text)
        ns = {"D": "DAV:"}
        hrefs = [el.text for el in root.findall(".//D:response/D:href", ns)]
        assert hrefs[0] == "/dav/Koofr/Vitral/"
        assert len(hrefs) == len(set(hrefs))
        for sub in ("lexicon", "profile", "clipboard", "relay"):
            assert f"/dav/Koofr/Vitral/{sub}/" in hrefs

        # Verify RFC 1123 GMT getlastmodified on every directory entry
        for mod_el in root.findall(".//D:response/D:propstat/D:prop/D:getlastmodified", ns):
            assert mod_el.text is not None
            parsed = email.utils.parsedate_to_datetime(mod_el.text)
            assert parsed is not None


@pytest.mark.harness
def test_webdav_crud_mkcol_put_get_head_move_copy_delete(webdav_server: WebDAVMockServer) -> None:
    with httpx.Client(timeout=5.0) as client:
        # PUT under missing parent -> 409 Conflict
        missing_put = client.put(
            f"{webdav_server.vitral_root_url}/no_such_dir/file.bin",
            content=b"abc",
        )
        assert missing_put.status_code == 409

        # MKCOL under missing parent -> 409 Conflict
        missing_mkcol = client.request(
            "MKCOL", f"{webdav_server.vitral_root_url}/no_such_dir/sub"
        )
        assert missing_mkcol.status_code == 409

        # MKCOL under existing parent -> 201, repeat -> 405
        mk = client.request("MKCOL", f"{webdav_server.vitral_root_url}/relay/batch1")
        assert mk.status_code == 201
        mk_dup = client.request("MKCOL", f"{webdav_server.vitral_root_url}/relay/batch1")
        assert mk_dup.status_code == 405

        payload = b"vitral-relay-payload-\x00\x01\x02"
        put_url = f"{webdav_server.vitral_root_url}/relay/batch1/item.enc"
        put_res = client.put(put_url, content=payload)
        assert put_res.status_code == 201
        assert "ETag" in put_res.headers
        assert "Last-Modified" in put_res.headers

        # Chunked upload works without truncation
        def _gen():
            yield b"chunk1-"
            yield b"chunk2"

        chunked_url = f"{webdav_server.vitral_root_url}/relay/batch1/chunked.bin"
        chunked_res = client.put(chunked_url, content=_gen())
        assert chunked_res.status_code == 201
        assert webdav_server.read_file_bytes("relay/batch1/chunked.bin") == b"chunk1-chunk2"

        head_res = client.head(put_url)
        assert head_res.status_code == 200
        assert int(head_res.headers["Content-Length"]) == len(payload)

        get_res = client.get(put_url)
        assert get_res.status_code == 200
        assert get_res.content == payload

        range_res = client.get(put_url, headers={"Range": "bytes=0-5"})
        assert range_res.status_code == 206
        assert range_res.content == payload[:6]

        # COPY and MOVE with Overwrite: F -> 412, Overwrite: T -> 204
        copy_dst = f"{webdav_server.vitral_root_url}/relay/batch1/copy.enc"
        cp_res = client.request("COPY", put_url, headers={"Destination": copy_dst})
        assert cp_res.status_code == 201

        cp_fail = client.request(
            "COPY", put_url, headers={"Destination": copy_dst, "Overwrite": "F"}
        )
        assert cp_fail.status_code == 412

        move_dst = f"{webdav_server.vitral_root_url}/relay/batch1/moved.enc"
        mv_res = client.request("MOVE", copy_dst, headers={"Destination": move_dst})
        assert mv_res.status_code == 201
        assert not webdav_server.file_exists("relay/batch1/copy.enc")
        assert webdav_server.file_exists("relay/batch1/moved.enc")

        listed = webdav_server.list_files("relay/batch1/")
        assert listed == [
            "relay/batch1/chunked.bin",
            "relay/batch1/item.enc",
            "relay/batch1/moved.enc",
        ]

        # Recursive DELETE of collection
        del_res = client.delete(f"{webdav_server.vitral_root_url}/relay/batch1/")
        assert del_res.status_code == 204
        assert webdav_server.list_files("relay/batch1/") == []
        assert client.get(put_url).status_code == 404
        assert any(entry["method"] == "PUT" for entry in webdav_server.request_log)


@pytest.mark.harness
def test_webdav_mtime_injection_and_error_injection(webdav_server: WebDAVMockServer) -> None:
    old_epoch = int(time.time()) - 90_000  # > 24h ago
    with httpx.Client(timeout=5.0) as client:
        url = f"{webdav_server.vitral_root_url}/relay/orphan.enc"
        put_res = client.put(
            url,
            content=b"orphan-bytes",
            headers={"X-Mock-Mtime": str(old_epoch)},
        )
        assert put_res.status_code == 201
        assert int(webdav_server.get_mtime("relay/orphan.enc")) == old_epoch

        # Mutate via set_mtime using relative & /Koofr/Vitral/ paths
        newer_epoch = old_epoch + 1000
        webdav_server.set_mtime("/Koofr/Vitral/relay/orphan.enc", newer_epoch)
        assert int(webdav_server.get_mtime("relay/orphan.enc")) == newer_epoch

        head_res = client.head(url)
        lm = email.utils.parsedate_to_datetime(head_res.headers["Last-Modified"]).timestamp()
        assert int(lm) == newer_epoch

        # Fault injection: one-shot 503 then recovery
        webdav_server.inject_error("PUT", 503, count=1)
        err_res = client.put(url, content=b"retry")
        assert err_res.status_code == 503
        ok_res = client.put(url, content=b"retry")
        assert ok_res.status_code == 204


@pytest.mark.harness
def test_localsend_info_and_register(localsend_server: LocalSendMockPeer) -> None:
    with httpx.Client(timeout=5.0) as client:
        info_res = client.get(f"{localsend_server.base_url}/api/localsend/v2/info")
        assert info_res.status_code == 200
        info = info_res.json()
        assert info["fingerprint"] == localsend_server.fingerprint
        assert info["version"] in ("2.0", "2.1")

        peer_payload = {
            "alias": "Peer-Linux",
            "version": "2.1",
            "deviceModel": "CachyOS",
            "deviceType": "desktop",
            "fingerprint": "peer-fp-001",
            "port": 53317,
            "protocol": "http",
        }
        reg_res = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/register",
            json=peer_payload,
        )
        assert reg_res.status_code == 200
        assert reg_res.json()["fingerprint"] == localsend_server.fingerprint
        assert "peer-fp-001" in localsend_server.registered_peers


@pytest.mark.harness
def test_localsend_prepare_upload_and_sha256_verification(localsend_server: LocalSendMockPeer) -> None:
    sender_info = {
        "alias": "Sender-Android",
        "version": "2.1",
        "deviceType": "mobile",
        "fingerprint": "sender-fp-001",
    }
    payload = b"hello vitral localsend payload!"
    digest = hashlib.sha256(payload).hexdigest()

    with httpx.Client(timeout=5.0) as client:
        # Empty files map -> 204 No Content
        empty_res = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/prepare-upload",
            json={"info": sender_info, "files": {}},
        )
        assert empty_res.status_code == 204

        # Valid prepare-upload -> upload
        prep = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/prepare-upload",
            json={
                "info": sender_info,
                "files": {
                    "f1": {
                        "id": "f1",
                        "fileName": "note.txt",
                        "size": len(payload),
                        "fileType": "text/plain",
                        "sha256": digest,
                    }
                },
            },
        )
        assert prep.status_code == 200
        prep_data = prep.json()
        session_id = prep_data["sessionId"]
        token = prep_data["files"]["f1"]

        # Wrong token -> 403
        bad_tok = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": session_id, "fileId": "f1", "token": "wrong"},
            content=payload,
        )
        assert bad_tok.status_code == 403

        # Wrong SHA-256 (same size, mutated bytes) -> 422
        tampered = b"X" + payload[1:]
        bad_sha = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": session_id, "fileId": "f1", "token": token},
            content=tampered,
        )
        assert bad_sha.status_code == 422
        assert len(localsend_server.received_transfers) == 0

        # Valid upload
        ok_up = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": session_id, "fileId": "f1", "token": token},
            content=payload,
        )
        assert ok_up.status_code == 200
        assert len(localsend_server.received_transfers) == 1
        rec = localsend_server.received_transfers[0]
        assert rec["data"] == payload
        assert rec["sha256_actual"] == digest


@pytest.mark.harness
def test_localsend_reject_next_and_cancel(localsend_server: LocalSendMockPeer) -> None:
    sender_info = {"alias": "Sender", "version": "2.1", "fingerprint": "fp-send"}
    req_body = {
        "info": sender_info,
        "files": {"f1": {"id": "f1", "fileName": "a.bin", "size": 3, "fileType": "application/octet-stream"}},
    }
    with httpx.Client(timeout=5.0) as client:
        localsend_server.reject_next(403)
        r403 = client.post(f"{localsend_server.base_url}/api/localsend/v2/prepare-upload", json=req_body)
        assert r403.status_code == 403

        localsend_server.reject_next(409)
        r409 = client.post(f"{localsend_server.base_url}/api/localsend/v2/prepare-upload", json=req_body)
        assert r409.status_code == 409

        prep = client.post(f"{localsend_server.base_url}/api/localsend/v2/prepare-upload", json=req_body)
        assert prep.status_code == 200
        sid = prep.json()["sessionId"]
        tok = prep.json()["files"]["f1"]

        cancel_res = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/cancel",
            params={"sessionId": sid},
        )
        assert cancel_res.status_code == 200

        blocked = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": sid, "fileId": "f1", "token": tok},
            content=b"123",
        )
        assert blocked.status_code == 403


@pytest.mark.harness
def test_localsend_udp_loopback_discovery() -> None:
    with LocalSendMockPeer(alias="Peer-A") as peer_a, LocalSendMockPeer(alias="Peer-B") as peer_b:
        assert peer_a.udp_port is not None and peer_b.udp_port is not None
        # Self announcement is filtered out
        peer_a.emit_udp_announcement("127.0.0.1", peer_a.udp_port)
        # Cross announcement is recorded
        peer_b.emit_udp_announcement("127.0.0.1", peer_a.udp_port)

        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline and not peer_a.discovered_peers:
            time.sleep(0.02)

        assert len(peer_a.discovered_peers) == 1
        assert peer_a.discovered_peers[0]["fingerprint"] == peer_b.fingerprint
        assert peer_a.discovered_peers[0]["alias"] == "Peer-B"


@pytest.mark.harness
def test_ai_mock_ollama_tags_and_generate(ai_server: AIMockServer) -> None:
    assert ai_server.ollama_url == ai_server.base_url
    assert ai_server.openai_url == f"{ai_server.base_url}/v1"

    tags = requests.get(f"{ai_server.ollama_url}/api/tags", timeout=5.0)
    assert tags.status_code == 200
    model_names = [m["name"] for m in tags.json()["models"]]
    assert "vitral-mock:latest" in model_names

    gen = requests.post(
        f"{ai_server.ollama_url}/api/generate",
        json={"model": "qwen2.5:7b", "prompt": "请翻译为英文：极简晶透毛玻璃设计"},
        timeout=5.0,
    )
    assert gen.status_code == 200
    data = gen.json()
    assert data["done"] is True
    assert "frosted-glass" in data["response"].lower()


@pytest.mark.harness
def test_ai_mock_four_vitral_modes_chat_and_openai(ai_server: AIMockServer) -> None:
    with httpx.Client(timeout=5.0) as client:
        # 1. Inline translate (EN / JA / DE)
        for marker, expected_sub in (
            ("翻译->EN：极简晶透毛玻璃设计", "frosted-glass"),
            ("翻译->JA：极简晶透毛玻璃设计", "すりガラス"),
            ("翻译->DE：极简晶透毛玻璃设计", "Milchglas"),
        ):
            r = client.post(
                f"{ai_server.ollama_url}/api/chat",
                json={"model": "qwen2.5:7b", "messages": [{"role": "user", "content": marker}]},
            )
            assert r.status_code == 200
            assert expected_sub in r.json()["message"]["content"]

        # 2. Spoken-to-written polish
        polish_res = client.post(
            f"{ai_server.openai_url}/chat/completions",
            json={
                "model": "qwen2.5:7b",
                "messages": [{"role": "user", "content": "口语润色：那个就是说今天下午三点然后开会啊"}],
            },
        )
        assert polish_res.status_code == 200
        polished = polish_res.json()["choices"][0]["message"]["content"]
        assert "那个" not in polished and "然后" not in polished

        # 3. Tier-3 constrained slot disambiguation -> strict JSON only
        ai_server.set_slot_choice(1)
        slot_res = client.post(
            f"{ai_server.openai_url}/chat/completions",
            json={
                "model": "qwen2.5:7b",
                "response_format": {"type": "json_object"},
                "messages": [
                    {
                        "role": "user",
                        "content": "混淆槽位消歧 candidate_slots: ['曝', '爆'] 上下文: _光老登金币",
                    }
                ],
            },
        )
        assert slot_res.status_code == 200
        raw_slot = slot_res.json()["choices"][0]["message"]["content"]
        assert "```" not in raw_slot
        assert json.loads(raw_slot) == {"slot_index": 1}

        # 4. P2 micro-capsule transcript summary
        sum_res = client.post(
            f"{ai_server.ollama_url}/api/chat",
            json={
                "model": "qwen2.5:7b",
                "messages": [
                    {"role": "user", "content": "提炼纪要：多端互传文件在校验接收后立即销毁，剪贴板保留置顶。"}
                ],
            },
        )
        assert sum_res.status_code == 200
        summary = sum_res.json()["message"]["content"]
        assert "1." in summary and "2." in summary


@pytest.mark.harness
def test_ai_mock_overrides_streaming_and_fault_injection(ai_server: AIMockServer) -> None:
    ai_server.set_response_override("custom-trigger", "OVERRIDE_OK")
    with httpx.Client(timeout=5.0) as client:
        ov = client.post(
            f"{ai_server.ollama_url}/api/chat",
            json={"model": "vitral-mock:latest", "messages": [{"role": "user", "content": "custom-trigger test"}]},
        )
        assert ov.status_code == 200
        assert ov.json()["message"]["content"] == "OVERRIDE_OK"

        # Ollama NDJSON streaming
        nd_res = client.post(
            f"{ai_server.ollama_url}/api/chat",
            json={
                "model": "vitral-mock:latest",
                "stream": True,
                "messages": [{"role": "user", "content": "custom-trigger stream"}],
            },
        )
        assert nd_res.status_code == 200
        nd_lines = [json.loads(line) for line in nd_res.text.strip().splitlines() if line.strip()]
        assert nd_lines[-1]["done"] is True
        assembled = "".join(item["message"]["content"] for item in nd_lines)
        assert assembled == "OVERRIDE_OK"

        # OpenAI SSE streaming
        sse_res = client.post(
            f"{ai_server.openai_url}/chat/completions",
            json={
                "model": "vitral-mock:latest",
                "stream": True,
                "messages": [{"role": "user", "content": "custom-trigger sse"}],
            },
        )
        assert sse_res.status_code == 200
        assert "data: [DONE]" in sse_res.text

        # Fault injection 429 on OpenAI endpoint
        ai_server.inject_error(429, "Rate limit exceeded", endpoint="/v1/chat/completions", once=True)
        err_res = client.post(
            f"{ai_server.openai_url}/chat/completions",
            json={"model": "vitral-mock:latest", "messages": [{"role": "user", "content": "hello"}]},
        )
        assert err_res.status_code == 429
        assert err_res.json()["error"]["code"] == "429"
        assert len(ai_server.request_log) >= 4


@pytest.mark.harness
def test_vitral_target_missing_artifacts_fail_cleanly(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    targets = discover_targets(tmp_path)
    assert targets.missing_artifacts()
    adapter = VitralTarget(root=tmp_path)

    for method in (
        adapter.require_core,
        adapter.require_cli,
        adapter.require_ffi,
        adapter.require_tokens,
        adapter.require_fcitx5,
        adapter.require_linux,
        adapter.require_android,
        adapter.require_android_apk,
        adapter.require_windows,
        adapter.require_windows_tsf,
        adapter.require_tauri,
    ):
        with pytest.raises(pytest.fail.Exception):
            method()

    for qp_call in (
        lambda: adapter.clipboard_ingest_remote("hello", is_cold_boot=False),
        adapter.quick_paste_state,
        adapter.consume_quick_paste,
        adapter.on_ime_open,
        adapter.on_ime_close,
        adapter.dismiss_quick_paste,
    ):
        with pytest.raises(pytest.fail.Exception):
            qp_call()

    owned_dir = adapter.state_dir
    assert owned_dir.is_dir()
    adapter.close()
    assert not owned_dir.exists()

    # Directory in VITRAL_CLI_BIN must not be treated as an executable file
    bogus_dir = tmp_path / "cli_dir"
    bogus_dir.mkdir()
    monkeypatch.setenv("VITRAL_CLI_BIN", str(bogus_dir))
    monkeypatch.setenv("VITRAL_CLI", str(bogus_dir))
    non_file_targets = discover_targets()
    assert non_file_targets.vitral_cli != bogus_dir.resolve()


@pytest.mark.harness
def test_discover_targets_cli_binary_names_and_root_isolation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    env_override_bin = tmp_path / "env_override_vitral"
    env_override_bin.write_text("#!/bin/sh\n", encoding="utf-8")
    monkeypatch.setenv("VITRAL_CLI_BIN", str(env_override_bin))

    # 1. Isolated root ignores env override and discovers target/debug/vitral
    debug_root = tmp_path / "repo_debug_vitral"
    debug_vitral = debug_root / "target" / "debug" / "vitral"
    debug_vitral.parent.mkdir(parents=True)
    debug_vitral.write_text("#!/bin/sh\n", encoding="utf-8")
    assert discover_targets(debug_root).vitral_cli == debug_vitral.resolve()

    # 2. target/release/vitral-cli is also discovered as fallback binary name
    legacy_root = tmp_path / "repo_release_vitral_cli"
    release_legacy = legacy_root / "target" / "release" / "vitral-cli"
    release_legacy.parent.mkdir(parents=True)
    release_legacy.write_text("#!/bin/sh\n", encoding="utf-8")
    assert discover_targets(legacy_root).vitral_cli == release_legacy.resolve()

    # 3. Priority order: target/release/vitral > target/debug/vitral > target/release/vitral-cli > target/debug/vitral-cli
    prio_root = tmp_path / "repo_priority"
    (prio_root / "target" / "debug").mkdir(parents=True)
    (prio_root / "target" / "release").mkdir(parents=True)
    p_dbg_legacy = prio_root / "target" / "debug" / "vitral-cli"
    p_rel_legacy = prio_root / "target" / "release" / "vitral-cli"
    p_dbg_vitral = prio_root / "target" / "debug" / "vitral"
    p_rel_vitral = prio_root / "target" / "release" / "vitral"
    for candidate in (p_dbg_legacy, p_rel_legacy, p_dbg_vitral, p_rel_vitral):
        candidate.write_text("#!/bin/sh\n", encoding="utf-8")
    assert discover_targets(prio_root).vitral_cli == p_rel_vitral.resolve()
    p_rel_vitral.unlink()
    assert discover_targets(prio_root).vitral_cli == p_dbg_vitral.resolve()
    p_dbg_vitral.unlink()
    assert discover_targets(prio_root).vitral_cli == p_rel_legacy.resolve()
    p_rel_legacy.unlink()
    assert discover_targets(prio_root).vitral_cli == p_dbg_legacy.resolve()

    # 4. Non-isolated discovery still respects VITRAL_CLI_BIN first
    assert discover_targets().vitral_cli == env_override_bin.resolve()


@pytest.mark.harness
def test_vitral_target_relay_poll_inbox_schemas(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    adapter = VitralTarget(root=tmp_path, state_dir=tmp_path / "state")
    responses: list[object] = [
        {"received": ["ephemeral-relay-secret"], "deleted_count": 1},
        {"received": [], "deleted_count": 0},
        {"received": None, "deleted_count": 0},
        {"received": [], "items": ["stale-fallback"]},
        {"items": [{"content": "legacy-relay-secret"}]},
        {"items": None},
        ["direct-list-item"],
        {},
    ]
    recorded_calls: list[tuple[str, object]] = []

    def _fake_invoke_cli(
        subcommand: str,
        payload: dict[str, object] | None = None,
        *,
        timeout: float = 15.0,
    ) -> object:
        recorded_calls.append((subcommand, payload))
        return responses.pop(0)

    monkeypatch.setattr(adapter, "_invoke_cli", _fake_invoke_cli)

    assert adapter.relay_poll_inbox() == ["ephemeral-relay-secret"]
    assert adapter.relay_poll_inbox() == []
    assert adapter.relay_poll_inbox() == []
    assert adapter.relay_poll_inbox() == []
    assert adapter.relay_poll_inbox() == [{"content": "legacy-relay-secret"}]
    assert adapter.relay_poll_inbox() == []
    assert adapter.relay_poll_inbox() == ["direct-list-item"]
    assert adapter.relay_poll_inbox() == []
    assert recorded_calls == [("relay-poll-inbox", {})] * 8


def _send_raw_http(host: str, port: int, raw: bytes) -> tuple[int, str]:
    with socket.create_connection((host, port), timeout=5.0) as sock:
        sock.sendall(raw)
        chunks: list[bytes] = []
        while True:
            data = sock.recv(4096)
            if not data:
                break
            chunks.append(data)
    text = b"".join(chunks).decode("utf-8", errors="replace")
    status_line = text.splitlines()[0] if text else ""
    parts = status_line.split()
    status_code = int(parts[1]) if len(parts) >= 2 and parts[1].isdigit() else -1
    return status_code, text


@pytest.mark.harness
def test_webdav_mtime_overflow_proppatch_range_move_and_no_live_slot(
    webdav_server: WebDAVMockServer,
) -> None:
    assert webdav_server.has_live_slot() is False
    webdav_server.assert_no_live_slot()

    with WebDAVMockServer() as isolated:
        assert isolated.has_live_slot() is False
        isolated.write_file_bytes("clipboard/live_slot.enc", b"forbidden")
        assert isolated.has_live_slot() is True
        with pytest.raises(AssertionError):
            isolated.assert_no_live_slot()

    with httpx.Client(timeout=5.0) as client:
        # Out-of-range mtime -> 400 without poisoning subsequent PROPFIND
        bad_put = client.put(
            f"{webdav_server.vitral_root_url}/relay/mtime_huge.bin",
            content=b"x",
            headers={"X-Mock-Mtime": "1e20"},
        )
        assert bad_put.status_code == 400
        assert not webdav_server.file_exists("relay/mtime_huge.bin")

        with pytest.raises(ValueError):
            webdav_server.write_file_bytes("relay/bad.bin", b"x", mtime=1e20)

        pf = client.request(
            "PROPFIND",
            f"{webdav_server.vitral_root_url}/relay/",
            headers={"Depth": "1", "Content-Type": "application/xml"},
            content='<?xml version="1.0" encoding="utf-8" ?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>',
        )
        assert pf.status_code == 207
        ET.fromstring(pf.text)

        # Valid PUT + set_mtime overflow guard + PROPPATCH -> PROPFIND persistence
        ok_url = f"{webdav_server.vitral_root_url}/profile/prefs.json"
        assert client.put(ok_url, content=b"abcdef").status_code == 201
        with pytest.raises(ValueError):
            webdav_server.set_mtime("profile/prefs.json", 1e20)

        proppatch_body = (
            '<?xml version="1.0" encoding="utf-8"?>'
            '<D:propertyupdate xmlns:D="DAV:" xmlns:V="https://vitral.dev/ns">'
            "<D:set><D:prop><V:device>arch-linux</V:device></D:prop></D:set>"
            "</D:propertyupdate>"
        )
        pp_res = client.request(
            "PROPPATCH",
            ok_url,
            headers={"Content-Type": "application/xml"},
            content=proppatch_body,
        )
        assert pp_res.status_code == 207

        pf_prop = client.request(
            "PROPFIND",
            ok_url,
            headers={"Depth": "0", "Content-Type": "application/xml"},
            content='<?xml version="1.0" encoding="utf-8" ?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>',
        )
        assert pf_prop.status_code == 207
        prop_root = ET.fromstring(pf_prop.text)
        dev_el = prop_root.find(".//{https://vitral.dev/ns}device")
        assert dev_el is not None and dev_el.text == "arch-linux"

        # Case-insensitive Range unit
        range_res = client.get(ok_url, headers={"Range": "BYTES=0-2"})
        assert range_res.status_code == 206
        assert range_res.content == b"abc"

        # MOVE with Depth != infinity -> 400 per RFC 4918 §9.9.2
        mv_bad = client.request(
            "MOVE",
            ok_url,
            headers={
                "Destination": f"{webdav_server.vitral_root_url}/profile/prefs_moved.json",
                "Depth": "0",
            },
        )
        assert mv_bad.status_code == 400


@pytest.mark.harness
def test_mock_servers_malformed_body_framing_return_400(
    webdav_server: WebDAVMockServer,
    localsend_server: LocalSendMockPeer,
    ai_server: AIMockServer,
) -> None:
    for raw_req in (
        b"PUT /dav/Koofr/Vitral/relay/bad.bin HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: abc\r\nConnection: close\r\n\r\n",
        b"PUT /dav/Koofr/Vitral/relay/bad2.bin HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nZZZZ\r\n0\r\n\r\n",
    ):
        status, _ = _send_raw_http("127.0.0.1", webdav_server.port, raw_req)
        assert status == 400

    for raw_req in (
        b"POST /api/localsend/v2/upload HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: -1\r\nConnection: close\r\n\r\n",
        b"POST /api/localsend/v2/upload HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nNOTHEX\r\n0\r\n\r\n",
    ):
        status, _ = _send_raw_http("127.0.0.1", localsend_server.port, raw_req)
        assert status == 400

    for raw_req in (
        b"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: abc\r\nConnection: close\r\n\r\n",
        b"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nZZZZ\r\n0\r\n\r\n",
    ):
        status, body = _send_raw_http("127.0.0.1", ai_server.port, raw_req)
        assert status == 400
        assert '"malformed_body"' in body


@pytest.mark.harness
def test_localsend_size_validation_duplicate_fileid_and_empty_pin(
    localsend_server: LocalSendMockPeer,
) -> None:
    sender_info = {"alias": "Sender", "version": "2.1", "fingerprint": "fp-reg"}
    with httpx.Client(timeout=5.0) as client:
        for bad_size in ("not-an-int", True, -1, None):
            file_entry: dict[str, object] = {"id": "f1", "fileName": "a.bin", "fileType": "application/octet-stream"}
            if bad_size is not None:
                file_entry["size"] = bad_size
            res = client.post(
                f"{localsend_server.base_url}/api/localsend/v2/prepare-upload",
                json={"info": sender_info, "files": {"f1": file_entry}},
            )
            assert res.status_code == 400

        # Duplicate fileId in multi-file session -> 409 Conflict
        prep = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/prepare-upload",
            json={
                "info": sender_info,
                "files": {
                    "f1": {"id": "f1", "fileName": "1.bin", "size": 2, "fileType": "application/octet-stream"},
                    "f2": {"id": "f2", "fileName": "2.bin", "size": 2, "fileType": "application/octet-stream"},
                },
            },
        )
        assert prep.status_code == 200
        sid = prep.json()["sessionId"]
        tok1 = prep.json()["files"]["f1"]
        tok2 = prep.json()["files"]["f2"]

        up1 = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": sid, "fileId": "f1", "token": tok1},
            content=b"ab",
        )
        assert up1.status_code == 200
        dup1 = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": sid, "fileId": "f1", "token": tok1},
            content=b"ab",
        )
        assert dup1.status_code == 409
        up2 = client.post(
            f"{localsend_server.base_url}/api/localsend/v2/upload",
            params={"sessionId": sid, "fileId": "f2", "token": tok2},
            content=b"cd",
        )
        assert up2.status_code == 200
        assert len(localsend_server.received_transfers) == 2

        # set_pin("") requires ?pin= parameter to be present
        localsend_server.set_pin("")
        try:
            no_pin = client.post(
                f"{localsend_server.base_url}/api/localsend/v2/prepare-upload",
                json={"info": sender_info, "files": {}},
            )
            assert no_pin.status_code == 401
            with_empty_pin = client.post(
                f"{localsend_server.base_url}/api/localsend/v2/prepare-upload",
                params={"pin": ""},
                json={"info": sender_info, "files": {}},
            )
            assert with_empty_pin.status_code == 204
        finally:
            localsend_server.set_pin(None)


@pytest.mark.harness
def test_ai_mock_v1_stream_envelope_polish_and_summary_edge_cases() -> None:
    with AIMockServer(default_stream=True) as srv, httpx.Client(timeout=5.0) as client:
        # /v1/chat/completions honors default_stream=True when stream is omitted
        sse = client.post(
            f"{srv.openai_url}/chat/completions",
            json={"model": "vitral-mock", "messages": [{"role": "user", "content": "hello"}]},
        )
        assert sse.status_code == 200
        assert "data: [DONE]" in sse.text

        # Non-object JSON body on /v1/chat/completions -> OpenAI envelope
        bad_arr = client.post(
            f"{srv.openai_url}/chat/completions",
            content=b"[1, 2, 3]",
            headers={"Content-Type": "application/json"},
        )
        assert bad_arr.status_code == 400
        err_obj = bad_arr.json()["error"]
        assert err_obj["type"] == "invalid_request_error"
        assert err_obj["code"] == "invalid_json"

        # Polish strips "就是说" cleanly without leaving "说"
        pol = client.post(
            f"{srv.openai_url}/chat/completions",
            json={
                "model": "vitral-mock",
                "stream": False,
                "messages": [{"role": "user", "content": "口语润色：那个就是说今天下午三点然后开会啊"}],
            },
        )
        assert pol.status_code == 200
        assert pol.json()["choices"][0]["message"]["content"] == "今天下午三点开会。"

        # Summary keeps real transcript lines containing "纪要"
        summ = client.post(
            f"{srv.openai_url}/chat/completions",
            json={
                "model": "vitral-mock",
                "stream": False,
                "messages": [{"role": "user", "content": "提炼纪要：讨论了会议纪要模板；确认周五发布。"}],
            },
        )
        assert summ.status_code == 200
        summary_text = summ.json()["choices"][0]["message"]["content"]
        assert "讨论了会议纪要模板" in summary_text
        assert "确认周五发布" in summary_text


@pytest.mark.harness
@pytest.mark.tokens
def test_design_tokens_v1_1_loaded_and_invariants(design_tokens: dict) -> None:
    loaded = load_design_tokens()
    assert_all_token_invariants(loaded)
    assert_prototype_reconciled_values(design_tokens)
    assert_english_only_brand("Vitral Frosted Glass IME", strict_ascii_only=True)
    with pytest.raises(AssertionError):
        assert_english_only_brand("PureIME")
    assert_no_toolbar_text_labels(
        [
            {"id": "vitral-logo", "stroke_width": 1.75},
            {"id": "lang-toggle", "strokeWidthPx": 1.75},
        ]
    )
    with pytest.raises(AssertionError):
        assert_no_toolbar_text_labels([{"id": "voice", "label": "Voice"}])
