#!/usr/bin/env bash
# scripts/build-dmg.sh — Build a signed, notarized Keitora .dmg
# Run from the repo root. Requires env vars set before invoking.
set -euo pipefail

# ── Validate required env vars ────────────────────────────────────────────────
: "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY to the full Developer ID cert name}"
# Notarization: set either Flow A or Flow B vars before running.
# Flow A: APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
# Flow B: APPLE_API_KEY, APPLE_API_ISSUER, APPLE_API_KEY_PATH
if [[ -z "${APPLE_ID:-}" && -z "${APPLE_API_KEY:-}" ]]; then
  echo "ERROR: Set either APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID (Flow A)" >&2
  echo "       or APPLE_API_KEY/APPLE_API_ISSUER/APPLE_API_KEY_PATH (Flow B)" >&2
  exit 1
fi

# ── Build, sign, notarize via Tauri ──────────────────────────────────────────
# `tauri build` runs `beforeBuildCommand` ("yarn build") itself, so the frontend
# is compiled here — no separate `yarn build` needed.
# Stage the CLI sidecar and build with the bundle overlay so the signed
# .dmg ships the `externalBin` sidecar (plain `tauri build` would omit them).
echo "==> Staging sidecars..."
yarn bundle-cli
echo "==> Running tauri build (frontend + sign + notarize, with sidecars)..."
yarn tauri build --config src-tauri/tauri.bundle.conf.json

# ── Locate outputs ────────────────────────────────────────────────────────────
# This is a Cargo workspace, so the bundle lands in the repo-root target/, not
# src-tauri/target/.
APP="target/release/bundle/macos/Keitora.app"
DMG=$(find target/release/bundle/dmg -name "*.dmg" | head -1)

if [[ -z "$DMG" ]]; then
  echo "ERROR: No .dmg found under target/release/bundle/dmg/" >&2
  exit 1
fi

echo "==> Produced DMG: $DMG"

# ── Verify code signature on .app ─────────────────────────────────────────────
echo "==> Verifying code signature..."
codesign --verify --deep --strict --verbose=2 "$APP"

# ── Verify Gatekeeper acceptance of .app ──────────────────────────────────────
echo "==> Verifying Gatekeeper acceptance..."
spctl --assess --type exec --verbose "$APP"

# ── Verify notarization staple on .dmg ────────────────────────────────────────
echo "==> Verifying notarization staple..."
xcrun stapler validate "$DMG"

echo ""
echo "Build and verification complete."
echo "DMG: $DMG"
