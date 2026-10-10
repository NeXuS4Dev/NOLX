//! Architecturally assigned exception vectors and stack-frame metadata.

/// CPU exception vector range, inclusive of the reserved exception slots.
pub const EXCEPTION_VECTOR_COUNT: u8 = 32;

/// Whether the processor pushes an error code for this exception vector in long mode.
///
/// The list follows the x86-64 exception-frame rules. Reserved vectors remain false unless
/// an architectural extension defines an error code for them.
#[must_use]
pub const fn exception_has_error_code(vector: u8) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_error_code_exceptions_are_exactly_the_known_vectors() {
        for vector in 0..EXCEPTION_VECTOR_COUNT {
            let expected = matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30);
            assert_eq!(
                exception_has_error_code(vector),
                expected,
                "vector {vector}"
            );
        }
    }
}
