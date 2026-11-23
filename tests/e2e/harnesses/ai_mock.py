"""Local Ollama + OpenAI-compatible mock AI server for Vitral E2E tests."""

from __future__ import annotations

import json
import re
import secrets
import threading
import time
import urllib.parse
from dataclasses import dataclass
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


def _default_models() -> list[dict[str, Any]]:
    now_iso = "2026-11-20T14:00:00Z"
    return [
        {
            "name": "qwen2.5:7b",
            "model": "qwen2.5:7b",
            "modified_at": now_iso,
            "size": 4683075271,
            "digest": "sha256:mockqwen257b0000000000000000000000000000000000000000000000000001",
            "details": {
                "parent_model": "",
                "format": "gguf",
                "family": "qwen2",
                "families": ["qwen2"],
                "parameter_size": "7.6B",
                "quantization_level": "Q4_K_M",
            },
        },
        {
            "name": "gemma2:9b",
            "model": "gemma2:9b",
            "modified_at": now_iso,
            "size": 5443152417,
            "digest": "sha256:mockgemma29b0000000000000000000000000000000000000000000000000002",
            "details": {
                "parent_model": "",
                "format": "gguf",
                "family": "gemma2",
                "families": ["gemma2"],
                "parameter_size": "9.2B",
                "quantization_level": "Q4_K_M",
            },
        },
        {
            "name": "vitral-mock:latest",
            "model": "vitral-mock:latest",
            "modified_at": now_iso,
            "size": 1024000000,
            "digest": "sha256:mockvitral0000000000000000000000000000000000000000000000000003",
            "details": {
                "parent_model": "",
                "format": "gguf",
                "family": "vitral-mock",
                "families": ["vitral-mock"],
                "parameter_size": "1.5B",
                "quantization_level": "Q8_0",
            },
        },
    ]


class _MalformedBodyError(ValueError):
    """Raised when an HTTP request body has invalid framing or headers."""


@dataclass
class _AIErrorRule:
    status_code: int
    message: str
    endpoint: str | None
    remaining: int | None


class _AIHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        handler_cls: type[BaseHTTPRequestHandler],
        owner: AIMockServer,
    ) -> None:
        self.owner = owner
        super().__init__(server_address, handler_cls)


class _AIRequestHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: _AIHTTPServer

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        return

    def _read_body(self) -> bytes:
        te = self.headers.get("Transfer-Encoding", "").lower()
        if "chunked" in te:
            chunks: list[bytes] = []
            while True:
                line = self.rfile.readline()
                if not line:
                    break
                token = line.strip().split(b";", 1)[0]
                if not token:
                    continue
                try:
                    size = int(token, 16)
                except ValueError as exc:
                    raise _MalformedBodyError(f"Invalid chunk size: {token!r}") from exc
                if size < 0:
                    raise _MalformedBodyError(f"Negative chunk size: {size}")
                if size == 0:
                    while True:
                        trailer = self.rfile.readline()
                        if not trailer or trailer in (b"\r\n", b"\n"):
                            break
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.read(2)
            return b"".join(chunks)

        cl = self.headers.get("Content-Length")
        if cl is not None:
            try:
                length = int(cl.strip())
            except ValueError as exc:
                raise _MalformedBodyError(f"Invalid Content-Length: {cl!r}") from exc
            if length < 0:
                raise _MalformedBodyError(f"Negative Content-Length: {length}")
            if length > 0:
                return self.rfile.read(length)
        return b""

    def _send_json(self, status: int, payload: Any, *, close: bool = False) -> None:
        raw = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        if close:
            self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(raw)

    def _send_stream_bytes(self, content_type: str, chunks: list[bytes]) -> None:
        payload = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _handle(self, method: str) -> None:
        owner = self.server.owner
        parsed = urllib.parse.urlsplit(self.path)
        req_headers = {k.lower(): v for k, v in self.headers.items()}
        try:
            raw_body = self._read_body()
        except _MalformedBodyError as exc:
            self.close_connection = True
            if parsed.path.startswith("/v1/"):
                payload: Any = {
                    "error": {
                        "message": str(exc),
                        "type": "invalid_request_error",
                        "param": None,
                        "code": "malformed_body",
                    }
                }
            else:
                payload = {"error": str(exc)}
            self._send_json(400, payload, close=True)
            owner._record_request(method, parsed.path, req_headers, None, None, 400)
            return
        owner._dispatch(self, method, parsed.path, req_headers, raw_body)

    def do_GET(self) -> None:  # noqa: N802
        self._handle("GET")

    def do_POST(self) -> None:  # noqa: N802
        self._handle("POST")


