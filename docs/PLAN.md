# beckon: plan of record

A free, fully open source (MIT) keyboard launcher for macOS in the Raycast
mold. You press a key, a panel appears, you type, things happen. No account,
no subscription, no telemetry, no network calls in the shipped build.

## Positioning

Prior art and why this exists anyway:

| Project | Stack | Gap beckon fills |
|---|---|---|
| Raycast | closed core | paid pro tier, account, cloud features |
| sol | Swift + React Native | JS runtime in the hot path |
| Asyar | Tauri v2 + Svelte | webview UI, npm dependency tree |
| SuperCmd | closed-ish, AI-forward | cloud AI posture |
| Vicinae | C++ + Qt | Linux only |
| Quicksilver | Objective-C | maintained but generationally old |

beckon's bet: **zero dependencies**. Two crates, std only, no wrapper
crates, no webview, no JS. The engine is a pure deterministic Rust library
tested on Linux CI; the macOS shell talks to AppKit, Carbon, and the
Accessibility APIs through hand-rolled Objective-C runtime FFI. Same inputs,
same ranking, byte for byte, forever.

## Principles

1. **Deterministic core.** All matching, ranking, parsing, and storage
   decisions live in `beckon-core`: std only, no floats in ranking paths
   (fixed-point integers), no clock reads outside injected parameters,
   golden tests for every ranking behavior.
2. **Airgap pure.** The shipped binary makes zero network calls. Anything
   that wants the network (currency rates, update checks) sits behind a
   cargo feature that is OFF by default.
3. **No LLM in the build.** Nothing model-backed ships in the default
   binary, ever.
4. **Own your data.** Clipboard history, snippets, frecency, and settings
   are plain files in `~/.beckon/`, human readable, trivially syncable and
   grep-able. Canonical JSON codec, atomic writes.
5. **One gate.** `scripts/verify.sh` is the single verification entrypoint;
   CI is a thin wrapper around it. Green gate or it does not merge.

## Feature matrix

### Navigation (the priority)

- App launching and switching: fuzzy match plus frecency, learns your habits
- Window switcher: every window of every app across spaces, fuzzy searchable
- Window management: halves, thirds, quarters, maximize, center, restore,
  move to next or previous display, all bindable to global hotkeys
- Menu item search: fuzzy search the frontmost app's entire menu bar and
  invoke any item (the single most underrated Raycast feature)
- File search: fast local index of user directories plus Spotlight fallback
- Quicklinks: parameterized URL and app links with `{query}` placeholders
- Per-command aliases and global hotkeys for any command

### Core commands

- Clipboard history: searchable, pinnable, paste as plain text, previews
- Snippets: keyword-expanded text with placeholders (date, clipboard, cursor)
- Calculator: arithmetic, unit conversion (offline tables), base conversion
  (hex, bin, oct), date math. Fixed-point decimal, no float drift
- Emoji and symbol picker with keyword search
- System commands: sleep, lock, empty trash, toggle dark mode, volume, mute,
  eject, kill process, quit app, restart app
- Dev utilities: UUID, base64 encode and decode, hashes, JSON pretty-print,
  epoch timestamp conversion, lorem

### Extensibility

- Script commands: drop an annotated shell, Python, or any-language script
  in `~/.beckon/scripts/` and it becomes a command
- Plugin protocol: JSON-RPC 2.0 over stdio, so a plugin is any executable in
  any language; list, search, run, respond. No SDK required
- Optional local integrations discovered at runtime, never bundled

### Polish

- Theming (colors, fonts) via plain config file
- Preferences window plus config file parity (the file is the truth)
- Signed, notarizable `.app` bundle built by script, no Xcode project

## Architecture

```
crates/
  beckon-core/     std-only deterministic engine (builds and tests anywhere)
    fuzzy.rs       subsequence matcher and scorer, golden-tested
    frecency.rs    integer half-life usage ranking
    router.rs      query parse, command registry, dispatch
    calc.rs        fixed-point calculator, units, bases, dates
    clipstore.rs   clipboard history model (dedupe, pin, search)
    snippets.rs    snippet store and placeholder expansion
    quicklinks.rs  parameterized links
    persist.rs     canonical JSON codec, atomic file store
  beckon-macos/    the shell (bin: beckon), cfg-gated so Linux CI compiles it
    ffi.rs         Objective-C runtime FFI foundation (msg_send, classes)
    panel.rs       non-activating floating NSPanel
    hotkey.rs      Carbon RegisterEventHotKey global hotkeys
    ui.rs          input field plus results table, keyboard driven
    apps.rs        application indexer (Info.plist walk)
    files.rs       file search
    pasteboard.rs  NSPasteboard change watcher
    winmgmt.rs     Accessibility API window management
    switcher.rs    CGWindowList window switcher
    menubar.rs     Accessibility menu item search
    system.rs      system command bindings
```

Rules of engagement for parallel work: each feature lives in its own file;
shared files (`lib.rs`, `main.rs`, registries) are merged by the integrator.

## Risks

- **The FFI bet.** Hand-rolled `objc_msgSend` FFI for a full AppKit UI is
  the highest-risk item and is the first spike. Go or no-go is decided by a
  working hotkey-summoned floating panel. Documented escape hatch: if the
  spike fails, the shell (and only the shell) may adopt a minimal
  Objective-C bridge crate; the core stays std only regardless.
- **Accessibility permissions.** Window management, menu search, and
  snippet expansion need the AX permission; the app must degrade gracefully
  and explain itself when unauthorized.
- **Paste injection.** Programmatic paste (clipboard history, snippets)
  needs CGEvent key synthesis; sequencing with pasteboard restore is fiddly
  and needs real-hardware testing.

## Milestones

- **M0 scaffold.** Workspace, gate, CI, plan, beads. (this commit)
- **M1 summonable.** Global hotkey summons the panel; fuzzy app launch with
  frecency; inline calculator; system commands. Daily drivable.
- **M2 hands on the system.** Clipboard history, window management, window
  switcher, file search.
- **M3 deep navigation.** Menu item search, snippets with expansion, emoji
  picker, quicklinks, dev utilities.
- **M4 open platform.** Script commands, plugin protocol, theming,
  preferences, `.app` packaging, docs, public release.

## Verification

`scripts/verify.sh`: fmt, clippy deny-warnings, build, full test suite,
zero-dependency audit (no `[dependencies]` entries beyond the internal core
crate), no-network audit (`std::net` is forbidden in both crates). CI runs
the same script on ubuntu (core plus stub shell) and macos (full shell).
