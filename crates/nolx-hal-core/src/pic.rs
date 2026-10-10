//! Interrupt-vector mapping for the dual 8259 Programmable Interrupt Controller.

/// First vector assigned to the master PIC (IRQ0–IRQ7).
pub const MASTER_VECTOR_OFFSET: u8 = 32;
/// First vector assigned to the slave PIC (IRQ8–IRQ15).
pub const SLAVE_VECTOR_OFFSET: u8 = 40;
/// Number of hardware IRQ inputs across the master and slave PICs.
pub const IRQ_COUNT: u8 = 16;

/// Convert a PIC IRQ line into its remapped IDT vector.
#[must_use]
pub const fn vector_for_irq(irq: u8) -> Option<u8> {
    if irq < 8 {
        Some(MASTER_VECTOR_OFFSET + irq)
    } else if irq < IRQ_COUNT {
        Some(SLAVE_VECTOR_OFFSET + irq - 8)
    } else {
        None
    }
}

/// Convert a remapped PIC vector into its IRQ line.
#[must_use]
pub const fn irq_for_vector(vector: u8) -> Option<u8> {
    if vector >= MASTER_VECTOR_OFFSET && vector < MASTER_VECTOR_OFFSET + 8 {
        Some(vector - MASTER_VECTOR_OFFSET)
    } else if vector >= SLAVE_VECTOR_OFFSET && vector < SLAVE_VECTOR_OFFSET + 8 {
        Some(vector - SLAVE_VECTOR_OFFSET + 8)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_legacy_irq_maps_to_a_distinct_vector_and_back() {
        for irq in 0..IRQ_COUNT {
            let vector = vector_for_irq(irq).expect("valid IRQ");
            assert_eq!(irq_for_vector(vector), Some(irq));
        }
        assert_eq!(vector_for_irq(IRQ_COUNT), None);
    }

    #[test]
    fn vectors_outside_the_pic_ranges_are_not_irq_vectors() {
        assert_eq!(irq_for_vector(31), None);
        assert_eq!(irq_for_vector(48), None);
        assert_eq!(irq_for_vector(255), None);
    }
}
