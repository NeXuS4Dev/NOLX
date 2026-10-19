//! Live interrupt-IN completion affinity and conservative command-ring reuse rules.
use crate::{device, Trb};
/// The exact outstanding transfer whose buffer may be consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Enabled device slot.
    pub slot: u8,
    /// xHCI device context index, not the USB endpoint address.
    pub endpoint: u8,
    /// Outstanding Normal TRB bus address; zero is retired and never matches.
    pub pointer: u64,
    /// Expected boot report length.
    pub length: u32,
}
/// A completion never independently authorizes DMA reclamation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// Another slot, endpoint, retired/stale TRB, or event type: ignore entirely.
    Unrelated,
    /// A complete report can be read, then a new transfer posted.
    Report,
    /// Incomplete report: do not decode stale trailing bytes; posting another transfer is safe.
    Short,
    /// Failed transfer: retire input, stop rearming, retain DMA until checked slot/controller stop.
    Failed(u8),
    /// Residual exceeds the submitted length: retire without reading the DMA buffer.
    InvalidResidual,
}
/// Match every affinity field before considering completion code or residual.
#[must_use]
pub fn classify(pending: Pending, event: &Trb) -> Completion {
    if pending.pointer == 0
        || pending.pointer & 15 != 0
        || pending.slot == 0
        || pending.endpoint == 0
        || pending.endpoint > 31
        || event[12] & 4 != 0
        || pending.length == 0
        || crate::trb_type(event) != crate::TRB_TRANSFER_EVENT
        || crate::event_slot(event) != pending.slot
        || device::endpoint(event) != pending.endpoint
        || device::pointer(event) != pending.pointer
    {
        return Completion::Unrelated;
    }
    let code = crate::completion_code(event);
    if !matches!(code, 1 | 13) {
        return Completion::Failed(code);
    }
    let residual = device::residual(event);
    if residual > pending.length {
        Completion::InvalidResidual
    } else if residual != 0 {
        Completion::Short
    } else {
        Completion::Report
    }
}
/// Known terminal command completions permit another command, but never imply successful cleanup.
/// Stopped/aborted command rings, reserved/unknown codes and missing completion require full reset.
#[must_use]
pub const fn command_reusable(code: u8) -> bool {
    matches!(code, 1..=23 | 26..=36) && code != 16
}
#[cfg(test)]
mod tests {
    use super::*;
    fn pending() -> Pending {
        Pending {
            slot: 2,
            endpoint: 3,
            pointer: 0x1040,
            length: 8,
        }
    }
    fn event(code: u8, residual: u32) -> Trb {
        device::trb(
            0x1040,
            u32::from(code) << 24 | residual,
            2 << 24 | 3 << 16 | u32::from(crate::TRB_TRANSFER_EVENT) << 10,
            true,
        )
    }
    #[test]
    fn stale_retired_or_foreign_completions_cannot_read_or_rearm() {
        let e = event(1, 0);
        for p in [
            Pending {
                pointer: 0,
                ..pending()
            },
            Pending {
                pointer: 0x1041,
                ..pending()
            },
            Pending {
                slot: 0,
                ..pending()
            },
            Pending {
                endpoint: 32,
                ..pending()
            },
            Pending {
                pointer: 0x1050,
                ..pending()
            },
            Pending {
                slot: 1,
                ..pending()
            },
            Pending {
                endpoint: 5,
                ..pending()
            },
            Pending {
                length: 0,
                ..pending()
            },
        ] {
            assert_eq!(classify(p, &e), Completion::Unrelated);
        }
        let mut ed = e;
        ed[12] |= 4;
        assert_eq!(classify(pending(), &ed), Completion::Unrelated);
        let mut e = e;
        e[13] = 0;
        assert_eq!(classify(pending(), &e), Completion::Unrelated);
    }
    #[test]
    fn short_reports_never_expose_stale_dma_tails_and_invalid_residuals_retire() {
        for code in [1, 13] {
            assert_eq!(classify(pending(), &event(code, 0)), Completion::Report);
            for n in 1..=8 {
                assert_eq!(classify(pending(), &event(code, n)), Completion::Short);
            }
            assert_eq!(
                classify(pending(), &event(code, 9)),
                Completion::InvalidResidual
            );
        }
    }
    #[test]
    fn every_unsuccessful_transfer_retires_without_rearming() {
        for code in 0..=255 {
            if !matches!(code, 1 | 13) {
                assert_eq!(
                    classify(pending(), &event(code, 0)),
                    Completion::Failed(code)
                );
            }
        }
    }
    #[test]
    fn stopped_aborted_unknown_or_reserved_command_outcomes_cannot_reuse_the_ring() {
        for code in [0, 16, 24, 25, 37, 191, 255] {
            assert!(!command_reusable(code));
        }
        for code in [1, 5, 9, 11, 17, 19, 26, 36] {
            assert!(command_reusable(code));
        }
    }
}
