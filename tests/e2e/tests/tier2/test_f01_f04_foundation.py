"""Tier 2 BVA and negative/corner-case tests for F01-F04 (Foundation)."""

from __future__ import annotations

import copy
import ctypes
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

import pytest
import yaml

from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets, discover_targets
from vitral_e2e.tokens import (
    EXPECTED_DARK_PALETTE,
    EXPECTED_LIGHT_PALETTE,
    assert_all_token_invariants,
    assert_english_only_brand,
    assert_no_toolbar_text_labels,
    load_design_tokens,
)

pytestmark = pytest.mark.tier2


def _repo_root(discovered_targets: DiscoveredTargets) -> Path:
    return discovered_targets.vitral_root


def _env_with_scripts_env(repo_root: Path) -> dict[str, str]:
    env_sh = repo_root / "scripts" / "env.sh"
    assert env_sh.is_file(), f"Missing scripts/env.sh at {env_sh}"
    proc = subprocess.run(
        ["bash", "-lc", f"source '{env_sh}' && env -0"],
        capture_output=True,
        check=True,
    )
    out: dict[str, str] = {}
    for entry in proc.stdout.split(b"\x00"):
        if not entry or b"=" not in entry:
            continue
        k, v = entry.split(b"=", 1)
        out[k.decode("utf-8", errors="replace")] = v.decode("utf-8", errors="replace")
    return out


# ======================================================================
# F01: Toolchains & Workspace Scaffolding (5 tests)
# ======================================================================


def test_f01_workspace_members_exact_boundaries(discovered_targets: DiscoveredTargets) -> None:
    root = _repo_root(discovered_targets)
    cargo_toml = root / "Cargo.toml"
    assert cargo_toml.is_file(), f"Missing Cargo.toml: {cargo_toml}"
    cargo_data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))

    expected_members = {
        "crates/vitral-tokens",
        "crates/vitral-core",
        "crates/vitral-ffi",
        "crates/vitral-jni",
        "crates/vitral-tsf",
        "crates/vitral-cli",
    }
    actual_members = set(cargo_data.get("workspace", {}).get("members", []))
    assert actual_members == expected_members, (
        f"Cargo workspace members mismatch: {sorted(actual_members)} != {sorted(expected_members)}"
    )
    for member in expected_members:
        member_toml = root / member / "Cargo.toml"
        assert member_toml.is_file(), f"Missing workspace member Cargo.toml: {member_toml}"

    excluded = set(cargo_data.get("workspace", {}).get("exclude", []))
    assert "apps/desktop-tauri/src-tauri" in excluded

    pnpm_ws = root / "pnpm-workspace.yaml"
    assert pnpm_ws.is_file(), f"Missing pnpm-workspace.yaml: {pnpm_ws}"
    ws_data = yaml.safe_load(pnpm_ws.read_text(encoding="utf-8"))
    packages = list(ws_data.get("packages", []))
    assert "apps/desktop-tauri" in packages


def test_f01_cross_target_triple_readiness(discovered_targets: DiscoveredTargets) -> None:
    root = _repo_root(discovered_targets)
    env = _env_with_scripts_env(root)
    rustup_bin = shutil.which("rustup", path=env.get("PATH", ""))
    assert rustup_bin is not None, "rustup not found after sourcing scripts/env.sh"

    res = subprocess.run(
        [rustup_bin, "target", "list", "--installed"],
        capture_output=True,
        text=True,
        env=env,
        check=True,
    )
    installed = {line.strip() for line in res.stdout.splitlines() if line.strip()}
    required_triples = {
        "x86_64-unknown-linux-gnu",
        "aarch64-linux-android",
        "armv7-linux-androideabi",
        "x86_64-linux-android",
        "x86_64-pc-windows-msvc",
    }
    missing = required_triples - installed
    assert not missing, f"Missing Rust cross-compilation targets: {sorted(missing)}"

    for tool in ("cargo-ndk", "cargo-xwin", "cargo-tauri", "cbindgen", "pnpm"):
        resolved = shutil.which(tool, path=env.get("PATH", ""))
        assert resolved is not None, f"Missing required user-space tool {tool!r} in PATH"
        assert not resolved.startswith("/usr/local/"), (
            f"Tool {tool!r} unexpectedly installed under privileged /usr/local: {resolved}"
        )


