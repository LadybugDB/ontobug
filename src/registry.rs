//! The schema registry: which named graphs hold schema rather than data, and which graphs each
//! one applies to — kept as RDF in the registry graph `<oxilite:schema>`.
//!
//! Each registered graph is a resource of the registry graph with the graph's IRI as subject
//! (`oxl:DefaultGraph` for the default graph), typed with its role and described with the
//! `oxl:` vocabulary ([`VOCABULARY`]). Every operation is a plain SPARQL 1.1 update built here,
//! so it runs unchanged on Oxigraph, oxilite or any SPARQL store.
//!
//! The wire format is the one oxilite writes (vocabulary 2.1): a dataset managed by ontolith
//! opens in oxilite with its registrations intact, and the other way round.
//!
//! The reader ([`entries_from_rows`]) and the SPARQL recipes of `docs/guide.md` agree on every
//! edge case: a graph typed with several roles counts for each, and only an `xsd:boolean` false
//! (`false` or `0`) deactivates. [`problems`] checks what the registry's own SHACL shapes (in
//! [`VOCABULARY`]) check, without a SHACL engine.

use crate::error::{Error, Result};
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{GraphName, GraphNameRef, Literal, NamedNode, NamedNodeRef, QuadRef, Term};
use spargebra::term::{GraphNamePattern, NamedNodePattern, TermPattern};
use spargebra::{GraphUpdateOperation, Update};
use std::collections::BTreeMap;

/// The system graph holding the registry. The IRI is kept from oxilite for compatibility.
pub const SCHEMA_GRAPH: &str = "oxilite:schema";

/// The system graph holding the `oxl:` vocabulary and the registry shapes.
pub const VOCABULARY_GRAPH: &str = "oxilite:vocabulary";

/// The version of the vocabulary that [`system_quads`] installs (`oxl:version` of
/// `<oxilite:vocabulary>` in the registry).
pub const VOCABULARY_VERSION: &str = "2.1";

/// The registry namespace (`oxl:`).
pub const NS: &str = "https://oxilite.dev/ns#";

/// The vocabulary as Turtle: the registry's classes and properties, and the SHACL shapes the
/// registry must conform to.
pub const VOCABULARY: &str = include_str!("../vocab/oxl.ttl");

/// IRIs of the registry vocabulary.
pub mod vocab {
    pub const SCHEMA_GRAPH: &str = "https://oxilite.dev/ns#SchemaGraph";
    pub const SYSTEM_GRAPH: &str = "https://oxilite.dev/ns#SystemGraph";
    pub const ONTOLOGY_GRAPH: &str = "https://oxilite.dev/ns#OntologyGraph";
    pub const SHAPES_GRAPH: &str = "https://oxilite.dev/ns#ShapesGraph";
    pub const SHEX_GRAPH: &str = "https://oxilite.dev/ns#ShExGraph";
    pub const APPLIES_TO: &str = "https://oxilite.dev/ns#appliesTo";
    pub const ACTIVE: &str = "https://oxilite.dev/ns#active";
    pub const ONTOLOGY_IRI: &str = "https://oxilite.dev/ns#ontologyIri";
    pub const VERSION: &str = "https://oxilite.dev/ns#version";
    pub const SHA256: &str = "https://oxilite.dev/ns#sha256";
    pub const LOADED_AT: &str = "https://oxilite.dev/ns#loadedAt";
    pub const DEFAULT_GRAPH: &str = "https://oxilite.dev/ns#DefaultGraph";
    pub const ALL_GRAPHS: &str = "https://oxilite.dev/ns#AllGraphs";
    /// Imports recorded in the registry (vocabulary 2.1; `owl:imports` there is still read).
    pub const IMPORTS: &str = "https://oxilite.dev/ns#imports";
    /// `owl:imports`: asserted in an ontology's own graph, and in registries of vocabulary 2.
    pub const OWL_IMPORTS: &str = "http://www.w3.org/2002/07/owl#imports";
    pub const OWL_ONTOLOGY: &str = "http://www.w3.org/2002/07/owl#Ontology";
    pub const OWL_VERSION_INFO: &str = "http://www.w3.org/2002/07/owl#versionInfo";
    pub const OWL_VERSION_IRI: &str = "http://www.w3.org/2002/07/owl#versionIRI";
}

