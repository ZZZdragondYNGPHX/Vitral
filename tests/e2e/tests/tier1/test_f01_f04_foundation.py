"""Tier 1 Foundation tests for Vitral (F01-F04: Toolchains, Tokens, Assets, FFI/JNI)."""

from __future__ import annotations

import ctypes
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets
from vitral_e2e.tokens import (
    assert_all_token_invariants,
    assert_prototype_reconciled_values,
)

# All 41 frozen C-ABI symbols declared in crates/vitral-ffi/include/vitral_core.h
EXPECTED_FFI_SYMBOLS: tuple[str, ...] = (
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
    "vitral_process_touch_key",
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
)

# All 39 camelCase JNI entrypoints declared in crates/vitral-jni/src/lib.rs
EXPECTED_JNI_METHODS: tuple[str, ...] = (
    "abiVersion",
    "nativeInit",
    "shutdown",
    "setPureOffline",
    "isPureOffline",
    "setSchema",
    "setOpenccMode",
    "reloadAssets",
    "onAppFocus",
    "processKey",
    "processTouchKey",
    "selectCandidate",
    "pinCandidate",
    "eraseCandidate",
    "hwBegin",
    "hwPushStroke",
    "hwEnd",
    "hwClear",
    "hwOverrideSlot",
    "importLexicon",
    "clipboardPush",
    "clipboardState",
    "clipboardIngestRemote",
    "quickPasteState",
    "consumeQuickPaste",
    "onImeOpen",
    "onImeClose",
    "dismissQuickPaste",
    "clipboardPin",
    "clipboardClearUnpinned",
    "relayDevicesMru",
    "relaySend",
    "relayPollInbox",
    "relayCleanupOrphans",
    "voiceStart",
    "voicePushPcm",
    "voiceStop",
    "aiInline",
    "restoreFromKoofr",
)


class _VitralStringResult(ctypes.Structure):
    _fields_ = [
        ("status", ctypes.c_int),
        ("json_utf8", ctypes.c_void_p),
        ("error_message", ctypes.c_void_p),
    ]


# ---------------------------------------------------------------------------
# F01: Toolchains & Workspace Scaffolding (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
def test_f01_workspace_manifests_and_scripts_present(vitral_target: VitralTarget) -> None:
    root = vitral_target.targets.vitral_root
    for rel in (
        "Cargo.toml",
        "package.json",
        "pnpm-workspace.yaml",
        "scripts/env.sh",
        "scripts/setup-toolchains.sh",
        "scripts/gen-tokens.py",
        "scripts/fetch-assets.sh",
    ):
        assert (root / rel).is_file(), f"Missing workspace deliverable: {rel}"

    cargo_toml = (root / "Cargo.toml").read_text(encoding="utf-8")
    for member in (
        "crates/vitral-tokens",
        "crates/vitral-core",
        "crates/vitral-ffi",
        "crates/vitral-jni",
        "crates/vitral-tsf",
        "crates/vitral-cli",
    ):
        assert member in cargo_toml, f"Cargo.toml missing workspace member {member}"

    fetch_script = vitral_target.require_fetch_assets_script()
    assert fetch_script.is_file()


