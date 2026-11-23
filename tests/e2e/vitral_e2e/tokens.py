"""Design tokens v1.1.0 loader and visual invariant validators for Vitral."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import Any

from .discovery import discover_targets

DEFAULT_TOKENS_PATH = Path(__file__).resolve().parents[3] / "tokens" / "design-tokens.json"

_HEX_COLOR_RE = re.compile(r"^#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?$")
_RGBA_COLOR_RE = re.compile(
    r"^rgba\(\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*(0(\.\d+)?|1(\.0+)?)\s*\)$"
)
_SHADOW_RE = re.compile(
    r"^(?:0|-?\d+(?:\.\d+)?px)\s+(?:0|-?\d+(?:\.\d+)?px)\s+\d+(?:\.\d+)?px\s+rgba\(\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*(0(\.\d+)?|1(\.0+)?)\s*\)$"
)

EXPECTED_DARK_PALETTE: dict[str, str] = {
    "surfacePanel": "rgba(22, 26, 38, 0.68)",
    "surfacePopover": "rgba(24, 29, 42, 0.74)",
    "surfaceKeyPrimary": "rgba(255, 255, 255, 0.08)",
    "surfaceKeyFunction": "rgba(255, 255, 255, 0.04)",
    "surfaceKeyPressedGlow": "rgba(56, 189, 248, 0.34)",
    "borderSpecular": "rgba(255, 255, 255, 0.16)",
    "accentPrimary": "#0ea5e9",
    "accentSoft": "rgba(14, 165, 233, 0.18)",
    "accentText": "#7dd3fc",
    "textPrimary": "#f8fafc",
    "textSecondary": "#94a3b8",
    "shadowElevation": "0 16px 36px rgba(0, 0, 0, 0.48)",
    "inkStrokePrimary": "#7dd3fc",
    "retroactiveFixGlow": "rgba(56, 189, 248, 0.65)",
}

EXPECTED_LIGHT_PALETTE: dict[str, str] = {
    "surfacePanel": "rgba(255, 255, 255, 0.74)",
    "surfacePopover": "rgba(255, 255, 255, 0.80)",
    "surfaceKeyPrimary": "rgba(255, 255, 255, 0.86)",
    "surfaceKeyFunction": "rgba(15, 23, 42, 0.06)",
    "surfaceKeyPressedGlow": "rgba(14, 165, 233, 0.26)",
    "borderSpecular": "rgba(255, 255, 255, 0.88)",
    "accentPrimary": "#0284c7",
    "accentSoft": "rgba(2, 132, 199, 0.14)",
    "accentText": "#0369a1",
    "textPrimary": "#0f172a",
    "textSecondary": "#64748b",
    "shadowElevation": "0 12px 28px rgba(15, 23, 42, 0.10)",
    "inkStrokePrimary": "#0284c7",
    "retroactiveFixGlow": "rgba(2, 132, 199, 0.55)",
}


def _is_strict_int(val: Any) -> bool:
    return isinstance(val, int) and not isinstance(val, bool)


def _is_strict_number(val: Any) -> bool:
    return isinstance(val, (int, float)) and not isinstance(val, bool)


def _assert_color_token(val: Any, name: str) -> None:
    assert isinstance(val, str), f"{name} must be str, got {type(val).__name__}"
    if name == "shadowElevation":
        assert _SHADOW_RE.match(val), f"{name} invalid shadow format: {val!r}"
    else:
        assert _HEX_COLOR_RE.match(val) or _RGBA_COLOR_RE.match(val), (
            f"{name} invalid color format: {val!r}"
        )


def resolve_tokens_path(path: Path | str | None = None) -> Path:
    if path is not None:
        return Path(path)
    env_path = os.environ.get("VITRAL_TOKENS_PATH", "").strip()
    if env_path:
        return Path(env_path).expanduser()
    discovered = discover_targets().design_tokens_path
    return discovered if discovered is not None else DEFAULT_TOKENS_PATH


def load_design_tokens(path: Path | str | None = None) -> dict[str, Any]:
    tokens_path = resolve_tokens_path(path)
    assert tokens_path.is_file(), f"Design tokens file not found: {tokens_path}"
    data = json.loads(tokens_path.read_text(encoding="utf-8"))
    assert isinstance(data, dict), f"Design tokens root must be JSON object: {tokens_path}"
    return data


def assert_brand_invariants(tokens: dict[str, Any]) -> None:
    assert tokens.get("$schema") == "https://vitral.dev/schemas/design-tokens-v1.json"
    brand = tokens.get("brand")
    assert isinstance(brand, dict), "brand must be a dict"
    assert brand.get("name") == "Vitral"
    assert brand.get("version") == "1.1.0"
    assert brand.get("materialStyle") == "frosted-glass"
    assert brand.get("cloudRoot") == "/Koofr/Vitral"


def _assert_palette(palette: Any, expected: dict[str, str], label: str) -> None:
    assert isinstance(palette, dict), f"color.{label} must be a dict"
    assert set(palette.keys()) == set(expected.keys()), (
        f"color.{label} keys mismatch: {sorted(palette.keys())} != {sorted(expected.keys())}"
    )
    for key, expected_val in expected.items():
        actual_val = palette.get(key)
        _assert_color_token(actual_val, key)
        assert actual_val == expected_val, (
            f"color.{label}.{key} mismatch: {actual_val!r} != {expected_val!r}"
        )


def assert_dark_palette_invariants(tokens: dict[str, Any]) -> None:
    color = tokens.get("color")
    assert isinstance(color, dict), "color must be a dict"
    _assert_palette(color.get("dark"), EXPECTED_DARK_PALETTE, "dark")


def assert_light_palette_invariants(tokens: dict[str, Any]) -> None:
    color = tokens.get("color")
    assert isinstance(color, dict), "color must be a dict"
    _assert_palette(color.get("light"), EXPECTED_LIGHT_PALETTE, "light")


def assert_blur_invariants(tokens: dict[str, Any]) -> None:
    blur = tokens.get("blur")
    assert isinstance(blur, dict), "blur must be a dict"
    assert _is_strict_int(blur.get("backdropStandardPx"))
    assert blur["backdropStandardPx"] == 18
    assert _is_strict_int(blur.get("backdropFloatingWindowPx"))
    assert blur["backdropFloatingWindowPx"] == 20


def assert_radius_invariants(tokens: dict[str, Any]) -> None:
    radius = tokens.get("radius")
    assert isinstance(radius, dict), "radius must be a dict"
    expected = {
        "desktopStatusCapsulePx": 9999,
        "desktopCandidateWindowPx": 16,
        "candidateActivePillPx": 10,
        "mobileKeyboardSheetPx": 16,
        "mobileKeycapPx": 10,
        "popoverPanelPx": 16,
    }
    assert set(radius.keys()) == set(expected.keys())
    for k, v in expected.items():
        assert _is_strict_int(radius.get(k)), f"radius.{k} must be strict int"
        assert radius[k] == v, f"radius.{k} mismatch: {radius[k]} != {v}"


def assert_typography_invariants(tokens: dict[str, Any]) -> None:
    typo = tokens.get("typography")
    assert isinstance(typo, dict), "typography must be a dict"
    assert typo.get("numericVariant") == "tabular-nums"
    assert _is_strict_int(typo.get("candidateFontSizeDefaultPx"))
    assert typo["candidateFontSizeDefaultPx"] == 16
    font_range = typo.get("candidateFontSizeRangePx")
    assert isinstance(font_range, list) and len(font_range) == 2
    assert all(_is_strict_int(x) for x in font_range)
    assert font_range == [13, 22]
    stroke_w = typo.get("toolbarIconStrokeWidthPx")
    assert _is_strict_number(stroke_w)
    assert float(stroke_w) == 1.75
    assert typo.get("toolbarTextLabelsAllowed") is False


def assert_motion_invariants(tokens: dict[str, Any]) -> None:
    motion = tokens.get("motion")
    assert isinstance(motion, dict), "motion must be a dict"
    assert motion.get("springCurve") == "cubic-bezier(0.22, 1, 0.36, 1)"
    assert _is_strict_int(motion.get("durationFastMs")) and motion["durationFastMs"] == 150
    assert _is_strict_int(motion.get("durationStandardMs")) and motion["durationStandardMs"] == 180
    assert (
        _is_strict_int(motion.get("desktopIdleFadeDelayMs"))
        and motion["desktopIdleFadeDelayMs"] == 5000
    )
    idle_opacity = motion.get("desktopIdleOpacity")
    assert _is_strict_number(idle_opacity) and float(idle_opacity) == 0.35


def assert_handwriting_invariants(tokens: dict[str, Any]) -> None:
    hw = tokens.get("handwriting")
    assert isinstance(hw, dict), "handwriting must be a dict"
    assert hw.get("modes") == ["phonetic-lookup-single", "continuous-contextual-lattice"]
    assert _is_strict_number(hw.get("strokeWidthPx")) and float(hw["strokeWidthPx"]) == 3.5
    assert (
        _is_strict_int(hw.get("superimposedInkFadeOutMs"))
        and hw["superimposedInkFadeOutMs"] == 420
    )
    assert (
        _is_strict_int(hw.get("retroactiveHighlightDurationMs"))
        and hw["retroactiveHighlightDurationMs"] == 600
    )
    assert _is_strict_int(hw.get("confusionTopK")) and hw["confusionTopK"] == 8
    assert (
        _is_strict_number(hw.get("ambiguousMarginThreshold"))
        and float(hw["ambiguousMarginThreshold"]) == 0.15
    )
    assert hw.get("showXiaoheCodeInLookupMode") is True


def assert_ergonomics_invariants(tokens: dict[str, Any]) -> None:
    ergo = tokens.get("ergonomics")
    assert isinstance(ergo, dict), "ergonomics must be a dict"
    assert ergo.get("desktopCandidateOrientation") == "horizontal"
    assert _is_strict_int(ergo.get("desktopCandidatePageSize"))
    assert ergo["desktopCandidatePageSize"] == 5
    assert ergo.get("desktopInitialToolbarItems") == ["vitral-logo", "lang-toggle"]
    ratio = ergo.get("mobileDockedScreenRatio")
    assert _is_strict_number(ratio) and float(ratio) == 0.26
    assert ergo.get("mobileTopHierarchy") == ["icon-toolbar", "candidate-bar", "key-grid"]

    floating = ergo.get("mobileFloatingWindow")
    assert isinstance(floating, dict)
    assert floating.get("freeDrag") is True
    assert floating.get("freeResize") is True
    assert _is_strict_int(floating.get("minWidthDp")) and floating["minWidthDp"] == 220
    assert _is_strict_int(floating.get("minHeightDp")) and floating["minHeightDp"] == 165

    assert ergo.get("keyFeedbackMode") == "in-key-specular-glow"
    assert (
        _is_strict_int(ergo.get("spacebarVoiceHoldThresholdMs"))
        and ergo["spacebarVoiceHoldThresholdMs"] == 350
    )
    assert ergo.get("spacebarVoiceVisual") == "in-place-waveform"

    haptics = ergo.get("haptics")
    assert isinstance(haptics, dict)
    assert _is_strict_int(haptics.get("letterTickMs")) and haptics["letterTickMs"] == 10
    assert _is_strict_int(haptics.get("actionThudMs")) and haptics["actionThudMs"] == 16


def assert_schema_keycaps_invariants(tokens: dict[str, Any]) -> None:
    ergo = tokens.get("ergonomics")
    assert isinstance(ergo, dict), "ergonomics must be a dict"
    keycaps = ergo.get("schemaKeycaps")
    assert isinstance(keycaps, dict), "ergonomics.schemaKeycaps must be a dict"

    flypy = keycaps.get("double_pinyin_flypy")
    assert isinstance(flypy, dict)
    assert _is_strict_int(flypy.get("rows")) and flypy["rows"] == 4
    assert flypy.get("topLeftSublabel") == "swipe-up-number-or-symbol"
    assert flypy.get("bottomRightSublabel") == "xiaohe-final"

    quanpin = keycaps.get("rime_ice_quanpin")
    assert isinstance(quanpin, dict)
    assert _is_strict_int(quanpin.get("rows")) and quanpin["rows"] == 5
    assert quanpin.get("dedicatedTopNumberRow") is True
    assert quanpin.get("topRightSublabel") == "swipe-up-symbol"


def assert_prototype_reconciled_values(tokens: dict[str, Any]) -> None:
    """Verify canonical token values that supersede earlier draft HTML prototypes."""
    assert tokens["brand"]["name"] == "Vitral"
    assert tokens["typography"]["toolbarTextLabelsAllowed"] is False
    assert tokens["typography"]["candidateFontSizeDefaultPx"] == 16
    assert tokens["typography"]["candidateFontSizeRangePx"] == [13, 22]
    assert tokens["motion"]["desktopIdleOpacity"] == 0.35
    assert tokens["radius"]["desktopCandidateWindowPx"] == 16
    assert tokens["radius"]["mobileKeycapPx"] == 10
    assert tokens["radius"]["popoverPanelPx"] == 16
    assert tokens["handwriting"]["strokeWidthPx"] == 3.5
    assert tokens["handwriting"]["superimposedInkFadeOutMs"] == 420
    assert tokens["handwriting"]["retroactiveHighlightDurationMs"] == 600
    floating = tokens["ergonomics"]["mobileFloatingWindow"]
    assert floating["minWidthDp"] == 220
    assert floating["minHeightDp"] == 165


def assert_english_only_brand(text: str, *, require_present: bool = True, strict_ascii_only: bool = False) -> None:
    assert isinstance(text, str), "Brand text must be str"
    lowered = text.lower()
    for forbidden in ("pureime", "pure-ime", "pure ime"):
        assert forbidden not in lowered, f"Forbidden legacy brand {forbidden!r} found in {text!r}"
    if require_present:
        assert "Vitral" in text, f"Canonical brand 'Vitral' missing from {text!r}"
    if strict_ascii_only:
        assert text.isascii(), f"Brand string must be ASCII-only English: {text!r}"
    assert not re.search(r"Vitral\s*[\u4e00-\u9fff]*输入法", text), (
        f"Chinese brand alias attached to Vitral in {text!r}"
    )


def assert_no_toolbar_text_labels(toolbar_items: list[dict[str, Any]]) -> None:
    assert isinstance(toolbar_items, list), "toolbar_items must be a list"
    for idx, item in enumerate(toolbar_items):
        assert isinstance(item, dict), f"toolbar_items[{idx}] must be a dict"
        label = item.get("label")
        text = item.get("text")
        show_label = item.get("show_label", item.get("showLabel", False))
        assert not show_label, f"toolbar_items[{idx}] must not display text labels"
        assert not (isinstance(label, str) and label.strip()), (
            f"toolbar_items[{idx}] has non-empty label {label!r}; toolbarTextLabelsAllowed is False"
        )
        assert not (isinstance(text, str) and text.strip()), (
            f"toolbar_items[{idx}] has non-empty text {text!r}; toolbarTextLabelsAllowed is False"
        )
        if "stroke_width" in item or "strokeWidthPx" in item:
            sw = item.get("stroke_width", item.get("strokeWidthPx"))
            assert _is_strict_number(sw) and float(sw) == 1.75, (
                f"toolbar_items[{idx}] stroke width {sw!r} != 1.75"
            )


def assert_all_token_invariants(tokens: dict[str, Any]) -> dict[str, Any]:
    assert_brand_invariants(tokens)
    assert_dark_palette_invariants(tokens)
    assert_light_palette_invariants(tokens)
    assert_blur_invariants(tokens)
    assert_radius_invariants(tokens)
    assert_typography_invariants(tokens)
    assert_motion_invariants(tokens)
    assert_handwriting_invariants(tokens)
    assert_ergonomics_invariants(tokens)
    assert_schema_keycaps_invariants(tokens)
    assert_prototype_reconciled_values(tokens)
    return tokens
