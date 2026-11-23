"""Deterministic integrity tests for Vitral E2E-M2 fixtures.

Uses only Python stdlib + pytest so it runs cleanly in minimal environments.
"""

from __future__ import annotations

import hashlib
import json
import math
import re
import struct
import wave
from pathlib import Path

import pytest

pytestmark = pytest.mark.fixtures

FIXTURES_DIR = Path(__file__).resolve().parents[2] / "fixtures"
SCEL_MAGIC = bytes.fromhex("401500004443530101000000")


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def _load_json(rel_path: str) -> dict:
    return json.loads((FIXTURES_DIR / rel_path).read_text(encoding="utf-8"))


def test_top_level_manifest_completeness_and_checksums() -> None:
    manifest_path = FIXTURES_DIR / "manifest.json"
    assert manifest_path.is_file(), "tests/fixtures/manifest.json must exist"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

    entries = manifest["files"]
    assert manifest["total_files"] == len(entries)
    assert len(entries) >= 19

    listed_paths = [e["path"] for e in entries]
    assert len(listed_paths) == len(set(listed_paths)), "Duplicate paths in manifest.json"

    actual_paths = sorted(
        p.relative_to(FIXTURES_DIR).as_posix()
        for p in FIXTURES_DIR.rglob("*")
        if p.is_file() and p.relative_to(FIXTURES_DIR).as_posix() != "manifest.json"
    )
    assert set(listed_paths) == set(actual_paths), (
        f"Manifest mismatch: extra={set(listed_paths) - set(actual_paths)}, "
        f"unlisted={set(actual_paths) - set(listed_paths)}"
    )

    for entry in entries:
        fp = FIXTURES_DIR / entry["path"]
        assert fp.is_file()
        assert fp.stat().st_size == entry["size_bytes"], f"Size mismatch for {entry['path']}"
        assert _sha256(fp) == entry["sha256"], f"SHA-256 mismatch for {entry['path']}"


def test_quanpin_corpus_progressive_structure() -> None:
    corpus = _load_json("quanpin/corpus.json")
    cases = corpus["cases"]
    assert corpus["total_cases"] == len(cases) >= 15

    bucket_counts = {"short": 0, "medium": 0, "long": 0}
    for case in cases:
        bucket = case["length_bucket"]
        bucket_counts[bucket] += 1
        assert "".join(case["syllable_segments"]) == case["pinyin"]
        assert len(case["expected_text"]) == len(case["syllable_segments"]) == case["char_count"]

        if bucket == "short":
            assert 2 <= case["char_count"] <= 3
        elif bucket == "medium":
            assert 5 <= case["char_count"] <= 8
        elif bucket == "long":
            assert case["char_count"] >= 10

        prog = case["progression"]
        assert len(prog) >= 2
        prev_len = 0
        for step in prog:
            prefix = step["prefix"]
            assert case["pinyin"].startswith(prefix)
            assert len(prefix) > prev_len
            prev_len = len(prefix)
        assert prog[-1]["prefix"] == case["pinyin"]
        assert prog[-1]["resolved"] == case["expected_text"]

    assert bucket_counts["short"] >= 5
    assert bucket_counts["medium"] >= 5
    assert bucket_counts["long"] >= 5


