#!/usr/bin/env bash
# ============================================
# Juicity macOS .app Bundle Script
# ============================================
# Creates a self-contained Juicity.app bundle with all non-system
# dylib dependencies bundled inside.
#
# Prerequisites:
#   - macOS build already completed (release binaries exist)
#   - icon.svg in the gui directory
#
# Usage:
#   ./gui/macos/bundle.sh [--target-dir <path>] [--app-name <name>]
#
# Environment variables:
#   TARGET_DIR   - Path to cargo target directory (default: ./target/<triple>/release)
#   APP_NAME     - Application name (default: Juicity)
#   VERSION      - Version string for Info.plist (default: 0.1.0)
#   BUILD_NUMBER - Build number for Info.plist (default: 1)
# ============================================

set -euo pipefail
# trace commands when VERBOSE is set
[[ -n "${VERBOSE:-}" ]] && set -x

# ── Paths ───────────────────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUI_DIR="${REPO_ROOT}/gui"

# Allow override via env or CLI args
TARGET_DIR="${TARGET_DIR:-}"
APP_NAME="${APP_NAME:-Juicity}"
VERSION="${VERSION:-0.1.0}"
BUILD_NUMBER="${BUILD_NUMBER:-1}"

# Parse CLI args
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target-dir) TARGET_DIR="$2"; shift 2 ;;
    --app-name)   APP_NAME="$2";   shift 2 ;;
    --version)    VERSION="$2";    shift 2 ;;
    --build-number) BUILD_NUMBER="$2"; shift 2 ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

# Auto-detect target dir if not specified
if [[ -z "${TARGET_DIR}" ]]; then
  # Detect host triple
  TRIPLE="$(rustc -vV | grep host | awk '{print $2}')"
  TARGET_DIR="${REPO_ROOT}/target/${TRIPLE}/release"
fi

echo "==> Juicity macOS Bundle"
echo "    Repo root:    ${REPO_ROOT}"
echo "    Target dir:   ${TARGET_DIR}"
echo "    App name:     ${APP_NAME}"
echo "    Version:      ${VERSION} (build ${BUILD_NUMBER})"

# Verify binaries exist
BINARY="${TARGET_DIR}/juicity-gui"
if [[ ! -f "${BINARY}" ]]; then
  echo "ERROR: juicity-gui binary not found at ${BINARY}"
  echo "Run 'cargo build --release -p juicity-gui' first."
  exit 1
fi

# ── Create .app bundle structure ─────────────────────────────────────────
APP_BUNDLE="${REPO_ROOT}/dist/${APP_NAME}.app"
rm -rf "${APP_BUNDLE}"

CONTENTS="${APP_BUNDLE}/Contents"
MACOS_DIR="${CONTENTS}/MacOS"
RESOURCES_DIR="${CONTENTS}/Resources"
FRAMEWORKS_DIR="${CONTENTS}/Frameworks"

mkdir -p "${MACOS_DIR}"
mkdir -p "${RESOURCES_DIR}"
mkdir -p "${FRAMEWORKS_DIR}"

echo "==> Created bundle structure at ${APP_BUNDLE}"

# ── Info.plist ───────────────────────────────────────────────────────────
sed -e "s/__VERSION__/${VERSION}/g" \
    -e "s/__BUILD_NUMBER__/${BUILD_NUMBER}/g" \
    "${GUI_DIR}/macos/Info.plist.in" > "${CONTENTS}/Info.plist"
echo "==> Generated Info.plist"

# ── Copy binaries ────────────────────────────────────────────────────────
cp "${BINARY}" "${MACOS_DIR}/juicity-gui"
chmod +x "${MACOS_DIR}/juicity-gui"

echo "==> Copied binaries to MacOS/"

