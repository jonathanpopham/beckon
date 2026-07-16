# beckon

A keyboard launcher for macOS. Press Option+Space, a panel appears, you
type, things happen. Free, MIT licensed, no account, no subscription, no
telemetry, and zero network calls in the shipped build.

**Zero dependencies.** Two Rust crates, std only. The engine
(`beckon-core`) is a pure deterministic library: fuzzy matching, frecency
ranking, a fixed-point calculator, snippet expansion, all golden tested on
Linux CI. The macOS shell talks to AppKit, Carbon, CoreGraphics, and the
Accessibility APIs through hand-rolled Objective-C and C FFI. No webview,
no JS runtime, no wrapper crates, no build.rs.

## What it does

- **Launch and switch apps** with fuzzy search that learns your habits
  (integer frecency, 14 day half-life; a strong match always wins)
- **Window management**: halves, thirds, quarters, maximize, center, move
  between displays ("window left half", or alias it to "lh")
- **Window switcher**: `win` lists every window of every app
- **Menu item search**: `menu export` finds "File > Export as PDF" in the
  frontmost app and presses it
- **Clipboard history**: `clip` searches everything you copied; Return
  pastes it into the app you came from. Password manager secrets are
  never captured (concealed pasteboard types respected)
- **File search**: `file report` ranks a local index, Spotlight backfills
- **Path browser**: paste any path (`~/geist/thing`, `/Applications/...`,
  even quoted, shell-escaped, or `file://` forms) and get Open, Reveal in
  Finder, Quick Look, and Copy Path; partial paths complete as you type
  and Return drills into folders, so `~/ge` walks the tree without Finder
- **Calculator inline**: type `0.1 + 0.2` and get exactly `0.3`
  (fixed-point, no float drift); units (`5 km in mi`), bases
  (`255 in hex`), percent (`200 * 15%`)
- **Snippets**: `snip` with `{date}`, `{time}`, `{clipboard}`,
  `{cursor}` placeholders, pasted where you were typing
- **Quicklinks**: `go google rust launcher` opens the search, properly
  percent-encoded
- **Emoji and symbols**: `emoji fire`, plus arrows, math, currency, and
  mac key glyphs
- **Dev utilities**: `uuid`, `b64`, `sha256`, `json`, `epoch`, `count`
- **System commands**: sleep, lock, empty trash, dark mode, volume,
  quit frontmost, restart Finder
- **Script commands**: drop an annotated executable in
  `~/.beckon/scripts/` and it becomes a command
- **Plugins**: any executable speaking JSON-RPC 2.0 over stdio, in any
  language; see [docs/PLUGINS.md](docs/PLUGINS.md)

## Install

Homebrew:

```sh
brew install --cask jonathanpopham/tap/beckon --no-quarantine
```

Or the install script (downloads the latest release to /Applications):

```sh
curl -fsSL https://raw.githubusercontent.com/jonathanpopham/beckon/main/scripts/install.sh | sh
```

Or from source (no toolchain beyond Rust):

```sh
git clone https://github.com/jonathanpopham/beckon
cd beckon
scripts/bundle.sh
open dist/Beckon.app
```

Press Option+Space. Add `Beckon.app` to System Settings > Login Items to
start it at login. Window management and paste need one Accessibility
grant; beckon offers the prompt when a feature first needs it and
degrades gracefully until then. Releases are ad-hoc signed, not
notarized; the `--no-quarantine` flag and the install script's xattr
step exist for that reason, and building from source sidesteps it
entirely.

## Change the hotkey

`~/.beckon/config.json`:

```json
{ "hotkey": { "key": "space", "modifiers": ["cmd", "shift"] } }
```

Keys: letters, digits, `space`, `tab`, `return`, `f1` to `f12`.
Modifiers: `cmd`, `opt`, `ctrl`, `shift` in any combination. Restart
beckon and the startup line confirms the chord.

## Configure

Everything lives in plain files under `~/.beckon/` (readable, greppable,
syncable, yours). `config.json` is the source of truth:

```json
{
  "aliases":     { "lh": "window.left-half" },
  "hotkey":      { "key": "space", "modifiers": ["opt"] },
  "max_results": 9,
  "theme":       { "background": "#1C1C21", "foreground": "#FFFFFF",
                   "accent": "#5AC8FA", "font_size": 22 },
  "triggers":    { "v": "clip" }
}
```

Every key is optional. Aliases can point at any command id or rewrite
into any trigger. Triggers rename the keyword sources (`clip`, `win`,
`file`, `menu`, `emoji`, `snip`, `go`).

## Verify

One gate, run anywhere, and CI is a thin wrapper around it:

```sh
scripts/verify.sh
```

fmt, clippy with denied warnings, the full test suite, a
zero-dependency audit, and a no-network audit. `beckon --smoke` runs a
headless end-to-end self-test of the real UI pipeline on macOS.

## Design

See [docs/PLAN.md](docs/PLAN.md) for the architecture and the positioning
against sol, Asyar, SuperCmd, and Vicinae. The short version of the bet:
the launcher you press a hundred times a day should be a deterministic
program you can read, not a subscription. Same inputs, same ranking, byte
for byte, forever.

## License

MIT
