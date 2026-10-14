//! The schema registry in Ladybug: one `SchemaEntry` node per registration.
//!
//! This is the store of record for which named graphs hold ontologies, SHACL shapes or
//! ShEx schemas and which data graphs each one governs. There is no RDF registry graph
//! and no SPARQL involved: every operation below is parameterised Cypher through the
//! `lbug` crate's Rust API (`Connection::prepare` plus `Connection::execute` for writes,
//! `Connection::query` for reads), so graph IRIs never touch a query string.
//!
//! ```no_run
//! use ontobug::{Catalog, LadybugSchemaStore};
//!
//! let catalog = Catalog::open("dataset.trig", "dataset.lbdb")?;
//! assert_eq!(catalog.list()?.len(), catalog.registry().load_entries()?.len());
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```

use crate::error::{Error, Result};
use crate::registry::{SchemaGraph, SchemaRole, RDF_TYPE};
use lbug::{Connection, Database, LogicalType, SystemConfig, Value};
use oxrdf::{GraphName, GraphNameRef, NamedNode};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

/// The default graph as stored in Ladybug (a plain label round-trips losslessly, while an
/// IRI for it would be ambiguous with a real named graph).
const DEFAULT_LABEL: &str = "DEFAULT";

/// A Ladybug-backed schema registry: one `SchemaEntry` node per registration.
///
/// The store shares ownership of the Ladybug [`Database`] through an [`Arc`], so it — and
/// the [`Catalog`](crate::Catalog) holding it — stays [`Clone`]. A fresh [`Connection`]
/// is opened per operation, which keeps the type free of the connection's borrow of the
/// database.
#[derive(Clone)]
pub struct LadybugSchemaStore {
    db: Arc<Database>,
    path: PathBuf,
}

/// The Cypher schema of the registry. Lists are JSON arrays in `STRING` properties so one
/// node holds a whole registration without relationship tables.
const CREATE_TABLE: &str = "CREATE NODE TABLE SchemaEntry(\
    id STRING PRIMARY KEY, \
    graph STRING, \
    role STRING, \
    ontologyIri STRING, \
    version STRING, \
    sha256 STRING, \
    active BOOLEAN, \
    loadedAt STRING, \
    appliesTo STRING, \
    imports STRING)";

/// Node and relationship tables for projected schema content: the schemas themselves as
/// a property graph (see [`LadybugSchemaStore::project_ontology`]), plus the lossless
/// triple store underneath it all (see [`LadybugSchemaStore::insert_triples`]).
const CONTENT_TABLES: [&str; 14] = [
    "CREATE NODE TABLE OntologyClass(\
        id STRING PRIMARY KEY, iri STRING, label STRING, comment STRING, namespace STRING, graph STRING)",
    "CREATE NODE TABLE OntologyProperty(\
        id STRING PRIMARY KEY, iri STRING, kind STRING, label STRING, comment STRING, \
        rangeDatatypes STRING, namespace STRING, graph STRING)",
    "CREATE NODE TABLE ShaclShape(\
        id STRING PRIMARY KEY, shape STRING, targetClasses STRING, targetNodes STRING, \
        targetSubjectsOf STRING, targetObjectsOf STRING, message STRING, severity STRING, \
        deactivated BOOLEAN, namespace STRING, graph STRING)",
    "CREATE NODE TABLE ShaclProperty(\
        id STRING PRIMARY KEY, shapeId STRING, path STRING, datatype STRING, class STRING, \
        nodeKind STRING, minCount INT64, maxCount INT64, pattern STRING, \
        minInclusive STRING, maxInclusive STRING, message STRING, severity STRING, \
        deactivated BOOLEAN, namespace STRING, graph STRING)",
    "CREATE REL TABLE SUBCLASS_OF(FROM OntologyClass TO OntologyClass)",
    "CREATE REL TABLE EQUIVALENT_CLASS(FROM OntologyClass TO OntologyClass)",
    "CREATE REL TABLE DOMAIN(FROM OntologyProperty TO OntologyClass)",
    "CREATE REL TABLE RANGE(FROM OntologyProperty TO OntologyClass)",
    "CREATE REL TABLE HAS_CONSTRAINT(FROM ShaclShape TO ShaclProperty)",
    "CREATE REL TABLE TARGETS(FROM ShaclShape TO OntologyClass)",
    "CREATE REL TABLE ON_PROPERTY(FROM ShaclProperty TO OntologyProperty)",
    "CREATE NODE TABLE Namespace(\
        iri STRING PRIMARY KEY, prefix STRING, sourceGraphs STRING, termCount INT64, triples INT64)",
    "CREATE NODE TABLE RdfNode(\
        id STRING PRIMARY KEY, kind STRING, value STRING, datatype STRING, lang STRING, namespace STRING)",
    "CREATE REL TABLE Statement(\
        FROM RdfNode TO RdfNode, predicate STRING, graph STRING, namespace STRING, ord INT64)",
];

/// Content relationship tables with their endpoint node tables.
// RDF predicates read while projecting schema content.
const RDFS_CLASS: &str = "http://www.w3.org/2000/01/rdf-schema#Class";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const RDFS_COMMENT: &str = "http://www.w3.org/2000/01/rdf-schema#comment";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
const RDF_PROPERTY: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property";
const OWL_CLASS: &str = "http://www.w3.org/2002/07/owl#Class";
const OWL_OBJECT_PROPERTY: &str = "http://www.w3.org/2002/07/owl#ObjectProperty";
const OWL_DATATYPE_PROPERTY: &str = "http://www.w3.org/2002/07/owl#DatatypeProperty";
const OWL_ANNOTATION_PROPERTY: &str = "http://www.w3.org/2002/07/owl#AnnotationProperty";
const OWL_EQUIVALENT_CLASS: &str = "http://www.w3.org/2002/07/owl#equivalentClass";
const OWL_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const SDO_DOMAIN_INCLUDES: &str = "http://schema.org/domainIncludes";
const SDO_RANGE_INCLUDES: &str = "http://schema.org/rangeIncludes";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";
const SH_NS: &str = "http://www.w3.org/ns/shacl#";

fn sh(local: &str) -> String {
    format!("{SH_NS}{local}")
}

/// An RDF namespace as a Ladybug subgraph: the naming scope shared by terms, kept as one
/// connected graph (RDF namespaces partition names, not data, so cross-namespace edges
/// stay traversable). Every content node carries its `namespace`; `Namespace` nodes are
/// the inventory, maintained by [`LadybugSchemaStore::refresh_namespaces`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceInfo {
    /// The namespace IRI (`http://schema.org/`), i.e. the term IRIs up to and including
    /// their last `#` or `/`.
    pub iri: String,
    /// Short prefix, harvested from `@prefix` declarations or derived from the IRI.
    pub prefix: String,
    /// RDF named graphs whose terms fall in this namespace.
    pub graphs: Vec<String>,
    /// Projected content nodes in this namespace.
    pub terms: usize,
    /// Triple rows whose subject falls in this namespace.
    pub triples: usize,
}

/// One RDF triple as stored losslessly in Ladybug: subject/predicate/object are
/// N-Triples tokens (`<iri>`, `_:label`, `"lex"^^<dt>`, `"lex"@lang`), so rows
/// concatenate back to byte-exact N-Triples; `lex` is the unescaped literal value
/// ("" for non-literals); `graph` is a graph token (see [`graph_token`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripleRow {
    pub graph: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub lex: String,
    pub namespace: String,
}

/// A graph name as stored in triple rows: the N-Triples token for named (`<iri>`) and
/// blank (`_:label`) graphs, `DEFAULT` for the default graph.
pub(crate) fn graph_token(graph: GraphNameRef<'_>) -> String {
    match graph {
        GraphNameRef::NamedNode(n) => n.to_string(),
        GraphNameRef::DefaultGraph => DEFAULT_LABEL.to_owned(),
        GraphNameRef::BlankNode(b) => b.to_string(),
    }
}

/// The namespace of a term IRI: up to and including its last `#`, `/` or `:` past the
/// scheme ("" for blank labels and bare hosts).
fn namespace_of(iri: &str) -> String {
    if iri.starts_with("_:") {
        return String::new();
    }
    match iri.rfind(['#', '/', ':']) {
        Some(i) if i >= 8 => iri[..i + 1].to_owned(),
        _ => String::new(),
    }
}

/// A term IRI without `<…>`, `^` path markers or blank-node prefixes, for namespace
/// computation over stored display forms.
fn bare_iri(s: &str) -> &str {
    s.strip_prefix('^').unwrap_or(s).trim_matches(['<', '>'])
}

