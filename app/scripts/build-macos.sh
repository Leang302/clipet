#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Clipet's signed macOS bundle must be built on macOS." >&2
  exit 1
fi

detect_signing_identity() {
  local line candidate apple_development=""
  while IFS= read -r line; do
    candidate="${line#*\"}"
    candidate="${candidate%%\"*}"
    case "$candidate" in
      "Developer ID Application:"*)
        printf '%s\n' "$candidate"
        return 0
        ;;
      "Apple Development:"*)
        if [[ -z "$apple_development" ]]; then
          apple_development="$candidate"
        fi
        ;;
    esac
  done < <(security find-identity -v -p codesigning)

  if [[ -n "$apple_development" ]]; then
    printf '%s\n' "$apple_development"
    return 0
  fi
  return 1
}

if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  if ! APPLE_SIGNING_IDENTITY="$(detect_signing_identity)"; then
    echo "No valid Developer ID Application or Apple Development signing identity was found." >&2
    echo "Refusing to create an unsigned Clipet build because macOS Accessibility grants would break after replacement." >&2
    exit 1
  fi
  export APPLE_SIGNING_IDENTITY
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir/.."

echo "Building Clipet with signing identity: $APPLE_SIGNING_IDENTITY"
exec npm run tauri build -- --target universal-apple-darwin --bundles app,dmg "$@"
