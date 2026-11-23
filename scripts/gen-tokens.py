#!/usr/bin/env python3
"""Design token validator and multi-target codegen for Vitral.

Reads tokens/design-tokens.json (v1.1.0), enforces all 10 frozen visual
invariant groups, and emits deterministic artifacts for:
  1. crates/vitral-tokens/src/generated.rs
  2. linux/fcitx5-vitral/include/vitral_tokens.hpp
  3. crates/vitral-tsf/include/vitral_tokens.hpp (byte-identical)
  4. apps/android/app/src/main/java/dev/vitral/ime/theme/VitralTokens.kt
  5. apps/desktop-tauri/src/generated/tokens.css
  6. apps/desktop-tauri/src/generated/tokens.ts
"""

from __future__ import annotations

import argparse
import difflib
import json
import math
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

HEX_COLOR_RE = re.compile(r"^#([0-9a-fA-F]{6})$")
RGBA_COLOR_RE = re.compile(
    r"^rgba\(\s*(\d{1,3})\s*,\s*(\d{1,3})\s*,\s*(\d{1,3})\s*,\s*(0(?:\.\d+)?|1(?:\.0+)?)\s*\)$"
)
SHADOW_RE = re.compile(
    r"^(-?\d+(?:\.\d+)?)(?:px)?\s+(-?\d+(?:\.\d+)?)(?:px)?\s+(\d+(?:\.\d+)?)(?:px)?\s+(rgba\([^)]+\)|#[0-9a-fA-F]{6})$"
)
BEZIER_RE = re.compile(
    r"^cubic-bezier\(\s*([0-9.]+)\s*,\s*([0-9.]+)\s*,\s*([0-9.]+)\s*,\s*([0-9.]+)\s*\)$"
)

COLOR_KEYS = (
    "surfacePanel",
    "surfacePopover",
    "surfaceKeyPrimary",
    "surfaceKeyFunction",
    "surfaceKeyPressedGlow",
    "borderSpecular",
    "accentPrimary",
    "accentSoft",
    "accentText",
    "textPrimary",
    "textSecondary",
    "shadowElevation",
    "inkStrokePrimary",
    "retroactiveFixGlow",
)

PURE_COLOR_KEYS = tuple(k for k in COLOR_KEYS if k != "shadowElevation")

CAMEL_TO_SNAKE_OVERRIDES = {
    "surfacePanel": "surface_panel",
    "surfacePopover": "surface_popover",
    "surfaceKeyPrimary": "surface_key_primary",
    "surfaceKeyFunction": "surface_key_function",
    "surfaceKeyPressedGlow": "surface_key_pressed_glow",
    "borderSpecular": "border_specular",
    "accentPrimary": "accent_primary",
    "accentSoft": "accent_soft",
    "accentText": "accent_text",
    "textPrimary": "text_primary",
    "textSecondary": "text_secondary",
    "shadowElevation": "shadow_elevation",
    "inkStrokePrimary": "ink_stroke_primary",
    "retroactiveFixGlow": "retroactive_fix_glow",
}

CAMEL_TO_CSS_VARS = {
    "surfacePanel": "--vitral-surface-panel",
    "surfacePopover": "--vitral-surface-popover",
    "surfaceKeyPrimary": "--vitral-surface-key-primary",
    "surfaceKeyFunction": "--vitral-surface-key-function",
    "surfaceKeyPressedGlow": "--vitral-surface-key-pressed-glow",
    "borderSpecular": "--vitral-border-specular",
    "accentPrimary": "--vitral-accent-primary",
    "accentSoft": "--vitral-accent-soft",
    "accentText": "--vitral-accent-text",
    "textPrimary": "--vitral-text-primary",
    "textSecondary": "--vitral-text-secondary",
    "shadowElevation": "--vitral-shadow-elevation",
    "inkStrokePrimary": "--vitral-ink-stroke-primary",
    "retroactiveFixGlow": "--vitral-retroactive-fix-glow",
}


@dataclass(frozen=True)
class RgbaColor:
    r: int
    g: int
    b: int
    a_u8: int
    alpha: float
    css: str

    @property
    def argb_u32(self) -> int:
        return (self.a_u8 << 24) | (self.r << 16) | (self.g << 8) | self.b

    @property
    def rgba_u32(self) -> int:
        return (self.r << 24) | (self.g << 16) | (self.b << 8) | self.a_u8


@dataclass(frozen=True)
class ShadowSpec:
    offset_x_px: float
    offset_y_px: float
    blur_px: float
    color: RgbaColor
    css: str


def parse_color(value: str) -> RgbaColor:
    if not isinstance(value, str):
        raise ValueError(f"Expected color string, got {type(value).__name__}")
    m_hex = HEX_COLOR_RE.match(value.strip())
    if m_hex:
        hex_str = m_hex.group(1)
        r = int(hex_str[0:2], 16)
        g = int(hex_str[2:4], 16)
        b = int(hex_str[4:6], 16)
        return RgbaColor(r=r, g=g, b=b, a_u8=255, alpha=1.0, css=value.strip())

    m_rgba = RGBA_COLOR_RE.match(value.strip())
    if m_rgba:
        r = int(m_rgba.group(1))
        g = int(m_rgba.group(2))
        b = int(m_rgba.group(3))
        alpha = float(m_rgba.group(4))
        if not (0 <= r <= 255 and 0 <= g <= 255 and 0 <= b <= 255):
            raise ValueError(f"RGB channel out of range in {value!r}")
        if not (0.0 <= alpha <= 1.0):
            raise ValueError(f"Alpha out of range in {value!r}")
        a_u8 = round(alpha * 255)
        return RgbaColor(r=r, g=g, b=b, a_u8=a_u8, alpha=alpha, css=value.strip())

    raise ValueError(f"Invalid color format: {value!r}")


def parse_shadow(value: str) -> ShadowSpec:
    if not isinstance(value, str):
        raise ValueError(f"Expected shadow string, got {type(value).__name__}")
    m = SHADOW_RE.match(value.strip())
    if not m:
        raise ValueError(f"Invalid shadowElevation format: {value!r}")
    ox = float(m.group(1))
    oy = float(m.group(2))
    blur = float(m.group(3))
    if blur < 0:
        raise ValueError(f"Negative shadow blur in {value!r}")
    col = parse_color(m.group(4))
    return ShadowSpec(offset_x_px=ox, offset_y_px=oy, blur_px=blur, color=col, css=value.strip())


