import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { VITRAL_TOKENS } from "../src/generated/tokens.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../../..");

const rawJson = JSON.parse(
  fs.readFileSync(path.join(repoRoot, "tokens/design-tokens.json"), "utf8"),
);
const cssText = fs.readFileSync(
  path.join(repoRoot, "apps/desktop-tauri/src/generated/tokens.css"),
  "utf8",
);

// 1. Exact deep equality between generated tokens.ts and tokens/design-tokens.json
assert.deepEqual(VITRAL_TOKENS, rawJson, "VITRAL_TOKENS must match design-tokens.json");

// 2. Enforce canonical reconciled invariants
assert.equal(VITRAL_TOKENS.brand.name, "Vitral");
assert.equal(VITRAL_TOKENS.typography.toolbarTextLabelsAllowed, false);
assert.equal(VITRAL_TOKENS.radius.mobileKeycapPx, 10);
assert.equal(VITRAL_TOKENS.radius.desktopCandidateWindowPx, 16);
assert.equal(VITRAL_TOKENS.radius.popoverPanelPx, 16);
assert.equal(VITRAL_TOKENS.motion.desktopIdleOpacity, 0.35);
assert.equal(VITRAL_TOKENS.handwriting.strokeWidthPx, 3.5);
assert.equal(VITRAL_TOKENS.handwriting.superimposedInkFadeOutMs, 420);
assert.equal(VITRAL_TOKENS.handwriting.retroactiveHighlightDurationMs, 600);
assert.equal(VITRAL_TOKENS.ergonomics.mobileFloatingWindow.minWidthDp, 220);
assert.equal(VITRAL_TOKENS.ergonomics.mobileFloatingWindow.minHeightDp, 165);
assert.equal(VITRAL_TOKENS.typography.candidateFontSizeDefaultPx, 16);
assert.deepEqual(VITRAL_TOKENS.typography.candidateFontSizeRangePx, [13, 22]);

// 3. Parse CSS custom properties from tokens.css and verify exact match
function parseCssBlocks(css) {
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const blocks = {};
  const blockRe = /([^{}]+)\{([^}]*)\}/g;
  let m;
  while ((m = blockRe.exec(stripped)) !== null) {
    const selector = m[1].trim().replace(/\s+/g, " ");
    const vars = {};
    for (const line of m[2].split(";")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      const idx = trimmed.indexOf(":");
      if (idx === -1) continue;
      const k = trimmed.slice(0, idx).trim();
      const v = trimmed.slice(idx + 1).trim();
      vars[k] = v;
    }
    blocks[selector] = vars;
  }
  return blocks;
}

const blocks = parseCssBlocks(cssText);
const rootVars = blocks[":root"];
const darkVars = blocks[':root, [data-theme="dark"]'];
const lightVars = blocks['[data-theme="light"]'];

assert.ok(rootVars, "tokens.css must define :root");
assert.ok(darkVars, "tokens.css must define :root, [data-theme=\"dark\"]");
assert.ok(lightVars, "tokens.css must define [data-theme=\"light\"]");

assert.equal(rootVars["--vitral-blur-standard"], `${rawJson.blur.backdropStandardPx}px`);
assert.equal(rootVars["--vitral-blur-floating"], `${rawJson.blur.backdropFloatingWindowPx}px`);
assert.equal(rootVars["--vitral-radius-status-capsule"], `${rawJson.radius.desktopStatusCapsulePx}px`);
assert.equal(rootVars["--vitral-radius-candidate-window"], `${rawJson.radius.desktopCandidateWindowPx}px`);
assert.equal(rootVars["--vitral-radius-candidate-pill"], `${rawJson.radius.candidateActivePillPx}px`);
assert.equal(rootVars["--vitral-radius-mobile-keycap"], `${rawJson.radius.mobileKeycapPx}px`);
assert.equal(rootVars["--vitral-radius-popover"], `${rawJson.radius.popoverPanelPx}px`);
assert.equal(rootVars["--vitral-idle-opacity"], `${rawJson.motion.desktopIdleOpacity}`);
assert.equal(rootVars["--vitral-hw-stroke-width"], `${rawJson.handwriting.strokeWidthPx}px`);
assert.equal(rootVars["--vitral-hw-fade-out"], `${rawJson.handwriting.superimposedInkFadeOutMs}ms`);
assert.equal(rootVars["--vitral-hw-retroactive-glow"], `${rawJson.handwriting.retroactiveHighlightDurationMs}ms`);

const colorMap = {
  surfacePanel: "--vitral-surface-panel",
  surfacePopover: "--vitral-surface-popover",
  surfaceKeyPrimary: "--vitral-surface-key-primary",
  surfaceKeyFunction: "--vitral-surface-key-function",
  surfaceKeyPressedGlow: "--vitral-surface-key-pressed-glow",
  borderSpecular: "--vitral-border-specular",
  accentPrimary: "--vitral-accent-primary",
  accentSoft: "--vitral-accent-soft",
  accentText: "--vitral-accent-text",
  textPrimary: "--vitral-text-primary",
  textSecondary: "--vitral-text-secondary",
  shadowElevation: "--vitral-shadow-elevation",
  inkStrokePrimary: "--vitral-ink-stroke-primary",
  retroactiveFixGlow: "--vitral-retroactive-fix-glow",
};

for (const [jsonKey, cssVar] of Object.entries(colorMap)) {
  assert.equal(darkVars[cssVar], rawJson.color.dark[jsonKey], `dark ${cssVar}`);
  assert.equal(lightVars[cssVar], rawJson.color.light[jsonKey], `light ${cssVar}`);
}

console.log("[OK] @vitral/desktop-tauri tokens.ts & tokens.css match design-tokens.json v1.1.0");
