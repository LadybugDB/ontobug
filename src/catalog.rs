//! [`Catalog`]: an RDF dataset with a schema registry — load ontologies, shapes and data into
//! named graphs, register and map the schemas, resolve imports, and validate data graphs
//! against the shapes that apply to them.
//!
//! Ladybug is the database. Every triple of every graph lives there as a lossless
//! [`TripleRow`]: N-Triples tokens that concatenate back to byte-exact N-Triples, plus
//! the unescaped literal value for inspection. The registry (which graph holds which
//! kind of schema) and the projected schemas (classes, properties, shapes as nodes and
//! edges) live alongside the triples and speak Cypher ([`Catalog::cypher`]). TriG and
//! N-Quads files are snapshots: [`Catalog::save`] writes one, [`Catalog::open`] seeds an
//! empty registry from one. Oxigraph remains only as the RDF parser and model.

use crate::error::{Error, Result};
use crate::ladybug::{graph_token, LadybugSchemaStore, NamespaceInfo, Projected, TripleRow};
use crate::registry::{self, owl, rdfs, SchemaGraph, SchemaRole, RDF_TYPE};
use crate::validate::{self, Mode, Report, ShexResult};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{GraphName, GraphNameRef, NamedNode};
use oxrdf::Triple;
use rudof_rdf::rdf_core::RDFFormat;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Prefixes used when saving a dataset as TriG.
const PREFIXES: [(&str, &str); 7] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("sh", "http://www.w3.org/ns/shacl#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("prov", "http://www.w3.org/ns/prov#"),
];

/// An IRI as an N-Triples token, for comparing stored rows without parsing.
fn tok(iri: &str) -> String {
    format!("<{iri}>")
}

/// What [`Catalog::add`] records besides the graph's content.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// The graphs the schema applies to; empty: every graph.
    pub applies_to: Vec<GraphName>,
    /// A version to pin; by default the ontology's `owl:versionIRI` or `owl:versionInfo`.
    pub version: Option<String>,
    /// Imports to record in the registry, besides the `owl:imports` the
    /// ontology asserts itself.
    pub imports: Vec<NamedNode>,
    /// Register the graph inactive.
    pub inactive: bool,
}

/// How [`Catalog::validate`] picks shapes and data.
#[derive(Debug, Clone)]
pub struct ValidateOptions {
    /// Shapes graphs to use instead of those the registry maps to the data graph.
    pub shapes: Vec<GraphName>,
    /// Add the ontologies in scope (imports included) to the data graph, so `sh:targetClass`
    /// and `sh:class` follow their `rdfs:subClassOf` axioms, as SHACL specifies.
    pub with_ontologies: bool,
    pub mode: Mode,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        Self {
            shapes: Vec::new(),
            with_ontologies: true,
            mode: Mode::Native,
        }
    }
}

/// A validation report together with the graphs it was computed from.
#[derive(Debug, Clone)]
pub struct Validation {
    pub data_graph: GraphName,
    pub shapes_graphs: Vec<GraphName>,
    pub ontology_graphs: Vec<GraphName>,
    pub report: Report,
}

/// The result of comparing a registered graph's recorded digest with a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// The document has the recorded digest.
    Same,
    /// The digest differs: the document changed since it was registered.
    Changed { recorded: String, actual: String },
    /// Nothing was recorded at registration.
    Unrecorded { actual: String },
}

/// An RDF dataset in Ladybug: triples, registry and projected schemas.
#[derive(Clone)]
pub struct Catalog {
    registry: LadybugSchemaStore,
}

/// Lowercase hex SHA-256 of a document.
pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The RDF format of a file, by extension.
pub fn format_of(path: &Path) -> Result<RdfFormat> {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(|e| match e.to_ascii_lowercase().as_str() {
            "ttl" => Some(RdfFormat::Turtle),
            "nt" => Some(RdfFormat::NTriples),
            "nq" => Some(RdfFormat::NQuads),
            "trig" => Some(RdfFormat::TriG),
            "rdf" | "owl" | "xml" => Some(RdfFormat::RdfXml),
            "n3" => Some(RdfFormat::N3),
            e => RdfFormat::from_extension(e),
        })
        .ok_or_else(|| Error::Other(format!("cannot tell the RDF format of {}", path.display())))
}

