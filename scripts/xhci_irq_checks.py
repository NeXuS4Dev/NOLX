"""Shared executable checks for native xHCI MSI/MSI-X, never inferred from HID output alone."""
import re


MANAGED_FIRST = 0x50
MANAGED_LAST = 0xDF


def verify_interrupts(text, reports=1, controllers=1, arms=1, transport=None, unique_vectors=True):
    assert 'polled fallback' not in text and 'xhci: polled work' not in text, text
    armed = re.findall(r'xhci: handle=(\d+) (MSI-X|MSI) vector=(0x[\da-f]+) delivered=(\d+), deferred HID', text)
    batches = re.findall(r'xhci: irq work handle=(\d+) vector=(0x[\da-f]+) delivered=(\d+) reports=(\d+)', text)
    assert len(armed) >= arms and len({a[0] for a in armed}) >= controllers, f"IRQ arms expected {arms}/{controllers} routes/controllers, got {armed}"
    first_vector = {}
    armed_vectors = {}
    previous = {}
    for handle, kind, vector, delivered in armed:
        numeric_handle = int(handle)
        numeric_vector = int(vector, 16)
        assert 0 <= numeric_handle < 16, armed
        assert MANAGED_FIRST <= numeric_vector <= MANAGED_LAST, armed
        assert transport is None or kind == transport, armed
        # Vector reuse across handles is allowed only when the caller knows the earlier arm
        # was checked-released; otherwise all observed controllers must have unique vectors.
        first_vector.setdefault(handle, vector)
        armed_vectors.setdefault(handle, set()).add(vector)
        assert int(delivered) > previous.get(handle, 0), armed
        previous[handle] = int(delivered)
    if unique_vectors:
        assert len(set(first_vector.values())) == len(first_vector), f"two live controllers shared a vector: {first_vector}"
    assert text.count('xhci: IRQ-only admission verified handle=') == len(armed), text
    previous = {}
    for handle, vector, delivered, count in batches:
        assert vector in armed_vectors.get(handle, set()), batches
        assert MANAGED_FIRST <= int(vector, 16) <= MANAGED_LAST, batches
        assert int(count) > 0 and int(delivered) > 0, batches
        assert int(delivered) >= previous.get(handle, 0), batches
        previous[handle] = int(delivered)
    total = sum(int(batch[3]) for batch in batches)
    assert total >= reports, f"IRQ report proof: expected >= {reports}, got {total} in {len(batches)} batches; last={batches[-3:]}"
    # Serial QMP key injection waits for each transition: 600 ordered reports cannot be
    # attributed to just the startup probe or a one-off interrupt followed by timer polling.
    if reports >= 600:
        maximum = max(int(b[2]) for b in batches)
        assert maximum >= 600, f"IRQ delivery proof: expected >= 600, got {maximum}; reports={total}, last={batches[-3:]}"
    return len(armed), len(batches)
