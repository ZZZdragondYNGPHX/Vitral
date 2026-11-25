"""Tier 1 Windows Platform, BYOK AI/Restore, and Engineering Discipline tests (F17-F20)."""

from __future__ import annotations

import json
import os
import struct
import wave
from pathlib import Path
from typing import Any

import pytest

from harnesses import AIMockServer, WebDAVMockServer
from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets
from vitral_e2e.tokens import assert_english_only_brand


def _parse_pe32_plus(pe_path: Path) -> dict[str, Any]:
    data = pe_path.read_bytes()
    assert len(data) >= 0x100, f"File too small to be valid PE: {pe_path}"
    assert data[:2] == b"MZ", f"Missing MZ DOS header in {pe_path}"
    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    assert data[pe_off : pe_off + 4] == b"PE\x00\x00", f"Missing PE signature in {pe_path}"

    machine, num_sections = struct.unpack_from("<HH", data, pe_off + 4)
    opt_hdr_size = struct.unpack_from("<H", data, pe_off + 20)[0]
    magic = struct.unpack_from("<H", data, pe_off + 24)[0]

    sec_off = pe_off + 24 + opt_hdr_size
    sections: list[tuple[str, int, int, int, int]] = []
    for i in range(num_sections):
        sec = data[sec_off + i * 40 : sec_off + (i + 1) * 40]
        name = sec[:8].rstrip(b"\x00").decode("ascii", errors="replace")
        vsize, vaddr, rsize, rptr = struct.unpack_from("<IIII", sec, 8)
        sections.append((name, vaddr, vsize, rptr, rsize))

    def rva_to_off(rva: int) -> int | None:
        for _, va, vs, rp, rs in sections:
            if va <= rva < va + max(vs, rs):
                return rp + (rva - va)
        return None

    exports: list[str] = []
    exp_rva, _exp_size = struct.unpack_from("<II", data, pe_off + 24 + 112)
    if exp_rva:
        eo = rva_to_off(exp_rva)
        if eo is not None:
            num_names = struct.unpack_from("<I", data, eo + 24)[0]
            names_rva = struct.unpack_from("<I", data, eo + 32)[0]
            no = rva_to_off(names_rva)
            if no is not None:
                for i in range(num_names):
                    nrva = struct.unpack_from("<I", data, no + 4 * i)[0]
                    off = rva_to_off(nrva)
                    if off is not None:
                        end = data.index(b"\x00", off)
                        exports.append(data[off:end].decode("ascii", errors="replace"))

    return {
        "machine": machine,
        "magic": magic,
        "sections": [s[0] for s in sections],
        "exports": exports,
    }


def _read_dir_text(base: Path, exts: tuple[str, ...]) -> str:
    if not base.is_dir():
        return ""
    chunks: list[str] = []
    for p in sorted(base.rglob("*")):
        if "target" in p.parts or "node_modules" in p.parts:
            continue
        if p.is_file() and p.suffix in exts:
            chunks.append(p.read_text(encoding="utf-8", errors="replace"))
    return "\n".join(chunks)


# ---------------------------------------------------------------------------
# F17: Windows TSF COM DLL & DirectComposition/Direct2D Acrylic Host (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_windows
def test_f17_windows_tsf_dll_valid_pe32_plus_x86_64(vitral_target: VitralTarget) -> None:
    dll_path = vitral_target.require_windows_tsf()
    pe = _parse_pe32_plus(dll_path)
    assert pe["machine"] == 0x8664, f"Expected AMD64 (0x8664), got {hex(pe['machine'])}"
    assert pe["magic"] == 0x020B, f"Expected PE32+ (0x020B), got {hex(pe['magic'])}"
    assert ".text" in pe["sections"]
    assert ".rdata" in pe["sections"]