def parse_cubic_bezier(value: str) -> tuple[float, float, float, float]:
    if not isinstance(value, str):
        raise ValueError(f"Expected cubic-bezier string, got {type(value).__name__}")
    m = BEZIER_RE.match(value.strip())
    if not m:
        raise ValueError(f"Invalid cubic-bezier format: {value!r}")
    coords = tuple(float(m.group(i)) for i in range(1, 5))
    if any(not math.isfinite(c) for c in coords):
        raise ValueError(f"Non-finite cubic-bezier value: {value!r}")
    return coords  # type: ignore[return-value]


def approx_eq(a: float, b: float, eps: float = 1e-6) -> bool:
    return isinstance(a, (int, float)) and not isinstance(a, bool) and abs(float(a) - b) <= eps


def validate_tokens(doc: dict[str, Any]) -> tuple[dict[str, dict[str, Any]], tuple[float, float, float, float]]:
    errors: list[str] = []

    def check(cond: bool, msg: str) -> None:
        if not cond:
            errors.append(msg)

    # Group 1 — Schema & Single-Source Integrity
    check(doc.get("$schema") == "https://vitral.dev/schemas/design-tokens-v1.json", "Group 1: $schema mismatch")
    brand = doc.get("brand", {})
    check(brand.get("name") == "Vitral", f"Group 1: brand.name must be 'Vitral', got {brand.get('name')!r}")
    check(
        all(isinstance(v, str) and v.isascii() for v in brand.values()),
        "Group 1: brand metadata must remain ASCII (no Chinese brand alias)",
    )
    check(brand.get("version") == "1.1.0", f"Group 1: brand.version must be '1.1.0', got {brand.get('version')!r}")
    check(brand.get("materialStyle") == "frosted-glass", "Group 1: brand.materialStyle must be 'frosted-glass'")
    check(brand.get("cloudRoot") == "/Koofr/Vitral", "Group 1: brand.cloudRoot must be '/Koofr/Vitral'")

    # Group 2 — Dark & Light Color Fidelity
    color_root = doc.get("color", {})
    parsed_themes: dict[str, dict[str, Any]] = {}
    for theme_name in ("dark", "light"):
        theme = color_root.get(theme_name)
        if not isinstance(theme, dict):
            errors.append(f"Group 2: missing color.{theme_name} object")
            continue
        missing = [k for k in COLOR_KEYS if k not in theme]
        extra = [k for k in theme.keys() if k not in COLOR_KEYS]
        check(not missing, f"Group 2: color.{theme_name} missing keys: {missing}")
        check(not extra, f"Group 2: color.{theme_name} unexpected keys: {extra}")
        parsed: dict[str, Any] = {}
        for k in PURE_COLOR_KEYS:
            if k in theme:
                try:
                    parsed[k] = parse_color(theme[k])
                except ValueError as exc:
                    errors.append(f"Group 2: color.{theme_name}.{k}: {exc}")
        if "shadowElevation" in theme:
            try:
                parsed["shadowElevation"] = parse_shadow(theme["shadowElevation"])
            except ValueError as exc:
                errors.append(f"Group 2: color.{theme_name}.shadowElevation: {exc}")
        parsed_themes[theme_name] = parsed

    if "dark" in parsed_themes and "light" in parsed_themes:
        d = parsed_themes["dark"]
        l = parsed_themes["light"]
        if "surfacePanel" in d:
            check(approx_eq(d["surfacePanel"].alpha, 0.68), "Group 2: dark.surfacePanel alpha must be 0.68")
        if "surfacePopover" in d:
            check(approx_eq(d["surfacePopover"].alpha, 0.74), "Group 2: dark.surfacePopover alpha must be 0.74")
        if "accentPrimary" in d:
            check(d["accentPrimary"].css.lower() == "#0ea5e9", "Group 2: dark.accentPrimary must be #0ea5e9")
        if "surfacePanel" in l:
            check(approx_eq(l["surfacePanel"].alpha, 0.74), "Group 2: light.surfacePanel alpha must be 0.74")
        if "surfacePopover" in l:
            check(approx_eq(l["surfacePopover"].alpha, 0.80), "Group 2: light.surfacePopover alpha must be 0.80")
        if "accentPrimary" in l:
            check(l["accentPrimary"].css.lower() == "#0284c7", "Group 2: light.accentPrimary must be #0284c7")

    # Group 3 — Blur & Elevation
    blur = doc.get("blur", {})
    check(blur.get("backdropStandardPx") == 18, "Group 3: blur.backdropStandardPx must be 18")
    check(blur.get("backdropFloatingWindowPx") == 20, "Group 3: blur.backdropFloatingWindowPx must be 20")

    # Group 4 — Corner Radii
    radius = doc.get("radius", {})
    check(radius.get("desktopStatusCapsulePx") == 9999, "Group 4: desktopStatusCapsulePx must be 9999")
    check(radius.get("desktopCandidateWindowPx") == 16, "Group 4: desktopCandidateWindowPx must be 16")
    check(radius.get("candidateActivePillPx") == 10, "Group 4: candidateActivePillPx must be 10")
    check(radius.get("mobileKeyboardSheetPx") == 16, "Group 4: mobileKeyboardSheetPx must be 16")
    check(radius.get("mobileKeycapPx") == 10, "Group 4: mobileKeycapPx must be 10")
    check(radius.get("popoverPanelPx") == 16, "Group 4: popoverPanelPx must be 16")

    # Group 5 — Typography & Zero-Text-Label Toolbar
    typo = doc.get("typography", {})
    check(typo.get("numericVariant") == "tabular-nums", "Group 5: numericVariant must be 'tabular-nums'")
    check(typo.get("candidateFontSizeDefaultPx") == 16, "Group 5: candidateFontSizeDefaultPx must be 16")
    check(typo.get("candidateFontSizeRangePx") == [13, 22], "Group 5: candidateFontSizeRangePx must be [13, 22]")
    check(approx_eq(typo.get("toolbarIconStrokeWidthPx"), 1.75), "Group 5: toolbarIconStrokeWidthPx must be 1.75")
    check(typo.get("toolbarTextLabelsAllowed") is False, "Group 5: toolbarTextLabelsAllowed must be false")

    # Group 6 — Motion & Idle Behavior
    motion = doc.get("motion", {})
    check(motion.get("springCurve") == "cubic-bezier(0.22, 1, 0.36, 1)", "Group 6: motion.springCurve mismatch")
    bezier: tuple[float, float, float, float] = (0.22, 1.0, 0.36, 1.0)
    try:
        bezier = parse_cubic_bezier(motion.get("springCurve", ""))
    except ValueError as exc:
        errors.append(f"Group 6: {exc}")
    check(motion.get("durationFastMs") == 150, "Group 6: motion.durationFastMs must be 150")
    check(motion.get("durationStandardMs") == 180, "Group 6: motion.durationStandardMs must be 180")
    check(motion.get("desktopIdleFadeDelayMs") == 5000, "Group 6: motion.desktopIdleFadeDelayMs must be 5000")
    check(approx_eq(motion.get("desktopIdleOpacity"), 0.35), "Group 6: motion.desktopIdleOpacity must be 0.35")

    # Group 7 — Desktop Capsule & Candidate Ergonomics
    erg = doc.get("ergonomics", {})
    check(erg.get("desktopCandidateOrientation") == "horizontal", "Group 7: desktopCandidateOrientation must be 'horizontal'")
    check(erg.get("desktopCandidatePageSize") == 5, "Group 7: desktopCandidatePageSize must be 5")
    check(
        erg.get("desktopInitialToolbarItems") == ["vitral-logo", "lang-toggle"],
        "Group 7: desktopInitialToolbarItems must be ['vitral-logo', 'lang-toggle']",
    )

    # Group 8 — Handwriting Engine Visual Invariants
    hw = doc.get("handwriting", {})
    check(
        hw.get("modes") == ["phonetic-lookup-single", "continuous-contextual-lattice"],
        "Group 8: handwriting.modes mismatch",
    )
    check(approx_eq(hw.get("strokeWidthPx"), 3.5), "Group 8: handwriting.strokeWidthPx must be 3.5")
    check(hw.get("superimposedInkFadeOutMs") == 420, "Group 8: handwriting.superimposedInkFadeOutMs must be 420")
    check(
        hw.get("retroactiveHighlightDurationMs") == 600,
        "Group 8: handwriting.retroactiveHighlightDurationMs must be 600",
    )
    check(hw.get("confusionTopK") == 8, "Group 8: handwriting.confusionTopK must be 8")
    check(
        approx_eq(hw.get("ambiguousMarginThreshold"), 0.15),
        "Group 8: handwriting.ambiguousMarginThreshold must be 0.15",
    )
    check(hw.get("showXiaoheCodeInLookupMode") is True, "Group 8: showXiaoheCodeInLookupMode must be true")

    # Group 9 — Mobile Ergonomics & Keycap Schemas
    check(approx_eq(erg.get("mobileDockedScreenRatio"), 0.26), "Group 9: mobileDockedScreenRatio must be 0.26")
    check(
        erg.get("mobileTopHierarchy") == ["icon-toolbar", "candidate-bar", "key-grid"],
        "Group 9: mobileTopHierarchy must be ['icon-toolbar', 'candidate-bar', 'key-grid']",
    )
    fw = erg.get("mobileFloatingWindow", {})
    check(fw.get("freeDrag") is True, "Group 9: mobileFloatingWindow.freeDrag must be true")
    check(fw.get("freeResize") is True, "Group 9: mobileFloatingWindow.freeResize must be true")
    check(fw.get("minWidthDp") == 220, "Group 9: mobileFloatingWindow.minWidthDp must be 220")
    check(fw.get("minHeightDp") == 165, "Group 9: mobileFloatingWindow.minHeightDp must be 165")
    check(erg.get("keyFeedbackMode") == "in-key-specular-glow", "Group 9: keyFeedbackMode must be 'in-key-specular-glow'")
    check(erg.get("spacebarVoiceHoldThresholdMs") == 350, "Group 9: spacebarVoiceHoldThresholdMs must be 350")
    check(erg.get("spacebarVoiceVisual") == "in-place-waveform", "Group 9: spacebarVoiceVisual must be 'in-place-waveform'")
    hap = erg.get("haptics", {})
    check(hap.get("letterTickMs") == 10, "Group 9: haptics.letterTickMs must be 10")
    check(hap.get("actionThudMs") == 16, "Group 9: haptics.actionThudMs must be 16")

    sk = erg.get("schemaKeycaps", {})
    flypy = sk.get("double_pinyin_flypy", {})
    check(flypy.get("rows") == 4, "Group 9: double_pinyin_flypy.rows must be 4")
    check(
        flypy.get("topLeftSublabel") == "swipe-up-number-or-symbol",
        "Group 9: double_pinyin_flypy.topLeftSublabel mismatch",
    )
    check(
        flypy.get("bottomRightSublabel") == "xiaohe-final",
        "Group 9: double_pinyin_flypy.bottomRightSublabel mismatch",
    )
    quanpin = sk.get("rime_ice_quanpin", {})
    check(quanpin.get("rows") == 4, "Group 9: rime_ice_quanpin.rows must be 4")
    check(quanpin.get("dedicatedTopNumberRow") is False, "Group 9: rime_ice_quanpin.dedicatedTopNumberRow must be false")
    check(quanpin.get("topRightSublabel") == "swipe-up-number-or-symbol", "Group 9: rime_ice_quanpin.topRightSublabel mismatch")

    if errors:
        raise ValueError("Design token invariant validation failed:\n  - " + "\n  - ".join(errors))

    return parsed_themes, bezier


