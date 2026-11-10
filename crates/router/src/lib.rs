//! L0 command router for SEED SAC.
//!
//! This crate is the deterministic first stage of the command pipeline: raw
//! natural-language input is normalised and matched against a small keyword
//! grammar. Anything that cannot be resolved locally is forwarded to L1 (the
//! LLM) instead of being guessed.

pub mod executor;
pub mod grammar;
pub mod l1_client;

/// A parsed user command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// SIGTERM a process by name.
    Kill(String),
    /// SIGSTOP a process by name.
    Freeze(String),
    /// SIGCONT a process by name.
    Resume(String),
    /// Drop the page cache.
    CleanCache,
    /// Fallback: forward to the LLM in Stage 4.
    QueryL1(String),
}

/// Parse raw user input into an [`Action`].
pub fn parse(input: &str) -> Action {
    grammar::parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_delegates_to_grammar() {
        assert_eq!(parse("kill node"), Action::Kill("node".to_owned()));
    }
}