def test_f01_setup_toolchains_negative_missing_env(
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    root = _repo_root(discovered_targets)
    setup_script = root / "scripts" / "setup-toolchains.sh"
    assert setup_script.is_file(), f"Missing setup-toolchains.sh: {setup_script}"

    isolated_home = tmp_path / "isolated_home"
    isolated_home.mkdir()
    isolated_env = {
        "HOME": str(isolated_home),
        "PATH": "/usr/bin:/bin",
        "CARGO_HOME": str(tmp_path / "missing_cargo"),
        "RUSTUP_HOME": str(tmp_path / "missing_rustup"),
        "ANDROID_HOME": str(tmp_path / "missing_android_sdk"),
        "ANDROID_SDK_ROOT": str(tmp_path / "missing_android_sdk"),
        "XWIN_CACHE_DIR": str(tmp_path / "missing_xwin"),
    }
    proc = subprocess.run(
        ["bash", str(setup_script), "--verify-only"],
        capture_output=True,
        text=True,
        env=isolated_env,
        check=False,
    )
    assert proc.returncode != 0, "setup-toolchains.sh --verify-only must fail when toolchains are missing"
    assert "[MISS]" in proc.stdout, f"Expected [MISS] diagnostic, got stdout={proc.stdout!r}"
    assert not (tmp_path / "missing_cargo").exists()
    assert not (tmp_path / "missing_android_sdk").exists()


@pytest.mark.requires_core
def test_f01_generated_artifact_discovery_boundary(
    discovered_targets: DiscoveredTargets,
    vitral_target: VitralTarget,
    tmp_path: Path,
) -> None:
    empty_root = tmp_path / "empty_vitral_repo"
    empty_root.mkdir()
    empty_discovered = discover_targets(empty_root)
    assert empty_discovered.vitral_cli is None
    assert empty_discovered.vitral_ffi_lib is None
    assert empty_discovered.fcitx5_plugin is None
    assert empty_discovered.android_apk is None
    assert empty_discovered.windows_tauri_exe is None

    empty_target = VitralTarget(targets=empty_discovered, state_dir=tmp_path / "empty_state")
    try:
        with pytest.raises(pytest.fail.Exception):
            empty_target.require_core()
    finally:
        empty_target.close()

    core_path = vitral_target.require_core()
    assert core_path.is_file(), f"Resolved core artifact does not exist: {core_path}"


def test_f01_user_space_env_sh_isolation(
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    root = _repo_root(discovered_targets)
    env_sh = root / "scripts" / "env.sh"
    assert env_sh.is_file(), f"Missing scripts/env.sh: {env_sh}"

    fake_home = tmp_path / "user_home"
    (fake_home / ".local" / "bin").mkdir(parents=True)
    (fake_home / ".cargo" / "bin").mkdir(parents=True)

    proc = subprocess.run(
        [
            "bash",
            "-lc",
            (
                f"export HOME='{fake_home}' PATH='/usr/bin:/bin'; "
                f"unset CARGO_HOME RUSTUP_HOME ANDROID_HOME ANDROID_SDK_ROOT ANDROID_NDK_HOME XWIN_CACHE_DIR JAVA_HOME; "
                f"source '{env_sh}' && source '{env_sh}' && "
                "printf 'CARGO_HOME=%s\nRUSTUP_HOME=%s\nANDROID_HOME=%s\nXWIN_CACHE_DIR=%s\nPATH=%s\n' "
                "\"$CARGO_HOME\" \"$RUSTUP_HOME\" \"$ANDROID_HOME\" \"$XWIN_CACHE_DIR\" \"$PATH\""
            ),
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    kv = dict(line.split("=", 1) for line in proc.stdout.strip().splitlines() if "=" in line)
    assert kv["CARGO_HOME"] == str(fake_home / ".cargo")
    assert kv["RUSTUP_HOME"] == str(fake_home / ".rustup")
    assert kv["ANDROID_HOME"] == str(fake_home / "Android" / "Sdk")
    assert kv["XWIN_CACHE_DIR"] == str(fake_home / ".cache" / "cargo-xwin")
    for path_val in (kv["CARGO_HOME"], kv["RUSTUP_HOME"], kv["ANDROID_HOME"], kv["XWIN_CACHE_DIR"]):
        assert not path_val.startswith("/usr/"), f"Non-user-space path detected: {path_val}"
        assert not path_val.startswith("/root"), f"Root path detected: {path_val}"
    path_entries = kv["PATH"].split(":")
    assert path_entries.count(str(fake_home / ".local" / "bin")) == 1
    assert path_entries.count(str(fake_home / ".cargo" / "bin")) == 1


# ======================================================================
# F02: Design Token Codegen & Conformance Validator (5 tests)
# ======================================================================


@pytest.mark.tokens
def test_f02_prototype_reconciled_numeric_boundaries(design_tokens: dict[str, Any]) -> None:
    assert design_tokens["brand"]["name"] == "Vitral"
    assert design_tokens["handwriting"]["superimposedInkFadeOutMs"] == 420
    assert design_tokens["handwriting"]["superimposedInkFadeOutMs"] != 400
    assert design_tokens["handwriting"]["retroactiveHighlightDurationMs"] == 600
    assert design_tokens["handwriting"]["retroactiveHighlightDurationMs"] != 900
    assert float(design_tokens["handwriting"]["strokeWidthPx"]) == pytest.approx(3.5)
    assert float(design_tokens["handwriting"]["strokeWidthPx"]) != pytest.approx(3.2)
    assert float(design_tokens["motion"]["desktopIdleOpacity"]) == pytest.approx(0.35)
    assert float(design_tokens["motion"]["desktopIdleOpacity"]) != pytest.approx(0.32)
    assert design_tokens["motion"]["desktopIdleFadeDelayMs"] == 5000

    floating = design_tokens["ergonomics"]["mobileFloatingWindow"]
    assert floating["minWidthDp"] == 220
    assert floating["minHeightDp"] == 165
    assert (floating["minWidthDp"], floating["minHeightDp"]) != (230, 175)

    typo = design_tokens["typography"]
    assert typo["candidateFontSizeRangePx"] == [13, 22]
    assert typo["candidateFontSizeRangePx"] != [13, 19]
    assert typo["candidateFontSizeDefaultPx"] == 16
    assert typo["candidateFontSizeDefaultPx"] != 15
    assert typo["toolbarTextLabelsAllowed"] is False
    assert float(typo["toolbarIconStrokeWidthPx"]) == pytest.approx(1.75)

    radius = design_tokens["radius"]
    assert radius["mobileKeycapPx"] == 10
    assert radius["candidateActivePillPx"] == 10
    assert radius["desktopCandidateWindowPx"] == 16
    assert radius["popoverPanelPx"] == 16
    assert radius["desktopStatusCapsulePx"] == 9999

    blur = design_tokens["blur"]
    assert blur["backdropStandardPx"] == 18
    assert blur["backdropFloatingWindowPx"] == 20


@pytest.mark.tokens
def test_f02_mutated_boundary_tokens_rejected(
    design_tokens: dict[str, Any],
    discovered_targets: DiscoveredTargets,
    tmp_path: Path,
) -> None:
    mutations: list[tuple[str, Any]] = [
        ("handwriting.ambiguousMarginThreshold", 0.149),
        ("handwriting.ambiguousMarginThreshold", 0.151),
        ("motion.desktopIdleOpacity", 0.32),
        ("typography.candidateFontSizeRangePx", [13, 19]),
        ("typography.toolbarTextLabelsAllowed", True),
        ("typography.toolbarIconStrokeWidthPx", 1.9),
        ("handwriting.retroactiveHighlightDurationMs", 900),
        ("handwriting.superimposedInkFadeOutMs", 400),
        ("ergonomics.mobileFloatingWindow.minWidthDp", 230),
        ("radius.mobileKeycapPx", 12),
        ("blur.backdropStandardPx", 16),
    ]
    gen_script = discovered_targets.gen_tokens_script
    assert gen_script is not None and gen_script.is_file()

    for idx, (dotted_key, bad_value) in enumerate(mutations):
        mutated = copy.deepcopy(design_tokens)
        parts = dotted_key.split(".")
        cursor: dict[str, Any] = mutated
        for part in parts[:-1]:
            cursor = cursor[part]
        cursor[parts[-1]] = bad_value

        with pytest.raises(AssertionError):
            assert_all_token_invariants(mutated)

        if dotted_key in {
            "typography.toolbarTextLabelsAllowed",
            "typography.toolbarIconStrokeWidthPx",
            "typography.candidateFontSizeRangePx",
            "motion.desktopIdleOpacity",
            "handwriting.retroactiveHighlightDurationMs",
        }:
            bad_json = tmp_path / f"bad_tokens_{idx}.json"
            bad_json.write_text(json.dumps(mutated, indent=2), encoding="utf-8")
            proc = subprocess.run(
                [
                    sys.executable,
                    str(gen_script),
                    "--check",
                    "--tokens",
                    str(bad_json),
                    "--root",
                    str(discovered_targets.vitral_root),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            assert proc.returncode != 0, (
                f"gen-tokens.py --check unexpectedly accepted mutated {dotted_key}={bad_value!r}"
            )


@pytest.mark.tokens
def test_f02_brand_english_only_negative_leaks() -> None:
    assert_english_only_brand("Vitral")
    assert_english_only_brand("Vitral Settings", strict_ascii_only=True)

    for bad_brand in ("PureIME", "pure-ime", "pure ime", "Vitral输入法", "Vitral 拼音输入法", ""):
        with pytest.raises(AssertionError):
            assert_english_only_brand(bad_brand)

    with pytest.raises(AssertionError):
        assert_english_only_brand("Vitral 玻璃", strict_ascii_only=True)


@pytest.mark.tokens
def test_f02_toolbar_text_label_and_stroke_rejection() -> None:
    valid_items = [
        {"id": "vitral-logo", "showLabel": False, "strokeWidthPx": 1.75},
        {"id": "lang-toggle", "label": "", "text": "", "stroke_width": 1.75},
    ]
    assert_no_toolbar_text_labels(valid_items)

    invalid_cases = [
        [{"id": "handwriting", "label": "手写", "strokeWidthPx": 1.75}],
        [{"id": "clipboard", "text": "Clipboard", "strokeWidthPx": 1.75}],
        [{"id": "settings", "showLabel": True, "strokeWidthPx": 1.75}],
        [{"id": "voice", "strokeWidthPx": 1.5}],
        [{"id": "voice", "strokeWidthPx": 1.9}],
        [{"id": "voice", "stroke_width": 2.2}],
    ]
    for bad_toolbar in invalid_cases:
        with pytest.raises(AssertionError):
            assert_no_toolbar_text_labels(bad_toolbar)


@pytest.mark.tokens
def test_f02_generated_rust_cpp_headers_token_parity(
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    root = _repo_root(discovered_targets)
    gen_script = discovered_targets.gen_tokens_script
    assert gen_script is not None and gen_script.is_file()

    proc = subprocess.run(
        [sys.executable, str(gen_script), "--check", "--root", str(root)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"gen-tokens.py --check failed: {proc.stderr}\n{proc.stdout}"

    rust_gen = (root / "crates" / "vitral-tokens" / "src" / "generated.rs").read_text(encoding="utf-8")
    linux_hpp = (root / "linux" / "fcitx5-vitral" / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")
    tsf_hpp = (root / "crates" / "vitral-tsf" / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")
    kt_gen = (
        root / "apps" / "android" / "app" / "src" / "main" / "java" / "dev" / "vitral" / "ime" / "theme" / "VitralTokens.kt"
    ).read_text(encoding="utf-8")
    css_gen = (root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.css").read_text(encoding="utf-8")

    assert linux_hpp == tsf_hpp, "Linux Fcitx5 and Windows TSF vitral_tokens.hpp must be byte-identical"

    for artifact_name, text in (
        ("generated.rs", rust_gen),
        ("vitral_tokens.hpp", tsf_hpp),
        ("VitralTokens.kt", kt_gen),
    ):
        assert "PureIME" not in text, f"Forbidden brand in {artifact_name}"
        assert '"Vitral"' in text, f"Missing Vitral brand in {artifact_name}"

    assert "backdrop_standard_px: 18" in rust_gen
    assert "desktop_idle_opacity: 0.35" in rust_gen
    assert "superimposed_ink_fade_out_ms: 420" in rust_gen
    assert "retroactive_highlight_duration_ms: 600" in rust_gen
    assert "candidate_font_size_range_px: [13, 22]" in rust_gen

    assert "kBackdropStandardPx = 18;" in tsf_hpp
    assert "kDesktopIdleOpacity = 0.35f;" in tsf_hpp
    assert "kRetroactiveHighlightDurationMs = 600;" in tsf_hpp
    assert "kToolbarTextLabelsAllowed = false;" in tsf_hpp

    assert "--vitral-blur-standard: 18px;" in css_gen
    assert "--vitral-hw-fade-out: 420ms;" in css_gen
    assert "--vitral-hw-retroactive-glow: 600ms;" in css_gen
    assert "--vitral-idle-opacity: 0.35;" in css_gen
    assert "--vitral-candidate-font-size-min: 13px;" in css_gen
    assert "--vitral-candidate-font-size-max: 22px;" in css_gen

    for val in EXPECTED_DARK_PALETTE.values():
        assert val in rust_gen and val in tsf_hpp and val in css_gen
    for val in EXPECTED_LIGHT_PALETTE.values():
        assert val in rust_gen and val in tsf_hpp and val in css_gen

    assert load_design_tokens(discovered_targets.design_tokens_path)["brand"]["version"] == design_tokens["brand"]["version"]


# ======================================================================
# F03: Automated Asset Fetch/Cache Scripts (5 tests)
# ======================================================================


def test_f03_assets_manifest_schema_and_sha256_bounds(discovered_targets: DiscoveredTargets) -> None:
    root = _repo_root(discovered_targets)
    manifest_path = root / "scripts" / "assets-manifest.json"
    assert manifest_path.is_file(), f"Missing assets-manifest.json: {manifest_path}"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

    assets = manifest.get("assets", [])
    assert isinstance(assets, list) and len(assets) >= 6
    seen_ids: set[str] = set()
    seen_groups: set[str] = set()
    sha_re = re.compile(r"^[0-9a-f]{64}$")

    for entry in assets:
        aid = entry["id"]
        assert aid not in seen_ids, f"Duplicate asset id: {aid}"
        seen_ids.add(aid)
        seen_groups.add(entry["group"])
        assert sha_re.match(entry["sha256"]), f"Invalid sha256 for {aid}: {entry['sha256']!r}"
        assert isinstance(entry["size"], int) and entry["size"] > 0
        assert entry["url"].startswith(("https://", "http://"))
        verify_files = entry.get("verify_files", [])
        assert isinstance(verify_files, list) and len(verify_files) >= 1

    assert {"rime", "grammar", "opencc", "handwriting", "voice", "reranker"} <= seen_groups


def _write_synthetic_manifest_and_asset(
    tmp_path: Path,
    payload: bytes = b"vitral-synthetic-asset-v1\n",
    url: str = "http://127.0.0.1:1/unreachable.bin",
) -> tuple[Path, Path, Path, Path]:
    cache_dir = tmp_path / "cache"
    assets_dir = tmp_path / "assets"
    cache_dir.mkdir(parents=True, exist_ok=True)
    (assets_dir / "synthetic").mkdir(parents=True, exist_ok=True)

    sha = hashlib.sha256(payload).hexdigest()
    manifest_path = tmp_path / "manifest.json"
    manifest_data = {
        "schema_version": 1,
        "project": "Vitral",
        "assets": [
            {
                "id": "synthetic-raw-asset",
                "group": "rime",
                "required": True,
                "archive_type": "raw",
                "url": url,
                "filename": "synthetic.bin",
                "size": len(payload),
                "sha256": sha,
                "license": "MIT",
                "stage_dir": "assets/synthetic",
                "stage_filename": "synthetic.bin",
                "verify_files": ["synthetic.bin"],
            }
        ],
    }
    manifest_path.write_text(json.dumps(manifest_data, indent=2), encoding="utf-8")
    staged_file = assets_dir / "synthetic" / "synthetic.bin"
    staged_file.write_bytes(payload)
    return manifest_path, cache_dir, assets_dir, staged_file


def test_f03_fetch_assets_corrupt_sha256_negative(
    vitral_target: VitralTarget,
    tmp_path: Path,
) -> None:
    fetch_script = vitral_target.require_fetch_assets_script()
    manifest_path, cache_dir, assets_dir, staged_file = _write_synthetic_manifest_and_asset(tmp_path)

    # Flip 1 bit in staged file while preserving exact byte length
    orig = bytearray(staged_file.read_bytes())
    orig[0] ^= 0x01
    staged_file.write_bytes(bytes(orig))

    proc = subprocess.run(
        [
            "bash",
            str(fetch_script),
            "--manifest",
            str(manifest_path),
            "--cache-dir",
            str(cache_dir),
            "--assets-dir",
            str(assets_dir),
            "--verify",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode != 0, "fetch-assets.sh --verify must reject 1-bit-flipped asset"
    assert "SHA-256 mismatch" in proc.stderr


def test_f03_fetch_assets_missing_required_file_negative(
    vitral_target: VitralTarget,
    tmp_path: Path,
) -> None:
    fetch_script = vitral_target.require_fetch_assets_script()
    manifest_path, cache_dir, assets_dir, staged_file = _write_synthetic_manifest_and_asset(tmp_path)
    staged_file.unlink()

    proc = subprocess.run(
        [
            "bash",
            str(fetch_script),
            "--manifest",
            str(manifest_path),
            "--cache-dir",
            str(cache_dir),
            "--assets-dir",
            str(assets_dir),
            "--verify",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode != 0, "fetch-assets.sh --verify must fail when verify_files target is missing"
    assert "missing" in proc.stderr.lower()


def test_f03_fetch_assets_cached_hit_offline_idempotent(
    vitral_target: VitralTarget,
    tmp_path: Path,
) -> None:
    fetch_script = vitral_target.require_fetch_assets_script()
    payload = b"vitral-cached-hit-offline-check\n"
    manifest_path, cache_dir, assets_dir, staged_file = _write_synthetic_manifest_and_asset(
        tmp_path,
        payload=payload,
        url="http://127.0.0.1:1/must_never_be_fetched.bin",
    )
    staged_file.unlink()
    (cache_dir / "synthetic.bin").write_bytes(payload)

    # Run WITHOUT --verify; valid cache must satisfy staging purely offline
    proc = subprocess.run(
        [
            "bash",
            str(fetch_script),
            "--manifest",
            str(manifest_path),
            "--cache-dir",
            str(cache_dir),
            "--assets-dir",
            str(assets_dir),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"Cached hit failed: {proc.stderr}\n{proc.stdout}"
    assert "[CACHE]" in proc.stdout
    assert "[FETCH]" not in proc.stdout
    assert staged_file.read_bytes() == payload


def test_f03_staged_assets_checksum_completeness(vitral_target: VitralTarget) -> None:
    fetch_script = vitral_target.require_fetch_assets_script()
    manifest_path = fetch_script.parent / "assets-manifest.json"
    assert manifest_path.is_file(), f"Missing assets-manifest.json: {manifest_path}"
    expected_count = len(
        json.loads(manifest_path.read_text(encoding="utf-8"))["assets"]
    )
    assert expected_count > 0
    proc = subprocess.run(
        ["bash", str(fetch_script), "--verify"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"Staged assets verification failed:\n{proc.stderr}\n{proc.stdout}"
    assert f"All {expected_count} asset entries verified cleanly" in proc.stdout


# ======================================================================
# F04: Frozen C-ABI & JNI Boundary Contracts (5 tests)
# ======================================================================


class _VitralStrokePoint(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_float),
        ("y", ctypes.c_float),
        ("timestamp_ms", ctypes.c_uint64),
    ]


class _VitralStringResult(ctypes.Structure):
    _fields_ = [
        ("status", ctypes.c_int32),
        ("json_utf8", ctypes.c_void_p),
        ("error_message", ctypes.c_void_p),
    ]


@pytest.mark.requires_core
def test_f04_ffi_abi_version_and_symbol_parity(vitral_target: VitralTarget) -> None:
    header_path = vitral_target.require_ffi_header()
    header_text = header_path.read_text(encoding="utf-8")
    assert "#define VITRAL_ABI_VERSION 1" in header_text

    cdll = vitral_target.load_ffi_cdll()
    cdll.vitral_abi_version.restype = ctypes.c_uint32
    assert cdll.vitral_abi_version() == 1

    required_symbols = [
        "vitral_abi_version",
        "vitral_init",
        "vitral_shutdown",
        "vitral_set_pure_offline",
        "vitral_is_pure_offline",
        "vitral_set_schema",
        "vitral_set_opencc_mode",
        "vitral_reload_assets",
        "vitral_free_string",
        "vitral_free_result",
        "vitral_on_app_focus",
        "vitral_process_key",
        "vitral_select_candidate",
        "vitral_pin_candidate",
        "vitral_erase_candidate",
        "vitral_hw_begin",
        "vitral_hw_push_stroke",
        "vitral_hw_end",
        "vitral_hw_clear",
        "vitral_hw_override_slot",
        "vitral_import_lexicon",
        "vitral_clipboard_push",
        "vitral_clipboard_state",
        "vitral_clipboard_ingest_remote",
        "vitral_quick_paste_state",
        "vitral_consume_quick_paste",
        "vitral_on_ime_open",
        "vitral_on_ime_close",
        "vitral_dismiss_quick_paste",
        "vitral_clipboard_pin",
        "vitral_clipboard_clear_unpinned",
        "vitral_relay_devices_mru",
        "vitral_relay_send",
        "vitral_relay_poll_inbox",
        "vitral_relay_cleanup_orphans",
        "vitral_voice_start",
        "vitral_voice_push_pcm",
        "vitral_voice_stop",
        "vitral_ai_inline",
        "vitral_restore_from_koofr",
    ]
    for sym in required_symbols:
        assert sym in header_text, f"Symbol {sym} missing from vitral_core.h"
        assert hasattr(cdll, sym), f"C-ABI symbol {sym} missing from libvitral_ffi.so"


@pytest.mark.requires_core
def test_f04_ffi_null_pointer_boundary_safety(vitral_target: VitralTarget) -> None:
    cdll = vitral_target.load_ffi_cdll()
    cdll.vitral_init.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)]
    cdll.vitral_init.restype = ctypes.c_int32
    cdll.vitral_process_key.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_int32]
    cdll.vitral_process_key.restype = _VitralStringResult
    cdll.vitral_hw_push_stroke.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(_VitralStrokePoint),
        ctypes.c_size_t,
        ctypes.c_bool,
    ]
    cdll.vitral_hw_push_stroke.restype = _VitralStringResult
    cdll.vitral_clipboard_push.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    cdll.vitral_clipboard_push.restype = _VitralStringResult
    cdll.vitral_free_result.argtypes = [_VitralStringResult]
    cdll.vitral_free_string.argtypes = [ctypes.c_void_p]

    err_ptr = ctypes.c_void_p()
    eng_ptr = ctypes.c_void_p()
    assert cdll.vitral_init(None, ctypes.byref(eng_ptr), ctypes.byref(err_ptr)) == 1
    if err_ptr.value:
        cdll.vitral_free_string(err_ptr)

    res_key = cdll.vitral_process_key(None, ord("a"), 0, 0)
    assert res_key.status == 1
    cdll.vitral_free_result(res_key)

    res_hw = cdll.vitral_hw_push_stroke(None, None, 1, True)
    assert res_hw.status == 1
    cdll.vitral_free_result(res_hw)

    res_clip = cdll.vitral_clipboard_push(None, None)
    assert res_clip.status == 1
    cdll.vitral_free_result(res_clip)


@pytest.mark.requires_core
def test_f04_ffi_invalid_utf8_and_json_rejection(vitral_target: VitralTarget, tmp_path: Path) -> None:
    cdll = vitral_target.load_ffi_cdll()
    cdll.vitral_init.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)]
    cdll.vitral_init.restype = ctypes.c_int32
    cdll.vitral_shutdown.argtypes = [ctypes.c_void_p]
    cdll.vitral_on_app_focus.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    cdll.vitral_on_app_focus.restype = _VitralStringResult
    cdll.vitral_clipboard_push.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    cdll.vitral_clipboard_push.restype = _VitralStringResult
    cdll.vitral_free_result.argtypes = [_VitralStringResult]
    cdll.vitral_free_string.argtypes = [ctypes.c_void_p]

    eng_ptr = ctypes.c_void_p()
    err_ptr = ctypes.c_void_p()
    assert cdll.vitral_init(b"{bad", ctypes.byref(eng_ptr), ctypes.byref(err_ptr)) in (3, 7)
    if err_ptr.value:
        cdll.vitral_free_string(err_ptr)

    cfg = json.dumps({"state_dir": str(tmp_path / "ffi_state")}).encode("utf-8")
    assert cdll.vitral_init(cfg, ctypes.byref(eng_ptr), ctypes.byref(err_ptr)) == 0
    try:
        res_utf8 = cdll.vitral_on_app_focus(eng_ptr, b"\xff\xfe\xfd")
        assert res_utf8.status == 2
        cdll.vitral_free_result(res_utf8)

        res_bad_json = cdll.vitral_clipboard_push(eng_ptr, b"{bad_json")
        assert res_bad_json.status in (3, 7)
        cdll.vitral_free_result(res_bad_json)
    finally:
        cdll.vitral_shutdown(eng_ptr)


@pytest.mark.requires_core
def test_f04_ffi_uninitialized_and_oob_index_errors(vitral_target: VitralTarget, tmp_path: Path) -> None:
    cdll = vitral_target.load_ffi_cdll()
    cdll.vitral_set_pure_offline.argtypes = [ctypes.c_void_p, ctypes.c_bool]
    cdll.vitral_set_pure_offline.restype = ctypes.c_int32
    cdll.vitral_init.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)]
    cdll.vitral_init.restype = ctypes.c_int32
    cdll.vitral_shutdown.argtypes = [ctypes.c_void_p]
    cdll.vitral_select_candidate.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    cdll.vitral_select_candidate.restype = _VitralStringResult
    cdll.vitral_hw_override_slot.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_size_t]
    cdll.vitral_hw_override_slot.restype = _VitralStringResult
    cdll.vitral_free_result.argtypes = [_VitralStringResult]

    assert cdll.vitral_set_pure_offline(None, True) in (1, 4)

    eng_ptr = ctypes.c_void_p()
    err_ptr = ctypes.c_void_p()
    cfg = json.dumps({"state_dir": str(tmp_path / "ffi_oob")}).encode("utf-8")
    assert cdll.vitral_init(cfg, ctypes.byref(eng_ptr), ctypes.byref(err_ptr)) == 0
    try:
        res_sel = cdll.vitral_select_candidate(eng_ptr, 9999)
        assert res_sel.status in (3, 8)
        cdll.vitral_free_result(res_sel)

        res_ovr = cdll.vitral_hw_override_slot(eng_ptr, 9999, 0)
        assert res_ovr.status in (3, 4, 8)
        cdll.vitral_free_result(res_ovr)
    finally:
        cdll.vitral_shutdown(eng_ptr)


@pytest.mark.requires_android
def test_f04_jni_exported_symbols_cover_quick_paste_and_hw(vitral_target: VitralTarget) -> None:
    jni_libs = vitral_target.require_android_jni()
    nm_bin = shutil.which("nm") or shutil.which("llvm-nm")
    assert nm_bin is not None, "nm/llvm-nm tool required to inspect libvitral_jni.so symbols"

    required_jni_syms = [
        "Java_dev_vitral_ime_core_VitralNative_abiVersion",
        "Java_dev_vitral_ime_core_VitralNative_nativeInit",
        "Java_dev_vitral_ime_core_VitralNative_processKey",
        "Java_dev_vitral_ime_core_VitralNative_hwBegin",
        "Java_dev_vitral_ime_core_VitralNative_hwPushStroke",
        "Java_dev_vitral_ime_core_VitralNative_hwEnd",
        "Java_dev_vitral_ime_core_VitralNative_hwOverrideSlot",
        "Java_dev_vitral_ime_core_VitralNative_clipboardIngestRemote",
        "Java_dev_vitral_ime_core_VitralNative_quickPasteState",
        "Java_dev_vitral_ime_core_VitralNative_consumeQuickPaste",
        "Java_dev_vitral_ime_core_VitralNative_onImeOpen",
        "Java_dev_vitral_ime_core_VitralNative_onImeClose",
        "Java_dev_vitral_ime_core_VitralNative_voiceStop",
    ]
    for lib_path in jni_libs:
        proc = subprocess.run([nm_bin, "-D", str(lib_path)], capture_output=True, text=True, check=True)
        for sym in required_jni_syms:
            assert sym in proc.stdout, f"JNI symbol {sym} missing from {lib_path}"