def _parse_scel_binary(data: bytes) -> dict:
    assert len(data) > 0x2628
    assert data[:12] == SCEL_MAGIC
    group_cnt, word_cnt = struct.unpack_from("<II", data, 0x120)

    def decode_slot(start: int, end: int) -> str:
        raw = data[start:end]
        text = raw.decode("utf-16le").rstrip("\x00")
        return text

    name = decode_slot(0x130, 0x338)
    lex_type = decode_slot(0x338, 0x540)
    desc = decode_slot(0x540, 0xD40)
    examples = decode_slot(0xD40, 0x1540)

    py_count = struct.unpack_from("<I", data, 0x1540)[0]
    assert py_count == 413
    pos = 0x1544
    py_table: dict[int, str] = {}
    for _ in range(py_count):
        idx, byte_len = struct.unpack_from("<HH", data, pos)
        pos += 4
        sy = data[pos : pos + byte_len].decode("utf-16le")
        pos += byte_len
        py_table[idx] = sy
    assert pos == 0x2628, f"Pinyin table must end at 0x2628, got {hex(pos)}"

    decoded_words = []
    parsed_groups = 0
    while pos < len(data):
        same_py_count, py_idx_bytes_len = struct.unpack_from("<HH", data, pos)
        pos += 4
        assert py_idx_bytes_len % 2 == 0 and py_idx_bytes_len > 0
        py_indices = struct.unpack_from(f"<{py_idx_bytes_len // 2}H", data, pos)
        pos += py_idx_bytes_len
        syllables = [py_table[i] for i in py_indices]
        for _ in range(same_py_count):
            word_bytes_len = struct.unpack_from("<H", data, pos)[0]
            pos += 2
            word = data[pos : pos + word_bytes_len].decode("utf-16le")
            pos += word_bytes_len
            ext_bytes_len = struct.unpack_from("<H", data, pos)[0]
            pos += 2
            ext_bytes = data[pos : pos + ext_bytes_len]
            pos += ext_bytes_len
            assert ext_bytes_len == 10
            freq = struct.unpack_from("<H", ext_bytes, 0)[0]
            decoded_words.append((word, " ".join(syllables), freq))
        parsed_groups += 1

    assert pos == len(data)
    assert parsed_groups == group_cnt
    assert len(decoded_words) == word_cnt
    return {
        "name": name,
        "type": lex_type,
        "description": desc,
        "examples": examples,
        "words": decoded_words,
    }


def test_lexicon_scel_valid_and_corrupt() -> None:
    scel_bytes = (FIXTURES_DIR / "lexicons/sample_hotwords.scel").read_bytes()
    parsed = _parse_scel_binary(scel_bytes)
    assert "Vitral" in parsed["name"]
    words_map = {w: (py, freq) for w, py, freq in parsed["words"]}
    assert len(parsed["words"]) >= 10
    for expected in ["爆光", "曝光", "老登", "爆金币", "老登金币", "晶透", "毛玻璃", "雾面玻璃", "小鹤双拼", "全拼解码"]:
        assert expected in words_map, f"Missing {expected} from sample_hotwords.scel"
        assert words_map[expected][1] > 0

    corrupt_bytes = (FIXTURES_DIR / "lexicons/corrupt.scel").read_bytes()
    assert len(corrupt_bytes) < 0x1540
    assert corrupt_bytes[:12] != SCEL_MAGIC
    with pytest.raises(AssertionError):
        _parse_scel_binary(corrupt_bytes)


def test_lexicon_rime_dict_yaml_and_tsv() -> None:
    yaml_text = (FIXTURES_DIR / "lexicons/sample_terms.dict.yaml").read_text(encoding="utf-8")
    lines = yaml_text.splitlines()
    assert any(line.startswith("#") for line in lines), "Expected # comments in sample_terms.dict.yaml"
    start_idx = lines.index("---")
    end_idx = lines.index("...")
    header_block = "\n".join(lines[start_idx + 1 : end_idx])
    assert "name: sample_terms" in header_block
    assert 'version: "1.0"' in header_block
    assert "sort: by_weight" in header_block
    assert "use_preset_vocabulary: false" in header_block
    assert "- text" in header_block and "- code" in header_block and "- weight" in header_block

    body_rows = [ln for ln in lines[end_idx + 1 :] if ln.strip() and not ln.lstrip().startswith("#")]
    assert len(body_rows) >= 10
    weights = []
    words = set()
    for row in body_rows:
        cols = row.split("\t")
        assert len(cols) == 3, f"Invalid dict.yaml row: {row!r}"
        word, pinyin, w_str = cols
        assert re.fullmatch(r"[a-z]+( [a-z]+)*", pinyin), f"Bad pinyin in {row!r}"
        wt = int(w_str)
        assert wt >= 0
        weights.append(wt)
        words.add(word)
    assert {"爆光", "老登", "爆金币"}.issubset(words)
    assert weights != sorted(weights, reverse=True), "Weights should be unsorted to exercise sort: by_weight"

    tsv_lines = [
        ln for ln in (FIXTURES_DIR / "lexicons/sample_pairs.tsv").read_text(encoding="utf-8").splitlines() if ln.strip()
    ]
    assert len(tsv_lines) >= 10
    for row in tsv_lines:
        cols = row.split("\t")
        assert len(cols) == 3, f"Invalid TSV row: {row!r}"
        assert re.fullmatch(r"[a-z]+( [a-z]+)*", cols[1])
        assert int(cols[2]) >= 0