def rs_str(val: str) -> str:
    return json.dumps(val, ensure_ascii=False)


def rs_rgba(col: RgbaColor) -> str:
    return (
        f"RgbaColor {{ r: {col.r}, g: {col.g}, b: {col.b}, a: {col.a_u8}, "
        f"alpha: {col.alpha:.6f}, argb_u32: 0x{col.argb_u32:08X}, "
        f"rgba_u32: 0x{col.rgba_u32:08X}, css: {rs_str(col.css)} }}"
    )


def rs_shadow(sh: ShadowSpec) -> str:
    return (
        f"ShadowSpec {{ offset_x_px: {sh.offset_x_px:.1f}, offset_y_px: {sh.offset_y_px:.1f}, "
        f"blur_px: {sh.blur_px:.1f}, color: {rs_rgba(sh.color)}, css: {rs_str(sh.css)} }}"
    )


def rs_theme(parsed: dict[str, Any]) -> str:
    lines = ["ThemePalette {"]
    for k in COLOR_KEYS:
        field = CAMEL_TO_SNAKE_OVERRIDES[k]
        if k == "shadowElevation":
            lines.append(f"        {field}: {rs_shadow(parsed[k])},")
        else:
            lines.append(f"        {field}: {rs_rgba(parsed[k])},")
    lines.append("    }")
    return "\n".join(lines)


