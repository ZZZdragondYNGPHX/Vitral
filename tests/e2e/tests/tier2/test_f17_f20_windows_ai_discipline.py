"""Tier 2 BVA and negative/corner-case tests for F17-F20 (Windows TSF/Tauri, AI/Restore, Research/Zero-Bloat)."""

from __future__ import annotations

import json
import re
import struct
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

from harnesses import AIMockServer, WebDAVMockServer
from vitral_e2e.adapter import VitralCommandError, VitralTarget
from vitral_e2e.discovery import DiscoveredTargets

pytestmark = pytest.mark.tier2


def _parse_pe_headers(data: bytes) -> tuple[int, int, int, list[tuple[int, int, int]], int, int]:
    assert data[:2] == b"MZ", "Missing MZ DOS header"
    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    assert data[pe_off : pe_off + 4] == b"PE\x00\x00", "Missing PE signature"
    machine, num_sections, _, _, _, opt_hdr_size, characteristics = struct.unpack_from(
        "<HHIIIHH", data, pe_off + 4
    )
    opt_off = pe_off + 24
    opt_magic = struct.unpack_from("<H", data, opt_off)[0]
    sec_off = opt_off + opt_hdr_size
    sections: list[tuple[int, int, int]] = []
    for i in range(num_sections):
        s = sec_off + i * 40
        vsize, va, raw_size, raw_ptr = struct.unpack_from("<IIII", data, s + 8)
        sections.append((va, max(vsize, raw_size), raw_ptr))
    return machine, characteristics, opt_magic, sections, opt_off, pe_off


def _rva_to_off(rva: int, sections: list[tuple[int, int, int]]) -> int | None:
    for va, span, raw_ptr in sections:
        if va <= rva < va + span:
            return raw_ptr + (rva - va)
    return None


def _read_cstr(data: bytes, off: int) -> str:
    end = data.index(b"\x00", off)
    return data[off:end].decode("ascii", errors="replace")


def _pe_exports(data: bytes) -> list[str]:
    _, _, opt_magic, sections, opt_off, _ = _parse_pe_headers(data)
    assert opt_magic == 0x20B, f"Expected PE32+ (0x20b), got {hex(opt_magic)}"
    export_rva, export_size = struct.unpack_from("<II", data, opt_off + 112)
    if export_rva == 0 or export_size == 0:
        return []
    exp_off = _rva_to_off(export_rva, sections)
    assert exp_off is not None
    num_names = struct.unpack_from("<I", data, exp_off + 24)[0]
    addr_of_names_rva = struct.unpack_from("<I", data, exp_off + 32)[0]
    names_off = _rva_to_off(addr_of_names_rva, sections)
    assert names_off is not None
    out: list[str] = []
    for i in range(num_names):
        name_rva = struct.unpack_from("<I", data, names_off + i * 4)[0]
        name_off = _rva_to_off(name_rva, sections)
        if name_off is not None:
            out.append(_read_cstr(data, name_off))
    return out


def _pe_imports(data: bytes) -> list[str]:
    _, _, opt_magic, sections, opt_off, _ = _parse_pe_headers(data)
    assert opt_magic == 0x20B
    import_rva, import_size = struct.unpack_from("<II", data, opt_off + 120)
    if import_rva == 0 or import_size == 0:
        return []
    imp_off = _rva_to_off(import_rva, sections)
    assert imp_off is not None
    out: list[str] = []
    while True:
        _, _, _, name_rva, first_thunk = struct.unpack_from("<IIIII", data, imp_off)
        if name_rva == 0 and first_thunk == 0:
            break
        name_off = _rva_to_off(name_rva, sections)
        if name_off is not None:
            out.append(_read_cstr(data, name_off))
        imp_off += 20
    return out


# ======================================================================
# F17: Windows TSF COM DLL & DirectComposition/Direct2D Acrylic Host (5 tests)
# ======================================================================


@pytest.mark.requires_windows
def test_f17_windows_tsf_dll_pe32plus_format_and_com_exports(vitral_target: VitralTarget) -> None:
    dll_path = vitral_target.require_windows_tsf()
    data = dll_path.read_bytes()
    machine, characteristics, opt_magic, _, _, _ = _parse_pe_headers(data)
    assert machine == 0x8664, f"Expected x86_64 COFF machine (0x8664), got {hex(machine)}"
    assert opt_magic == 0x20B, f"Expected PE32+ optional magic (0x20b), got {hex(opt_magic)}"
    assert characteristics & 0x2000, f"IMAGE_FILE_DLL flag missing: {hex(characteristics)}"

    exports = set(_pe_exports(data))
    for sym in ("DllGetClassObject", "DllCanUnloadNow", "DllRegisterServer", "DllUnregisterServer"):
        assert sym in exports, f"Required COM export {sym!r} missing from {dll_path} exports={sorted(exports)}"