@pytest.mark.tier1
def test_f01_rust_toolchain_five_cross_targets_installed(
    discovered_targets: DiscoveredTargets,
) -> None:
    root = discovered_targets.vitral_root
    proc = subprocess.run(
        ["bash", "-lc", "source scripts/env.sh && rustup target list --installed"],
        cwd=root,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert proc.returncode == 0, f"rustup target list failed: {proc.stderr}"
    installed = {line.strip() for line in proc.stdout.splitlines() if line.strip()}
    expected_targets = {
        "x86_64-unknown-linux-gnu",
        "aarch64-linux-android",
        "armv7-linux-androideabi",
        "x86_64-linux-android",
        "x86_64-pc-windows-msvc",
    }
    missing = expected_targets - installed
    assert not missing, f"Missing rustup targets: {sorted(missing)}"


@pytest.mark.tier1
def test_f01_cross_compile_cli_tools_available(
    discovered_targets: DiscoveredTargets,
) -> None:
    root = discovered_targets.vitral_root
    proc = subprocess.run(
        ["bash", str(root / "scripts" / "setup-toolchains.sh"), "--verify-only"],
        cwd=root,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert proc.returncode == 0, (
        f"setup-toolchains.sh --verify-only failed (rc={proc.returncode}):\n"
        f"{proc.stdout}\n{proc.stderr}"
    )
    assert "[MISS]" not in proc.stdout, f"Missing tools reported:\n{proc.stdout}"
    for tool_marker in (
        "cargo-ndk",
        "cargo-xwin",
        "cargo-tauri",
        "cbindgen",
        "pnpm",
        "lld-link",
        "temurin-jdk17",
        "sdkmanager",
        "27.3.13750724",
    ):
        assert tool_marker in proc.stdout, f"Expected {tool_marker!r} in verify-only output"


@pytest.mark.tier1
def test_f01_git_repo_initialized_and_no_legacy_brand_paths(
    discovered_targets: DiscoveredTargets,
) -> None:
    root = discovered_targets.vitral_root
    assert (root / ".git").is_dir(), "Git repository must be initialized at project root"
    for dir_rel in (
        "apps/desktop-tauri",
        "apps/android",
        "linux/fcitx5-vitral",
        "crates",
        "tokens",
        "scripts",
        "tests/fixtures",
    ):
        assert (root / dir_rel).is_dir(), f"Missing workspace directory: {dir_rel}"

    for scan_dir in ("crates", "apps", "linux", "scripts", "tokens"):
        base = root / scan_dir
        for p in base.rglob("*"):
            if "node_modules" in p.parts or "target" in p.parts:
                continue
            assert "pureime" not in p.name.lower(), f"Legacy PureIME path found: {p}"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f01_vitral_cli_health_and_abi_version_smoke(vitral_target: VitralTarget) -> None:
    cli = vitral_target.require_cli()
    health_proc = subprocess.run(
        [str(cli), "health"],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    assert health_proc.returncode == 0, f"vitral-cli health failed: {health_proc.stderr}"
    payload = json.loads(health_proc.stdout)
    assert payload["status"] == "ok"
    assert payload["brand"] == "Vitral"
    assert payload["version"] == "1.1.0"
    assert payload["abi_version"] == 1

    abi_proc = subprocess.run(
        [str(cli), "abi-version"],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    assert abi_proc.returncode == 0
    assert abi_proc.stdout.strip() == "1"


# ---------------------------------------------------------------------------
# F02: Design Token Codegen & Conformance Validator (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.tokens
def test_f02_design_tokens_json_all_10_invariant_groups(vitral_target: VitralTarget) -> None:
    tokens_path = vitral_target.require_tokens()
    tokens = json.loads(tokens_path.read_text(encoding="utf-8"))
    assert_all_token_invariants(tokens)


@pytest.mark.tier1
@pytest.mark.tokens
def test_f02_gen_tokens_check_mode_passes_cleanly(
    discovered_targets: DiscoveredTargets,
) -> None:
    script = discovered_targets.gen_tokens_script
    assert script is not None and script.is_file(), "scripts/gen-tokens.py missing"
    proc = subprocess.run(
        ["python3", str(script), "--check"],
        cwd=discovered_targets.vitral_root,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert proc.returncode == 0, f"gen-tokens.py --check failed: {proc.stdout}\n{proc.stderr}"
    assert "v1.1.0" in proc.stdout


@pytest.mark.tier1
@pytest.mark.tokens
def test_f02_cpp_headers_linux_and_windows_byte_identical(
    discovered_targets: DiscoveredTargets,
) -> None:
    root = discovered_targets.vitral_root
    linux_hpp = root / "linux/fcitx5-vitral/include/vitral_tokens.hpp"
    win_hpp = root / "crates/vitral-tsf/include/vitral_tokens.hpp"
    assert linux_hpp.is_file(), f"Missing {linux_hpp}"
    assert win_hpp.is_file(), f"Missing {win_hpp}"

    linux_bytes = linux_hpp.read_bytes()
    win_bytes = win_hpp.read_bytes()
    assert linux_bytes == win_bytes, "Linux and Windows generated C++ token headers drifted"

    text = linux_bytes.decode("utf-8")
    for expected_snippet in (
        'kName = "Vitral"',
        "kBackdropStandardPx = 18",
        "kBackdropFloatingWindowPx = 20",
        "kDesktopStatusCapsulePx = 9999",
        "kDesktopCandidateWindowPx = 16",
        "kMobileKeycapPx = 10",
        "kToolbarTextLabelsAllowed = false",
        "kDesktopIdleOpacity = 0.35f",
        "kStrokeWidthPx = 3.50f",
        "kSuperimposedInkFadeOutMs = 420",
        "kRetroactiveHighlightDurationMs = 600",
    ):
        assert expected_snippet in text, f"Missing C++ token constant: {expected_snippet}"


@pytest.mark.tier1
@pytest.mark.tokens
def test_f02_rust_kotlin_ts_css_generated_tokens_match_v1_1(
    discovered_targets: DiscoveredTargets,
) -> None:
    root = discovered_targets.vitral_root

    rust_rs = (root / "crates/vitral-tokens/src/generated.rs").read_text(encoding="utf-8")
    for needle in (
        'name: "Vitral"',
        "alpha: 0.680000",
        "alpha: 0.740000",
        "mobile_keycap_px: 10",
        "candidate_font_size_default_px: 16",
        "candidate_font_size_range_px: [13, 22]",
        "stroke_width_px: 3.50",
        "superimposed_ink_fade_out_ms: 420",
        "retroactive_highlight_duration_ms: 600",
        "min_width_dp: 220",
        "min_height_dp: 165",
        "desktop_idle_opacity: 0.35",
        "toolbar_text_labels_allowed: false",
    ):
        assert needle in rust_rs, f"Rust generated.rs missing {needle}"

    kt = (root / "apps/android/app/src/main/java/dev/vitral/ime/theme/VitralTokens.kt").read_text(
        encoding="utf-8"
    )
    for needle in (
        'const val name: String = "Vitral"',
        "const val mobileKeycapPx: Int = 10",
        "const val candidateFontSizeDefaultPx: Int = 16",
        "const val candidateFontSizeMinPx: Int = 13",
        "const val candidateFontSizeMaxPx: Int = 22",
        "const val strokeWidthPx: Float = 3.50f",
        "const val superimposedInkFadeOutMs: Int = 420",
        "const val retroactiveHighlightDurationMs: Int = 600",
        "const val floatingMinWidthDp: Int = 220",
        "const val floatingMinHeightDp: Int = 165",
        "const val desktopIdleOpacity: Float = 0.35f",
        "const val toolbarTextLabelsAllowed: Boolean = false",
    ):
        assert needle in kt, f"Kotlin VitralTokens.kt missing {needle}"

    css = (root / "apps/desktop-tauri/src/generated/tokens.css").read_text(encoding="utf-8")
    for needle in (
        "--vitral-radius-mobile-keycap: 10px;",
        "--vitral-radius-candidate-window: 16px;",
        "--vitral-radius-popover: 16px;",
        "--vitral-candidate-font-size-default: 16px;",
        "--vitral-candidate-font-size-min: 13px;",
        "--vitral-candidate-font-size-max: 22px;",
        "--vitral-idle-opacity: 0.35;",
        "--vitral-hw-stroke-width: 3.5px;",
        "--vitral-hw-fade-out: 420ms;",
        "--vitral-hw-retroactive-glow: 600ms;",
    ):
        assert needle in css, f"Tauri tokens.css missing {needle}"

    ts = (root / "apps/desktop-tauri/src/generated/tokens.ts").read_text(encoding="utf-8")
    assert '"name": "Vitral"' in ts
    assert '"toolbarTextLabelsAllowed": false' in ts


@pytest.mark.tier1
@pytest.mark.tokens
def test_f02_prototype_reconciled_values_override_drafts(
    design_tokens: dict[str, Any],
) -> None:
    assert_prototype_reconciled_values(design_tokens)
    assert design_tokens["color"]["dark"]["surfacePanel"] == "rgba(22, 26, 38, 0.68)"
    assert design_tokens["color"]["dark"]["surfacePopover"] == "rgba(24, 29, 42, 0.74)"
    assert design_tokens["color"]["light"]["surfacePanel"] == "rgba(255, 255, 255, 0.74)"
    assert design_tokens["color"]["light"]["surfacePopover"] == "rgba(255, 255, 255, 0.80)"
    assert design_tokens["color"]["light"]["shadowElevation"] == "0 12px 28px rgba(15, 23, 42, 0.10)"
    assert design_tokens["blur"]["backdropStandardPx"] == 18
    assert design_tokens["blur"]["backdropFloatingWindowPx"] == 20
    assert design_tokens["motion"]["desktopIdleFadeDelayMs"] == 5000


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_core
def test_f02_vitral_cli_tokens_verify_runtime_parity(vitral_target: VitralTarget) -> None:
    cli = vitral_target.require_cli()
    proc = subprocess.run(
        [str(cli), "tokens-verify"],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    assert proc.returncode == 0, f"tokens-verify failed: {proc.stderr}"
    payload = json.loads(proc.stdout)
    assert payload["brand"] == "Vitral"
    assert payload["toolbar_text_labels_allowed"] is False
    assert payload["desktop_candidate_page_size"] == 5


# ---------------------------------------------------------------------------
# F03: Automated Asset Fetch/Cache Scripts (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
def test_f03_assets_manifest_schema_and_pinned_groups(
    discovered_targets: DiscoveredTargets,
) -> None:
    manifest_path = discovered_targets.vitral_root / "scripts" / "assets-manifest.json"
    assert manifest_path.is_file()
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    assert manifest["schema_version"] == 1
    assert manifest["project"] == "Vitral"

    assets = manifest["assets"]
    assert isinstance(assets, list) and len(assets) >= 6
    groups = {entry["group"] for entry in assets}
    assert groups == {"rime", "grammar", "opencc", "handwriting", "voice", "reranker"}

    for entry in assets:
        assert re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]), f"Bad sha256 for {entry['id']}"
        assert isinstance(entry["size"], int) and entry["size"] > 0
        assert entry["url"].startswith("https://")
        assert isinstance(entry["verify_files"], list) and len(entry["verify_files"]) >= 1


@pytest.mark.tier1
def test_f03_fetch_assets_dry_run_exits_zero(vitral_target: VitralTarget) -> None:
    script = vitral_target.require_fetch_assets_script()
    proc = subprocess.run(
        ["bash", str(script), "--dry-run"],
        cwd=vitral_target.targets.vitral_root,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert proc.returncode == 0, f"fetch-assets.sh --dry-run failed: {proc.stderr}"
    assert "[DRY-RUN]" in proc.stdout
    assert "rime-ice-full" in proc.stdout


@pytest.mark.tier1
def test_f03_fetch_assets_verify_offline_checksums_pass(vitral_target: VitralTarget) -> None:
    script = vitral_target.require_fetch_assets_script()
    proc = subprocess.run(
        ["bash", str(script), "--verify"],
        cwd=vitral_target.targets.vitral_root,
        capture_output=True,
        text=True,
        timeout=25,
        check=False,
    )
    assert proc.returncode == 0, f"fetch-assets.sh --verify failed:\n{proc.stdout}\n{proc.stderr}"
    assert "verified cleanly" in proc.stdout

    assets_dir = vitral_target.targets.assets_dir
    assert assets_dir is not None and assets_dir.is_dir()
    for rel in (
        "rime/rime_ice.schema.yaml",
        "rime/double_pinyin_flypy.schema.yaml",
        "rime/wanxiang-lts-zh-hans.gram",
        "opencc/s2t.json",
        "handwriting/hw_model.pt",
        "voice/sensevoice-small/model.int8.onnx",
        "reranker/model_int8.onnx",
    ):
        p = assets_dir / rel
        assert p.is_file() and p.stat().st_size > 0, f"Staged asset missing/empty: {p}"


@pytest.mark.tier1
def test_f03_handwriting_classes_extracted_deterministically(
    discovered_targets: DiscoveredTargets,
) -> None:
    assert discovered_targets.assets_dir is not None
    classes_path = discovered_targets.assets_dir / "handwriting" / "hw_model.classes.json"
    assert classes_path.is_file()
    classes = json.loads(classes_path.read_text(encoding="utf-8"))
    assert isinstance(classes, list)
    assert len(classes) == 15146
    assert all(isinstance(cp, int) and not isinstance(cp, bool) for cp in classes)

    codepoint_set = set(classes)
    for ch in ("耄", "曝", "爆", "光", "老", "登", "金", "币"):
        assert ord(ch) in codepoint_set, f"Required character {ch!r} (U+{ord(ch):04X}) missing"


@pytest.mark.tier1
def test_f03_opencc_tables_staged_self_contained(
    discovered_targets: DiscoveredTargets,
) -> None:
    assert discovered_targets.assets_dir is not None
    opencc_dir = discovered_targets.assets_dir / "opencc"
    assert opencc_dir.is_dir()
    for fname in (
        "s2t.json",
        "t2s.json",
        "STCharacters.ocd2",
        "STPhrases.ocd2",
        "TSCharacters.ocd2",
    ):
        p = opencc_dir / fname
        assert p.is_file() and p.stat().st_size > 0, f"Missing OpenCC asset: {p}"


# ---------------------------------------------------------------------------
# F04: Frozen C-ABI & JNI Boundary Contracts (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
def test_f04_ffi_header_declares_frozen_abi_v1_symbols(vitral_target: VitralTarget) -> None:
    header_path = vitral_target.require_ffi_header()
    header = header_path.read_text(encoding="utf-8")
    assert "#define VITRAL_ABI_VERSION 1" in header

    for status_name in (
        "VITRAL_STATUS_OK = 0",
        "VITRAL_STATUS_ERR_NULL_POINTER = 1",
        "VITRAL_STATUS_ERR_INVALID_UTF8 = 2",
        "VITRAL_STATUS_ERR_INVALID_ARGUMENT = 3",
        "VITRAL_STATUS_ERR_NOT_INITIALIZED = 4",
        "VITRAL_STATUS_ERR_PURE_OFFLINE_VIOLATION = 5",
        "VITRAL_STATUS_ERR_IO = 6",
        "VITRAL_STATUS_ERR_PARSE = 7",
        "VITRAL_STATUS_ERR_NOT_FOUND = 8",
        "VITRAL_STATUS_ERR_INTEGRITY = 9",
        "VITRAL_STATUS_ERR_MODEL_UNAVAILABLE = 10",
        "VITRAL_STATUS_ERR_INTERNAL = 11",
        "VITRAL_STATUS_ERR_PANIC = 99",
    ):
        assert status_name in header, f"Missing VitralStatus enum variant: {status_name}"

    m = re.search(r'extern "C" \{(.*?)#ifdef __cplusplus', header, re.S)
    assert m is not None, 'extern "C" block missing in vitral_core.h'
    declared = re.findall(r"\b(vitral_\w+)\s*\(", m.group(1))
    assert len(declared) == len(EXPECTED_FFI_SYMBOLS) == 41
    assert set(declared) == set(EXPECTED_FFI_SYMBOLS)


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f04_ffi_cdll_loads_and_reports_abi_version_1(vitral_target: VitralTarget) -> None:
    cdll = vitral_target.load_ffi_cdll()
    abi_fn = cdll.vitral_abi_version
    abi_fn.restype = ctypes.c_uint32
    assert abi_fn() == 1

    for sym in EXPECTED_FFI_SYMBOLS:
        assert hasattr(cdll, sym), f"Missing C-ABI symbol in shared library: {sym}"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f04_ffi_init_shutdown_and_pure_offline_lifecycle(vitral_target: VitralTarget) -> None:
    ffi_lib = vitral_target.require_ffi()
    script = """
import ctypes, sys
cdll = ctypes.CDLL(sys.argv[1])
cdll.vitral_init.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)]
cdll.vitral_init.restype = ctypes.c_int
cdll.vitral_shutdown.argtypes = [ctypes.c_void_p]
cdll.vitral_set_pure_offline.argtypes = [ctypes.c_void_p, ctypes.c_bool]
cdll.vitral_set_pure_offline.restype = ctypes.c_int
cdll.vitral_is_pure_offline.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_bool)]
cdll.vitral_is_pure_offline.restype = ctypes.c_int
cdll.vitral_free_string.argtypes = [ctypes.c_void_p]

engine = ctypes.c_void_p()
err_ptr = ctypes.c_void_p()
status = cdll.vitral_init(b"{}", ctypes.byref(engine), ctypes.byref(err_ptr))
assert status == 0
assert bool(engine.value)
if err_ptr.value:
    cdll.vitral_free_string(err_ptr)
try:
    assert cdll.vitral_set_pure_offline(engine, True) == 0
    flag = ctypes.c_bool(False)
    assert cdll.vitral_is_pure_offline(engine, ctypes.byref(flag)) == 0
    assert flag.value is True

    assert cdll.vitral_set_pure_offline(engine, False) == 0
    assert cdll.vitral_is_pure_offline(engine, ctypes.byref(flag)) == 0
    assert flag.value is False
finally:
    cdll.vitral_shutdown(engine)
"""
    proc = subprocess.run(
        [sys.executable, "-c", script, str(ffi_lib)],
        capture_output=True,
        text=True,
        env=vitral_target._build_env(),
        timeout=25,
        check=False,
    )
    assert proc.returncode == 0, f"FFI init/shutdown subprocess failed:\n{proc.stdout}\n{proc.stderr}"


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f04_ffi_string_result_and_free_contract(vitral_target: VitralTarget) -> None:
    ffi_lib = vitral_target.require_ffi()
    script = """
import ctypes, json, sys
class _VitralStringResult(ctypes.Structure):
    _fields_ = [("status", ctypes.c_int), ("json_utf8", ctypes.c_void_p), ("error_message", ctypes.c_void_p)]

cdll = ctypes.CDLL(sys.argv[1])
cdll.vitral_init.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)]
cdll.vitral_init.restype = ctypes.c_int
cdll.vitral_shutdown.argtypes = [ctypes.c_void_p]
cdll.vitral_quick_paste_state.argtypes = [ctypes.c_void_p]
cdll.vitral_quick_paste_state.restype = _VitralStringResult
cdll.vitral_free_result.argtypes = [_VitralStringResult]
cdll.vitral_free_string.argtypes = [ctypes.c_void_p]

engine = ctypes.c_void_p()
err_ptr = ctypes.c_void_p()
assert cdll.vitral_init(b"{}", ctypes.byref(engine), ctypes.byref(err_ptr)) == 0
try:
    res = cdll.vitral_quick_paste_state(engine)
    try:
        assert res.status == 0
        assert res.json_utf8 is not None
        assert res.error_message is None
        raw_json = ctypes.cast(res.json_utf8, ctypes.c_char_p).value.decode("utf-8")
        parsed = json.loads(raw_json)
        assert isinstance(parsed, dict)
    finally:
        cdll.vitral_free_result(res)
    cdll.vitral_free_string(None)
finally:
    cdll.vitral_shutdown(engine)
"""
    proc = subprocess.run(
        [sys.executable, "-c", script, str(ffi_lib)],
        capture_output=True,
        text=True,
        env=vitral_target._build_env(),
        timeout=25,
        check=False,
    )
    assert proc.returncode == 0, f"FFI string result subprocess failed:\n{proc.stdout}\n{proc.stderr}"


@pytest.mark.tier1
def test_f04_jni_exports_all_38_camelcase_methods_and_v1_2_hooks(
    discovered_targets: DiscoveredTargets,
) -> None:
    jni_rs = discovered_targets.vitral_root / "crates" / "vitral-jni" / "src" / "lib.rs"
    assert jni_rs.is_file(), f"Missing {jni_rs}"
    text = jni_rs.read_text(encoding="utf-8")

    found_syms = re.findall(
        r'pub extern "system" fn (Java_dev_vitral_ime_core_VitralNative_\w+)',
        text,
    )
    assert len(found_syms) == 39, f"Expected 39 JNI exports, found {len(found_syms)}"
    assert all("_1" not in sym for sym in found_syms), (
        f"Underscore-mangled JNI symbol detected: {[s for s in found_syms if '_1' in s]}"
    )

    found_methods = {sym.removeprefix("Java_dev_vitral_ime_core_VitralNative_") for sym in found_syms}
    assert found_methods == set(EXPECTED_JNI_METHODS)

    for v12_hook in (
        "clipboardIngestRemote",
        "quickPasteState",
        "consumeQuickPaste",
        "onImeOpen",
        "onImeClose",
        "dismissQuickPaste",
    ):
        assert v12_hook in found_methods


@pytest.mark.tier1
@pytest.mark.requires_android
def test_f04_jni_native_so_exports_verified_via_nm_or_readelf(
    vitral_target: VitralTarget,
) -> None:
    jni_libs = vitral_target.require_android_jni()
    nm_bin = shutil.which("nm") or shutil.which("llvm-nm")
    assert nm_bin is not None, "nm or llvm-nm required to inspect libvitral_jni.so exports"

    required_syms = {f"Java_dev_vitral_ime_core_VitralNative_{m}" for m in EXPECTED_JNI_METHODS}

    for lib_path in jni_libs:
        magic = lib_path.read_bytes()[:4]
        assert magic == b"\x7fELF", f"{lib_path} is not a valid ELF binary"
        proc = subprocess.run(
            [nm_bin, "-D", "--defined-only", str(lib_path)],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        assert proc.returncode == 0, f"{nm_bin} failed on {lib_path}: {proc.stderr}"
        exported = {line.split()[-1] for line in proc.stdout.splitlines() if line.strip()}
        missing = required_syms - exported
        assert not missing, f"{lib_path} missing JNI symbols: {sorted(missing)}"