/// The graphs the registry maintains itself. They are never registrations.
pub const SYSTEM_GRAPHS: [&str; 2] = [SCHEMA_GRAPH, VOCABULARY_GRAPH];

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

    /// The role's name: `ontology`, `shacl` or `shex`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ontology => "ontology",
            Self::Shacl => "shacl",
            Self::Shex => "shex",
        }
    }

    /// The class typing a registered graph of this role.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Ontology => vocab::ONTOLOGY_GRAPH,
            Self::Shacl => vocab::SHAPES_GRAPH,
            Self::Shex => vocab::SHEX_GRAPH,
        }
    }

    /// The role a class names.
    pub fn from_class(iri: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.class() == iri)
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

/// One registered graph, as the registry graph describes it.
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
    /// Import targets (`oxl:imports`). An import naming another active registered ontology (by graph
    /// name or `oxl:ontologyIri`) brings it into this ontology's scope.
    pub imports: Vec<NamedNode>,
    /// The graphs the schema applies to; empty means every graph (written as
    /// `oxl:appliesTo oxl:AllGraphs`).
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

// ---------------------------------------------------------------------------------- SPARQL

/// The IRI naming a graph in the registry (`oxl:DefaultGraph` for the default graph).
pub fn graph_node(g: GraphNameRef<'_>) -> Result<NamedNode> {
    match g {
        GraphNameRef::NamedNode(n) => Ok(n.into_owned()),
        GraphNameRef::DefaultGraph => Ok(NamedNode::new_unchecked(vocab::DEFAULT_GRAPH)),
        GraphNameRef::BlankNode(_) => Err(Error::Other(
            "a graph named by a blank node cannot be registered as schema".into(),
        )),
    }
}

/// The graph a registry IRI names (`oxl:DefaultGraph` is the default graph).
pub fn node_graph(n: &NamedNode) -> GraphName {
    if n.as_str() == vocab::DEFAULT_GRAPH {
        GraphName::DefaultGraph
    } else {
        n.clone().into()
    }
}

fn iri(s: &str) -> String {
    format!("<{s}>")
}

fn describe_delete(node: &NamedNode) -> String {
    format!(
        "DELETE WHERE {{ GRAPH {} {{ {node} ?p ?o }} }}",
        iri(SCHEMA_GRAPH)
    )
}

/// Is this literal an `xsd:boolean` false? (`"0"^^xsd:boolean` too; a plain `"false"` is not.)
fn is_false(l: &Literal) -> bool {
    l.datatype() == xsd::BOOLEAN && matches!(l.value(), "false" | "0")
}

/// The `oxl:appliesTo` objects of a registration: its targets, or `oxl:AllGraphs`.
fn targets_of(applies_to: &[GraphName]) -> Result<Vec<NamedNode>> {
    if applies_to.is_empty() {
        return Ok(vec![NamedNode::new_unchecked(vocab::ALL_GRAPHS)]);
    }
    applies_to.iter().map(|g| graph_node(g.as_ref())).collect()
}

/// The `xsd:dateTime` of now, where a clock is available.
fn now() -> Option<String> {
    Some(oxsdatatypes::DateTime::now().to_string())
}

/// SPARQL registering a graph: its old description, if any, is replaced. A named graph is
/// created (`CREATE SILENT GRAPH`) so the registry never names a graph the store lacks.
/// `loaded_at` is set to now when absent.
pub fn register_update(entry: &SchemaGraph) -> Result<String> {
    let node = graph_node(entry.graph.as_ref())?;
    let mut out = String::new();
    if let GraphName::NamedNode(n) = &entry.graph {
        out.push_str(&format!("CREATE SILENT GRAPH {n} ;\n"));
    }
    out.push_str(&describe_delete(&node));
    out.push_str(" ;\nINSERT DATA { GRAPH ");
    out.push_str(&iri(SCHEMA_GRAPH));
    out.push_str(" {\n");
    let mut triple = |p: &str, o: String| out.push_str(&format!("  {node} {} {o} .\n", iri(p)));
    triple(rdf::TYPE.as_str(), iri(entry.role.class()));
    triple(vocab::ACTIVE, Literal::from(entry.active).to_string());
    for t in targets_of(&entry.applies_to)? {
        triple(vocab::APPLIES_TO, t.to_string());
    }
    if let Some(i) = &entry.iri {
        triple(vocab::ONTOLOGY_IRI, i.to_string());
    }
    if let Some(v) = &entry.version {
        triple(vocab::VERSION, Literal::new_simple_literal(v).to_string());
    }
    if let Some(h) = &entry.sha256 {
        triple(vocab::SHA256, Literal::new_simple_literal(h).to_string());
    }
    for i in &entry.imports {
        triple(vocab::IMPORTS, i.to_string());
    }
    if let Some(t) = entry.loaded_at.clone().or_else(now) {
        triple(
            vocab::LOADED_AT,
            Literal::new_typed_literal(t, xsd::DATE_TIME).to_string(),
        );
    }
    out.push_str("} }");
    Ok(out)
}

