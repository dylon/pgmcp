#!/usr/bin/env python3
"""Smoke-test the compiled pgmcp web UI in a real browser.

The test serves `webui/resources` from a temporary localhost server, mocks the
closed REST surfaces, opens the compiled app in headless Chromium, and checks
the primary views on mobile and desktop viewports for runtime errors and
horizontal body overflow.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

try:
    from playwright.sync_api import expect, sync_playwright
except ImportError as exc:  # pragma: no cover - environment preflight
    raise SystemExit("python playwright package is required for this smoke test") from exc


REPO = Path(__file__).resolve().parents[1]
WEBUI_RESOURCES = REPO / "webui" / "resources"
PRODUCTION_CSP = (
    "default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' data:; "
    "style-src 'self' 'unsafe-inline'; style-src-elem 'self'; "
    "style-src-attr 'unsafe-inline'; script-src 'self' 'wasm-unsafe-eval'; object-src 'none'; "
    "base-uri 'none'; frame-ancestors 'none'"
)
STATS_KINDS = ("status", "index", "cron", "clients", "telemetry", "counters")
STATS_LABELS = {
    "status": "Status",
    "index": "Index",
    "cron": "Cron",
    "clients": "Clients",
    "telemetry": "Telemetry",
    "counters": "Counters",
}
QUERY_MODES = {
    "semantic": {"label": "Semantic", "request": {"mode": "semantic", "limit": 10, "query": "webui"}},
    "text": {"label": "Text", "request": {"mode": "text", "limit": 10, "query": "webui"}},
    "grep": {"label": "Grep", "request": {"mode": "grep", "limit": 10, "pattern": "webui"}},
}
WORK_REQUEST = {"view": ["next-actionable"], "limit": ["25"]}


def stats_data_for(kind: str) -> dict:
    """Realistic per-kind /api/stats payloads keyed by the REAL backend struct
    field names (verbatim snake_case), so the Overview normalizers are exercised
    against the same keys the daemon emits."""
    if kind == "status":
        return {
            "daemon": {"version": "0.1.0", "phase": "ready", "uptime_secs": 3600,
                       "current_rss_bytes": 100000000, "peak_rss_bytes": 120000000,
                       "http_mcp_sessions": 2, "heavy_cron_running": False},
            "database": {"url": "postgres://localhost/pgmcp", "name": "pgmcp",
                         "server_version": "17", "pool_size": 8, "pool_active": 1, "pool_idle": 7},
            "embeddings": {"model": "bge-m3", "dimensions": 1024, "backend": "candle", "device": "cuda"},
            "pools": {"general": {"max_threads": 8, "active_workers": 1, "queue_depth": 0}},
        }
    if kind == "index":
        return {"project_count": 3, "indexed_file_count": 2195, "chunk_count": 50000,
                "per_project": [
                    {"project_name": "pgmcp", "indexed_file_count": 2195, "chunk_count": 50000},
                    {"project_name": "f1r3node", "indexed_file_count": 1200, "chunk_count": 30000}]}
    if kind == "cron":
        return {
            "rollup": [
                {"job_name": "quality-history", "last_outcome": "failed", "run_count": 12,
                 "ok_count": 10, "fail_count": 2, "skip_count": 0, "avg_ms": 1500,
                 "last_error": "connection reset by peer", "last_skip_reason": None},
                {"job_name": "db-maintenance", "last_outcome": "ok", "run_count": 40,
                 "ok_count": 40, "fail_count": 0, "skip_count": 0, "avg_ms": 200,
                 "last_error": None, "last_skip_reason": None}],
            "recent": [
                {"job_name": "quality-history", "outcome": "failed", "duration_ms": 1600,
                 "started_at": "2026-07-06T14:00:00Z", "skip_reason": None,
                 "error_detail": "connection reset by peer"},
                {"job_name": "db-maintenance", "outcome": "skipped", "duration_ms": 0,
                 "started_at": "2026-07-06T13:00:00Z", "skip_reason": "db_down", "error_detail": None}],
        }
    if kind == "clients":
        return {
            "active": [{"mcp_session_id": "s1", "client_name": "claude-code", "project": "pgmcp",
                        "cwd": "/home/dylon/ws", "pid": 123, "idle_secs": 5, "alive": True}],
            "project_matrix": [{"client_name": "claude-code", "project": "pgmcp",
                                "edit_count": 10, "read_count": 20, "last_activity": "2026-07-05T14:00:00Z"}],
        }
    if kind == "telemetry":
        return {"tools": [{"tool": "semantic_search", "calls": 100, "error_count": 2,
                           "avg_duration_ms": 50, "max_duration_ms": 500, "last_ts": "2026-07-06T14:00:00Z"}]}
    if kind == "counters":
        return {"uptime_secs": 3600, "mcp_requests": 1000, "mcp_errors": 5, "files_indexed": 2195,
                "chunks_embedded": 50000, "bytes_processed": 123456789,
                "active_work_pool_threads": 1, "work_pool_queue_depth": 0, "embed_errors": 0}
    return {"ok": True, "panel": kind}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, _fmt: str, *_args: object) -> None:
        return

    def send_body(self, status: int, body: bytes | str, content_type: str) -> None:
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-security-policy", PRODUCTION_CSP)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        if parsed.path == "/favicon.ico":
            self.send_body(204, b"", "image/x-icon")
        elif parsed.path in ("/webui", "/webui/"):
            self.send_body(200, (WEBUI_RESOURCES / "index.html").read_bytes(), "text/html; charset=utf-8")
        elif parsed.path == "/webui/app.js":
            self.send_body(200, (WEBUI_RESOURCES / "app.js").read_bytes(), "application/javascript")
        elif parsed.path == "/webui/app.css":
            self.send_body(200, (WEBUI_RESOURCES / "app.css").read_bytes(), "text/css")
        elif parsed.path.startswith("/webui/grammars/"):
            rel = parsed.path[len("/webui/"):]
            fpath = WEBUI_RESOURCES / rel
            if ".." in rel or not fpath.is_file():
                self.send_body(404, "not found", "text/plain")
            else:
                ct = "application/wasm" if rel.endswith(".wasm") else "text/plain"
                self.send_body(200, fpath.read_bytes(), ct)
        elif parsed.path == "/api/stats":
            query = parse_qs(parsed.query)
            kind = query.get("kind", ["status"])[0]
            extra = set(query) - {"kind", "include_exited"}
            if extra or len(query.get("kind", [])) != 1 or kind not in STATS_KINDS:
                self.send_body(400, json.dumps({"error": f"unexpected stats request: {query}"}), "application/json")
                return
            payload = {"kind": kind, "server_seq": 41, "data": stats_data_for(kind)}
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/mandates":
            query = parse_qs(parsed.query)
            if query != {"scope": ["all"]}:
                self.send_body(400, json.dumps({"error": f"unexpected mandates request: {query}"}), "application/json")
                return
            payload = {
                "requested_project": None,
                "requested_cwd": None,
                "requested_scope": "all",
                "as_of_seq": None,
                "server_seq": 42,
                "found_project": True,
                "durable_mandates": [
                    {"id": 7, "scope": "global", "project_id": None, "polarity": "always",
                     "imperative": "Prefer re-frame.", "target": None,
                     "source_mandate_id": None, "promoted_at": "2026-07-05T12:00:00Z",
                     "file_path": None, "created_by": "operator",
                     "updated_at": "2026-07-05T12:00:00Z", "retired_at": None},
                ],
                "mandates": {
                    "sources": [
                        {
                            "scope": "project",
                            "kind": "agents",
                            "path": "AGENTS.md",
                            "text": "Use re-frame.",
                        }
                    ],
                    "project_override": {
                        "source_path": ".pgmcp.toml",
                        "sha256": "abc",
                        "size_bytes": 27,
                        "truncated": False,
                        "text": "[git]\nindex_history = true\n",
                    },
                    "skipped_sources": [
                        {
                            "scope": "workspace",
                            "kind": "claude",
                            "path": "CLAUDE.md",
                            "reason": "too large",
                        }
                    ],
                }
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/work_items":
            query = parse_qs(parsed.query)
            if query != WORK_REQUEST:
                self.send_body(400, json.dumps({"error": f"unexpected work request: {query}"}), "application/json")
                return
            payload = {
                "view": "next-actionable",
                "count": 1,
                "server_seq": 43,
                "items": [
                    {
                        "id": 1,
                        "public_id": "WI-1",
                        "parent_id": None,
                        "project_id": None,
                        "definition_id": None,
                        "root_id": None,
                        "kind": "task",
                        "status": "ready",
                        "title": "Build the pgmcp web UI",
                        "body": "Use re-frame and preserve CESK benefits.",
                        "parametric": False,
                        "parametric_corpus": None,
                        "parametric_expected": None,
                        "priority": 7,
                        "weight": 1.0,
                        "computed_score": 9.5,
                        "claimed_percent": 25,
                        "origin": "user_explicit",
                        "created_by": "codex",
                        "created_at": "2026-07-04T00:00:00Z",
                        "updated_at": "2026-07-04T00:00:00Z",
                        "started_at": None,
                        "completed_at": None,
                        "verified_at": None,
                        "due_at": None,
                        "snooze_until": None,
                        "severity": None,
                        "claimed_by": None,
                        "claimed_at": None,
                        "lease_expires_at": None,
                        "claim_count": 0,
                        "assignee": "codex",
                        "assigned_at": None,
                        "assigned_by": None,
                    }
                ],
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/resources":
            payload = {
                "system": {
                    "cpu": {
                        "per_core_pct": [12.5, 40.0],
                        "aggregate_pct": 26.25,
                        "core_count": 2,
                        "load1": 1.2,
                        "load5": 0.9,
                        "load15": 0.7,
                    },
                    "memory": {
                        "total_bytes": 34359738368,
                        "available_bytes": 12000000000,
                        "used_bytes": 22359738368,
                        "used_pct": 65.1,
                        "swap_total_bytes": 0,
                        "swap_used_bytes": 0,
                    },
                    "gpu": [
                        {
                            "index": 0,
                            "name": "NVIDIA Test GPU",
                            "util_pct": 55,
                            "mem_total_bytes": 8589934592,
                            "mem_used_bytes": 7516192768,
                            "mem_used_pct": 87.5,
                            "temperature_c": 62,
                            "power_watts": 120.0,
                        }
                    ],
                    "process": {"rss_bytes": 700000000, "peak_rss_bytes": 800000000, "threads": 42},
                    "sampled_at_ms": 0,
                },
                "worker_pools": {
                    "active_work_pool_threads": 17,
                    "work_pool_queue_depth": 0,
                    "work_pool_tasks_completed": 13430,
                    "pool_pressure_ms_total": 382346,
                    "embed_workers_alive": 2,
                    "db_pool_size": 10,
                    "db_pool_idle": 8,
                    "db_pool_active": 2,
                },
                "uptime_secs": 60495,
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/metrics":
            payload = {
                "series": "tool_calls", "bucket": "hour", "since_minutes": 1440,
                "buckets": [
                    {"ts": "2026-07-05T13:00:00Z", "calls": 120, "errors": 3, "avg_ms": 12.5},
                    {"ts": "2026-07-05T14:00:00Z", "calls": 200, "errors": 1, "avg_ms": 9.0},
                ],
                "server_seq": 0,
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/db/tables":
            payload = {"tables": [{
                "name": "work_items", "label": "Work items",
                "columns": [
                    {"name": "public_id", "type": "text", "sortable": True, "filterable": True},
                    {"name": "status", "type": "text", "sortable": True, "filterable": True},
                ],
                "default_sort": "public_id", "max_limit": 200,
            }]}
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/db/rows":
            payload = {
                "table": "work_items",
                "columns": [{"name": "public_id", "type": "text"}, {"name": "status", "type": "text"}],
                "rows": [{"public_id": "WI-1", "status": "ready"}],
                "total": 1, "limit": 50, "offset": 0, "server_seq": 0,
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/logs/tail":
            payload = {
                "path": "/tmp/pgmcp.log", "truncated": False,
                "lines": [{"text": "daemon started", "level": "INFO",
                           "ts": "2026-07-05T14:00:00Z", "target": "pgmcp"}],
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/logs/grep":
            payload = {
                "truncated": False,
                "matches": [{"line_number": 42, "line": "error: timeout",
                             "matched": [{"text": "error", "start": 1,
                                          "end": 6, "distance": 0}]}],
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/experiments":
            payload = {
                "experiments": [{"id": 1, "slug": "exp-1", "title": "Reranker A/B",
                                 "status": "verified", "project": "pgmcp",
                                 "created_at": "2026-07-01T00:00:00Z"}],
                "server_seq": 0,
            }
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/api/work_items/tree":
            payload = {"root": "PLAN-1", "nodes": [
                {"id": 1, "public_id": "PLAN-1", "kind": "plan", "status": "in_progress", "title": "Root plan", "depth": 0},
                {"id": 2, "public_id": "EPIC-1", "kind": "epic", "status": "ready", "title": "An epic", "depth": 1},
                {"id": 3, "public_id": "WI-1", "kind": "task", "status": "ready", "title": "A task", "depth": 2}]}
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path.startswith("/api/work_items/"):
            payload = {
                "item": {"id": 1, "public_id": parsed.path.rsplit("/", 1)[-1], "kind": "task", "status": "ready",
                         "title": "A task", "body": "Task **body** markdown.", "priority": 5, "claimed_percent": 25,
                         "assignee": "dylon", "created_at": "2026-07-01T00:00:00Z", "updated_at": "2026-07-02T00:00:00Z"},
                "timeline": [{"kind": "status", "at": "2026-07-01T00:00:00Z", "actor": "agent",
                              "summary": "pending -> ready", "detail": {}}],
                "acceptance_criteria": [{"id": 1, "criterion_kind": "test", "description": "tests pass",
                                         "gate": "verify.sh", "required": True}],
                "bug_details": None, "server_seq": 0}
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path.startswith("/api/experiments/") and parsed.path.endswith("/ledger"):
            slug = parsed.path.split("/")[3]
            self.send_body(200, json.dumps({"slug": slug, "ledger": "# Ledger\n\nResults for the experiment."}), "application/json")
        elif parsed.path.startswith("/api/experiments/"):
            slug = parsed.path.split("/")[3]
            payload = {
                "experiment": {"id": 1, "slug": slug, "title": "Reranker A/B", "question": "Does rerank help?",
                               "context": None, "kind": "experiment", "status": "verified", "project": "pgmcp",
                               "git_ref": None, "plan_ref": None, "correction": None,
                               "created_at": "2026-07-01T00:00:00Z", "updated_at": "2026-07-02T00:00:00Z"},
                "hypotheses": [{"hypothesis_id": 1, "statement": "rerank improves recall",
                                "primary_metric": "recall@10", "verdict": "supported"}],
                "measurements": [{"run_id": 1, "arm_label": "A", "status": "usable", "sample_count": 100}],
                "decisions": [{"result_id": 1, "test_type": "t-test", "metric": "recall@10",
                               "p_value": 0.01, "effect_size": 0.2, "verdict": "accept"}],
                "artifacts": [{"artifact_id": 1, "kind": "report", "tool": "eval", "label": "run report"}],
                "timeline": [{"at": "2026-07-01T00:00:00Z", "event": "opened", "detail": None}],
                "server_seq": 0}
            self.send_body(200, json.dumps(payload), "application/json")
        elif parsed.path == "/webui/ws":
            self.send_body(426, "websocket unavailable in smoke test", "text/plain")
        else:
            self.send_body(404, "not found", "text/plain")

    def do_PATCH(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        if length:
            self.rfile.read(length)
        parsed = urlparse(self.path)
        if (parsed.path.startswith("/api/work_items/")
                or parsed.path.startswith("/api/mandates")
                or parsed.path.startswith("/api/experiments/")):
            self.send_body(200, json.dumps({"ok": True}), "application/json")
        else:
            self.send_body(404, "not found", "text/plain")

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length) if length else b"{}"
        parsed = urlparse(self.path)
        if parsed.path.startswith("/api/work_items/") or parsed.path.startswith("/api/mandates"):
            self.send_body(200, json.dumps({"ok": True}), "application/json")
            return
        if parsed.path != "/api/query":
            self.send_body(404, "not found", "text/plain")
            return

        request = json.loads(body.decode("utf-8"))
        mode = request.get("mode")
        expected = QUERY_MODES.get(mode, {}).get("request")
        if expected is None or request != expected:
            self.send_body(400, json.dumps({"error": f"unexpected query request: {request}"}), "application/json")
            return
        payload = {
            "mode": mode,
            "data": {
                "results": [
                    {
                        "file_path": "/workspace/pgmcp/src/lib.rs",
                        "relative_path": "src/lib.rs",
                        "project_name": "pgmcp",
                        "start_line": 10,
                        "end_line": 12,
                        "language": "rust",
                        "similarity": 0.99,
                        "chunk": f"pub mod webui; // {mode}",
                    }
                ],
                "truncated": mode == "grep",
                "rerank_used": False,
                "colbert_used": False,
            },
        }
        self.send_body(200, json.dumps(payload), "application/json")


def chromium_path() -> str:
    configured = os.environ.get("PLAYWRIGHT_CHROMIUM")
    if configured:
        return configured
    for candidate in ("chromium", "chromium-browser", "google-chrome-stable", "google-chrome"):
        path = shutil.which(candidate)
        if path:
            return path
    raise SystemExit("no Chromium executable found")


def assert_no_body_overflow(page, label: str) -> None:
    overflow = page.evaluate(
        """() => ({
          bodyScrollWidth: document.body.scrollWidth,
          documentScrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth
        })"""
    )
    scroll_width = max(overflow["bodyScrollWidth"], overflow["documentScrollWidth"])
    print(f"{label}: {scroll_width}px scroll / {overflow['clientWidth']}px viewport")
    if scroll_width > overflow["clientWidth"] + 1:
        raise AssertionError(f"horizontal overflow on {label}: {overflow}")


def machine_control(page) -> dict[str, str]:
    return page.evaluate("() => window.__pgmcpWebuiMachine().s.control")


def machine_store(page) -> dict:
    return page.evaluate("() => window.__pgmcpWebuiMachine().s")


def assert_control(page, expected: dict[str, str], label: str) -> None:
    control = machine_control(page)
    for key, value in expected.items():
        if control.get(key) != value:
            raise AssertionError(f"{label}: expected control.{key}={value!r}, got {control!r}")


def wait_for_control(page, expected: dict[str, str], label: str) -> None:
    page.wait_for_function(
        """expected => {
          const control = window.__pgmcpWebuiMachine().s.control;
          return Object.entries(expected).every(([key, value]) => control[key] === value);
        }""",
        arg=expected,
    )
    assert_control(page, expected, label)


def wait_for_stats_payload(page, kind: str, label: str) -> None:
    page.wait_for_function(
        """kind => {
          const store = window.__pgmcpWebuiMachine().s;
          const payload = store.domain.stats[kind];
          return store.ui["stats-kind"] === kind &&
                 payload &&
                 payload.kind === kind &&
                 payload.data;
        }""",
        arg=kind,
    )
    store = machine_store(page)
    payload = store["domain"]["stats"][kind]
    if store["ui"]["stats-kind"] != kind or payload["kind"] != kind:
        raise AssertionError(f"{label}: expected stats payload for {kind!r}, got {store!r}")


def wait_for_query_payload(page, mode: str, label: str) -> None:
    page.wait_for_function(
        """mode => {
          const store = window.__pgmcpWebuiMachine().s;
          const form = store.ui.query;
          const payload = store.domain["query-result"];
          const rows = payload && payload.data && payload.data.results;
          return store.control.query === "loaded" &&
                 form.mode === mode &&
                 payload &&
                 payload.mode === mode &&
                 Array.isArray(rows) &&
                 rows.length === 1 &&
                 rows[0].chunk.includes(mode);
        }""",
        arg=mode,
    )
    store = machine_store(page)
    payload = store["domain"]["query-result"]
    if store["ui"]["query"]["mode"] != mode or payload["mode"] != mode:
        raise AssertionError(f"{label}: expected query payload for {mode!r}, got {store!r}")


def set_query_mode(page, mode: str, label: str) -> None:
    mode_label = QUERY_MODES[mode]["label"]
    page.locator(".query-mode").click()
    page.get_by_text(mode_label, exact=True).last.click()
    page.wait_for_function(
        """mode => window.__pgmcpWebuiMachine().s.ui.query.mode === mode""",
        arg=mode,
    )
    wait_for_control(page, {"query": "editing"}, label)


def press_alt_left(page) -> None:
    page.keyboard.down("Alt")
    page.keyboard.press("ArrowLeft")
    page.keyboard.up("Alt")


def exercise_app(context, url: str, viewport: dict[str, int], label: str) -> None:
    page = context.new_page()
    page.set_viewport_size(viewport)
    response = page.goto(url, wait_until="networkidle")
    if response is None:
        raise AssertionError(f"{label}: initial navigation produced no response")
    csp = response.header_value("content-security-policy")
    if csp != PRODUCTION_CSP:
        raise AssertionError(f"{label}: unexpected CSP {csp!r}")

    expect(page.get_by_text("operator console")).to_be_visible()
    wait_for_control(
        page,
        {
            "view": "overview",
            "connection": "closed",
            "activity": "ready",
            "query": "editing",
            "mandates": "idle",
            "work": "idle",
            "events": "streaming",
        },
        f"{label}/initial-control",
    )
    wait_for_stats_payload(page, "status", f"{label}/status-loaded")
    for kind in STATS_KINDS[1:]:
        # scope to the content toolbar — a nav tab may share a stats label (e.g. Clients)
        page.get_by_role("main").get_by_role("button", name=STATS_LABELS[kind]).click()
        wait_for_stats_payload(page, kind, f"{label}/{kind}-loaded")
        wait_for_control(page, {"activity": "ready"}, f"{label}/{kind}-ready")
    assert_no_body_overflow(page, f"{label}/overview")

    page.get_by_role("button", name="Resources").click()
    wait_for_control(page, {"view": "resources"}, f"{label}/resources-view")
    expect(page.get_by_text("Load average")).to_be_visible()
    wait_for_control(page, {"resources": "loaded"}, f"{label}/resources-loaded")
    assert_no_body_overflow(page, f"{label}/resources")

    page.get_by_role("button", name="Metrics").click()
    wait_for_control(page, {"view": "metrics"}, f"{label}/metrics-view")
    expect(page.get_by_text("Series data")).to_be_visible()
    assert_no_body_overflow(page, f"{label}/metrics")

    page.get_by_role("banner").get_by_role("button", name="Clients").click()
    wait_for_control(page, {"view": "clients"}, f"{label}/clients-view")
    expect(page.get_by_text("Active clients")).to_be_visible()
    assert_no_body_overflow(page, f"{label}/clients")

    page.get_by_role("button", name="Database").click()
    wait_for_control(page, {"view": "database"}, f"{label}/database-view")
    page.get_by_role("button", name="Work items").click()
    expect(page.get_by_text("WI-1").first).to_be_visible()
    assert_no_body_overflow(page, f"{label}/database")

    page.get_by_role("button", name="Logs").click()
    wait_for_control(page, {"view": "logs"}, f"{label}/logs-view")
    expect(page.get_by_text("daemon started")).to_be_visible()
    assert_no_body_overflow(page, f"{label}/logs")

    page.get_by_role("button", name="Experiments").click()
    wait_for_control(page, {"view": "experiments"}, f"{label}/experiments-view")
    expect(page.get_by_text("Reranker A/B")).to_be_visible()
    # split-pane drill-down: click the row (no "Open" button) → detail on the right
    page.get_by_text("Reranker A/B").click()
    expect(page.get_by_text("Hypotheses").first).to_be_visible()
    page.get_by_role("button", name="Ledger").click()
    expect(page.get_by_text("Results for the experiment")).to_be_visible()
    page.locator(".md-detail-head button:visible").first.click()
    assert_no_body_overflow(page, f"{label}/experiments")

    page.get_by_role("button", name="Query").click()
    wait_for_control(page, {"view": "query", "query": "editing"}, f"{label}/query-view")
    page.get_by_placeholder("search terms / pattern").fill("webui")
    page.get_by_role("button", name="Run").click()
    expect(page.get_by_text("src/lib.rs")).to_be_visible()
    expect(page.get_by_text(":10-12")).to_be_visible()
    wait_for_query_payload(page, "semantic", f"{label}/semantic-loaded")
    wait_for_control(page, {"query": "loaded", "activity": "ready"}, f"{label}/query-loaded")
    for mode in ("text", "grep"):
        set_query_mode(page, mode, f"{label}/{mode}-editing")
        page.get_by_role("button", name="Run").click()
        wait_for_query_payload(page, mode, f"{label}/{mode}-loaded")
        wait_for_control(page, {"query": "loaded", "activity": "ready"}, f"{label}/{mode}-ready")
    assert_no_body_overflow(page, f"{label}/query")

    page.get_by_role("button", name="Events").click()
    wait_for_control(page, {"view": "events", "events": "streaming"}, f"{label}/events-view")
    press_alt_left(page)
    wait_for_control(page, {"view": "query"}, f"{label}/alt-left-back-to-query")
    page.get_by_role("button", name="Events").click()
    wait_for_control(page, {"view": "events", "events": "streaming"}, f"{label}/events-view-after-back")
    expect(page.get_by_text("Tracker")).to_be_visible()
    page.get_by_role("button", name="Pause").click()
    expect(page.get_by_role("button", name="Resume")).to_be_visible()
    wait_for_control(page, {"events": "paused"}, f"{label}/events-pause-state")
    page.get_by_role("button", name="Resume").click()
    expect(page.get_by_role("button", name="Pause")).to_be_visible()
    wait_for_control(page, {"events": "streaming"}, f"{label}/events-resumed")
    assert_no_body_overflow(page, f"{label}/events")

    page.get_by_role("button", name="Mandates").click()
    wait_for_control(page, {"view": "mandates", "mandates": "idle"}, f"{label}/mandates-view")
    page.get_by_role("button", name="Load").click()
    expect(page.get_by_text("Use re-frame.")).to_be_visible()
    expect(page.get_by_text(".pgmcp.toml")).to_be_visible()
    expect(page.get_by_text("too large")).to_be_visible()
    wait_for_control(page, {"mandates": "loaded", "activity": "ready"}, f"{label}/mandates-loaded")
    assert_no_body_overflow(page, f"{label}/mandates")

    page.get_by_role("button", name="Work").click()
    wait_for_control(page, {"view": "work", "work": "idle"}, f"{label}/work-view")
    page.get_by_role("button", name="Load").click()
    expect(page.get_by_text("WI-1")).to_be_visible()
    expect(page.get_by_text("Build the pgmcp web UI")).to_be_visible()
    wait_for_control(page, {"work": "loaded", "activity": "ready"}, f"{label}/work-loaded")
    # split-pane drill-down: click the row head (no "Detail" button) → detail on the right
    page.get_by_text("Build the pgmcp web UI").click()
    expect(page.get_by_text("Acceptance criteria").first).to_be_visible()
    # column sort: clicking a header adds a ▲/▼ indicator
    page.get_by_role("columnheader", name="Kind").first.click()
    expect(page.locator("th").filter(has_text="▲").first).to_be_visible()
    page.locator(".md-detail-head button:visible").first.click()
    # tree view (GET /api/work_items/tree?root=PLAN-1): fill root + toggle hierarchy
    page.get_by_placeholder("tree root").fill("PLAN-1")
    page.get_by_role("checkbox").click()
    expect(page.get_by_text("Root plan")).to_be_visible()
    expect(page.get_by_text("An epic")).to_be_visible()
    assert_no_body_overflow(page, f"{label}/work")
    page.close()


def collect_page_diagnostics(page_errors: list[str], console_messages: list[tuple[str, str]]):
    def register(page) -> None:
        page.on("pageerror", lambda exc: page_errors.append(str(exc)))
        page.on("console", lambda msg: console_messages.append((msg.type, msg.text)))

    return register


def main() -> int:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/webui"
    page_errors: list[str] = []
    console_messages: list[tuple[str, str]] = []

    try:
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(
                executable_path=chromium_path(),
                headless=True,
                args=["--no-sandbox"],
            )
            page_context = browser.new_context()
            page_context.on("page", collect_page_diagnostics(page_errors, console_messages))
            for viewport, label in (
                ({"width": 390, "height": 844}, "mobile"),
                ({"width": 1366, "height": 900}, "desktop"),
            ):
                exercise_app(page_context, url, viewport, label)
            page_context.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()

    filtered_console = [
        (kind, text)
        for kind, text in console_messages
        if kind in ("error", "warning") and "WebSocket" not in text
    ]
    if page_errors or filtered_console:
        print(f"page_errors={page_errors}", file=sys.stderr)
        print(f"console={filtered_console}", file=sys.stderr)
        return 1

    print(f"webui render smoke passed: {url}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