@pytest.mark.requires_windows
def test_f17_windows_tsf_no_linux_so_linkage(vitral_target: VitralTarget) -> None:
    dll_path = vitral_target.require_windows_tsf()
    imports = _pe_imports(dll_path.read_bytes())
    assert len(imports) >= 1
    for dll_name in imports:
        lower = dll_name.lower()
        assert ".so" not in lower, f"Linux shared object leaked into Windows PE imports: {dll_name}"
        assert lower.endswith(".dll"), f"Non-DLL import in PE table: {dll_name}"


@pytest.mark.requires_windows
@pytest.mark.tokens
def test_f17_windows_dcomp_token_radii_16px_not_12px(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_windows()
    tsf_dir = discovered_targets.vitral_root / "crates" / "vitral-tsf"
    hpp = (tsf_dir / "include" / "vitral_tokens.hpp").read_text(encoding="utf-8")

    assert f"kDesktopCandidateWindowPx = {design_tokens['radius']['desktopCandidateWindowPx']};" in hpp
    assert f"kPopoverPanelPx = {design_tokens['radius']['popoverPanelPx']};" in hpp
    assert f"kDesktopStatusCapsulePx = {design_tokens['radius']['desktopStatusCapsulePx']};" in hpp
    assert f"kBackdropStandardPx = {design_tokens['blur']['backdropStandardPx']};" in hpp
    assert "kDesktopIdleOpacity = 0.35f;" in hpp
    assert "kDesktopIdleFadeDelayMs = 5000;" in hpp
    assert "kRetroactiveHighlightDurationMs = 600;" in hpp


@pytest.mark.requires_windows
def test_f17_windows_tsf_composition_override_and_quick_paste(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_windows()
    tsf_src = discovered_targets.vitral_root / "crates" / "vitral-tsf" / "src"
    rs_text = "\n".join(p.read_text(encoding="utf-8", errors="replace") for p in tsf_src.rglob("*.rs"))
    assert "ITfTextInputProcessor" in rs_text
    assert "ITfComposition" in rs_text
    assert "live_slot.enc" not in rs_text


@pytest.mark.requires_windows
def test_f17_windows_tsf_english_only_vitral_brand(vitral_target: VitralTarget) -> None:
    dll_path = vitral_target.require_windows_tsf()
    data = dll_path.read_bytes()
    assert b"Vitral" in data
    assert b"PureIME" not in data and b"pure-ime" not in data.lower()
    assert "Vitral输入法".encode("utf-8") not in data
    assert "Vitral输入法".encode("utf-16le") not in data


# ======================================================================
# F18: Shared Tauri v2 Management Center Windows Build (5 tests)
# ======================================================================


@pytest.mark.requires_windows
def test_f18_windows_tauri_exe_pe32plus_format(vitral_target: VitralTarget) -> None:
    _, exe_path = vitral_target.require_windows()
    data = exe_path.read_bytes()
    machine, characteristics, opt_magic, _, _, _ = _parse_pe_headers(data)
    assert machine == 0x8664, f"Expected x86_64 Windows exe, got {hex(machine)}"
    assert opt_magic == 0x20B, f"Expected PE32+ optional magic, got {hex(opt_magic)}"
    assert not (characteristics & 0x2000), "Windows Tauri .exe must not have IMAGE_FILE_DLL flag"


@pytest.mark.requires_windows
def test_f18_windows_tauri_english_only_vitral_metadata(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    _, exe_path = vitral_target.require_windows()
    data = exe_path.read_bytes()
    assert b"Vitral" in data
    assert b"PureIME" not in data
    assert "Vitral输入法".encode("utf-8") not in data

    tauri_conf = discovered_targets.vitral_root / "apps" / "desktop-tauri" / "src-tauri" / "tauri.conf.json"
    if tauri_conf.is_file():
        conf_text = tauri_conf.read_text(encoding="utf-8")
        assert "PureIME" not in conf_text
        assert "Vitral" in conf_text


@pytest.mark.requires_windows
@pytest.mark.tokens
def test_f18_windows_tauri_shared_ui_token_and_slider_bounds(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_windows()
    tokens_ts = (
        discovered_targets.vitral_root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.ts"
    ).read_text(encoding="utf-8")
    assert '"candidateFontSizeDefaultPx": 16' in tokens_ts
    assert '"candidateFontSizeRangePx": [\n      13,\n      22\n    ]' in tokens_ts


@pytest.mark.requires_windows
def test_f18_windows_tauri_no_linux_runtime_imports(vitral_target: VitralTarget) -> None:
    _, exe_path = vitral_target.require_windows()
    imports = _pe_imports(exe_path.read_bytes())
    assert len(imports) >= 1
    for imp in imports:
        lower = imp.lower()
        assert ".so" not in lower
        assert "webkit2gtk" not in lower and "libgtk" not in lower


@pytest.mark.requires_windows
def test_f18_windows_tauri_3tier_and_offline_feature_parity(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    vitral_target.require_windows()
    tauri_dir = discovered_targets.vitral_root / "apps" / "desktop-tauri"
    files = list(tauri_dir.rglob("*.ts")) + list(tauri_dir.rglob("*.tsx")) + list(tauri_dir.rglob("*.rs"))
    combined = "\n".join(p.read_text(encoding="utf-8", errors="replace") for p in files)
    assert "live_slot.enc" not in combined
    assert "pure_offline" in combined or "pureOffline" in combined


# ======================================================================
# F19: BYOK / Ollama AI Pipeline, P2 Recorder & Full Restore (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f19_ai_tier3_slot_disambiguation_bounds_and_lock(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    ai_server: AIMockServer,
) -> None:
    assert discovered_targets.fixtures_dir is not None
    hw_dir = discovered_targets.fixtures_dir / "handwriting"
    scel_path = discovered_targets.fixtures_dir / "lexicons" / "sample_hotwords.scel"
    fx_ambig = json.loads((hw_dir / "mode2_baoguang_laodeng.json").read_text(encoding="utf-8"))
    fx_locked = json.loads((hw_dir / "mode2_clear_locked.json").read_text(encoding="utf-8"))

    ai_server.set_slot_choice(99)  # out-of-bounds slot index returned by BYOK endpoint
    ai_server.clear_request_log()
    vitral_target.init({
        "ai_endpoint": ai_server.openai_url,
        "ai_mock_url": ai_server.base_url,
        "openai_url": ai_server.openai_url,
        "ollama_url": ai_server.ollama_url,
        "enable_tier3_ai": True,
        "ai_tier3_enabled": True,
    })
    vitral_target.import_lexicon(scel_path, "scel")

    vitral_target.hw_begin("ContinuousLattice")
    for stroke in fx_ambig["strokes"]:
        pts = [{"x": float(p["x"]), "y": float(p["y"]), "timestamp_ms": int(p["t"])} for p in stroke]
        vitral_target.hw_push_stroke(pts, pen_up=True)
    res = vitral_target.hw_end()

    assert len(ai_server.request_log) >= 1, "Expected Tier-3 BYOK endpoint to be invoked during Mode 2 hw_end"
    slots = res["slots"]
    assert len(slots) >= 6
    top8_chars = [item["char"] if isinstance(item, dict) else str(item) for item in slots[0]["top8"]]
    assert 1 <= len(top8_chars) <= 8
    assert slots[0]["char"] in top8_chars, (
        f"OOB BYOK slot choice 99 corrupted slot 0 char {slots[0]['char']!r} not in {top8_chars}"
    )
    assert res["composing_text"] in ("爆光老登金币", "曝光老登金币")

    # Locked high-confidence slot must remain untouched even when BYOK returns an alternative choice
    vitral_target.hw_clear()
    ai_server.set_slot_choice(1)
    vitral_target.hw_begin("ContinuousLattice")
    for stroke in fx_locked["strokes"]:
        pts = [{"x": float(p["x"]), "y": float(p["y"]), "timestamp_ms": int(p["t"])} for p in stroke]
        vitral_target.hw_push_stroke(pts, pen_up=True)
    locked_res = vitral_target.hw_end()
    assert locked_res["slots"][0]["locked_by_entropy"] is True
    assert locked_res["slots"][0]["char"] == "曝"
    assert locked_res["composing_text"] == "曝光老登金币"


@pytest.mark.requires_core
def test_f19_ai_mock_http_500_429_and_timeout_resilience(
    vitral_target: VitralTarget,
    ai_server: AIMockServer,
) -> None:
    vitral_target.init({"ai_endpoint": ai_server.ollama_url, "ai_timeout_ms": 500})

    for status in (500, 429):
        ai_server.inject_error(status_code=status, message=f"Fault {status}", once=True)
        with pytest.raises(VitralCommandError):
            vitral_target.ai_inline("translate", "测试容错", "en")

    ai_server.inject_delay(2.0)
    with pytest.raises(VitralCommandError):
        vitral_target.ai_inline("polish", "测试超时", "zh")


@pytest.mark.requires_core
def test_f19_ai_empty_and_pure_offline_block(
    vitral_target: VitralTarget,
    ai_server: AIMockServer,
) -> None:
    vitral_target.init({"ai_endpoint": ai_server.ollama_url})
    with pytest.raises(VitralCommandError):
        vitral_target.ai_inline("translate", "", "en")

    vitral_target.set_pure_offline(True)
    ai_server.clear_request_log()
    with pytest.raises(VitralCommandError):
        vitral_target.ai_inline("translate", "离线拦截测试", "en")
    assert len(ai_server.request_log) == 0


@pytest.mark.requires_core
@pytest.mark.tokens
def test_f19_p2_recorder_inside_pure_icon_library_only(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
    design_tokens: dict[str, Any],
) -> None:
    vitral_target.require_tauri()
    assert design_tokens["typography"]["toolbarTextLabelsAllowed"] is False
    assert design_tokens["ergonomics"]["desktopInitialToolbarItems"] == ["vitral-logo", "lang-toggle"]
    assert float(design_tokens["typography"]["toolbarIconStrokeWidthPx"]) == pytest.approx(1.75)

    root = discovered_targets.vitral_root
    tauri_src = root / "apps" / "desktop-tauri" / "src"
    tauri_ui_files = [
        p
        for p in tauri_src.rglob("*")
        if p.is_file()
        and p.suffix.lower() in (".ts", ".tsx", ".js", ".jsx", ".html")
        and "generated" not in p.parts
    ]
    assert len(tauri_ui_files) >= 1, f"Expected non-generated Tauri UI sources under {tauri_src}"

    surface_files = list(tauri_ui_files)
    for extra_dir, exts in (
        (root / "linux" / "fcitx5-vitral" / "src", (".cpp", ".cc", ".h", ".hpp")),
        (root / "apps" / "android" / "app" / "src", (".kt", ".java")),
    ):
        if extra_dir.is_dir():
            surface_files.extend(
                p
                for p in extra_dir.rglob("*")
                if p.is_file() and p.suffix.lower() in exts and p.name != "VitralTokens.kt"
            )

    combined = "\n".join(p.read_text(encoding="utf-8", errors="replace") for p in surface_files)
    lower = combined.lower()
    assert "ask-ai" not in lower and "问 ai" not in lower and "问AI" not in combined
    assert "showlabel: true" not in lower and "toolbartextlabelsallowed: true" not in lower
    assert re.search(
        r"(?:p2[-_]?recorder|p2[-_]?voice|voice[-_]?recorder|ai[-_]?recorder|micro[-_]?capsule|microphone|\brecorder\b|\bmic\b|mic[-_]?icon|micIcon|voiceRecorder|p2Recorder)",
        combined,
        re.IGNORECASE,
    ), "Expected P2 AI voice recorder component in UI/toolbar sources"
    assert "toolbar" in lower or "icon" in lower


@pytest.mark.requires_core
def test_f19_koofr_restore_empty_and_corrupt_payload_abort(
    vitral_target: VitralTarget,
    webdav_server: WebDAVMockServer,
) -> None:
    vitral_target.init({
        "koofr_endpoint": webdav_server.koofr_endpoint,
        "webdav_url": webdav_server.koofr_endpoint,
    })
    # Empty WebDAV mock -> restore must abort cleanly without wiping local state
    with pytest.raises(VitralCommandError):
        vitral_target.restore_from_koofr({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "webdav_url": webdav_server.koofr_endpoint,
        })

    # Tampered/corrupt encrypted backup -> must fail integrity/parse check
    webdav_server.write_file_bytes("profile/config.json.enc", b"corrupt-non-ciphertext")
    webdav_server.write_file_bytes("lexicon/snapshot.zst.enc", b"corrupt-snapshot")
    with pytest.raises(VitralCommandError):
        vitral_target.restore_from_koofr({
            "koofr_endpoint": webdav_server.koofr_endpoint,
            "webdav_url": webdav_server.koofr_endpoint,
        })

    webdav_server.assert_no_live_slot()


# ======================================================================
# F20: Research-Driven Engineering Notes & Zero-Bloat Verification (5 tests)
# ======================================================================


@pytest.mark.requires_core
def test_f20_research_docs_exist_and_cover_core_topics(discovered_targets: DiscoveredTargets) -> None:
    research_dir = discovered_targets.vitral_root / "docs" / "research"
    assert research_dir.is_dir(), f"Missing docs/research directory: {research_dir}"
    docs = sorted(research_dir.glob("*.md"))
    assert len(docs) >= 2, f"Expected M0 + M1+ research docs under {research_dir}, found {[d.name for d in docs]}"
    combined = "\n".join(d.read_text(encoding="utf-8", errors="replace").lower() for d in docs)
    for topic in ("quanpin", "handwriting", "frecency", "webdav"):
        assert topic in combined, f"Research notes missing required domain topic {topic!r}"


_RAW_STR_START_RE = re.compile(r'(?:br|cr|r)(#*)"')
_CFG_TEST_ATTR_RE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def _advance_rust_non_code(text: str, cur: int, n: int) -> int | None:
    """If text[cur:] starts a Rust comment, string, raw string, or char/byte-char literal, return end index."""
    ch = text[cur]
    nxt = text[cur + 1] if cur + 1 < n else ""
    if ch == "/" and nxt == "/":
        cur += 2
        while cur < n and text[cur] != "\n":
            cur += 1
        return cur + 1 if cur < n else n
    if ch == "/" and nxt == "*":
        depth = 1
        cur += 2
        while cur < n and depth > 0:
            c0 = text[cur]
            c1 = text[cur + 1] if cur + 1 < n else ""
            if c0 == "/" and c1 == "*":
                depth += 1
                cur += 2
            elif c0 == "*" and c1 == "/":
                depth -= 1
                cur += 2
            else:
                cur += 1
        return cur
    prev = text[cur - 1] if cur > 0 else ""
    if ch in ("r", "b", "c") and not (prev.isalnum() or prev == "_"):
        rm = _RAW_STR_START_RE.match(text, cur)
        if rm is not None:
            raw_hashes = len(rm.group(1))
            end_hashes = "#" * raw_hashes
            cur = rm.end()
            while cur < n:
                if text[cur] == '"' and text.startswith(end_hashes, cur + 1):
                    return cur + 1 + raw_hashes
                cur += 1
            return n
        if ch == "b" and nxt == "'":
            cur += 1
            ch = "'"
            nxt = text[cur + 1] if cur + 1 < n else ""
    if ch == "'":
        if nxt == "\\":
            if cur + 2 >= n:
                return n
            cur += 3
            while cur < n and text[cur] not in ("'", "\n"):
                cur += 1
            return cur + 1 if cur < n and text[cur] == "'" else cur
        if cur + 2 < n and text[cur + 2] == "'" and nxt != "'":
            return cur + 3
        return cur + 1
    if ch == '"':
        cur += 1
        while cur < n:
            c = text[cur]
            if c == "\\":
                cur += 2
            elif c == '"':
                return cur + 1
            else:
                cur += 1
        return n
    return None


def _strip_rust_cfg_test_blocks(text: str) -> str:
    """Remove all #[cfg(test)] annotated items/modules from Rust source text."""
    out: list[str] = []
    pos = 0
    chunk_start = 0
    n = len(text)
    while pos < n:
        skipped = _advance_rust_non_code(text, pos, n)
        if skipped is not None:
            pos = skipped
            continue
        if text[pos] == "#":
            m = _CFG_TEST_ATTR_RE.match(text, pos)
            if m is not None:
                out.append(text[chunk_start:pos])
                cur = m.end()
                depth = 0
                paren_depth = 0
                bracket_depth = 0
                saw_brace = False
                while cur < n:
                    inner_skipped = _advance_rust_non_code(text, cur, n)
                    if inner_skipped is not None:
                        cur = inner_skipped
                        continue
                    ch = text[cur]
                    if ch == "{":
                        depth += 1
                        saw_brace = True
                    elif ch == "}":
                        if saw_brace:
                            depth -= 1
                            if depth == 0:
                                cur += 1
                                break
                    elif not saw_brace:
                        if ch == "(":
                            paren_depth += 1
                        elif ch == ")" and paren_depth > 0:
                            paren_depth -= 1
                        elif ch == "[":
                            bracket_depth += 1
                        elif ch == "]" and bracket_depth > 0:
                            bracket_depth -= 1
                        elif ch == ";" and paren_depth == 0 and bracket_depth == 0:
                            cur += 1
                            break
                    cur += 1
                pos = cur
                chunk_start = cur
                continue
        pos += 1
    out.append(text[chunk_start:])
    return "".join(out)


def _prod_text_for_scan(path: Path) -> str:
    text = path.read_text(encoding="utf-8", errors="replace")
    if path.suffix.lower() == ".rs":
        text = _strip_rust_cfg_test_blocks(text)
    return text


def test_f20_zero_bloat_forbidden_features_absent(discovered_targets: DiscoveredTargets) -> None:
    root = discovered_targets.vitral_root
    forbidden_bloat = ("斗图", "书影音", "花体字", "问 AI", "ask-ai")
    scan_roots = [root / "apps", root / "linux", root / "crates"]
    exts = {".rs", ".ts", ".tsx", ".js", ".jsx", ".css", ".html", ".kt", ".java", ".cpp", ".hpp", ".h"}

    for base in scan_roots:
        if not base.is_dir():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix.lower() not in exts:
                continue
            if any(part in ("target", "node_modules", "build", "tests") for part in path.parts):
                continue
            text = _prod_text_for_scan(path)
            for needle in forbidden_bloat:
                assert needle not in text, f"Forbidden bloat string {needle!r} found in {path}"


def test_f20_brand_vitral_only_no_pureime_or_cn_alias(discovered_targets: DiscoveredTargets) -> None:
    root = discovered_targets.vitral_root
    scan_roots = [root / "apps", root / "linux", root / "crates", root / "tokens"]
    exts = {".rs", ".ts", ".tsx", ".js", ".jsx", ".css", ".html", ".kt", ".java", ".cpp", ".hpp", ".h", ".json"}

    for base in scan_roots:
        if not base.is_dir():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix.lower() not in exts:
                continue
            if any(part in ("target", "node_modules", "build", "tests") for part in path.parts):
                continue
            text = _prod_text_for_scan(path)
            lower = text.lower()
            assert "pureime" not in lower, f"Legacy brand 'PureIME' found in {path}"
            assert "pure-ime" not in lower, f"Legacy brand 'pure-ime' found in {path}"
            assert not re.search(r"Vitral\s*[\u4e00-\u9fff]*输入法", text), (
                f"Chinese brand alias attached to Vitral in {path}"
            )


@pytest.mark.tokens
def test_f20_single_source_of_truth_token_drift_zero(discovered_targets: DiscoveredTargets) -> None:
    root = discovered_targets.vitral_root
    gen_script = discovered_targets.gen_tokens_script
    assert gen_script is not None and gen_script.is_file()

    proc = subprocess.run(
        [sys.executable, str(gen_script), "--check", "--root", str(root)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, f"Token codegen drift detected:\n{proc.stderr}\n{proc.stdout}"

    generated_paths = [
        root / "crates" / "vitral-tokens" / "src" / "generated.rs",
        root / "linux" / "fcitx5-vitral" / "include" / "vitral_tokens.hpp",
        root / "crates" / "vitral-tsf" / "include" / "vitral_tokens.hpp",
        root / "apps" / "android" / "app" / "src" / "main" / "java" / "dev" / "vitral" / "ime" / "theme" / "VitralTokens.kt",
        root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.css",
        root / "apps" / "desktop-tauri" / "src" / "generated" / "tokens.ts",
    ]
    draft_literals = ("900ms", "0.32f", "0.32;", "230x175")
    for gp in generated_paths:
        assert gp.is_file(), f"Missing generated token file: {gp}"
        txt = gp.read_text(encoding="utf-8")
        for bad in draft_literals:
            assert bad not in txt, f"Superseded draft literal {bad!r} drifted into {gp}"


@pytest.mark.requires_core
def test_f20_test_all_and_ci_runner_no_silent_skip(
    vitral_target: VitralTarget,
    discovered_targets: DiscoveredTargets,
) -> None:
    test_all = vitral_target.require_test_all_script()
    assert test_all.is_file()

    pytest_ini = (discovered_targets.vitral_root / "tests" / "e2e" / "pytest.ini").read_text(encoding="utf-8")
    assert "--strict-markers" in pytest_ini

    tier2_dir = discovered_targets.vitral_root / "tests" / "e2e" / "tests" / "tier2"
    forbidden_skip = "pytest." + "skip"
    for py_file in sorted(tier2_dir.glob("*.py")):
        src = py_file.read_text(encoding="utf-8")
        assert forbidden_skip not in src, f"Silent skip call found in {py_file}"