/// SPARQL setting the graphs a registration applies to (empty: every graph), keeping the rest
/// of its description. Nothing happens to an unregistered graph.
pub fn remap_update(graph: GraphNameRef<'_>, applies_to: &[GraphName]) -> Result<String> {
    let node = graph_node(graph)?;
    let (g, a) = (iri(SCHEMA_GRAPH), iri(vocab::APPLIES_TO));
    let insert = targets_of(applies_to)?
        .iter()
        .map(|t| format!("{node} {a} {t} ."))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "DELETE {{ GRAPH {g} {{ {node} {a} ?t }} }} INSERT {{ GRAPH {g} {{ {insert} }} }} \
         WHERE {{ GRAPH {g} {{ {node} a ?role OPTIONAL {{ {node} {a} ?t }} }} }}"
    ))
}

/// SPARQL removing a registration (the graph's triples stay).
pub fn unregister_update(graph: GraphNameRef<'_>) -> Result<String> {
    Ok(describe_delete(&graph_node(graph)?))
}

/// SPARQL activating or deactivating a registration (nothing happens to an unregistered graph).
pub fn set_active_update(graph: GraphNameRef<'_>, active: bool) -> Result<String> {
    let node = graph_node(graph)?;
    let (g, a) = (iri(SCHEMA_GRAPH), iri(vocab::ACTIVE));
    Ok(format!(
        "DELETE {{ GRAPH {g} {{ {node} {a} ?a }} }} INSERT {{ GRAPH {g} {{ {node} {a} {} }} }} \
         WHERE {{ GRAPH {g} {{ {node} a ?role OPTIONAL {{ {node} {a} ?a }} }} }}",
        Literal::from(active)
    ))
}

/// SPARQL removing a registration and every triple of its graph.
pub fn drop_update(graph: GraphNameRef<'_>) -> Result<String> {
    let node = graph_node(graph)?;
    let drop = match graph {
        GraphNameRef::NamedNode(n) => format!("DROP SILENT GRAPH {n}"),
        _ => "CLEAR SILENT DEFAULT".to_owned(),
    };
    Ok(format!("{} ;\n{drop}", describe_delete(&node)))
}

/// SPARQL asking whether a graph is registered.
pub fn registered_query(graph: GraphNameRef<'_>) -> Result<String> {
    Ok(format!(
        "ASK {{ GRAPH {} {{ {} a ?role }} }}",
        iri(SCHEMA_GRAPH),
        graph_node(graph)?
    ))
}

/// SPARQL counting the triples of a graph (`?n`).
pub fn size_query(graph: GraphNameRef<'_>) -> String {
    match graph {
        GraphNameRef::NamedNode(n) => {
            format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH {n} {{ ?s ?p ?o }} }}")
        }
        _ => "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }".to_owned(),
    }
}

/// SPARQL reading the registry: `?g ?p ?o` rows, parsed by [`entries_from_rows`].
pub fn entries_query() -> String {
    format!(
        "SELECT ?g ?p ?o WHERE {{ GRAPH {} {{ ?g ?p ?o }} }}",
        iri(SCHEMA_GRAPH)
    )
}

