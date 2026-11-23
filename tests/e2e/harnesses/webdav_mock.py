"""Local RFC 4918 + Apache OpenDAL-compatible WebDAV mock server for Vitral E2E tests."""

from __future__ import annotations

import copy
import email.utils
import hashlib
import math
import mimetypes
import posixpath
import threading
import time
import urllib.parse
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from xml.sax.saxutils import escape as xml_escape

DEFAULT_SEEDED_COLLECTIONS = (
    "/dav",
    "/dav/Koofr",
    "/dav/Koofr/Vitral",
    "/dav/Koofr/Vitral/lexicon",
    "/dav/Koofr/Vitral/lexicon/crdt",
    "/dav/Koofr/Vitral/lexicon/imports",
    "/dav/Koofr/Vitral/profile",
    "/dav/Koofr/Vitral/clipboard",
    "/dav/Koofr/Vitral/relay",
)

DAV_ALLOWED_METHODS = (
    "OPTIONS, PROPFIND, PROPPATCH, GET, HEAD, PUT, DELETE, MKCOL, COPY, MOVE"
)


@dataclass
class _Node:
    is_dir: bool
    data: bytes = b""
    mtime: float = field(default_factory=time.time)
    content_type: str = "application/octet-stream"
    etag: str = ""
    dead_props: dict[tuple[str, str], str] = field(default_factory=dict)

    def refresh_etag(self, path: str) -> None:
        h = hashlib.sha256()
        h.update(path.encode("utf-8"))
        h.update(b":dir:" if self.is_dir else b":file:")
        h.update(str(self.mtime).encode("ascii"))
        if not self.is_dir:
            h.update(self.data)
        self.etag = f'"{h.hexdigest()[:24]}"'


@dataclass
class _ErrorRule:
    method: str
    status: int
    remaining: int | None
    path_prefix: str | None
    body: bytes


def _normalize_dav_path(raw_path: str) -> str:
    """Normalize and validate an HTTP path; raises ValueError on traversal."""
    path_only = raw_path.split("?", 1)[0]
    decoded = urllib.parse.unquote(path_only)
    if "\x00" in decoded:
        raise ValueError("Null byte in path")
    if not decoded.startswith("/"):
        decoded = "/" + decoded
    parts = decoded.split("/")
    if ".." in parts:
        raise ValueError("Path traversal not allowed")
    norm = posixpath.normpath(decoded)
    if norm.startswith("//"):
        norm = "/" + norm.lstrip("/")
    if norm != "/" and norm.endswith("/"):
        norm = norm.rstrip("/")
    return norm


class _MalformedBodyError(ValueError):
    """Raised when an HTTP request body has invalid framing or headers."""


def _validate_epoch_mtime(epoch_sec: float | int) -> float:
    """Validate that an epoch timestamp is finite and RFC-1123 formattable."""
    if isinstance(epoch_sec, bool):
        raise ValueError("Boolean is not a valid mtime")
    val = float(epoch_sec)
    if not math.isfinite(val):
        raise ValueError(f"Non-finite mtime: {epoch_sec!r}")
    try:
        dt = datetime.fromtimestamp(val, timezone.utc)
        if not (0 <= dt.year <= 9999):
            raise ValueError(f"Year out of range: {dt.year}")
        email.utils.formatdate(val, usegmt=True)
    except (OverflowError, OSError, ValueError) as exc:
        raise ValueError(f"Out-of-range mtime: {epoch_sec!r}") from exc
    return val


def _format_rfc1123(epoch_sec: float) -> str:
    val = _validate_epoch_mtime(epoch_sec)
    return email.utils.formatdate(val, usegmt=True)


def _parse_http_date(value: str) -> float | None:
    try:
        dt = email.utils.parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    if dt is None:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.timestamp()


def _etag_matches(header_val: str, current_etag: str) -> bool:
    raw = header_val.strip()
    if raw == "*":
        return True
    candidates = [item.strip() for item in raw.split(",") if item.strip()]
    for cand in candidates:
        if cand == current_etag or cand.lstrip("W/") == current_etag.lstrip("W/"):
            return True
    return False


class _WebDAVHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, server_address: tuple[str, int], handler_cls: type[BaseHTTPRequestHandler], owner: WebDAVMockServer) -> None:
        self.owner = owner
        super().__init__(server_address, handler_cls)


