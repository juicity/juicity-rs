#!/usr/bin/env bash
# ============================================
# Juicity Windows Bundle Script (MSYS2)
# ============================================
# Packages juicity-gui.exe and its GTK4 runtime dependencies into a
# self-contained distribution folder, including dbus-daemon.exe which
# GLib/GIO requires to start a session bus on Windows.
#
# Run this script from an MSYS2 CLANG64 or CLANGARM64 shell after
# building the project:
#
#   bash gui/windows/bundle.sh
#
# Prerequisites (install via MSYS2 pacman):
#   pacman -S mingw-w64-clang-x86_64-gtk4 \
#             mingw-w64-clang-x86_64-libadwaita \
#             mingw-w64-clang-x86_64-dbus
#
# Environment variables:
#   TARGET_DIR   - Path to cargo release dir (auto-detected if unset)
#   APP_NAME     - Distribution folder name  (default: juicity-gui-windows)
#   MSYS2_PREFIX - MSYS2 environment prefix  (default: /clang64)
# ============================================

set -euo pipefail
[[ -n "${VERBOSE:-}" ]] && set -x

# ── Paths ────────────────────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

TARGET_DIR="${TARGET_DIR:-}"
APP_NAME="${APP_NAME:-juicity-gui-windows}"
MSYS2_PREFIX="${MSYS2_PREFIX:-/clang64}"

# Fallback: probe other common MSYS2 environments
if [[ ! -d "${MSYS2_PREFIX}" ]]; then
  for p in /ucrt64 /mingw64 /clangarm64; do
    if [[ -f "${p}/bin/libgtk-4-1.dll" ]]; then
      MSYS2_PREFIX="${p}"
      break
    fi
  done
fi

if [[ ! -d "${MSYS2_PREFIX}" ]]; then
  echo "ERROR: MSYS2 prefix not found (tried /clang64, /ucrt64, /mingw64, /clangarm64)."
  echo "       Run this script from inside an MSYS2 shell."
  exit 1
fi

# Auto-detect cargo output directory
if [[ -z "${TARGET_DIR}" ]]; then
  TRIPLE="$(rustc -vV 2>/dev/null | grep '^host' | awk '{print $2}')"
  TARGET_DIR="${REPO_ROOT}/target/${TRIPLE}/release"
  if [[ ! -f "${TARGET_DIR}/juicity-gui.exe" ]]; then
    TARGET_DIR="${REPO_ROOT}/target/release"
  fi
fi

echo "==> Juicity Windows Bundle"
echo "    Repo root:    ${REPO_ROOT}"
echo "    Target dir:   ${TARGET_DIR}"
echo "    MSYS2 prefix: ${MSYS2_PREFIX}"

# ── Verify binaries ──────────────────────────────────────────────────────
GUI_BIN="${TARGET_DIR}/juicity-gui.exe"
if [[ ! -f "${GUI_BIN}" ]]; then
  echo "ERROR: juicity-gui.exe not found at ${GUI_BIN}"
  echo "Run 'cargo build --release -p juicity-gui' first."
  exit 1
fi
CLIENT_BIN="${TARGET_DIR}/juicity-client.exe"

# ── Create distribution directory ────────────────────────────────────────
DIST_DIR="${REPO_ROOT}/dist/${APP_NAME}"
rm -rf "${DIST_DIR}"
mkdir -p "${DIST_DIR}"
echo "==> Created distribution directory: ${DIST_DIR}"

# ── Copy main binaries ────────────────────────────────────────────────────
cp "${GUI_BIN}" "${DIST_DIR}/juicity-gui.exe"
if [[ -f "${CLIENT_BIN}" ]]; then
  cp "${CLIENT_BIN}" "${DIST_DIR}/juicity-client.exe"
fi
echo "==> Copied binaries"

# ── Collect DLL dependencies ─────────────────────────────────────────────
echo "==> Collecting DLL dependencies..."

# Collect all DLLs from the MSYS2 prefix that an executable depends on.
# Uses ldd (available inside MSYS2) which prints the full resolved path.
collect_deps() {
  local bin="$1"
  ldd "${bin}" 2>/dev/null \
    | grep -i "=> ${MSYS2_PREFIX}" \
    | awk '{print $3}'
}

copy_dll() {
  local dll_path="$1"
  local dll_name
  dll_name="$(basename "${dll_path}")"
  if [[ -f "${dll_path}" && ! -f "${DIST_DIR}/${dll_name}" ]]; then
    cp "${dll_path}" "${DIST_DIR}/${dll_name}"
    echo "  Copied: ${dll_name}"
    # Recurse into the DLL's own dependencies
    while IFS= read -r dep; do
      copy_dll "${dep}"
    done < <(collect_deps "${DIST_DIR}/${dll_name}")
  fi
}

for bin in "${DIST_DIR}/juicity-gui.exe" "${DIST_DIR}/juicity-client.exe"; do
  [[ -f "${bin}" ]] || continue
  while IFS= read -r dep; do
    copy_dll "${dep}"
  done < <(collect_deps "${bin}")
done

