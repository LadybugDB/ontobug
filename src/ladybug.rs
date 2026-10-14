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
use oxrdf::{GraphName, NamedNode};
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
/// a property graph (see [`LadybugSchemaStore::project_ontology`]).
const CONTENT_TABLES: [&str; 11] = [
    "CREATE NODE TABLE OntologyClass(\
        id STRING PRIMARY KEY, iri STRING, label STRING, comment STRING, graph STRING)",
    "CREATE NODE TABLE OntologyProperty(\
        id STRING PRIMARY KEY, iri STRING, kind STRING, label STRING, comment STRING, \
        rangeDatatypes STRING, graph STRING)",
    "CREATE NODE TABLE ShaclShape(\
        id STRING PRIMARY KEY, shape STRING, targetClasses STRING, targetNodes STRING, \
        targetSubjectsOf STRING, targetObjectsOf STRING, message STRING, severity STRING, \
        deactivated BOOLEAN, graph STRING)",
    "CREATE NODE TABLE ShaclProperty(\
        id STRING PRIMARY KEY, shapeId STRING, path STRING, datatype STRING, class STRING, \
        nodeKind STRING, minCount INT64, maxCount INT64, pattern STRING, \
        minInclusive STRING, maxInclusive STRING, message STRING, severity STRING, \
        deactivated BOOLEAN, graph STRING)",
    "CREATE REL TABLE SUBCLASS_OF(FROM OntologyClass TO OntologyClass)",
    "CREATE REL TABLE EQUIVALENT_CLASS(FROM OntologyClass TO OntologyClass)",
    "CREATE REL TABLE DOMAIN(FROM OntologyProperty TO OntologyClass)",
    "CREATE REL TABLE RANGE(FROM OntologyProperty TO OntologyClass)",
    "CREATE REL TABLE HAS_CONSTRAINT(FROM ShaclShape TO ShaclProperty)",
    "CREATE REL TABLE TARGETS(FROM ShaclShape TO OntologyClass)",
    "CREATE REL TABLE ON_PROPERTY(FROM ShaclProperty TO OntologyProperty)",
];

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
        let mut mk_class = conn
            .prepare(
                "CREATE (:OntologyClass {id: $id, iri: $iri, label: $label, comment: $comment, graph: $graph})",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut mk_prop = conn
            .prepare(
                "CREATE (:OntologyProperty {id: $id, iri: $iri, kind: $kind, label: $label, \
                 comment: $comment, rangeDatatypes: $rangeDatatypes, graph: $graph})",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut class_nodes = 0;
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
                conn.execute(
                    &mut mk_class,
                    vec![
                        ("id", Value::String(id)),
                        ("iri", Value::String(iri)),
                        ("label", Value::String(label)),
                        ("comment", Value::String(comment)),
                        ("graph", Value::String(glabel.clone())),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
                class_nodes += 1;
            }
        }
        let mut prop_nodes = 0;
        for (iri, (kind, label, comment)) in &props {
            let id = content_id(&glabel, iri);
            if have_props.insert(id.clone()) {
                let dts = datatypes.get(iri).cloned().unwrap_or_default();
                conn.execute(
                    &mut mk_prop,
                    vec![
                        ("id", Value::String(id)),
                        ("iri", Value::String(iri.clone())),
                        ("kind", Value::String(kind.clone())),
                        ("label", Value::String(label.clone())),
                        ("comment", Value::String(comment.clone())),
                        (
                            "rangeDatatypes",
                            Value::String(serde_json::to_string(&dts)?),
                        ),
                        ("graph", Value::String(glabel.clone())),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
                prop_nodes += 1;
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
                conn.execute(
                    &mut mk_prop,
                    vec![
                        ("id", Value::String(id)),
                        ("iri", Value::String(iri)),
                        ("kind", Value::String(String::new())),
                        ("label", Value::String(String::new())),
                        ("comment", Value::String(String::new())),
                        (
                            "rangeDatatypes",
                            Value::String(serde_json::to_string(&dts)?),
                        ),
                        ("graph", Value::String(glabel.clone())),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
                prop_nodes += 1;
            }
        }
        // Edges resolve to this graph's nodes first, then to any graph's node with the
        // same IRI, so projections that share classes stay connected.
        let make_edges = |table: &str,
                          from_table: &str,
                          to_table: &str,
                          pairs: &[(String, String)],
                          from_pool: &std::collections::BTreeSet<String>,
                          to_pool: &std::collections::BTreeSet<String>|
         -> Result<usize> {
            let mut stmt = conn
                .prepare(&format!(
                    "MATCH (a:{from_table}) WHERE a.id = $from MATCH (b:{to_table}) WHERE b.id = $to \
                     CREATE (a)-[:{table}]->(b)"
                ))
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            let mut n = 0;
            for (a, b) in pairs {
                let (Some(from), Some(to)) = (
                    resolve_node(from_pool, &glabel, a),
                    resolve_node(to_pool, &glabel, b),
                ) else {
                    continue;
                };
                conn.execute(
                    &mut stmt,
                    vec![("from", Value::String(from)), ("to", Value::String(to))],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
                n += 1;
            }
            Ok(n)
        };
        let mut edges = 0;
        edges += make_edges(
            "SUBCLASS_OF",
            "OntologyClass",
            "OntologyClass",
            &subclass,
            &have_classes,
            &have_classes,
        )?;
        edges += make_edges(
            "EQUIVALENT_CLASS",
            "OntologyClass",
            "OntologyClass",
            &equiv,
            &have_classes,
            &have_classes,
        )?;
        edges += make_edges(
            "DOMAIN",
            "OntologyProperty",
            "OntologyClass",
            &domain,
            &have_props,
            &have_classes,
        )?;
        edges += make_edges(
            "RANGE",
            "OntologyProperty",
            "OntologyClass",
            &range,
            &have_props,
            &have_classes,
        )?;
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
        let mut mk_shape = conn
            .prepare(
                "CREATE (:ShaclShape {id: $id, shape: $shape, targetClasses: $targetClasses, \
                 targetNodes: $targetNodes, targetSubjectsOf: $targetSubjectsOf, \
                 targetObjectsOf: $targetObjectsOf, message: $message, severity: $severity, \
                 deactivated: $deactivated, graph: $graph})",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut mk_prop = conn
            .prepare(
                "CREATE (:ShaclProperty {id: $id, shapeId: $shapeId, path: $path, datatype: $datatype, \
                 class: $class, nodeKind: $nodeKind, minCount: $minCount, maxCount: $maxCount, \
                 pattern: $pattern, minInclusive: $minInclusive, maxInclusive: $maxInclusive, \
                 message: $message, severity: $severity, deactivated: $deactivated, graph: $graph})",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
        let mut mk_has = conn
            .prepare(
                "MATCH (s:ShaclShape) WHERE s.id = $shape MATCH (p:ShaclProperty) WHERE p.id = $prop \
                 CREATE (s)-[:HAS_CONSTRAINT]->(p)",
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
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
            conn.execute(
                &mut mk_shape,
                vec![
                    ("id", Value::String(sid.clone())),
                    ("shape", Value::String(key.clone())),
                    (
                        "targetClasses",
                        Value::String(serde_json::to_string(&target_classes)?),
                    ),
                    (
                        "targetNodes",
                        Value::String(serde_json::to_string(
                            &objs(&key, &sh("targetNode"))
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>(),
                        )?),
                    ),
                    (
                        "targetSubjectsOf",
                        Value::String(serde_json::to_string(&strings(objs(
                            &key,
                            &sh("targetSubjectsOf"),
                        )))?),
                    ),
                    (
                        "targetObjectsOf",
                        Value::String(serde_json::to_string(&strings(objs(
                            &key,
                            &sh("targetObjectsOf"),
                        )))?),
                    ),
                    (
                        "message",
                        Value::String(first_literal(objs(&key, &sh("message")))),
                    ),
                    (
                        "severity",
                        Value::String(first_iri(objs(&key, &sh("severity")))),
                    ),
                    (
                        "deactivated",
                        Value::Bool(is_true(objs(&key, &sh("deactivated")))),
                    ),
                    ("graph", Value::String(glabel.clone())),
                ],
            )
            .map_err(|e| Error::Ladybug(e.to_string()))?;
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
                conn.execute(
                    &mut mk_prop,
                    vec![
                        ("id", Value::String(cid.clone())),
                        ("shapeId", Value::String(sid.clone())),
                        ("path", Value::String(path)),
                        (
                            "datatype",
                            Value::String(first_iri(objs(pkey, &sh("datatype")))),
                        ),
                        ("class", Value::String(first_iri(objs(pkey, &sh("class"))))),
                        (
                            "nodeKind",
                            Value::String(first_iri(objs(pkey, &sh("nodeKind")))),
                        ),
                        ("minCount", opt_int(first_int(objs(pkey, &sh("minCount"))))),
                        ("maxCount", opt_int(first_int(objs(pkey, &sh("maxCount"))))),
                        (
                            "pattern",
                            Value::String(first_literal(objs(pkey, &sh("pattern")))),
                        ),
                        (
                            "minInclusive",
                            Value::String(first_literal(objs(pkey, &sh("minInclusive")))),
                        ),
                        (
                            "maxInclusive",
                            Value::String(first_literal(objs(pkey, &sh("maxInclusive")))),
                        ),
                        (
                            "message",
                            Value::String(first_literal(objs(pkey, &sh("message")))),
                        ),
                        (
                            "severity",
                            Value::String(first_iri(objs(pkey, &sh("severity")))),
                        ),
                        (
                            "deactivated",
                            Value::Bool(is_true(objs(pkey, &sh("deactivated")))),
                        ),
                        ("graph", Value::String(glabel.clone())),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
                conn.execute(
                    &mut mk_has,
                    vec![
                        ("shape", Value::String(sid.clone())),
                        ("prop", Value::String(cid)),
                    ],
                )
                .map_err(|e| Error::Ladybug(e.to_string()))?;
            }
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
