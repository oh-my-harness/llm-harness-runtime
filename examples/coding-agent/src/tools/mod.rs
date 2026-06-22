pub mod bash;
pub mod edit;
pub mod grep;
pub mod read;
pub mod write;

/// Phase 1 tool names in declaration order.
pub const PHASE_1_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep"];

#[cfg(test)]
mod tests {
    #[test]
    fn phase_1_has_five_tools() {
        assert_eq!(super::PHASE_1_TOOLS.len(), 5);
    }

    #[test]
    fn phase_1_tools_are_unique() {
        let mut sorted: Vec<&str> = super::PHASE_1_TOOLS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), super::PHASE_1_TOOLS.len());
    }

    #[test]
    fn phase_1_includes_grep() {
        assert!(
            super::PHASE_1_TOOLS.contains(&"grep"),
            "PHASE_1_TOOLS should include grep"
        );
    }
}
