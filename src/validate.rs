//! SHACL and ShEx validation with [rudof](https://rudof-project.github.io/).
//!
//! The engines run unchanged over rudof's in-memory graph. [`Report`] flattens rudof's
//! validation report into plain strings so callers need not depend on rudof's types.

use crate::error::{Error, Result};
use rudof_iri::IriS;
use rudof_rdf::rdf_core::RDFFormat;
use rudof_rdf::rdf_impl::{OxigraphInMemory, ReaderMode};
use shacl::ir::IRSchema;
use shacl::rdf::ShaclParser;
use shacl::validator::engine::{Engine, NativeEngine, SparqlEngine};
use shacl::validator::processor::ShaclProcessor;
use shacl::validator::report::ValidationReport;
use shacl::validator::ShaclConfig;
pub use shacl::validator::ShaclValidationMode as Mode;
use shex_ast::compact::ShapeMapParser;
use shex_ast::ir::map_state::MapState;
use shex_ast::ir::schema_ir::SchemaIR;
use shex_ast::ir::semantic_actions_registry::SemanticActionsRegistry;
use shex_ast::{ResolveMethod, ShExParser};
use shex_validation::{Validator, ValidatorConfig};
use std::fmt;

/// One SHACL validation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// `Violation`, `Warning` or `Info`.
    pub severity: String,
    pub focus: String,
    pub path: Option<String>,
    pub value: Option<String>,
    /// The constraint component, e.g. `sh:MinCountConstraintComponent`.
    pub component: String,
    /// The shape that produced the result.
    pub source: Option<String>,
    pub message: Option<String>,
}

/// The outcome of a SHACL validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub conforms: bool,
    pub results: Vec<Finding>,
}

impl Report {
    /// Results of severity `Violation`.
    pub fn violations(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity == "Violation")
            .count()
    }

    fn from_rudof(r: &ValidationReport) -> Self {
        let mut results: Vec<Finding> = r
            .results()
            .iter()
            .map(|v| {
                let msg = v
                    .message()
                    .iter()
                    .map(|(_, m)| m.clone())
                    .collect::<Vec<_>>();
                Finding {
                    severity: v.severity().to_string(),
                    focus: v.focus_node().to_string(),
                    path: v.path().map(ToString::to_string),
                    value: v.value().map(ToString::to_string),
                    component: v.constraint_component().to_string(),
                    source: v.source().map(ToString::to_string),
                    message: (!msg.is_empty()).then(|| msg.join(" / ")),
                }
            })
            .collect();
        results.sort_by(|a, b| {
            (&a.focus, &a.path, &a.component).cmp(&(&b.focus, &b.path, &b.component))
        });
        Self {
            conforms: r.conforms(),
            results,
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.conforms {
            return writeln!(f, "conforms: yes");
        }
        writeln!(f, "conforms: no ({} results)", self.results.len())?;
        for r in &self.results {
            write!(f, "- {} {}", r.severity, r.focus)?;
            if let Some(p) = &r.path {
                write!(f, " {p}")?;
            }
            if let Some(v) = &r.value {
                write!(f, " = {v}")?;
            }
            writeln!(f)?;
            writeln!(f, "    {}", short(&r.component))?;
            if let Some(m) = &r.message {
                writeln!(f, "    {m}")?;
            }
        }
        Ok(())
    }
}

/// `sh:MinCountConstraintComponent` for the full IRI.
fn short(iri: &str) -> String {
    iri.trim_matches(|c| c == '<' || c == '>')
        .replace("http://www.w3.org/ns/shacl#", "sh:")
}

fn graph(data: &str, format: &RDFFormat) -> Result<OxigraphInMemory> {
    OxigraphInMemory::from_str(data, format, None, &ReaderMode::Strict)
        .map_err(|e| Error::Validation(format!("data graph: {e}")))
}