def test_lexicon_polyphones_purity_and_oracle_alignment() -> None:
    raw_lines = (FIXTURES_DIR / "lexicons/polyphone_unannotated.txt").read_text(encoding="utf-8").splitlines()
    phrases = [ln for ln in raw_lines if ln.strip()]
    assert len(phrases) >= 12
    for idx, phrase in enumerate(phrases, start=1):
        assert "\t" not in phrase, f"Tab found in unannotated line {idx}"
        assert not re.search(r"[a-zA-Z0-9]", phrase), f"ASCII letter/digit found in unannotated line {idx}: {phrase!r}"

    oracle = _load_json("lexicons/expected_polyphones.json")
    assert set(oracle["polyphones_covered"]) == {"重", "行", "长", "乐", "得", "发"}

    readings_by_char: dict[str, set[tuple[str, str]]] = {ch: set() for ch in oracle["polyphones_covered"]}
    for case in oracle["cases"]:
        line_no = case["line"]
        assert 1 <= line_no <= len(phrases)
        assert phrases[line_no - 1] == case["text"]
        assert case["text"][case["target_index"]] == case["target_char"]
        assert len(case["expected_full_pinyin"].split()) == len(case["text"])
        assert case["expected_syllable"] not in case["disallowed_syllables"]
        assert case["expected_toned_syllable"] not in case["disallowed_syllables"]
        readings_by_char[case["target_char"]].add(
            (case["expected_syllable"], case["expected_toned_syllable"])
        )

    for ch, readings in readings_by_char.items():
        assert len(readings) >= 2, f"Polyphone {ch} needs >= 2 distinct readings, got {readings}"


def test_lexicon_malformed_edge_cases() -> None:
    text = (FIXTURES_DIR / "lexicons/malformed_edge.txt").read_text(encoding="utf-8")
    lines = text.splitlines()
    assert any(ln == "" for ln in lines), "Need blank line"
    assert any(ln.strip() == "" and len(ln) > 0 for ln in lines), "Need whitespace-only line"
    assert any(ln.startswith("#") for ln in lines), "Need # comment line"
    assert any("\t" not in ln and ln.strip() and not ln.startswith("#") for ln in lines), "Need missing-tab line"
    assert any(len(ln.split("\t")) == 2 for ln in lines), "Need 2-col line"
    assert any(len(ln.split("\t")) > 3 for ln in lines), "Need >3-col line"
    assert any(len(ln) > 256 for ln in lines), "Need >256-char line"
    assert any(
        len(ln.split("\t")) == 3 and ln.split("\t")[2].startswith("-") for ln in lines
    ), "Need negative weight"
    assert any(
        len(ln.split("\t")) == 3 and not ln.split("\t")[2].lstrip("-").isdigit() for ln in lines
    ), "Need non-numeric weight"


@pytest.mark.parametrize(
    "filename",
    [
        "handwriting/mode1_mao.json",
        "handwriting/mode1_di.json",
        "handwriting/mode1_slow_reversed.json",
        "handwriting/mode2_baoguang_laodeng.json",
        "handwriting/mode2_clear_locked.json",
        "handwriting/mode2_horizontal_connected.json",
    ],
)
def test_handwriting_common_invariants(filename: str) -> None:
    data = _load_json(filename)
    assert data["mode"] in {"mode1", "mode2"}
    assert data["coordinate_space"] == {"min": 0.0, "max": 1.0}
    strokes = data["strokes"]
    assert len(strokes) >= 1

    prev_t = -1
    for stroke in strokes:
        assert len(stroke) >= 2
        for pt in stroke:
            assert 0.0 <= pt["x"] <= 1.0
            assert 0.0 <= pt["y"] <= 1.0
            assert isinstance(pt["t"], int)
            assert pt["t"] > prev_t
            prev_t = pt["t"]


