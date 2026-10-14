//! The schema registry: which named graphs hold schema rather than data, and which
//! graphs each one applies to.
//!
//! Registrations live in Ladybug (see [`crate::ladybug`]), one `SchemaEntry` node per
//! registered graph and role — not in an RDF graph, and not in any SPARQL store. This
//! module holds the data types, the structural checks of [`problems`], and the IRIs of the
//! RDF vocabularies read from graph content (`owl:`, `rdfs:`). There is no oxilite
//! compatibility layer: the `oxl:` vocabulary and the `<oxilite:schema>` graph are gone.

use crate::error::{Error, Result};
use oxrdf::{GraphName, GraphNameRef, NamedNode};

/// IRIs read from an ontology graph's own triples.
pub mod owl {
    pub const ONTOLOGY: &str = "http://www.w3.org/2002/07/owl#Ontology";
    pub const IMPORTS: &str = "http://www.w3.org/2002/07/owl#imports";
    pub const VERSION_INFO: &str = "http://www.w3.org/2002/07/owl#versionInfo";
    pub const VERSION_IRI: &str = "http://www.w3.org/2002/07/owl#versionIRI";
    pub const EQUIVALENT_CLASS: &str = "http://www.w3.org/2002/07/owl#equivalentClass";
}

/// IRIs read from an ontology graph's own triples.
pub mod rdfs {
    pub const SUB_CLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
}

/// `rdf:type`, for content inspection without SPARQL.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// What a registered graph holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SchemaRole {
    /// An OWL / RDFS ontology.
    Ontology,
    /// A SHACL shapes graph.
    Shacl,
    /// A ShEx schema.
    Shex,
}

impl SchemaRole {
    pub const ALL: [Self; 3] = [Self::Ontology, Self::Shacl, Self::Shex];

    /// The role's name: `ontology`, `shacl` or `shex`. This is what Ladybug stores.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ontology => "ontology",
            Self::Shacl => "shacl",
            Self::Shex => "shex",
        }
    }
}

impl std::fmt::Display for SchemaRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for SchemaRole {
    type Err = Error;

    /// Parses a role name (`shapes` is accepted for `shacl`, `owl` for `ontology`), ignoring case.
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "ontology" | "owl" => Self::Ontology,
            "shacl" | "shapes" => Self::Shacl,
            "shex" => Self::Shex,
            _ => {
                return Err(Error::Other(format!(
                    "unknown schema role {s}: one of ontology, shacl, shex"
                )))
            }
        })
    }
}

/// One registered graph, as the Ladybug registry describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaGraph {
    pub graph: GraphName,
    pub role: SchemaRole,
    /// The `owl:Ontology` IRI, when it differs from the graph name.
    pub iri: Option<NamedNode>,
    /// A version to pin (`owl:versionIRI`, a tag…).
    pub version: Option<String>,
    /// Digest of the document the graph was loaded from, for drift detection.
    pub sha256: Option<String>,
    /// Import targets. An import naming another active registered ontology (by graph name or
    /// ontology IRI) brings it into this ontology's scope.
    pub imports: Vec<NamedNode>,
    /// The graphs the schema applies to; empty means every graph.
    pub applies_to: Vec<GraphName>,
    /// Inactive graphs stay registered but stop contributing.
    pub active: bool,
    /// When the graph was registered (`xsd:dateTime` lexical form).
    pub loaded_at: Option<String>,
}

impl SchemaGraph {
    /// An active registration applying to every graph, with nothing else recorded.
    pub fn new(graph: GraphName, role: SchemaRole) -> Self {
        Self {
            graph,
            role,
            iri: None,
            version: None,
            sha256: None,
            imports: Vec::new(),
            applies_to: Vec::new(),
            active: true,
            loaded_at: None,
        }
    }

    /// Does this schema apply to `target`?
    pub fn applies(&self, target: GraphNameRef<'_>) -> bool {
        self.applies_to.is_empty() || self.applies_to.iter().any(|g| g.as_ref() == target)
    }

    /// Is `iri` this registration's graph name or its ontology IRI?
    pub fn is_named(&self, iri: &str) -> bool {
        matches!(&self.graph, GraphName::NamedNode(n) if n.as_str() == iri)
            || self.iri.as_ref().is_some_and(|i| i.as_str() == iri)
    }
}

/// What is wrong with these registrations, one message per violation. Empty means valid.
///
/// Entries are already typed (graph names are IRIs or the default graph, roles are parsed),
/// so this checks what typing cannot: digests are 64 lowercase hex digits when recorded,
/// and no graph is registered twice in one role.
pub fn problems(entries: &[SchemaGraph]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for e in entries {
        let node = e.graph.to_string();
        if !seen.insert((e.graph.to_string(), e.role)) {
            out.push(format!("{node}: registered twice as {}", e.role.name()));
        }
        if matches!(e.graph, GraphName::BlankNode(_)) {
            out.push(format!("{node}: a blank node cannot name a graph"));
        }
        if let Some(h) = &e.sha256 {
            if h.len() != 64 || !h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                out.push(format!("{node}: sha256 {h} is not 64 lowercase hex digits"));
            }
        }
        if e.version.as_ref().is_some_and(|v| v.is_empty()) {
            out.push(format!("{node}: version is empty"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse_leniently() {
        use std::str::FromStr;
        assert_eq!(SchemaRole::from_str("OWL").unwrap(), SchemaRole::Ontology);
        assert_eq!(SchemaRole::from_str("shapes").unwrap(), SchemaRole::Shacl);
        assert!(SchemaRole::from_str("sparql").is_err());
    }

    #[test]
    fn problems_catch_bad_digests_and_dupes() {
        let mut e = SchemaGraph::new(
            NamedNode::new("http://ex.org/onto").unwrap().into(),
            SchemaRole::Ontology,
        );
        e.sha256 = Some("ABC".into());
        let mut entries = vec![e.clone(), e.clone()];
        entries[1].sha256 = Some("a".repeat(64));
        let p = problems(&entries);
        assert_eq!(p.len(), 2, "{p:#?}");
        assert!(p.iter().any(|m| m.contains("not 64 lowercase hex")));
        assert!(p.iter().any(|m| m.contains("registered twice")));
        assert!(problems(&entries[1..]).is_empty());
    }
}