/// Parses a SHACL shapes graph and compiles it.
pub fn shacl_schema(shapes: &str, format: &RDFFormat) -> Result<IRSchema> {
    let graph = OxigraphInMemory::from_str(shapes, format, None, &ReaderMode::Strict)
        .map_err(|e| Error::Shapes(e.to_string()))?;
    let ast = ShaclParser::new(graph)
        .parse()
        .map_err(|e| Error::Shapes(e.to_string()))?;
    IRSchema::try_from(ast).map_err(|e| Error::Shapes(e.to_string()))
}

struct Processor {
    data: OxigraphInMemory,
}

impl ShaclProcessor<OxigraphInMemory> for Processor {
    fn store(&self) -> &OxigraphInMemory {
        &self.data
    }

    fn runner(mode: &Mode, config: &ShaclConfig) -> Box<dyn Engine<OxigraphInMemory>> {
        match mode {
            Mode::Native => Box::new(NativeEngine::new(config.recursion_semantics())),
            Mode::Sparql => Box::new(SparqlEngine::new(config.recursion_semantics())),
        }
    }
}

/// Validates N-Triples data against N-Triples (or Turtle) shapes.
pub fn shacl(data: &str, shapes: &str, shapes_format: &RDFFormat, mode: &Mode) -> Result<Report> {
    let schema = shacl_schema(shapes, shapes_format)?;
    let report = Processor {
        data: graph(data, &RDFFormat::NTriples)?,
    }
    .validate(&schema, mode, &ShaclConfig::default())
    .map_err(|e| Error::Validation(e.to_string()))?;
    Ok(Report::from_rudof(&report))
}

/// Parses and compiles a ShEx schema (ShExC).
pub fn shex_schema(schema: &str, base: &str) -> Result<SchemaIR> {
    let base = IriS::new(base).map_err(|e| Error::Shapes(e.to_string()))?;
    let ast = ShExParser::parse(schema, Some(base.clone()), &base)
        .map_err(|e| Error::Shapes(e.to_string()))?;
    let config = ValidatorConfig::default();
    let mut map_state = MapState::default();
    let registry = SemanticActionsRegistry::default();
    registry.set_map_state(&mut map_state);
    let mut compiler = shex_ast::ir::ast2ir::AST2IR::new(&ResolveMethod::default(), map_state);
    let mut compiled = SchemaIR::new(registry);
    compiler
        .compile(
            &ast,
            &base,
            &Some(base.clone()),
            &mut compiled,
            config.external_resolvers(),
        )
        .map_err(|e| Error::Shapes(e.to_string()))?;
    Ok(compiled)
}

/// One node/shape pair of a ShEx result shape map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShexResult {
    pub node: String,
    pub shape: String,
    pub conformant: bool,
    pub reason: String,
}

/// Validates N-Triples data against a ShExC schema for the nodes of a shape map
/// (compact syntax, e.g. `<http://ex/a>@<http://ex/S>`).
pub fn shex(data: &str, shexc: &str, base: &str, shape_map: &str) -> Result<Vec<ShexResult>> {
    let schema = shex_schema(shexc, base)?;
    let data = graph(data, &RDFFormat::NTriples)?;
    let nodes_pm = Some(data.prefixmap().clone());
    let shapes_pm = Some(schema.prefixmap());
    let map = ShapeMapParser::parse(shape_map, &nodes_pm, &None, &shapes_pm, &None)
        .map_err(|e| Error::Shapes(e.to_string()))?;
    let validator = Validator::new(&schema, &ValidatorConfig::default())
        .map_err(|e| Error::Validation(e.to_string()))?;
    let result = validator
        .validate_shapemap(&map, &data, &schema, &nodes_pm)
        .map_err(|e| Error::Validation(e.to_string()))?;
    let mut out: Vec<ShexResult> = result
        .iter()
        .map(|(node, shape, status)| ShexResult {
            node: node.to_string(),
            shape: shape.to_string(),
            conformant: status.is_conformant(),
            reason: status.reason(),
        })
        .collect();
    out.sort_by(|a, b| (&a.node, &a.shape).cmp(&(&b.node, &b.shape)));
    Ok(out)
}
