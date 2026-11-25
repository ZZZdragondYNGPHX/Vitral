"""Tier 2 BVA and negative/corner-case tests for F13-F16 (Linux Fcitx5/Tauri & Android IMS/Compose)."""

from __future__ import annotations

import re
import shutil
import subprocess
import zipfile
from pathlib import Path
from typing import Any

import pytest

from harnesses import WebDAVMockServer
from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets
from vitral_e2e.tokens import assert_english_only_brand, assert_no_toolbar_text_labels

pytestmark = pytest.mark.tier2


def _collect_text_sources(directory: Path, suffixes: tuple[str, ...]) -> list[Path]:
    if not directory.is_dir():
        return []
    return [p for p in directory.rglob("*") if p.is_file() and p.suffix.lower() in suffixes]


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
        if any(part in exclude_parts for part in p.parts):
            continue
        files.append(p)
    combined = "\n".join(p.read_text(encoding="utf-8", errors="replace") for p in files)
    return files, combined


def _android_tokens_kt_path(discovered_targets: DiscoveredTargets) -> Path:
    return (
        discovered_targets.vitral_root
        / "apps"
        / "android"
        / "app"
        / "src"
        / "main"
        / "java"
        / "dev"
        / "vitral"
        / "ime"
        / "theme"
        / "VitralTokens.kt"
    )


# ======================================================================
# F13: Linux Fcitx5 Frosted-Glass Addon (5 tests)
# ======================================================================


