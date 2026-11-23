"""Loopback LocalSend v2 HTTP + UDP discovery mock peer for Vitral E2E tests."""

from __future__ import annotations

import hashlib
import json
import secrets
import socket
import threading
import urllib.parse
from dataclasses import dataclass
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class _MalformedBodyError(ValueError):
    """Raised when an HTTP request body has invalid framing or headers."""


@dataclass
class _RejectionRule:
    status_code: int
    message: str
    endpoints: set[str] | None
    remaining: int


class _LocalSendHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        handler_cls: type[BaseHTTPRequestHandler],
        owner: LocalSendMockPeer,
    ) -> None:
        self.owner = owner
        super().__init__(server_address, handler_cls)


class _LocalSendRequestHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: _LocalSendHTTPServer

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

    def _send_empty(self, status: int) -> None:
        self.send_response(status)
        if status not in (204, 304):
            self.send_header("Content-Length", "0")
        self.end_headers()

    def _handle(self, method: str) -> None:
        owner = self.server.owner
        parsed = urllib.parse.urlsplit(self.path)
        query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
        req_headers = {k.lower(): v for k, v in self.headers.items()}
        try:
            body = self._read_body()
        except _MalformedBodyError as exc:
            self.close_connection = True
            self._send_json(400, {"message": str(exc)}, close=True)
            owner._record_request(method, parsed.path, parsed.query, req_headers, 0, 400)
            return
        status = owner._dispatch_http(self, method, parsed.path, query, req_headers, body)
        owner._record_request(method, parsed.path, parsed.query, req_headers, len(body), status)

    def do_GET(self) -> None:  # noqa: N802
        self._handle("GET")

    def do_POST(self) -> None:  # noqa: N802
        self._handle("POST")