class AIMockServer:
    """Thread-safe local mock supporting Ollama and OpenAI Chat Completions APIs."""

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = 0,
        *,
        default_stream: bool = False,
    ) -> None:
        self.host = host
        self._requested_port = port
        self.default_stream = default_stream

        self._lock = threading.RLock()
        self._httpd: _AIHTTPServer | None = None
        self._thread: threading.Thread | None = None

        self._models: list[dict[str, Any]] = _default_models()
        self._slot_choice_override: int | None = None
        self._overrides: dict[str, str] = {}
        self._errors: list[_AIErrorRule] = []
        self._delay_sec: float = 0.0
        self._request_log: list[dict[str, Any]] = []

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self) -> AIMockServer:
        if self._httpd is not None:
            return self
        self._httpd = _AIHTTPServer((self.host, self._requested_port), _AIRequestHandler, self)
        self._thread = threading.Thread(
            target=self._httpd.serve_forever,
            name="vitral-ai-mock",
            daemon=True,
        )
        self._thread.start()
        return self

    def stop(self) -> None:
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()
            self._httpd = None
        if self._thread is not None:
            self._thread.join(timeout=5.0)
            self._thread = None

    def __enter__(self) -> AIMockServer:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()

    @property
    def port(self) -> int:
        if self._httpd is None:
            raise RuntimeError("AIMockServer is not started")
        return int(self._httpd.server_address[1])

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def ollama_url(self) -> str:
        return self.base_url

    @property
    def openai_url(self) -> str:
        return f"{self.base_url}/v1"

    @property
    def request_log(self) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(item) for item in self._request_log]

    def clear_request_log(self) -> None:
        with self._lock:
            self._request_log.clear()

    # ------------------------------------------------------------------
    # Control API
    # ------------------------------------------------------------------

    def set_slot_choice(self, idx: int | None) -> None:
        with self._lock:
            self._slot_choice_override = None if idx is None else int(idx)

    def set_response_override(self, key: str, response: str) -> None:
        with self._lock:
            self._overrides[key] = str(response)

    def clear_overrides(self) -> None:
        with self._lock:
            self._overrides.clear()
            self._slot_choice_override = None

    def inject_error(
        self,
        status_code: int = 500,
        message: str = "Injected AI fault",
        endpoint: str | None = None,
        once: bool = True,
        count: int | None = None,
    ) -> None:
        remaining = count if count is not None else (1 if once else None)
        with self._lock:
            self._errors.append(
                _AIErrorRule(
                    status_code=int(status_code),
                    message=message,
                    endpoint=endpoint,
                    remaining=remaining,
                )
            )

    def inject_delay(self, seconds: float) -> None:
        with self._lock:
            self._delay_sec = max(0.0, float(seconds))

    def set_models(self, models: list[dict[str, Any] | str]) -> None:
        normalized: list[dict[str, Any]] = []
        now_iso = "2026-11-20T14:00:00Z"
        for m in models:
            if isinstance(m, dict):
                normalized.append(dict(m))
            else:
                normalized.append(
                    {
                        "name": str(m),
                        "model": str(m),
                        "modified_at": now_iso,
                        "size": 1024000000,
                        "digest": f"sha256:{secrets.token_hex(32)}",
                        "details": {
                            "parent_model": "",
                            "format": "gguf",
                            "family": "vitral-mock",
                            "families": ["vitral-mock"],
                            "parameter_size": "7B",
                            "quantization_level": "Q4_K_M",
                        },
                    }
                )
        with self._lock:
            self._models = normalized

    def clear(self) -> None:
        with self._lock:
            self._slot_choice_override = None
            self._overrides.clear()
            self._errors.clear()
            self._delay_sec = 0.0
            self._request_log.clear()
            self._models = _default_models()

    # ------------------------------------------------------------------
    # Routing & Response Generation
    # ------------------------------------------------------------------

    def _pop_error(self, path: str) -> tuple[int, str] | None:
        with self._lock:
            for idx, rule in enumerate(self._errors):
                if rule.endpoint is not None and rule.endpoint != path:
                    continue
                res = (rule.status_code, rule.message)
                if rule.remaining is not None:
                    rule.remaining -= 1
                    if rule.remaining <= 0:
                        self._errors.pop(idx)
                return res
        return None

    def _record_request(
        self,
        method: str,
        path: str,
        headers: dict[str, str],
        body: Any,
        detected_mode: str | None,
        status_code: int,
    ) -> None:
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with self._lock:
            self._request_log.append(
                {
                    "timestamp_utc": ts,
                    "method": method,
                    "path": path,
                    "headers": headers,
                    "body": body,
                    "detected_mode": detected_mode,
                    "status_code": status_code,
                }
            )

    def _route_prompt(
        self,
        text: str,
        *,
        format_spec: Any = None,
    ) -> tuple[str, str]:
        lowered = text.lower()
        is_json_format = False
        if format_spec == "json" or isinstance(format_spec, dict):
            if isinstance(format_spec, dict):
                rf_type = format_spec.get("type")
                if rf_type in ("json_object", "json_schema") or "properties" in format_spec:
                    is_json_format = True
            else:
                is_json_format = True

        slot_markers = ("slot_index", "槽位", "混淆", "candidate_slots", "disambiguate")
        translate_markers = ("translate", "翻译", "边写边译", "->en", "->ja", "->de")
        polish_markers = ("polish", "润色", "纠错", "口语", "书面语")
        summary_markers = ("summarize", "summary", "提炼", "纪要", "录音", "transcript")

        if is_json_format or any(m in lowered for m in slot_markers):
            mode = "slot_choice"
        elif any(m in lowered for m in translate_markers):
            mode = "translate"
        elif any(m in lowered for m in polish_markers):
            mode = "polish"
        elif any(m in lowered for m in summary_markers):
            mode = "summary"
        else:
            mode = "general"

        with self._lock:
            overrides = dict(self._overrides)
            slot_override = self._slot_choice_override

        # Check explicit overrides before heuristic generation
        if mode in overrides:
            return mode, overrides[mode]
        for key, val in overrides.items():
            if key == "*" or key in text:
                return mode, val

        if mode == "slot_choice":
            if slot_override is not None:
                idx = slot_override
            elif ("老登" in text or "金币" in text) and "爆" in text:
                idx = 1
            else:
                idx = 0
            return mode, json.dumps({"slot_index": idx}, separators=(",", ":"))

        if mode == "translate":
            return mode, _render_translation(text, lowered)

        if mode == "polish":
            return mode, _render_polish(text)

        if mode == "summary":
            return mode, _render_summary(text)

        return mode, "Vitral AI mock response."

    def _dispatch(
        self,
        handler: _AIRequestHandler,
        method: str,
        path: str,
        headers: dict[str, str],
        raw_body: bytes,
    ) -> None:
        with self._lock:
            delay = self._delay_sec
        if delay > 0:
            time.sleep(delay)

        err = self._pop_error(path)
        if err is not None:
            status_code, msg = err
            if path.startswith("/v1/"):
                payload: Any = {
                    "error": {
                        "message": msg,
                        "type": "mock_error",
                        "param": None,
                        "code": str(status_code),
                    }
                }
            else:
                payload = {"error": msg}
            handler._send_json(status_code, payload)
            self._record_request(method, path, headers, None, None, status_code)
            return

        if path == "/api/tags" and method == "GET":
            with self._lock:
                models = [dict(m) for m in self._models]
            handler._send_json(200, {"models": models})
            self._record_request(method, path, headers, None, "tags", 200)
            return

        if method != "POST":
            handler._send_json(405, {"error": "Method Not Allowed"})
            self._record_request(method, path, headers, None, None, 405)
            return

        try:
            body = json.loads(raw_body.decode("utf-8")) if raw_body else {}
        except (UnicodeDecodeError, json.JSONDecodeError):
            if path.startswith("/v1/"):
                handler._send_json(
                    400,
                    {"error": {"message": "Invalid JSON body", "type": "invalid_request_error", "param": None, "code": "400"}},
                )
            else:
                handler._send_json(400, {"error": "Invalid JSON body"})
            self._record_request(method, path, headers, None, None, 400)
            return

        if not isinstance(body, dict):
            if path.startswith("/v1/"):
                handler._send_json(
                    400,
                    {
                        "error": {
                            "message": "JSON body must be an object",
                            "type": "invalid_request_error",
                            "param": None,
                            "code": "invalid_json",
                        }
                    },
                )
            else:
                handler._send_json(400, {"error": "JSON body must be an object"})
            self._record_request(method, path, headers, body, None, 400)
            return

        if path == "/api/chat":
            self._handle_ollama_chat(handler, headers, body)
            return
        if path == "/api/generate":
            self._handle_ollama_generate(handler, headers, body)
            return
        if path == "/v1/chat/completions":
            self._handle_openai_chat(handler, headers, body)
            return

        handler._send_json(404, {"error": f"Unknown endpoint: {path}"})
        self._record_request(method, path, headers, body, None, 404)

    def _handle_ollama_chat(
        self,
        handler: _AIRequestHandler,
        headers: dict[str, str],
        body: dict[str, Any],
    ) -> None:
        messages = body.get("messages")
        if not isinstance(messages, list):
            handler._send_json(400, {"error": "'messages' must be a list"})
            self._record_request("POST", "/api/chat", headers, body, None, 400)
            return

        model = str(body.get("model") or "vitral-mock:latest")
        prompt_text = "\n".join(
            str(m.get("content", "")) for m in messages if isinstance(m, dict)
        )
        mode, content = self._route_prompt(prompt_text, format_spec=body.get("format"))
        stream = bool(body.get("stream", self.default_stream))
        created_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

        if stream:
            mid = max(1, len(content) // 2)
            parts = [content[:mid], content[mid:]] if len(content) > 1 else [content]
            lines: list[bytes] = []
            for p in parts:
                if not p:
                    continue
                chunk = {
                    "model": model,
                    "created_at": created_at,
                    "message": {"role": "assistant", "content": p},
                    "done": False,
                }
                lines.append((json.dumps(chunk, ensure_ascii=False) + "\n").encode("utf-8"))
            final = {
                "model": model,
                "created_at": created_at,
                "message": {"role": "assistant", "content": ""},
                "done_reason": "stop",
                "done": True,
                "total_duration": 12000000,
                "load_duration": 1000000,
                "prompt_eval_count": max(1, len(prompt_text) // 4),
                "prompt_eval_duration": 3000000,
                "eval_count": max(1, len(content) // 4),
                "eval_duration": 8000000,
            }
            lines.append((json.dumps(final, ensure_ascii=False) + "\n").encode("utf-8"))
            handler._send_stream_bytes("application/x-ndjson", lines)
        else:
            payload = {
                "model": model,
                "created_at": created_at,
                "message": {"role": "assistant", "content": content},
                "done_reason": "stop",
                "done": True,
                "total_duration": 12000000,
                "load_duration": 1000000,
                "prompt_eval_count": max(1, len(prompt_text) // 4),
                "prompt_eval_duration": 3000000,
                "eval_count": max(1, len(content) // 4),
                "eval_duration": 8000000,
            }
            handler._send_json(200, payload)
        self._record_request("POST", "/api/chat", headers, body, mode, 200)

    def _handle_ollama_generate(
        self,
        handler: _AIRequestHandler,
        headers: dict[str, str],
        body: dict[str, Any],
    ) -> None:
        model = body.get("model")
        if not isinstance(model, str) or not model.strip():
            handler._send_json(400, {"error": "'model' is required"})
            self._record_request("POST", "/api/generate", headers, body, None, 400)
            return

        system_text = str(body.get("system") or "")
        prompt_raw = str(body.get("prompt") or "")
        combined = f"{system_text}\n{prompt_raw}".strip()
        mode, content = self._route_prompt(combined, format_spec=body.get("format"))
        stream = bool(body.get("stream", self.default_stream))
        created_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

        if stream:
            mid = max(1, len(content) // 2)
            parts = [content[:mid], content[mid:]] if len(content) > 1 else [content]
            lines: list[bytes] = []
            for p in parts:
                if not p:
                    continue
                chunk = {
                    "model": model,
                    "created_at": created_at,
                    "response": p,
                    "done": False,
                }
                lines.append((json.dumps(chunk, ensure_ascii=False) + "\n").encode("utf-8"))
            final = {
                "model": model,
                "created_at": created_at,
                "response": "",
                "done": True,
                "done_reason": "stop",
                "context": [1, 2, 3],
                "total_duration": 10000000,
                "load_duration": 1000000,
                "prompt_eval_count": max(1, len(combined) // 4),
                "prompt_eval_duration": 2000000,
                "eval_count": max(1, len(content) // 4),
                "eval_duration": 7000000,
            }
            lines.append((json.dumps(final, ensure_ascii=False) + "\n").encode("utf-8"))
            handler._send_stream_bytes("application/x-ndjson", lines)
        else:
            payload = {
                "model": model,
                "created_at": created_at,
                "response": content,
                "done": True,
                "done_reason": "stop",
                "context": [1, 2, 3],
                "total_duration": 10000000,
                "load_duration": 1000000,
                "prompt_eval_count": max(1, len(combined) // 4),
                "prompt_eval_duration": 2000000,
                "eval_count": max(1, len(content) // 4),
                "eval_duration": 7000000,
            }
            handler._send_json(200, payload)
        self._record_request("POST", "/api/generate", headers, body, mode, 200)

    def _handle_openai_chat(
        self,
        handler: _AIRequestHandler,
        headers: dict[str, str],
        body: dict[str, Any],
    ) -> None:
        messages = body.get("messages")
        if not isinstance(messages, list):
            handler._send_json(
                400,
                {
                    "error": {
                        "message": "'messages' must be a list",
                        "type": "invalid_request_error",
                        "param": "messages",
                        "code": "invalid_messages",
                    }
                },
            )
            self._record_request("POST", "/v1/chat/completions", headers, body, None, 400)
            return

        model = str(body.get("model") or "vitral-mock")
        prompt_text = "\n".join(
            str(m.get("content", "")) for m in messages if isinstance(m, dict)
        )
        mode, content = self._route_prompt(prompt_text, format_spec=body.get("response_format"))
        stream_val = body.get("stream")
        stream = self.default_stream if stream_val is None else bool(stream_val)
        created = int(time.time())
        cmpl_id = f"chatcmpl-{secrets.token_hex(8)}"

        if stream:
            mid = max(1, len(content) // 2)
            parts = [content[:mid], content[mid:]] if len(content) > 1 else [content]
            sse_chunks: list[bytes] = []
            first = {
                "id": cmpl_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}],
            }
            sse_chunks.append(f"data: {json.dumps(first, ensure_ascii=False)}\n\n".encode("utf-8"))
            for p in parts:
                if not p:
                    continue
                delta = {
                    "id": cmpl_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {"content": p}, "finish_reason": None}],
                }
                sse_chunks.append(f"data: {json.dumps(delta, ensure_ascii=False)}\n\n".encode("utf-8"))
            end_chunk = {
                "id": cmpl_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            }
            sse_chunks.append(f"data: {json.dumps(end_chunk, ensure_ascii=False)}\n\n".encode("utf-8"))
            sse_chunks.append(b"data: [DONE]\n\n")
            handler._send_stream_bytes("text/event-stream; charset=utf-8", sse_chunks)
        else:
            prompt_tokens = max(1, len(prompt_text) // 4)
            completion_tokens = max(1, len(content) // 4)
            payload = {
                "id": cmpl_id,
                "object": "chat.completion",
                "created": created,
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": content, "refusal": None},
                        "logprobs": None,
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                },
            }
            handler._send_json(200, payload)
        self._record_request("POST", "/v1/chat/completions", headers, body, mode, 200)


def _detect_target_lang(lowered: str) -> str:
    if any(tok in lowered for tok in ("->ja", "日语", "日本語", "japanese", "to ja", "into ja", "lang=ja", "lang: ja")):
        return "JA"
    if any(tok in lowered for tok in ("->de", "德语", "deutsch", "german", "to de", "into de", "lang=de", "lang: de")):
        return "DE"
    return "EN"


def _render_translation(text: str, lowered: str) -> str:
    lang = _detect_target_lang(lowered)
    if any(k in text for k in ("毛玻璃", "极简", "冗余", "输入法")):
        if lang == "JA":
            return "透明感のあるすりガラス風ミニマルデザインで、余計な機能を一切排除。"
        if lang == "DE":
            return "Minimalistisches Milchglas-Design, völlig frei von unnötigem Ballast."
        return "Built with airy frosted-glass minimalism, completely free of feature bloat."

    if lang == "JA":
        return "午後の会議スケジュールをご確認ください。よろしくお願いします。"
    if lang == "DE":
        return "Bitte bestaetigen Sie den Besprechungstermin fuer heute Nachmittag."
    return "Please confirm the meeting schedule for this afternoon."


def _render_polish(text: str) -> str:
    if any(k in text for k in ("毛玻璃", "极简", "冗余", "输入法")):
        return "本输入法采用极简晶透毛玻璃设计，摒弃一切冗余功能。"

    # Extract trailing Chinese/source line if prompt has instructions on first line
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    candidate = lines[-1] if lines else text.strip()
    candidate = re.sub(r"^(请)?(将)?(以下)?(口语|内容|句子)?(纠错|润色|整理)?[:：]?\s*", "", candidate)
    for filler in ("那个", "就是说", "就是", "然后", "嗯", "啊", "吧", "呢"):
        candidate = candidate.replace(filler, "")
    candidate = re.sub(r"[，,]{2,}", "，", candidate)
    candidate = re.sub(r"\s+", " ", candidate).strip(" ，,")
    if candidate and not candidate.endswith(("。", "！", "？", ".", "!", "?")):
        candidate += "。"
    return candidate or "已完成书面语润色整理。"


def _strip_summary_instruction_prefix(segment: str) -> str:
    s = segment.strip()
    # Strip explicit instruction prefixes followed by colon/punctuation
    s = re.sub(
        r"^(?:请)?(?:帮我)?(?:总结|提炼|生成|整理)?(?:以下|这段)?(?:语音|会议|录音)?(?:转写)?(?:要点|纪要|摘要|总结)?\s*[:：]\s*",
        "",
        s,
    )
    s = re.sub(
        r"^(?:please\s+)?(?:summarize|summary(?:\s+of)?)(?:\s+the)?(?:\s+following)?(?:\s+(?:transcript|recording|meeting|notes|summary))?\s*[:：]\s*",
        "",
        s,
        flags=re.IGNORECASE,
    )
    # Drop pure instruction-only lines without content
    if re.fullmatch(
        r"(?:请)?(?:帮我)?(?:总结|提炼|生成|整理)(?:以下|这段)?(?:语音|会议|录音)?(?:转写)?(?:要点|纪要|摘要|总结)?",
        s,
    ):
        return ""
    if re.fullmatch(
        r"(?:please\s+)?(?:summarize|make\s+a\s+summary\s+of)(?:\s+the)?(?:\s+following)?(?:\s+(?:transcript|recording|meeting|notes|audio))?\.?",
        s,
        flags=re.IGNORECASE,
    ):
        return ""
    return s.strip()


def _render_summary(text: str) -> str:
    if any(k in text for k in ("毛玻璃", "剪贴板", "互传", "阅后即焚")):
        return (
            "1. 锁定晶透毛玻璃材质与圆润胶囊工具栏；\n"
            "2. 剪贴板执行 N 条环形持久化 + 图钉置顶保护，互传文件阅后即焚。"
        )
    filtered: list[str] = []
    for raw_line in text.splitlines():
        cleaned_line = _strip_summary_instruction_prefix(raw_line)
        if not cleaned_line:
            continue
        for seg in re.split(r"[。；;]+", cleaned_line):
            cleaned_seg = _strip_summary_instruction_prefix(seg)
            if cleaned_seg:
                filtered.append(cleaned_seg)
    if not filtered:
        return "1. 完成语音录音转写与要点提炼。"
    if len(filtered) == 1:
        return f"1. {filtered[0]}。"
    return f"1. {filtered[0]}；\n2. {filtered[1]}。"
