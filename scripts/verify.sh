#!/usr/bin/env bash
# beckon verification gate: the SINGLE entrypoint. CI is a thin wrapper
# that runs exactly this script. No verification logic lives in YAML.
set -euo pipefail

cd "$(dirname "$0")/.."

FAILED=0
banner() { printf '\n==> %s\n' "$*"; }
pass()   { printf 'PASS: %s\n' "$*"; }
fail()   { printf 'FAIL: %s\n' "$*"; FAILED=1; }
die()    { printf 'FAIL: %s\n' "$*"; exit 1; }

banner "0. preflight"
command -v cargo >/dev/null 2>&1 || die "cargo not found"
pass "cargo present ($(cargo --version))"

banner "1. formatting"
if cargo fmt --all -- --check; then pass "rustfmt clean"; else fail "rustfmt"; fi

banner "2. clippy (deny warnings)"
if command -v cargo-clippy >/dev/null 2>&1 || cargo clippy --version >/dev/null 2>&1; then
  if cargo clippy --workspace --all-targets -- -D warnings; then
    pass "clippy clean"
  else
    fail "clippy"
  fi
else
  printf 'SKIP: clippy not installed\n'
fi

banner "3. build"
cargo build --workspace || die "build failed; nothing after this can run"
pass "workspace builds"

banner "4. tests"
if cargo test --workspace; then pass "all tests green"; else fail "tests"; fi

banner "5. zero-dependency audit"
# Every [dependencies] section may contain only the internal beckon-core
# path dep. Any external crate name fails the gate.
DEP_VIOLATIONS=$(awk '
  /^\[/{in_deps = ($0 == "[dependencies]")}
  in_deps && /=/ && $1 != "beckon-core" {print FILENAME ": " $0}
' crates/*/Cargo.toml)
if [ -z "$DEP_VIOLATIONS" ]; then
  pass "no external dependencies"
else
  printf '%s\n' "$DEP_VIOLATIONS"
  fail "external dependency detected"
fi

banner "6. no-network audit"
if grep -rn 'std::net' crates/*/src; then
  fail "std::net usage found; the shipped build makes zero network calls"
else
  pass "no std::net usage"
fi

banner "result"
if [ "$FAILED" -ne 0 ]; then
  printf 'GATE: FAIL\n'; exit 1
fi
printf 'GATE: PASS\n'
