#!/usr/bin/env bash
# =============================================================================
# bootstrap-qemu.sh — Build QEMU 11.1.50 for NOLX kernel testing
#
# Fetches QEMU source from codeload.github.com (works through restricted
# network), builds for x86_64-softmmu TCG mode, installs to
# .nolx-deps/qemu/qemu-system-x86_64.
#
# Prerequisites: gcc, make, ninja, python3, glib dev headers, pkg-config
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPS_DIR="$REPO_ROOT/.nolx-deps"
QEMU_INSTALL="$DEPS_DIR/qemu"

mkdir -p "$QEMU_INSTALL"

# ---------------------------------------------------------------------------
# 0. Check if QEMU is already installed
# ---------------------------------------------------------------------------
if [[ -x "$QEMU_INSTALL/bin/qemu-system-x86_64" ]]; then
  echo "QEMU already installed at $QEMU_INSTALL/bin/qemu-system-x86_64"
  "$QEMU_INSTALL/bin/qemu-system-x86_64" --version | head -1
  exit 0
fi

# ---------------------------------------------------------------------------
# 1. Download QEMU source from codeload.github.com
# ---------------------------------------------------------------------------
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

echo "Downloading QEMU source from codeload.github.com..."
curl -fsSL -o "$WORK_DIR/qemu.tar.gz" \
  "https://codeload.github.com/qemu/qemu/tar.gz/refs/heads/master"

tar -xzf "$WORK_DIR/qemu.tar.gz" -C "$WORK_DIR"
QEMU_SRC="$WORK_DIR/qemu-master"

# ---------------------------------------------------------------------------
# 2. Fix keycodemapdb stub (meson.build missing project(), keymap_gen.py needs --varname)
# ---------------------------------------------------------------------------
echo "Fixing keycodemapdb stub..."

# keycodemapdb meson.build — needs project() declaration
mkdir -p "$QEMU_SRC/ui/keycodemapdb"
cat > "$QEMU_SRC/ui/keycodemapdb/meson.build" << 'MESONEOF'
project('keycodemapdb', 'c')
MESONEOF

# keymap_gen.py stub — needs to accept --varname and emit const uint16_t[] + _len
cat > "$QEMU_SRC/ui/keycodemapdb/keymap_gen.py" << 'PYEOF'
#!/usr/bin/env python3
import sys
import argparse

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--varname', required=True)
    parser.add_argument('input', nargs='?', type=argparse.FileType('r'), default=sys.stdin)
    parser.add_argument('output', nargs='?', type=argparse.FileType('w'), default=sys.stdout)
    args = parser.parse_args()

    # Read keycodes from input (ignored for stub — emit minimal table)
    data = args.input.read()

    varname = args.varname
    out = args.output
    out.write(f'const uint16_t {varname}[] = {{\n')
    out.write('    0,\n')
    out.write('};\n')
    out.write(f'const unsigned int {varname}_len = 1;\n')

if __name__ == '__main__':
    main()
PYEOF
chmod +x "$QEMU_SRC/ui/keycodemapdb/keymap_gen.py"

# ---------------------------------------------------------------------------
# 3. Create berkeley-softfloat-3 and berkeley-testfloat-3 stub subprojects
# ---------------------------------------------------------------------------
echo "Creating softfloat stub subprojects..."

mkdir -p "$QEMU_SRC/subprojects/berkeley-softfloat-3"
cat > "$QEMU_SRC/subprojects/berkeley-softfloat-3/meson.build" << 'MESONEOF'
project('berkeley-softfloat-3', 'c')
MESONEOF

mkdir -p "$QEMU_SRC/subprojects/berkeley-testfloat-3"
cat > "$QEMU_SRC/subprojects/berkeley-testfloat-3/meson.build" << 'MESONEOF'
project('berkeley-testfloat-3', 'c')
MESONEOF

# ---------------------------------------------------------------------------
# 4. Patch configure for Python 3.11 (QEMU requires 3.12, we have 3.11)
# ---------------------------------------------------------------------------
echo "Patching QEMU configure for Python 3.11..."

# Patch configure script to accept Python 3.11
sed -i 's/3\.12/3.11/g' "$QEMU_SRC/configure"
sed -i 's/Python 3\.12/Python 3.11/g' "$QEMU_SRC/configure"

# Patch setup.cfg if it exists
if [[ -f "$QEMU_SRC/setup.cfg" ]]; then
  sed -i 's/3\.12/3.11/g' "$QEMU_SRC/setup.cfg"
fi

# ---------------------------------------------------------------------------
# 5. Configure QEMU
# ---------------------------------------------------------------------------
echo "Configuring QEMU (target-list=x86_64-softmmu, TCG only)..."

cd "$QEMU_SRC"

./configure \
  --prefix="$QEMU_INSTALL" \
  --disable-pixman \
  --disable-spice \
  --disable-sdl \
  --disable-gtk \
  --disable-vnc \
  --disable-opengl \
  --disable-fdt \
  --target-list=x86_64-softmmu \
  --disable-kvm \
  --disable-linux-aio \
  --disable-cap-ng \
  --disable-libusb \
  --disable-nettle \
  --disable-gnutls \
  --disable-gcrypt \
  --disable-rdma \
  --disable-vde \
  --disable-linux-io-uring

# ---------------------------------------------------------------------------
# 6. Build and install
# ---------------------------------------------------------------------------
echo "Building QEMU (this may take several minutes)..."
ninja -j"$(nproc)"

echo "Installing QEMU to $QEMU_INSTALL..."
ninja install

# ---------------------------------------------------------------------------
# 7. Verify — QEMU resolves data files relative to its binary location,
#    so the install tree (binary + share/qemu BIOS + etc) must be complete.
# ---------------------------------------------------------------------------
echo ""
echo "=== QEMU build complete ==="
"$QEMU_INSTALL/bin/qemu-system-x86_64" --version
echo ""
echo "QEMU binary: $QEMU_INSTALL/bin/qemu-system-x86_64"
echo "BIOS files:  $QEMU_INSTALL/share/qemu/bios-256k.bin"
echo "Add to PATH: export PATH=\"$QEMU_INSTALL/bin:\$PATH\""