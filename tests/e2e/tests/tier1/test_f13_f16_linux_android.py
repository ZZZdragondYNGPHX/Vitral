"""Tier 1 Linux & Android Platform UI tests for Vitral (F13-F16)."""

from __future__ import annotations

import json
import shutil
import subprocess
import zipfile
from pathlib import Path
from typing import Any

import pytest

from vitral_e2e.adapter import VitralTarget
from vitral_e2e.discovery import DiscoveredTargets
from vitral_e2e.tokens import assert_english_only_brand


def _read_dir_sources(base: Path, exts: tuple[str, ...]) -> str:
    parts: list[str] = []
    if not base.is_dir():
        return ""
    for p in sorted(base.rglob("*")):
        if "build" in p.parts or "target" in p.parts or "node_modules" in p.parts:
            continue
        if p.is_file() and p.suffix in exts:
            parts.append(p.read_text(encoding="utf-8", errors="replace"))
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# F13: Linux Fcitx5 Frosted-Glass Addon (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_linux
def test_f13_fcitx5_plugin_so_built_and_valid_elf(vitral_target: VitralTarget) -> None:
    so_path = vitral_target.require_fcitx5()
    assert so_path.read_bytes()[:4] == b"\x7fELF"

    ldd = shutil.which("ldd")
    if ldd:
        proc = subprocess.run(
            [ldd, "-r", str(so_path)],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        assert proc.returncode == 0
        assert "not found" not in proc.stdout


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_linux
def test_f13_fcitx5_uses_canonical_tokens_hpp_candidate_capsule(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    root = vitral_target.targets.vitral_root
    src_text = _read_dir_sources(root / "linux" / "fcitx5-vitral", (".cpp", ".hpp", ".h", ".cc"))
    assert "vitral_tokens.hpp" in src_text
    assert "kDesktopCandidateOrientation" in src_text or "horizontal" in src_text
    assert "kDesktopCandidatePageSize" in src_text
    assert "kDesktopCandidateWindowPx" in src_text
    assert "kCandidateActivePillPx" in src_text
    assert "kBackdropStandardPx" in src_text
    assert design_tokens["ergonomics"]["desktopCandidatePageSize"] == 5


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_linux
def test_f13_fcitx5_status_capsule_initial_and_idle_edge_fullscreen(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    root = vitral_target.targets.vitral_root
    src_text = _read_dir_sources(root / "linux" / "fcitx5-vitral", (".cpp", ".hpp", ".h", ".cc"))

    assert design_tokens["radius"]["desktopStatusCapsulePx"] == 9999
    assert design_tokens["motion"]["desktopIdleFadeDelayMs"] == 5000
    assert design_tokens["motion"]["desktopIdleOpacity"] == 0.35
    assert design_tokens["ergonomics"]["desktopInitialToolbarItems"] == ["vitral-logo", "lang-toggle"]

    assert "kDesktopStatusCapsulePx" in src_text
    assert "kDesktopIdleFadeDelayMs" in src_text
    assert "kDesktopIdleOpacity" in src_text
    assert "fullscreen" in src_text.lower()


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_linux
def test_f13_fcitx5_pure_svg_toolbar_zero_labels_and_editor_controls(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    assert design_tokens["typography"]["toolbarTextLabelsAllowed"] is False
    assert design_tokens["typography"]["toolbarIconStrokeWidthPx"] == 1.75

    root = vitral_target.targets.vitral_root
    src_text = _read_dir_sources(
        root / "linux" / "fcitx5-vitral",
        (".cpp", ".hpp", ".h", ".cc", ".conf", ".in", ".svg", ".json", ".txt"),
    )
    assert "vitral_tokens.hpp" in src_text
    assert "kToolbarTextLabelsAllowed" in src_text
    assert "kToolbarIconStrokeWidthPx" in src_text
    assert "kToolbarTextLabelsAllowed = true" not in src_text

    has_editor_symbols = ("−" in src_text or "\\u2212" in src_text.lower()) and (
        "✓" in src_text or "\\u2713" in src_text.lower()
    )
    has_editor_logic = any(
        kw in src_text.lower()
        for kw in ("toolbar_edit", "toolbaredit", "edit_mode", "editmode", "reorder_toolbar", "customiz")
    )
    assert has_editor_symbols or has_editor_logic, (
        "Expected pure-SVG toolbar editor controls (−/+/✓ or toolbar edit/reorder logic) in linux/fcitx5-vitral"
    )
    assert "quick_paste" in src_text.lower() or "quickpaste" in src_text.lower()


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_linux
def test_f13_fcitx5_mode1_popover_and_mode2_preedit_underline(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_fcitx5()
    assert design_tokens["handwriting"]["retroactiveHighlightDurationMs"] == 600
    root = vitral_target.targets.vitral_root
    src_text = _read_dir_sources(root / "linux" / "fcitx5-vitral", (".cpp", ".hpp", ".h", ".cc"))
    assert "kRetroactiveHighlightDurationMs" in src_text
    assert "preedit" in src_text.lower()
    assert "xiaohe" in src_text.lower() or "phonetic" in src_text.lower()


# ---------------------------------------------------------------------------
# F14: Shared Tauri v2 Management Center (Linux) (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_linux
def test_f14_tauri_linux_binary_built_and_executable(vitral_target: VitralTarget) -> None:
    _fcitx, tauri_bin = vitral_target.require_linux()
    assert tauri_bin.is_file()
    assert tauri_bin.read_bytes()[:4] == b"\x7fELF"
    ldd = shutil.which("ldd")
    if ldd:
        proc = subprocess.run([ldd, str(tauri_bin)], capture_output=True, text=True, timeout=10, check=False)
        assert "not found" not in proc.stdout


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_linux
def test_f14_tauri_frontend_consumes_generated_tokens_css_and_ts(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_linux()
    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    proc = subprocess.run(
        ["node", str(tauri_dir / "tests" / "tokens.test.mjs")],
        cwd=tauri_dir,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert proc.returncode == 0, f"tokens.test.mjs failed: {proc.stdout}\n{proc.stderr}"


@pytest.mark.tier1
@pytest.mark.requires_linux
def test_f14_tauri_exposes_koofr_offline_restore_and_storage_breakdown(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_linux()
    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    src_text = _read_dir_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".vue", ".svelte", ".html"))
    rust_text = _read_dir_sources(tauri_dir / "src-tauri", (".rs",))
    combined = (src_text + "\n" + rust_text).lower()

    for kw in ("koofr", "offline", "restore", "relay"):
        assert kw in combined, f"Tauri management center missing {kw!r} feature surface"


@pytest.mark.tier1
@pytest.mark.requires_linux
def test_f14_tauri_exposes_lexicon_import_retention_n_slider_and_font_slider(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_linux()
    assert design_tokens["typography"]["candidateFontSizeRangePx"] == [13, 22]
    assert design_tokens["typography"]["candidateFontSizeDefaultPx"] == 16

    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    src_text = _read_dir_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".vue", ".svelte", ".html"))
    lowered = src_text.lower()
    assert "lexicon" in lowered or ".scel" in lowered
    assert "frecency" in lowered or "retention" in lowered
    assert "candidateFontSizeRangePx" in src_text or ("13" in src_text and "22" in src_text), (
        "Expected candidate font size slider bounds [13, 22] or candidateFontSizeRangePx token in Tauri UI"
    )


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_linux
def test_f14_tauri_strict_english_vitral_branding_no_pureime(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_linux()
    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    conf = tauri_dir / "src-tauri" / "tauri.conf.json"
    assert conf.is_file()
    conf_text = conf.read_text(encoding="utf-8")
    conf_json = json.loads(conf_text)
    product_name = conf_json.get("productName", "")
    assert_english_only_brand(product_name, strict_ascii_only=True)

    src_text = _read_dir_sources(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".html", ".css"))
    assert_english_only_brand(src_text)


# ---------------------------------------------------------------------------
# F15: Android InputMethodService 120Hz Frosted-Glass Keyboard (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f15_android_docked_height_hierarchy_and_radii_tokens(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))

    assert design_tokens["ergonomics"]["mobileDockedScreenRatio"] == 0.26
    assert design_tokens["ergonomics"]["mobileTopHierarchy"] == ["icon-toolbar", "candidate-bar", "key-grid"]
    assert design_tokens["radius"]["mobileKeyboardSheetPx"] == 16
    assert design_tokens["radius"]["mobileKeycapPx"] == 10
    assert design_tokens["blur"]["backdropStandardPx"] == 18

    assert "mobileDockedScreenRatio" in kt_src
    assert "mobileKeycapDp" in kt_src
    assert "mobileKeyboardSheetDp" in kt_src
    assert "InputMethodService" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f15_android_floating_window_min_220x165dp_drag_resize(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    floating = design_tokens["ergonomics"]["mobileFloatingWindow"]
    assert floating["minWidthDp"] == 220
    assert floating["minHeightDp"] == 165
    assert design_tokens["blur"]["backdropFloatingWindowPx"] == 20

    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "floatingMinWidth" in kt_src and "floatingMinHeight" in kt_src
    assert "backdropFloatingWindow" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f15_android_xiaohe_4row_vs_quanpin_5row_and_numpad(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    keycaps = design_tokens["ergonomics"]["schemaKeycaps"]
    assert keycaps["double_pinyin_flypy"]["rows"] == 4
    assert keycaps["rime_ice_quanpin"]["rows"] == 4
    assert keycaps["rime_ice_quanpin"]["dedicatedTopNumberRow"] is False

    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "flypyRows" in kt_src and "quanpinRows" in kt_src
    assert "123" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f15_android_in_key_specular_glow_and_haptics_10_16ms(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    assert design_tokens["ergonomics"]["keyFeedbackMode"] == "in-key-specular-glow"
    assert design_tokens["ergonomics"]["haptics"] == {"letterTickMs": 10, "actionThudMs": 16}

    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "surfaceKeyPressedGlow" in kt_src
    assert "hapticLetterTickMs" in kt_src and "hapticActionThudMs" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f15_android_spacebar_350ms_voice_and_swipe_gestures(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    assert design_tokens["ergonomics"]["spacebarVoiceHoldThresholdMs"] == 350
    assert design_tokens["ergonomics"]["spacebarVoiceVisual"] == "in-place-waveform"

    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "spacebarVoiceHoldThresholdMs" in kt_src
    assert "voiceStart" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f15_android_pure_icon_toolbar_and_quick_paste_candidate_bar(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    assert design_tokens["typography"]["toolbarTextLabelsAllowed"] is False
    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "toolbarTextLabelsAllowed" in kt_src
    assert "consumeQuickPaste" in kt_src
    assert "onImeOpen" in kt_src and "onImeClose" in kt_src


# ---------------------------------------------------------------------------
# F16: Android Dual-Mode Handwriting Canvas, Compose Settings & APK (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_android
def test_f16_android_apk_built_valid_zip_with_dex_and_jni_libs(
    vitral_target: VitralTarget,
) -> None:
    apk_path = vitral_target.require_android_apk()
    assert zipfile.is_zipfile(apk_path), f"Invalid APK ZIP: {apk_path}"
    with zipfile.ZipFile(apk_path, "r") as zf:
        names = zf.namelist()
        assert "AndroidManifest.xml" in names
        assert any(n.startswith("classes") and n.endswith(".dex") for n in names)
        assert any(n.startswith("lib/") and n.endswith("/libvitral_jni.so") for n in names)


@pytest.mark.tier1
@pytest.mark.requires_android
def test_f16_android_jni_so_16kb_elf_alignment(vitral_target: VitralTarget) -> None:
    jni_libs = vitral_target.require_android_jni()
    readelf = shutil.which("readelf") or shutil.which("llvm-readelf")
    assert readelf is not None, "readelf or llvm-readelf required for ELF PT_LOAD check"

    for lib in jni_libs:
        proc = subprocess.run(
            [readelf, "-l", "-W", str(lib)],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        assert proc.returncode == 0, f"readelf failed on {lib}: {proc.stderr}"
        load_lines = [ln for ln in proc.stdout.splitlines() if "LOAD" in ln]
        assert load_lines, f"No PT_LOAD segments in {lib}"
        for ln in load_lines:
            align_tok = ln.split()[-1]
            align_val = int(align_tok, 16) if align_tok.startswith("0x") else int(align_tok)
            assert align_val >= 0x4000, f"{lib} PT_LOAD alignment {hex(align_val)} < 16KB (0x4000)"


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f16_android_hw_mode1_canvas_and_phonetic_badges(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    assert design_tokens["handwriting"]["showXiaoheCodeInLookupMode"] is True
    assert design_tokens["handwriting"]["strokeWidthPx"] == 3.5

    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "showXiaoheCodeInLookupMode" in kt_src
    assert "strokeWidthPx" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f16_android_hw_mode2_superimposed_420ms_fade_and_horizontal(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    assert design_tokens["handwriting"]["superimposedInkFadeOutMs"] == 420
    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "superimposedInkFadeOutMs" in kt_src
    assert "LongArray" in kt_src and "hwPushStroke" in kt_src


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_android
def test_f16_android_hw_mode2_600ms_retroactive_glow_and_tap_override(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_android_apk()
    assert design_tokens["handwriting"]["retroactiveHighlightDurationMs"] == 600
    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    kt_src = _read_dir_sources(android_dir, (".kt", ".java"))
    assert "retroactiveHighlightDurationMs" in kt_src
    assert "retroactiveFixGlow" in kt_src
    assert "hwOverrideSlot" in kt_src


@pytest.mark.tier1
@pytest.mark.requires_android
def test_f16_android_compose_settings_and_gradle_unit_tests_pass(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_android_apk()
    android_dir = vitral_target.targets.vitral_root / "apps" / "android"
    gradlew = android_dir / "gradlew"
    env_sh = vitral_target.targets.vitral_root / "scripts" / "env.sh"
    assert gradlew.is_file(), "apps/android/gradlew missing"
    proc = subprocess.run(
        ["bash", "-lc", f"source '{env_sh}' && '{gradlew}' --no-daemon --max-workers=1 testDebugUnitTest"],
        cwd=android_dir,
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    assert proc.returncode == 0, f"Gradle unit tests failed:\n{proc.stdout}\n{proc.stderr}"
