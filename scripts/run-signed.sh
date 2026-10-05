#!/bin/sh
# Cargo's runner on macOS (see .cargo/config.toml). Signs the app with the
# identity from create-signing-identity.sh before `cargo run` or
# `cargo tauri dev` start it, so the Keychain sees every rebuild as the same
# app. Test binaries, and Macs without the identity, run as built.
set -eu

identity="Squint Local Signing"
if [ "$(basename "$1")" = squint ] && security find-identity -p codesigning | grep -q "\"$identity\""; then
  if ! out=$(codesign --force --sign "$identity" --identifier com.quintpieters.squint "$1" 2>&1); then
    printf '%s\n%s\n' "$out" "Running unsigned, so the Keychain will ask for your password." >&2
  fi
fi
exec "$@"