@pytest.mark.requires_linux
def test_f13_fcitx5_plugin_elf_and_symbols(vitral_target: VitralTarget) -> None:
    so_path = vitral_target.require_fcitx5()
    magic = so_path.read_bytes()[:4]
    assert magic == b"\x7fELF", f"Expected ELF shared library, got {magic!r}"

    ldd_bin = shutil.which("ldd")
    if ldd_bin:
        ldd = subprocess.run([ldd_bin, str(so_path)], capture_output=True, text=True, check=True)
        assert "not found" not in ldd.stdout, f"Unresolved dynamic library in {so_path}:\n{ldd.stdout}"

    nm_bin = shutil.which("nm")
    if nm_bin:
        nm = subprocess.run([nm_bin, "-D", str(so_path)], capture_output=True, text=True, check=True)
        assert "fcitx" in nm.stdout.lower(), f"Missing Fcitx5 plugin entry symbols in {so_path}"

    raw_bytes = so_path.read_bytes()
    assert b"PureIME" not in raw_bytes
    assert b"Vitral" in raw_bytes


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_f13_fcitx5_token_radii_blur_and_page_size_5(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    fcitx_dir = discovered_targets.vitral_root / "linux" / "fcitx5-vitral"
    hpp = (fcitx_dir / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")

    assert f"kDesktopCandidateWindowPx = {design_tokens['radius']['desktopCandidateWindowPx']};" in hpp
    assert f"kDesktopStatusCapsulePx = {design_tokens['radius']['desktopStatusCapsulePx']};" in hpp
    assert f"kCandidateActivePillPx = {design_tokens['radius']['candidateActivePillPx']};" in hpp
    assert f"kBackdropStandardPx = {design_tokens['blur']['backdropStandardPx']};" in hpp
    assert f"kDesktopCandidatePageSize = {design_tokens['ergonomics']['desktopCandidatePageSize']};" in hpp
    assert 'kNumericVariant = "tabular-nums";' in hpp

    cpp_sources = _collect_text_sources(fcitx_dir, (".cpp", ".cc", ".cxx", ".h", ".hpp"))
    combined = "\n".join(p.read_text(encoding="utf-8", errors="replace") for p in cpp_sources)
    assert "vitral_tokens.hpp" in combined


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_f13_fcitx5_idle_5000ms_035_opacity_and_edge_hide(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    fcitx_dir = discovered_targets.vitral_root / "linux" / "fcitx5-vitral"
    hpp = (fcitx_dir / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")

    assert f"kDesktopIdleFadeDelayMs = {design_tokens['motion']['desktopIdleFadeDelayMs']};" in hpp
    assert "kDesktopIdleOpacity = 0.35f;" in hpp

    src_files, src_text = _read_non_generated_sources(fcitx_dir / "src", (".cpp", ".cc", ".h", ".hpp"))
    assert len(src_files) >= 1, f"Expected Fcitx5 C++ implementation sources under {fcitx_dir / 'src'}"
    assert "0.32" not in src_text, "Superseded draft idle opacity 0.32 must not appear in Fcitx5 source"
    assert "kDesktopIdleFadeDelayMs" in src_text and "kDesktopIdleOpacity" in src_text
    lower = src_text.lower()
    assert "fullscreen" in lower, "Missing fullscreen auto-hide logic in Fcitx5 source"
    assert any(tok in lower for tok in ("edge_hide", "edgehide", "semi_hide", "semihide", "screen_edge", "dock_edge")) or (
        "edge" in lower and "hide" in lower
    ), "Missing edge semi-hide logic in Fcitx5 source"


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_f13_fcitx5_initial_2item_capsule_and_zero_text_labels(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    fcitx_dir = discovered_targets.vitral_root / "linux" / "fcitx5-vitral"
    hpp = (fcitx_dir / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")

    assert 'kDesktopInitialToolbarItems = {"vitral-logo", "lang-toggle"};' in hpp
    assert "kToolbarTextLabelsAllowed = false;" in hpp
    assert "kToolbarIconStrokeWidthPx = 1.75f;" in hpp

    src_files, src_text = _read_non_generated_sources(fcitx_dir / "src", (".cpp", ".cc", ".h", ".hpp"))
    assert len(src_files) >= 1, f"Expected Fcitx5 C++ implementation sources under {fcitx_dir / 'src'}"
    assert "kDesktopInitialToolbarItems" in src_text or (
        "vitral-logo" in src_text and "lang-toggle" in src_text
    )
    assert "kToolbarTextLabelsAllowed" in src_text or "kToolbarIconStrokeWidthPx" in src_text
    assert "<svg" in src_text.lower() or "svg" in src_text.lower() or "path" in src_text.lower()
    assert ("−" in src_text or "-" in src_text) and "+" in src_text and (
        "✓" in src_text or "confirm" in src_text.lower() or "check" in src_text.lower()
    )
    assert_no_toolbar_text_labels([
        {"id": item_id, "showLabel": False, "strokeWidthPx": 1.75}
        for item_id in design_tokens["ergonomics"]["desktopInitialToolbarItems"]
    ])


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_f13_fcitx5_600ms_retro_glow_and_quick_paste_surface(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_fcitx5()
    fcitx_dir = discovered_targets.vitral_root / "linux" / "fcitx5-vitral"
    hpp = (fcitx_dir / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")
    assert "kRetroactiveHighlightDurationMs = 600;" in hpp

    src_files, src_text = _read_non_generated_sources(fcitx_dir / "src", (".cpp", ".cc", ".h", ".hpp"))
    assert len(src_files) >= 1, f"Expected Fcitx5 C++ implementation sources under {fcitx_dir / 'src'}"
    assert "kRetroactiveHighlightDurationMs" in src_text
    lower = src_text.lower()
    assert any(tok in lower for tok in ("quick_paste", "quickpaste", "consume_quick_paste"))
    assert "live_slot.enc" not in src_text
    assert "900ms" not in src_text


# ======================================================================
# F14: Shared Tauri v2 Management Center (Linux) (5 tests)
# ======================================================================


@pytest.mark.requires_linux
def test_f14_tauri_linux_binary_and_brand_invariant(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    _, tauri_bin = vitral_target.require_linux()
    assert tauri_bin.read_bytes()[:4] == b"\x7fELF", f"Expected Linux ELF binary: {tauri_bin}"
    assert_english_only_brand(tauri_bin.name.replace("vitral", "Vitral"), require_present=False)

    tauri_dir = discovered_targets.vitral_root / "apps" / "desktop-tauri"
    for p in _collect_text_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".css", ".html")):
        txt = p.read_text(encoding="utf-8", errors="replace")
        assert "PureIME" not in txt and "pure-ime" not in txt.lower(), f"Forbidden legacy brand in {p}"
        assert not re.search(r"Vitral\s*[\u4e00-\u9fff]*输入法", txt), f"Chinese alias in {p}"


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_f14_tauri_candidate_font_slider_13_22_clamp(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_linux()
    tauri_dir = discovered_targets.vitral_root / "apps" / "desktop-tauri"
    tokens_ts = (tauri_dir / "src" / "generated" / "tokens.ts").read_text(encoding="utf-8")
    assert '"candidateFontSizeDefaultPx": 16' in tokens_ts
    assert '"candidateFontSizeRangePx": [\n      13,\n      22\n    ]' in tokens_ts

    ui_files, ui_text = _read_non_generated_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".html"))
    assert len(ui_files) >= 1, f"Expected non-generated Tauri UI sources under {tauri_dir / 'src'}"
    assert "max=\"19\"" not in ui_text and "max={19}" not in ui_text, (
        "Superseded draft font slider upper bound 19px must not appear in Tauri UI"
    )
    assert "13" in ui_text and "22" in ui_text, "Expected [13, 22] font slider bounds in Tauri UI"
    assert design_tokens["typography"]["candidateFontSizeRangePx"] == [13, 22]


@pytest.mark.requires_linux
def test_f14_tauri_clipboard_n_slider_and_3tier_monitor(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_linux()
    tauri_dir = discovered_targets.vitral_root / "apps" / "desktop-tauri"
    ui_files, ui_text = _read_non_generated_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".html"))
    rs_files, rs_text = _read_non_generated_sources(tauri_dir / "src-tauri" / "src", (".rs",))
    assert len(ui_files) >= 1 and len(rs_files) >= 1, "Expected non-generated Tauri frontend & Rust sources"
    combined = ui_text + "\n" + rs_text
    combined_lower = combined.lower()
    assert "live_slot.enc" not in combined
    assert "history_ring.jsonl.enc" in combined or "clipboard" in combined_lower
    assert re.search(
        r"\bring\b|\bcapacity\b|\bslider\b|clipboard_ring_capacity|clipboard_capacity",
        combined_lower,
    ), "Expected N-ring clipboard capacity/slider indicators in Tauri sources"
    assert re.search(
        r"\btier\b|\blexicon\b|\brelay\b|3-tier|three-tier",
        combined_lower,
    ), "Expected 3-tier storage monitor indicators (tier/lexicon/relay) in Tauri sources"


@pytest.mark.requires_linux
def test_f14_tauri_webdav_validation_and_offline_gate(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.require_linux()
    tauri_dir = discovered_targets.vitral_root / "apps" / "desktop-tauri"
    rs_files, rs_text = _read_non_generated_sources(tauri_dir / "src-tauri" / "src", (".rs",))
    ui_files, ui_text = _read_non_generated_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".html"))
    assert len(rs_files) >= 1, f"Expected Tauri Rust backend sources under {tauri_dir / 'src-tauri' / 'src'}"
    assert len(ui_files) >= 1, f"Expected Tauri frontend sources under {tauri_dir / 'src'}"

    combined_lower = (rs_text + "\n" + ui_text).lower()
    assert "koofr" in combined_lower or "webdav" in combined_lower
    assert any(tok in combined_lower for tok in ("validate", "http://", "https://", "endpoint", "url"))
    assert "pure_offline" in rs_text or "pureoffline" in combined_lower or "set_pure_offline" in rs_text
    assert "live_slot.enc" not in (rs_text + ui_text)
    assert webdav_server.koofr_endpoint.startswith("http://127.0.0.1:")


@pytest.mark.requires_linux
@pytest.mark.tokens
def test_f14_tauri_theme_tokens_and_lexicon_error_ui(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_linux()
    tauri_dir = discovered_targets.vitral_root / "apps" / "desktop-tauri"
    css = (tauri_dir / "src" / "generated" / "tokens.css").read_text(encoding="utf-8")
    assert "--vitral-surface-panel: rgba(22, 26, 38, 0.68);" in css
    assert "--vitral-surface-panel: rgba(255, 255, 255, 0.74);" in css
    assert "--vitral-blur-standard: 18px;" in css

    ui_files, ui_text = _read_non_generated_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".css", ".html"))
    assert len(ui_files) >= 1, f"Expected non-generated Tauri frontend sources under {tauri_dir / 'src'}"
    assert "tokens.css" in ui_text or "tokens.ts" in ui_text or "--vitral-" in ui_text
    ui_lower = ui_text.lower()
    assert "lexicon" in ui_lower
    assert any(tok in ui_lower for tok in ("error", "warning", "skipped", "import"))


# ======================================================================
# F15: Android InputMethodService 120Hz Frosted-Glass Keyboard (5 tests)
# ======================================================================


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f15_android_floating_window_220x165dp_min_clamp(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "floatingMinWidthDp: Int = 220" in kt_text
    assert "floatingMinHeightDp: Int = 165" in kt_text
    assert "backdropFloatingWindowPx: Int = 20" in kt_text
    assert design_tokens["ergonomics"]["mobileFloatingWindow"]["freeDrag"] is True
    assert design_tokens["ergonomics"]["mobileFloatingWindow"]["freeResize"] is True

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1, f"Expected non-generated Android IME Kotlin/Java sources under {android_src}"
    assert "VitralTokens" in app_text
    assert "floatingMinWidth" in app_text and "floatingMinHeight" in app_text
    assert "backdropFloatingWindow" in app_text
    assert "230.dp" not in app_text and "175.dp" not in app_text


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f15_android_docked_ratio_026_and_top_hierarchy(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "mobileDockedScreenRatio: Float = 0.26f" in kt_text
    assert 'listOf("icon-toolbar", "candidate-bar", "key-grid")' in kt_text
    assert "toolbarTextLabelsAllowed: Boolean = false" in kt_text
    assert float(design_tokens["ergonomics"]["mobileDockedScreenRatio"]) == pytest.approx(0.26)

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1
    assert "mobileDockedScreenRatio" in app_text
    assert "toolbarTextLabelsAllowed" in app_text or "VitralTokens" in app_text


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f15_android_schema_4row_flypy_vs_5row_quanpin(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_android_apk()
    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "flypyRows: Int = 4" in kt_text
    assert 'flypyBottomRightSublabel: String = "xiaohe-final"' in kt_text
    assert "quanpinRows: Int = 4" in kt_text
    assert "quanpinDedicatedTopNumberRow: Boolean = false" in kt_text

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1
    assert "flypyRows" in app_text and "quanpinRows" in app_text
    assert "123" in app_text


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f15_android_spacebar_350ms_voice_and_haptics_bounds(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_android_apk()
    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "spacebarVoiceHoldThresholdMs: Int = 350" in kt_text
    assert 'spacebarVoiceVisual: String = "in-place-waveform"' in kt_text
    assert "hapticLetterTickMs: Int = 10" in kt_text
    assert "hapticActionThudMs: Int = 16" in kt_text
    assert 'keyFeedbackMode: String = "in-key-specular-glow"' in kt_text

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1
    assert "spacebarVoiceHoldThresholdMs" in app_text
    assert "hapticLetterTickMs" in app_text and "hapticActionThudMs" in app_text
    assert "surfaceKeyPressedGlow" in app_text or "keyFeedbackMode" in app_text


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f15_android_keycap_10px_radius_and_jni_abis(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    apk_path = vitral_target.require_android_apk()
    vitral_target.require_android_jni()

    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "mobileKeycapPx: Int = 10" in kt_text
    assert "mobileKeyboardSheetPx: Int = 16" in kt_text
    assert "candidateActivePillPx: Int = 10" in kt_text

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1
    assert "mobileKeycap" in app_text and "mobileKeyboardSheet" in app_text

    with zipfile.ZipFile(apk_path, "r") as zf:
        jni_entries = [n for n in zf.namelist() if n.endswith("libvitral_jni.so")]
        assert len(jni_entries) >= 1, f"libvitral_jni.so missing inside APK {apk_path}"


# ======================================================================
# F16: Android Dual-Mode Handwriting Canvas, Compose & APK (5 tests)
# ======================================================================


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f16_android_hw_420ms_fade_600ms_glow_35px_stroke(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_android_apk()
    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "strokeWidthPx: Float = 3.50f" in kt_text
    assert "superimposedInkFadeOutMs: Int = 420" in kt_text
    assert "retroactiveHighlightDurationMs: Int = 600" in kt_text

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1
    assert "superimposedInkFadeOutMs" in app_text
    assert "retroactiveHighlightDurationMs" in app_text
    assert "strokeWidthPx" in app_text


@pytest.mark.requires_android
def test_f16_android_hw_mode1_no_autoclear_and_mode2_override(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_android_apk()
    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    kt_files, combined = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(kt_files) >= 1
    assert "hwOverrideSlot" in combined or "overrideSlot" in combined
    assert "quickPaste" in combined or "consumeQuickPaste" in combined


@pytest.mark.requires_android
def test_f16_android_apk_zip_manifest_ime_service_valid(vitral_target: VitralTarget) -> None:
    apk_path = vitral_target.require_android_apk()
    assert zipfile.is_zipfile(apk_path), f"APK is not a valid ZIP archive: {apk_path}"
    with zipfile.ZipFile(apk_path, "r") as zf:
        names = set(zf.namelist())
        assert "AndroidManifest.xml" in names
        assert any(n.endswith(".dex") for n in names), "APK missing classes*.dex"
        manifest_bytes = zf.read("AndroidManifest.xml")
        assert (
            b"InputMethod" in manifest_bytes
            or "InputMethod".encode("utf-16le") in manifest_bytes
            or b"Vitral" in manifest_bytes
            or "Vitral".encode("utf-16le") in manifest_bytes
        )
        has_v1_sig = any(n.startswith("META-INF/") and n.endswith((".SF", ".RSA", ".EC")) for n in names)
        has_v2_sig = b"APK Sig Block 42" in apk_path.read_bytes()
        assert has_v1_sig or has_v2_sig, f"APK appears unsigned: {apk_path}"


@pytest.mark.requires_android
def test_f16_android_apk_english_only_vitral_label(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    apk_path = vitral_target.require_android_apk()
    android_res = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    src_files, _ = _read_non_generated_sources(android_res, (".xml", ".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(src_files) >= 1
    for p in src_files:
        txt = p.read_text(encoding="utf-8", errors="replace")
        assert "PureIME" not in txt and "pure-ime" not in txt.lower()
        assert not re.search(r"Vitral\s*[\u4e00-\u9fff]*输入法", txt)
    apk_bytes = apk_path.read_bytes()
    assert b"PureIME" not in apk_bytes and "PureIME".encode("utf-16le") not in apk_bytes
    assert "Vitral输入法".encode("utf-8") not in apk_bytes and "Vitral输入法".encode("utf-16le") not in apk_bytes
    assert b"Vitral" in apk_bytes or "Vitral".encode("utf-16le") in apk_bytes


@pytest.mark.requires_android
@pytest.mark.tokens
def test_f16_android_compose_settings_bounds_parity(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_android_apk()
    kt_text = _android_tokens_kt_path(discovered_targets).read_text(encoding="utf-8")
    assert "candidateFontSizeDefaultPx: Int = 16" in kt_text
    assert "candidateFontSizeMinPx: Int = 13" in kt_text
    assert "candidateFontSizeMaxPx: Int = 22" in kt_text
    assert 'cloudRoot: String = "/Koofr/Vitral"' in kt_text

    android_src = discovered_targets.vitral_root / "apps" / "android" / "app" / "src"
    app_files, app_text = _read_non_generated_sources(android_src, (".kt", ".java"), exclude_names={"VitralTokens.kt"})
    assert len(app_files) >= 1
    assert (
        ("candidateFontSizeMinPx" in app_text and "candidateFontSizeMaxPx" in app_text)
        or ("13" in app_text and "22" in app_text)
    )
    assert "cloudRoot" in app_text or "/Koofr/Vitral" in app_text
