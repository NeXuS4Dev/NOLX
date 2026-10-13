#!/usr/bin/env python3
"""Write the 1 MiB disk image that the boot smoke test attaches to the virtio block device.

The image has three things the kernel checks:

* the bytes `NOLX-BLK-SECTOR0` at the start of sector 0, which the virtio-blk probe reads back
  and compares;
* a protective MBR, so the image is a GPT disk to any reader that looks for one;
* a GUID Partition Table with a primary header in LBA 1, a primary table in LBA 2 to 33, a backup
  table, and a backup header in the last LBA. It holds two partitions, `nolx-data` at LBA 40 to 79
  and `nolx-log` at LBA 128 to 255.

The kernel's round trip writes to sectors 300 to 303, which lie in the usable area between the
partitions, so the table is not overwritten.

Usage: make-test-disk.py OUTPUT
"""

import struct
import sys
import uuid
import zlib
from pathlib import Path

SECTOR = 512
SECTORS = 2048
PATTERN = b"NOLX-BLK-SECTOR0"
ENTRY_SIZE = 128
ENTRY_COUNT = 128
TABLE_BYTES = ENTRY_SIZE * ENTRY_COUNT
TABLE_SECTORS = TABLE_BYTES // SECTOR
PRIMARY_TABLE_LBA = 2
BACKUP_TABLE_LBA = SECTORS - 1 - TABLE_SECTORS
FIRST_USABLE = PRIMARY_TABLE_LBA + TABLE_SECTORS
LAST_USABLE = BACKUP_TABLE_LBA - 1
DISK_GUID = uuid.UUID("5e2b7c1a-3f4d-4a8e-9b61-0d2c7e8f9a10").bytes_le
LINUX_DATA = uuid.UUID("0fc63daf-8483-4772-8e79-3d69d8477de4").bytes_le

# (type GUID, unique GUID, first LBA, last LBA, name)
PARTITIONS = [
    (LINUX_DATA, uuid.UUID(int=0x1001).bytes_le, 40, 79, "nolx-data"),
    (LINUX_DATA, uuid.UUID(int=0x1002).bytes_le, 128, 255, "nolx-log"),
]


def table_bytes() -> bytes:
    table = bytearray(TABLE_BYTES)
    for slot, (type_guid, unique_guid, first, last, name) in enumerate(PARTITIONS):
        entry = bytearray(ENTRY_SIZE)
        entry[0:16] = type_guid
        entry[16:32] = unique_guid
        struct.pack_into("<QQQ", entry, 32, first, last, 0)
        encoded = name.encode("utf-16-le")
        entry[56 : 56 + len(encoded)] = encoded
        table[slot * ENTRY_SIZE : (slot + 1) * ENTRY_SIZE] = entry
    return bytes(table)


def header(my_lba: int, alternate_lba: int, entries_lba: int, table_crc: int) -> bytes:
    h = bytearray(SECTOR)
    h[0:8] = b"EFI PART"
    # Revision 1.0, header size 92, CRC placeholder (zero while the CRC is computed), reserved.
    struct.pack_into("<IIII", h, 8, 0x00010000, 92, 0, 0)
    struct.pack_into("<QQQQ", h, 24, my_lba, alternate_lba, FIRST_USABLE, LAST_USABLE)
    h[56:72] = DISK_GUID
    struct.pack_into("<QIII", h, 72, entries_lba, ENTRY_COUNT, ENTRY_SIZE, table_crc)
    struct.pack_into("<I", h, 16, zlib.crc32(bytes(h[:92])) & 0xFFFFFFFF)
    return bytes(h)


def image() -> bytes:
    img = bytearray(SECTORS * SECTOR)
    img[0 : len(PATTERN)] = PATTERN

    # Protective MBR: one partition of type 0xEE covering the disk after LBA 0.
    img[446 + 4] = 0xEE
    struct.pack_into("<II", img, 446 + 8, 1, SECTORS - 1)
    img[510:512] = b"\x55\xaa"

    table = table_bytes()
    table_crc = zlib.crc32(table) & 0xFFFFFFFF
    img[SECTOR : 2 * SECTOR] = header(1, SECTORS - 1, PRIMARY_TABLE_LBA, table_crc)
    img[PRIMARY_TABLE_LBA * SECTOR : PRIMARY_TABLE_LBA * SECTOR + TABLE_BYTES] = table
    img[BACKUP_TABLE_LBA * SECTOR : BACKUP_TABLE_LBA * SECTOR + TABLE_BYTES] = table
    backup = header(SECTORS - 1, 1, BACKUP_TABLE_LBA, table_crc)
    img[(SECTORS - 1) * SECTOR : SECTORS * SECTOR] = backup
    return bytes(img)


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    Path(sys.argv[1]).write_bytes(image())


if __name__ == "__main__":
    main()
