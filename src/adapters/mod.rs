//! Agent adapters for selx. Everything agent-specific lives here; the core
//! layer only knows the [`crate::core::Adapter`] trait.

pub mod claude_ide;
pub mod herdr;
pub mod log;
pub mod text_inject;

pub use claude_ide::ClaudeIdeAdapter;
pub use log::LogAdapter;
pub use text_inject::TextInjectAdapter;