/// The registry path for a dataset file: the same stem with an `.lbdb` extension
/// (`dataset.trig` goes with `dataset.lbdb`).
pub fn default_registry_path(dataset: &Path) -> PathBuf {
    dataset.with_extension("lbdb")
}

/// A parsed quad as an ingestible triple: its graph token plus the triple itself.
/// Node/edge decomposition (and namespace derivation) happens in the store.
fn ingest_item(quad: &oxrdf::Quad) -> (String, Triple) {
    (
        graph_token(quad.graph_name.as_ref()),
        Triple {
            subject: quad.subject.clone(),
            predicate: quad.predicate.clone(),
            object: quad.object.clone(),
        },
    )
}

/// Parses a document exactly as the old loader did: single-graph formats land in `graph`
/// (with its base IRI when named), dataset formats keep their own graphs.
fn parse_document(
    graph: GraphNameRef<'_>,
    data: &[u8],
    format: RdfFormat,
) -> Result<Vec<oxrdf::Quad>> {
    let parser = RdfParser::from_format(format).with_default_graph(graph.into_owned());
    let parser = match graph {
        GraphNameRef::NamedNode(n) => parser.with_base_iri(n.as_str()).unwrap_or_else(|_| {
            RdfParser::from_format(format).with_default_graph(graph.into_owned())
        }),
        _ => parser,
    };
    parser
        .for_slice(data)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Other(format!("RDF syntax: {e}")))
}

