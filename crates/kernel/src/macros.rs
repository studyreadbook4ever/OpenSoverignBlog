//! Portable, inert AI macro blocks.
//!
//! Macro blocks are fenced JSON inside required Markdown. The kernel parses and
//! validates them but never invokes a model. An external capability-scoped
//! adapter resolves blocks, then submits the complete resulting Markdown as a
//! normal AI2AI revision proposal. Publication remains a separate operation.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{AiActor, AiPolicySnapshot, ContextReceipt};

pub const MACRO_SPEC_VERSION: &str = "1.0";
pub const MACRO_FENCE: &str = "```osb-ai-macro";
const MAX_MACRO_JSON_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroPhase {
    Draft,
    Publish,
    Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MacroInvocation {
    pub spec_version: String,
    pub invocation_id: Uuid,
    pub macro_id: String,
    pub definition_version: String,
    pub phase: MacroPhase,
    pub inputs: serde_json::Value,
    pub actor: AiActor,
    pub policy: AiPolicySnapshot,
    #[serde(default)]
    pub context_receipts: Vec<ContextReceipt>,
    #[serde(default = "default_review")]
    pub requires_review: bool,
    pub created_at: DateTime<Utc>,
}

impl MacroInvocation {
    pub fn validate(&self) -> Result<(), MacroError> {
        if self.spec_version != MACRO_SPEC_VERSION {
            return Err(MacroError::UnsupportedVersion);
        }
        if self.invocation_id.is_nil()
            || !qualified_id(&self.macro_id)
            || !semantic_version(&self.definition_version)
            || self.actor.id.trim().is_empty()
            || self.actor.id.len() > 300
            || self.created_at > Utc::now() + Duration::minutes(5)
            || serde_json::to_vec(&self.inputs)
                .is_ok_and(|value| value.len() > MAX_MACRO_JSON_BYTES)
            || self.context_receipts.len() > 10_000
        {
            return Err(MacroError::InvalidInvocation);
        }
        self.policy
            .validate_for_provider(self.actor.provider.as_deref())
            .map_err(|_| MacroError::InvalidInvocation)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MacroBlock {
    pub byte_start: usize,
    pub byte_end: usize,
    pub invocation: MacroInvocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MacroResolution {
    pub invocation_id: Uuid,
    pub replacement_markdown: String,
    pub output_hash: String,
    pub resolved_by: String,
    pub resolved_at: DateTime<Utc>,
    #[serde(default)]
    pub author_approved: bool,
}

impl MacroResolution {
    pub fn new(
        invocation_id: Uuid,
        replacement_markdown: impl Into<String>,
        resolved_by: impl Into<String>,
        author_approved: bool,
    ) -> Self {
        let replacement_markdown = replacement_markdown.into();
        Self {
            invocation_id,
            output_hash: hash_markdown(&replacement_markdown),
            replacement_markdown,
            resolved_by: resolved_by.into(),
            resolved_at: Utc::now(),
            author_approved,
        }
    }

    pub fn validate(&self) -> Result<(), MacroError> {
        if self.invocation_id.is_nil()
            || self.replacement_markdown.len() > 10 * 1024 * 1024
            || self.replacement_markdown.contains('\0')
            || self.resolved_by.trim().is_empty()
            || self.resolved_by.len() > 300
            || self.output_hash != hash_markdown(&self.replacement_markdown)
            || self.resolved_at > Utc::now() + Duration::minutes(5)
        {
            return Err(MacroError::InvalidResolution);
        }
        Ok(())
    }
}

pub fn parse_macro_blocks(source: &str) -> Result<Vec<MacroBlock>, MacroError> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, String)> = None;
    let mut offset = 0usize;
    let mut invocation_ids = BTreeSet::new();

    for line in source.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\r', '\n']).trim();
        match &mut open {
            None if marker == MACRO_FENCE => open = Some((offset, String::new())),
            Some((start, json)) if marker == "```" => {
                let invocation: MacroInvocation =
                    serde_json::from_str(json).map_err(|_| MacroError::InvalidJson)?;
                invocation.validate()?;
                if !invocation_ids.insert(invocation.invocation_id) {
                    return Err(MacroError::DuplicateInvocation);
                }
                blocks.push(MacroBlock {
                    byte_start: *start,
                    // Keep the closing fence's line ending outside the
                    // replacement range. This preserves the Markdown block
                    // boundary and prevents resolved output from running into
                    // the following paragraph.
                    byte_end: offset + line.trim_end_matches(['\r', '\n']).len(),
                    invocation,
                });
                open = None;
            }
            Some((_, json)) => {
                if json.len().saturating_add(line.len()) > MAX_MACRO_JSON_BYTES {
                    return Err(MacroError::MacroTooLarge);
                }
                json.push_str(line);
            }
            None => {}
        }
        offset += line.len();
    }
    if open.is_some() {
        return Err(MacroError::UnterminatedBlock);
    }
    Ok(blocks)
}

/// Applies validated resolutions and returns portable Markdown suitable for a
/// complete AI2AI revision proposal. Unknown or duplicate resolutions fail;
/// unresolved blocks remain inert and visible as code.
pub fn apply_macro_resolutions(
    source: &str,
    resolutions: Vec<MacroResolution>,
) -> Result<String, MacroError> {
    let blocks = parse_macro_blocks(source)?;
    let known_ids: BTreeSet<_> = blocks
        .iter()
        .map(|block| block.invocation.invocation_id)
        .collect();
    let mut by_id = BTreeMap::new();
    for resolution in resolutions {
        resolution.validate()?;
        if !known_ids.contains(&resolution.invocation_id) {
            return Err(MacroError::UnknownInvocation);
        }
        let id = resolution.invocation_id;
        if by_id.insert(id, resolution).is_some() {
            return Err(MacroError::DuplicateResolution);
        }
    }

    let mut output = source.to_owned();
    for block in blocks.iter().rev() {
        let Some(resolution) = by_id.get(&block.invocation.invocation_id) else {
            continue;
        };
        if block.invocation.requires_review && !resolution.author_approved {
            return Err(MacroError::ReviewRequired);
        }
        output.replace_range(
            block.byte_start..block.byte_end,
            &resolution.replacement_markdown,
        );
    }
    Ok(output)
}

fn hash_markdown(source: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(source.as_bytes()))
}

