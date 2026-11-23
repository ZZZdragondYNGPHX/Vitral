"""Non-raising artifact discovery for Vitral E2E tests."""

from __future__ import annotations

import os
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable


def _default_repo_root() -> Path:
    # .../Vitral/tests/e2e/vitral_e2e/discovery.py -> parents[3] == .../Vitral
    return Path(__file__).resolve().parents[3]


def _first_existing(
    env_vars: str | Iterable[str] | None,
    candidates: Iterable[Path | str | None],
    *,
    allow_env: bool = True,
    expect_dir: bool = False,
) -> Path | None:
    def _exists(p: Path) -> bool:
        return p.is_dir() if expect_dir else p.is_file()

    if allow_env and env_vars is not None:
        names = (env_vars,) if isinstance(env_vars, str) else tuple(env_vars)
        for name in names:
            raw = os.environ.get(name, "").strip()
            if raw:
                p = Path(raw).expanduser()
                if _exists(p):
                    return p.resolve()

    for cand in candidates:
        if cand is None:
            continue
        p = Path(cand).expanduser()
        if _exists(p):
            return p.resolve()
    return None


def _glob_candidates(base: Path, pattern: str) -> list[Path]:
    if not base.exists():
        return []
    return sorted(p for p in base.glob(pattern) if p.is_file())


@dataclass(frozen=True)
class DiscoveredTargets:
    """Resolved paths (or None when unbuilt/missing) for all Vitral deliverables."""

    vitral_root: Path
    design_tokens_path: Path | None = None
    fixtures_dir: Path | None = None
    assets_dir: Path | None = None
    fetch_assets_script: Path | None = None
    test_all_script: Path | None = None
    gen_tokens_script: Path | None = None
    vitral_cli: Path | None = None
    vitral_ffi_lib: Path | None = None
    vitral_ffi_header: Path | None = None
    vitral_jni_libs: list[Path] = field(default_factory=list)
    fcitx5_plugin: Path | None = None
    tauri_linux_bin: Path | None = None
    android_apk: Path | None = None
    windows_tsf_dll: Path | None = None
    windows_tauri_exe: Path | None = None

    def missing_artifacts(self) -> list[str]:
        missing: list[str] = []
        checks = (
            ("design_tokens_path", self.design_tokens_path),
            ("fixtures_dir", self.fixtures_dir),
            ("assets_dir", self.assets_dir),
            ("fetch_assets_script", self.fetch_assets_script),
            ("test_all_script", self.test_all_script),
            ("gen_tokens_script", self.gen_tokens_script),
            ("vitral_cli", self.vitral_cli),
            ("vitral_ffi_lib", self.vitral_ffi_lib),
            ("vitral_ffi_header", self.vitral_ffi_header),
            ("fcitx5_plugin", self.fcitx5_plugin),
            ("tauri_linux_bin", self.tauri_linux_bin),
            ("android_apk", self.android_apk),
            ("windows_tsf_dll", self.windows_tsf_dll),
            ("windows_tauri_exe", self.windows_tauri_exe),
        )
        for name, val in checks:
            if val is None:
                missing.append(name)
        if not self.vitral_jni_libs:
            missing.append("vitral_jni_libs")
        return missing

    def as_dict(self) -> dict[str, object]:
        return {
            "vitral_root": str(self.vitral_root),
            "design_tokens_path": str(self.design_tokens_path) if self.design_tokens_path else None,
            "fixtures_dir": str(self.fixtures_dir) if self.fixtures_dir else None,
            "assets_dir": str(self.assets_dir) if self.assets_dir else None,
            "fetch_assets_script": str(self.fetch_assets_script) if self.fetch_assets_script else None,
            "test_all_script": str(self.test_all_script) if self.test_all_script else None,
            "gen_tokens_script": str(self.gen_tokens_script) if self.gen_tokens_script else None,
            "vitral_cli": str(self.vitral_cli) if self.vitral_cli else None,
            "vitral_ffi_lib": str(self.vitral_ffi_lib) if self.vitral_ffi_lib else None,
            "vitral_ffi_header": str(self.vitral_ffi_header) if self.vitral_ffi_header else None,
            "vitral_jni_libs": [str(p) for p in self.vitral_jni_libs],
            "fcitx5_plugin": str(self.fcitx5_plugin) if self.fcitx5_plugin else None,
            "tauri_linux_bin": str(self.tauri_linux_bin) if self.tauri_linux_bin else None,
            "android_apk": str(self.android_apk) if self.android_apk else None,
            "windows_tsf_dll": str(self.windows_tsf_dll) if self.windows_tsf_dll else None,
            "windows_tauri_exe": str(self.windows_tauri_exe) if self.windows_tauri_exe else None,
        }


