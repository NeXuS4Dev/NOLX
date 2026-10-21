#!/usr/bin/env bash
# =============================================================================
# bootstrap-rust.sh — Install the pinned Rust toolchain for NOLX development
#
# Installs via pip from pypi.org (the `arena-rust-toolchain` package, which works
# through the restricted network this sandbox has). The toolchain is extracted to
# ${RUST_PREFIX:-/opt/rust/prefix}.
#
# Prerequisites: pip3, python3, tar, and zstd (or the `zstandard` Python module,
# which this script installs when `zstd` is not on PATH).
# =============================================================================
set -euo pipefail

VERSION="${NOLX_RUST_VERSION:-1.97.0}"
RUST_PREFIX="${RUST_PREFIX:-/opt/rust/prefix}"

# ---------------------------------------------------------------------------
# 0. Already installed?
# ---------------------------------------------------------------------------
if [[ -x "$RUST_PREFIX/bin/cargo" ]] && [[ -x "$RUST_PREFIX/bin/rustc" ]]; then
  echo "Rust already installed at $RUST_PREFIX/bin"
  "$RUST_PREFIX/bin/rustc" --version
  "$RUST_PREFIX/bin/cargo" --version
  exit 0
fi

# ---------------------------------------------------------------------------
# 1. Install the package and every split data part.
#
# `arena-rust-toolchain` alone is a 4 KB stub: the payload lives in the
# `data1..data3` extras, and without them extraction fails. The previous version
# of this script installed only the stub, printed "installation complete", and
# left no compiler behind.
# ---------------------------------------------------------------------------
PIP_FLAGS=()
if pip3 install --help 2>/dev/null | grep -q -- '--break-system-packages'; then
  PIP_FLAGS+=(--break-system-packages)
fi
echo "Installing arena-rust-toolchain ${VERSION} [all] from pypi.org..."
pip3 install "${PIP_FLAGS[@]}" --upgrade "arena-rust-toolchain[all]==${VERSION}"

# ---------------------------------------------------------------------------
# 2. Extract. `zstd` may be missing from a minimal image; the Python module can
#    decompress the stream just as well.
# ---------------------------------------------------------------------------
PREFIX_PARENT="$(dirname "$RUST_PREFIX")"
mkdir -p "$PREFIX_PARENT" 2>/dev/null || sudo mkdir -p "$PREFIX_PARENT"

WRITE_CMD=(bash -c)
if [[ ! -w "$PREFIX_PARENT" ]]; then
  echo "Extracting through sudo: $PREFIX_PARENT is not writable by $(id -un)"
  WRITE_CMD=(sudo bash -c)
fi

echo "Extracting to $RUST_PREFIX..."
"${WRITE_CMD[@]}" "RUST_PREFIX='$RUST_PREFIX' python3 -" <<'PY'
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile

target = pathlib.Path(os.environ["RUST_PREFIX"])
target.mkdir(parents=True, exist_ok=True)

import importlib
parts = []
for package in ("arena_rust_toolchain_data1", "arena_rust_toolchain_data2",
                "arena_rust_toolchain_data3", "arena_rust_toolchain_data4"):
    try:
        module = importlib.import_module(package)
    except ImportError:
        continue
    directory = pathlib.Path(module.__path__[0])
    parts.extend(sorted(p for p in directory.iterdir()
                        if p.is_file() and not p.name.startswith("_")))
if not parts:
    raise SystemExit("no toolchain payload parts were installed")

combined = pathlib.Path("/tmp/nolx-toolchain.tar.zst")
with open(combined, "wb") as output:
    for part in parts:
        with open(part, "rb") as source:
            shutil.copyfileobj(source, output, length=1024 * 1024)

if shutil.which("zstd"):
    with open(combined, "rb") as source, \
            subprocess.Popen(["zstd", "-d", "-c"], stdin=source,
                             stdout=subprocess.PIPE) as zstd, \
            tarfile.open(fileobj=zstd.stdout, mode="r|") as archive:
        archive.extractall(target)
else:
    try:
        import zstandard
    except ImportError:
        raise SystemExit("neither `zstd` nor the `zstandard` module is available")
    with open(combined, "rb") as source, \
            zstandard.ZstdDecompressor().stream_reader(source) as reader, \
            tarfile.open(fileobj=reader, mode="r|") as archive:
        archive.extractall(target)
combined.unlink(missing_ok=True)
PY

# ---------------------------------------------------------------------------
# 3. Verify. A script that prints success without a compiler is worse than one
#    that fails, so this is a hard check.
# ---------------------------------------------------------------------------
if [[ ! -x "$RUST_PREFIX/bin/cargo" || ! -x "$RUST_PREFIX/bin/rustc" ]]; then
  echo "ERROR: extraction finished but $RUST_PREFIX/bin has no cargo/rustc." >&2
  exit 1
fi
echo
echo "=== Rust installation complete ==="
"$RUST_PREFIX/bin/rustc" --version
"$RUST_PREFIX/bin/cargo" --version
echo
echo "Add to PATH: export PATH=\"$RUST_PREFIX/bin:\$PATH\""
echo "Set: export RUSTUP_HOME=\"$RUST_PREFIX\" CARGO_HOME=\"$RUST_PREFIX\""
echo "Then: source scripts/nolx-env.sh"
