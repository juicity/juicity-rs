#!/usr/bin/env bash
# ============================================
# Juicity macOS .app Bundle Script
# ============================================
# Creates a self-contained Juicity.app bundle with all GTK4/libadwaita
# dylib dependencies and runtime resources bundled inside.
#
# Prerequisites:
#   - macOS build already completed (release binaries exist)
#   - GTK4 + libadwaita installed via Homebrew
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
CLIENT="${TARGET_DIR}/juicity-client"
if [[ ! -f "${BINARY}" ]]; then
  echo "ERROR: juicity-gui binary not found at ${BINARY}"
  echo "Run 'cargo build --release -p juicity-gui' first."
  exit 1
fi
if [[ ! -f "${CLIENT}" ]]; then
  echo "WARNING: juicity-client binary not found at ${CLIENT}"
  echo "Run 'cargo build --release -p juicity-client' first."
  CLIENT=""
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

if [[ -n "${CLIENT}" ]]; then
  cp "${CLIENT}" "${MACOS_DIR}/juicity-client"
  chmod +x "${MACOS_DIR}/juicity-client"
fi

# Copy shadowsocks-rust binaries (sslocal, ssurl) if available
SS_DIR="${SS_DIR:-}"
if [[ -n "${SS_DIR}" && -d "${SS_DIR}" ]]; then
  if [[ -f "${SS_DIR}/sslocal" ]]; then
    cp "${SS_DIR}/sslocal" "${MACOS_DIR}/"
    chmod +x "${MACOS_DIR}/sslocal"
  fi
  if [[ -f "${SS_DIR}/ssurl" ]]; then
    cp "${SS_DIR}/ssurl" "${MACOS_DIR}/"
    chmod +x "${MACOS_DIR}/ssurl"
  fi
fi

echo "==> Copied binaries to MacOS/"

# ── Generate icon (icns) from SVG ───────────────────────────────────────
# We use the already-generated PNGs from the build script, or generate one.
ICNS_PATH="${RESOURCES_DIR}/icon.icns"
if command -v iconutil &>/dev/null; then
  # Build iconset directory from the PNGs generated during build
  ICONSET_DIR="${REPO_ROOT}/dist/Juicity.iconset"
  rm -rf "${ICONSET_DIR}"
  mkdir -p "${ICONSET_DIR}"

  # Use PNGs from build script output
  BUILD_OUT_DIR="$(dirname "$(find "${TARGET_DIR}" -name "build" -type d 2>/dev/null | head -1)" 2>/dev/null || true)"
  # Generate icons from SVG at various sizes
  if command -v sips &>/dev/null; then
    # Generate a 1024x1024 PNG from SVG (convert via rsvg or cairosvg)
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

      # Convert iconset to icns
      iconutil -c icns "${ICONSET_DIR}" -o "${ICNS_PATH}" || {
        echo "WARNING: iconutil failed. Proceeding without icon."
      }
      rm -rf "${ICONSET_DIR}"
    fi
  fi
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

# We need to process multiple binaries
BINS_TO_PROCESS=("${MACOS_DIR}/juicity-gui")
if [[ -n "${CLIENT}" ]]; then
  BINS_TO_PROCESS+=("${MACOS_DIR}/juicity-client")
fi
if [[ -f "${MACOS_DIR}/sslocal" ]]; then
  BINS_TO_PROCESS+=("${MACOS_DIR}/sslocal")
fi
if [[ -f "${MACOS_DIR}/ssurl" ]]; then
  BINS_TO_PROCESS+=("${MACOS_DIR}/ssurl")
fi

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
[[ -f "${MACOS_DIR}/juicity-client" ]] && fix_rpath "${MACOS_DIR}/juicity-client"
[[ -f "${MACOS_DIR}/sslocal" ]] && fix_rpath "${MACOS_DIR}/sslocal"
[[ -f "${MACOS_DIR}/ssurl" ]] && fix_rpath "${MACOS_DIR}/ssurl"

