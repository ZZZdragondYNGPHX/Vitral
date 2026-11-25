"""Opaque-box target adapter mirroring Vitral's C-ABI / CLI / Platform contracts."""

from __future__ import annotations

import base64
import ctypes
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any

import pytest

from .discovery import DiscoveredTargets, discover_targets


class VitralCommandError(RuntimeError):
    """Raised when vitral-cli returns a non-zero exit status or invalid JSON."""

    def __init__(self, subcommand: str, returncode: int, stdout: str, stderr: str) -> None:
        self.subcommand = subcommand
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr
        super().__init__(
            f"vitral-cli {subcommand!r} failed (exit={returncode}): {stderr.strip() or stdout.strip()}"
        )


class VitralTarget:
    """Opaque-box adapter over Vitral CLI, FFI library, and platform artifacts."""

    _live_instances: list["VitralTarget"] = []

    def __init__(
        self,
        targets: DiscoveredTargets | None = None,
        state_dir: Path | None = None,
        root: Path | None = None,
        env_overrides: dict[str, str] | None = None,
    ) -> None:
        if targets is not None:
            self.targets = targets
        elif root is not None:
            self.targets = discover_targets(root)
        else:
            self.targets = discover_targets()

        if state_dir is None:
            self._owns_state_dir = True
            self.state_dir = Path(tempfile.mkdtemp(prefix="vitral_e2e_state_"))
        else:
            self._owns_state_dir = False
            self.state_dir = Path(state_dir)
            self.state_dir.mkdir(parents=True, exist_ok=True)

        self.env_overrides: dict[str, str] = dict(env_overrides or {})
        self._initialized = False
        VitralTarget._live_instances.append(self)

    # ------------------------------------------------------------------
    # Clean Fail-Fast Target Requirements (NEVER skip, NEVER ImportError)
    # ------------------------------------------------------------------

    def require_core(self) -> Path:
        if self.targets.vitral_cli is not None:
            return self.targets.vitral_cli
        if self.targets.vitral_ffi_lib is not None:
            return self.targets.vitral_ffi_lib
        pytest.fail(
            "Target artifact not built yet: vitral-core "
            f"(neither vitral-cli nor libvitral_ffi.so found under {self.targets.vitral_root})"
        )

    def require_cli(self) -> Path:
        if self.targets.vitral_cli is None:
            pytest.fail(
                "Target artifact not built yet: vitral-cli "
                f"(expected under {self.targets.vitral_root}/target/{{release,debug}}/vitral-cli or VITRAL_CLI_BIN)"
            )
        return self.targets.vitral_cli

    def require_ffi(self) -> Path:
        if self.targets.vitral_ffi_lib is None:
            pytest.fail(
                "Target artifact not built yet: libvitral_ffi.so "
                f"(expected under {self.targets.vitral_root}/target/{{release,debug}}/libvitral_ffi.so or VITRAL_FFI_LIB)"
            )
        return self.targets.vitral_ffi_lib

    def require_ffi_header(self) -> Path:
        if self.targets.vitral_ffi_header is None:
            pytest.fail(
                "Target artifact not built yet: vitral_core.h "
                f"(expected at {self.targets.vitral_root}/crates/vitral-ffi/include/vitral_core.h)"
            )
        return self.targets.vitral_ffi_header

    def require_tokens(self) -> Path:
        if self.targets.design_tokens_path is None:
            pytest.fail(
                "Target artifact not built yet: tokens/design-tokens.json "
                f"(expected under {self.targets.vitral_root}/tokens/design-tokens.json)"
            )
        return self.targets.design_tokens_path

    def require_fcitx5(self) -> Path:
        if self.targets.fcitx5_plugin is None:
            pytest.fail(
                "Target artifact not built yet: libvitral-fcitx5.so "
                f"(expected under {self.targets.vitral_root}/linux/fcitx5-vitral/build or VITRAL_FCITX5_SO)"
            )
        return self.targets.fcitx5_plugin

    def require_linux(self) -> tuple[Path, Path]:
        missing: list[str] = []
        if self.targets.fcitx5_plugin is None:
            missing.append("libvitral-fcitx5.so")
        if self.targets.tauri_linux_bin is None:
            missing.append("Linux Tauri v2 binary")
        if missing:
            pytest.fail(
                "Target artifact not built yet: Linux desktop targets missing "
                f"({', '.join(missing)}) under {self.targets.vitral_root}"
            )
        assert self.targets.fcitx5_plugin is not None
        assert self.targets.tauri_linux_bin is not None
        return (self.targets.fcitx5_plugin, self.targets.tauri_linux_bin)

    def require_android(self) -> Path:
        return self.require_android_apk()

    def require_android_apk(self) -> Path:
        if self.targets.android_apk is None:
            pytest.fail(
                "Target artifact not built yet: Android APK "
                f"(expected under {self.targets.vitral_root}/apps/android/.../outputs/apk/ or VITRAL_ANDROID_APK)"
            )
        return self.targets.android_apk

    def require_android_jni(self) -> list[Path]:
        if not self.targets.vitral_jni_libs:
            pytest.fail(
                "Target artifact not built yet: libvitral_jni.so "
                f"(expected under {self.targets.vitral_root}/target/*/release/ or apps/android/**/jniLibs/)"
            )
        return self.targets.vitral_jni_libs

    def require_windows(self) -> tuple[Path, Path]:
        missing: list[str] = []
        if self.targets.windows_tsf_dll is None:
            missing.append("vitral_tsf.dll")
        if self.targets.windows_tauri_exe is None:
            missing.append("Windows Tauri v2 exe")
        if missing:
            pytest.fail(
                "Target artifact not built yet: Windows desktop targets missing "
                f"({', '.join(missing)}) under {self.targets.vitral_root}"
            )
        assert self.targets.windows_tsf_dll is not None
        assert self.targets.windows_tauri_exe is not None
        return (self.targets.windows_tsf_dll, self.targets.windows_tauri_exe)

    def require_windows_tsf(self) -> Path:
        if self.targets.windows_tsf_dll is None:
            pytest.fail(
                "Target artifact not built yet: vitral_tsf.dll "
                f"(expected under {self.targets.vitral_root}/target/x86_64-pc-windows-msvc/ or VITRAL_WINDOWS_TSF_DLL)"
            )
        return self.targets.windows_tsf_dll

    def require_tauri(self) -> Path:
        if self.targets.tauri_linux_bin is not None:
            return self.targets.tauri_linux_bin
        if self.targets.windows_tauri_exe is not None:
            return self.targets.windows_tauri_exe
        pytest.fail(
            "Target artifact not built yet: Tauri v2 management center binary "
            f"(expected under {self.targets.vitral_root}/apps/desktop-tauri/ or VITRAL_TAURI_BIN)"
        )

    def require_fetch_assets_script(self) -> Path:
        if self.targets.fetch_assets_script is None:
            pytest.fail(
                "Target artifact not built yet: scripts/fetch-assets.sh "
                f"(expected under {self.targets.vitral_root}/scripts/fetch-assets.sh)"
            )
        return self.targets.fetch_assets_script

    def require_test_all_script(self) -> Path:
        if self.targets.test_all_script is None:
            pytest.fail(
                "Target artifact not built yet: scripts/test-all.sh "
                f"(expected under {self.targets.vitral_root}/scripts/test-all.sh)"
            )
        return self.targets.test_all_script

    def load_ffi_cdll(self) -> ctypes.CDLL:
        lib_path = self.require_ffi()
        return ctypes.CDLL(str(lib_path))

    # ------------------------------------------------------------------
    # Opaque-Box CLI Invocation Transport
    # ------------------------------------------------------------------

    def _build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["VITRAL_STATE_DIR"] = str(self.state_dir)
        env["VITRAL_ROOT"] = str(self.targets.vitral_root)
        if self.targets.design_tokens_path is not None:
            env["VITRAL_TOKENS_PATH"] = str(self.targets.design_tokens_path)
        if self.targets.assets_dir is not None:
            env["VITRAL_ASSETS_DIR"] = str(self.targets.assets_dir)
        env.update(self.env_overrides)
        return env

    def _invoke_cli(
        self,
        subcommand: str,
        payload: dict[str, Any] | None = None,
        *,
        timeout: float = 15.0,
    ) -> Any:
        cli = self.require_cli()
        cmd = [str(cli), "--state-dir", str(self.state_dir), subcommand]
        stdin_text = json.dumps(payload if payload is not None else {}, ensure_ascii=False)
        proc = subprocess.run(
            cmd,
            input=stdin_text,
            capture_output=True,
            text=True,
            env=self._build_env(),
            timeout=timeout,
            check=False,
        )
        if proc.returncode != 0:
            raise VitralCommandError(subcommand, proc.returncode, proc.stdout, proc.stderr)
        out = proc.stdout.strip()
        if not out:
            return {"ok": True}
        try:
            return json.loads(out)
        except json.JSONDecodeError as exc:
            raise VitralCommandError(
                subcommand,
                proc.returncode,
                proc.stdout,
                f"Invalid JSON from vitral-cli: {exc}",
            ) from exc

    # ------------------------------------------------------------------
    # PROJECT.md § Interface Contracts Wrapper Surface
    # ------------------------------------------------------------------

    def init(self, config: dict[str, Any] | None = None) -> dict[str, Any]:
        res = self._invoke_cli("init", {"config": config or {}})
        self._initialized = True
        return res

    def shutdown(self) -> dict[str, Any]:
        res = self._invoke_cli("shutdown", {})
        self._initialized = False
        return res

    def set_pure_offline(self, enabled: bool) -> dict[str, Any]:
        return self._invoke_cli("set-pure-offline", {"enabled": bool(enabled)})

    def is_pure_offline(self) -> bool:
        res = self._invoke_cli("is-pure-offline", {})
        if isinstance(res, bool):
            return res
        return bool(res.get("pure_offline", res.get("enabled", False)))

    def set_schema(self, schema: str) -> dict[str, Any]:
        return self._invoke_cli("set-schema", {"schema": schema})

    def set_opencc_mode(self, mode: str) -> dict[str, Any]:
        return self._invoke_cli("set-opencc-mode", {"mode": mode})

    def reload_assets(self) -> dict[str, Any]:
        return self._invoke_cli("reload-assets", {})

    def on_app_focus(self, app_id: str) -> dict[str, Any]:
        return self._invoke_cli("on-app-focus", {"app_id": app_id})

    def process_key(
        self,
        keycode: str,
        modifiers: list[str] | None = None,
        swipe_dir: str | None = None,
    ) -> dict[str, Any]:
        return self._invoke_cli(
            "process-key",
            {
                "keycode": keycode,
                "modifiers": list(modifiers or []),
                "swipe_dir": swipe_dir,
            },
        )

    def select_candidate(self, idx: int) -> dict[str, Any]:
        return self._invoke_cli("select-candidate", {"idx": int(idx)})

    def pin_candidate(self, idx: int) -> dict[str, Any]:
        return self._invoke_cli("pin-candidate", {"idx": int(idx)})

    def erase_candidate(self, idx: int) -> dict[str, Any]:
        return self._invoke_cli("erase-candidate", {"idx": int(idx)})

    def hw_begin(self, mode: str) -> dict[str, Any]:
        return self._invoke_cli("hw-begin", {"mode": mode})

    def hw_push_stroke(
        self,
        points: list[dict[str, float]],
        pen_up: bool = True,
    ) -> dict[str, Any]:
        return self._invoke_cli(
            "hw-push-stroke",
            {"points": points, "pen_up": bool(pen_up)},
        )

    def hw_end(self) -> dict[str, Any]:
        return self._invoke_cli("hw-end", {})

    def hw_clear(self) -> dict[str, Any]:
        return self._invoke_cli("hw-clear", {})

    def hw_override_slot(self, slot_idx: int, candidate_idx: int) -> dict[str, Any]:
        return self._invoke_cli(
            "hw-override-slot",
            {"slot_idx": int(slot_idx), "candidate_idx": int(candidate_idx)},
        )

    def import_lexicon(self, path: Path | str, format_hint: str | None = None) -> dict[str, Any]:
        return self._invoke_cli(
            "import-lexicon",
            {"path": str(path), "format_hint": format_hint},
        )

    def clipboard_push(self, payload: str, source_device: str | None = None) -> dict[str, Any]:
        return self._invoke_cli(
            "clipboard-push",
            {"payload": payload, "source_device": source_device},
        )

    def clipboard_ingest_remote(self, payload: str | dict[str, Any], is_cold_boot: bool = False) -> dict[str, Any]:
        return self._invoke_cli(
            "clipboard-ingest-remote",
            {"payload": payload, "is_cold_boot": bool(is_cold_boot)},
        )

    def clipboard_state(self) -> dict[str, Any]:
        """Return persistent N-item clipboard ring state (independent of the current copy and mobile suggestion)."""
        return self._invoke_cli("clipboard-state", {})

    def clipboard_poll(self) -> dict[str, Any]:
        return self._invoke_cli("clipboard-poll", {})

    def quick_paste_state(self) -> dict[str, Any]:
        return self._invoke_cli("quick-paste-state", {})

    def consume_quick_paste(self) -> dict[str, Any]:
        return self._invoke_cli("consume-quick-paste", {})

    def on_ime_open(self) -> dict[str, Any]:
        return self._invoke_cli("on-ime-open", {})

    def on_ime_close(self) -> dict[str, Any]:
        return self._invoke_cli("on-ime-close", {})

    def dismiss_quick_paste(self) -> dict[str, Any]:
        return self._invoke_cli("dismiss-quick-paste", {})

    def clipboard_pin(self, entry_id: str, shortcut_code: str) -> dict[str, Any]:
        return self._invoke_cli(
            "clipboard-pin",
            {"id": entry_id, "shortcut_code": shortcut_code},
        )

    def clipboard_clear_unpinned(self) -> dict[str, Any]:
        return self._invoke_cli("clipboard-clear-unpinned", {})

    def relay_devices_mru(self) -> list[dict[str, Any]]:
        res = self._invoke_cli("relay-devices-mru", {})
        if isinstance(res, list):
            return res
        return list(res.get("devices", []))

    def relay_send(self, device_id: str, path_or_text: str) -> dict[str, Any]:
        return self._invoke_cli(
            "relay-send",
            {"device_id": device_id, "path_or_text": path_or_text},
        )

    def relay_poll_inbox(self) -> list[Any]:
        res = self._invoke_cli("relay-poll-inbox", {})
        if isinstance(res, list):
            return res
        if isinstance(res, dict) and "received" in res:
            return list(res["received"] or [])
        return list(res.get("items") or [])

    def relay_cleanup_orphans(self) -> dict[str, Any]:
        return self._invoke_cli("relay-cleanup-orphans", {})

    def voice_start(self) -> dict[str, Any]:
        return self._invoke_cli("voice-start", {})

    def voice_push_pcm(self, pcm_i16: bytes) -> dict[str, Any]:
        return self._invoke_cli(
            "voice-push-pcm",
            {"pcm_i16_b64": base64.b64encode(pcm_i16).decode("ascii")},
        )

    def voice_stop(self) -> dict[str, Any]:
        return self._invoke_cli("voice-stop", {})

    def ai_inline(self, mode: str, text: str, target_lang: str = "en") -> dict[str, Any]:
        return self._invoke_cli(
            "ai-inline",
            {"mode": mode, "text": text, "target_lang": target_lang},
        )

    def restore_from_koofr(self, cfg: dict[str, Any]) -> dict[str, Any]:
        return self._invoke_cli("restore-from-koofr", {"config": cfg})

    def close(self) -> None:
        had_session = self._initialized or (self.state_dir / ".vitral-session").is_dir()
        if had_session and self.targets.vitral_cli is not None:
            try:
                self.shutdown()
            except Exception:
                pass
        self._initialized = False
        try:
            VitralTarget._live_instances.remove(self)
        except ValueError:
            pass
        if self._owns_state_dir:
            shutil.rmtree(self.state_dir, ignore_errors=True)