# ── Generate icon (icns) from the build script output ───────────────────
# build.rs rasterizes gui/icon.svg at exactly the sizes an .iconset needs, so
# the bundle does not depend on an SVG converter being installed.
ICNS_PATH="${RESOURCES_DIR}/icon.icns"
ICONSET_DIR="${REPO_ROOT}/dist/Juicity.iconset"
if command -v iconutil &>/dev/null; then
  rm -rf "${ICONSET_DIR}"
  mkdir -p "${ICONSET_DIR}"

  BUILD_OUT_DIR="$(ls -dt "${TARGET_DIR}"/build/juicity-gui-*/out 2>/dev/null | head -n 1 || true)"

  if [[ -n "${BUILD_OUT_DIR}" && -f "${BUILD_OUT_DIR}/1024.png" ]]; then
    # "<size> <iconset filename>" pairs required by `iconutil`.
    while read -r SIZE NAME; do
      cp "${BUILD_OUT_DIR}/${SIZE}.png" "${ICONSET_DIR}/${NAME}"
    done <<'MEMBERS'
16 icon_16x16.png
32 icon_16x16@2x.png
32 icon_32x32.png
64 icon_32x32@2x.png
128 icon_128x128.png
256 icon_128x128@2x.png
256 icon_256x256.png
512 icon_256x256@2x.png
512 icon_512x512.png
1024 icon_512x512@2x.png
MEMBERS
  elif command -v sips &>/dev/null; then
    # Fallback: render the SVG ourselves and let sips produce every size.
    TMP_PNG="${REPO_ROOT}/dist/juicity-icon-1024.png"
    if command -v rsvg-convert &>/dev/null; then
      rsvg-convert -w 1024 -h 1024 "${GUI_DIR}/icon.svg" -o "${TMP_PNG}"
    elif command -v convert &>/dev/null; then
      convert -background none -size 1024x1024 "${GUI_DIR}/icon.svg" "${TMP_PNG}"
    else
      echo "WARNING: No SVG converter found (rsvg-convert or ImageMagick). Skipping icon generation."
      TMP_PNG=""
    fi

    if [[ -n "${TMP_PNG}" && -f "${TMP_PNG}" ]]; then
      for size in 16 32 64 128 256 512 1024; do
        # Standard size
        sips -z "${size}" "${size}" "${TMP_PNG}" \
          --out "${ICONSET_DIR}/icon_${size}x${size}.png" &>/dev/null || true
        # Retina size (2x)
        if [[ ${size} -le 512 ]]; then
          sips -z "$((size*2))" "$((size*2))" "${TMP_PNG}" \
            --out "${ICONSET_DIR}/icon_${size}x${size}@2x.png" &>/dev/null || true
        fi
      done
      rm -f "${TMP_PNG}"
    fi
  fi

  if compgen -G "${ICONSET_DIR}/*.png" >/dev/null; then
    # Convert iconset to icns
    iconutil -c icns "${ICONSET_DIR}" -o "${ICNS_PATH}" || {
      echo "WARNING: iconutil failed. Proceeding without icon."
    }
  fi
  rm -rf "${ICONSET_DIR}"
fi

if [[ ! -f "${ICNS_PATH}" ]]; then
  echo "WARNING: icns icon not generated. App will use default icon."
fi
echo "==> Generated icon"

# ── Bundle dylib dependencies ────────────────────────────────────────────
echo "==> Bundling dylib dependencies..."

# Use a temp file to track copied dylib names (bash 3.2 compatible dedup)
COPIED_FILE="$(mktemp "${FRAMEWORKS_DIR}/.copied.XXXXXX")"
trap 'rm -f "${COPIED_FILE}"' EXIT

# We need to process the main binary
BINS_TO_PROCESS=("${MACOS_DIR}/juicity-gui")

