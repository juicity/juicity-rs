#!/usr/bin/env bash
# ============================================
# Juicity-RS Build Script for Unix/Linux
# ============================================

set -e

echo "[Juicity-RS] Building project..."

# Check if Rust/Cargo is installed
if ! command -v cargo &>/dev/null; then
    echo "[ERROR] Cargo not found! Please install Rust from https://rustup.rs/"
    exit 1
fi

# Build in release mode by default
BUILD_MODE="${1:-release}"

case "$BUILD_MODE" in
    debug)
        echo "[Juicity-RS] Build mode: debug"
        cargo build
        ;;
    release)
        echo "[Juicity-RS] Build mode: release"
        cargo build --release
        ;;
    *)
        echo "[ERROR] Unknown build mode: $BUILD_MODE. Use \"debug\" or \"release\"."
        exit 1
        ;;
esac

echo ""
echo "[Juicity-RS] Build successful!"
echo ""
echo "Binaries:"
echo "  juicity-server: target/${BUILD_MODE}/juicity-server"
echo "  juicity-client: target/${BUILD_MODE}/juicity-client"