/// Parses N-Triples text into triples (for feeding the projectors).
fn parse_ntriples(s: &str) -> Result<Vec<Triple>> {
    use oxrdfio::RdfParser as Parser;
    Parser::from_format(oxrdfio::RdfFormat::NTriples)
        .for_slice(s.as_bytes())
        .map(|q| {
            q.map(|q| Triple {
                subject: q.subject,
                predicate: q.predicate,
                object: q.object,
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Other(format!("data graph: {e}")))
}

impl Catalog {
    /// An empty dataset with an in-memory registry.
    pub fn new() -> Result<Self> {
        Ok(Self {
            registry: LadybugSchemaStore::open_in_memory()?,
        })
    }

    /// Opens a dataset: the Ladybug database at `registry` holds the triples, the
    /// registry and the projections. When its triple store is empty and the TriG or
    /// N-Quads file at `data` exists, the file seeds it; otherwise the file is ignored
    /// (the `.lbdb` is live, the file is a snapshot — see [`Catalog::save`]).
    pub fn open(data: impl AsRef<Path>, registry: impl AsRef<Path>) -> Result<Self> {
        let c = Self {
            registry: LadybugSchemaStore::open(registry)?,
        };
        let data = data.as_ref();
        if data.exists() && c.registry.count_triples(None)? == 0 {
            let format = format_of(data)?;
            let bytes = std::fs::read(data)?;
            let items: Vec<(String, Triple)> = RdfParser::from_format(format)
                .for_slice(&bytes)
                .collect::<std::result::Result<Vec<oxrdf::Quad>, _>>()
                .map_err(|e| Error::Other(format!("RDF syntax: {e}")))?
                .iter()
                .map(ingest_item)
                .collect();
            c.registry.ingest_quads(&items)?;
            c.registry.refresh_namespaces()?;
        }
        Ok(c)
    }

    /// Writes the RDF content as a TriG or N-Quads snapshot (by extension), replacing the
    /// file atomically. The Ladybug database stays the live dataset.
    pub fn save(&self, data: impl AsRef<Path>) -> Result<()> {
        let data = data.as_ref();
        let format = format_of(data)?;
        if !format.supports_datasets() {
            return Err(Error::Other(format!(
                "{} must be TriG (.trig) or N-Quads (.nq) to hold a dataset",
                data.display()
            )));
        }
        let rows = self.registry.read_all_triples_ordered()?;
        let mut out = String::new();
        if format == RdfFormat::TriG {
            for (p, iri) in PREFIXES {
                out.push_str(&format!("@prefix {p}: <{iri}> .\n"));
            }
            // Default-graph triples first (bare), then one GRAPH block per graph.
            for r in rows.iter().filter(|r| r.graph == "DEFAULT") {
                out.push_str(&format!("{} {} {} .\n", r.subject, r.predicate, r.object));
            }
            let mut current: Option<&str> = None;
            for r in rows.iter().filter(|r| r.graph != "DEFAULT") {
                if current != Some(r.graph.as_str()) {
                    if current.is_some() {
                        out.push_str("}\n");
                    }
                    out.push_str(&format!("GRAPH {} {{\n", r.graph));
                    current = Some(r.graph.as_str());
                }
                out.push_str(&format!("{} {} {} .\n", r.subject, r.predicate, r.object));
            }
            if current.is_some() {
                out.push_str("}\n");
            }
        } else {
            for r in &rows {
                if r.graph == "DEFAULT" {
                    out.push_str(&format!("{} {} {} .\n", r.subject, r.predicate, r.object));
                } else {
                    out.push_str(&format!(
                        "{} {} {} {} .\n",
                        r.subject, r.predicate, r.object, r.graph
                    ));
                }
            }
        }
        let tmp = data.with_extension("tmp~");
        std::fs::write(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, data)?;
        Ok(())
    }

    /// The Ladybug database: triples, registry and projections.
    pub fn registry(&self) -> &LadybugSchemaStore {
        &self.registry
    }

    /// Runs Cypher against the Ladybug database: the registry, the projected schemas,
    /// and the triples themselves (`RdfNode` nodes, `Statement` rels).
    pub fn cypher(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        self.registry.cypher(cypher)
    }

    // ------------------------------------------------------------------------------ loading

    /// Loads RDF into a graph (a dataset format keeps its own graphs). With `replace`, the
    /// graph's triples are deleted first.
    pub fn load(
        &self,
        graph: GraphNameRef<'_>,
        data: &[u8],
        format: RdfFormat,
        replace: bool,
    ) -> Result<()> {
        let target = graph_token(graph);
        if replace {
            self.registry.delete_graph_triples(&target)?;
        }
        let items: Vec<(String, Triple)> = parse_document(graph, data, format)?
            .iter()
            .map(ingest_item)
            .collect();
        self.registry.ingest_quads(&items)?;
        self.registry.refresh_namespaces()?;
        Ok(())
    }

    /// A graph as N-Triples, reconstructed byte-exactly from its triple rows.
    pub fn graph_ntriples(&self, graph: GraphNameRef<'_>) -> Result<String> {
        let mut out = String::new();
        for r in self.registry.read_triples(&graph_token(graph))? {
            out.push_str(&format!("{} {} {} .\n", r.subject, r.predicate, r.object));
        }
        Ok(out)
    }

    /// The number of triples in a graph.
    pub fn graph_len(&self, graph: GraphNameRef<'_>) -> Result<usize> {
        self.registry.count_triples(Some(&graph_token(graph)))
    }

    // ----------------------------------------------------------------------------- registry

    /// Registers a graph, replacing any previous registration of it in the same role.
    /// `loaded_at` defaults to now when absent.
    pub fn register(&self, entry: &SchemaGraph) -> Result<()> {
        reject_blank_graph(entry.graph.as_ref())?;
        let mut e = entry.clone();
        if e.loaded_at.is_none() {
            e.loaded_at = Some(now());
        }
        e.applies_to.sort_by_key(|g| g.to_string());
        e.imports.sort();
        e.imports.dedup();
        self.registry.upsert(&e)
    }

    /// Loads a schema document into `graph` (replacing its content) and registers it with
    /// `role`, recording the document's SHA-256. For an ontology, the `owl:Ontology` IRI and
    /// its version are read from the document when not given.
    pub fn add(
        &self,
        role: SchemaRole,
        graph: GraphNameRef<'_>,
        document: &[u8],
        format: RdfFormat,
        options: &AddOptions,
    ) -> Result<SchemaGraph> {
        reject_blank_graph(graph)?;
        self.load(graph, document, format, true)?;
        let mut e = SchemaGraph::new(graph.into_owned(), role);
        e.applies_to = options.applies_to.clone();
        e.imports = options.imports.clone();
        e.active = !options.inactive;
        e.sha256 = Some(sha256_hex(document));
        e.version = options.version.clone();
        if role == SchemaRole::Ontology {
            let (iri, version) = self.ontology_header(graph)?;
            if !iri.as_ref().is_some_and(|i| e.is_named(i.as_str())) {
                e.iri = iri;
            }
            e.version = e.version.or(version);
        }
        self.register(&e)?;
        if format == RdfFormat::Turtle {
            // Explicit `@prefix` declarations become the namespaces' prefixes.
            for (prefix, iri) in harvest_turtle_prefixes(document) {
                self.registry.set_prefix(&iri, &prefix)?;
            }
        }
        if matches!(role, SchemaRole::Ontology | SchemaRole::Shacl) {
            // The schema itself is stored in Ladybug as nodes and edges, not just
            // registered: re-projecting replaces the graph's previous projection.
            self.project_graph_content(graph)?;
        }
        Ok(self
            .list()?
            .into_iter()
            .find(|r| r.graph == e.graph && r.role == role)
            .unwrap_or(e))
    }

    /// The `owl:Ontology` a graph declares (when exactly one), and its version: the
    /// `owl:versionIRI` when present, else the `owl:versionInfo`. Read from the triple
    /// rows directly.
    fn ontology_header(
        &self,
        graph: GraphNameRef<'_>,
    ) -> Result<(Option<NamedNode>, Option<String>)> {
        let type_tok = tok(RDF_TYPE);
        let onto_tok = tok(owl::ONTOLOGY);
        let mut onts = BTreeSet::new();
        let mut version_iri = None;
        let mut version_info = None;
        for r in self.registry.read_triples(&graph_token(graph))? {
            if r.predicate == type_tok && r.object == onto_tok && r.subject.starts_with('<') {
                onts.insert(r.subject[1..r.subject.len() - 1].to_owned());
            } else if r.predicate == tok(owl::VERSION_IRI) && r.object.starts_with('<') {
                let v = r.object[1..r.object.len() - 1].to_owned();
                if version_iri.is_none() {
                    version_iri = Some(v);
                }
            } else if r.predicate == tok(owl::VERSION_INFO) && version_info.is_none() {
                version_info = Some(r.lex.clone());
            }
        }
        if onts.len() != 1 {
            return Ok((None, None));
        }
        let iri = NamedNode::new(onts.into_iter().next().expect("one"))?;
        Ok((Some(iri), version_iri.or(version_info)))
    }

    /// The registrations of `graph`, in every role.
    fn entries_of(&self, graph: GraphNameRef<'_>) -> Result<Vec<SchemaGraph>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|e| e.graph.as_ref() == graph)
            .collect())
    }

    /// Sets the graphs a registration applies to (empty: every graph), keeping the rest.
    /// Every role of the graph shares the new targets.
    pub fn map(&self, graph: GraphNameRef<'_>, applies_to: &[GraphName]) -> Result<()> {
        let mut entries = self.entries_of(graph)?;
        if entries.is_empty() {
            return Err(Error::NotRegistered(graph.to_string()));
        }
        for e in &mut entries {
            e.applies_to = applies_to.to_vec();
            self.registry.upsert(e)?;
        }
        Ok(())
    }

    /// Activates or deactivates a registration, in every role of the graph.
    pub fn set_active(&self, graph: GraphNameRef<'_>, active: bool) -> Result<()> {
        let mut entries = self.entries_of(graph)?;
        if entries.is_empty() {
            return Err(Error::NotRegistered(graph.to_string()));
        }
        for e in &mut entries {
            e.active = active;
            self.registry.upsert(e)?;
        }
        Ok(())
    }

    /// Removes a registration; the graph's triples stay.
    pub fn unregister(&self, graph: GraphNameRef<'_>) -> Result<()> {
        self.require(graph)?;
        self.registry.remove_graph(&graph.into_owned())
    }

    /// Removes a registration and every triple of its graph.
    pub fn drop_graph(&self, graph: GraphNameRef<'_>) -> Result<()> {
        self.registry.delete_graph_triples(&graph_token(graph))?;
        self.registry
            .clear_content_projection(&graph.into_owned())?;
        self.registry.remove_graph(&graph.into_owned())?;
        self.registry.refresh_namespaces()?;
        Ok(())
    }

    /// Is the graph registered (in any role)?
    pub fn is_registered(&self, graph: GraphNameRef<'_>) -> Result<bool> {
        Ok(!self.entries_of(graph)?.is_empty())
    }

    fn require(&self, graph: GraphNameRef<'_>) -> Result<()> {
        if self.is_registered(graph)? {
            Ok(())
        } else {
            Err(Error::NotRegistered(graph.to_string()))
        }
    }

    /// Every registration, once per role, ordered by role and graph.
    pub fn list(&self) -> Result<Vec<SchemaGraph>> {
        self.registry.load_entries()
    }

    /// The RDF triples of a graph, parsed from its reconstructed N-Triples.
    fn graph_triples(&self, graph: GraphNameRef<'_>) -> Result<Vec<Triple>> {
        parse_ntriples(&self.graph_ntriples(graph)?)
    }

    /// Stores a registered graph's content in Ladybug as nodes and edges, replacing its
    /// previous projection: ontology classes and properties with `SUBCLASS_OF`,
    /// `EQUIVALENT_CLASS`, `DOMAIN` and `RANGE` edges, SHACL shapes with
    /// `HAS_CONSTRAINT` edges linked back via `TARGETS` and `ON_PROPERTY` (rebuilt over
    /// all shapes, since their links may point into the rebuilt graph). Unregistering
    /// keeps the projection with the triples; [`Catalog::drop_graph`] removes both.
    pub fn project_graph_content(&self, graph: GraphNameRef<'_>) -> Result<Projected> {
        let entries: Vec<SchemaGraph> = self
            .list()?
            .into_iter()
            .filter(|e| e.graph.as_ref() == graph)
            .collect();
        if entries.is_empty() {
            return Err(Error::NotRegistered(graph.to_string()));
        }
        let owned = graph.into_owned();
        let triples = self.graph_triples(graph)?;
        self.registry.clear_content_projection(&owned)?;
        let mut projected = Projected::default();
        for e in &entries {
            match e.role {
                SchemaRole::Ontology => {
                    let (classes, properties, edges) =
                        self.registry.project_ontology(&owned, &triples)?;
                    projected.classes += classes;
                    projected.properties += properties;
                    projected.edges += edges;
                }
                SchemaRole::Shacl => {
                    let (shapes, constraints) = self.registry.project_shapes(&owned, &triples)?;
                    projected.shapes += shapes;
                    projected.constraints += constraints;
                }
                SchemaRole::Shex => {}
            }
        }
        projected.links = self.registry.relink_shapes()?;
        projected.namespaces = self.registry.refresh_namespaces()?;
        Ok(projected)
    }

    /// Every namespace in the projected content, ordered by prefix: the subgraphs of
    /// the Ladybug database (see [`NamespaceInfo`]).
    pub fn namespaces(&self) -> Result<Vec<NamespaceInfo>> {
        self.registry.list_namespaces()
    }

    /// What is wrong with the registry: structural violations over the Ladybug entries.
    /// Empty means valid.
    pub fn problems(&self) -> Result<Vec<String>> {
        Ok(registry::problems(&self.list()?))
    }

    /// Compares a registered graph's recorded SHA-256 with a document.
    pub fn verify(&self, graph: GraphNameRef<'_>, document: &[u8]) -> Result<Drift> {
        let e = self
            .list()?
            .into_iter()
            .find(|e| e.graph.as_ref() == graph)
            .ok_or_else(|| Error::NotRegistered(graph.to_string()))?;
        let actual = sha256_hex(document);
        Ok(match e.sha256 {
            Some(r) if r == actual => Drift::Same,
            Some(recorded) => Drift::Changed { recorded, actual },
            None => Drift::Unrecorded { actual },
        })
    }

    // ------------------------------------------------------------------------------ scoping

    /// The `owl:imports` an ontology graph asserts about itself, read from its triple
    /// rows directly.
    fn asserted_imports(&self, graph: GraphNameRef<'_>) -> Result<Vec<String>> {
        let imports_tok = tok(owl::IMPORTS);
        let mut out = BTreeSet::new();
        for r in self.registry.read_triples(&graph_token(graph))? {
            if r.predicate == imports_tok && r.object.starts_with('<') {
                out.insert(r.object[1..r.object.len() - 1].to_owned());
            }
        }
        Ok(out.into_iter().collect())
    }

    /// The active schemas of `role` that apply to `target`. For ontologies, the active
    /// registered ontologies they import (recorded in the registry or asserted in their own
    /// graph, matched by graph name or ontology IRI) are added, transitively.
    pub fn schemas_for(
        &self,
        target: GraphNameRef<'_>,
        role: SchemaRole,
    ) -> Result<Vec<SchemaGraph>> {
        let active: Vec<SchemaGraph> = self
            .list()?
            .into_iter()
            .filter(|e| e.role == role && e.active)
            .collect();
        let mut chosen: Vec<SchemaGraph> = active
            .iter()
            .filter(|e| e.applies(target))
            .cloned()
            .collect();
        if role != SchemaRole::Ontology {
            return Ok(chosen);
        }
        let mut todo = chosen.clone();
        while let Some(e) = todo.pop() {
            let mut imports: Vec<String> =
                e.imports.iter().map(|i| i.as_str().to_owned()).collect();
            imports.extend(self.asserted_imports(e.graph.as_ref())?);
            for i in imports {
                for o in active.iter().filter(|o| o.is_named(&i)) {
                    if !chosen.iter().any(|c| c.graph == o.graph) {
                        chosen.push(o.clone());
                        todo.push(o.clone());
                    }
                }
            }
        }
        Ok(chosen)
    }

    // --------------------------------------------------------------------------- validation

    /// Validates a data graph with SHACL: against the active shapes graphs mapped to it (or
    /// those named in `options`), with the ontologies in scope added to the data graph.
    /// Every input is reconstructed from the triple rows in Ladybug.
    pub fn validate(
        &self,
        target: GraphNameRef<'_>,
        options: &ValidateOptions,
    ) -> Result<Validation> {
        let shapes_graphs: Vec<GraphName> = if options.shapes.is_empty() {
            self.schemas_for(target, SchemaRole::Shacl)?
                .into_iter()
                .map(|e| e.graph)
                .collect()
        } else {
            options.shapes.clone()
        };
        if shapes_graphs.is_empty() {
            return Err(Error::NoSchema {
                role: "SHACL shapes",
                graph: target.to_string(),
            });
        }
        let ontology_graphs: Vec<GraphName> = if options.with_ontologies {
            self.schemas_for(target, SchemaRole::Ontology)?
                .into_iter()
                .map(|e| e.graph)
                .collect()
        } else {
            Vec::new()
        };
        let mut shapes = String::new();
        for g in &shapes_graphs {
            shapes.push_str(&self.graph_ntriples(g.as_ref())?);
        }
        let mut data_rows = self.read_rows(std::slice::from_ref(&target.into_owned()))?;
        for g in &ontology_graphs {
            data_rows.extend(self.read_rows(std::slice::from_ref(g))?);
        }
        let mut data = rows_ntriples(&data_rows);
        if !ontology_graphs.is_empty() {
            data.push_str(&type_closure_rows(&data_rows)?);
        }
        let report = validate::shacl(&data, &shapes, &RDFFormat::NTriples, &options.mode)?;
        Ok(Validation {
            data_graph: target.into_owned(),
            shapes_graphs,
            ontology_graphs,
            report,
        })
    }

    /// The triple rows of several graphs, in order.
    fn read_rows(&self, graphs: &[GraphName]) -> Result<Vec<TripleRow>> {
        let mut out = Vec::new();
        for g in graphs {
            out.extend(self.registry.read_triples(&graph_token(g.as_ref()))?);
        }
        Ok(out)
    }

    /// Validates the nodes of a ShEx shape map in a data graph against a ShEx schema (ShExC
    /// text; ShEx graphs in the registry are recorded, the schema text is passed here).
    pub fn validate_shex(
        &self,
        target: GraphNameRef<'_>,
        shexc: &str,
        base: &str,
        shape_map: &str,
    ) -> Result<Vec<ShexResult>> {
        validate::shex(&self.graph_ntriples(target)?, shexc, base, shape_map)
    }
}

/// Triple rows as N-Triples lines.
fn rows_ntriples(rows: &[TripleRow]) -> String {
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!("{} {} {} .\n", r.subject, r.predicate, r.object));
    }
    out
}