fn sanitize_prefix(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// A short prefix for a namespace IRI: the last path segment (`…/hr#` → `hr`), else the
/// host's first label (`http://schema.org/` → `schema`), suffixed on collision.
fn auto_prefix(iri: &str, taken: &mut std::collections::BTreeSet<String>) -> String {
    let no_scheme = iri.split("://").nth(1).unwrap_or(iri);
    let mut base = String::new();
    let trimmed = no_scheme.trim_end_matches(['#', '/']);
    if trimmed.contains('/') || trimmed.contains('#') {
        if let Some(seg) = trimmed.rsplit(['#', '/']).next() {
            base = sanitize_prefix(seg);
        }
    }
    if base.is_empty() {
        let host = no_scheme.split('/').next().unwrap_or("");
        let host = host.strip_prefix("www.").unwrap_or(host);
        base = sanitize_prefix(host.split('.').next().unwrap_or(""));
    }
    if base.is_empty() || !base.starts_with(|c: char| c.is_ascii_alphabetic()) {
        base = format!("ns{base}");
    }
    let mut i = 0;
    loop {
        let cand = if i == 0 {
            base.clone()
        } else {
            format!("{base}{i}")
        };
        if taken.insert(cand.clone()) {
            return cand;
        }
        i += 1;
    }
}

/// What projecting one graph's content built (see
/// [`Catalog::project_graph_content`](crate::Catalog::project_graph_content)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Projected {
    pub classes: usize,
    pub properties: usize,
    /// `SUBCLASS_OF`, `EQUIVALENT_CLASS`, `DOMAIN` and `RANGE` edges.
    pub edges: usize,
    pub shapes: usize,
    pub constraints: usize,
    /// `TARGETS` plus `ON_PROPERTY` links into the ontology, rebuilt over all shapes.
    pub links: usize,
    /// Namespaces present after the refresh.
    pub namespaces: usize,
}