/// The registrations described by `?g ?p ?o` rows, ordered by role and graph. Subjects
/// without a role class are ignored.
pub fn entries_from_rows(rows: &[Vec<Option<Term>>]) -> Vec<SchemaGraph> {
    let mut by: BTreeMap<String, (NamedNode, Vec<(String, Term)>)> = BTreeMap::new();
    for row in rows {
        let (Some(Term::NamedNode(s)), Some(Term::NamedNode(p)), Some(o)) = (
            row.first().cloned().flatten(),
            row.get(1).cloned().flatten(),
            row.get(2).cloned().flatten(),
        ) else {
            continue;
        };
        by.entry(s.as_str().to_owned())
            .or_insert_with(|| (s.clone(), Vec::new()))
            .1
            .push((p.into_string(), o));
    }
    let mut out = Vec::new();
    for (_, (node, props)) in by {
        // A graph may hold several roles (an ontology with its SHACL shapes): one entry each,
        // so scoping counts it once per role.
        let mut roles: Vec<SchemaRole> = props
            .iter()
            .filter_map(|(p, o)| match o {
                Term::NamedNode(c) if p == rdf::TYPE.as_str() => SchemaRole::from_class(c.as_str()),
                _ => None,
            })
            .collect();
        roles.sort();
        roles.dedup();
        let Some(&first) = roles.first() else {
            continue;
        };
        let mut e = SchemaGraph::new(node_graph(&node), first);
        let mut all = false;
        for (p, o) in props {
            match (p.as_str(), o) {
                (vocab::ACTIVE, Term::Literal(l)) if is_false(&l) => {
                    e.active = false;
                }
                (vocab::APPLIES_TO, Term::NamedNode(n)) if n.as_str() == vocab::ALL_GRAPHS => {
                    all = true;
                }
                (vocab::APPLIES_TO, Term::NamedNode(n)) => e.applies_to.push(node_graph(&n)),
                (vocab::ONTOLOGY_IRI, Term::NamedNode(n)) => e.iri = Some(n),
                (vocab::VERSION, Term::Literal(l)) => e.version = Some(l.value().to_owned()),
                (vocab::SHA256, Term::Literal(l)) => e.sha256 = Some(l.value().to_owned()),
                (vocab::LOADED_AT, Term::Literal(l)) => e.loaded_at = Some(l.value().to_owned()),
                (vocab::IMPORTS | vocab::OWL_IMPORTS, Term::NamedNode(n)) => e.imports.push(n),
                _ => {}
            }
        }
        if all {
            e.applies_to.clear();
        }
        e.applies_to.sort_by_key(ToString::to_string);
        e.imports.sort();
        e.imports.dedup();
        for role in roles {
            out.push(SchemaGraph { role, ..e.clone() });
        }
    }
    out.sort_by_key(|e| (e.role, e.graph.to_string()));
    out
}

// ------------------------------------------------------------------------ change detection

fn registry_graph(g: GraphNameRef<'_>) -> bool {
    matches!(g, GraphNameRef::NamedNode(n) if n.as_str() == SCHEMA_GRAPH)
}

/// Does writing this quad change what the registry says? (Any quad of `<oxilite:schema>`.)
pub fn is_registry_quad(q: QuadRef<'_>) -> bool {
    registry_graph(q.graph_name)
}

/// Can this update change the registry? Any triple written to `<oxilite:schema>` counts. With
/// a variable graph (conservative), a triple counts when it could be a registry triple: a
/// variable predicate, an `oxl:` predicate, `owl:imports`, or `rdf:type` with a variable or
/// `oxl:` class — so a bulk `INSERT { GRAPH ?g { ?s a ex:Person } }` rebuilds nothing.
pub fn update_touches_registry(update: &Update) -> bool {
    let registry_triple = |p: &NamedNodePattern, o: &TermPattern| match p {
        NamedNodePattern::Variable(_) => true,
        NamedNodePattern::NamedNode(n) if n.as_ref() == rdf::TYPE => match o {
            TermPattern::NamedNode(c) => c.as_str().starts_with(NS),
            TermPattern::Variable(_) => true,
            _ => false,
        },
        NamedNodePattern::NamedNode(n) => {
            n.as_str().starts_with(NS) || n.as_str() == vocab::OWL_IMPORTS
        }
    };
    let counts = |g: &GraphNamePattern, p: &NamedNodePattern, o: &TermPattern| match g {
        GraphNamePattern::NamedNode(n) => n.as_str() == SCHEMA_GRAPH,
        GraphNamePattern::DefaultGraph => false,
        GraphNamePattern::Variable(_) => registry_triple(p, o),
    };
    update.operations.iter().any(|op| match op {
        GraphUpdateOperation::InsertData { data } => data.iter().any(|q| {
            matches!(&q.graph_name, spargebra::term::GraphName::NamedNode(n) if n.as_str() == SCHEMA_GRAPH)
        }),
        GraphUpdateOperation::DeleteData { data } => data.iter().any(|q| {
            matches!(&q.graph_name, spargebra::term::GraphName::NamedNode(n) if n.as_str() == SCHEMA_GRAPH)
        }),
        GraphUpdateOperation::DeleteInsert { delete, insert, .. } => {
            delete.iter().any(|q| {
                let o: TermPattern = q.object.clone().into();
                counts(&q.graph_name, &q.predicate, &o)
            }) || insert
                .iter()
                .any(|q| counts(&q.graph_name, &q.predicate, &q.object))
        }
        GraphUpdateOperation::Create { .. } => false,
        GraphUpdateOperation::Load { .. }
        | GraphUpdateOperation::Clear { .. }
        | GraphUpdateOperation::Drop { .. } => true,
    })
}