def render_rust(doc: dict[str, Any], themes: dict[str, dict[str, Any]], bezier: tuple[float, float, float, float]) -> str:
    b = doc["brand"]
    bl = doc["blur"]
    r = doc["radius"]
    t = doc["typography"]
    m = doc["motion"]
    hw = doc["handwriting"]
    e = doc["ergonomics"]
    fw = e["mobileFloatingWindow"]
    hap = e["haptics"]
    flypy = e["schemaKeycaps"]["double_pinyin_flypy"]
    qp = e["schemaKeycaps"]["rime_ice_quanpin"]

    return f"""// @generated by scripts/gen-tokens.py — DO NOT EDIT.
// Source: tokens/design-tokens.json (v{b['version']})

use crate::{{
    BlurSpec, BrandSpec, DesignTokensStatic, ErgonomicsSpec, FloatingWindowSpec,
    HandwritingSpec, HapticsSpec, MotionCurve, MotionSpec, QuanpinKeycapSpec, RadiusSpec,
    RgbaColor, SchemaKeycapsSpec, ShadowSpec, ThemePalette, TypographySpec, XiaoheKeycapSpec,
}};

pub const RAW_DESIGN_TOKENS_JSON: &str = include_str!("../../../tokens/design-tokens.json");

pub const TOKENS: DesignTokensStatic = DesignTokensStatic {{
    schema: {rs_str(doc['$schema'])},
    brand: BrandSpec {{
        name: {rs_str(b['name'])},
        version: {rs_str(b['version'])},
        material_style: {rs_str(b['materialStyle'])},
        cloud_root: {rs_str(b['cloudRoot'])},
    }},
    dark: {rs_theme(themes['dark'])},
    light: {rs_theme(themes['light'])},
    blur: BlurSpec {{
        backdrop_standard_px: {bl['backdropStandardPx']},
        backdrop_floating_window_px: {bl['backdropFloatingWindowPx']},
    }},
    radius: RadiusSpec {{
        desktop_status_capsule_px: {r['desktopStatusCapsulePx']},
        desktop_candidate_window_px: {r['desktopCandidateWindowPx']},
        candidate_active_pill_px: {r['candidateActivePillPx']},
        mobile_keyboard_sheet_px: {r['mobileKeyboardSheetPx']},
        mobile_keycap_px: {r['mobileKeycapPx']},
        popover_panel_px: {r['popoverPanelPx']},
    }},
    typography: TypographySpec {{
        numeric_variant: {rs_str(t['numericVariant'])},
        candidate_font_size_default_px: {t['candidateFontSizeDefaultPx']},
        candidate_font_size_range_px: [{t['candidateFontSizeRangePx'][0]}, {t['candidateFontSizeRangePx'][1]}],
        toolbar_icon_stroke_width_px: {t['toolbarIconStrokeWidthPx']:.2f},
        toolbar_text_labels_allowed: {str(t['toolbarTextLabelsAllowed']).lower()},
    }},
    motion: MotionSpec {{
        spring_curve: MotionCurve {{
            x1: {bezier[0]:.4f},
            y1: {bezier[1]:.4f},
            x2: {bezier[2]:.4f},
            y2: {bezier[3]:.4f},
            css: {rs_str(m['springCurve'])},
        }},
        duration_fast_ms: {m['durationFastMs']},
        duration_standard_ms: {m['durationStandardMs']},
        desktop_idle_fade_delay_ms: {m['desktopIdleFadeDelayMs']},
        desktop_idle_opacity: {m['desktopIdleOpacity']:.2f},
    }},
    handwriting: HandwritingSpec {{
        modes: [{rs_str(hw['modes'][0])}, {rs_str(hw['modes'][1])}],
        stroke_width_px: {hw['strokeWidthPx']:.2f},
        superimposed_ink_fade_out_ms: {hw['superimposedInkFadeOutMs']},
        retroactive_highlight_duration_ms: {hw['retroactiveHighlightDurationMs']},
        confusion_top_k: {hw['confusionTopK']},
        ambiguous_margin_threshold: {hw['ambiguousMarginThreshold']:.2f},
        show_xiaohe_code_in_lookup_mode: {str(hw['showXiaoheCodeInLookupMode']).lower()},
    }},
    ergonomics: ErgonomicsSpec {{
        desktop_candidate_orientation: {rs_str(e['desktopCandidateOrientation'])},
        desktop_candidate_page_size: {e['desktopCandidatePageSize']},
        desktop_initial_toolbar_items: [{rs_str(e['desktopInitialToolbarItems'][0])}, {rs_str(e['desktopInitialToolbarItems'][1])}],
        mobile_docked_screen_ratio: {e['mobileDockedScreenRatio']:.2f},
        mobile_top_hierarchy: [
            {rs_str(e['mobileTopHierarchy'][0])},
            {rs_str(e['mobileTopHierarchy'][1])},
            {rs_str(e['mobileTopHierarchy'][2])},
        ],
        mobile_floating_window: FloatingWindowSpec {{
            free_drag: {str(fw['freeDrag']).lower()},
            free_resize: {str(fw['freeResize']).lower()},
            min_width_dp: {fw['minWidthDp']},
            min_height_dp: {fw['minHeightDp']},
        }},
        key_feedback_mode: {rs_str(e['keyFeedbackMode'])},
        spacebar_voice_hold_threshold_ms: {e['spacebarVoiceHoldThresholdMs']},
        spacebar_voice_visual: {rs_str(e['spacebarVoiceVisual'])},
        haptics: HapticsSpec {{
            letter_tick_ms: {hap['letterTickMs']},
            action_thud_ms: {hap['actionThudMs']},
        }},
        schema_keycaps: SchemaKeycapsSpec {{
            double_pinyin_flypy: XiaoheKeycapSpec {{
                rows: {flypy['rows']},
                top_left_sublabel: {rs_str(flypy['topLeftSublabel'])},
                bottom_right_sublabel: {rs_str(flypy['bottomRightSublabel'])},
            }},
            rime_ice_quanpin: QuanpinKeycapSpec {{
                rows: {qp['rows']},
                dedicated_top_number_row: {str(qp['dedicatedTopNumberRow']).lower()},
                top_right_sublabel: {rs_str(qp['topRightSublabel'])},
            }},
        }},
    }},
}};
"""


