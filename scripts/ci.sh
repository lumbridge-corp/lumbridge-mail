#!/usr/bin/env bash
# What CI runs, and what to run before a pull request: scripts/ci.sh
# Scrub check, clippy, the whole test suite. Offline once the crates are fetched (--locked).
# Tests use made-up mail, a throwaway home and GPUI's headless test platform: no account, no network, no display.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/scrub-check
# A throwaway home: nothing here may read or write a real config, store, keyring entry or socket. cargo and rustup keep
# their own directories, so the build uses the caches that are already there.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
T=$(mktemp -d "${TMPDIR:-/tmp}/lci.XXXXXX")
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/home" "$T/config" "$T/data" "$T/state" "$T/cache" "$T/run"
chmod 700 "$T/run"
export HOME="$T/home" XDG_CONFIG_HOME="$T/config" XDG_DATA_HOME="$T/data" XDG_STATE_HOME="$T/state" XDG_CACHE_HOME="$T/cache" XDG_RUNTIME_DIR="$T/run"
# No AI endpoint, remote daemon or other setting from the caller's environment. The Postgres mirror tests run only
# against a throwaway server named in LUMBRIDGE_MAIL_PG_TEST_URL (see crates/lumbridge-mail-pg/README.md).
for v in $(env | grep -oE '^(LBMAIL_|LUMBRIDGE_MAIL_)[A-Za-z0-9_]*' | grep -vxE 'LUMBRIDGE_MAIL_PG_TEST_URL' || true); do unset "$v"; done
rustc --version
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast
