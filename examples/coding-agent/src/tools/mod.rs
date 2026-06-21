pub mod bash;
pub mod edit;
pub mod read;
pub mod write;

/// Phase 1 tool names in declaration order.
pub const PHASE_1_TOOLS: &[&str] = &["read", "bash", "edit", "write"];

#[cfg(test)]
mod tests {
    #[test]
    fn phase_1_has_four_tools() {
        assert_eq!(super::PHASE_1_TOOLS.len(), 4);
    }

    #[test]
    fn phase_1_tools_are_unique() {
        let mut sorted: Vec<&str> = super::PHASE_1_TOOLS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), super::PHASE_1_TOOLS.len());
    }
}