def cpp_rgba(col: RgbaColor) -> str:
    return (
        f"RgbaColor{{{col.r}, {col.g}, {col.b}, {col.a_u8}, "
        f"{col.alpha:.6f}f, 0x{col.argb_u32:08X}u, 0x{col.rgba_u32:08X}u, {rs_str(col.css)}}}"
    )


def cpp_shadow(sh: ShadowSpec) -> str:
    return (
        f"ShadowSpec{{{sh.offset_x_px:.1f}f, {sh.offset_y_px:.1f}f, {sh.blur_px:.1f}f, "
        f"{cpp_rgba(sh.color)}, {rs_str(sh.css)}}}"
    )


def cpp_theme(parsed: dict[str, Any]) -> str:
    items = []
    for k in COLOR_KEYS:
        if k == "shadowElevation":
            items.append(f"    {cpp_shadow(parsed[k])}")
        else:
            items.append(f"    {cpp_rgba(parsed[k])}")
    return "{\n" + ",\n".join(items) + "\n}"


def render_cpp(doc: dict[str, Any], themes: dict[str, dict[str, Any]], bezier: tuple[float, float, float, float]) -> str:
    b = doc["brand"]
    bl = doc["blur"]
    r = doc["radius"]
    t = doc["typography"]
    m = doc["motion"]
    hw = doc["handwriting"]
    e = doc["ergonomics"]
    fw = e["mobileFloatingWindow"]
    hap = e["haptics"]
    flypy = e["schemaKeycaps"]["double_pinyin_flypy"]
    qp = e["schemaKeycaps"]["rime_ice_quanpin"]

    return f"""// @generated by scripts/gen-tokens.py — DO NOT EDIT.
// Source: tokens/design-tokens.json (v{b['version']})

#pragma once
#ifndef VITRAL_TOKENS_HPP
#define VITRAL_TOKENS_HPP

#include <array>
#include <cstdint>
#include <string_view>

namespace vitral::tokens {{

struct RgbaColor {{
    uint8_t r;
    uint8_t g;
    uint8_t b;
    uint8_t a;
    float alpha;
    uint32_t argb;
    uint32_t rgba;
    std::string_view css;
}};

struct ShadowSpec {{
    float offset_x_px;
    float offset_y_px;
    float blur_px;
    RgbaColor color;
    std::string_view css;
}};

struct ThemePalette {{
    RgbaColor surface_panel;
    RgbaColor surface_popover;
    RgbaColor surface_key_primary;
    RgbaColor surface_key_function;
    RgbaColor surface_key_pressed_glow;
    RgbaColor border_specular;
    RgbaColor accent_primary;
    RgbaColor accent_soft;
    RgbaColor accent_text;
    RgbaColor text_primary;
    RgbaColor text_secondary;
    ShadowSpec shadow_elevation;
    RgbaColor ink_stroke_primary;
    RgbaColor retroactive_fix_glow;
}};

inline constexpr ThemePalette kDarkTheme = {cpp_theme(themes['dark'])};

inline constexpr ThemePalette kLightTheme = {cpp_theme(themes['light'])};

namespace brand {{
inline constexpr std::string_view kSchema = {rs_str(doc['$schema'])};
inline constexpr std::string_view kName = {rs_str(b['name'])};
inline constexpr std::string_view kVersion = {rs_str(b['version'])};
inline constexpr std::string_view kMaterialStyle = {rs_str(b['materialStyle'])};
inline constexpr std::string_view kCloudRoot = {rs_str(b['cloudRoot'])};
}}  // namespace brand

namespace blur {{
inline constexpr int32_t kBackdropStandardPx = {bl['backdropStandardPx']};
inline constexpr int32_t kBackdropFloatingWindowPx = {bl['backdropFloatingWindowPx']};
}}  // namespace blur

namespace radius {{
inline constexpr int32_t kDesktopStatusCapsulePx = {r['desktopStatusCapsulePx']};
inline constexpr int32_t kDesktopCandidateWindowPx = {r['desktopCandidateWindowPx']};
inline constexpr int32_t kCandidateActivePillPx = {r['candidateActivePillPx']};
inline constexpr int32_t kMobileKeyboardSheetPx = {r['mobileKeyboardSheetPx']};
inline constexpr int32_t kMobileKeycapPx = {r['mobileKeycapPx']};
inline constexpr int32_t kPopoverPanelPx = {r['popoverPanelPx']};
}}  // namespace radius

namespace typography {{
inline constexpr std::string_view kNumericVariant = {rs_str(t['numericVariant'])};
inline constexpr int32_t kCandidateFontSizeDefaultPx = {t['candidateFontSizeDefaultPx']};
inline constexpr int32_t kCandidateFontSizeMinPx = {t['candidateFontSizeRangePx'][0]};
inline constexpr int32_t kCandidateFontSizeMaxPx = {t['candidateFontSizeRangePx'][1]};
inline constexpr std::array<int32_t, 2> kCandidateFontSizeRangePx = {{{t['candidateFontSizeRangePx'][0]}, {t['candidateFontSizeRangePx'][1]}}};
inline constexpr float kToolbarIconStrokeWidthPx = {t['toolbarIconStrokeWidthPx']:.2f}f;
inline constexpr bool kToolbarTextLabelsAllowed = {str(t['toolbarTextLabelsAllowed']).lower()};
}}  // namespace typography

namespace motion {{
inline constexpr std::string_view kSpringCurveCss = {rs_str(m['springCurve'])};
inline constexpr float kSpringX1 = {bezier[0]:.4f}f;
inline constexpr float kSpringY1 = {bezier[1]:.4f}f;
inline constexpr float kSpringX2 = {bezier[2]:.4f}f;
inline constexpr float kSpringY2 = {bezier[3]:.4f}f;
inline constexpr int32_t kDurationFastMs = {m['durationFastMs']};
inline constexpr int32_t kDurationStandardMs = {m['durationStandardMs']};
inline constexpr int32_t kDesktopIdleFadeDelayMs = {m['desktopIdleFadeDelayMs']};
inline constexpr float kDesktopIdleOpacity = {m['desktopIdleOpacity']:.2f}f;
}}  // namespace motion

namespace handwriting {{
inline constexpr std::array<std::string_view, 2> kModes = {{{rs_str(hw['modes'][0])}, {rs_str(hw['modes'][1])}}};
inline constexpr float kStrokeWidthPx = {hw['strokeWidthPx']:.2f}f;
inline constexpr int32_t kSuperimposedInkFadeOutMs = {hw['superimposedInkFadeOutMs']};
inline constexpr int32_t kRetroactiveHighlightDurationMs = {hw['retroactiveHighlightDurationMs']};
inline constexpr int32_t kConfusionTopK = {hw['confusionTopK']};
inline constexpr float kAmbiguousMarginThreshold = {hw['ambiguousMarginThreshold']:.2f}f;
inline constexpr bool kShowXiaoheCodeInLookupMode = {str(hw['showXiaoheCodeInLookupMode']).lower()};
}}  // namespace handwriting

namespace ergonomics {{
inline constexpr std::string_view kDesktopCandidateOrientation = {rs_str(e['desktopCandidateOrientation'])};
inline constexpr int32_t kDesktopCandidatePageSize = {e['desktopCandidatePageSize']};
inline constexpr std::array<std::string_view, 2> kDesktopInitialToolbarItems = {{{rs_str(e['desktopInitialToolbarItems'][0])}, {rs_str(e['desktopInitialToolbarItems'][1])}}};
inline constexpr float kMobileDockedScreenRatio = {e['mobileDockedScreenRatio']:.2f}f;
inline constexpr std::array<std::string_view, 3> kMobileTopHierarchy = {{{rs_str(e['mobileTopHierarchy'][0])}, {rs_str(e['mobileTopHierarchy'][1])}, {rs_str(e['mobileTopHierarchy'][2])}}};
inline constexpr bool kFloatingFreeDrag = {str(fw['freeDrag']).lower()};
inline constexpr bool kFloatingFreeResize = {str(fw['freeResize']).lower()};
inline constexpr int32_t kFloatingMinWidthDp = {fw['minWidthDp']};
inline constexpr int32_t kFloatingMinHeightDp = {fw['minHeightDp']};
inline constexpr std::string_view kKeyFeedbackMode = {rs_str(e['keyFeedbackMode'])};
inline constexpr int32_t kSpacebarVoiceHoldThresholdMs = {e['spacebarVoiceHoldThresholdMs']};
inline constexpr std::string_view kSpacebarVoiceVisual = {rs_str(e['spacebarVoiceVisual'])};
inline constexpr int32_t kHapticLetterTickMs = {hap['letterTickMs']};
inline constexpr int32_t kHapticActionThudMs = {hap['actionThudMs']};
inline constexpr int32_t kFlypyRows = {flypy['rows']};
inline constexpr std::string_view kFlypyTopLeftSublabel = {rs_str(flypy['topLeftSublabel'])};
inline constexpr std::string_view kFlypyBottomRightSublabel = {rs_str(flypy['bottomRightSublabel'])};
inline constexpr int32_t kQuanpinRows = {qp['rows']};
inline constexpr bool kQuanpinDedicatedTopNumberRow = {str(qp['dedicatedTopNumberRow']).lower()};
inline constexpr std::string_view kQuanpinTopRightSublabel = {rs_str(qp['topRightSublabel'])};
}}  // namespace ergonomics

}}  // namespace vitral::tokens

#endif  // VITRAL_TOKENS_HPP
"""


