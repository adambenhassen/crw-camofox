#!/usr/bin/env python3
"""MCP smoke test for the crw stack.

Exercises both MCP servers the Compose stack exposes and asserts the contract
the README/skills document:

  * crw `/mcp`  — 6 scraping tools, a working scrape, and the 4-engine search cap.
  * camofox-mcp — 47 interactive-browser tools, bearer auth enforced.

Hard assertions exit non-zero. The Camofox-backed search is heavy (real Firefox)
and resource-sensitive on small CI runners, so it runs best-effort: reported,
never fatal.

Env (all optional):
  CRW_MCP_URL       default http://localhost:3000/mcp
  CAMOFOX_MCP_URL   default http://localhost:9378/mcp
  CAMOFOX_KEY       default crw-local-dev-insecure-default-key
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

CRW_URL = os.environ.get("CRW_MCP_URL", "http://localhost:3000/mcp")
CAMOFOX_URL = os.environ.get("CAMOFOX_MCP_URL", "http://localhost:9378/mcp")
CAMOFOX_KEY = os.environ.get("CAMOFOX_KEY", "crw-local-dev-insecure-default-key")

EXPECTED_CRW_TOOLS = {
    "crw_scrape", "crw_crawl", "crw_check_crawl_status",
    "crw_map", "crw_search", "crw_parse_file",
}
EXPECTED_CAMOFOX_TOOL_COUNT = 47

failures: list[str] = []


def rpc(url: str, method: str, params: dict | None = None, *, rid: int = 1,
        key: str | None = None, timeout: int = 60) -> dict:
    """POST a JSON-RPC request; parse JSON or SSE (`data:` lines). Returns the
    JSON-RPC envelope. Raises urllib HTTPError for non-2xx (e.g. 401)."""
    body = json.dumps({"jsonrpc": "2.0", "id": rid, "method": method,
                        "params": params or {}}).encode()
    headers = {"Content-Type": "application/json",
               "Accept": "application/json, text/event-stream"}
    if key:
        headers["Authorization"] = f"Bearer {key}"
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode()
    # SSE: pull the last `data:` payload; else treat as plain JSON.
    datas = [ln[5:].strip() for ln in raw.splitlines() if ln.startswith("data:")]
    return json.loads(datas[-1] if datas else raw)


def check(name: str, cond: bool, detail: str = "") -> None:
    mark = "✅" if cond else "❌"
    print(f"{mark} {name}" + (f" — {detail}" if detail else ""))
    if not cond:
        failures.append(name)


def main() -> int:
    print(f"== crw /mcp ({CRW_URL}) ==")
    try:
        init = rpc(CRW_URL, "initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"}})
        check("crw initialize", init.get("result", {}).get("serverInfo", {}).get("name") == "crw",
              str(init.get("result", {}).get("serverInfo")))

        tools = {t["name"] for t in rpc(CRW_URL, "tools/list", rid=2)["result"]["tools"]}
        check("crw tools/list == 6 expected", tools == EXPECTED_CRW_TOOLS,
              f"got {sorted(tools)}")

        scr = rpc(CRW_URL, "tools/call", {
            "name": "crw_scrape",
            "arguments": {"url": "https://example.com", "formats": ["markdown"]}},
            rid=3, timeout=90)
        scr_txt = scr["result"]["content"][0]["text"]
        check("crw_scrape returns markdown", "Example Domain" in scr_txt,
              f"{len(scr_txt)} chars")

        cap = rpc(CRW_URL, "tools/call", {
            "name": "crw_search",
            "arguments": {"query": "x", "engines":
                          ["google", "bing", "duckduckgo", "wikipedia", "github"]}},
            rid=4, timeout=30)
        cap_txt = cap["result"]["content"][0]["text"]
        check("crw_search rejects >4 engines", cap["result"].get("isError") and "at most 4" in cap_txt,
              cap_txt[:80])
    except Exception as e:  # noqa: BLE001
        check("crw /mcp reachable", False, repr(e))

    print(f"\n== camofox-mcp ({CAMOFOX_URL}) ==")
    try:
        no_auth_code = None
        try:
            rpc(CAMOFOX_URL, "tools/list", rid=1, key=None, timeout=15)
        except urllib.error.HTTPError as e:
            no_auth_code = e.code
        check("camofox-mcp rejects no-auth (401)", no_auth_code == 401, f"got {no_auth_code}")

        init = rpc(CAMOFOX_URL, "initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"}}, key=CAMOFOX_KEY, timeout=20)
        check("camofox-mcp initialize", init["result"]["serverInfo"]["name"] == "camofox-mcp",
              str(init["result"]["serverInfo"]))

        n = len(rpc(CAMOFOX_URL, "tools/list", rid=2, key=CAMOFOX_KEY, timeout=20)["result"]["tools"])
        check(f"camofox-mcp tools/list == {EXPECTED_CAMOFOX_TOOL_COUNT}",
              n == EXPECTED_CAMOFOX_TOOL_COUNT, f"got {n}")
    except Exception as e:  # noqa: BLE001
        check("camofox-mcp reachable", False, repr(e))

    # Best-effort: the Camofox-backed search drives real Firefox and is slow /
    # resource-sensitive on small runners. Report, never fail the run.
    print("\n== best-effort: crw_search (Camofox/Firefox) ==")
    try:
        s = rpc(CRW_URL, "tools/call", {
            "name": "crw_search", "arguments": {"query": "camoufox firefox", "limit": 2}},
            rid=9, timeout=90)
        txt = s["result"]["content"][0]["text"]
        try:
            n = len(json.loads(txt).get("data", {}).get("results", []))
            print(f"ℹ️  crw_search returned {n} results")
        except Exception:
            print(f"ℹ️  crw_search (best-effort) — isError={s['result'].get('isError')}: {txt[:120]}")
    except Exception as e:  # noqa: BLE001
        print(f"ℹ️  crw_search best-effort skipped: {e!r}")

    print()
    if failures:
        print(f"FAILED: {len(failures)} hard assertion(s): {failures}")
        return 1
    print("PASS: all hard assertions held")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
