use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    ConceptTag,
    Session,
    Turn,
    Fact,
}

impl std::fmt::Display for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeKind::File => write!(f, "file"),
            NodeKind::ConceptTag => write!(f, "concept_tag"),
            NodeKind::Session => write!(f, "session"),
            NodeKind::Turn => write!(f, "turn"),
            NodeKind::Fact => write!(f, "fact"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Modifies,
    Reads,
    Mentions,
    OccurredIn,
    RelatedTo,
}

impl std::fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EdgeKind::Modifies => write!(f, "modifies"),
            EdgeKind::Reads => write!(f, "reads"),
            EdgeKind::Mentions => write!(f, "mentions"),
            EdgeKind::OccurredIn => write!(f, "occurred_in"),
            EdgeKind::RelatedTo => write!(f, "related_to"),
        }
    }
}