class LocalSendMockPeer:
    """Thread-safe LocalSend v2 HTTP REST + UDP loopback mock peer."""

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = 0,
        *,
        alias: str = "Vitral-Mock-Peer",
        version: str = "2.1",
        device_model: str = "Vitral-Test-Device",
        device_type: str = "desktop",
        fingerprint: str | None = None,
        enable_udp: bool = True,
        udp_bind_host: str = "127.0.0.1",
        udp_port: int = 0,
    ) -> None:
        self.host = host
        self._requested_port = port
        self.alias = alias
        self.version = version
        self.device_model = device_model
        self.device_type = device_type
        self.fingerprint = fingerprint or secrets.token_hex(16)
        self.enable_udp = enable_udp
        self.udp_bind_host = udp_bind_host
        self._requested_udp_port = udp_port

        self._lock = threading.RLock()
        self._httpd: _LocalSendHTTPServer | None = None
        self._http_thread: threading.Thread | None = None
        self._udp_sock: socket.socket | None = None
        self._udp_thread: threading.Thread | None = None
        self._udp_stop = threading.Event()

        self._pin: str | None = None
        self._busy: bool = False
        self._rejections: list[_RejectionRule] = []
        self._registered_peers: dict[str, dict[str, Any]] = {}
        self._sessions: dict[str, dict[str, Any]] = {}
        self._received_transfers: list[dict[str, Any]] = []
        self._request_log: list[dict[str, Any]] = []
        self._discovered_peers: list[dict[str, Any]] = []

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self) -> LocalSendMockPeer:
        if self._httpd is not None:
            return self
        self._httpd = _LocalSendHTTPServer(
            (self.host, self._requested_port),
            _LocalSendRequestHandler,
            self,
        )
        self._http_thread = threading.Thread(
            target=self._httpd.serve_forever,
            name="vitral-localsend-http",
            daemon=True,
        )
        self._http_thread.start()

        if self.enable_udp:
            self._udp_stop.clear()
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind((self.udp_bind_host, self._requested_udp_port))
            sock.settimeout(0.2)
            self._udp_sock = sock
            self._udp_thread = threading.Thread(
                target=self._udp_listen_loop,
                name="vitral-localsend-udp",
                daemon=True,
            )
            self._udp_thread.start()
        return self

    def stop(self) -> None:
        self._udp_stop.set()
        if self._udp_sock is not None:
            try:
                self._udp_sock.close()
            except OSError:
                pass
            self._udp_sock = None
        if self._udp_thread is not None:
            self._udp_thread.join(timeout=3.0)
            self._udp_thread = None

        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()
            self._httpd = None
        if self._http_thread is not None:
            self._http_thread.join(timeout=5.0)
            self._http_thread = None

    def __enter__(self) -> LocalSendMockPeer:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()

    @property
    def port(self) -> int:
        if self._httpd is None:
            raise RuntimeError("LocalSendMockPeer is not started")
        return int(self._httpd.server_address[1])

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def udp_port(self) -> int | None:
        if self._udp_sock is None:
            return None
        return int(self._udp_sock.getsockname()[1])

    @property
    def device_info(self) -> dict[str, Any]:
        port_val = self._httpd.server_address[1] if self._httpd is not None else self._requested_port
        return {
            "alias": self.alias,
            "version": self.version,
            "deviceModel": self.device_model,
            "deviceType": self.device_type,
            "fingerprint": self.fingerprint,
            "port": int(port_val),
            "protocol": "http",
            "download": False,
        }

    # ------------------------------------------------------------------
    # State Inspection & Control API
    # ------------------------------------------------------------------

    @property
    def registered_peers(self) -> dict[str, dict[str, Any]]:
        with self._lock:
            return {k: dict(v) for k, v in self._registered_peers.items()}

    @property
    def received_transfers(self) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(item) for item in self._received_transfers]

    @property
    def request_log(self) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(item) for item in self._request_log]

    @property
    def discovered_peers(self) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(item) for item in self._discovered_peers]

    def clear_request_log(self) -> None:
        with self._lock:
            self._request_log.clear()

    def reject_next(
        self,
        status_code: int = 403,
        message: str = "Rejected by mock peer",
        endpoints: set[str] | None = None,
        count: int = 1,
    ) -> None:
        target_eps = {"prepare-upload"} if endpoints is None else {e.strip("/") for e in endpoints}
        with self._lock:
            self._rejections.append(
                _RejectionRule(
                    status_code=int(status_code),
                    message=message,
                    endpoints=target_eps,
                    remaining=int(count),
                )
            )

    def set_pin(self, pin: str | None) -> None:
        with self._lock:
            self._pin = pin

    def set_busy(self, busy: bool) -> None:
        with self._lock:
            self._busy = bool(busy)

    def clear(self) -> None:
        with self._lock:
            self._received_transfers.clear()
            self._registered_peers.clear()
            self._sessions.clear()
            self._request_log.clear()
            self._discovered_peers.clear()
            self._rejections.clear()
            self._busy = False

    # ------------------------------------------------------------------
    # UDP Discovery
    # ------------------------------------------------------------------

    def emit_udp_announcement(self, target_host: str, target_port: int) -> None:
        payload = dict(self.device_info)
        payload["announce"] = True
        raw = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        sock = self._udp_sock
        if sock is not None:
            sock.sendto(raw, (target_host, int(target_port)))
            return
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as tmp:
            tmp.sendto(raw, (target_host, int(target_port)))

    def emit_announcement(self, target_host: str, target_port: int) -> None:
        self.emit_udp_announcement(target_host, target_port)

    def _udp_listen_loop(self) -> None:
        while not self._udp_stop.is_set():
            sock = self._udp_sock
            if sock is None:
                break
            try:
                data, addr = sock.recvfrom(65535)
            except socket.timeout:
                continue
            except OSError:
                break
            try:
                msg = json.loads(data.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if not isinstance(msg, dict):
                continue
            fp = msg.get("fingerprint")
            if not isinstance(fp, str) or not fp or fp == self.fingerprint:
                continue
            entry = dict(msg)
            entry["_source_addr"] = f"{addr[0]}:{addr[1]}"
            with self._lock:
                self._discovered_peers = [
                    p for p in self._discovered_peers if p.get("fingerprint") != fp
                ]
                self._discovered_peers.append(entry)

    # ------------------------------------------------------------------
    # HTTP Dispatch
    # ------------------------------------------------------------------

    def _pop_rejection(self, endpoint: str) -> tuple[int, str] | None:
        with self._lock:
            for idx, rule in enumerate(self._rejections):
                if rule.endpoints is not None and endpoint not in rule.endpoints and "*" not in rule.endpoints:
                    continue
                res = (rule.status_code, rule.message)
                rule.remaining -= 1
                if rule.remaining <= 0:
                    self._rejections.pop(idx)
                return res
        return None

    def _record_request(
        self,
        method: str,
        path: str,
        query: str,
        headers: dict[str, str],
        body_len: int,
        status_code: int,
    ) -> None:
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with self._lock:
            self._request_log.append(
                {
                    "timestamp_utc": ts,
                    "method": method,
                    "path": path,
                    "query": query,
                    "headers": headers,
                    "body_len": body_len,
                    "status_code": status_code,
                }
            )

    def _dispatch_http(
        self,
        handler: _LocalSendRequestHandler,
        method: str,
        path: str,
        query: dict[str, list[str]],
        headers: dict[str, str],
        body: bytes,
    ) -> int:
        prefix = "/api/localsend/v2/"
        if not path.startswith(prefix):
            handler._send_json(404, {"message": "Unknown route"})
            return 404
        endpoint = path[len(prefix) :].strip("/")

        rej = self._pop_rejection(endpoint)
        if rej is not None:
            status_code, msg = rej
            handler._send_json(status_code, {"message": msg})
            return status_code

        if endpoint == "info" and method == "GET":
            info = {
                "alias": self.alias,
                "version": self.version,
                "deviceModel": self.device_model,
                "deviceType": self.device_type,
                "fingerprint": self.fingerprint,
                "download": False,
            }
            handler._send_json(200, info)
            return 200

        if endpoint == "register" and method == "POST":
            try:
                data = json.loads(body.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                handler._send_json(400, {"message": "Invalid JSON"})
                return 400
            if not isinstance(data, dict) or not data.get("fingerprint") or not data.get("alias"):
                handler._send_json(400, {"message": "Missing alias or fingerprint"})
                return 400
            with self._lock:
                self._registered_peers[str(data["fingerprint"])] = dict(data)
            handler._send_json(200, self.device_info)
            return 200

        if endpoint == "prepare-upload" and method == "POST":
            with self._lock:
                pin_required = self._pin
                is_busy = self._busy
            if is_busy:
                handler._send_json(409, {"message": "Blocked by another session"})
                return 409
            if pin_required is not None:
                if "pin" not in query or not query["pin"] or query["pin"][0] != pin_required:
                    handler._send_json(401, {"message": "Invalid or missing PIN"})
                    return 401
            try:
                data = json.loads(body.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                handler._send_json(400, {"message": "Invalid JSON"})
                return 400
            if (
                not isinstance(data, dict)
                or not isinstance(data.get("info"), dict)
                or not isinstance(data.get("files"), dict)
            ):
                handler._send_json(400, {"message": "Missing info or files"})
                return 400

            files_map: dict[str, Any] = data["files"]
            if len(files_map) == 0:
                handler._send_empty(204)
                return 204

            session_id = secrets.token_hex(12)
            tokens: dict[str, str] = {}
            normalized_files: dict[str, dict[str, Any]] = {}
            for file_id, meta in files_map.items():
                if not isinstance(meta, dict):
                    handler._send_json(400, {"message": f"Invalid file metadata for {file_id}"})
                    return 400
                size_val = meta.get("size")
                if not isinstance(size_val, int) or isinstance(size_val, bool) or size_val < 0:
                    handler._send_json(400, {"message": f"Invalid file size for {file_id}"})
                    return 400
                tokens[str(file_id)] = secrets.token_hex(16)
                normalized_files[str(file_id)] = dict(meta)

            with self._lock:
                self._sessions[session_id] = {
                    "id": session_id,
                    "sender_info": dict(data["info"]),
                    "files": normalized_files,
                    "tokens": tokens,
                    "status": "active",
                    "uploaded_files": set(),
                }
            handler._send_json(200, {"sessionId": session_id, "files": tokens})
            return 200

        if endpoint == "upload" and method == "POST":
            session_id = (query.get("sessionId") or [""])[0]
            file_id = (query.get("fileId") or [""])[0]
            token = (query.get("token") or [""])[0]
            if not session_id or not file_id or not token:
                handler._send_json(400, {"message": "Missing sessionId, fileId, or token"})
                return 400

            with self._lock:
                session = self._sessions.get(session_id)
                if session is None or session["status"] != "active":
                    handler._send_json(403, {"message": "Invalid or cancelled session"})
                    return 403
                expected_token = session["tokens"].get(file_id)
                if expected_token is None or expected_token != token:
                    handler._send_json(403, {"message": "Invalid file token"})
                    return 403
                if file_id in session["uploaded_files"]:
                    handler._send_json(
                        409,
                        {"message": f"File {file_id} already uploaded in session {session_id}"},
                    )
                    return 409
                file_meta = dict(session["files"][file_id])
                sender_info = dict(session["sender_info"])

            expected_size = file_meta.get("size")
            if expected_size is not None:
                if isinstance(expected_size, bool):
                    handler._send_json(400, {"message": "Invalid expected file size"})
                    return 400
                try:
                    expected_int = int(expected_size)
                except (TypeError, ValueError):
                    handler._send_json(400, {"message": "Invalid expected file size"})
                    return 400
                if expected_int < 0 or len(body) != expected_int:
                    handler._send_json(
                        400,
                        {"message": f"Size mismatch: expected {expected_size}, got {len(body)}"},
                    )
                    return 400

            sha256_actual = hashlib.sha256(body).hexdigest()
            sha256_expected = file_meta.get("sha256")
            if isinstance(sha256_expected, str) and sha256_expected.strip():
                if sha256_actual.lower() != sha256_expected.strip().lower():
                    handler._send_json(
                        422,
                        {
                            "message": "SHA-256 verification failed",
                            "expected": sha256_expected,
                            "actual": sha256_actual,
                        },
                    )
                    return 422

            with self._lock:
                session = self._sessions.get(session_id)
                if session is None or session["status"] != "active":
                    handler._send_json(403, {"message": "Session cancelled during upload"})
                    return 403
                if file_id in session["uploaded_files"]:
                    handler._send_json(
                        409,
                        {"message": f"File {file_id} already uploaded in session {session_id}"},
                    )
                    return 409
                session["uploaded_files"].add(file_id)
                if len(session["uploaded_files"]) >= len(session["files"]):
                    session["status"] = "completed"
                self._received_transfers.append(
                    {
                        "session_id": session_id,
                        "file_id": file_id,
                        "file_name": file_meta.get("fileName", file_id),
                        "file_type": file_meta.get("fileType", "application/octet-stream"),
                        "size": len(body),
                        "expected_size": expected_size,
                        "sha256_actual": sha256_actual,
                        "sha256_expected": sha256_expected,
                        "data": bytes(body),
                        "sender_info": sender_info,
                        "received_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                    }
                )
            handler._send_empty(200)
            return 200

        if endpoint == "cancel" and method == "POST":
            session_id = (query.get("sessionId") or [""])[0]
            if not session_id:
                handler._send_json(400, {"message": "Missing sessionId"})
                return 400
            with self._lock:
                session = self._sessions.get(session_id)
                if session is not None:
                    session["status"] = "cancelled"
            handler._send_empty(200)
            return 200

        handler._send_json(405, {"message": "Method Not Allowed"})
        return 405