// ---------------------------------------------------------------------------- system graphs

/// The system graphs a new store starts with: the vocabulary in `<oxilite:vocabulary>`, and in
/// `<oxilite:schema>` a description of both system graphs (`oxl:SystemGraph`, with the
/// vocabulary's version). System graphs are never registrations.
pub fn system_quads() -> Vec<oxrdf::Quad> {
    let vocabulary = NamedNode::new_unchecked(VOCABULARY_GRAPH);
    let registry = NamedNode::new_unchecked(SCHEMA_GRAPH);
    let mut out: Vec<oxrdf::Quad> = oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .for_slice(VOCABULARY.as_bytes())
        .map(|q| {
            let q = q.expect("the bundled vocabulary is valid Turtle");
            oxrdf::Quad::new(q.subject, q.predicate, q.object, vocabulary.clone())
        })
        .collect();
    let t = |s: &NamedNode, p: &str, o: Term| {
        oxrdf::Quad::new(s.clone(), NamedNode::new_unchecked(p), o, registry.clone())
    };
    let system: Term = NamedNode::new_unchecked(vocab::SYSTEM_GRAPH).into();
    let label = "http://www.w3.org/2000/01/rdf-schema#label";
    out.extend([
        t(&registry, rdf::TYPE.as_str(), system.clone()),
        t(
            &registry,
            label,
            Literal::new_simple_literal("schema registry").into(),
        ),
        t(&vocabulary, rdf::TYPE.as_str(), system),
        t(
            &vocabulary,
            label,
            Literal::new_simple_literal("oxilite vocabulary").into(),
        ),
        t(&vocabulary, vocab::APPLIES_TO, registry.clone().into()),
        t(
            &vocabulary,
            vocab::ONTOLOGY_IRI,
            NamedNode::new_unchecked(NS).into(),
        ),
        t(
            &vocabulary,
            vocab::VERSION,
            Literal::new_simple_literal(VOCABULARY_VERSION).into(),
        ),
    ]);
    out
}

/// SPARQL installing (or refreshing) the system graphs on any store: the vocabulary graph is
/// replaced and the system graphs' descriptions rewritten; registrations are untouched.
pub fn system_graphs_update() -> String {
    let (reg, voc) = (iri(SCHEMA_GRAPH), iri(VOCABULARY_GRAPH));
    let mut out = format!(
        "CREATE SILENT GRAPH {reg} ;\nCREATE SILENT GRAPH {voc} ;\nCLEAR SILENT GRAPH {voc} ;\n\
         DELETE WHERE {{ GRAPH {reg} {{ {reg} ?p ?o }} }} ;\n\
         DELETE WHERE {{ GRAPH {reg} {{ {voc} ?p ?o }} }} ;\nINSERT DATA {{\n"
    );
    for q in system_quads() {
        out.push_str(&format!(
            "  GRAPH {} {{ {} {} {} . }}\n",
            q.graph_name, q.subject, q.predicate, q.object
        ));
    }
    out.push('}');
    out
}

/// SPARQL `ASK`: are the system graphs installed at the current vocabulary version?
pub fn system_graphs_ready_query() -> String {
    format!(
        "ASK {{ GRAPH {} {{ {} {} {} }} }}",
        iri(SCHEMA_GRAPH),
        iri(VOCABULARY_GRAPH),
        iri(vocab::VERSION),
        Literal::new_simple_literal(VOCABULARY_VERSION)
    )
}

// ------------------------------------------------------------------------------ validation