def discover_targets(root: Path | None = None) -> DiscoveredTargets:
    """Locate Vitral artifacts without raising if targets are unbuilt.

    When an explicit ``root`` path is provided, discovery is strictly scoped to
    that directory (ignoring global PATH, system directories, and environment
    overrides) so isolation tests remain deterministic even after builds exist.
    """
    isolated = root is not None
    if isolated:
        vitral_root = Path(root).resolve()
    else:
        env_root = os.environ.get("VITRAL_ROOT", "").strip()
        vitral_root = Path(env_root).expanduser().resolve() if env_root else _default_repo_root()

    allow_env = not isolated

    design_tokens_path = _first_existing(
        "VITRAL_TOKENS_PATH",
        [vitral_root / "tokens" / "design-tokens.json"],
        allow_env=allow_env,
    )
    fixtures_dir = _first_existing(
        "VITRAL_FIXTURES_DIR",
        [vitral_root / "tests" / "fixtures"],
        allow_env=allow_env,
        expect_dir=True,
    )
    assets_dir = _first_existing(
        "VITRAL_ASSETS_DIR",
        [vitral_root / "assets"],
        allow_env=allow_env,
        expect_dir=True,
    )
    fetch_assets_script = _first_existing(
        "VITRAL_FETCH_ASSETS_SCRIPT",
        [vitral_root / "scripts" / "fetch-assets.sh"],
        allow_env=allow_env,
    )
    test_all_script = _first_existing(
        "VITRAL_TEST_ALL_SCRIPT",
        [vitral_root / "scripts" / "test-all.sh"],
        allow_env=allow_env,
    )
    gen_tokens_script = _first_existing(
        "VITRAL_GEN_TOKENS_SCRIPT",
        [vitral_root / "scripts" / "gen-tokens.py"],
        allow_env=allow_env,
    )

    cli_candidates: list[Path | str | None] = [
        vitral_root / "target" / "release" / "vitral",
        vitral_root / "target" / "debug" / "vitral",
        vitral_root / "target" / "release" / "vitral-cli",
        vitral_root / "target" / "debug" / "vitral-cli",
    ]
    if not isolated:
        cli_candidates.extend(
            [
                shutil.which("vitral"),
                shutil.which("vitral-cli"),
            ]
        )
    vitral_cli = _first_existing(
        ("VITRAL_CLI_BIN", "VITRAL_CLI"),
        cli_candidates,
        allow_env=allow_env,
    )

    vitral_ffi_lib = _first_existing(
        "VITRAL_FFI_LIB",
        [
            vitral_root / "target" / "release" / "libvitral_ffi.so",
            vitral_root / "target" / "debug" / "libvitral_ffi.so",
        ],
        allow_env=allow_env,
    )
    vitral_ffi_header = _first_existing(
        "VITRAL_FFI_HEADER",
        [vitral_root / "crates" / "vitral-ffi" / "include" / "vitral_core.h"],
        allow_env=allow_env,
    )

    jni_libs: list[Path] = []
    if allow_env:
        env_jni = os.environ.get("VITRAL_JNI_DIR", "").strip()
        if env_jni:
            env_jni_path = Path(env_jni).expanduser()
            if env_jni_path.is_file():
                jni_libs.append(env_jni_path.resolve())
            elif env_jni_path.is_dir():
                jni_libs.extend(sorted(p.resolve() for p in env_jni_path.rglob("libvitral_jni.so") if p.is_file()))
    if not jni_libs:
        jni_candidates = _glob_candidates(vitral_root / "target", "*/release/libvitral_jni.so")
        jni_candidates.extend(_glob_candidates(vitral_root / "target", "*/debug/libvitral_jni.so"))
        jni_candidates.extend(_glob_candidates(vitral_root / "apps" / "android", "**/jniLibs/**/libvitral_jni.so"))
        jni_libs = sorted({p.resolve() for p in jni_candidates})

    fcitx5_candidates: list[Path] = [
        vitral_root / "linux" / "fcitx5-vitral" / "build" / "libvitral-fcitx5.so",
        vitral_root / "linux" / "fcitx5-vitral" / "build" / "src" / "libvitral-fcitx5.so",
        vitral_root / "target" / "release" / "libvitral-fcitx5.so",
    ]
    if not isolated:
        fcitx5_candidates.extend(
            [
                Path("~/.local/lib/fcitx5/libvitral-fcitx5.so").expanduser(),
                Path("/usr/lib/fcitx5/libvitral-fcitx5.so"),
            ]
        )
    fcitx5_plugin = _first_existing(
        "VITRAL_FCITX5_SO",
        fcitx5_candidates,
        allow_env=allow_env,
    )

    tauri_linux_bin = _first_existing(
        ("VITRAL_TAURI_LINUX_BIN", "VITRAL_TAURI_BIN"),
        [
            vitral_root / "apps" / "desktop-tauri" / "src-tauri" / "target" / "release" / "vitral-desktop",
            vitral_root / "apps" / "desktop-tauri" / "src-tauri" / "target" / "release" / "vitral",
            vitral_root / "target" / "release" / "vitral-desktop",
            vitral_root / "apps" / "desktop-tauri" / "src-tauri" / "target" / "debug" / "vitral-desktop",
            vitral_root / "target" / "debug" / "vitral-desktop",
        ],
        allow_env=allow_env,
    )

    android_apk_candidates: list[Path] = [
        vitral_root / "target" / "release" / "Vitral-android.apk",
        vitral_root / "apps" / "android" / "app" / "build" / "outputs" / "apk" / "debug" / "app-debug.apk",
        *_glob_candidates(vitral_root / "apps" / "android" / "app" / "build" / "outputs" / "apk" / "release", "*.apk"),
        *_glob_candidates(vitral_root / "apps" / "android", "**/outputs/apk/**/*.apk"),
    ]
    android_apk = _first_existing(
        "VITRAL_ANDROID_APK",
        android_apk_candidates,
        allow_env=allow_env,
    )

    windows_tsf_dll = _first_existing(
        "VITRAL_WINDOWS_TSF_DLL",
        [
            vitral_root / "target" / "x86_64-pc-windows-msvc" / "release" / "vitral_tsf.dll",
            vitral_root / "target" / "x86_64-pc-windows-msvc" / "debug" / "vitral_tsf.dll",
        ],
        allow_env=allow_env,
    )

    windows_tauri_candidates: list[Path] = [
        *_glob_candidates(
            vitral_root / "apps" / "desktop-tauri" / "src-tauri" / "target" / "x86_64-pc-windows-msvc" / "release",
            "*.exe",
        ),
        vitral_root / "target" / "x86_64-pc-windows-msvc" / "release" / "vitral-desktop.exe",
    ]
    windows_tauri_exe = _first_existing(
        ("VITRAL_WINDOWS_TAURI_EXE", "VITRAL_TAURI_WINDOWS_EXE"),
        windows_tauri_candidates,
        allow_env=allow_env,
    )

    return DiscoveredTargets(
        vitral_root=vitral_root,
        design_tokens_path=design_tokens_path,
        fixtures_dir=fixtures_dir,
        assets_dir=assets_dir,
        fetch_assets_script=fetch_assets_script,
        test_all_script=test_all_script,
        gen_tokens_script=gen_tokens_script,
        vitral_cli=vitral_cli,
        vitral_ffi_lib=vitral_ffi_lib,
        vitral_ffi_header=vitral_ffi_header,
        vitral_jni_libs=jni_libs,
        fcitx5_plugin=fcitx5_plugin,
        tauri_linux_bin=tauri_linux_bin,
        android_apk=android_apk,
        windows_tsf_dll=windows_tsf_dll,
        windows_tauri_exe=windows_tauri_exe,
    )