# Process binaries and their dependencies
QUEUE=("${BINS_TO_PROCESS[@]}")
while [[ ${#QUEUE[@]} -gt 0 ]]; do
  BIN="${QUEUE[0]}"
  QUEUE=("${QUEUE[@]:1}")
  [[ -f "${BIN}" ]] || continue

  while IFS= read -r dep; do
    [[ -z "${dep}" ]] && continue
    # Skip system dylibs (those in /usr/lib/ or /System/)
    case "${dep}" in
      /usr/lib/*|/System/*) continue ;;
    esac
    # Also skip the binary itself
    [[ "${dep}" == "${BIN}" ]] && continue

    dep_name="$(basename "${dep}")"
    # Skip if already copied (check temp file)
    if grep -qFx "${dep_name}" "${COPIED_FILE}" 2>/dev/null; then
      continue
    fi
    echo "${dep_name}" >> "${COPIED_FILE}"

    target="${FRAMEWORKS_DIR}/${dep_name}"
    if [[ -f "${dep}" ]]; then
      cp -n "${dep}" "${target}" 2>/dev/null || true
      chmod 644 "${target}" 2>/dev/null || true
      QUEUE+=("${target}")
      echo "  Copied: ${dep_name}"
    fi
  done < <(otool -L "${BIN}" 2>/dev/null | tail -n +2 | awk '{print $1}')
done

COPIED_COUNT="$(wc -l < "${COPIED_FILE}" 2>/dev/null || echo 0)"
rm -f "${COPIED_FILE}"
trap - EXIT
echo "==> Copied ${COPIED_COUNT} unique dylib dependencies"

# ── Fix up dylib paths with install_name_tool ────────────────────────────
echo "==> Fixing dylib paths..."

fix_rpath() {
  local BIN="$1"
  [[ ! -f "${BIN}" ]] && return

  # Change ID of the binary itself (if it's a dylib in Frameworks)
  if [[ "${BIN}" == "${FRAMEWORKS_DIR}/"* ]]; then
    install_name_tool -id "@rpath/$(basename "${BIN}")" "${BIN}" 2>/dev/null || true
  fi

  # Fix references to other dylibs
  while IFS= read -r line; do
    dep_path="$(echo "${line}" | awk '{print $1}')"
    [[ -z "${dep_path}" ]] && continue
    case "${dep_path}" in
      /usr/lib/*|/System/*) continue ;;
    esac
    dep_name="$(basename "${dep_path}")"
    # Only fix if we have this dylib in our Frameworks
    if [[ -f "${FRAMEWORKS_DIR}/${dep_name}" ]]; then
      new_path="@executable_path/../Frameworks/${dep_name}"
      if [[ "${dep_path}" != "${new_path}" ]]; then
        install_name_tool -change "${dep_path}" "${new_path}" "${BIN}" 2>/dev/null || true
      fi
    fi
  done < <(otool -L "${BIN}" 2>/dev/null | tail -n +2)
}

# Fix all binaries and dylibs in the bundle
fix_rpath "${MACOS_DIR}/juicity-gui"

for dylib in "${FRAMEWORKS_DIR}"/*.dylib; do
  [[ -f "${dylib}" ]] && fix_rpath "${dylib}"
done

echo "==> Fixed dylib paths"

# ── Ad-hoc code signing ─────────────────────────────────────────────────
echo "==> Applying ad-hoc code signature..."
if command -v codesign &>/dev/null; then
  # Sign the frameworks first (deepest level)
  for dylib in "${FRAMEWORKS_DIR}"/*.dylib; do
    [[ -f "${dylib}" ]] && codesign --force --sign - "${dylib}" 2>/dev/null || true
  done
  # Sign binaries
  codesign --force --sign - --options runtime \
    --entitlements "${GUI_DIR}/macos/Entitlements.plist" \
    "${MACOS_DIR}/juicity-gui" 2>/dev/null || \
    codesign --force --sign - "${MACOS_DIR}/juicity-gui" 2>/dev/null || true
  # Sign entire bundle
  codesign --force --deep --sign - "${APP_BUNDLE}" 2>/dev/null || true
  echo "  Ad-hoc code signature applied"
else
  echo "  WARNING: codesign not found, skipping"
fi

# ── Output ───────────────────────────────────────────────────────────────
echo ""
echo "============================================"
echo "  Bundle created: ${APP_BUNDLE}"
echo "  Contents:"
echo "    Info.plist"
echo "    MacOS/juicity-gui"
echo "    Frameworks/ ($(ls "${FRAMEWORKS_DIR}" 2>/dev/null | wc -l) dylibs)"
echo "    Resources/ (icons)"
echo "  Size: $(du -sh "${APP_BUNDLE}" | cut -f1)"
echo "============================================"