/// What is wrong with the registry described by `?g ?p ?o` rows (see [`entries_query`]): the
/// violations of the registry's SHACL shapes (`oxl:RegistrationShape` in [`VOCABULARY`]),
/// found without a SHACL engine, one message per violation. Empty means valid.
///
/// A node is checked when it has a role class, `oxl:SystemGraph`, or a registration property.
pub fn problems(rows: &[Vec<Option<Term>>]) -> Vec<String> {
    const PROPS: [&str; 8] = [
        vocab::APPLIES_TO,
        vocab::ACTIVE,
        vocab::ONTOLOGY_IRI,
        vocab::VERSION,
        vocab::SHA256,
        vocab::LOADED_AT,
        vocab::IMPORTS,
        vocab::OWL_IMPORTS,
    ];
    let mut by: BTreeMap<String, Vec<(String, Term)>> = BTreeMap::new();
    for row in rows {
        let (Some(s), Some(Term::NamedNode(p)), Some(o)) = (
            row.first().cloned().flatten(),
            row.get(1).cloned().flatten(),
            row.get(2).cloned().flatten(),
        ) else {
            continue;
        };
        by.entry(s.to_string())
            .or_default()
            .push((p.into_string(), o));
    }
    let mut out = Vec::new();
    for (node, props) in by {
        let values = |p: &'static str| props.iter().filter(move |(q, _)| q == p).map(|(_, o)| o);
        let typed = values(rdf::TYPE.as_str()).any(|o| {
            matches!(o, Term::NamedNode(c)
                if SchemaRole::from_class(c.as_str()).is_some() || c.as_str() == vocab::SYSTEM_GRAPH)
        });
        let described = props.iter().any(|(p, _)| PROPS.contains(&p.as_str()));
        if !typed && !described {
            continue;
        }
        let mut bad = |m: String| out.push(format!("{node}: {m}"));
        if node.starts_with("_:") {
            bad("a blank node cannot name a graph".into());
        }
        if !typed {
            bad("has registration properties but no role class (oxl:OntologyGraph, oxl:ShapesGraph, oxl:ShExGraph)".into());
        }
        for p in [
            vocab::ACTIVE,
            vocab::ONTOLOGY_IRI,
            vocab::VERSION,
            vocab::SHA256,
            vocab::LOADED_AT,
        ] {
            let n = values(p).count();
            if n > 1 {
                bad(format!("<{p}> has {n} values, at most one is allowed"));
            }
        }
        for p in [
            vocab::APPLIES_TO,
            vocab::ONTOLOGY_IRI,
            vocab::IMPORTS,
            vocab::OWL_IMPORTS,
        ] {
            for o in values(p).filter(|o| !matches!(o, Term::NamedNode(_))) {
                bad(format!("<{p}> {o} is not an IRI"));
            }
        }
        let literal = |p: &'static str, dt: NamedNodeRef<'static>| {
            values(p)
                .filter(move |o| !matches!(o, Term::Literal(l) if l.datatype() == dt))
                .map(move |o| format!("<{p}> {o} is not an <{}>", dt.as_str()))
                .collect::<Vec<_>>()
        };
        for m in literal(vocab::ACTIVE, xsd::BOOLEAN)
            .into_iter()
            .chain(literal(vocab::VERSION, xsd::STRING))
            .chain(literal(vocab::SHA256, xsd::STRING))
            .chain(literal(vocab::LOADED_AT, xsd::DATE_TIME))
        {
            bad(m);
        }
        for o in values(vocab::ACTIVE) {
            if let Term::Literal(l) = o {
                if l.datatype() == xsd::BOOLEAN
                    && !matches!(l.value(), "true" | "false" | "1" | "0")
                {
                    bad(format!(
                        "<{}> {o} is not a valid xsd:boolean",
                        vocab::ACTIVE
                    ));
                }
            }
        }
        for o in values(vocab::SHA256) {
            if let Term::Literal(l) = o {
                let v = l.value();
                if v.len() != 64 || !v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                    bad(format!(
                        "<{}> {o} is not 64 lowercase hex digits",
                        vocab::SHA256
                    ));
                }
            }
        }
    }
    out
}

