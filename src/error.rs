/// Errors of the catalog, the registry and validation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Other(String),
    #[error("invalid IRI: {0}")]
    Iri(#[from] oxrdf::IriParseError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("RDF syntax: {0}")]
    Load(#[from] oxigraph::store::LoaderError),
    #[error("RDF serialization: {0}")]
    Serialize(#[from] oxigraph::store::SerializerError),
    #[error("storage: {0}")]
    Storage(#[from] oxigraph::store::StorageError),
    #[error("SPARQL query: {0}")]
    Query(#[from] oxigraph::sparql::QueryEvaluationError),
    #[error("SPARQL update: {0}")]
    Update(#[from] oxigraph::sparql::UpdateEvaluationError),
    #[error("SPARQL syntax: {0}")]
    Syntax(#[from] oxigraph::sparql::SparqlSyntaxError),
    #[error("invalid shapes: {0}")]
    Shapes(String),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("{0} is not registered")]
    NotRegistered(String),
    #[error("no active {role} graph applies to {graph}; register one or name it explicitly")]
    NoSchema { role: &'static str, graph: String },
}

pub type Result<T> = std::result::Result<T, Error>;
