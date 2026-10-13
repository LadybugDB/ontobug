//! # Ontolith
//!
//! Vocabulary and ontology management for RDF datasets. Ontologies (RDFS/OWL), SHACL shapes
//! and ShEx schemas live in named graphs next to the data; a registry graph, itself plain RDF,
//! says which graph holds which kind of schema and which data graphs each one governs. Data
//! graphs are validated against the shapes mapped to them, with the ontologies in scope.
//!
//! ```no_run
//! use ontolith::{AddOptions, Catalog, SchemaRole, ValidateOptions, parse_graph};
//! use oxigraph::io::RdfFormat;
//!
//! let catalog = Catalog::open("dataset.trig")?;
//! catalog.install_system_graphs()?;
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
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```
//!
//! The registry's wire format is oxilite's `oxl:` vocabulary 2.1, so a dataset managed here
//! opens in [oxilite](https://oxilitedb.com) with its registrations intact.

mod catalog;
mod error;
pub mod registry;
pub mod validate;

pub use catalog::{
    format_of, parse_graph, parse_targets, sha256_hex, AddOptions, Catalog, Drift, ValidateOptions,
    Validation,
};
pub use error::{Error, Result};
pub use registry::{SchemaGraph, SchemaRole};
pub use validate::{Finding, Mode, Report, ShexResult};

pub use oxigraph;