fn qualified_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.starts_with(|character: char| character.is_ascii_lowercase())
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        })
}

fn semantic_version(value: &str) -> bool {
    let core = value.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<_> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| part.parse::<u64>().is_ok())
}

const fn default_review() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MacroError {
    #[error("unsupported macro contract version")]
    UnsupportedVersion,
    #[error("macro invocation is invalid")]
    InvalidInvocation,
    #[error("macro block JSON is invalid")]
    InvalidJson,
    #[error("macro block exceeds its byte limit")]
    MacroTooLarge,
    #[error("macro block is not terminated")]
    UnterminatedBlock,
    #[error("macro invocation id is duplicated")]
    DuplicateInvocation,
    #[error("macro resolution is invalid or its output hash does not match")]
    InvalidResolution,
    #[error("macro resolution does not match an invocation")]
    UnknownInvocation,
    #[error("macro resolution is duplicated")]
    DuplicateResolution,
    #[error("macro output requires explicit author review")]
    ReviewRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AiActorKind, DataBoundary};

    fn invocation(id: Uuid) -> MacroInvocation {
        MacroInvocation {
            spec_version: MACRO_SPEC_VERSION.into(),
            invocation_id: id,
            macro_id: "org.example.expand".into(),
            definition_version: "1.0.0".into(),
            phase: MacroPhase::Draft,
            inputs: serde_json::json!({"topic": "AI2AI"}),
            actor: AiActor {
                kind: AiActorKind::Agent,
                id: "local-agent".into(),
                provider: None,
                model: None,
            },
            policy: AiPolicySnapshot {
                data_boundary: DataBoundary::LocalOnly,
                allowed_provider_ids: vec![],
                allowed_capabilities: vec!["content.propose".into()],
                max_cost: Some(0.0),
                max_tokens: Some(1000),
            },
            context_receipts: vec![],
            requires_review: true,
            created_at: Utc::now(),
        }
    }

    fn source(value: &MacroInvocation) -> String {
        format!(
            "Before\n\n{MACRO_FENCE}\n{}\n```\n\nAfter\n",
            serde_json::to_string(value).unwrap()
        )
    }

    #[test]
    fn fenced_invocations_are_inert_until_a_reviewed_resolution_is_applied() {
        let value = invocation(Uuid::now_v7());
        let source = source(&value);
        let blocks = parse_macro_blocks(&source).unwrap();
        assert_eq!(blocks.len(), 1);

        let unreviewed = MacroResolution::new(value.invocation_id, "Expanded", "agent", false);
        assert_eq!(
            apply_macro_resolutions(&source, vec![unreviewed]),
            Err(MacroError::ReviewRequired)
        );
        let reviewed = MacroResolution::new(value.invocation_id, "Expanded", "agent", true);
        assert_eq!(
            apply_macro_resolutions(&source, vec![reviewed]).unwrap(),
            "Before\n\nExpanded\n\nAfter\n"
        );
    }

    #[test]
    fn duplicate_and_unterminated_blocks_fail_closed() {
        let value = invocation(Uuid::now_v7());
        let duplicated = format!("{}{}", source(&value), source(&value));
        assert_eq!(
            parse_macro_blocks(&duplicated),
            Err(MacroError::DuplicateInvocation)
        );
        assert_eq!(
            parse_macro_blocks(&format!(
                "{MACRO_FENCE}\n{}",
                serde_json::to_string(&value).unwrap()
            )),
            Err(MacroError::UnterminatedBlock)
        );
    }
}
