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
use crate::registry::{SchemaGraph, SchemaRole};
use lbug::{Connection, Database, SystemConfig, Value};
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
        let ddl = CREATE_TABLE.to_string();
        let if_not_exists = "CREATE NODE TABLE IF NOT EXISTS SchemaEntry(\
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
        if conn.query(if_not_exists).is_err() {
            match conn.query(&ddl) {
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
