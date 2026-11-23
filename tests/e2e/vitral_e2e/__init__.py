"""Opaque-box E2E test adapter, target discovery, and design-token helpers for Vitral."""

from .adapter import VitralCommandError, VitralTarget
from .discovery import DiscoveredTargets, discover_targets
from .tokens import (
    DEFAULT_TOKENS_PATH,
    assert_all_token_invariants,
    assert_blur_invariants,
    assert_brand_invariants,
    assert_dark_palette_invariants,
    assert_english_only_brand,
    assert_ergonomics_invariants,
    assert_handwriting_invariants,
    assert_light_palette_invariants,
    assert_motion_invariants,
    assert_no_toolbar_text_labels,
    assert_prototype_reconciled_values,
    assert_radius_invariants,
    assert_schema_keycaps_invariants,
    assert_typography_invariants,
    load_design_tokens,
)

__all__ = [
    "DEFAULT_TOKENS_PATH",
    "DiscoveredTargets",
    "VitralCommandError",
    "VitralTarget",
    "assert_all_token_invariants",
    "assert_blur_invariants",
    "assert_brand_invariants",
    "assert_dark_palette_invariants",
    "assert_english_only_brand",
    "assert_ergonomics_invariants",
    "assert_handwriting_invariants",
    "assert_light_palette_invariants",
    "assert_motion_invariants",
    "assert_no_toolbar_text_labels",
    "assert_prototype_reconciled_values",
    "assert_radius_invariants",
    "assert_schema_keycaps_invariants",
    "assert_typography_invariants",
    "discover_targets",
    "load_design_tokens",
]