impl LadybugSchemaStore {
    /// Opens (creating when missing) the Ladybug database at `path` and ensures the
    /// `SchemaEntry` table exists. `path` is a `.lbdb` path, e.g. `"dataset.lbdb"`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let db = Arc::new(
            Database::new(path.clone(), SystemConfig::default())
                .map_err(|e| Error::Ladybug(e.to_string()))?,
        );
        let store = Self { db, path };
        store.ensure_table()?;
        Ok(store)
    }

    /// An in-memory registry. Nothing is written to disk; all entries vanish with the process.
    pub fn open_in_memory() -> Result<Self> {
        let db = Arc::new(
            Database::new(":memory:", SystemConfig::default())
                .map_err(|e| Error::Ladybug(e.to_string()))?,
        );
        let store = Self {
            db,
            path: PathBuf::from(":memory:"),
        };
        store.ensure_table()?;
        Ok(store)
    }

    /// Where this registry is persisted (`:memory:` for [`Self::open_in_memory`]).
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connection(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| Error::Ladybug(e.to_string()))
    }

    fn ensure_table(&self) -> Result<()> {
        let conn = self.connection()?;
        // `IF NOT EXISTS` keeps re-opens idempotent across lbug versions that accept it;
        // otherwise fall back to a plain CREATE and ignore "already exists".
        let mut ddls = vec![CREATE_TABLE.to_string()];
        ddls.extend(CONTENT_TABLES.iter().map(|s| s.to_string()));
        for ddl in &ddls {
            Self::create_ignoring_exists(&conn, ddl)?;
        }
        // Databases written before a column existed gain it here: `ADD IF NOT EXISTS`
        // NULL-fills existing rows, and the backfill derives their values.
        for (table, column, dtype) in [
            ("OntologyClass", "namespace", "STRING"),
            ("OntologyProperty", "namespace", "STRING"),
            ("ShaclShape", "namespace", "STRING"),
            ("ShaclProperty", "namespace", "STRING"),
            ("Namespace", "triples", "INT64"),
        ] {
            conn.query(&format!(
                "ALTER TABLE {table} ADD IF NOT EXISTS {column} {dtype}"
            ))
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        }
        self.backfill_namespaces()?;
        self.refresh_namespaces()?;
        Ok(())
    }

    fn create_ignoring_exists(conn: &Connection<'_>, ddl: &str) -> Result<()> {
        let if_not_exists = ddl.replacen("TABLE ", "TABLE IF NOT EXISTS ", 1);
        if conn.query(&if_not_exists).is_err() {
            match conn.query(ddl) {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("already exists") && !msg.contains("Exists") {
                        return Err(Error::Ladybug(msg));
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs Cypher against the registry and returns the rows, each value through its
    /// display form. This is the query surface that replaces SPARQL: the registry is a
    /// property graph, so it speaks Cypher, e.g.
    /// `MATCH (e:SchemaEntry) WHERE e.role = 'ontology' RETURN e.graph`.
    pub fn cypher(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let conn = self.connection()?;
        let result = conn
            .query(cypher)
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(result
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect())
    }

    /// Saves `entries`, replacing any previous node with the same graph and role.
    pub fn save_entries(&self, entries: &[SchemaGraph]) -> Result<()> {
        for e in entries {
            self.upsert(e)?;
        }
        Ok(())
    }

    /// Registers a graph, replacing any previous registration of it in the same role.
    pub fn upsert(&self, entry: &SchemaGraph) -> Result<()> {
        let conn = self.connection()?;
        let id = entry_id(&entry.graph, entry.role);
        // Delete-then-create: portable across Ladybug versions without relying on MERGE.
        let mut del = conn
            .prepare("MATCH (e:SchemaEntry) WHERE e.id = $id DETACH DELETE e")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        conn.execute(&mut del, vec![("id", Value::String(id.clone()))])
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut create = conn
            .prepare(
                "CREATE (:SchemaEntry {id: $id, graph: $graph, role: $role, \
                 ontologyIri: $ontologyIri, version: $version, sha256: $sha256, \
                 active: $active, loadedAt: $loadedAt, appliesTo: $appliesTo, imports: $imports})",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        conn.execute(
            &mut create,
            vec![
                ("id", Value::String(id)),
                ("graph", Value::String(graph_label(&entry.graph))),
                ("role", Value::String(entry.role.name().to_owned())),
                (
                    "ontologyIri",
                    Value::String(
                        entry
                            .iri
                            .as_ref()
                            .map(|i| i.as_str().to_owned())
                            .unwrap_or_default(),
                    ),
                ),
                (
                    "version",
                    Value::String(entry.version.clone().unwrap_or_default()),
                ),
                (
                    "sha256",
                    Value::String(entry.sha256.clone().unwrap_or_default()),
                ),
                ("active", Value::Bool(entry.active)),
                (
                    "loadedAt",
                    Value::String(entry.loaded_at.clone().unwrap_or_default()),
                ),
                (
                    "appliesTo",
                    Value::String(serde_json::to_string(
                        &entry.applies_to.iter().map(graph_label).collect::<Vec<_>>(),
                    )?),
                ),
                (
                    "imports",
                    Value::String(serde_json::to_string(
                        &entry
                            .imports
                            .iter()
                            .map(|i| i.as_str().to_owned())
                            .collect::<Vec<_>>(),
                    )?),
                ),
            ],
        )
        .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(())
    }

    /// Removes the registrations of `graph` in every role.
    pub fn remove_graph(&self, graph: &GraphName) -> Result<()> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("MATCH (e:SchemaEntry) WHERE e.graph = $graph DETACH DELETE e")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        conn.execute(
            &mut stmt,
            vec![("graph", Value::String(graph_label(graph)))],
        )
        .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(())
    }

    /// Deletes every registration node.
    pub fn clear(&self) -> Result<()> {
        let conn = self.connection()?;
        conn.query("MATCH (e:SchemaEntry) DETACH DELETE e")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(())
    }

    /// Every registration, ordered by role then graph, like
    /// [`Catalog::list`](crate::Catalog::list).
    pub fn load_entries(&self) -> Result<Vec<SchemaGraph>> {
        let conn = self.connection()?;
        let result = conn
            .query(
                "MATCH (e:SchemaEntry) RETURN e.graph, e.role, e.ontologyIri, e.version, \
                 e.sha256, e.active, e.loadedAt, e.appliesTo, e.imports",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut out = Vec::new();
        for row in result {
            let g = value_string(&row, 0)?;
            let r = value_string(&row, 1)?;
            let role = SchemaRole::from_str(&r)?;
            let graph = if g == DEFAULT_LABEL {
                GraphName::DefaultGraph
            } else {
                GraphName::NamedNode(NamedNode::new(&g)?)
            };
            let mut e = SchemaGraph::new(graph, role);
            let iri = value_string(&row, 2)?;
            if !iri.is_empty() {
                e.iri = Some(NamedNode::new(&iri)?);
            }
            let version = value_string(&row, 3)?;
            if !version.is_empty() {
                e.version = Some(version);
            }
            let sha256 = value_string(&row, 4)?;
            if !sha256.is_empty() {
                e.sha256 = Some(sha256);
            }
            e.active = value_bool(&row, 5)?;
            let loaded = value_string(&row, 6)?;
            if !loaded.is_empty() {
                e.loaded_at = Some(loaded);
            }
            let applies: Vec<String> = serde_json::from_str(&value_string(&row, 7)?)?;
            e.applies_to = applies
                .into_iter()
                .map(|s| {
                    if s == DEFAULT_LABEL {
                        Ok(GraphName::DefaultGraph)
                    } else {
                        NamedNode::new(&s).map(GraphName::from).map_err(Error::from)
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            let imports: Vec<String> = serde_json::from_str(&value_string(&row, 8)?)?;
            e.imports = imports
                .into_iter()
                .map(|s| NamedNode::new(&s).map_err(Error::from))
                .collect::<Result<Vec<_>>>()?;
            e.applies_to.sort_by_key(|g| g.to_string());
            e.imports.sort();
            e.imports.dedup();
            out.push(e);
        }
        out.sort_by_key(|e: &SchemaGraph| (e.role, e.graph.to_string()));
        Ok(out)
    }
}

/// The node id of a registration: graph plus role, so one graph can hold several roles.
fn entry_id(graph: &GraphName, role: SchemaRole) -> String {
    format!("{}\x1f{}", graph_label(graph), role.name())
}

fn graph_label(g: &GraphName) -> String {
    match g {
        GraphName::NamedNode(n) => n.as_str().to_owned(),
        _ => DEFAULT_LABEL.to_owned(),
    }
}

fn value_string(row: &[Value], i: usize) -> Result<String> {
    match row.get(i) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Null(_)) => Ok(String::new()),
        Some(v) => Ok(v.to_string()),
        None => Err(Error::Other(format!("ladybug row has no column {i}"))),
    }
}

fn value_bool(row: &[Value], i: usize) -> Result<bool> {
    match row.get(i) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::Null(_)) => Ok(true),
        Some(v) => Ok(v.to_string() != "False" && v.to_string() != "false"),
        None => Err(Error::Other(format!("ladybug row has no column {i}"))),
    }
}

// ---------------------------------------------------------------------------
// Content projection: schema graphs as nodes and edges.
//
// The registry (`SchemaEntry` nodes) says *which* graphs hold schemas. The projection
// stores the schemas *themselves*: ontology classes and properties with `SUBCLASS_OF`,
// `EQUIVALENT_CLASS`, `DOMAIN` and `RANGE` edges, and SHACL shapes with
// `HAS_CONSTRAINT` edges linked back into the ontology via `TARGETS` and `ON_PROPERTY`.
// Re-projecting a graph replaces its nodes and edges; shapes are relinked globally
// afterwards, since their links may point into the rebuilt graph.

/// The node id of a content node: owning graph plus IRI, so two graphs can project the
/// same IRI without colliding.
fn content_id(graph_label: &str, iri: &str) -> String {
    format!("{graph_label}\x1f{iri}")
}

fn opt_int(v: Option<i64>) -> Value {
    match v {
        Some(i) => Value::Int64(i),
        None => Value::Null(LogicalType::Int64),
    }
}

fn subj_iri(t: &oxrdf::Triple) -> Option<String> {
    match &t.subject {
        oxrdf::NamedOrBlankNode::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => None,
    }
}

/// A node id for `iri`: this graph's node when present, else any graph's node with the
/// same IRI (edge endpoints stubbed by another graph's projection stay connected).
fn resolve_node(
    pool: &std::collections::BTreeSet<String>,
    glabel: &str,
    iri: &str,
) -> Option<String> {
    let own = content_id(glabel, iri);
    if pool.contains(&own) {
        return Some(own);
    }
    pool.iter()
        .find(|id| id.ends_with(&format!("\x1f{iri}")))
        .cloned()
}

fn iri_of(term: &oxrdf::Term) -> Option<String> {
    match term {
        oxrdf::Term::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => None,
    }
}

fn set_kind(
    map: &mut std::collections::BTreeMap<String, (String, String, String)>,
    s: String,
    kind: &str,
) {
    map.entry(s)
        .and_modify(|p| {
            if p.0 == "property" || p.0.is_empty() {
                p.0 = kind.to_owned();
            }
        })
        .or_insert_with(|| (kind.to_owned(), String::new(), String::new()));
}

fn literal_of(term: &oxrdf::Term) -> Option<String> {
    match term {
        oxrdf::Term::Literal(l) => Some(l.value().to_owned()),
        _ => None,
    }
}

impl LadybugSchemaStore {
    /// Deletes every projected content node of `graph` (classes, properties, shapes and
    /// their constraints, with their edges). Registry nodes are untouched.
    pub fn clear_content_projection(&self, graph: &GraphName) -> Result<()> {
        let conn = self.connection()?;
        let glabel = graph_label(graph);
        for table in [
            "OntologyClass",
            "OntologyProperty",
            "ShaclShape",
            "ShaclProperty",
        ] {
            let mut stmt = conn
                .prepare(&format!(
                    "MATCH (n:{table}) WHERE n.graph = $graph DETACH DELETE n"
                ))
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            conn.execute(&mut stmt, vec![("graph", Value::String(glabel.clone()))])
                .map_err(|e| Error::Ladybug(e.to_string()))?;
        }
        Ok(())
    }

    /// The ids already stored for a node table, across all graphs (to skip duplicate
    /// stubs when an edge points at a class another graph projected).
    fn existing_ids(&self, table: &str) -> Result<std::collections::BTreeSet<String>> {
        Ok(self
            .cypher(&format!("MATCH (n:{table}) RETURN n.id"))?
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .collect())
    }

    /// Projects an ontology graph's triples: classes, properties, and the subclass,
    /// equivalence, domain and range edges. Returns `(classes, properties, edges)`.
    /// Callers clear the graph's projection first (see
    /// [`Self::clear_content_projection`]).
    pub fn project_ontology(
        &self,
        graph: &GraphName,
        triples: &[oxrdf::Triple],
    ) -> Result<(usize, usize, usize)> {
        let glabel = graph_label(graph);
        let mut classes: std::collections::BTreeMap<String, (String, String)> =
            std::collections::BTreeMap::new();
        let mut props: std::collections::BTreeMap<String, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut datatypes: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        // Declarations first, so labels/comments only attach to declared terms.
        for t in triples {
            let Some(s) = subj_iri(t) else {
                continue;
            };
            if t.predicate.as_str() != RDF_TYPE {
                continue;
            }
            let Some(o) = iri_of(&t.object) else {
                continue;
            };
            match o.as_str() {
                RDFS_CLASS | OWL_CLASS => {
                    classes.entry(s).or_default();
                }
                OWL_OBJECT_PROPERTY => set_kind(&mut props, s, "object"),
                OWL_DATATYPE_PROPERTY => set_kind(&mut props, s, "datatype"),
                OWL_ANNOTATION_PROPERTY => set_kind(&mut props, s, "annotation"),
                RDF_PROPERTY => set_kind(&mut props, s, "property"),
                _ => {}
            }
        }
        for t in triples {
            let (p, o) = (t.predicate.as_str(), &t.object);
            if p != RDFS_LABEL && p != RDFS_COMMENT {
                continue;
            }
            let (Some(s), Some(v)) = (subj_iri(t), literal_of(o)) else {
                continue;
            };
            if p == RDFS_LABEL {
                if let Some(c) = classes.get_mut(&s) {
                    if c.0.is_empty() {
                        c.0 = v;
                    }
                } else if let Some(pr) = props.get_mut(&s) {
                    if pr.1.is_empty() {
                        pr.1 = v;
                    }
                }
            } else if let Some(c) = classes.get_mut(&s) {
                if c.1.is_empty() {
                    c.1 = v;
                }
            } else if let Some(pr) = props.get_mut(&s) {
                if pr.2.is_empty() {
                    pr.2 = v;
                }
            }
        }
        // Edges: (predicate-group, from-iri, to-iri).
        let mut subclass = Vec::new();
        let mut equiv = Vec::new();
        let mut domain = Vec::new();
        let mut range = Vec::new();
        for t in triples {
            let (Some(s), p) = (subj_iri(t), t.predicate.as_str()) else {
                continue;
            };
            let is_edge = matches!(
                p,
                OWL_SUBCLASS_OF
                    | OWL_EQUIVALENT_CLASS
                    | RDFS_DOMAIN
                    | RDFS_RANGE
                    | SDO_DOMAIN_INCLUDES
                    | SDO_RANGE_INCLUDES
            );
            if !is_edge {
                continue;
            }
            let Some(o) = iri_of(&t.object) else {
                continue;
            };
            match p {
                OWL_SUBCLASS_OF => subclass.push((s, o)),
                OWL_EQUIVALENT_CLASS => equiv.push((s, o)),
                RDFS_DOMAIN | SDO_DOMAIN_INCLUDES => domain.push((s, o)),
                _ if o.starts_with(XSD_NS) => {
                    datatypes.entry(s).or_default().insert(o);
                }
                _ => range.push((s, o)),
            }
        }
        let conn = self.connection()?;
        let mut have_classes = self.existing_ids("OntologyClass")?;
        let mut have_props = self.existing_ids("OntologyProperty")?;
        let mut class_inserts: Vec<Vec<Value>> = Vec::new();
        let mut class_rows: Vec<(String, String, String, String)> = classes
            .iter()
            .map(|(iri, (label, comment))| {
                (
                    content_id(&glabel, iri),
                    iri.clone(),
                    label.clone(),
                    comment.clone(),
                )
            })
            .collect();
        // Stubs for edge endpoints no graph projected (external classes). An IRI
        // projected by any graph is skipped: edges resolve to that node instead.
        let class_known = |iri: &str| {
            have_classes.contains(&content_id(&glabel, iri))
                || have_classes
                    .iter()
                    .any(|id| id.ends_with(&format!("\x1f{iri}")))
        };
        let mut stubs: Vec<String> = subclass
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .chain(equiv.iter().flat_map(|(a, b)| [a.clone(), b.clone()]))
            .chain(domain.iter().map(|(_, b)| b.clone()))
            .chain(range.iter().map(|(_, b)| b.clone()))
            .collect();
        stubs.sort();
        stubs.dedup();
        for iri in stubs {
            if classes.contains_key(&iri) || class_known(&iri) {
                continue;
            }
            class_rows.push((content_id(&glabel, &iri), iri, String::new(), String::new()));
        }
        for (id, iri, label, comment) in class_rows {
            if have_classes.insert(id.clone()) {
                let ns = namespace_of(&iri);
                class_inserts.push(vec![
                    Value::String(id),
                    Value::String(iri),
                    Value::String(label),
                    Value::String(comment),
                    Value::String(ns),
                    Value::String(glabel.clone()),
                ]);
            }
        }
        Self::batch_insert_nodes(
            &conn,
            "OntologyClass",
            &["id", "iri", "label", "comment", "namespace", "graph"],
            class_inserts,
        )?;
        let class_nodes = have_classes
            .iter()
            .filter(|id| id.starts_with(&format!("{glabel}\u{1f}")))
            .count();
        // NOTE: the graph's projection was cleared first, so every own-graph id above
        // is one this call inserted.
        let mut prop_inserts: Vec<Vec<Value>> = Vec::new();
        for (iri, (kind, label, comment)) in &props {
            let id = content_id(&glabel, iri);
            if have_props.insert(id.clone()) {
                let dts = datatypes.get(iri).cloned().unwrap_or_default();
                let ns = namespace_of(iri);
                prop_inserts.push(vec![
                    Value::String(id),
                    Value::String(iri.clone()),
                    Value::String(kind.clone()),
                    Value::String(label.clone()),
                    Value::String(comment.clone()),
                    Value::String(serde_json::to_string(&dts)?),
                    Value::String(ns),
                    Value::String(glabel.clone()),
                ]);
            }
        }
        // Stubs for properties only named by domain/range triples; same cross-graph
        // skip as for classes.
        let mut pstubs: Vec<String> = domain
            .iter()
            .map(|(a, _)| a.clone())
            .chain(range.iter().map(|(a, _)| a.clone()))
            .chain(datatypes.keys().cloned())
            .collect();
        pstubs.sort();
        pstubs.dedup();
        for iri in pstubs {
            let id = content_id(&glabel, &iri);
            if props.contains_key(&iri)
                || have_props.contains(&id)
                || have_props
                    .iter()
                    .any(|existing| existing.ends_with(&format!("\x1f{iri}")))
            {
                continue;
            }
            let dts = datatypes.get(&iri).cloned().unwrap_or_default();
            if have_props.insert(id.clone()) {
                let ns = namespace_of(&iri);
                prop_inserts.push(vec![
                    Value::String(id),
                    Value::String(iri),
                    Value::String(String::new()),
                    Value::String(String::new()),
                    Value::String(String::new()),
                    Value::String(serde_json::to_string(&dts)?),
                    Value::String(ns),
                    Value::String(glabel.clone()),
                ]);
            }
        }
        Self::batch_insert_nodes(
            &conn,
            "OntologyProperty",
            &[
                "id",
                "iri",
                "kind",
                "label",
                "comment",
                "rangeDatatypes",
                "namespace",
                "graph",
            ],
            prop_inserts,
        )?;
        let prop_nodes = have_props
            .iter()
            .filter(|id| id.starts_with(&format!("{glabel}\u{1f}")))
            .count();
        // Edges resolve to this graph's nodes first, then to any graph's node with the
        // same IRI, so projections that share classes stay connected.
        let mut edges = 0;
        for (rel, from_table, to_table, pairs, from_pool, to_pool) in [
            (
                "SUBCLASS_OF",
                "OntologyClass",
                "OntologyClass",
                &subclass,
                &have_classes,
                &have_classes,
            ),
            (
                "EQUIVALENT_CLASS",
                "OntologyClass",
                "OntologyClass",
                &equiv,
                &have_classes,
                &have_classes,
            ),
            (
                "DOMAIN",
                "OntologyProperty",
                "OntologyClass",
                &domain,
                &have_props,
                &have_classes,
            ),
            (
                "RANGE",
                "OntologyProperty",
                "OntologyClass",
                &range,
                &have_props,
                &have_classes,
            ),
        ] {
            let mut resolved = Vec::new();
            for (a, b) in pairs {
                if let (Some(from), Some(to)) = (
                    resolve_node(from_pool, &glabel, a),
                    resolve_node(to_pool, &glabel, b),
                ) {
                    resolved.push((from, to));
                }
            }
            edges += Self::batch_insert_rels(&conn, rel, from_table, to_table, &resolved)?;
        }
        Ok((class_nodes, prop_nodes, edges))
    }

    /// Projects a SHACL shapes graph's triples: shape and property-constraint nodes with
    /// `HAS_CONSTRAINT` edges. Returns `(shapes, constraints)`. Links back into the
    /// ontology (`TARGETS`, `ON_PROPERTY`) are rebuilt by [`Self::relink_shapes`].
    pub fn project_shapes(
        &self,
        graph: &GraphName,
        triples: &[oxrdf::Triple],
    ) -> Result<(usize, usize)> {
        let glabel = graph_label(graph);
        let mut by_subj: std::collections::BTreeMap<String, Vec<&oxrdf::Triple>> =
            std::collections::BTreeMap::new();
        for t in triples {
            by_subj.entry(t.subject.to_string()).or_default().push(t);
        }
        let objs = |key: &str, pred: &str| -> Vec<&oxrdf::Term> {
            by_subj
                .get(key)
                .map(|ts| {
                    ts.iter()
                        .filter(|t| t.predicate.as_str() == pred)
                        .map(|t| &t.object)
                        .collect()
                })
                .unwrap_or_default()
        };
        let conn = self.connection()?;
        let strings = |terms: Vec<&oxrdf::Term>| -> Vec<String> {
            terms.into_iter().filter_map(iri_of).collect()
        };
        let first_literal = |terms: Vec<&oxrdf::Term>| -> String {
            terms
                .into_iter()
                .filter_map(literal_of)
                .next()
                .unwrap_or_default()
        };
        let first_int = |terms: Vec<&oxrdf::Term>| -> Option<i64> {
            terms
                .into_iter()
                .filter_map(literal_of)
                .find_map(|v| v.parse().ok())
        };
        let first_iri = |terms: Vec<&oxrdf::Term>| -> String {
            terms
                .into_iter()
                .filter_map(iri_of)
                .next()
                .unwrap_or_default()
        };
        let is_true = |terms: Vec<&oxrdf::Term>| -> bool {
            terms
                .into_iter()
                .filter_map(literal_of)
                .any(|v| v == "true" || v == "1")
        };
        let mut shapes = 0;
        let mut constraints = 0;
        let mut shape_rows: Vec<Vec<Value>> = Vec::new();
        let mut constraint_rows: Vec<Vec<Value>> = Vec::new();
        let mut has_pairs: Vec<(String, String)> = Vec::new();
        let mut keys: Vec<String> = by_subj.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let is_node_shape = objs(&key, RDF_TYPE)
                .into_iter()
                .filter_map(iri_of)
                .any(|c| c == sh("NodeShape"));
            if !is_node_shape {
                continue;
            }
            shapes += 1;
            let sid = content_id(&glabel, &key);
            let mut target_classes = strings(objs(&key, &sh("targetClass")));
            // A shape that is also a class instance targets that class implicitly.
            for t in objs(&key, RDF_TYPE) {
                let Some(c) = iri_of(t) else {
                    continue;
                };
                if !c.starts_with(SH_NS) && c != RDFS_CLASS && c != OWL_CLASS {
                    target_classes.push(c);
                }
            }
            target_classes.sort();
            target_classes.dedup();
            shape_rows.push(vec![
                Value::String(sid.clone()),
                Value::String(key.clone()),
                Value::String(serde_json::to_string(&target_classes)?),
                Value::String(serde_json::to_string(
                    &objs(&key, &sh("targetNode"))
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )?),
                Value::String(serde_json::to_string(&strings(objs(
                    &key,
                    &sh("targetSubjectsOf"),
                )))?),
                Value::String(serde_json::to_string(&strings(objs(
                    &key,
                    &sh("targetObjectsOf"),
                )))?),
                Value::String(first_literal(objs(&key, &sh("message")))),
                Value::String(first_iri(objs(&key, &sh("severity")))),
                Value::Bool(is_true(objs(&key, &sh("deactivated")))),
                Value::String(namespace_of(bare_iri(&key))),
                Value::String(glabel.clone()),
            ]);
            let prop_terms: Vec<String> = objs(&key, &sh("property"))
                .into_iter()
                .map(ToString::to_string)
                .collect();
            for (i, pkey) in prop_terms.iter().enumerate() {
                constraints += 1;
                let cid = format!("{sid}#{i}");
                let path_terms = objs(pkey, &sh("path"));
                // A bare IRI with no triples of its own is its own path.
                let mut path = path_terms
                    .into_iter()
                    .filter_map(iri_of)
                    .next()
                    .unwrap_or_default();
                if path.is_empty() {
                    if pkey.starts_with("<") && pkey.ends_with(">") {
                        path = pkey.trim_matches(['<', '>']).to_owned();
                    } else {
                        // Blank path node: `^iri` for inverse paths, else the blank label.
                        path = objs(pkey, &sh("inversePath"))
                            .into_iter()
                            .filter_map(iri_of)
                            .next()
                            .map(|iri| format!("^{iri}"))
                            .unwrap_or_else(|| pkey.clone());
                    }
                }
                constraint_rows.push(vec![
                    Value::String(cid.clone()),
                    Value::String(sid.clone()),
                    Value::String(path.clone()),
                    Value::String(first_iri(objs(pkey, &sh("datatype")))),
                    Value::String(first_iri(objs(pkey, &sh("class")))),
                    Value::String(first_iri(objs(pkey, &sh("nodeKind")))),
                    opt_int(first_int(objs(pkey, &sh("minCount")))),
                    opt_int(first_int(objs(pkey, &sh("maxCount")))),
                    Value::String(first_literal(objs(pkey, &sh("pattern")))),
                    Value::String(first_literal(objs(pkey, &sh("minInclusive")))),
                    Value::String(first_literal(objs(pkey, &sh("maxInclusive")))),
                    Value::String(first_literal(objs(pkey, &sh("message")))),
                    Value::String(first_iri(objs(pkey, &sh("severity")))),
                    Value::Bool(is_true(objs(pkey, &sh("deactivated")))),
                    Value::String(namespace_of(bare_iri(&path))),
                    Value::String(glabel.clone()),
                ]);
                has_pairs.push((sid.clone(), cid));
            }
        }
        Self::batch_insert_nodes(
            &conn,
            "ShaclShape",
            &[
                "id",
                "shape",
                "targetClasses",
                "targetNodes",
                "targetSubjectsOf",
                "targetObjectsOf",
                "message",
                "severity",
                "deactivated",
                "namespace",
                "graph",
            ],
            shape_rows,
        )?;
        Self::batch_insert_nodes(
            &conn,
            "ShaclProperty",
            &[
                "id",
                "shapeId",
                "path",
                "datatype",
                "class",
                "nodeKind",
                "minCount",
                "maxCount",
                "pattern",
                "minInclusive",
                "maxInclusive",
                "message",
                "severity",
                "deactivated",
                "namespace",
                "graph",
            ],
            constraint_rows,
        )?;
        Self::batch_insert_rels(
            &conn,
            "HAS_CONSTRAINT",
            "ShaclShape",
            "ShaclProperty",
            &has_pairs,
        )?;
        Ok((shapes, constraints))
    }

    /// Rebuilds the links from shapes into the ontology over all shapes: `TARGETS` from a
    /// shape's target classes to matching `OntologyClass` nodes (any graph), and
    /// `ON_PROPERTY` from a constraint's path to the matching `OntologyProperty` node.
    /// Returns the number of links. Run after every projection: rebuilding a graph deletes
    /// the class nodes its shapes pointed at.
    pub fn relink_shapes(&self) -> Result<usize> {
        let conn = self.connection()?;
        conn.query("MATCH ()-[r:TARGETS]->() DELETE r")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        conn.query("MATCH ()-[r:ON_PROPERTY]->() DELETE r")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut mk_targets = conn
            .prepare(
                "MATCH (s:ShaclShape) WHERE s.id = $shape MATCH (c:OntologyClass) WHERE c.iri = $iri \
                 CREATE (s)-[:TARGETS]->(c)",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        for row in self.cypher("MATCH (s:ShaclShape) RETURN s.id, s.targetClasses")? {
            let (Some(sid), Some(targets)) = (row.first(), row.get(1)) else {
                continue;
            };
            let iris: Vec<String> = serde_json::from_str(targets).unwrap_or_default();
            for iri in iris {
                conn.execute(
                    &mut mk_targets,
                    vec![
                        ("shape", Value::String(sid.clone())),
                        ("iri", Value::String(iri)),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            }
        }
        let mut mk_on = conn
            .prepare(
                "MATCH (p:ShaclProperty) WHERE p.id = $prop MATCH (o:OntologyProperty) WHERE o.iri = $iri \
                 CREATE (p)-[:ON_PROPERTY]->(o)",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        for row in self.cypher("MATCH (p:ShaclProperty) RETURN p.id, p.path")? {
            let (Some(pid), Some(path)) = (row.first(), row.get(1)) else {
                continue;
            };
            let iri = path.strip_prefix('^').unwrap_or(path);
            if !iri.contains("://") {
                continue;
            }
            conn.execute(
                &mut mk_on,
                vec![
                    ("prop", Value::String(pid.clone())),
                    ("iri", Value::String(iri.to_owned())),
                ],
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        }
        let count = |cypher: &str| -> Result<usize> {
            Ok(self
                .cypher(cypher)?
                .first()
                .and_then(|r| r.first())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0))
        };
        Ok(count("MATCH ()-[r:TARGETS]->() RETURN count(r)")?
            + count("MATCH ()-[r:ON_PROPERTY]->() RETURN count(r)")?)
    }

    /// Runs Cypher with parameters (see [`Self::cypher`]), returning display strings.
    pub fn cypher_params(
        &self,
        cypher: &str,
        params: Vec<(&str, Value)>,
    ) -> Result<Vec<Vec<String>>> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(cypher)
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let result = conn
            .execute(&mut stmt, params)
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(result
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect())
    }

    /// Records an explicit prefix for a namespace IRI (e.g. harvested from a Turtle
    /// document's `@prefix` declarations). The prefix sticks: [`Self::refresh_namespaces`]
    /// keeps existing prefixes and only derives new ones. A prefix taken by another
    /// namespace is left alone.
    pub fn set_prefix(&self, iri: &str, prefix: &str) -> Result<()> {
        if iri.is_empty() || prefix.is_empty() {
            return Ok(());
        }
        let rows = self.cypher_params("MATCH (n:Namespace) RETURN n.iri, n.prefix", vec![])?;
        let mut taken: std::collections::BTreeSet<String> = BTreeSet::new();
        let mut owned: Option<String> = None;
        for row in rows {
            if row.len() < 2 {
                continue;
            }
            taken.insert(row[1].clone());
            if row[0] == iri {
                owned = Some(row[1].clone());
            }
        }
        if owned.as_deref() == Some(prefix) {
            return Ok(());
        }
        if taken.contains(prefix) {
            return Ok(());
        }
        let conn = self.connection()?;
        if owned.is_some() {
            let mut stmt = conn
                .prepare("MATCH (n:Namespace) WHERE n.iri = $iri SET n.prefix = $prefix")
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            conn.execute(
                &mut stmt,
                vec![
                    ("iri", Value::String(iri.to_owned())),
                    ("prefix", Value::String(prefix.to_owned())),
                ],
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        } else {
            let mut stmt = conn
                .prepare(
                    "CREATE (:Namespace {iri: $iri, prefix: $prefix, sourceGraphs: $graphs, termCount: $count})",
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            conn.execute(
                &mut stmt,
                vec![
                    ("iri", Value::String(iri.to_owned())),
                    ("prefix", Value::String(prefix.to_owned())),
                    ("graphs", Value::String("[]".to_owned())),
                    ("count", Value::Int64(0)),
                ],
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        }
        Ok(())
    }

    /// Backfills the `namespace` property on content nodes projected before namespaces
    /// existed (or with an empty one), deriving it from each node's own IRI.
    fn backfill_namespaces(&self) -> Result<()> {
        let conn = self.connection()?;
        for (table, source) in [
            ("OntologyClass", "iri"),
            ("OntologyProperty", "iri"),
            ("ShaclShape", "shape"),
            ("ShaclProperty", "path"),
        ] {
            let rows = self.cypher(&format!(
                "MATCH (n:{table}) WHERE n.namespace IS NULL OR n.namespace = '' \
                 RETURN n.id, n.{source}"
            ))?;
            if rows.is_empty() {
                continue;
            }
            let mut stmt = conn
                .prepare(&format!(
                    "MATCH (n:{table}) WHERE n.id = $id SET n.namespace = $namespace"
                ))
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            for row in rows {
                if row.len() < 2 {
                    continue;
                }
                conn.execute(
                    &mut stmt,
                    vec![
                        ("id", Value::String(row[0].clone())),
                        ("namespace", Value::String(namespace_of(bare_iri(&row[1])))),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Rebuilds the `Namespace` inventory from the projected content and the triple
    /// rows: one node per namespace with its term and triple counts and source graphs.
    /// Existing prefixes stick; brand-new namespaces get derived ones. Returns the
    /// number of namespaces.
    pub fn refresh_namespaces(&self) -> Result<usize> {
        self.backfill_namespaces()?;
        let mut prefixes: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for row in self.cypher("MATCH (n:Namespace) RETURN n.iri, n.prefix")? {
            if row.len() == 2 && !row[1].is_empty() {
                prefixes.insert(row[0].clone(), row[1].clone());
            }
        }
        let mut taken: std::collections::BTreeSet<String> = prefixes.values().cloned().collect();
        let mut terms: std::collections::BTreeMap<String, (usize, BTreeSet<String>)> =
            std::collections::BTreeMap::new();
        let mut triple_counts: std::collections::BTreeMap<String, (usize, BTreeSet<String>)> =
            std::collections::BTreeMap::new();
        for table in [
            "OntologyClass",
            "OntologyProperty",
            "ShaclShape",
            "ShaclProperty",
        ] {
            for row in self.cypher(&format!(
                "MATCH (n:{table}) WHERE NOT n.namespace = '' RETURN n.namespace, n.graph"
            ))? {
                if row.len() < 2 || row[0].is_empty() {
                    continue;
                }
                let e = terms.entry(row[0].clone()).or_insert((0, BTreeSet::new()));
                e.0 += 1;
                e.1.insert(row[1].clone());
            }
        }
        for row in self.cypher(
            "MATCH ()-[r:Statement]->() WHERE NOT r.namespace = '' RETURN r.namespace, r.graph",
        )? {
            if row.len() < 2 || row[0].is_empty() {
                continue;
            }
            let e = triple_counts
                .entry(row[0].clone())
                .or_insert((0, BTreeSet::new()));
            e.0 += 1;
            e.1.insert(row[1].clone());
        }
        let conn = self.connection()?;
        conn.query("MATCH (n:Namespace) DETACH DELETE n")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut mk = conn
            .prepare(
                "CREATE (:Namespace {iri: $iri, prefix: $prefix, sourceGraphs: $graphs, termCount: $count, triples: $triples})",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut iris: BTreeSet<String> = terms.keys().cloned().collect();
        iris.extend(triple_counts.keys().cloned());
        for iri in &iris {
            let prefix = prefixes
                .get(iri)
                .cloned()
                .unwrap_or_else(|| auto_prefix(iri, &mut taken));
            taken.insert(prefix.clone());
            let mut graphs: BTreeSet<String> = BTreeSet::new();
            let mut count = 0;
            let mut triples = 0;
            if let Some((c, g)) = terms.get(iri) {
                count = *c;
                graphs.extend(g.iter().cloned());
            }
            if let Some((c, g)) = triple_counts.get(iri) {
                triples = *c;
                graphs.extend(g.iter().cloned());
            }
            // Graph tokens are `<iri>`/`_:label`/`DEFAULT`; inventory shows plain IRIs.
            let graphs: Vec<String> = graphs
                .into_iter()
                .map(|g| {
                    g.strip_prefix('<')
                        .and_then(|s| s.strip_suffix('>'))
                        .unwrap_or(&g)
                        .to_owned()
                })
                .collect();
            conn.execute(
                &mut mk,
                vec![
                    ("iri", Value::String(iri.clone())),
                    ("prefix", Value::String(prefix)),
                    ("graphs", Value::String(serde_json::to_string(&graphs)?)),
                    ("count", Value::Int64(count as i64)),
                    ("triples", Value::Int64(triples as i64)),
                ],
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        }
        Ok(iris.len())
    }

    /// Every namespace, ordered by prefix.
    pub fn list_namespaces(&self) -> Result<Vec<NamespaceInfo>> {
        let mut out = Vec::new();
        for row in
            self.cypher("MATCH (n:Namespace) RETURN n.iri, n.prefix, n.sourceGraphs, n.termCount, n.triples ORDER BY n.prefix")?
        {
            if row.len() < 4 {
                continue;
            }
            let graphs: Vec<String> = serde_json::from_str(&row[2]).unwrap_or_default();
            out.push(NamespaceInfo {
                iri: row[0].clone(),
                prefix: row[1].clone(),
                graphs,
                terms: row[3].parse().unwrap_or(0),
                triples: row.get(4).and_then(|v| v.parse().ok()).unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Bulk node insert: one multi-`CREATE` with numbered parameters per chunk instead of
    /// one transaction per row (orders of magnitude faster on large documents). Each inner
    /// vec holds the column values in `cols` order.
    fn batch_insert_nodes(
        conn: &Connection<'_>,
        table: &str,
        cols: &[&str],
        rows: Vec<Vec<Value>>,
    ) -> Result<()> {
        const BATCH: usize = 500;
        for chunk in rows.chunks(BATCH) {
            let mut owned: Vec<(String, Value)> = Vec::new();
            let mut patterns = Vec::new();
            for (i, row) in chunk.iter().enumerate() {
                debug_assert_eq!(row.len(), cols.len());
                let mut assigns = Vec::new();
                for (j, col) in cols.iter().enumerate() {
                    let name = format!("p{i}_{j}");
                    assigns.push(format!("{col}: ${name}"));
                    owned.push((name, row[j].clone()));
                }
                patterns.push(format!("(:{table} {{{}}})", assigns.join(", ")));
            }
            let mut stmt = conn
                .prepare(&format!("CREATE {}", patterns.join(", ")))
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            conn.execute(
                &mut stmt,
                owned.iter().map(|(k, v)| (k.as_str(), v.clone())).collect(),
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        }
        Ok(())
    }

    /// Bulk relationship insert between known node ids via `UNWIND` over parallel id
    /// lists (Ladybug list extraction is 1-based). Returns relationships created.
    fn batch_insert_rels(
        conn: &Connection<'_>,
        rel: &str,
        from_table: &str,
        to_table: &str,
        pairs: &[(String, String)],
    ) -> Result<usize> {
        const BATCH: usize = 500;
        let mut n = 0;
        for chunk in pairs.chunks(BATCH) {
            let from: Vec<Value> = chunk
                .iter()
                .map(|(a, _)| Value::String(a.clone()))
                .collect();
            let to: Vec<Value> = chunk
                .iter()
                .map(|(_, b)| Value::String(b.clone()))
                .collect();
            let m = chunk.len();
            let mut stmt = conn
                .prepare(&format!(
                    "UNWIND range(1, {m}) AS i MATCH (a:{from_table}) WHERE a.id = $from[i] \
                 MATCH (b:{to_table}) WHERE b.id = $to[i] CREATE (a)-[:{rel}]->(b)"
                ))
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            conn.execute(
                &mut stmt,
                vec![
                    ("from", Value::List(LogicalType::String, from)),
                    ("to", Value::List(LogicalType::String, to)),
                ],
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
            n += chunk.len();
        }
        Ok(n)
    }
    fn write_copy_csv(header: &[&str], rows: &[Vec<String>]) -> Result<PathBuf> {
        fn esc(s: &str) -> String {
            let mut o = String::with_capacity(s.len() + 2);
            o.push('"');
            for c in s.chars() {
                if c == '"' {
                    o.push('"');
                }
                o.push(c);
            }
            o.push('"');
            o
        }
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ontobug-copy-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut text = String::new();
        text.push_str(&header.iter().map(|h| esc(h)).collect::<Vec<_>>().join(","));
        text.push('\n');
        for r in rows {
            text.push_str(&r.iter().map(|c| esc(c)).collect::<Vec<_>>().join(","));
            text.push('\n');
        }
        let path = dir.join("copy.csv");
        std::fs::write(&path, text).map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(path)
    }

    /// One native `COPY` of CSV rows into a table (skipped when empty). Quoted
    /// newlines need a single-threaded read.
    fn copy_csv(
        conn: &Connection<'_>,
        table: &str,
        header: &[&str],
        rows: &[Vec<String>],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let path = Self::write_copy_csv(header, rows)?;
        let r = conn.query(&format!(
            "COPY {table} FROM '{}' (HEADER true, PARALLEL=FALSE)",
            path.display()
        ));
        let _ = std::fs::remove_dir_all(path.parent().expect("file has a parent"));
        r.map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(())
    }

    /// A term as an `RdfNode` row: id (the N-Triples token), kind, bare value,
    /// datatype/lang (literals) and namespace.
    fn node_row(term: &oxrdf::Term) -> (String, Vec<String>) {
        match term {
            oxrdf::Term::NamedNode(n) => {
                let iri = n.as_str().to_owned();
                (
                    format!("<{iri}>"),
                    vec![
                        format!("<{iri}>"),
                        "iri".to_owned(),
                        iri.clone(),
                        String::new(),
                        String::new(),
                        namespace_of(&iri),
                    ],
                )
            }
            oxrdf::Term::BlankNode(b) => {
                let label = b.as_str().to_owned();
                (
                    format!("_:{label}"),
                    vec![
                        format!("_:{label}"),
                        "blank".to_owned(),
                        label,
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                )
            }
            oxrdf::Term::Literal(l) => {
                let id = l.to_string();
                (
                    id,
                    vec![
                        l.to_string(),
                        "literal".to_owned(),
                        l.value().to_owned(),
                        l.datatype().as_str().to_owned(),
                        l.language().unwrap_or_default().to_owned(),
                        String::new(),
                    ],
                )
            }
            oxrdf::Term::Triple(t) => {
                let id = t.to_string();
                (
                    id,
                    vec![
                        t.to_string(),
                        "triple".to_owned(),
                        t.to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                )
            }
        }
    }

    /// Ingests triples as nodes and edges: distinct terms become `RdfNode` rows
    /// (deduplicated against the store), one `Statement` rel per distinct triple
    /// (deduplicated within the batch and against the affected graphs). Both go in
    /// with one native `COPY` per table. Returns statements inserted.
    pub fn ingest_quads(&self, items: &[(String, oxrdf::Triple)]) -> Result<usize> {
        // Distinct triples first (RDF is a set).
        let mut seen = BTreeSet::new();
        let mut triples: Vec<(String, oxrdf::Triple)> = Vec::new();
        for (graph, t) in items {
            let key = (
                graph.clone(),
                t.subject.to_string(),
                t.predicate.to_string(),
                t.object.to_string(),
            );
            if seen.insert(key) {
                triples.push((graph.clone(), t.clone()));
            }
        }
        if triples.is_empty() {
            return Ok(0);
        }
        // Nodes, deduplicated against the store.
        let mut have: BTreeSet<String> = self
            .cypher("MATCH (n:RdfNode) RETURN n.id")?
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .collect();
        let mut node_rows: Vec<Vec<String>> = Vec::new();
        // (from-id, to-id, predicate, graph, namespace) per statement, in order.
        let mut rel_keys: Vec<(String, String, String, String, String)> = Vec::new();
        for (graph, t) in &triples {
            let (sid, srow) = Self::node_row(&t.subject.clone().into());
            let (oid, orow) = Self::node_row(&t.object);
            if have.insert(sid.clone()) {
                node_rows.push(srow);
            }
            if have.insert(oid.clone()) {
                node_rows.push(orow);
            }
            let pred = t.predicate.as_str().to_owned();
            let ns = match &t.subject {
                oxrdf::NamedOrBlankNode::NamedNode(n) => namespace_of(n.as_str()),
                _ => String::new(),
            };
            rel_keys.push((sid, oid, pred, graph.clone(), ns));
        }
        // Ordinals continue per graph after existing statements.
        let graphs: BTreeSet<String> = rel_keys.iter().map(|(_, _, _, g, _)| g.clone()).collect();
        let mut next: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        for g in &graphs {
            let max: i64 = self
                .cypher_params(
                    "MATCH ()-[r:Statement]->() WHERE r.graph = $graph RETURN max(r.ord)",
                    vec![("graph", Value::String(g.clone()))],
                )?
                .first()
                .and_then(|r| r.first())
                .and_then(|v| v.parse().ok())
                .unwrap_or(-1);
            next.insert(g.clone(), max + 1);
        }
        // Existing statements of the affected graphs, to keep set semantics.
        let mut have_rels = BTreeSet::new();
        for g in &graphs {
            for row in self.cypher_params(
                "MATCH (a:RdfNode)-[r:Statement]->(b:RdfNode) WHERE r.graph = $graph \
                 RETURN a.id, r.predicate, b.id",
                vec![("graph", Value::String(g.clone()))],
            )? {
                if row.len() == 3 {
                    have_rels.insert((row[0].clone(), row[1].clone(), row[2].clone()));
                }
            }
        }
        let conn = self.connection()?;
        Self::copy_csv(
            &conn,
            "RdfNode",
            &["id", "kind", "value", "datatype", "lang", "namespace"],
            &node_rows,
        )?;
        let mut rel_rows: Vec<Vec<String>> = Vec::new();
        for (from, to, pred, graph, ns) in rel_keys {
            if !have_rels.insert((from.clone(), pred.clone(), to.clone())) {
                continue;
            }
            let ord = next.entry(graph.clone()).or_insert(0);
            rel_rows.push(vec![from, to, pred, graph, ns, ord.to_string()]);
            *ord += 1;
        }
        Self::copy_csv(
            &conn,
            "Statement",
            &["FROM", "TO", "predicate", "graph", "namespace", "ord"],
            &rel_rows,
        )?;
        Ok(rel_rows.len())
    }

    /// Deletes every statement of a graph (by graph token, see [`graph_token`]), then
    /// the nodes orphaned by it (terms used only by that graph).
    pub fn delete_graph_triples(&self, graph: &str) -> Result<()> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("MATCH ()-[r:Statement]->() WHERE r.graph = $graph DELETE r")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        conn.execute(&mut stmt, vec![("graph", Value::String(graph.to_owned()))])
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        conn.query("MATCH (n:RdfNode) WHERE NOT (n)--() DELETE n")
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(())
    }

    /// A row of a statement join: subject and object ids (N-Triples tokens), the bare
    /// predicate IRI, the object node's value (the literal lexicon, if literal) and
    /// the statement namespace.
    fn statement_rows(&self, where_graph: Option<&str>) -> Result<Vec<Vec<String>>> {
        let (filter, params) = match where_graph {
            Some(g) => (
                "WHERE r.graph = $graph ".to_owned(),
                vec![("graph", Value::String(g.to_owned()))],
            ),
            None => (String::new(), Vec::new()),
        };
        let order = match where_graph {
            Some(_) => "ORDER BY r.ord",
            None => "ORDER BY r.graph, r.ord",
        };
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(&format!(
                "MATCH (a:RdfNode)-[r:Statement]->(b:RdfNode) {filter}RETURN a.id, r.predicate, b.id, b.value, r.namespace, r.graph {order}"
            ))
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let result = conn
            .execute(&mut stmt, params)
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        Ok(result
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect())
    }

    /// The triples of a graph in document order.
    pub fn read_triples(&self, graph: &str) -> Result<Vec<TripleRow>> {
        let mut out = Vec::new();
        for row in self.statement_rows(Some(graph))? {
            if row.len() < 6 {
                continue;
            }
            out.push(TripleRow {
                graph: graph.to_owned(),
                subject: row[0].clone(),
                predicate: format!("<{}>", row[1]),
                object: row[2].clone(),
                lex: row[3].clone(),
                namespace: row[4].clone(),
            });
        }
        Ok(out)
    }

    /// Every triple, ordered by graph then document order.
    pub fn read_all_triples_ordered(&self) -> Result<Vec<TripleRow>> {
        let mut out = Vec::new();
        for row in self.statement_rows(None)? {
            if row.len() < 6 {
                continue;
            }
            out.push(TripleRow {
                subject: row[0].clone(),
                predicate: format!("<{}>", row[1]),
                object: row[2].clone(),
                lex: row[3].clone(),
                namespace: row[4].clone(),
                graph: row[5].clone(),
            });
        }
        Ok(out)
    }

    /// The number of statements, overall or for one graph token.
    pub fn count_triples(&self, graph: Option<&str>) -> Result<usize> {
        let rows = match graph {
            Some(g) => self.cypher_params(
                "MATCH ()-[r:Statement]->() WHERE r.graph = $graph RETURN count(r)",
                vec![("graph", Value::String(g.to_owned()))],
            )?,
            None => self.cypher("MATCH ()-[r:Statement]->() RETURN count(r)")?,
        };
        Ok(rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_migrates_pre_namespace_tables() {
        let store = LadybugSchemaStore::open_in_memory().unwrap();
        // Downgrade to the legacy schema: drop rels, then nodes, and recreate the
        // node tables without the `namespace` column; then fill them the old way.
        let conn = store.connection().unwrap();
        for rel in [
            "SUBCLASS_OF",
            "EQUIVALENT_CLASS",
            "DOMAIN",
            "RANGE",
            "HAS_CONSTRAINT",
            "TARGETS",
            "ON_PROPERTY",
        ] {
            conn.query(&format!("DROP TABLE {rel}")).unwrap();
        }
        for table in [
            "OntologyClass",
            "OntologyProperty",
            "ShaclShape",
            "ShaclProperty",
        ] {
            conn.query(&format!("DROP TABLE {table}")).unwrap();
        }
        for (table, cols) in [
            (
                "OntologyClass",
                vec![
                    "id STRING PRIMARY KEY",
                    "iri STRING",
                    "label STRING",
                    "comment STRING",
                    "graph STRING",
                ],
            ),
            (
                "OntologyProperty",
                vec![
                    "id STRING PRIMARY KEY",
                    "iri STRING",
                    "kind STRING",
                    "label STRING",
                    "comment STRING",
                    "rangeDatatypes STRING",
                    "graph STRING",
                ],
            ),
        ] {
            conn.query(&format!("CREATE NODE TABLE {table}({})", cols.join(", ")))
                .unwrap();
        }
        store
            .cypher("CREATE (:OntologyClass {id: 'g\u{1f}http://ex.org/C1', iri: 'http://ex.org/C1', label: '', comment: '', graph: 'g'})")
            .unwrap();
        store
            .cypher("CREATE (:OntologyClass {id: 'g\u{1f}http://ex.org/C2', iri: 'http://ex.org/C2', label: '', comment: '', graph: 'g'})")
            .unwrap();
        store
            .cypher("CREATE (:OntologyProperty {id: 'g\u{1f}http://ex.org/p', iri: 'http://ex.org/p', kind: 'object', label: '', comment: '', rangeDatatypes: '[]', graph: 'g'})")
            .unwrap();
        // Reopening (ensure) must ALTER the tables, backfill namespaces, and refresh
        // the inventory — without touching existing rows.
        store.ensure_table().unwrap();
        assert_eq!(
            store
                .cypher("MATCH (c:OntologyClass) RETURN c.iri, c.namespace ORDER BY c.iri")
                .unwrap(),
            vec![
                vec!["http://ex.org/C1".to_owned(), "http://ex.org/".to_owned()],
                vec!["http://ex.org/C2".to_owned(), "http://ex.org/".to_owned()],
            ]
        );
        assert_eq!(
            store
                .cypher("MATCH (p:OntologyProperty) RETURN p.namespace")
                .unwrap(),
            vec![vec!["http://ex.org/".to_owned()]]
        );
        let ns = store.list_namespaces().unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].iri, "http://ex.org/");
        assert_eq!(ns[0].prefix, "ex");
        assert_eq!(ns[0].terms, 3);
        // Second run is a no-op.
        store.ensure_table().unwrap();
        assert_eq!(store.list_namespaces().unwrap().len(), 1);
    }

    #[test]
    fn backfill_repairs_nodes_without_a_namespace() {
        let store = LadybugSchemaStore::open_in_memory().unwrap();
        // As written by projections predating namespaces: no namespace property.
        store
            .cypher(
                "CREATE (:OntologyClass {id: 'g\u{1f}http://ex.org/C', iri: 'http://ex.org/C', \
                 label: '', comment: '', graph: 'g'})",
            )
            .unwrap();
        assert_eq!(store.refresh_namespaces().unwrap(), 1);
        let ns = store.list_namespaces().unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].iri, "http://ex.org/");
        assert_eq!(ns[0].prefix, "ex");
        assert_eq!(ns[0].terms, 1);
    }

    fn entry(graph: &str, role: SchemaRole) -> SchemaGraph {
        let mut e = SchemaGraph::new(GraphName::NamedNode(NamedNode::new(graph).unwrap()), role);
        e.applies_to = vec![GraphName::NamedNode(
            NamedNode::new("https://ex.org/data").unwrap(),
        )];
        e.version = Some("v1".into());
        e.sha256 = Some("a".repeat(64));
        e.imports = vec![NamedNode::new("https://ex.org/base").unwrap()];
        e.loaded_at = Some("2026-01-01T00:00:00Z".into());
        e
    }

    #[test]
    fn triples_live_as_nodes_and_edges() {
        use oxrdf::{Literal, NamedNode};
        let store = LadybugSchemaStore::open_in_memory().unwrap();
        let g = "<https://ex.org/g>".to_owned();
        let alice = oxrdf::Triple {
            subject: NamedNode::new("https://example.org/people/alice")
                .unwrap()
                .into(),
            predicate: NamedNode::new("http://schema.org/email").unwrap(),
            object: Literal::new_simple_literal("a\"l\nice@ex.example").into(),
        };
        let n = store
            .ingest_quads(&[(g.clone(), alice.clone()), (g.clone(), alice.clone())])
            .unwrap();
        // The duplicate triple ingests once: RDF is a set.
        assert_eq!(n, 1);
        assert_eq!(store.count_triples(Some(&g)).unwrap(), 1);
        assert_eq!(store.count_triples(None).unwrap(), 1);
        // Subject and literal object are nodes; the predicate is a rel.
        assert_eq!(
            store
                .cypher("MATCH (n:RdfNode) RETURN n.kind ORDER BY n.kind")
                .unwrap(),
            vec![vec!["iri".to_owned()], vec!["literal".to_owned()]]
        );
        let rows = store.read_triples(&g).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject, "<https://example.org/people/alice>");
        assert_eq!(rows[0].predicate, "<http://schema.org/email>");
        assert_eq!(rows[0].object, "\"a\\\"l\\nice@ex.example\"");
        assert_eq!(rows[0].lex, "a\"l\nice@ex.example");
        assert_eq!(rows[0].namespace, "https://example.org/people/");
        // Appending the same triples again inserts nothing.
        assert_eq!(store.ingest_quads(&[(g.clone(), alice)]).unwrap(), 0);
        // Deleting the graph removes its rels and orphans its nodes.
        store.delete_graph_triples(&g).unwrap();
        assert_eq!(store.count_triples(None).unwrap(), 0);
        assert_eq!(
            store.cypher("MATCH (n:RdfNode) RETURN count(n)").unwrap(),
            vec![vec!["0".to_owned()]]
        );
    }

    #[test]
    fn roundtrip_in_memory() {
        let store = LadybugSchemaStore::open_in_memory().unwrap();
        assert_eq!(
            store.cypher("MATCH (e:SchemaEntry) RETURN e.id").unwrap(),
            Vec::<Vec<String>>::new()
        );
        let mut entries = vec![
            entry("https://ex.org/onto", SchemaRole::Ontology),
            entry("https://ex.org/shapes", SchemaRole::Shacl),
        ];
        // An empty applies-to (every graph) and a second role on one graph both survive.
        entries[1].applies_to.clear();
        entries.push(SchemaGraph::new(
            entries[0].graph.clone(),
            SchemaRole::Shacl,
        ));
        store.save_entries(&entries).unwrap();
        let back = store.load_entries().unwrap();
        assert_eq!(back.len(), 3);
        assert!(back.contains(&entries[0]));
        assert!(back
            .iter()
            .any(|e| e.graph.to_string() == "<https://ex.org/shapes>"
                && e.role == SchemaRole::Shacl
                && e.applies_to.is_empty()));
        // The default graph round-trips through its label.
        let mut d = SchemaGraph::new(GraphName::DefaultGraph, SchemaRole::Ontology);
        d.applies_to = vec![GraphName::DefaultGraph];
        store.save_entries(std::slice::from_ref(&d)).unwrap();
        assert!(store.load_entries().unwrap().contains(&d));
        // Re-saving a graph replaces its roles' nodes rather than duplicating them.
        let n = store.load_entries().unwrap().len();
        store
            .save_entries(std::slice::from_ref(&entries[0]))
            .unwrap();
        assert_eq!(store.load_entries().unwrap().len(), n);
        // Removal drops every role of the graph.
        store.remove_graph(&entries[0].graph).unwrap();
        let back = store.load_entries().unwrap();
        assert!(!back.iter().any(|e| e.graph == entries[0].graph));
        store.clear().unwrap();
        assert!(store.load_entries().unwrap().is_empty());
    }
}
