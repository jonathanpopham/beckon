# beckon plugins

A beckon plugin is any executable, in any language, that reads JSON-RPC 2.0
requests from stdin (one per line) and writes responses to stdout (one per
line). No SDK, no linking, no registration.

Install: drop an executable into `~/.beckon/plugins/` (or
`$BECKON_HOME/plugins/` if you set `BECKON_HOME`) and make it executable
(`chmod +x`). beckon discovers it at startup, spawns it the first time it is
needed, and keeps it running for the session.

The normative protocol definition lives in `crates/beckon-core/src/rpc.rs`;
its golden tests are the test vectors. This document restates it for plugin
authors.

## Transport

- beckon spawns your executable on first use and keeps it running for the
  session. When beckon exits, your stdin closes: exit on EOF.
- Each request is a single line of UTF-8 JSON terminated by `\n`. Each
  response is a single line of UTF-8 JSON terminated by `\n`.
- Answer every request, in order, echoing the integer `id` of the request.
- stdout carries protocol lines only. Diagnostics go to stderr; beckon
  forwards your stderr to its own stderr, prefixed with your file name.
- Do not emit JSON float literals. beckon stores integers only and rejects
  any float.
- beckon enforces a 2 second per-response timeout and a 1 MiB per-line size
  cap. Miss either and your plugin is disabled for the rest of the session.

## Methods

The protocol is versioned by the integer `protocol` field of the manifest
result. This is protocol version 1.

### beckon.manifest

Sent once, immediately after spawn (the handshake). Params: `{}`. Result
fields, all required:

| field         | type    | meaning                                          |
| ------------- | ------- | ------------------------------------------------ |
| `protocol`    | integer | protocol version; must be `1`                    |
| `name`        | string  | stable identity, used in item ids; keep it short |
| `version`     | string  | your plugin's own version, informational         |
| `keyword`     | string  | trigger word: `<keyword> <text>` routes to you   |
| `description` | string  | one line shown to the user                       |

```text
-> {"id":1,"jsonrpc":"2.0","method":"beckon.manifest","params":{}}
<- {"jsonrpc":"2.0","id":1,"result":{"protocol":1,"name":"demo","version":"1.0.0","keyword":"demo","description":"Echoes your query"}}
```

### beckon.query

Sent every time the user's input changes while your keyword is active.
Params: `{"query": string}`, the text after the keyword (may be empty).
Result: `{"items": [...]}` where each item has:

| field      | type   | meaning                                             |
| ---------- | ------ | --------------------------------------------------- |
| `id`       | string | required, non-empty; echoed back on activate        |
| `title`    | string | required; the row's main text                       |
| `subtitle` | string | optional, defaults to empty; the secondary line     |

```text
-> {"id":2,"jsonrpc":"2.0","method":"beckon.query","params":{"query":"hello"}}
<- {"jsonrpc":"2.0","id":2,"result":{"items":[{"id":"echo","title":"Echo: hello","subtitle":"Copy to clipboard"}]}}
```

### beckon.activate

Sent when the user picks one of your items. Params: `{"id": string}`, the
item id you returned from `beckon.query`. Result:
`{"action": ..., "value": ...}` where `action` is one of:

- `"none"`: nothing further; `value` not required
- `"copy"`: beckon copies `value` (string, required) to the clipboard
- `"paste"`: beckon copies `value` and pastes it into the frontmost app
- `"open"`: beckon opens `value` as a URL or path

```text
-> {"id":3,"jsonrpc":"2.0","method":"beckon.activate","params":{"id":"echo"}}
<- {"jsonrpc":"2.0","id":3,"result":{"action":"copy","value":"hello"}}
```

## Errors

Report a per-request failure with a standard JSON-RPC 2.0 error response
instead of a result:

```text
<- {"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}
```

`code` must be an integer and `message` a string. An error response is a
well-formed exchange: your plugin stays alive. Garbage on stdout, a missed
timeout, or an oversized line is not: beckon kills your process and disables
the plugin for the session.

## The example plugin

[`plugins/example-plugin.py`](plugins/example-plugin.py) is a complete,
working plugin in about 40 lines of stdlib-only Python. Its keyword is
`demo`; it echoes your query back as two items and activating one copies the
text. Try it:

```sh
mkdir -p ~/.beckon/plugins
cp docs/plugins/example-plugin.py ~/.beckon/plugins/
chmod +x ~/.beckon/plugins/example-plugin.py
```

Restart beckon, then type `demo hello world` in the launcher.

## Write one in any language

The whole job is a read-line, write-line loop. In pseudocode:

```text
loop:
    line = read a line from stdin        # EOF means beckon quit: exit 0
    req  = parse line as JSON
    switch req.method:
        "beckon.manifest": result = {protocol: 1, name, version, keyword, description}
        "beckon.query":    result = {items: [...]} built from req.params.query
        "beckon.activate": result = {action, value} for req.params.id
        anything else:     error  = {code: -32601, message: "method not found"}
    write one line: {"jsonrpc": "2.0", "id": req.id, "result": result}
                or: {"jsonrpc": "2.0", "id": req.id, "error": error}
    flush stdout                          # unflushed output looks like a timeout
```

That is the entire contract. Checklist:

1. Make it executable: a shebang line plus `chmod +x`, or a compiled binary.
2. Read stdin line by line; exit when stdin closes.
3. Answer `beckon.manifest` within 2 seconds of starting. Slow runtimes
   should do any heavy setup after the handshake, or lazily on first query.
4. Flush stdout after every response. Buffered output is the most common
   cause of a plugin being disabled: the bytes never arrive, beckon times
   out, and your process is killed.
5. One line per response, no floats, integers echoed as integers.
6. Keep stdout clean. Print debug output to stderr; beckon forwards it.
7. Keep whatever state you like in memory between requests; your process
   lives for the whole session. The example uses this to remember the last
   query so activate can copy it.

To debug by hand, run your plugin in a terminal and paste request lines from
the examples above into it; the responses must come back on single lines.
