pub mod generated;

pub use generated::{RAW_DESIGN_TOKENS_JSON, TOKENS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgbaColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub alpha: f32,
    pub argb_u32: u32,
    pub rgba_u32: u32,
    pub css: &'static str,
}

impl RgbaColor {
    pub const fn to_f32_array(&self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.alpha,
        ]
    }

    pub const fn to_argb_u32(&self) -> u32 {
        self.argb_u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowSpec {
    pub offset_x_px: f32,
    pub offset_y_px: f32,
    pub blur_px: f32,
    pub color: RgbaColor,
    pub css: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemePalette {
    pub surface_panel: RgbaColor,
    pub surface_popover: RgbaColor,
    pub surface_key_primary: RgbaColor,
    pub surface_key_function: RgbaColor,
    pub surface_key_pressed_glow: RgbaColor,
    pub border_specular: RgbaColor,
    pub accent_primary: RgbaColor,
    pub accent_soft: RgbaColor,
    pub accent_text: RgbaColor,
    pub text_primary: RgbaColor,
    pub text_secondary: RgbaColor,
    pub shadow_elevation: ShadowSpec,
    pub ink_stroke_primary: RgbaColor,
    pub retroactive_fix_glow: RgbaColor,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionCurve {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub css: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrandSpec {
    pub name: &'static str,
    pub version: &'static str,
    pub material_style: &'static str,
    pub cloud_root: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlurSpec {
    pub backdrop_standard_px: u16,
    pub backdrop_floating_window_px: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadiusSpec {
    pub desktop_status_capsule_px: u16,
    pub desktop_candidate_window_px: u16,
    pub candidate_active_pill_px: u16,
    pub mobile_keyboard_sheet_px: u16,
    pub mobile_keycap_px: u16,
    pub popover_panel_px: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypographySpec {
    pub numeric_variant: &'static str,
    pub candidate_font_size_default_px: u16,
    pub candidate_font_size_range_px: [u16; 2],
    pub toolbar_icon_stroke_width_px: f32,
    pub toolbar_text_labels_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSpec {
    pub spring_curve: MotionCurve,
    pub duration_fast_ms: u32,
    pub duration_standard_ms: u32,
    pub desktop_idle_fade_delay_ms: u32,
    pub desktop_idle_opacity: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HandwritingSpec {
    pub modes: [&'static str; 2],
    pub stroke_width_px: f32,
    pub superimposed_ink_fade_out_ms: u32,
    pub retroactive_highlight_duration_ms: u32,
    pub confusion_top_k: u8,
    pub ambiguous_margin_threshold: f32,
    pub show_xiaohe_code_in_lookup_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingWindowSpec {
    pub free_drag: bool,
    pub free_resize: bool,
    pub min_width_dp: u16,
    pub min_height_dp: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HapticsSpec {
    pub letter_tick_ms: u16,
    pub action_thud_ms: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XiaoheKeycapSpec {
    pub rows: u8,
    pub top_left_sublabel: &'static str,
    pub bottom_right_sublabel: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuanpinKeycapSpec {
    pub rows: u8,
    pub dedicated_top_number_row: bool,
    pub top_right_sublabel: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaKeycapsSpec {
    pub double_pinyin_flypy: XiaoheKeycapSpec,
    pub rime_ice_quanpin: QuanpinKeycapSpec,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErgonomicsSpec {
    pub desktop_candidate_orientation: &'static str,
    pub desktop_candidate_page_size: u8,
    pub desktop_initial_toolbar_items: [&'static str; 2],
    pub mobile_docked_screen_ratio: f32,
    pub mobile_top_hierarchy: [&'static str; 3],
    pub mobile_floating_window: FloatingWindowSpec,
    pub key_feedback_mode: &'static str,
    pub spacebar_voice_hold_threshold_ms: u32,
    pub spacebar_voice_visual: &'static str,
    pub haptics: HapticsSpec,
    pub schema_keycaps: SchemaKeycapsSpec,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesignTokensStatic {
    pub schema: &'static str,
    pub brand: BrandSpec,
    pub dark: ThemePalette,
    pub light: ThemePalette,
    pub blur: BlurSpec,
    pub radius: RadiusSpec,
    pub typography: TypographySpec,
    pub motion: MotionSpec,
    pub handwriting: HandwritingSpec,
    pub ergonomics: ErgonomicsSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignTokensDocument {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub brand: BrandTokens,
    pub color: ColorThemes,
    pub blur: BlurTokens,
    pub radius: RadiusTokens,
    pub typography: TypographyTokens,
    pub motion: MotionTokens,
    pub handwriting: HandwritingTokens,
    pub ergonomics: ErgonomicsTokens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrandTokens {
    pub name: String,
    pub version: String,
    pub material_style: String,
    pub cloud_root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorThemes {
    pub dark: ThemeColorsRaw,
    pub light: ThemeColorsRaw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeColorsRaw {
    pub surface_panel: String,
    pub surface_popover: String,
    pub surface_key_primary: String,
    pub surface_key_function: String,
    pub surface_key_pressed_glow: String,
    pub border_specular: String,
    pub accent_primary: String,
    pub accent_soft: String,
    pub accent_text: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub shadow_elevation: String,
    pub ink_stroke_primary: String,
    pub retroactive_fix_glow: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlurTokens {
    pub backdrop_standard_px: u16,
    pub backdrop_floating_window_px: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusTokens {
    pub desktop_status_capsule_px: u16,
    pub desktop_candidate_window_px: u16,
    pub candidate_active_pill_px: u16,
    pub mobile_keyboard_sheet_px: u16,
    pub mobile_keycap_px: u16,
    pub popover_panel_px: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypographyTokens {
    pub numeric_variant: String,
    pub candidate_font_size_default_px: u16,
    pub candidate_font_size_range_px: [u16; 2],
    pub toolbar_icon_stroke_width_px: f32,
    pub toolbar_text_labels_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MotionTokens {
    pub spring_curve: String,
    pub duration_fast_ms: u32,
    pub duration_standard_ms: u32,
    pub desktop_idle_fade_delay_ms: u32,
    pub desktop_idle_opacity: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandwritingTokens {
    pub modes: Vec<String>,
    pub stroke_width_px: f32,
    pub superimposed_ink_fade_out_ms: u32,
    pub retroactive_highlight_duration_ms: u32,
    pub confusion_top_k: u8,
    pub ambiguous_margin_threshold: f32,
    pub show_xiaohe_code_in_lookup_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloatingWindowTokens {
    pub free_drag: bool,
    pub free_resize: bool,
    pub min_width_dp: u16,
    pub min_height_dp: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HapticsTokens {
    pub letter_tick_ms: u16,
    pub action_thud_ms: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XiaoheKeycapTokens {
    pub rows: u8,
    pub top_left_sublabel: String,
    pub bottom_right_sublabel: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuanpinKeycapTokens {
    pub rows: u8,
    pub dedicated_top_number_row: bool,
    pub top_right_sublabel: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaKeycapsTokens {
    pub double_pinyin_flypy: XiaoheKeycapTokens,
    pub rime_ice_quanpin: QuanpinKeycapTokens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErgonomicsTokens {
    pub desktop_candidate_orientation: String,
    pub desktop_candidate_page_size: u8,
    pub desktop_initial_toolbar_items: Vec<String>,
    pub mobile_docked_screen_ratio: f32,
    pub mobile_top_hierarchy: Vec<String>,
    pub mobile_floating_window: FloatingWindowTokens,
    pub key_feedback_mode: String,
    pub spacebar_voice_hold_threshold_ms: u32,
    pub spacebar_voice_visual: String,
    pub haptics: HapticsTokens,
    pub schema_keycaps: SchemaKeycapsTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenError {
    #[error("invalid color literal: {0}")]
    InvalidColor(String),
    #[error("invalid shadow literal: {0}")]
    InvalidShadow(String),
    #[error("invalid cubic-bezier literal: {0}")]
    InvalidCubicBezier(String),
    #[error("json parse error: {0}")]
    JsonParse(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenViolation {
    pub group: u8,
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub alpha: f32,
    pub argb_u32: u32,
    pub rgba_u32: u32,
}

impl ParsedColor {
    pub const fn to_f32_array(&self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.alpha,
        ]
    }

    pub const fn to_argb_u32(&self) -> u32 {
        self.argb_u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedShadow {
    pub offset_x_px: f32,
    pub offset_y_px: f32,
    pub blur_px: f32,
    pub color: ParsedColor,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

pub fn parse_color(value: &str) -> Result<ParsedColor, TokenError> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(TokenError::InvalidColor(value.to_string()));
        }
        let r = u8::from_str_radix(&hex[0..2], 16)
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        let g = u8::from_str_radix(&hex[2..4], 16)
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        let b = u8::from_str_radix(&hex[4..6], 16)
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        let a = 255u8;
        let argb_u32 =
            ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let rgba_u32 =
            ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32);
        return Ok(ParsedColor {
            r,
            g,
            b,
            a,
            alpha: 1.0,
            argb_u32,
            rgba_u32,
        });
    }

    if let Some(inner) = trimmed
        .strip_prefix("rgba(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 4 {
            return Err(TokenError::InvalidColor(value.to_string()));
        }
        let r: u8 = parts[0]
            .parse()
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        let g: u8 = parts[1]
            .parse()
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        let b: u8 = parts[2]
            .parse()
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        let alpha: f32 = parts[3]
            .parse()
            .map_err(|_| TokenError::InvalidColor(value.to_string()))?;
        if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
            return Err(TokenError::InvalidColor(value.to_string()));
        }
        let a = (alpha * 255.0).round() as u8;
        let argb_u32 =
            ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let rgba_u32 =
            ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32);
        return Ok(ParsedColor {
            r,
            g,
            b,
            a,
            alpha,
            argb_u32,
            rgba_u32,
        });
    }

    Err(TokenError::InvalidColor(value.to_string()))
}

fn parse_px_float(token: &str) -> Result<f32, TokenError> {
    let raw = token.strip_suffix("px").unwrap_or(token);
    let val: f32 = raw
        .parse()
        .map_err(|_| TokenError::InvalidShadow(token.to_string()))?;
    if !val.is_finite() {
        return Err(TokenError::InvalidShadow(token.to_string()));
    }
    Ok(val)
}

pub fn parse_shadow(value: &str) -> Result<ParsedShadow, TokenError> {
    let trimmed = value.trim();
    let mut parts = trimmed.splitn(4, char::is_whitespace);
    let ox_str = parts
        .next()
        .ok_or_else(|| TokenError::InvalidShadow(value.to_string()))?;
    let oy_str = parts
        .next()
        .ok_or_else(|| TokenError::InvalidShadow(value.to_string()))?;
    let blur_str = parts
        .next()
        .ok_or_else(|| TokenError::InvalidShadow(value.to_string()))?;
    let color_str = parts
        .next()
        .map(|s| s.trim())
        .ok_or_else(|| TokenError::InvalidShadow(value.to_string()))?;

    let offset_x_px = parse_px_float(ox_str)?;
    let offset_y_px = parse_px_float(oy_str)?;
    let blur_px = parse_px_float(blur_str)?;
    if blur_px < 0.0 {
        return Err(TokenError::InvalidShadow(value.to_string()));
    }
    let color = parse_color(color_str).map_err(|_| TokenError::InvalidShadow(value.to_string()))?;
    Ok(ParsedShadow {
        offset_x_px,
        offset_y_px,
        blur_px,
        color,
    })
}

pub fn parse_cubic_bezier(value: &str) -> Result<ParsedBezier, TokenError> {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix("cubic-bezier(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| TokenError::InvalidCubicBezier(value.to_string()))?;
    let nums: Vec<f32> = inner
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TokenError::InvalidCubicBezier(value.to_string()))?;
    if nums.len() != 4 || nums.iter().any(|v| !v.is_finite()) {
        return Err(TokenError::InvalidCubicBezier(value.to_string()));
    }
    Ok(ParsedBezier {
        x1: nums[0],
        y1: nums[1],
        x2: nums[2],
        y2: nums[3],
    })
}

fn approx_f32(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-4
}

pub fn load_tokens_from_str(json_str: &str) -> Result<DesignTokensDocument, TokenError> {
    serde_json::from_str(json_str).map_err(|e| TokenError::JsonParse(e.to_string()))
}

pub fn load_embedded_tokens() -> Result<DesignTokensDocument, TokenError> {
    load_tokens_from_str(RAW_DESIGN_TOKENS_JSON)
}

pub fn validate_tokens(doc: &DesignTokensDocument) -> Result<(), Vec<TokenViolation>> {
    let mut violations = Vec::new();
    let mut push = |group: u8, field: &str, message: &str| {
        violations.push(TokenViolation {
            group,
            field: field.to_string(),
            message: message.to_string(),
        });
    };

    // Group 1: Schema & Single-Source Integrity
    if doc.schema != "https://vitral.dev/schemas/design-tokens-v1.json" {
        push(1, "$schema", &format!("unexpected schema: {}", doc.schema));
    }
    if doc.brand.name != "Vitral" {
        push(1, "brand.name", &format!("must be 'Vitral', got '{}'", doc.brand.name));
    }
    if !doc.brand.name.is_ascii()
        || !doc.brand.version.is_ascii()
        || !doc.brand.material_style.is_ascii()
        || !doc.brand.cloud_root.is_ascii()
    {
        push(1, "brand", "brand strings must be strictly ASCII");
    }
    if doc.brand.version != "1.1.0" {
        push(1, "brand.version", &format!("must be '1.1.0', got '{}'", doc.brand.version));
    }
    if doc.brand.material_style != "frosted-glass" {
        push(1, "brand.materialStyle", "must be 'frosted-glass'");
    }
    if doc.brand.cloud_root != "/Koofr/Vitral" {
        push(1, "brand.cloudRoot", "must be '/Koofr/Vitral'");
    }

    // Group 2: Dark & Light Color Fidelity
    for (theme_name, raw) in [("dark", &doc.color.dark), ("light", &doc.color.light)] {
        if !raw.extra.is_empty() {
            push(2, &format!("color.{theme_name}"), "unexpected extra color keys");
        }
        let entries = [
            ("surfacePanel", &raw.surface_panel),
            ("surfacePopover", &raw.surface_popover),
            ("surfaceKeyPrimary", &raw.surface_key_primary),
            ("surfaceKeyFunction", &raw.surface_key_function),
            ("surfaceKeyPressedGlow", &raw.surface_key_pressed_glow),
            ("borderSpecular", &raw.border_specular),
            ("accentPrimary", &raw.accent_primary),
            ("accentSoft", &raw.accent_soft),
            ("accentText", &raw.accent_text),
            ("textPrimary", &raw.text_primary),
            ("textSecondary", &raw.text_secondary),
            ("inkStrokePrimary", &raw.ink_stroke_primary),
            ("retroactiveFixGlow", &raw.retroactive_fix_glow),
        ];
        for (k, val) in entries {
            if let Err(err) = parse_color(val) {
                push(2, &format!("color.{theme_name}.{k}"), &err.to_string());
            }
        }
        if let Err(err) = parse_shadow(&raw.shadow_elevation) {
            push(2, &format!("color.{theme_name}.shadowElevation"), &err.to_string());
        }
    }

    if let Ok(c) = parse_color(&doc.color.dark.surface_panel) {
        if !approx_f32(c.alpha, 0.68) {
            push(2, "color.dark.surfacePanel", "alpha must be 0.68");
        }
    }
    if let Ok(c) = parse_color(&doc.color.dark.surface_popover) {
        if !approx_f32(c.alpha, 0.74) {
            push(2, "color.dark.surfacePopover", "alpha must be 0.74");
        }
    }
    if !doc.color.dark.accent_primary.eq_ignore_ascii_case("#0ea5e9") {
        push(2, "color.dark.accentPrimary", "must be #0ea5e9");
    }
    if let Ok(c) = parse_color(&doc.color.light.surface_panel) {
        if !approx_f32(c.alpha, 0.74) {
            push(2, "color.light.surfacePanel", "alpha must be 0.74");
        }
    }
    if let Ok(c) = parse_color(&doc.color.light.surface_popover) {
        if !approx_f32(c.alpha, 0.80) {
            push(2, "color.light.surfacePopover", "alpha must be 0.80");
        }
    }
    if !doc.color.light.accent_primary.eq_ignore_ascii_case("#0284c7") {
        push(2, "color.light.accentPrimary", "must be #0284c7");
    }

    // Group 3: Blur & Elevation
    if doc.blur.backdrop_standard_px != 18 {
        push(3, "blur.backdropStandardPx", "must be 18");
    }
    if doc.blur.backdrop_floating_window_px != 20 {
        push(3, "blur.backdropFloatingWindowPx", "must be 20");
    }

    // Group 4: Corner Radii
    if doc.radius.desktop_status_capsule_px != 9999 {
        push(4, "radius.desktopStatusCapsulePx", "must be 9999");
    }
    if doc.radius.desktop_candidate_window_px != 16 {
        push(4, "radius.desktopCandidateWindowPx", "must be 16");
    }
    if doc.radius.candidate_active_pill_px != 10 {
        push(4, "radius.candidateActivePillPx", "must be 10");
    }
    if doc.radius.mobile_keyboard_sheet_px != 16 {
        push(4, "radius.mobileKeyboardSheetPx", "must be 16");
    }
    if doc.radius.mobile_keycap_px != 10 {
        push(4, "radius.mobileKeycapPx", "must be 10");
    }
    if doc.radius.popover_panel_px != 16 {
        push(4, "radius.popoverPanelPx", "must be 16");
    }

    // Group 5: Typography & Zero-Text-Label Toolbar
    if doc.typography.numeric_variant != "tabular-nums" {
        push(5, "typography.numericVariant", "must be 'tabular-nums'");
    }
    if doc.typography.candidate_font_size_default_px != 16 {
        push(5, "typography.candidateFontSizeDefaultPx", "must be 16");
    }
    if doc.typography.candidate_font_size_range_px != [13, 22] {
        push(5, "typography.candidateFontSizeRangePx", "must be [13, 22]");
    }
    if !approx_f32(doc.typography.toolbar_icon_stroke_width_px, 1.75) {
        push(5, "typography.toolbarIconStrokeWidthPx", "must be 1.75");
    }
    if doc.typography.toolbar_text_labels_allowed {
        push(5, "typography.toolbarTextLabelsAllowed", "must be false");
    }

    // Group 6: Motion & Idle Behavior
    if doc.motion.spring_curve != "cubic-bezier(0.22, 1, 0.36, 1)" {
        push(6, "motion.springCurve", "must be 'cubic-bezier(0.22, 1, 0.36, 1)'");
    }
    if let Err(err) = parse_cubic_bezier(&doc.motion.spring_curve) {
        push(6, "motion.springCurve", &err.to_string());
    }
    if doc.motion.duration_fast_ms != 150 {
        push(6, "motion.durationFastMs", "must be 150");
    }
    if doc.motion.duration_standard_ms != 180 {
        push(6, "motion.durationStandardMs", "must be 180");
    }
    if doc.motion.desktop_idle_fade_delay_ms != 5000 {
        push(6, "motion.desktopIdleFadeDelayMs", "must be 5000");
    }
    if !approx_f32(doc.motion.desktop_idle_opacity, 0.35) {
        push(6, "motion.desktopIdleOpacity", "must be 0.35");
    }

    // Group 7: Desktop Capsule & Candidate Ergonomics
    if doc.ergonomics.desktop_candidate_orientation != "horizontal" {
        push(7, "ergonomics.desktopCandidateOrientation", "must be 'horizontal'");
    }
    if doc.ergonomics.desktop_candidate_page_size != 5 {
        push(7, "ergonomics.desktopCandidatePageSize", "must be 5");
    }
    if doc.ergonomics.desktop_initial_toolbar_items != ["vitral-logo", "lang-toggle"] {
        push(7, "ergonomics.desktopInitialToolbarItems", "must be ['vitral-logo', 'lang-toggle']");
    }

    // Group 8: Handwriting Engine Visual Invariants
    if doc.handwriting.modes != ["phonetic-lookup-single", "continuous-contextual-lattice"] {
        push(8, "handwriting.modes", "must match frozen dual modes");
    }
    if !approx_f32(doc.handwriting.stroke_width_px, 3.5) {
        push(8, "handwriting.strokeWidthPx", "must be 3.5");
    }
    if doc.handwriting.superimposed_ink_fade_out_ms != 420 {
        push(8, "handwriting.superimposedInkFadeOutMs", "must be 420");
    }
    if doc.handwriting.retroactive_highlight_duration_ms != 600 {
        push(8, "handwriting.retroactiveHighlightDurationMs", "must be 600");
    }
    if doc.handwriting.confusion_top_k != 8 {
        push(8, "handwriting.confusionTopK", "must be 8");
    }
    if !approx_f32(doc.handwriting.ambiguous_margin_threshold, 0.15) {
        push(8, "handwriting.ambiguousMarginThreshold", "must be 0.15");
    }
    if !doc.handwriting.show_xiaohe_code_in_lookup_mode {
        push(8, "handwriting.showXiaoheCodeInLookupMode", "must be true");
    }

    // Group 9: Mobile Ergonomics & Keycap Schemas
    if !approx_f32(doc.ergonomics.mobile_docked_screen_ratio, 0.26) {
        push(9, "ergonomics.mobileDockedScreenRatio", "must be 0.26");
    }
    if doc.ergonomics.mobile_top_hierarchy != ["icon-toolbar", "candidate-bar", "key-grid"] {
        push(9, "ergonomics.mobileTopHierarchy", "must be ['icon-toolbar', 'candidate-bar', 'key-grid']");
    }
    if !doc.ergonomics.mobile_floating_window.free_drag {
        push(9, "ergonomics.mobileFloatingWindow.freeDrag", "must be true");
    }
    if !doc.ergonomics.mobile_floating_window.free_resize {
        push(9, "ergonomics.mobileFloatingWindow.freeResize", "must be true");
    }
    if doc.ergonomics.mobile_floating_window.min_width_dp != 220 {
        push(9, "ergonomics.mobileFloatingWindow.minWidthDp", "must be 220");
    }
    if doc.ergonomics.mobile_floating_window.min_height_dp != 165 {
        push(9, "ergonomics.mobileFloatingWindow.minHeightDp", "must be 165");
    }
    if doc.ergonomics.key_feedback_mode != "in-key-specular-glow" {
        push(9, "ergonomics.keyFeedbackMode", "must be 'in-key-specular-glow'");
    }
    if doc.ergonomics.spacebar_voice_hold_threshold_ms != 350 {
        push(9, "ergonomics.spacebarVoiceHoldThresholdMs", "must be 350");
    }
    if doc.ergonomics.spacebar_voice_visual != "in-place-waveform" {
        push(9, "ergonomics.spacebarVoiceVisual", "must be 'in-place-waveform'");
    }
    if doc.ergonomics.haptics.letter_tick_ms != 10 {
        push(9, "ergonomics.haptics.letterTickMs", "must be 10");
    }
    if doc.ergonomics.haptics.action_thud_ms != 16 {
        push(9, "ergonomics.haptics.actionThudMs", "must be 16");
    }
    if doc.ergonomics.schema_keycaps.double_pinyin_flypy.rows != 4 {
        push(9, "ergonomics.schemaKeycaps.double_pinyin_flypy.rows", "must be 4");
    }
    if doc.ergonomics.schema_keycaps.double_pinyin_flypy.top_left_sublabel
        != "swipe-up-number-or-symbol"
    {
        push(
            9,
            "ergonomics.schemaKeycaps.double_pinyin_flypy.topLeftSublabel",
            "must be 'swipe-up-number-or-symbol'",
        );
    }
    if doc.ergonomics.schema_keycaps.double_pinyin_flypy.bottom_right_sublabel != "xiaohe-final" {
        push(
            9,
            "ergonomics.schemaKeycaps.double_pinyin_flypy.bottomRightSublabel",
            "must be 'xiaohe-final'",
        );
    }
    if doc.ergonomics.schema_keycaps.rime_ice_quanpin.rows != 5 {
        push(9, "ergonomics.schemaKeycaps.rime_ice_quanpin.rows", "must be 5");
    }
    if !doc.ergonomics.schema_keycaps.rime_ice_quanpin.dedicated_top_number_row {
        push(
            9,
            "ergonomics.schemaKeycaps.rime_ice_quanpin.dedicatedTopNumberRow",
            "must be true",
        );
    }
    if doc.ergonomics.schema_keycaps.rime_ice_quanpin.top_right_sublabel != "swipe-up-symbol" {
        push(
            9,
            "ergonomics.schemaKeycaps.rime_ice_quanpin.topRightSublabel",
            "must be 'swipe-up-symbol'",
        );
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn check_palette_parity(
    theme_name: &str,
    raw: &ThemeColorsRaw,
    gen: &ThemePalette,
    violations: &mut Vec<TokenViolation>,
) {
    let pairs: [(&str, &str, RgbaColor); 13] = [
        ("surfacePanel", &raw.surface_panel, gen.surface_panel),
        ("surfacePopover", &raw.surface_popover, gen.surface_popover),
        ("surfaceKeyPrimary", &raw.surface_key_primary, gen.surface_key_primary),
        ("surfaceKeyFunction", &raw.surface_key_function, gen.surface_key_function),
        (
            "surfaceKeyPressedGlow",
            &raw.surface_key_pressed_glow,
            gen.surface_key_pressed_glow,
        ),
        ("borderSpecular", &raw.border_specular, gen.border_specular),
        ("accentPrimary", &raw.accent_primary, gen.accent_primary),
        ("accentSoft", &raw.accent_soft, gen.accent_soft),
        ("accentText", &raw.accent_text, gen.accent_text),
        ("textPrimary", &raw.text_primary, gen.text_primary),
        ("textSecondary", &raw.text_secondary, gen.text_secondary),
        ("inkStrokePrimary", &raw.ink_stroke_primary, gen.ink_stroke_primary),
        ("retroactiveFixGlow", &raw.retroactive_fix_glow, gen.retroactive_fix_glow),
    ];

    for (key, raw_str, gen_col) in pairs {
        match parse_color(raw_str) {
            Ok(parsed) => {
                if parsed.argb_u32 != gen_col.argb_u32 || raw_str != gen_col.css {
                    violations.push(TokenViolation {
                        group: 10,
                        field: format!("color.{theme_name}.{key}"),
                        message: "generated color constant does not match JSON".to_string(),
                    });
                }
            }
            Err(err) => violations.push(TokenViolation {
                group: 10,
                field: format!("color.{theme_name}.{key}"),
                message: err.to_string(),
            }),
        }
    }

    match parse_shadow(&raw.shadow_elevation) {
        Ok(sh) => {
            if !approx_f32(sh.offset_x_px, gen.shadow_elevation.offset_x_px)
                || !approx_f32(sh.offset_y_px, gen.shadow_elevation.offset_y_px)
                || !approx_f32(sh.blur_px, gen.shadow_elevation.blur_px)
                || sh.color.argb_u32 != gen.shadow_elevation.color.argb_u32
                || raw.shadow_elevation != gen.shadow_elevation.css
            {
                violations.push(TokenViolation {
                    group: 10,
                    field: format!("color.{theme_name}.shadowElevation"),
                    message: "generated shadow constant does not match JSON".to_string(),
                });
            }
        }
        Err(err) => violations.push(TokenViolation {
            group: 10,
            field: format!("color.{theme_name}.shadowElevation"),
            message: err.to_string(),
        }),
    }
}

pub fn validate_generated_parity() -> Result<(), Vec<TokenViolation>> {
    let doc = load_embedded_tokens().map_err(|e| {
        vec![TokenViolation {
            group: 1,
            field: "RAW_DESIGN_TOKENS_JSON".to_string(),
            message: e.to_string(),
        }]
    })?;
    validate_tokens(&doc)?;

    let mut violations = Vec::new();
    if doc.schema != TOKENS.schema
        || doc.brand.name != TOKENS.brand.name
        || doc.brand.version != TOKENS.brand.version
        || doc.brand.material_style != TOKENS.brand.material_style
        || doc.brand.cloud_root != TOKENS.brand.cloud_root
    {
        violations.push(TokenViolation {
            group: 10,
            field: "brand".to_string(),
            message: "generated brand parity mismatch".to_string(),
        });
    }

    check_palette_parity("dark", &doc.color.dark, &TOKENS.dark, &mut violations);
    check_palette_parity("light", &doc.color.light, &TOKENS.light, &mut violations);

    if doc.blur.backdrop_standard_px != TOKENS.blur.backdrop_standard_px
        || doc.blur.backdrop_floating_window_px != TOKENS.blur.backdrop_floating_window_px
        || doc.radius.desktop_status_capsule_px != TOKENS.radius.desktop_status_capsule_px
        || doc.radius.desktop_candidate_window_px != TOKENS.radius.desktop_candidate_window_px
        || doc.radius.candidate_active_pill_px != TOKENS.radius.candidate_active_pill_px
        || doc.radius.mobile_keyboard_sheet_px != TOKENS.radius.mobile_keyboard_sheet_px
        || doc.radius.mobile_keycap_px != TOKENS.radius.mobile_keycap_px
        || doc.radius.popover_panel_px != TOKENS.radius.popover_panel_px
        || doc.typography.candidate_font_size_default_px
            != TOKENS.typography.candidate_font_size_default_px
        || doc.typography.candidate_font_size_range_px
            != TOKENS.typography.candidate_font_size_range_px
        || !approx_f32(
            doc.typography.toolbar_icon_stroke_width_px,
            TOKENS.typography.toolbar_icon_stroke_width_px,
        )
        || doc.typography.toolbar_text_labels_allowed
            != TOKENS.typography.toolbar_text_labels_allowed
        || doc.motion.duration_fast_ms != TOKENS.motion.duration_fast_ms
        || doc.motion.duration_standard_ms != TOKENS.motion.duration_standard_ms
        || doc.motion.desktop_idle_fade_delay_ms != TOKENS.motion.desktop_idle_fade_delay_ms
        || !approx_f32(
            doc.motion.desktop_idle_opacity,
            TOKENS.motion.desktop_idle_opacity,
        )
        || !approx_f32(
            doc.handwriting.stroke_width_px,
            TOKENS.handwriting.stroke_width_px,
        )
        || doc.handwriting.superimposed_ink_fade_out_ms
            != TOKENS.handwriting.superimposed_ink_fade_out_ms
        || doc.handwriting.retroactive_highlight_duration_ms
            != TOKENS.handwriting.retroactive_highlight_duration_ms
        || doc.handwriting.confusion_top_k != TOKENS.handwriting.confusion_top_k
        || !approx_f32(
            doc.handwriting.ambiguous_margin_threshold,
            TOKENS.handwriting.ambiguous_margin_threshold,
        )
        || doc.ergonomics.desktop_candidate_page_size
            != TOKENS.ergonomics.desktop_candidate_page_size
        || doc.ergonomics.mobile_floating_window.min_width_dp
            != TOKENS.ergonomics.mobile_floating_window.min_width_dp
        || doc.ergonomics.mobile_floating_window.min_height_dp
            != TOKENS.ergonomics.mobile_floating_window.min_height_dp
    {
        violations.push(TokenViolation {
            group: 10,
            field: "static_tokens".to_string(),
            message: "generated scalar/array token parity mismatch".to_string(),
        });
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root")
    }

    #[test]
    fn valid_embedded_tokens_and_generated_parity() {
        let doc = load_embedded_tokens().expect("embedded design-tokens.json parses");
        validate_tokens(&doc).expect("all 10 invariant groups pass");
        validate_generated_parity().expect("generated Rust constants match JSON");

        assert_eq!(TOKENS.dark.surface_panel.to_argb_u32(), 0xAD16_1A26);
        assert_eq!(TOKENS.dark.accent_primary.to_argb_u32(), 0xFF0E_A5E9);
        let arr = TOKENS.dark.accent_primary.to_f32_array();
        assert!((arr[0] - 14.0 / 255.0).abs() < 1e-5);
        assert!((arr[3] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn color_shadow_and_bezier_parsers_handle_valid_and_invalid_inputs() {
        let hex = parse_color("#0ea5e9").unwrap();
        assert_eq!((hex.r, hex.g, hex.b, hex.a), (14, 165, 233, 255));
        let rgba = parse_color("rgba(22, 26, 38, 0.68)").unwrap();
        assert_eq!((rgba.r, rgba.g, rgba.b, rgba.a), (22, 26, 38, 173));
        assert!(parse_color("rgb(0,0,0)").is_err());
        assert!(parse_color("rgba(22, 26, 38, 1.5)").is_err());

        let sh = parse_shadow("0 16px 36px rgba(0, 0, 0, 0.48)").unwrap();
        assert_eq!((sh.offset_x_px, sh.offset_y_px, sh.blur_px), (0.0, 16.0, 36.0));
        assert!(parse_shadow("0 16px -4px rgba(0,0,0,0.4)").is_err());

        let bz = parse_cubic_bezier("cubic-bezier(0.22, 1, 0.36, 1)").unwrap();
        assert!((bz.x1 - 0.22).abs() < 1e-6);
        assert!(parse_cubic_bezier("ease-in-out").is_err());
    }

    #[test]
    fn rejects_prototype_drift_and_invariant_violations() {
        let base = load_embedded_tokens().unwrap();

        let mut d = base.clone();
        d.typography.toolbar_text_labels_allowed = true;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.brand.name = "PureIME".to_string();
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.brand.name = "Vitral输入法".to_string();
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.radius.mobile_keycap_px = 12;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.radius.desktop_candidate_window_px = 12;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.motion.desktop_idle_opacity = 0.32;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.handwriting.superimposed_ink_fade_out_ms = 400;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.handwriting.retroactive_highlight_duration_ms = 900;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.handwriting.stroke_width_px = 3.2;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.ergonomics.mobile_floating_window.min_width_dp = 200;
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.typography.candidate_font_size_range_px = [13, 19];
        assert!(validate_tokens(&d).is_err());

        let mut d = base.clone();
        d.color.dark.surface_panel = "rgba(24, 29, 42, 0.72)".to_string();
        assert!(validate_tokens(&d).is_err());
    }

    #[test]
    fn cross_target_generated_files_match_design_tokens() {
        let root = repo_root();
        let linux_hpp =
            std::fs::read_to_string(root.join("linux/fcitx5-vitral/include/vitral_tokens.hpp"))
                .expect("linux vitral_tokens.hpp");
        let win_hpp =
            std::fs::read_to_string(root.join("crates/vitral-tsf/include/vitral_tokens.hpp"))
                .expect("windows vitral_tokens.hpp");
        assert_eq!(linux_hpp, win_hpp, "C++ headers must be byte-identical");
        assert!(linux_hpp.contains("0xAD161A26u"));
        assert!(linux_hpp.contains("kToolbarTextLabelsAllowed = false"));
        assert!(linux_hpp.contains("kSuperimposedInkFadeOutMs = 420"));
        assert!(linux_hpp.contains("kRetroactiveHighlightDurationMs = 600"));
        assert!(linux_hpp.contains("kDesktopIdleOpacity = 0.35f"));

        let kt = std::fs::read_to_string(
            root.join("apps/android/app/src/main/java/dev/vitral/ime/theme/VitralTokens.kt"),
        )
        .expect("VitralTokens.kt");
        assert!(kt.contains("const val name: String = \"Vitral\""));
        assert!(kt.contains("const val surfacePanelArgb: Long = 0xAD161A26L"));
        assert!(kt.contains("const val toolbarTextLabelsAllowed: Boolean = false"));
        assert!(kt.contains("const val mobileKeycapPx: Int = 10"));
        assert!(kt.contains("const val floatingMinWidthDp: Int = 220"));
        assert!(kt.contains("const val floatingMinHeightDp: Int = 165"));

        let css = std::fs::read_to_string(
            root.join("apps/desktop-tauri/src/generated/tokens.css"),
        )
        .expect("tokens.css");
        assert!(css.contains("--vitral-radius-status-capsule: 9999px;"));
        assert!(css.contains("--vitral-radius-candidate-window: 16px;"));
        assert!(css.contains("--vitral-idle-opacity: 0.35;"));
        assert!(css.contains("--vitral-hw-fade-out: 420ms;"));
        assert!(css.contains("--vitral-hw-retroactive-glow: 600ms;"));

        let ts = std::fs::read_to_string(
            root.join("apps/desktop-tauri/src/generated/tokens.ts"),
        )
        .expect("tokens.ts");
        assert!(ts.contains("\"name\": \"Vitral\""));
        assert!(ts.contains("\"toolbarTextLabelsAllowed\": false"));
    }
}
