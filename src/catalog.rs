//! [`Catalog`]: an RDF dataset with a schema registry — load ontologies, shapes and data into
//! named graphs, register and map the schemas, resolve imports, and validate data graphs
//! against the shapes that apply to them.
//!
//! Graph content (RDF triples) lives in an Oxigraph store, loaded from and saved to a TriG
//! or N-Quads file. The registry — which graph holds which kind of schema and which data
//! graphs each one governs — lives in Ladybug (see [`crate::ladybug`]) and speaks Cypher,
//! not SPARQL: [`Catalog::cypher`] runs Cypher against it. Content inspection (ontology
//! headers, `owl:imports`, triple counts, the subclass closure for validation) reads the
//! store directly, without a query language.

use crate::error::{Error, Result};
use crate::ladybug::LadybugSchemaStore;
use crate::registry::{self, owl, rdfs, SchemaGraph, SchemaRole, RDF_TYPE};
use crate::validate::{self, Mode, Report, ShexResult};
use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use oxigraph::model::{GraphName, GraphNameRef, NamedNode, NamedOrBlankNode, Term};
use oxigraph::store::Store;
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

/// An RDF dataset with a Ladybug-backed schema registry.
#[derive(Clone)]
pub struct Catalog {
    store: Store,
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

impl Catalog {
    /// An empty dataset with an in-memory registry.
    pub fn new() -> Result<Self> {
        Ok(Self {
            store: Store::new()?,
            registry: LadybugSchemaStore::open_in_memory()?,
        })
    }

    /// Opens a dataset: RDF content from the TriG or N-Quads file at `data` (an empty
    /// dataset if the file does not exist) and the registry from the Ladybug database at
    /// `registry` (created when missing). See [`default_registry_path`].
    pub fn open(data: impl AsRef<Path>, registry: impl AsRef<Path>) -> Result<Self> {
        let data = data.as_ref();
        let store = Store::new()?;
        if data.exists() {
            let format = format_of(data)?;
            store.load_from_reader(
                RdfParser::from_format(format),
                std::fs::read(data)?.as_slice(),
            )?;
        }
        Ok(Self {
            store,
            registry: LadybugSchemaStore::open(registry)?,
        })
    }

    /// Saves the RDF content (TriG or N-Quads, by extension), replacing the file atomically.
    /// The registry needs no saving: Ladybug persists it as it changes.
    pub fn save(&self, data: impl AsRef<Path>) -> Result<()> {
        let data = data.as_ref();
        let format = format_of(data)?;
        if !format.supports_datasets() {
            return Err(Error::Other(format!(
                "{} must be TriG (.trig) or N-Quads (.nq) to hold a dataset",
                data.display()
            )));
        }
        let mut serializer = RdfSerializer::from_format(format);
        if format == RdfFormat::TriG {
            for (p, iri) in PREFIXES {
                serializer = serializer.with_prefix(p, iri).expect("valid prefix");
            }
        }
        // Grouped by graph and subject, so each graph is written as one block.
        let mut quads = self
            .store
            .iter()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        quads.sort_by_cached_key(|q| (q.graph_name.to_string(), q.subject.to_string()));
        let mut writer = serializer.for_writer(Vec::new());
        for q in &quads {
            writer.serialize_quad(q)?;
        }
        let bytes = writer.finish()?;
        let tmp = data.with_extension("tmp~");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, data)?;
        Ok(())
    }

    /// The underlying Oxigraph store, for anything the catalog does not wrap.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The Ladybug schema registry.
    pub fn registry(&self) -> &LadybugSchemaStore {
        &self.registry
    }

    /// Runs Cypher against the schema registry (see
    /// [`LadybugSchemaStore::cypher`]). This replaces the old SPARQL query surface: the
    /// registry is a property graph, so it speaks Cypher.
    pub fn cypher(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        self.registry.cypher(cypher)
    }

    // ------------------------------------------------------------------------------ loading

    /// Loads RDF into a graph (a dataset format keeps its own graphs). With `replace`, the
    /// graph is cleared first.
    pub fn load(
        &self,
        graph: GraphNameRef<'_>,
        data: &[u8],
        format: RdfFormat,
        replace: bool,
    ) -> Result<()> {
        if replace {
            self.store.clear_graph(graph)?;
        }
        let parser = RdfParser::from_format(format).with_default_graph(graph.into_owned());
        let parser = match graph {
            GraphNameRef::NamedNode(n) => parser.with_base_iri(n.as_str()).unwrap_or_else(|_| {
                RdfParser::from_format(format).with_default_graph(graph.into_owned())
            }),
            _ => parser,
        };
        self.store.load_from_reader(parser, data)?;
        Ok(())
    }

    /// A graph as N-Triples.
    pub fn graph_ntriples(&self, graph: GraphNameRef<'_>) -> Result<String> {
        let bytes = self
            .store
            .dump_graph_to_writer(graph, RdfFormat::NTriples, Vec::new())?;
        String::from_utf8(bytes).map_err(|e| Error::Other(e.to_string()))
    }