DLL_COUNT="$(find "${DIST_DIR}" -maxdepth 1 -name "*.dll" | wc -l)"
echo "==> Collected ${DLL_COUNT} DLL(s)"

# ── Explicitly bundle dbus-daemon.exe ────────────────────────────────────
# GLib/GIO on Windows looks for dbus-daemon.exe to start a session bus.
# Without it the runtime prints:
#   GLib-GIO-WARNING: win32 session dbus binary not found
# and some GIO features (e.g. GSettings change notifications) may not work.
echo "==> Bundling dbus-daemon..."
DBUS_DAEMON="${MSYS2_PREFIX}/bin/dbus-daemon.exe"
if [[ -f "${DBUS_DAEMON}" ]]; then
  cp "${DBUS_DAEMON}" "${DIST_DIR}/dbus-daemon.exe"
  # Collect any DLLs required by dbus-daemon not already present
  while IFS= read -r dep; do
    copy_dll "${dep}"
  done < <(collect_deps "${DIST_DIR}/dbus-daemon.exe")
  echo "  Bundled dbus-daemon.exe"
else
  echo "  WARNING: dbus-daemon.exe not found at ${DBUS_DAEMON}"
  echo "           Install with: pacman -S mingw-w64-clang-x86_64-dbus"
fi

# ── Bundle GTK4 runtime resources ────────────────────────────────────────
echo "==> Bundling GTK4 runtime resources..."

# GLib schemas
SCHEMAS_SRC="${MSYS2_PREFIX}/share/glib-2.0/schemas"
SCHEMAS_DST="${DIST_DIR}/share/glib-2.0/schemas"
if [[ -d "${SCHEMAS_SRC}" ]]; then
  mkdir -p "${SCHEMAS_DST}"
  cp -R "${SCHEMAS_SRC}/" "${SCHEMAS_DST}/"
  if [[ ! -f "${SCHEMAS_DST}/gschemas.compiled" ]]; then
    glib-compile-schemas "${SCHEMAS_DST}" 2>/dev/null || true
  fi
  echo "  Bundled GLib schemas"
fi

# GDK pixbuf loaders
PIXBUF_SRC="${MSYS2_PREFIX}/lib/gdk-pixbuf-2.0"
PIXBUF_DST="${DIST_DIR}/lib/gdk-pixbuf-2.0"
if [[ -d "${PIXBUF_SRC}" ]]; then
  mkdir -p "${PIXBUF_DST}"
  cp -R "${PIXBUF_SRC}/" "${PIXBUF_DST}/"
  # Loaders are DLLs; collect their own dependencies too
  find "${PIXBUF_DST}" -name "*.dll" | while read -r loader; do
    while IFS= read -r dep; do
      copy_dll "${dep}"
    done < <(collect_deps "${loader}")
  done
  echo "  Bundled GDK pixbuf loaders"
fi

# GTK4 print/media/imm modules
GTK4_SRC="${MSYS2_PREFIX}/lib/gtk-4.0"
GTK4_DST="${DIST_DIR}/lib/gtk-4.0"
if [[ -d "${GTK4_SRC}" ]]; then
  mkdir -p "${GTK4_DST}"
  cp -R "${GTK4_SRC}/" "${GTK4_DST}/"
  echo "  Bundled GTK4 modules"
fi

# Hicolor icon theme
ICON_SRC="${MSYS2_PREFIX}/share/icons/hicolor"
ICON_DST="${DIST_DIR}/share/icons/hicolor"
if [[ -d "${ICON_SRC}" ]]; then
  mkdir -p "${ICON_DST}"
  cp -R "${ICON_SRC}/" "${ICON_DST}/"
  echo "  Bundled hicolor icon theme"
fi

# Adwaita icon theme
ADW_ICON_SRC="${MSYS2_PREFIX}/share/icons/Adwaita"
ADW_ICON_DST="${DIST_DIR}/share/icons/Adwaita"
if [[ -d "${ADW_ICON_SRC}" ]]; then
  mkdir -p "${ADW_ICON_DST}"
  cp -R "${ADW_ICON_SRC}/" "${ADW_ICON_DST}/"
  echo "  Bundled Adwaita icon theme"
fi

# ── Copy example config ──────────────────────────────────────────────────
cp "${REPO_ROOT}/client/examples/config.json" "${DIST_DIR}/client-config.json" 2>/dev/null || true

# ── Output ───────────────────────────────────────────────────────────────
TOTAL_DLLS="$(find "${DIST_DIR}" -maxdepth 1 -name "*.dll" -o -name "*.exe" | grep -c "\.dll$" || true)"
echo ""
echo "============================================"
echo "  Bundle created: ${DIST_DIR}"
echo "  Binaries: juicity-gui.exe, juicity-client.exe"
echo "  dbus-daemon.exe: $([ -f "${DIST_DIR}/dbus-daemon.exe" ] && echo yes || echo MISSING)"
echo "  DLLs: ${TOTAL_DLLS}"
echo "  Size: $(du -sh "${DIST_DIR}" | cut -f1)"
echo "============================================"
