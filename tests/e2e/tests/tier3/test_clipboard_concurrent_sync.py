"""Two devices editing one persistent WebDAV clipboard ring."""

from concurrent.futures import ThreadPoolExecutor
from threading import Barrier
from urllib.request import Request, urlopen

import pytest

from vitral_e2e.adapter import VitralTarget


@pytest.mark.tier3
@pytest.mark.requires_core
@pytest.mark.parametrize("path", ["/clipboard/history_ring.jsonl.enc", "/clipboard/current.json.enc"])
def test_clipboard_writers_retry_same_revision(
    vitral_target, discovered_targets, webdav_server, tmp_path, monkeypatch, path
):
    a = vitral_target
    b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "second-device")
    try:
        for device, name in ((a, "one"), (b, "two")):
            device.init({"device_id": name, "koofr_endpoint": webdav_server.koofr_endpoint})
        a.clipboard_push("initial")
        with urlopen(Request(webdav_server.vitral_root_url + path, method="HEAD")) as response:
            revision = response.headers["ETag"]

        # Both writers must see the seeded revision before either PUT can complete.
        writers = Barrier(2, timeout=10)
        put = webdav_server._handle_put

        def simultaneous_put(handler, target, headers, body):
            if target.endswith(path) and headers.get("if-match") == revision:
                writers.wait()
            return put(handler, target, headers, body)

        monkeypatch.setattr(webdav_server, "_handle_put", simultaneous_put)
        with ThreadPoolExecutor(max_workers=2) as pool:
            first = pool.submit(a.clipboard_push, "from one")
            second = pool.submit(b.clipboard_push, "from two")
            first.result()
            second.result()

        assert any(row["status_code"] == 412 for row in webdav_server.request_log)
        for device in (a, b):
            device.clipboard_poll()
            assert {item["content"] for item in device.clipboard_state()["ring"]} == {
                "initial", "from one", "from two"
            }
        assert a.quick_paste_state()["current_copy"] == b.quick_paste_state()["current_copy"]
    finally:
        b.close()