for dylib in "${FRAMEWORKS_DIR}"/*.dylib; do
  [[ -f "${dylib}" ]] && fix_rpath "${dylib}"
done

echo "==> Fixed dylib paths"

# ── Bundle GTK4 runtime resources ────────────────────────────────────────
echo "==> Bundling GTK4 runtime resources..."

# Find Homebrew prefix
BREW_PREFIX=""
if command -v brew &>/dev/null; then
  BREW_PREFIX="$(brew --prefix 2>/dev/null || true)"
fi
if [[ -z "${BREW_PREFIX}" ]]; then
  # Fallback: common paths
  for p in /opt/homebrew /usr/local; do
    if [[ -f "${p}/lib/libgtk-4.dylib" ]]; then
      BREW_PREFIX="${p}"
      break
    fi
  done
fi

if [[ -z "${BREW_PREFIX}" ]]; then
  echo "WARNING: Homebrew prefix not found. GTK4 runtime resources will not be bundled."
else
  echo "  Homebrew prefix: ${BREW_PREFIX}"

  # GLib schemas
  SCHEMAS_SRC="${BREW_PREFIX}/share/glib-2.0/schemas"
  SCHEMAS_DST="${RESOURCES_DIR}/share/glib-2.0/schemas"
  if [[ -d "${SCHEMAS_SRC}" ]]; then
    mkdir -p "${SCHEMAS_DST}"
    cp -R "${SCHEMAS_SRC}/" "${SCHEMAS_DST}/"
    # Ensure compiled schemas exist
    if [[ ! -f "${SCHEMAS_DST}/gschemas.compiled" ]]; then
      if command -v glib-compile-schemas &>/dev/null; then
        glib-compile-schemas "${SCHEMAS_DST}" || true
      fi
    fi
    echo "  Bundled GLib schemas"
  fi

  # GDK pixbuf loaders
  PIXBUF_LIB="${BREW_PREFIX}/lib/gdk-pixbuf-2.0"
  PIXBUF_DST="${RESOURCES_DIR}/lib/gdk-pixbuf-2.0"
  if [[ -d "${PIXBUF_LIB}" ]]; then
    mkdir -p "${PIXBUF_DST}"
    cp -R "${PIXBUF_LIB}/" "${PIXBUF_DST}/"
    echo "  Bundled GDK pixbuf loaders"
  fi

  # GTK4 modules (print backends, media backends, imm modules)
  GTK_MODULES_SRC="${BREW_PREFIX}/lib/gtk-4.0"
  GTK_MODULES_DST="${RESOURCES_DIR}/lib/gtk-4.0"
  if [[ -d "${GTK_MODULES_SRC}" ]]; then
    mkdir -p "${GTK_MODULES_DST}"
    cp -R "${GTK_MODULES_SRC}/" "${GTK_MODULES_DST}/"
    echo "  Bundled GTK4 modules"
  fi

  # Hicolor icon theme (minimal — just the index.theme + some fallback)
  ICON_SRC="${BREW_PREFIX}/share/icons/hicolor"
  ICON_DST="${RESOURCES_DIR}/share/icons/hicolor"
  if [[ -d "${ICON_SRC}" ]]; then
    mkdir -p "${ICON_DST}"
    cp -R "${ICON_SRC}/" "${ICON_DST}/"
    echo "  Bundled hicolor icon theme"
  fi

  # Adwaita icon theme (needed by GTK4)
  ADW_ICON_SRC="${BREW_PREFIX}/share/icons/Adwaita"
  ADW_ICON_DST="${RESOURCES_DIR}/share/icons/Adwaita"
  if [[ -d "${ADW_ICON_SRC}" ]]; then
    mkdir -p "${ADW_ICON_DST}"
    cp -R "${ADW_ICON_SRC}/" "${ADW_ICON_DST}/"
    echo "  Bundled Adwaita icon theme"
  fi

  # dbus-daemon binary (GLib/GIO needs this to start a session bus;
  # without it GTK4 prints "win32 session dbus binary not found" or
  # the macOS equivalent and some GIO features may not work).
  DBUS_BIN="${BREW_PREFIX}/bin/dbus-daemon"
  if [[ -f "${DBUS_BIN}" ]]; then
    cp "${DBUS_BIN}" "${MACOS_DIR}/dbus-daemon"
    chmod +x "${MACOS_DIR}/dbus-daemon"
    # Bundle any additional dylibs required by dbus-daemon that were not
    # already pulled in while processing the main application binaries.
    while IFS= read -r dep; do
      [[ -z "${dep}" ]] && continue
      case "${dep}" in /usr/lib/*|/System/*) continue ;; esac
      dep_name="$(basename "${dep}")"
      if [[ -f "${dep}" && ! -f "${FRAMEWORKS_DIR}/${dep_name}" ]]; then
        cp -n "${dep}" "${FRAMEWORKS_DIR}/${dep_name}" 2>/dev/null || true
        chmod 644 "${FRAMEWORKS_DIR}/${dep_name}" 2>/dev/null || true
        fix_rpath "${FRAMEWORKS_DIR}/${dep_name}"
        echo "  Copied dbus dep: ${dep_name}"
      fi
    done < <(otool -L "${MACOS_DIR}/dbus-daemon" 2>/dev/null | tail -n +2 | awk '{print $1}')
    fix_rpath "${MACOS_DIR}/dbus-daemon"
    echo "  Bundled dbus-daemon"
  else
    echo "  WARNING: dbus-daemon not found at ${DBUS_BIN}"
    echo "           Install with: brew install dbus"
  fi
fi

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
  [[ -f "${MACOS_DIR}/juicity-client" ]] && \
    codesign --force --sign - "${MACOS_DIR}/juicity-client" 2>/dev/null || true
  [[ -f "${MACOS_DIR}/dbus-daemon" ]] && \
    codesign --force --sign - "${MACOS_DIR}/dbus-daemon" 2>/dev/null || true
  # Sign entire bundle
  codesign --force --deep --sign - "${APP_BUNDLE}" 2>/dev/null || true
  echo "  Ad-hoc code signature applied"
else
  echo "  WARNING: codesign not found, skipping"
fi

# ── Copy config file ─────────────────────────────────────────────────────
cp "${REPO_ROOT}/client/examples/config.json" "${RESOURCES_DIR}/client-config.json" 2>/dev/null || true

# ── Output ───────────────────────────────────────────────────────────────
echo ""
echo "============================================"
echo "  Bundle created: ${APP_BUNDLE}"
echo "  Contents:"
echo "    Info.plist"
echo "    MacOS/juicity-gui"
echo "    MacOS/juicity-client"
echo "    MacOS/dbus-daemon  (GLib/GIO session bus)"
echo "    Frameworks/ ($(ls "${FRAMEWORKS_DIR}" 2>/dev/null | wc -l) dylibs)"
echo "    Resources/ (icons, schemas, themes)"
echo "  Size: $(du -sh "${APP_BUNDLE}" | cut -f1)"
echo "============================================"
