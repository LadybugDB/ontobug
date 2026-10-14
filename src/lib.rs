//! # Ontobug
//!
//! Vocabulary and ontology management for RDF datasets. Ontologies (RDFS/OWL), SHACL shapes
//! and ShEx schemas live in named graphs next to the data; a Ladybug registry says which
//! graph holds which kind of schema and which data graphs each one governs. Data graphs
//! are validated against the shapes mapped to them, with the ontologies in scope.
//!
//! ```no_run
//! use ontobug::{AddOptions, Catalog, SchemaRole, ValidateOptions, parse_graph};
//! use oxigraph::io::RdfFormat;
//!
//! let catalog = Catalog::open("dataset.trig", "dataset.lbdb")?;
//! let data = parse_graph("https://example.org/data/staff")?;
//! catalog.add(
//!     SchemaRole::Ontology,
//!     parse_graph("https://example.org/onto/hr")?.as_ref(),
//!     &std::fs::read("hr.ttl")?,
//!     RdfFormat::Turtle,
//!     &AddOptions { applies_to: vec![data.clone()], ..Default::default() },
//! )?;
//! let v = catalog.validate(data.as_ref(), &ValidateOptions::default())?;
//! println!("{}", v.report);
//! catalog.save("dataset.trig")?;
//! // The registry needs no saving: Ladybug persists it as it changes.
//! // Query it with Cypher:
//! let rows = catalog.cypher("MATCH (e:SchemaEntry) RETURN e.graph, e.role")?;
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```
//!
//! Ladybug is the database: every triple lives there losslessly, alongside the
//! registry and the projected schemas, all speaking Cypher. TriG/N-Quads files are
//! snapshots — [`Catalog::save`] writes one, [`Catalog::open`] seeds an empty registry
//! from one. There is no SPARQL surface and no oxilite compatibility layer.

mod catalog;
mod error;
pub mod ladybug;
pub mod registry;
pub mod validate;

pub use catalog::{
    default_registry_path, format_of, parse_graph, parse_targets, sha256_hex, AddOptions, Catalog,
    Drift, ValidateOptions, Validation,
};
pub use error::{Error, Result};
pub use ladybug::{LadybugSchemaStore, NamespaceInfo, Projected, TripleRow};
pub use registry::{SchemaGraph, SchemaRole};
pub use validate::{Finding, Mode, Report, ShexResult};

pub use oxigraph;