/// A graph named by a blank node cannot be registered: no other graph can name it.
fn reject_blank_graph(graph: GraphNameRef<'_>) -> Result<()> {
    match graph {
        GraphNameRef::BlankNode(_) => Err(Error::Other(
            "a graph named by a blank node cannot be registered as schema".into(),
        )),
        _ => Ok(()),
    }
}

/// The `xsd:dateTime` of now, where a clock is available.
fn now() -> String {
    oxsdatatypes::DateTime::now().to_string()
}

/// The `rdf:type` triples entailed through `rdfs:subClassOf` and `owl:equivalentClass`, as
/// N-Triples. SHACL's `sh:targetClass` and `sh:class` follow `rdfs:subClassOf` in the data
/// graph; writing the closure out makes every engine agree, whatever its class index does.
/// Computed from triple rows by graph traversal.
fn type_closure_rows(rows: &[TripleRow]) -> Result<String> {
    let subclass_tok = tok(rdfs::SUB_CLASS_OF);
    let equiv_tok = tok(owl::EQUIVALENT_CLASS);
    let type_tok = tok(RDF_TYPE);
    let named = |t: &str| t.starts_with('<');
    // Child class -> parent classes, through subClassOf and equivalentClass (both ways).
    let mut parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in rows {
        if !named(&r.subject) || !named(&r.object) {
            continue;
        }
        if r.predicate == subclass_tok {
            parents
                .entry(r.subject.clone())
                .or_default()
                .insert(r.object.clone());
        } else if r.predicate == equiv_tok {
            parents
                .entry(r.subject.clone())
                .or_default()
                .insert(r.object.clone());
            parents
                .entry(r.object.clone())
                .or_default()
                .insert(r.subject.clone());
        }
    }
    let mut out = String::new();
    for r in rows {
        if r.predicate != type_tok || !named(&r.object) {
            continue;
        }
        // Transitive parents of the asserted class (the class itself is already stated).
        let mut seen = BTreeSet::from([r.object.clone()]);
        let mut todo: Vec<String> = parents
            .get(&r.object)
            .map(|ps| ps.iter().cloned().collect())
            .unwrap_or_default();
        while let Some(p) = todo.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            out.push_str(&format!("{} <{RDF_TYPE}> {p} .\n", r.subject));
            if let Some(next) = parents.get(&p) {
                todo.extend(next.iter().cloned());
            }
        }
    }
    Ok(out)
}