def render_kotlin_theme_obj(name: str, parsed: dict[str, Any]) -> str:
    lines = [f"    object {name} {{"]
    for k in COLOR_KEYS:
        if k == "shadowElevation":
            sh: ShadowSpec = parsed[k]
            lines.append(
                f"        const val shadowElevationArgb: Long = 0x{sh.color.argb_u32:08X}L"
            )
            lines.append(
                f"        val shadowElevation = ShadowSpec({sh.offset_x_px:.1f}f, {sh.offset_y_px:.1f}f, "
                f"{sh.blur_px:.1f}f, shadowElevationArgb, Color(shadowElevationArgb))"
            )
        else:
            col: RgbaColor = parsed[k]
            lines.append(f"        const val {k}Argb: Long = 0x{col.argb_u32:08X}L")
            lines.append(f"        val {k}: Color = Color({k}Argb)")
    lines.append("    }")
    return "\n".join(lines)


def render_kotlin(doc: dict[str, Any], themes: dict[str, dict[str, Any]], bezier: tuple[float, float, float, float]) -> str:
    b = doc["brand"]
    bl = doc["blur"]
    r = doc["radius"]
    t = doc["typography"]
    m = doc["motion"]
    hw = doc["handwriting"]
    e = doc["ergonomics"]
    fw = e["mobileFloatingWindow"]
    hap = e["haptics"]
    flypy = e["schemaKeycaps"]["double_pinyin_flypy"]
    qp = e["schemaKeycaps"]["rime_ice_quanpin"]

    return f"""// @generated by scripts/gen-tokens.py — DO NOT EDIT.
// Source: tokens/design-tokens.json (v{b['version']})

package dev.vitral.ime.theme

import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

object VitralTokens {{
    data class ShadowSpec(
        val offsetXPx: Float,
        val offsetYPx: Float,
        val blurPx: Float,
        val colorArgb: Long,
        val color: Color,
    )

    object Brand {{
        const val schema: String = {rs_str(doc['$schema'])}
        const val name: String = {rs_str(b['name'])}
        const val version: String = {rs_str(b['version'])}
        const val materialStyle: String = {rs_str(b['materialStyle'])}
        const val cloudRoot: String = {rs_str(b['cloudRoot'])}
    }}

{render_kotlin_theme_obj("Dark", themes["dark"])}

{render_kotlin_theme_obj("Light", themes["light"])}

    object Blur {{
        const val backdropStandardPx: Int = {bl['backdropStandardPx']}
        const val backdropFloatingWindowPx: Int = {bl['backdropFloatingWindowPx']}
        val backdropStandardDp: Dp = {bl['backdropStandardPx']}.dp
        val backdropFloatingWindowDp: Dp = {bl['backdropFloatingWindowPx']}.dp
    }}

    object Radius {{
        const val desktopStatusCapsulePx: Int = {r['desktopStatusCapsulePx']}
        const val desktopCandidateWindowPx: Int = {r['desktopCandidateWindowPx']}
        const val candidateActivePillPx: Int = {r['candidateActivePillPx']}
        const val mobileKeyboardSheetPx: Int = {r['mobileKeyboardSheetPx']}
        const val mobileKeycapPx: Int = {r['mobileKeycapPx']}
        const val popoverPanelPx: Int = {r['popoverPanelPx']}
        val mobileKeyboardSheetDp: Dp = {r['mobileKeyboardSheetPx']}.dp
        val mobileKeycapDp: Dp = {r['mobileKeycapPx']}.dp
        val candidateActivePillDp: Dp = {r['candidateActivePillPx']}.dp
        val popoverPanelDp: Dp = {r['popoverPanelPx']}.dp
    }}

    object Typography {{
        const val numericVariant: String = {rs_str(t['numericVariant'])}
        const val candidateFontSizeDefaultPx: Int = {t['candidateFontSizeDefaultPx']}
        val candidateFontSizeDefaultSp: TextUnit = {t['candidateFontSizeDefaultPx']}.sp
        const val candidateFontSizeMinPx: Int = {t['candidateFontSizeRangePx'][0]}
        const val candidateFontSizeMaxPx: Int = {t['candidateFontSizeRangePx'][1]}
        const val toolbarIconStrokeWidthPx: Float = {t['toolbarIconStrokeWidthPx']:.2f}f
        const val toolbarTextLabelsAllowed: Boolean = {str(t['toolbarTextLabelsAllowed']).lower()}
    }}

    object Motion {{
        const val springCurveCss: String = {rs_str(m['springCurve'])}
        val springCurve: CubicBezierEasing = CubicBezierEasing({bezier[0]:.2f}f, {bezier[1]:.2f}f, {bezier[2]:.2f}f, {bezier[3]:.2f}f)
        const val durationFastMs: Int = {m['durationFastMs']}
        const val durationStandardMs: Int = {m['durationStandardMs']}
        const val desktopIdleFadeDelayMs: Int = {m['desktopIdleFadeDelayMs']}
        const val desktopIdleOpacity: Float = {m['desktopIdleOpacity']:.2f}f
    }}

    object Handwriting {{
        val modes: List<String> = listOf({rs_str(hw['modes'][0])}, {rs_str(hw['modes'][1])})
        const val strokeWidthPx: Float = {hw['strokeWidthPx']:.2f}f
        const val superimposedInkFadeOutMs: Int = {hw['superimposedInkFadeOutMs']}
        const val retroactiveHighlightDurationMs: Int = {hw['retroactiveHighlightDurationMs']}
        const val confusionTopK: Int = {hw['confusionTopK']}
        const val ambiguousMarginThreshold: Float = {hw['ambiguousMarginThreshold']:.2f}f
        const val showXiaoheCodeInLookupMode: Boolean = {str(hw['showXiaoheCodeInLookupMode']).lower()}
    }}

    object Ergonomics {{
        const val desktopCandidateOrientation: String = {rs_str(e['desktopCandidateOrientation'])}
        const val desktopCandidatePageSize: Int = {e['desktopCandidatePageSize']}
        val desktopInitialToolbarItems: List<String> = listOf({rs_str(e['desktopInitialToolbarItems'][0])}, {rs_str(e['desktopInitialToolbarItems'][1])})
        const val mobileDockedScreenRatio: Float = {e['mobileDockedScreenRatio']:.2f}f
        val mobileTopHierarchy: List<String> = listOf({rs_str(e['mobileTopHierarchy'][0])}, {rs_str(e['mobileTopHierarchy'][1])}, {rs_str(e['mobileTopHierarchy'][2])})
        const val floatingFreeDrag: Boolean = {str(fw['freeDrag']).lower()}
        const val floatingFreeResize: Boolean = {str(fw['freeResize']).lower()}
        const val floatingMinWidthDp: Int = {fw['minWidthDp']}
        const val floatingMinHeightDp: Int = {fw['minHeightDp']}
        val floatingMinWidth: Dp = {fw['minWidthDp']}.dp
        val floatingMinHeight: Dp = {fw['minHeightDp']}.dp
        const val keyFeedbackMode: String = {rs_str(e['keyFeedbackMode'])}
        const val spacebarVoiceHoldThresholdMs: Int = {e['spacebarVoiceHoldThresholdMs']}
        const val spacebarVoiceVisual: String = {rs_str(e['spacebarVoiceVisual'])}
        const val hapticLetterTickMs: Int = {hap['letterTickMs']}
        const val hapticActionThudMs: Int = {hap['actionThudMs']}
        const val flypyRows: Int = {flypy['rows']}
        const val flypyTopLeftSublabel: String = {rs_str(flypy['topLeftSublabel'])}
        const val flypyBottomRightSublabel: String = {rs_str(flypy['bottomRightSublabel'])}
        const val quanpinRows: Int = {qp['rows']}
        const val quanpinDedicatedTopNumberRow: Boolean = {str(qp['dedicatedTopNumberRow']).lower()}
        const val quanpinTopRightSublabel: String = {rs_str(qp['topRightSublabel'])}
    }}
}}
"""


