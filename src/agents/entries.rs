//! Structural search metadata. Legacy text visitors discard these annotations.
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SearchKind {
    User,
    Assistant,
    ToolInput,
    ToolOutput,
    #[default]
    Unknown,
}

impl SearchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolInput => "tool-input",
            Self::ToolOutput => "tool-output",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_role(role: super::MessageRole) -> Self {
        match role {
            super::MessageRole::User => Self::User,
            super::MessageRole::Assistant => Self::Assistant,
        }
    }
}

pub type TypedVisitor<'a> = dyn FnMut(SearchKind, &str) -> bool + 'a;

/// Numbers nonempty searchable source records, then their fragments. Metadata
/// records do not consume a number. Filters must be applied AFTER this visitor.
pub struct RecordVisitor<'a> {
    record: usize,
    prefix: Sha256,
    visit: &'a mut dyn FnMut(usize, usize, SearchKind, &str, &str) -> bool,
}

impl<'a> RecordVisitor<'a> {
    pub fn new(visit: &'a mut dyn FnMut(usize, usize, SearchKind, &str, &str) -> bool) -> Self {
        Self {
            record: 0,
            prefix: Sha256::new(),
            visit,
        }
    }

    pub fn record(&mut self, f: impl FnOnce(&mut TypedVisitor<'_>) -> bool) -> bool {
        let mut fragment = 0;
        f(&mut |kind, text| {
            if text.is_empty() {
                return true;
            }
            if fragment == 0 {
                self.record += 1;
            }
            fragment += 1;
            // Include the searchable prefix and record boundaries: editing or
            // removing earlier context must not silently open an unrelated hit.
            self.prefix.update((self.record as u64).to_le_bytes());
            self.prefix.update((fragment as u64).to_le_bytes());
            self.prefix.update(kind.as_str());
            self.prefix.update([0]);
            self.prefix.update((text.len() as u64).to_le_bytes());
            self.prefix.update(text.as_bytes());
            let fingerprint = format!("{:x}", self.prefix.clone().finalize());
            (self.visit)(self.record, fragment, kind, text, &fingerprint)
        })
    }
}