@pytest.mark.tier1
@pytest.mark.requires_windows
def test_f17_windows_tsf_dll_exports_com_entrypoints(vitral_target: VitralTarget) -> None:
    dll_path = vitral_target.require_windows_tsf()
    pe = _parse_pe32_plus(dll_path)
    exported = set(pe["exports"])
    for sym in (
        "DllGetClassObject",
        "DllCanUnloadNow",
        "DllRegisterServer",
        "DllUnregisterServer",
    ):
        assert sym in exported, f"vitral_tsf.dll missing COM export {sym!r}"


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_windows
def test_f17_windows_tsf_implements_itf_composition_and_600ms_glow(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_windows_tsf()
    assert design_tokens["handwriting"]["retroactiveHighlightDurationMs"] == 600
    tsf_dir = vitral_target.targets.vitral_root / "crates" / "vitral-tsf" / "src"
    src_text = _read_dir_text(tsf_dir, (".rs", ".cpp", ".hpp", ".h"))

    assert "ITfTextInputProcessor" in src_text, "M4 TSF ITfTextInputProcessor not implemented yet"
    assert "ITfComposition" in src_text, "M4 TSF ITfComposition not implemented yet"
    assert "retroactive_highlight_duration_ms" in src_text or "kRetroactiveHighlightDurationMs" in src_text


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_windows
def test_f17_windows_tsf_dcomp_d2d_uses_canonical_tokens(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_windows_tsf()
    assert design_tokens["radius"]["desktopCandidateWindowPx"] == 16
    assert design_tokens["radius"]["desktopStatusCapsulePx"] == 9999
    assert design_tokens["motion"]["desktopIdleOpacity"] == 0.35
    assert design_tokens["typography"]["toolbarTextLabelsAllowed"] is False

    tsf_root = vitral_target.targets.vitral_root / "crates" / "vitral-tsf"
    src_text = _read_dir_text(tsf_root / "src", (".rs", ".cpp", ".hpp", ".h"))
    assert "DirectComposition" in src_text or "IDComposition" in src_text, (
        "M4 DirectComposition acrylic renderer not implemented yet"
    )
    assert "ID2D1" in src_text or "Direct2D" in src_text


@pytest.mark.tier1
@pytest.mark.requires_windows
def test_f17_windows_tsf_mode1_popover_and_quick_paste_candidate(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_windows_tsf()
    tsf_src = vitral_target.targets.vitral_root / "crates" / "vitral-tsf" / "src"
    src_text = _read_dir_text(tsf_src, (".rs", ".cpp", ".hpp", ".h"))
    assert all(command in src_text for command in ("candidate-state", "select-candidate")), (
        "Candidate integration missing in vitral-tsf"
    )
    assert "quick_paste_id" not in src_text, "Desktop candidate bar must not display Quick Paste"
    assert "on_ime_open" in src_text and "on_ime_close" in src_text


# ---------------------------------------------------------------------------
# F18: Shared Tauri v2 Management Center Windows Build (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_windows
def test_f18_windows_tauri_exe_valid_pe32_plus_x86_64(vitral_target: VitralTarget) -> None:
    _tsf_dll, exe_path = vitral_target.require_windows()
    pe = _parse_pe32_plus(exe_path)
    assert pe["machine"] == 0x8664
    assert pe["magic"] == 0x020B


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_windows
def test_f18_windows_tauri_shares_tokens_css_ts_and_v1_1_values(
    vitral_target: VitralTarget,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_windows()
    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    css = (tauri_dir / "src" / "generated" / "tokens.css").read_text(encoding="utf-8")
    ts = (tauri_dir / "src" / "generated" / "tokens.ts").read_text(encoding="utf-8")
    assert f"--vitral-idle-opacity: {design_tokens['motion']['desktopIdleOpacity']};" in css
    assert '"name": "Vitral"' in ts


@pytest.mark.tier1
@pytest.mark.tokens
@pytest.mark.requires_windows
def test_f18_windows_tauri_english_vitral_branding_invariant(
    vitral_target: VitralTarget,
) -> None:
    vitral_target.require_windows()
    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    conf = (tauri_dir / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8")
    assert_english_only_brand(conf)


@pytest.mark.tier1
@pytest.mark.requires_windows
def test_f18_windows_tauri_cargo_xwin_config_parity(vitral_target: VitralTarget) -> None:
    vitral_target.require_windows()
    cargo_toml = (
        vitral_target.targets.vitral_root / "apps" / "desktop-tauri" / "src-tauri" / "Cargo.toml"
    )
    assert cargo_toml.is_file()
    text = cargo_toml.read_text(encoding="utf-8")
    assert "vitral-core" in text and "vitral-tokens" in text


@pytest.mark.tier1
@pytest.mark.requires_windows
def test_f18_windows_tauri_exposes_settings_parity_surface(vitral_target: VitralTarget) -> None:
    vitral_target.require_windows()
    tauri_dir = vitral_target.targets.vitral_root / "apps" / "desktop-tauri"
    ui_text = _read_dir_text(tauri_dir / "src", (".ts", ".tsx", ".js", ".jsx", ".html")).lower()
    for kw in ("koofr", "offline", "restore", "lexicon"):
        assert kw in ui_text


# ---------------------------------------------------------------------------
# F19: BYOK / Ollama AI Pipeline, P2 Recorder & Full Device Restore (6 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f19_byok_ollama_inline_translate_while_typing(
    vitral_target: VitralTarget,
    ai_server: AIMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init({"ai_endpoint": ai_server.ollama_url, "ai_model": "qwen2.5:7b"})

    res = vitral_target.ai_inline(
        "translate",
        "这款输入法主打极简晶透毛玻璃体验，绝不堆砌臃肿功能。",
        "en",
    )
    assert "output_text" in res and isinstance(res["output_text"], str), (
        f"Missing canonical 'output_text' in AiInlineResult: {res}"
    )
    out_text = res["output_text"]
    assert "frosted-glass" in out_text.lower(), f"Expected 'frosted-glass' translation, got {res}"
    assert len(ai_server.request_log) >= 1


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f19_byok_ollama_spoken_to_written_polish(
    vitral_target: VitralTarget,
    ai_server: AIMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init({"ai_endpoint": ai_server.openai_url, "ai_model": "qwen2.5:7b"})

    res = vitral_target.ai_inline("polish", "那个就是说今天下午三点然后开会啊")
    assert "output_text" in res and isinstance(res["output_text"], str), (
        f"Missing canonical 'output_text' in AiInlineResult: {res}"
    )
    out_text = res["output_text"]
    assert out_text == "今天下午三点开会。"
    assert len(ai_server.request_log) >= 1


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f19_tier3_byok_constrained_slot_disambiguation(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    ai_server: AIMockServer,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None

    fx = json.loads(
        (discovered_targets.fixtures_dir / "handwriting" / "mode2_baoguang_laodeng.json").read_text(
            encoding="utf-8"
        )
    )
    ai_server.set_slot_choice(1)
    vitral_target.init({"ai_endpoint": ai_server.openai_url, "ai_tier3_enabled": True})
    ai_server.clear_request_log()
    vitral_target.hw_begin("ContinuousLattice")
    for stroke in fx["strokes"]:
        pts = [
            {
                "x": float(p["x"]),
                "y": float(p["y"]),
                "timestamp_ms": int(p.get("timestamp_ms", p.get("t", 0))),
            }
            for p in stroke
        ]
        vitral_target.hw_push_stroke(pts, pen_up=True)
    res = vitral_target.hw_end()
    assert res["composing_text"] == "爆光老登金币"
    assert len(ai_server.request_log) >= 1


@pytest.mark.tier1
@pytest.mark.requires_core
@pytest.mark.tokens
def test_f19_p2_ai_voice_recorder_summarize_and_icon_only_mount(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    ai_server: AIMockServer,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_core()
    assert design_tokens["typography"]["toolbarTextLabelsAllowed"] is False
    assert discovered_targets.fixtures_dir is not None

    wav_path = discovered_targets.fixtures_dir / "audio" / "long_20s_16k.wav"
    with wave.open(str(wav_path), "rb") as w:
        pcm = w.readframes(w.getnframes())

    vitral_target.init({"ai_endpoint": ai_server.ollama_url, "ai_model": "qwen2.5:7b"})
    vitral_target.voice_start()
    vitral_target.voice_push_pcm(pcm)
    voice_res = vitral_target.voice_stop()
    assert "transcript" in voice_res and isinstance(voice_res["transcript"], str), (
        f"Missing canonical 'transcript' in VoiceResult: {voice_res}"
    )
    transcript = voice_res["transcript"].strip()
    assert transcript

    ai_server.clear_request_log()
    sum_res = vitral_target.ai_inline("summary", transcript)
    assert "output_text" in sum_res and isinstance(sum_res["output_text"], str), (
        f"Missing canonical 'output_text' in AiInlineResult: {sum_res}"
    )
    summary_text = sum_res["output_text"]
    assert "1." in summary_text and "2." in summary_text
    assert len(ai_server.request_log) >= 1


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f19_one_click_restore_from_koofr_recovers_full_state(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    webdav_server: WebDAVMockServer,
    tmp_path: Path,
) -> None:
    vitral_target.require_core()
    assert discovered_targets.fixtures_dir is not None
    tsv_path = discovered_targets.fixtures_dir / "lexicons" / "sample_pairs.tsv"

    dev_a = vitral_target
    dev_a.init({"koofr_endpoint": webdav_server.koofr_endpoint, "device_id": "dev-a"})
    dev_a.import_lexicon(tsv_path, "tsv")
    pushed = dev_a.clipboard_push("restore-pinned-value")
    assert "ring" in pushed and isinstance(pushed["ring"], list) and pushed["ring"], (
        f"Expected ClipboardState with non-empty 'ring', got {pushed}"
    )
    entry = next((e for e in pushed["ring"] if e.get("content") == "restore-pinned-value"), pushed["ring"][0])
    entry_id = str(entry["id"])
    dev_a.clipboard_pin(entry_id, "rst")

    dev_b = VitralTarget(targets=discovered_targets, state_dir=tmp_path / "dev_b_restore")
    try:
        dev_b.init({"device_id": "dev-b"})
        restore_res = dev_b.restore_from_koofr({"koofr_endpoint": webdav_server.koofr_endpoint})
        assert "restored" in restore_res and restore_res["restored"] is True, (
            f"Expected RestoreResult.restored == True, got {restore_res}"
        )

        cb_b = dev_b.clipboard_state()
        assert "ring" in cb_b and isinstance(cb_b["ring"], list), f"Missing 'ring' in {cb_b}"
        ring_texts = [e["content"] for e in cb_b["ring"]]
        assert "restore-pinned-value" in ring_texts
        webdav_server.assert_no_live_slot()
    finally:
        dev_b.close()


@pytest.mark.tier1
@pytest.mark.requires_core
def test_f19_pure_offline_blocks_ai_inline_even_with_valid_endpoint(
    vitral_target: VitralTarget,
    ai_server: AIMockServer,
) -> None:
    vitral_target.require_core()
    vitral_target.init({"ai_endpoint": ai_server.ollama_url})
    vitral_target.set_pure_offline(True)
    ai_server.clear_request_log()

    with pytest.raises(VitralCommandError, match=r"(?i)pure.?offline"):
        vitral_target.ai_inline("translate", "测试纯离线拦截", "en")

    assert ai_server.request_log == []


# ---------------------------------------------------------------------------
# F20: Research-Driven Engineering Notes & Zero-Bloat Verification (5 tests)
# ---------------------------------------------------------------------------


@pytest.mark.tier1
def test_f20_research_notes_exist_under_docs_research(
    discovered_targets: DiscoveredTargets,
) -> None:
    research_dir = discovered_targets.vitral_root / "docs" / "research"
    assert research_dir.is_dir(), "Missing docs/research/"
    m0_doc = research_dir / "m0_toolchains_tokens_assets.md"
    assert m0_doc.is_file(), "Missing M0 research doc: docs/research/m0_toolchains_tokens_assets.md"
    content = m0_doc.read_text(encoding="utf-8")
    assert len(content.splitlines()) >= 50
    assert "PROPOSAL.md" in content
    assert "design-tokens.json" in content


@pytest.mark.tier1
@pytest.mark.tokens
def test_f20_zero_bloat_invariant_no_forbidden_features_or_text_toolbars(
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    assert design_tokens["typography"]["toolbarTextLabelsAllowed"] is False
    root = discovered_targets.vitral_root
    proposal = (root / "docs" / "PROPOSAL.md").read_text(encoding="utf-8")
    assert "Zero Bloat" in proposal

    # Verify zero bloat modules/flags in source & generated UI trees
    for scan_dir in ("crates", "apps", "linux", "tokens"):
        base = root / scan_dir
        if not base.is_dir():
            continue
        for p in base.rglob("*"):
            if "node_modules" in p.parts or "target" in p.parts or ".git" in p.parts:
                continue
            if not p.is_file() or p.suffix not in (".rs", ".ts", ".tsx", ".css", ".json", ".kt", ".hpp", ".cpp", ".toml"):
                continue
            text = p.read_text(encoding="utf-8", errors="replace")
            for forbidden_bloat in ("meme_pack", "sticker_pack", "dou_tu", "book_movie_card", "chat_ai_companion"):
                assert forbidden_bloat not in text.lower(), f"Bloat symbol {forbidden_bloat!r} in {p}"


@pytest.mark.tier1
@pytest.mark.tokens
def test_f20_strict_english_vitral_brand_no_chinese_alias_or_pureime(
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    brand_name = design_tokens["brand"]["name"]
    assert brand_name == "Vitral"
    assert brand_name.isascii()

    root = discovered_targets.vitral_root
    must_brand_files = {
        "apps/desktop-tauri/src/generated/tokens.ts",
        "crates/vitral-tokens/src/generated.rs",
        "apps/android/app/src/main/java/dev/vitral/ime/theme/VitralTokens.kt",
        "linux/fcitx5-vitral/include/vitral_tokens.hpp",
        "README.md",
        "docs/PROPOSAL.md",
    }
    for rel in (
        "Cargo.toml",
        "package.json",
        "apps/desktop-tauri/package.json",
        *sorted(must_brand_files),
    ):
        text = (root / rel).read_text(encoding="utf-8")
        assert_english_only_brand(text, require_present=(rel in must_brand_files))


@pytest.mark.tier1
def test_f20_scripts_test_all_sh_exists_and_executable(vitral_target: VitralTarget) -> None:
    script = vitral_target.require_test_all_script()
    assert script.is_file()
    assert os.access(script, os.X_OK), f"{script} must be executable"
    content = script.read_text(encoding="utf-8")
    assert "cargo" in content or "pytest" in content


@pytest.mark.tier1
def test_f20_no_agent_metadata_polluting_vitral_repo(
    discovered_targets: DiscoveredTargets,
) -> None:
    root = discovered_targets.vitral_root
    assert not (root / ".agents" / "teamwork").exists(), ".agents/teamwork/ must not live in Vitral repo"
    assert not (root / "GEMINI.md").exists(), "GEMINI.md must not exist in Vitral repo"

    manifest_path = root / "tests" / "fixtures" / "manifest.json"
    assert manifest_path.is_file()
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    assert manifest["total_files"] == 19
    for entry in manifest["files"]:
        assert (root / "tests" / "fixtures" / entry["path"]).is_file()