class _WebDAVRequestHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: _WebDAVHTTPServer

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        return

    def _read_body(self) -> bytes:
        te = self.headers.get("Transfer-Encoding", "").lower()
        if "chunked" in te:
            chunks: list[bytes] = []
            while True:
                size_line = self.rfile.readline()
                if not size_line:
                    break
                size_token = size_line.strip().split(b";", 1)[0]
                if not size_token:
                    continue
                try:
                    chunk_size = int(size_token, 16)
                except ValueError as exc:
                    raise _MalformedBodyError(f"Invalid chunk size: {size_token!r}") from exc
                if chunk_size < 0:
                    raise _MalformedBodyError(f"Negative chunk size: {chunk_size}")
                if chunk_size == 0:
                    while True:
                        trailer = self.rfile.readline()
                        if not trailer or trailer in (b"\r\n", b"\n"):
                            break
                    break
                data = self.rfile.read(chunk_size)
                chunks.append(data)
                # consume trailing CRLF after chunk payload
                crlf = self.rfile.read(2)
                if crlf not in (b"\r\n", b""):
                    pass
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

    def _send_bytes(
        self,
        status: int,
        body: bytes = b"",
        *,
        headers: dict[str, str] | None = None,
        is_head: bool = False,
        omit_content_length_for_no_body: bool = False,
    ) -> None:
        self.send_response(status)
        if headers:
            for k, v in headers.items():
                self.send_header(k, v)
        if status in (204, 304) or omit_content_length_for_no_body:
            self.end_headers()
            return
        if is_head:
            self.end_headers()
            return
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _handle(self, method: str) -> None:
        owner = self.server.owner
        parsed = urllib.parse.urlsplit(self.path)
        req_headers = {k.lower(): v for k, v in self.headers.items()}
        try:
            body = self._read_body()
        except _MalformedBodyError as exc:
            self.close_connection = True
            self._send_bytes(
                400,
                str(exc).encode("utf-8"),
                headers={"Content-Type": "text/plain", "Connection": "close"},
            )
            owner._record_request(method, parsed.path, parsed.query, req_headers, 0, 400)
            return

        try:
            norm_path = _normalize_dav_path(parsed.path)
        except ValueError:
            status = 400
            self._send_bytes(status, b"Bad path", headers={"Content-Type": "text/plain"})
            owner._record_request(method, parsed.path, parsed.query, req_headers, len(body), status)
            return

        injected = owner._pop_injected_error(method, norm_path)
        if injected is not None:
            inj_status, inj_body = injected
            if inj_status in (204, 304):
                self._send_bytes(inj_status, b"")
            else:
                self._send_bytes(
                    inj_status,
                    inj_body,
                    headers={"Content-Type": "text/plain"},
                    is_head=(method == "HEAD"),
                )
            owner._record_request(method, norm_path, parsed.query, req_headers, len(body), inj_status)
            return

        status = owner._dispatch(self, method, norm_path, req_headers, body)
        owner._record_request(method, norm_path, parsed.query, req_headers, len(body), status)

    def do_OPTIONS(self) -> None:  # noqa: N802
        self._handle("OPTIONS")

    def do_PROPFIND(self) -> None:  # noqa: N802
        self._handle("PROPFIND")

    def do_PROPPATCH(self) -> None:  # noqa: N802
        self._handle("PROPPATCH")

    def do_GET(self) -> None:  # noqa: N802
        self._handle("GET")

    def do_HEAD(self) -> None:  # noqa: N802
        self._handle("HEAD")

    def do_PUT(self) -> None:  # noqa: N802
        self._handle("PUT")

    def do_DELETE(self) -> None:  # noqa: N802
        self._handle("DELETE")

    def do_MKCOL(self) -> None:  # noqa: N802
        self._handle("MKCOL")

    def do_MOVE(self) -> None:  # noqa: N802
        self._handle("MOVE")

    def do_COPY(self) -> None:  # noqa: N802
        self._handle("COPY")


