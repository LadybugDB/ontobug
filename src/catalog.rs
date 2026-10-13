//! [`Catalog`]: an RDF dataset with a schema registry — load ontologies, shapes and data into
//! named graphs, register and map the schemas, resolve imports, and validate data graphs
//! against the shapes that apply to them.
//!
//! The dataset is an in-memory Oxigraph store, loaded from and saved to a TriG or N-Quads file.
//! Every registry operation is the portable SPARQL of [`crate::registry`].

use crate::error::{Error, Result};
use crate::registry::{self, vocab, SchemaGraph, SchemaRole, NS, SCHEMA_GRAPH, SYSTEM_GRAPHS};
use crate::validate::{self, Mode, Report, ShexResult};
use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use oxigraph::model::{GraphName, GraphNameRef, NamedNode, Term};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use rudof_rdf::rdf_core::RDFFormat;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;

/// Prefixes used when saving a dataset as TriG.
const PREFIXES: [(&str, &str); 8] = [
    ("oxl", NS),
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
    /// Imports to record in the registry (`oxl:imports`), besides the `owl:imports` the
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

/// An RDF dataset with a schema registry.
#[derive(Clone)]
pub struct Catalog {
    store: Store,
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

impl Catalog {
    /// An empty dataset, without system graphs (see [`Catalog::install_system_graphs`]).
    pub fn new() -> Result<Self> {
        Ok(Self {
            store: Store::new()?,
        })
    }

    /// The dataset of a TriG or N-Quads file; an empty one if the file does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let c = Self::new()?;
        if path.exists() {
            let format = format_of(path)?;
            c.store.load_from_reader(
                RdfParser::from_format(format),
                std::fs::read(path)?.as_slice(),
            )?;
        }
        Ok(c)
    }

    /// Saves the dataset (TriG or N-Quads, by extension), replacing the file atomically.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let format = format_of(path)?;
        if !format.supports_datasets() {
            return Err(Error::Other(format!(
                "{} must be TriG (.trig) or N-Quads (.nq) to hold a dataset",
                path.display()
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
        let tmp = path.with_extension("tmp~");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The underlying Oxigraph store, for anything the catalog does not wrap.
    pub fn store(&self) -> &Store {
        &self.store
    }

    // ------------------------------------------------------------------------------- SPARQL

    /// Runs a SPARQL update.
    pub fn update(&self, sparql: &str) -> Result<()> {
        SparqlEvaluator::new()
            .parse_update(sparql)?
            .on_store(&self.store)
            .execute()?;
        Ok(())
    }

    /// Runs a SPARQL query.
    pub fn query(&self, sparql: &str) -> Result<QueryResults<'static>> {
        Ok(SparqlEvaluator::new()
            .parse_query(sparql)?
            .on_store(&self.store)
            .execute()?)
    }

    /// The rows of a SELECT, for the given variables.
    fn rows(&self, sparql: &str, vars: &[&str]) -> Result<Vec<Vec<Option<Term>>>> {
        let QueryResults::Solutions(solutions) = self.query(sparql)? else {
            return Err(Error::Other("expected a SELECT".into()));
        };
        let mut out = Vec::new();
        for s in solutions {
            let s = s?;
            out.push(vars.iter().map(|v| s.get(*v).cloned()).collect());
        }
        Ok(out)
    }

    fn ask(&self, sparql: &str) -> Result<bool> {
        match self.query(sparql)? {
            QueryResults::Boolean(b) => Ok(b),
            _ => Err(Error::Other("expected an ASK".into())),
        }
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

    /// The number of triples in a graph.
    pub fn graph_len(&self, graph: GraphNameRef<'_>) -> Result<usize> {
        let rows = self.rows(&registry::size_query(graph), &["n"])?;
        Ok(match rows.first().and_then(|r| r[0].as_ref()) {
            Some(Term::Literal(l)) => l.value().parse().unwrap_or(0),
            _ => 0,
        })
    }

    // ----------------------------------------------------------------------------- registry

    /// Installs (or refreshes) `<oxilite:schema>` and `<oxilite:vocabulary>`; does nothing when
    /// they are already at the current vocabulary version. Returns whether it wrote anything.
    pub fn install_system_graphs(&self) -> Result<bool> {
        if self.ask(&registry::system_graphs_ready_query())? {
            return Ok(false);
        }
        self.update(&registry::system_graphs_update())?;
        Ok(true)
    }

    /// Registers a graph, replacing any previous description of it.
    pub fn register(&self, entry: &SchemaGraph) -> Result<()> {
        self.update(&registry::register_update(entry)?)
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
        registry::graph_node(graph)?;
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

    /// The `owl:Ontology` a graph declares (when exactly one), and its version.
    fn ontology_header(
        &self,
        graph: GraphNameRef<'_>,
    ) -> Result<(Option<NamedNode>, Option<String>)> {
        let q = format!(
            "SELECT ?o ?vi ?v WHERE {{ {} {{ ?o a <{}> \
             OPTIONAL {{ ?o <{}> ?vi }} OPTIONAL {{ ?o <{}> ?v }} }} }}",
            graph_clause(graph),
            vocab::OWL_ONTOLOGY,
            vocab::OWL_VERSION_IRI,
            vocab::OWL_VERSION_INFO,
        );
        let rows = self.rows(&q, &["o", "vi", "v"])?;
        let onts: BTreeSet<String> = rows
            .iter()
            .filter_map(|r| match &r[0] {
                Some(Term::NamedNode(n)) => Some(n.as_str().to_owned()),
                _ => None,
            })
            .collect();
        if onts.len() != 1 {
            return Ok((None, None));
        }
        let iri = NamedNode::new(onts.into_iter().next().expect("one"))?;
        let version = rows.iter().find_map(|r| match (&r[1], &r[2]) {
            (Some(Term::NamedNode(n)), _) => Some(n.as_str().to_owned()),
            (_, Some(Term::Literal(l))) => Some(l.value().to_owned()),
            _ => None,
        });
        Ok((Some(iri), version))
    }

    /// Sets the graphs a registration applies to (empty: every graph), keeping the rest.
    pub fn map(&self, graph: GraphNameRef<'_>, applies_to: &[GraphName]) -> Result<()> {
        self.require(graph)?;
        self.update(&registry::remap_update(graph, applies_to)?)
    }

    /// Activates or deactivates a registration.
    pub fn set_active(&self, graph: GraphNameRef<'_>, active: bool) -> Result<()> {
        self.require(graph)?;
        self.update(&registry::set_active_update(graph, active)?)
    }

    /// Removes a registration; the graph's triples stay.
    pub fn unregister(&self, graph: GraphNameRef<'_>) -> Result<()> {
        self.require(graph)?;
        self.update(&registry::unregister_update(graph)?)
    }

    /// Removes a registration and every triple of its graph.
    pub fn drop_graph(&self, graph: GraphNameRef<'_>) -> Result<()> {
        self.update(&registry::drop_update(graph)?)
    }

    /// Is the graph registered (in any role)?
    pub fn is_registered(&self, graph: GraphNameRef<'_>) -> Result<bool> {
        self.ask(&registry::registered_query(graph)?)
    }

    fn require(&self, graph: GraphNameRef<'_>) -> Result<()> {
        if self.is_registered(graph)? {
            Ok(())
        } else {
            Err(Error::NotRegistered(graph.to_string()))
        }
    }

    fn registry_rows(&self) -> Result<Vec<Vec<Option<Term>>>> {
        self.rows(&registry::entries_query(), &["g", "p", "o"])
    }

    /// Every registration, once per role, ordered by role and graph.
    pub fn list(&self) -> Result<Vec<SchemaGraph>> {
        Ok(registry::entries_from_rows(&self.registry_rows()?))
    }

    /// What is wrong with the registry: the violations of `oxl:RegistrationShape`, found
    /// without a SHACL engine. Empty means valid.
    pub fn problems(&self) -> Result<Vec<String>> {
        Ok(registry::problems(&self.registry_rows()?))
    }

    /// Validates the registry graph against `oxl:RegistrationShape` with a SHACL engine.
    pub fn check_registry(&self) -> Result<Report> {
        let data = self.graph_ntriples(named(SCHEMA_GRAPH).as_ref())?;
        validate::shacl(
            &data,
            registry::VOCABULARY,
            &RDFFormat::Turtle,
            &Mode::Native,
        )
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

    /// The `owl:imports` an ontology graph asserts about itself (or anything in it).
    fn asserted_imports(&self, graph: GraphNameRef<'_>) -> Result<Vec<String>> {
        let q = format!(
            "SELECT DISTINCT ?i WHERE {{ {} {{ ?o <{}> ?i }} }}",
            graph_clause(graph),
            vocab::OWL_IMPORTS
        );
        Ok(self
            .rows(&q, &["i"])?
            .into_iter()
            .filter_map(|r| match &r[0] {
                Some(Term::NamedNode(n)) => Some(n.as_str().to_owned()),
                _ => None,
            })
            .collect())
    }

    /// The active schemas of `role` that apply to `target`. For ontologies, the active
    /// registered ontologies they import (recorded in the registry or asserted in their own
    /// graph, matched by graph name or `oxl:ontologyIri`) are added, transitively.
    pub fn schemas_for(
        &self,
        target: GraphNameRef<'_>,
        role: SchemaRole,
    ) -> Result<Vec<SchemaGraph>> {
        let active: Vec<SchemaGraph> = self
            .list()?
            .into_iter()
            .filter(|e| e.role == role && e.active && !is_system(&e.graph))
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

/// The `rdf:type` triples entailed through `rdfs:subClassOf` and `owl:equivalentClass`, as
/// N-Triples. SHACL's `sh:targetClass` and `sh:class` follow `rdfs:subClassOf` in the data
/// graph; writing the closure out makes every engine agree, whatever its class index does.
fn type_closure(ntriples: &str) -> Result<String> {
    let store = Store::new()?;
    store.load_from_reader(RdfFormat::NTriples, ntriples.as_bytes())?;
    let q = "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
             PREFIX owl: <http://www.w3.org/2002/07/owl#> \
             CONSTRUCT { ?x a ?super } WHERE { \
               ?x a ?c . \
               ?c (rdfs:subClassOf|owl:equivalentClass|^owl:equivalentClass)+ ?super . \
               FILTER (isIRI(?super)) }";
    let QueryResults::Graph(triples) = SparqlEvaluator::new()
        .parse_query(q)?
        .on_store(&store)
        .execute()?
    else {
        unreachable!("a CONSTRUCT returns triples");
    };
    let mut out = String::new();
    for t in triples {
        out.push_str(&format!("{} .\n", t?));
    }
    Ok(out)
}

fn named(iri: &str) -> GraphName {
    NamedNode::new_unchecked(iri).into()
}

fn is_system(g: &GraphName) -> bool {
    matches!(g, GraphName::NamedNode(n) if SYSTEM_GRAPHS.contains(&n.as_str()))
}

/// `GRAPH <g>` for a named graph, nothing for the default graph (the default graph of an
/// Oxigraph query is the store's default graph).
fn graph_clause(g: GraphNameRef<'_>) -> String {
    match g {
        GraphNameRef::NamedNode(n) => format!("GRAPH {n}"),
        _ => String::new(),
    }
}

/// Parses a graph argument: an IRI, `<IRI>`, or `DEFAULT` for the default graph.
pub fn parse_graph(s: &str) -> Result<GraphName> {
    if s.eq_ignore_ascii_case("default") {
        return Ok(GraphName::DefaultGraph);
    }
    let s = s.trim_start_matches('<').trim_end_matches('>');
    if s == vocab::DEFAULT_GRAPH {
        return Ok(GraphName::DefaultGraph);
    }
    Ok(NamedNode::new(s)?.into())
}

/// Parses `applies-to` targets: `ALL` (or none) is every graph.
pub fn parse_targets<S: AsRef<str>>(items: &[S]) -> Result<Vec<GraphName>> {
    if items.iter().any(|s| {
        s.as_ref().eq_ignore_ascii_case("all")
            || s.as_ref().trim_matches(['<', '>']) == vocab::ALL_GRAPHS
    }) {
        return Ok(Vec::new());
    }
    items.iter().map(|s| parse_graph(s.as_ref())).collect()
}
