#!/usr/bin/env bash
# =============================================================================
# bootstrap-rust.sh — Install Rust 1.97.0 for NOLX kernel development
#
# Installs via pip from pypi.org (arena-rust-toolchain package, works through
# restricted network). Installs to /opt/rust/prefix by default.
#
# Prerequisites: pip3, python3
# =============================================================================
set -euo pipefail

RUST_PREFIX="${RUST_PREFIX:-/opt/rust/prefix}"

# ---------------------------------------------------------------------------
# 0. Check if Rust is already installed
# ---------------------------------------------------------------------------
if [[ -x "$RUST_PREFIX/bin/cargo" ]] && [[ -x "$RUST_PREFIX/bin/rustc" ]]; then
  echo "Rust already installed at $RUST_PREFIX/bin"
  "$RUST_PREFIX/bin/rustc" --version
  "$RUST_PREFIX/bin/cargo" --version
  exit 0
fi

# ---------------------------------------------------------------------------
# 1. Install arena-rust-toolchain from pypi.org
# ---------------------------------------------------------------------------
echo "Installing arena-rust-toolchain 1.97.0 from pypi.org..."
pip3 install --break-system-packages arena-rust-toolchain 2>/dev/null || \
pip3 install arena-rust-toolchain

# ---------------------------------------------------------------------------
# 2. Verify installation
# ---------------------------------------------------------------------------
echo ""
echo "=== Rust installation complete ==="
if [[ -x "$RUST_PREFIX/bin/cargo" ]]; then
  "$RUST_PREFIX/bin/cargo" --version
  "$RUST_PREFIX/bin/rustc" --version
  echo ""
  echo "Rust binaries: $RUST_PREFIX/bin"
  echo "Add to PATH: export PATH=\"$RUST_PREFIX/bin:\$PATH\""
  echo "Set: export RUSTUP_HOME=\"$RUST_PREFIX\""
  echo "Set: export CARGO_HOME=\"$RUST_PREFIX\""
else
  echo "WARNING: Rust not found at $RUST_PREFIX/bin"
  echo "Check where pip installed it and update RUST_PREFIX accordingly."
  exit 1
fi