def render_css(doc: dict[str, Any]) -> str:
    b = doc["brand"]
    bl = doc["blur"]
    r = doc["radius"]
    t = doc["typography"]
    m = doc["motion"]
    hw = doc["handwriting"]
    e = doc["ergonomics"]
    dk = doc["color"]["dark"]
    lt = doc["color"]["light"]

    dark_vars = "\n".join(f"  {CAMEL_TO_CSS_VARS[k]}: {dk[k]};" for k in COLOR_KEYS)
    light_vars = "\n".join(f"  {CAMEL_TO_CSS_VARS[k]}: {lt[k]};" for k in COLOR_KEYS)

    return f"""/* @generated by scripts/gen-tokens.py — DO NOT EDIT. */
/* Source: tokens/design-tokens.json (v{b['version']}) */

:root {{
  --vitral-blur-standard: {bl['backdropStandardPx']}px;
  --vitral-blur-floating: {bl['backdropFloatingWindowPx']}px;
  --vitral-radius-status-capsule: {r['desktopStatusCapsulePx']}px;
  --vitral-radius-candidate-window: {r['desktopCandidateWindowPx']}px;
  --vitral-radius-candidate-pill: {r['candidateActivePillPx']}px;
  --vitral-radius-mobile-sheet: {r['mobileKeyboardSheetPx']}px;
  --vitral-radius-mobile-keycap: {r['mobileKeycapPx']}px;
  --vitral-radius-popover: {r['popoverPanelPx']}px;
  --vitral-font-numeric: {t['numericVariant']};
  --vitral-candidate-font-size-default: {t['candidateFontSizeDefaultPx']}px;
  --vitral-candidate-font-size-min: {t['candidateFontSizeRangePx'][0]}px;
  --vitral-candidate-font-size-max: {t['candidateFontSizeRangePx'][1]}px;
  --vitral-toolbar-stroke-width: {t['toolbarIconStrokeWidthPx']}px;
  --vitral-motion-spring: {m['springCurve']};
  --vitral-motion-fast: {m['durationFastMs']}ms;
  --vitral-motion-standard: {m['durationStandardMs']}ms;
  --vitral-idle-fade-delay: {m['desktopIdleFadeDelayMs']}ms;
  --vitral-idle-opacity: {m['desktopIdleOpacity']};
  --vitral-hw-stroke-width: {hw['strokeWidthPx']}px;
  --vitral-hw-fade-out: {hw['superimposedInkFadeOutMs']}ms;
  --vitral-hw-retroactive-glow: {hw['retroactiveHighlightDurationMs']}ms;
  --vitral-desktop-candidate-page-size: {e['desktopCandidatePageSize']};
  --vitral-mobile-docked-ratio: {e['mobileDockedScreenRatio']};
}}

:root,
[data-theme="dark"] {{
{dark_vars}
}}

[data-theme="light"] {{
{light_vars}
}}
"""