    /// The number of triples in a graph, counted from the store directly.
    pub fn graph_len(&self, graph: GraphNameRef<'_>) -> Result<usize> {
        let target = graph.into_owned();
        Ok(self
            .store
            .iter()
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .filter(|q| q.graph_name == target)
            .count())
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
        Ok(self
            .list()?
            .into_iter()
            .find(|r| r.graph == e.graph && r.role == role)
            .unwrap_or(e))
    }

    /// The `owl:Ontology` a graph declares (when exactly one), and its version: the
    /// `owl:versionIRI` when present, else the `owl:versionInfo`. Read from the store
    /// directly, without SPARQL.
    fn ontology_header(
        &self,
        graph: GraphNameRef<'_>,
    ) -> Result<(Option<NamedNode>, Option<String>)> {
        let target = graph.into_owned();
        let mut onts = BTreeSet::new();
        let mut version_iri = None;
        let mut version_info = None;
        for q in self
            .store
            .iter()
            .collect::<std::result::Result<Vec<_>, _>>()?
        {
            if q.graph_name != target {
                continue;
            }
            match (q.predicate.as_str(), &q.object) {
                (RDF_TYPE, Term::NamedNode(c)) if c.as_str() == owl::ONTOLOGY => {
                    if let NamedOrBlankNode::NamedNode(s) = &q.subject {
                        onts.insert(s.as_str().to_owned());
                    }
                }
                (p, Term::NamedNode(v)) if p == owl::VERSION_IRI => {
                    version_iri = Some(v.as_str().to_owned());
                }
                (p, Term::Literal(v)) if p == owl::VERSION_INFO => {
                    version_info = Some(v.value().to_owned());
                }
                _ => {}
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
        self.registry.remove_graph(&graph.into_owned())?;
        self.store.clear_graph(graph)?;
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

    /// The `owl:imports` an ontology graph asserts about itself, read from the store
    /// directly, without SPARQL.
    fn asserted_imports(&self, graph: GraphNameRef<'_>) -> Result<Vec<String>> {
        let target = graph.into_owned();
        let mut out = BTreeSet::new();
        for q in self
            .store
            .iter()
            .collect::<std::result::Result<Vec<_>, _>>()?
        {
            if q.graph_name != target || q.predicate.as_str() != owl::IMPORTS {
                continue;
            }
            if let Term::NamedNode(n) = &q.object {
                out.insert(n.as_str().to_owned());
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
        let mut data = self.graph_ntriples(target)?;
        if !ontology_graphs.is_empty() {
            for g in &ontology_graphs {
                data.push_str(&self.graph_ntriples(g.as_ref())?);
            }
            data.push_str(&type_closure(&data)?);
        }
        let report = validate::shacl(&data, &shapes, &RDFFormat::NTriples, &options.mode)?;
        Ok(Validation {
            data_graph: target.into_owned(),
            shapes_graphs,
            ontology_graphs,
            report,
        })
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
/// Computed by graph traversal, without SPARQL.
fn type_closure(ntriples: &str) -> Result<String> {
    use oxrdfio::RdfParser;
    let triples: Vec<oxrdf::Triple> = RdfParser::from_format(oxrdfio::RdfFormat::NTriples)
        .for_slice(ntriples.as_bytes())
        .map(|q| {
            q.map(|q| oxrdf::Triple {
                subject: q.subject,
                predicate: q.predicate,
                object: q.object,
            })
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Other(format!("data graph: {e}")))?;
    // Child class -> parent classes, through subClassOf and equivalentClass (both ways).
    let mut parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in &triples {
        let (child, parent) = match (t.predicate.as_str(), &t.subject, &t.object) {
            (p, NamedOrBlankNode::NamedNode(c), Term::NamedNode(pa)) if p == rdfs::SUB_CLASS_OF => {
                (c.as_str(), pa.as_str())
            }
            (p, NamedOrBlankNode::NamedNode(a), Term::NamedNode(b))
                if p == owl::EQUIVALENT_CLASS =>
            {
                parents
                    .entry(a.as_str().to_owned())
                    .or_default()
                    .insert(b.as_str().to_owned());
                (b.as_str(), a.as_str())
            }
            _ => continue,
        };
        parents
            .entry(child.to_owned())
            .or_default()
            .insert(parent.to_owned());
    }
    let mut out = String::new();
    for t in &triples {
        let (subject, class) = match (&t.subject, &t.object) {
            (s, Term::NamedNode(c)) if t.predicate.as_str() == RDF_TYPE => (s, c.as_str()),
            _ => continue,
        };
        // Transitive parents of the asserted class (the class itself is already stated).
        let mut seen = BTreeSet::from([class.to_owned()]);
        let mut todo: Vec<String> = parents
            .get(class)
            .map(|ps| ps.iter().cloned().collect())
            .unwrap_or_default();
        while let Some(p) = todo.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            out.push_str(&format!(
                "{subject} <{RDF_TYPE}> <{p}> .\n",
                subject = subject
            ));
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
