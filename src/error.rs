/// Errors of the catalog, the registry and validation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Other(String),
    #[error("invalid IRI: {0}")]
    Iri(#[from] oxrdf::IriParseError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid shapes: {0}")]
    Shapes(String),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("{0} is not registered")]
    NotRegistered(String),
    #[error("no active {role} graph applies to {graph}; register one or name it explicitly")]
    NoSchema { role: &'static str, graph: String },
    #[error("ladybug: {0}")]
    Ladybug(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