def test_handwriting_specific_scenarios() -> None:
    mao = _load_json("handwriting/mode1_mao.json")
    assert mao["expected"]["char"] == "耄"
    assert mao["expected"]["toned_pinyin"] == "mào"
    assert mao["expected"]["xiaohe_code"] == "mc"
    assert len(mao["strokes"]) == 10

    di = _load_json("handwriting/mode1_di.json")
    assert di["expected"]["char"] == "頔"
    assert di["expected"]["toned_pinyin"] == "dí"
    assert di["expected"]["xiaohe_code"] == "di"
    assert len(di["strokes"]) == 14

    slow = _load_json("handwriting/mode1_slow_reversed.json")
    assert slow["expected"]["reversed_stroke_order"] is True
    assert slow["expected"]["auto_clear_forbidden"] is True
    assert slow["strokes"][-1][-1]["t"] > 5000

    baoguang = _load_json("handwriting/mode2_baoguang_laodeng.json")
    assert baoguang["expected"]["final_text"] == "爆光老登金币"
    assert baoguang["expected"]["slot0_locked_by_entropy"] is False
    assert baoguang["expected"]["override_test"]["override_to"] == "曝"
    segs = baoguang["char_segments"]
    assert len(segs) == 6
    for i in range(1, len(segs)):
        assert segs[i]["t_start_ms"] - segs[i - 1]["t_end_ms"] > 420

    locked = _load_json("handwriting/mode2_clear_locked.json")
    assert locked["expected"]["final_text"] == "曝光老登金币"
    assert locked["expected"]["slot0_locked_by_entropy"] is True
    assert locked["expected"]["progressive_composing"][-1]["retroactive_changed_slots"] == []

    horiz = _load_json("handwriting/mode2_horizontal_connected.json")
    assert horiz["expected"]["final_text"] == "手写中文"
    centroids = [s["centroid_x"] for s in horiz["char_segments"]]
    assert centroids == sorted(centroids)
    assert all(centroids[i] > centroids[i - 1] for i in range(1, len(centroids)))
    bboxes = [s["bbox"] for s in horiz["char_segments"]]
    for i in range(1, len(bboxes)):
        assert bboxes[i]["x_min"] > bboxes[i - 1]["x_max"]


def test_audio_wav_headers_rms_clipping_and_manifest() -> None:
    manifest = _load_json("audio/audio_manifest.json")
    fmt = manifest["audio_format"]
    assert fmt["sample_rate_hz"] == 16000
    assert fmt["channels"] == 1
    assert fmt["bits_per_sample"] == 16

    clips = {c["file"]: c for c in manifest["clips"]}
    expected_files = {
        "mandarin_short_16k.wav",
        "mandarin_mixed_en_16k.wav",
        "long_20s_16k.wav",
        "silence_noise_16k.wav",
    }
    assert set(clips.keys()) == expected_files

    for fname, meta in clips.items():
        wav_path = FIXTURES_DIR / "audio" / fname
        assert wav_path.is_file()
        assert _sha256(wav_path) == meta["sha256"]

        with wave.open(str(wav_path), "rb") as w:
            assert w.getframerate() == 16000
            assert w.getnchannels() == 1
            assert w.getsampwidth() == 2
            nframes = w.getnframes()
            assert nframes > 0
            raw = w.readframes(nframes)

        samples = struct.unpack(f"<{len(raw) // 2}h", raw)
        rms = math.sqrt(sum(s * s for s in samples) / len(samples))
        peak = max(abs(s) for s in samples)
        duration = nframes / 16000.0

        assert rms > 0.0, f"{fname} must not be digital zero"
        assert peak < 32768, f"{fname} has int16 clipping (peak={peak})"
        assert meta["min_duration_sec"] <= duration <= meta["max_duration_sec"]
        assert abs(duration - meta["duration_sec"]) < 0.01
        assert abs(rms - meta["rms_int16"]) < 0.5
        assert peak == meta["peak_int16"]

        if fname == "long_20s_16k.wav":
            assert duration >= 20.0
        if meta["expect_non_empty_transcript"]:
            assert rms >= meta["min_rms_int16"]
        else:
            assert meta["min_rms_int16"] <= rms <= meta["max_rms_int16"]
