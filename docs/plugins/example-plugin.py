#!/usr/bin/env python3
"""beckon example plugin: echoes your query back.

The reference implementation of the beckon plugin protocol, version 1
(see docs/PLUGINS.md). Reads JSON-RPC 2.0 requests from stdin, one per
line, and answers each with one line on stdout. Python stdlib only.

Install: copy this file into ~/.beckon/plugins/ and chmod +x it.
Then type "demo hello" in the launcher.
"""
import json
import sys

last_query = ""


def handle(method, params):
    global last_query
    if method == "beckon.manifest":
        return {"protocol": 1, "name": "demo", "version": "1.0.0",
                "keyword": "demo", "description": "Echoes your query"}
    if method == "beckon.query":
        last_query = params.get("query", "")
        return {"items": [
            {"id": "echo", "title": "Echo: " + last_query,
             "subtitle": "Copy to clipboard"},
            {"id": "upper", "title": last_query.upper(),
             "subtitle": "Copy the query in uppercase"},
        ]}
    if method == "beckon.activate":
        value = last_query.upper() if params.get("id") == "upper" else last_query
        return {"action": "copy", "value": value}
    return None  # unknown method: JSON-RPC error below


for line in sys.stdin:  # exits cleanly on EOF, i.e. when beckon quits
    if not line.strip():
        continue
    req = json.loads(line)
    resp = {"jsonrpc": "2.0", "id": req["id"]}
    result = handle(req.get("method"), req.get("params") or {})
    if result is None:
        resp["error"] = {"code": -32601, "message": "method not found"}
    else:
        resp["result"] = result
    print(json.dumps(resp), flush=True)
