"""Pytest fixtures for Vitral opaque-box E2E test suite."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Iterator

import pytest

from harnesses import AIMockServer, LocalSendMockPeer, WebDAVMockServer
from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets, discover_targets
from vitral_e2e.tokens import assert_all_token_invariants, load_design_tokens


@pytest.fixture(scope="session")
def design_tokens() -> dict[str, Any]:
    tokens = load_design_tokens()
    assert_all_token_invariants(tokens)
    return tokens


@pytest.fixture(scope="session")
def discovered_targets() -> DiscoveredTargets:
    return discover_targets()


@pytest.fixture(scope="function")
def vitral_target(tmp_path: Path, discovered_targets: DiscoveredTargets) -> Iterator[VitralTarget]:
    target = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "vitral_state")
    try:
        yield target
    finally:
        target.close()


@pytest.fixture(scope="function")
def webdav_server() -> Iterator[WebDAVMockServer]:
    srv = WebDAVMockServer(host="127.0.0.1", port=0)
    srv.start()
    try:
        yield srv
    finally:
        srv.stop()


@pytest.fixture(scope="function")
def localsend_server() -> Iterator[LocalSendMockPeer]:
    srv = LocalSendMockPeer(host="127.0.0.1", port=0)
    srv.start()
    try:
        yield srv
    finally:
        srv.stop()


@pytest.fixture(scope="function")
def ai_server() -> Iterator[AIMockServer]:
    srv = AIMockServer(host="127.0.0.1", port=0)
    srv.start()
    try:
        yield srv
    finally:
        srv.stop()