/// The registry triples of a registration, as quads of `<oxilite:schema>` (for writers that
/// cannot run SPARQL, like the migration).
pub fn entry_quads(entry: &SchemaGraph) -> Result<Vec<oxrdf::Quad>> {
    let node = graph_node(entry.graph.as_ref())?;
    let g = NamedNode::new_unchecked(SCHEMA_GRAPH);
    let q = |p: &str, o: Term| {
        oxrdf::Quad::new(node.clone(), NamedNode::new_unchecked(p), o, g.clone())
    };
    let mut out = vec![
        q(
            rdf::TYPE.as_str(),
            NamedNode::new_unchecked(entry.role.class()).into(),
        ),
        q(vocab::ACTIVE, Literal::from(entry.active).into()),
    ];
    for t in targets_of(&entry.applies_to)? {
        out.push(q(vocab::APPLIES_TO, t.into()));
    }
    if let Some(i) = &entry.iri {
        out.push(q(vocab::ONTOLOGY_IRI, i.clone().into()));
    }
    if let Some(v) = &entry.version {
        out.push(q(vocab::VERSION, Literal::new_simple_literal(v).into()));
    }
    if let Some(h) = &entry.sha256 {
        out.push(q(vocab::SHA256, Literal::new_simple_literal(h).into()));
    }
    for i in &entry.imports {
        out.push(q(vocab::IMPORTS, i.clone().into()));
    }
    if let Some(t) = &entry.loaded_at {
        out.push(q(
            vocab::LOADED_AT,
            Literal::new_typed_literal(t, xsd::DATE_TIME).into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_update_parses_and_reads_back() {
        let mut e = SchemaGraph::new(
            NamedNode::new_unchecked("http://ex.org/onto").into(),
            SchemaRole::Ontology,
        );
        e.version = Some("v \"1\"".into());
        e.applies_to = vec![
            NamedNode::new_unchecked("http://ex.org/data").into(),
            GraphName::DefaultGraph,
        ];
        e.imports = vec![NamedNode::new_unchecked("http://ex.org/base")];
        e.loaded_at = Some("2026-09-26T10:00:00Z".into());
        let u = register_update(&e).unwrap();
        spargebra::SparqlParser::new().parse_update(&u).unwrap();
        for s in [
            unregister_update(e.graph.as_ref()).unwrap(),
            set_active_update(e.graph.as_ref(), false).unwrap(),
            drop_update(e.graph.as_ref()).unwrap(),
            drop_update(GraphNameRef::DefaultGraph).unwrap(),
        ] {
            spargebra::SparqlParser::new().parse_update(&s).unwrap();
        }
        let rows: Vec<Vec<Option<Term>>> = entry_quads(&e)
            .unwrap()
            .into_iter()
            .map(|q| {
                vec![
                    Some(q.subject.into()),
                    Some(q.predicate.into()),
                    Some(q.object),
                ]
            })
            .collect();
        let back = entries_from_rows(&rows);
        let mut want = e.clone();
        want.applies_to.sort_by_key(ToString::to_string);
        assert_eq!(back, vec![want]);
        assert_eq!(problems(&rows), Vec::<String>::new());
        spargebra::SparqlParser::new()
            .parse_update(&remap_update(e.graph.as_ref(), &[]).unwrap())
            .unwrap();
    }

    fn row(s: &str, p: &str, o: Term) -> Vec<Option<Term>> {
        vec![
            Some(NamedNode::new_unchecked(s).into()),
            Some(NamedNode::new_unchecked(p).into()),
            Some(o),
        ]
    }

    fn class(c: &str) -> Term {
        NamedNode::new_unchecked(c).into()
    }

    #[test]
    fn readers_agree_on_booleans_and_roles() {
        let g = "http://ex.org/g";
        let ty = rdf::TYPE.as_str();
        // A graph with two roles is listed once per role.
        let rows = vec![
            row(g, ty, class(vocab::ONTOLOGY_GRAPH)),
            row(g, ty, class(vocab::SHAPES_GRAPH)),
        ];
        let roles: Vec<_> = entries_from_rows(&rows).iter().map(|e| e.role).collect();
        assert_eq!(roles, vec![SchemaRole::Ontology, SchemaRole::Shacl]);
        // Only an xsd:boolean false deactivates, in either lexical form.
        for (o, active) in [
            (Literal::from(false), false),
            (Literal::new_typed_literal("0", xsd::BOOLEAN), false),
            (Literal::new_simple_literal("false"), true),
            (Literal::from(true), true),
        ] {
            let rows = vec![
                row(g, ty, class(vocab::ONTOLOGY_GRAPH)),
                row(g, vocab::ACTIVE, o.clone().into()),
            ];
            assert_eq!(entries_from_rows(&rows)[0].active, active, "{o}");
        }
    }

    #[test]
    fn registry_writes_are_detected_narrowly() {
        let touches = |u: &str| {
            let u =
                format!("PREFIX oxl: <https://oxilite.dev/ns#> PREFIX ex: <http://ex.org/> {u}");
            update_touches_registry(&spargebra::SparqlParser::new().parse_update(&u).unwrap())
        };
        // Bulk typing through a variable graph is not a registry write.
        assert!(!touches(
            "INSERT { GRAPH ?g { ?s a ex:Person } } WHERE { GRAPH ?g { ?s ex:p ?o } }"
        ));
        assert!(!touches(
            "DELETE { GRAPH ?g { ?s ex:p ?o } } WHERE { GRAPH ?g { ?s ex:p ?o } }"
        ));
        // Anything that could be one is.
        assert!(touches(
            "INSERT { GRAPH ?g { ?s a oxl:OntologyGraph } } WHERE { GRAPH ?g { ?s ex:p ?o } }"
        ));
        assert!(touches(
            "INSERT { GRAPH ?g { ?s a ?c } } WHERE { GRAPH ?g { ?s ex:p ?c } }"
        ));
        assert!(touches("DELETE { GRAPH ?g { ?s <http://www.w3.org/2002/07/owl#imports> ?o } } WHERE { GRAPH ?g { ?s ?p ?o } }"));
        assert!(touches("DELETE { GRAPH <oxilite:schema> { ?s ex:p ?o } } WHERE { GRAPH <oxilite:schema> { ?s ex:p ?o } }"));
        assert!(touches(
            "INSERT DATA { GRAPH <oxilite:schema> { ex:a ex:b ex:c } }"
        ));
    }

    #[test]
    fn vocabulary_is_consistent_with_the_system_graphs() {
        use std::collections::BTreeSet;
        let rdfs = |l: &str| format!("http://www.w3.org/2000/01/rdf-schema#{l}");
        let disjoint = "http://www.w3.org/2002/07/owl#disjointWith";
        let vocabulary: Vec<oxrdf::Triple> =
            oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
                .for_slice(VOCABULARY.as_bytes())
                .map(|q| oxrdf::Triple::from(q.unwrap()))
                .collect();
        let objects = |s: &str, p: &str| -> Vec<String> {
            vocabulary
                .iter()
                .filter(|t| t.subject.to_string() == format!("<{s}>") && t.predicate.as_str() == p)
                .filter_map(|t| match &t.object {
                    Term::NamedNode(n) => Some(n.as_str().to_owned()),
                    _ => None,
                })
                .collect()
        };
        // RDFS entailment of the types of each subject of <oxilite:schema>: its rdf:type and
        // the domains of its properties, closed over rdfs:subClassOf.
        let mut types: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for q in system_quads() {
            if !matches!(&q.graph_name, GraphName::NamedNode(g) if g.as_str() == SCHEMA_GRAPH) {
                continue;
            }
            let entry = types.entry(q.subject.to_string()).or_default();
            match (&q.predicate, &q.object) {
                (p, Term::NamedNode(c)) if *p == rdf::TYPE => {
                    entry.insert(c.as_str().to_owned());
                }
                (p, _) => entry.extend(objects(p.as_str(), &rdfs("domain"))),
            }
        }
        for (node, classes) in &mut types {
            let mut todo: Vec<String> = classes.iter().cloned().collect();
            while let Some(c) = todo.pop() {
                for sup in objects(&c, &rdfs("subClassOf")) {
                    if classes.insert(sup.clone()) {
                        todo.push(sup);
                    }
                }
            }
            for c in classes.iter() {
                for d in objects(c, disjoint) {
                    assert!(
                        !classes.contains(&d),
                        "{node} is both <{c}> and the disjoint <{d}>"
                    );
                }
            }
            assert!(classes.contains(vocab::SYSTEM_GRAPH), "{node}: {classes:?}");
        }
        assert_eq!(types.len(), SYSTEM_GRAPHS.len());
        // Every registration property has a declared domain.
        for p in [
            vocab::APPLIES_TO,
            vocab::ACTIVE,
            vocab::ONTOLOGY_IRI,
            vocab::VERSION,
            vocab::SHA256,
            vocab::LOADED_AT,
            vocab::IMPORTS,
        ] {
            assert_eq!(
                objects(p, &rdfs("domain")),
                [format!("{NS}RegisteredGraph")],
                "{p}"
            );
        }
    }

    #[test]
    fn problems_mirror_the_registry_shapes() {
        let g = "http://ex.org/g";
        let ty = rdf::TYPE.as_str();
        let rows = vec![
            row(g, ty, class(vocab::ONTOLOGY_GRAPH)),
            row(
                g,
                vocab::ACTIVE,
                Literal::new_simple_literal("false").into(),
            ),
            row(g, vocab::ACTIVE, Literal::from(true).into()),
            row(
                g,
                vocab::APPLIES_TO,
                Literal::new_simple_literal("x").into(),
            ),
            row(g, vocab::SHA256, Literal::new_simple_literal("ABC").into()),
            row(
                "http://ex.org/h",
                vocab::APPLIES_TO,
                class(vocab::ALL_GRAPHS),
            ),
        ];
        let p = problems(&rows);
        assert_eq!(p.len(), 5, "{p:#?}");
        assert!(p
            .iter()
            .any(|m| m.starts_with("<http://ex.org/h>") && m.contains("no role class")));
    }
}