def render_ts(doc: dict[str, Any]) -> str:
    b = doc["brand"]
    json_body = json.dumps(doc, indent=2, ensure_ascii=False)
    return f"""// @generated by scripts/gen-tokens.py — DO NOT EDIT.
// Source: tokens/design-tokens.json (v{b['version']})

export const VITRAL_TOKENS = {json_body} as const;

export type VitralThemeMode = "dark" | "light";
export type VitralTokens = typeof VITRAL_TOKENS;
"""


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate and generate Vitral design tokens.")
    parser.add_argument("--check", action="store_true", help="Verify generated files match tokens without writing.")
    parser.add_argument("--write", action="store_true", help="Write generated files (default action).")
    parser.add_argument("--tokens", type=Path, default=None, help="Path to tokens/design-tokens.json")
    parser.add_argument("--root", type=Path, default=None, help="Repository root path")
    args = parser.parse_args()

    repo_root = (args.root or Path(__file__).resolve().parent.parent).resolve()
    tokens_path = (args.tokens or (repo_root / "tokens" / "design-tokens.json")).resolve()

    if not tokens_path.is_file():
        print(f"ERROR: tokens file not found: {tokens_path}", file=sys.stderr)
        return 1

    with tokens_path.open("r", encoding="utf-8") as f:
        doc = json.load(f)

    try:
        themes, bezier = validate_tokens(doc)
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    cpp_header = render_cpp(doc, themes, bezier)
    outputs: dict[Path, str] = {
        repo_root / "crates" / "vitral-tokens" / "src" / "generated.rs": render_rust(doc, themes, bezier),
        repo_root / "linux" / "fcitx5-vitral" / "include" / "vitral_tokens.hpp": cpp_header,
        repo_root / "crates" / "vitral-tsf" / "include" / "vitral_tokens.hpp": cpp_header,
        repo_root / "apps" / "android" / "app" / "src" / "main" / "java" / "dev" / "vitral" / "ime" / "theme" / "VitralTokens.kt": render_kotlin(doc, themes, bezier),
        repo_root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.css": render_css(doc),
        repo_root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.ts": render_ts(doc),
    }

    if args.check:
        mismatches = 0
        for path, expected in outputs.items():
            if not path.is_file():
                print(f"[DRIFT] Missing generated file: {path}", file=sys.stderr)
                mismatches += 1
                continue
            actual = path.read_text(encoding="utf-8")
            if actual != expected:
                print(f"[DRIFT] Out-of-date generated file: {path}", file=sys.stderr)
                diff = difflib.unified_diff(
                    actual.splitlines(keepends=True),
                    expected.splitlines(keepends=True),
                    fromfile=str(path),
                    tofile=f"{path} (expected)",
                )
                sys.stderr.writelines(list(diff)[:40])
                mismatches += 1
        linux_cpp = repo_root / "linux" / "fcitx5-vitral" / "include" / "vitral_tokens.hpp"
        tsf_cpp = repo_root / "crates" / "vitral-tsf" / "include" / "vitral_tokens.hpp"
        if linux_cpp.is_file() and tsf_cpp.is_file():
            if linux_cpp.read_bytes() != tsf_cpp.read_bytes():
                print("[DRIFT] Linux and Windows C++ headers are not byte-identical!", file=sys.stderr)
                mismatches += 1
        if mismatches:
            return 1
        print(f"[OK] All {len(outputs)} generated token files match {tokens_path.name} (v{doc['brand']['version']}).")
        return 0

    for path, content in outputs.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
        print(f"[GEN] {path.relative_to(repo_root)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
