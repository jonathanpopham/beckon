# beckon

A keyboard launcher for macOS. Press a key, a panel appears, you type,
things happen. Free, MIT licensed, no account, no subscription, no
telemetry, and zero network calls in the shipped build.

**Zero dependencies.** Two Rust crates, std only. The engine
(`beckon-core`) is a pure deterministic library: fuzzy matching, frecency
ranking, a fixed-point calculator, clipboard and snippet stores, all golden
tested on Linux CI. The macOS shell talks to AppKit and the Accessibility
APIs through hand-rolled Objective-C runtime FFI. No webview, no JS
runtime, no wrapper crates.

## Status

Early. See [docs/PLAN.md](docs/PLAN.md) for the full feature matrix,
architecture, and milestones. The short version:

- M1: hotkey summons the panel, fuzzy app launch that learns your habits,
  inline calculator, system commands
- M2: clipboard history, window management, window switcher, file search
- M3: menu item search, snippets, emoji, quicklinks, dev utilities
- M4: script commands and a JSON-RPC plugin protocol so any executable in
  any language can extend it

## Build

```sh
cargo build --release
./target/release/beckon --version
```

## Verify

One gate, run anywhere:

```sh
scripts/verify.sh
```

CI is a thin wrapper around that script and nothing else.

## Data

Everything beckon remembers lives in plain files under `~/.beckon/`:
readable, greppable, syncable, yours.

## License

MIT
