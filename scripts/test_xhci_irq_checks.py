#!/usr/bin/env python3
"""Reject misleading interrupt evidence (input output alone, polling, stale routes/counts)."""
import unittest
from xhci_irq_checks import verify_interrupts

BASE = '''xhci: IRQ-only admission verified handle=0
xhci: handle=0 MSI-X vector=0xd0 delivered=1, deferred HID
xhci: irq work handle=0 vector=0xd0 delivered=2 reports=1
'''


class InterruptEvidence(unittest.TestCase):
    def test_live_message_route_and_irq_admitted_batch(self):
        self.assertEqual(verify_interrupts(BASE), (1, 1))

    def test_input_console_output_alone_is_not_evidence(self):
        with self.assertRaises(AssertionError):
            verify_interrupts('input: usb keyboard usage=0x04 pressed=true')
        with self.assertRaises(AssertionError):
            verify_interrupts(BASE.split('xhci: irq work')[0])

    def test_poll_fallback_and_zero_delivery_cannot_pass(self):
        for text in [BASE+'polled fallback', BASE+'xhci: polled work', BASE.replace('delivered=2', 'delivered=0')]:
            with self.assertRaises(AssertionError):
                verify_interrupts(text)

    def test_stale_rearm_and_wrong_vector_cannot_pass(self):
        for text in [BASE+BASE, BASE.replace('0xd0', '0xd1')]:
            with self.assertRaises(AssertionError):
                verify_interrupts(text)

    def test_a_probe_followed_by_600_polled_reports_cannot_pass(self):
        with self.assertRaises(AssertionError):
            verify_interrupts(BASE.replace('reports=1', 'reports=604'), reports=604)


if __name__ == '__main__':
    unittest.main()