/// Parses a graph argument: an IRI, `<IRI>`, or `DEFAULT` for the default graph.
pub fn parse_graph(s: &str) -> Result<GraphName> {
    let s = s.trim_start_matches('<').trim_end_matches('>');
    if s.eq_ignore_ascii_case("default") {
        return Ok(GraphName::DefaultGraph);
    }
    Ok(NamedNode::new(s)?.into())
}

/// Parses `applies-to` targets: `ALL` (or none) is every graph.
pub fn parse_targets<S: AsRef<str>>(items: &[S]) -> Result<Vec<GraphName>> {
    if items.iter().any(|s| s.as_ref().eq_ignore_ascii_case("all")) {
        return Ok(Vec::new());
    }
    items.iter().map(|s| parse_graph(s.as_ref())).collect()
}

/// The `@prefix name: <iri>` declarations of a Turtle document, for namespace prefixes.
/// Hand-rolled line scan (the RDF parser does not expose prefixes); anything else
/// falls back to derived prefixes.
fn harvest_turtle_prefixes(document: &[u8]) -> Vec<(String, String)> {
    let Ok(text) = std::str::from_utf8(document) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("@prefix") else {
            continue;
        };
        let Some((name, rest)) = rest.trim_start().split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty()
            || !name.starts_with(|c: char| c.is_ascii_alphabetic())
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let iri = rest
            .trim_start()
            .trim_end_matches('.')
            .trim_end()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim();
        if iri.is_empty() {
            continue;
        }
        out.push((name.to_owned(), iri.to_owned()));
    }
    out
}