class WebDAVMockServer:
    """Thread-safe in-memory WebDAV server tailored to Koofr & Apache OpenDAL."""

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = 0,
        root_path: str = "/dav/Koofr/Vitral",
        *,
        allow_depth_infinity: bool = True,
    ) -> None:
        self.host = host
        self._requested_port = port
        norm_root = _normalize_dav_path(root_path)
        self.root_path = norm_root
        self.allow_depth_infinity = allow_depth_infinity

        self._lock = threading.RLock()
        self._nodes: dict[str, _Node] = {}
        self._request_log: list[dict[str, Any]] = []
        self._error_rules: list[_ErrorRule] = []
        self._httpd: _WebDAVHTTPServer | None = None
        self._thread: threading.Thread | None = None

        self._seed_initial_tree()

    def _seed_initial_tree(self) -> None:
        now = time.time()
        with self._lock:
            self._nodes.clear()
            root_node = _Node(is_dir=True, mtime=now, content_type="httpd/unix-directory")
            root_node.refresh_etag("/")
            self._nodes["/"] = root_node

            # Seed default Koofr/Vitral layout plus configured root_path hierarchy
            paths_to_seed = list(DEFAULT_SEEDED_COLLECTIONS)
            if self.root_path not in paths_to_seed:
                parts = [p for p in self.root_path.split("/") if p]
                cur = ""
                for part in parts:
                    cur = f"{cur}/{part}"
                    paths_to_seed.append(cur)
                for sub in ("lexicon", "lexicon/crdt", "lexicon/imports", "profile", "clipboard", "relay"):
                    paths_to_seed.append(f"{self.root_path}/{sub}")

            for p in paths_to_seed:
                if p not in self._nodes:
                    node = _Node(is_dir=True, mtime=now, content_type="httpd/unix-directory")
                    node.refresh_etag(p)
                    self._nodes[p] = node

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start(self) -> WebDAVMockServer:
        if self._httpd is not None:
            return self
        self._httpd = _WebDAVHTTPServer((self.host, self._requested_port), _WebDAVRequestHandler, self)
        self._thread = threading.Thread(
            target=self._httpd.serve_forever,
            name="vitral-webdav-mock",
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

    def __enter__(self) -> WebDAVMockServer:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()

    @property
    def port(self) -> int:
        if self._httpd is None:
            raise RuntimeError("WebDAVMockServer is not started")
        return int(self._httpd.server_address[1])

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def koofr_endpoint(self) -> str:
        return f"{self.base_url}/dav/Koofr"

    @property
    def webdav_url(self) -> str:
        return f"{self.base_url}/dav/Koofr"

    @property
    def vitral_root_url(self) -> str:
        return f"{self.base_url}{self.root_path}"

    # ------------------------------------------------------------------
    # Test Control API
    # ------------------------------------------------------------------

    def _resolve_control_path(self, rel_or_abs: str) -> str:
        raw = rel_or_abs.strip()
        if not raw:
            return self.root_path
        norm = _normalize_dav_path(raw if raw.startswith("/") else f"{self.root_path}/{raw}")
        if norm == self.root_path or norm.startswith(self.root_path + "/"):
            return norm
        if norm == "/Koofr/Vitral" or norm.startswith("/Koofr/Vitral/"):
            suffix = norm[len("/Koofr/Vitral") :]
            return self.root_path + suffix
        if norm.startswith("/dav/"):
            return norm
        return _normalize_dav_path(f"{self.root_path}/{norm.lstrip('/')}")

    def set_mtime(self, rel_path: str, epoch_sec: float | int) -> None:
        ts = _validate_epoch_mtime(epoch_sec)
        target = self._resolve_control_path(rel_path)
        with self._lock:
            node = self._nodes.get(target)
            if node is None:
                raise FileNotFoundError(f"WebDAV path not found: {rel_path} -> {target}")
            node.mtime = ts
            node.refresh_etag(target)

    def get_mtime(self, rel_path: str) -> float:
        target = self._resolve_control_path(rel_path)
        with self._lock:
            node = self._nodes.get(target)
            if node is None:
                raise FileNotFoundError(f"WebDAV path not found: {rel_path} -> {target}")
            return float(node.mtime)

    @property
    def request_log(self) -> list[dict[str, Any]]:
        with self._lock:
            return [dict(item) for item in self._request_log]

    def clear_request_log(self) -> None:
        with self._lock:
            self._request_log.clear()

    def list_files(self, prefix: str = "") -> list[str]:
        norm_prefix = ""
        if prefix:
            resolved = self._resolve_control_path(prefix)
            if resolved == self.root_path:
                norm_prefix = ""
            elif resolved.startswith(self.root_path + "/"):
                norm_prefix = resolved[len(self.root_path) + 1 :]
            else:
                norm_prefix = prefix.lstrip("/")

        root_prefix = self.root_path + "/"
        with self._lock:
            out: list[str] = []
            for path, node in self._nodes.items():
                if node.is_dir:
                    continue
                if path.startswith(root_prefix):
                    rel = path[len(root_prefix) :]
                    if not norm_prefix or rel.startswith(norm_prefix):
                        out.append(rel)
            return sorted(out)

    def file_exists(self, rel_path: str) -> bool:
        target = self._resolve_control_path(rel_path)
        with self._lock:
            node = self._nodes.get(target)
            return node is not None and not node.is_dir

    def read_file_bytes(self, rel_path: str) -> bytes:
        target = self._resolve_control_path(rel_path)
        with self._lock:
            node = self._nodes.get(target)
            if node is None or node.is_dir:
                raise FileNotFoundError(f"WebDAV file not found: {rel_path} -> {target}")
            return bytes(node.data)

    def write_file_bytes(
        self,
        rel_path: str,
        data: bytes,
        mtime: float | None = None,
        content_type: str | None = None,
    ) -> None:
        ts = _validate_epoch_mtime(mtime) if mtime is not None else time.time()
        target = self._resolve_control_path(rel_path)
        ctype = content_type or mimetypes.guess_type(target)[0] or "application/octet-stream"
        with self._lock:
            parent = posixpath.dirname(target)
            self._ensure_ancestors(parent, ts)
            node = _Node(is_dir=False, data=bytes(data), mtime=ts, content_type=ctype)
            node.refresh_etag(target)
            self._nodes[target] = node

    def has_live_slot(self) -> bool:
        """Return True if any live_slot.enc file exists under /dav/Koofr/Vitral/."""
        root_prefix = self.root_path.rstrip("/") + "/"
        with self._lock:
            return any(
                (p.startswith(root_prefix) or p.startswith("/dav/Koofr/Vitral/"))
                and p.endswith("live_slot.enc")
                for p in self._nodes
            )

    def assert_no_live_slot(self) -> None:
        """Assert that live_slot.enc neither exists nor was ever uploaded (PROPOSAL.md v1.2)."""
        with self._lock:
            bad_nodes = [p for p in self._nodes if p.endswith("live_slot.enc")]
            bad_reqs = []
            for req in self._request_log:
                method = str(req.get("method", "")).upper()
                if method not in ("PUT", "MKCOL", "COPY", "MOVE"):
                    continue
                req_path = str(req.get("path", "")).rstrip("/")
                dest_raw = str(req.get("headers", {}).get("destination", ""))
                dest_path = urllib.parse.unquote(urllib.parse.urlsplit(dest_raw).path).rstrip("/")
                if req_path.endswith("live_slot.enc") or dest_path.endswith("live_slot.enc"):
                    bad_reqs.append(req)
        if bad_nodes or bad_reqs:
            raise AssertionError(
                f"Forbidden live_slot.enc detected on WebDAV mock: nodes={bad_nodes}, requests={bad_reqs}"
            )

    def _ensure_ancestors(self, dir_path: str, ts: float) -> None:
        if dir_path in self._nodes:
            return
        parts = [p for p in dir_path.split("/") if p]
        cur = ""
        for part in parts:
            cur = f"{cur}/{part}"
            if cur not in self._nodes:
                node = _Node(is_dir=True, mtime=ts, content_type="httpd/unix-directory")
                node.refresh_etag(cur)
                self._nodes[cur] = node

    def inject_error(
        self,
        method: str,
        status: int,
        count: int | None = 1,
        path_prefix: str | None = None,
        body: bytes | str = b"",
    ) -> None:
        raw_body = body.encode("utf-8") if isinstance(body, str) else bytes(body)
        norm_prefix: str | None = None
        if path_prefix is not None:
            norm_prefix = self._resolve_control_path(path_prefix)
        with self._lock:
            self._error_rules.append(
                _ErrorRule(
                    method=method.upper(),
                    status=int(status),
                    remaining=count,
                    path_prefix=norm_prefix,
                    body=raw_body,
                )
            )

    def clear_errors(self) -> None:
        with self._lock:
            self._error_rules.clear()

    def _pop_injected_error(self, method: str, norm_path: str) -> tuple[int, bytes] | None:
        m_upper = method.upper()
        with self._lock:
            for idx, rule in enumerate(self._error_rules):
                if rule.method not in ("*", m_upper):
                    continue
                if rule.path_prefix is not None and not (
                    norm_path == rule.path_prefix or norm_path.startswith(rule.path_prefix + "/")
                ):
                    continue
                result = (rule.status, rule.body)
                if rule.remaining is not None:
                    rule.remaining -= 1
                    if rule.remaining <= 0:
                        self._error_rules.pop(idx)
                return result
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

    # ------------------------------------------------------------------
    # HTTP/WebDAV Request Dispatch
    # ------------------------------------------------------------------

    def _dispatch(
        self,
        handler: _WebDAVRequestHandler,
        method: str,
        path: str,
        headers: dict[str, str],
        body: bytes,
    ) -> int:
        if method == "OPTIONS":
            handler._send_bytes(
                200,
                b"",
                headers={
                    "DAV": "1, 2",
                    "Allow": DAV_ALLOWED_METHODS,
                    "MS-Author-Via": "DAV",
                    "Accept-Ranges": "bytes",
                },
            )
            return 200

        if method == "PROPFIND":
            return self._handle_propfind(handler, path, headers, body)
        if method == "PROPPATCH":
            return self._handle_proppatch(handler, path, body)
        if method in ("GET", "HEAD"):
            return self._handle_get_or_head(handler, method, path, headers)
        if method == "PUT":
            return self._handle_put(handler, path, headers, body)
        if method == "DELETE":
            return self._handle_delete(handler, path)
        if method == "MKCOL":
            return self._handle_mkcol(handler, path, body)
        if method == "MOVE":
            return self._handle_move_or_copy(handler, path, headers, is_move=True)
        if method == "COPY":
            return self._handle_move_or_copy(handler, path, headers, is_move=False)

        handler._send_bytes(405, b"Method Not Allowed", headers={"Allow": DAV_ALLOWED_METHODS})
        return 405

    def _check_conditionals(
        self,
        node: _Node,
        headers: dict[str, str],
        *,
        is_read: bool,
    ) -> int | None:
        if_match = headers.get("if-match")
        if if_match is not None and not _etag_matches(if_match, node.etag):
            return 412

        if_unmod = headers.get("if-unmodified-since")
        if if_unmod is not None:
            ts = _parse_http_date(if_unmod)
            if ts is not None and int(node.mtime) > int(ts):
                return 412

        if_none = headers.get("if-none-match")
        if if_none is not None and _etag_matches(if_none, node.etag):
            return 304 if is_read else 412

        if is_read:
            if_mod = headers.get("if-modified-since")
            if if_mod is not None:
                ts = _parse_http_date(if_mod)
                if ts is not None and int(node.mtime) <= int(ts):
                    return 304
        return None

    def _immediate_children(self, dir_path: str) -> list[tuple[str, _Node]]:
        prefix = "/" if dir_path == "/" else dir_path + "/"
        children: list[tuple[str, _Node]] = []
        for p, n in self._nodes.items():
            if p == dir_path or not p.startswith(prefix):
                continue
            rest = p[len(prefix) :]
            if "/" not in rest:
                children.append((p, n))
        children.sort(key=lambda item: item[0])
        return children

    def _all_descendants(self, dir_path: str) -> list[tuple[str, _Node]]:
        prefix = "/" if dir_path == "/" else dir_path + "/"
        out = [(p, n) for p, n in self._nodes.items() if p != dir_path and p.startswith(prefix)]
        out.sort(key=lambda item: item[0])
        return out

    def _handle_propfind(
        self,
        handler: _WebDAVRequestHandler,
        path: str,
        headers: dict[str, str],
        body: bytes,
    ) -> int:
        depth_raw = headers.get("depth", "infinity").strip().lower()
        if depth_raw not in ("0", "1", "infinity"):
            handler._send_bytes(400, b"Invalid Depth header", headers={"Content-Type": "text/plain"})
            return 400
        if depth_raw == "infinity" and not self.allow_depth_infinity:
            handler._send_bytes(403, b"propfind-finite-depth", headers={"Content-Type": "text/plain"})
            return 403

        propname_only = False
        if body.strip():
            try:
                root_xml = ET.fromstring(body)
            except ET.ParseError:
                handler._send_bytes(400, b"Malformed XML body", headers={"Content-Type": "text/plain"})
                return 400
            for elem in root_xml.iter():
                if elem.tag.endswith("propname"):
                    propname_only = True
                    break

        with self._lock:
            target = self._nodes.get(path)
            if target is None:
                handler._send_bytes(404, b"Not Found", headers={"Content-Type": "text/plain"})
                return 404

            def _snapshot(n: _Node) -> _Node:
                snap = copy.copy(n)
                snap.dead_props = dict(n.dead_props)
                return snap

            items: list[tuple[str, _Node]] = [(path, _snapshot(target))]
            if target.is_dir:
                if depth_raw == "1":
                    items.extend((p, _snapshot(n)) for p, n in self._immediate_children(path))
                elif depth_raw == "infinity":
                    items.extend((p, _snapshot(n)) for p, n in self._all_descendants(path))

        xml_bytes = self._build_multistatus_xml(items, propname_only=propname_only).encode("utf-8")
        handler._send_bytes(
            207,
            xml_bytes,
            headers={"Content-Type": 'application/xml; charset="utf-8"'},
        )
        return 207

    def _build_multistatus_xml(
        self,
        items: list[tuple[str, _Node]],
        *,
        propname_only: bool = False,
    ) -> str:
        lines = [
            '<?xml version="1.0" encoding="utf-8"?>',
            '<D:multistatus xmlns:D="DAV:">',
        ]
        for node_path, node in items:
            if node.is_dir:
                href_raw = "/" if node_path == "/" else node_path.rstrip("/") + "/"
            else:
                href_raw = node_path
            href_quoted = urllib.parse.quote(href_raw, safe="/")
            href_xml = xml_escape(href_quoted)
            display_name = "/" if node_path == "/" else posixpath.basename(node_path)
            display_xml = xml_escape(display_name)

            lines.append("  <D:response>")
            lines.append(f"    <D:href>{href_xml}</D:href>")
            lines.append("    <D:propstat>")
            lines.append("      <D:prop>")
            if propname_only:
                lines.append("        <D:resourcetype/>")
                lines.append("        <D:getlastmodified/>")
                lines.append("        <D:getetag/>")
                lines.append("        <D:displayname/>")
                if not node.is_dir:
                    lines.append("        <D:getcontentlength/>")
                    lines.append("        <D:getcontenttype/>")
                for ns_uri, local in sorted(node.dead_props):
                    tag_local = local.split(":")[-1]
                    if not tag_local:
                        continue
                    if ns_uri == "DAV:":
                        lines.append(f"        <D:{tag_local}/>")
                    elif ns_uri:
                        ns_esc = xml_escape(ns_uri, {'"': "&quot;"})
                        lines.append(f'        <{tag_local} xmlns="{ns_esc}"/>')
                    else:
                        lines.append(f"        <{tag_local}/>")
            else:
                if node.is_dir:
                    lines.append("        <D:resourcetype><D:collection/></D:resourcetype>")
                else:
                    lines.append("        <D:resourcetype/>")
                    lines.append(f"        <D:getcontentlength>{len(node.data)}</D:getcontentlength>")
                    lines.append(
                        f"        <D:getcontenttype>{xml_escape(node.content_type)}</D:getcontenttype>"
                    )
                lines.append(f"        <D:getlastmodified>{_format_rfc1123(node.mtime)}</D:getlastmodified>")
                lines.append(f"        <D:getetag>{xml_escape(node.etag)}</D:getetag>")
                lines.append(f"        <D:displayname>{display_xml}</D:displayname>")
                for (ns_uri, local), prop_val in sorted(node.dead_props.items()):
                    tag_local = local.split(":")[-1]
                    if not tag_local:
                        continue
                    val_xml = xml_escape(prop_val)
                    if ns_uri == "DAV:":
                        lines.append(f"        <D:{tag_local}>{val_xml}</D:{tag_local}>")
                    elif ns_uri:
                        ns_esc = xml_escape(ns_uri, {'"': "&quot;"})
                        lines.append(f'        <{tag_local} xmlns="{ns_esc}">{val_xml}</{tag_local}>')
                    else:
                        lines.append(f"        <{tag_local}>{val_xml}</{tag_local}>")
            lines.append("      </D:prop>")
            lines.append("      <D:status>HTTP/1.1 200 OK</D:status>")
            lines.append("    </D:propstat>")
            lines.append("  </D:response>")
        lines.append("</D:multistatus>")
        return "\n".join(lines)

    def _handle_proppatch(
        self,
        handler: _WebDAVRequestHandler,
        path: str,
        body: bytes,
    ) -> int:
        try:
            root_xml = ET.fromstring(body)
        except ET.ParseError:
            handler._send_bytes(400, b"Malformed XML body", headers={"Content-Type": "text/plain"})
            return 400

        with self._lock:
            node = self._nodes.get(path)
            if node is None:
                handler._send_bytes(404, b"Not Found", headers={"Content-Type": "text/plain"})
                return 404

            for action in root_xml:
                tag_local = action.tag.split("}")[-1]
                for prop_container in action:
                    if prop_container.tag.split("}")[-1] != "prop":
                        continue
                    for prop in prop_container:
                        ns, _, local = prop.tag.partition("}")
                        if not local:
                            key = ("", ns)
                        else:
                            key = (ns.lstrip("{"), local)
                        if tag_local == "set":
                            node.dead_props[key] = prop.text or ""
                        elif tag_local == "remove":
                            node.dead_props.pop(key, None)

        href_raw = "/" if path == "/" else (path + "/" if node.is_dir else path)
        href_xml = xml_escape(urllib.parse.quote(href_raw, safe="/"))
        resp_xml = (
            '<?xml version="1.0" encoding="utf-8"?>\n'
            '<D:multistatus xmlns:D="DAV:">\n'
            "  <D:response>\n"
            f"    <D:href>{href_xml}</D:href>\n"
            "    <D:propstat>\n"
            "      <D:prop/>\n"
            "      <D:status>HTTP/1.1 200 OK</D:status>\n"
            "    </D:propstat>\n"
            "  </D:response>\n"
            "</D:multistatus>"
        ).encode("utf-8")
        handler._send_bytes(207, resp_xml, headers={"Content-Type": 'application/xml; charset="utf-8"'})
        return 207

    def _handle_get_or_head(
        self,
        handler: _WebDAVRequestHandler,
        method: str,
        path: str,
        headers: dict[str, str],
    ) -> int:
        is_head = method == "HEAD"
        with self._lock:
            node = self._nodes.get(path)
            if node is None:
                handler._send_bytes(404, b"Not Found", is_head=is_head)
                return 404

            cond = self._check_conditionals(node, headers, is_read=True)
            if cond == 304:
                handler._send_bytes(
                    304,
                    b"",
                    headers={"ETag": node.etag, "Last-Modified": _format_rfc1123(node.mtime)},
                )
                return 304
            if cond == 412:
                handler._send_bytes(412, b"Precondition Failed", is_head=is_head)
                return 412

            if node.is_dir:
                if is_head:
                    handler._send_bytes(
                        200,
                        b"",
                        headers={
                            "Content-Type": node.content_type,
                            "Content-Length": "0",
                            "ETag": node.etag,
                            "Last-Modified": _format_rfc1123(node.mtime),
                            "Accept-Ranges": "bytes",
                        },
                        is_head=True,
                    )
                    return 200
                handler._send_bytes(405, b"Cannot GET a collection", headers={"Allow": DAV_ALLOWED_METHODS})
                return 405

            data = bytes(node.data)
            base_headers = {
                "Content-Type": node.content_type,
                "ETag": node.etag,
                "Last-Modified": _format_rfc1123(node.mtime),
                "Accept-Ranges": "bytes",
            }

        range_hdr = headers.get("range")
        if range_hdr and range_hdr.strip().lower().startswith("bytes="):
            _, _, spec_part = range_hdr.partition("=")
            spec = spec_part.split(",", 1)[0].strip()
            total = len(data)
            parsed_range = _parse_byte_range(spec, total)
            if parsed_range is None:
                handler._send_bytes(
                    416,
                    b"Range Not Satisfiable",
                    headers={"Content-Range": f"bytes */{total}"},
                    is_head=is_head,
                )
                return 416
            start, end = parsed_range
            slice_bytes = data[start : end + 1]
            base_headers["Content-Range"] = f"bytes {start}-{end}/{total}"
            if is_head:
                base_headers["Content-Length"] = str(len(slice_bytes))
            handler._send_bytes(206, slice_bytes, headers=base_headers, is_head=is_head)
            return 206

        if is_head:
            base_headers["Content-Length"] = str(len(data))
        handler._send_bytes(200, data, headers=base_headers, is_head=is_head)
        return 200

    def _handle_put(
        self,
        handler: _WebDAVRequestHandler,
        path: str,
        headers: dict[str, str],
        body: bytes,
    ) -> int:
        if path == "/":
            handler._send_bytes(405, b"Cannot PUT root")
            return 405

        mtime_hdr = headers.get("x-mock-mtime")
        if mtime_hdr is not None:
            try:
                mtime_val = _validate_epoch_mtime(float(mtime_hdr))
            except ValueError:
                handler._send_bytes(400, b"Invalid X-Mock-Mtime header")
                return 400
        else:
            mtime_val = time.time()

        parent = posixpath.dirname(path) or "/"
        ctype = headers.get("content-type") or mimetypes.guess_type(path)[0] or "application/octet-stream"

        with self._lock:
            parent_node = self._nodes.get(parent)
            if parent_node is None or not parent_node.is_dir:
                handler._send_bytes(409, b"Missing or non-collection parent")
                return 409

            existing = self._nodes.get(path)
            if existing is not None:
                if existing.is_dir:
                    handler._send_bytes(405, b"Target is a collection")
                    return 405
                cond = self._check_conditionals(existing, headers, is_read=False)
                if cond == 412:
                    handler._send_bytes(412, b"Precondition Failed")
                    return 412
                status = 204
            else:
                if headers.get("if-match") is not None:
                    handler._send_bytes(412, b"Precondition Failed")
                    return 412
                status = 201

            node = _Node(is_dir=False, data=bytes(body), mtime=mtime_val, content_type=ctype)
            node.refresh_etag(path)
            self._nodes[path] = node
            resp_headers = {
                "ETag": node.etag,
                "Last-Modified": _format_rfc1123(node.mtime),
            }

        handler._send_bytes(status, b"", headers=resp_headers)
        return status

    def _handle_delete(self, handler: _WebDAVRequestHandler, path: str) -> int:
        if path == "/":
            handler._send_bytes(403, b"Cannot delete root")
            return 403
        with self._lock:
            if path not in self._nodes:
                handler._send_bytes(404, b"Not Found")
                return 404
            prefix = path + "/"
            to_delete = [p for p in self._nodes if p == path or p.startswith(prefix)]
            for p in to_delete:
                del self._nodes[p]
        handler._send_bytes(204, b"")
        return 204

    def _handle_mkcol(
        self,
        handler: _WebDAVRequestHandler,
        path: str,
        body: bytes,
    ) -> int:
        if body:
            handler._send_bytes(415, b"Unsupported Media Type")
            return 415
        if path == "/":
            handler._send_bytes(405, b"Already exists")
            return 405

        parent = posixpath.dirname(path) or "/"
        with self._lock:
            if path in self._nodes:
                handler._send_bytes(405, b"Resource already exists")
                return 405
            parent_node = self._nodes.get(parent)
            if parent_node is None or not parent_node.is_dir:
                handler._send_bytes(409, b"Parent collection missing")
                return 409
            node = _Node(is_dir=True, mtime=time.time(), content_type="httpd/unix-directory")
            node.refresh_etag(path)
            self._nodes[path] = node
        handler._send_bytes(201, b"")
        return 201

    def _handle_move_or_copy(
        self,
        handler: _WebDAVRequestHandler,
        src_path: str,
        headers: dict[str, str],
        *,
        is_move: bool,
    ) -> int:
        dest_hdr = headers.get("destination")
        if not dest_hdr:
            handler._send_bytes(400, b"Missing Destination header")
            return 400

        parsed_dest = urllib.parse.urlsplit(dest_hdr)
        if parsed_dest.netloc:
            allowed_hosts = {f"{self.host}:{self.port}", f"localhost:{self.port}", self.host, "localhost"}
            if parsed_dest.netloc not in allowed_hosts:
                handler._send_bytes(502, b"Bad Gateway")
                return 502

        try:
            dst_path = _normalize_dav_path(parsed_dest.path)
        except ValueError:
            handler._send_bytes(400, b"Invalid Destination path")
            return 400

        overwrite_raw = headers.get("overwrite", "T").strip().upper()
        if overwrite_raw not in ("T", "F"):
            handler._send_bytes(400, b"Invalid Overwrite header")
            return 400
        overwrite = overwrite_raw == "T"

        depth_raw = headers.get("depth", "infinity").strip().lower()
        if is_move:
            if "depth" in headers and depth_raw != "infinity":
                handler._send_bytes(400, b"Invalid Depth header for MOVE")
                return 400
        elif depth_raw not in ("0", "infinity"):
            handler._send_bytes(400, b"Invalid Depth header for COPY")
            return 400

        if src_path == "/" or dst_path == "/":
            handler._send_bytes(403, b"Cannot move/copy root")
            return 403
        if dst_path == src_path or dst_path.startswith(src_path + "/"):
            handler._send_bytes(403, b"Destination equals or is inside source")
            return 403

        dst_parent = posixpath.dirname(dst_path) or "/"
        with self._lock:
            src_node = self._nodes.get(src_path)
            if src_node is None:
                handler._send_bytes(404, b"Source not found")
                return 404
            dst_parent_node = self._nodes.get(dst_parent)
            if dst_parent_node is None or not dst_parent_node.is_dir:
                handler._send_bytes(409, b"Destination parent missing")
                return 409

            dest_existed = dst_path in self._nodes
            if dest_existed and not overwrite:
                handler._send_bytes(412, b"Destination exists and Overwrite is F")
                return 412

            if dest_existed:
                dst_prefix = dst_path + "/"
                for p in [k for k in self._nodes if k == dst_path or k.startswith(dst_prefix)]:
                    del self._nodes[p]

            if src_node.is_dir and (is_move or depth_raw == "infinity"):
                src_prefix = src_path + "/"
                items_to_copy = [
                    (p, self._nodes[p])
                    for p in sorted(self._nodes)
                    if p == src_path or p.startswith(src_prefix)
                ]
            else:
                items_to_copy = [(src_path, src_node)]

            for old_p, old_node in items_to_copy:
                rel = old_p[len(src_path) :]
                new_p = dst_path + rel
                cloned = _Node(
                    is_dir=old_node.is_dir,
                    data=bytes(old_node.data),
                    mtime=old_node.mtime,
                    content_type=old_node.content_type,
                    dead_props=dict(old_node.dead_props),
                )
                cloned.refresh_etag(new_p)
                self._nodes[new_p] = cloned

            if is_move:
                src_prefix = src_path + "/"
                for p in [k for k in self._nodes if k == src_path or k.startswith(src_prefix)]:
                    del self._nodes[p]

        status = 204 if dest_existed else 201
        handler._send_bytes(status, b"")
        return status


def _parse_byte_range(spec: str, total: int) -> tuple[int, int] | None:
    if "-" not in spec or total <= 0:
        return None
    start_s, end_s = spec.split("-", 1)
    try:
        if start_s == "":
            suffix = int(end_s)
            if suffix <= 0:
                return None
            start = max(0, total - suffix)
            end = total - 1
            return (start, end)
        start = int(start_s)
        end = total - 1 if end_s == "" else int(end_s)
    except ValueError:
        return None
    if start < 0 or end < start or start >= total:
        return None
    return (start, min(end, total - 1))
