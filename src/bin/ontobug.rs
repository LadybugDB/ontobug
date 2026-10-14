//! `ontobug`: manage the ontologies, shapes and data graphs of a dataset, and validate.
//!
//! RDF content lives in a TriG/N-Quads file; the schema registry lives in a Ladybug
//! database and speaks Cypher.

use clap::{Parser, Subcommand, ValueEnum};
use ontobug::oxigraph::model::{GraphName, NamedNode};
use ontobug::{
    default_registry_path, format_of, parse_graph, parse_targets, AddOptions, Catalog, Drift,
    SchemaGraph, SchemaRole, ValidateOptions, Validation,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "ontobug",
    version,
    about = "Vocabulary and ontology management for RDF datasets"
)]
struct Cli {
    /// The dataset file (TriG `.trig` or N-Quads `.nq`). Created when missing.
    #[arg(short, long, env = "ONTOBUG_DATASET", default_value = "dataset.trig")]
    dataset: PathBuf,
    /// The Ladybug registry database (`.lbdb`). Defaults to the dataset stem, e.g.
    /// `dataset.lbdb` next to `dataset.trig`.
    #[arg(short, long, env = "ONTOBUG_LADYBUG")]
    ladybug: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Role {
    Ontology,
    Shacl,
    Shex,
}

impl From<Role> for SchemaRole {
    fn from(r: Role) -> Self {
        match r {
            Role::Ontology => SchemaRole::Ontology,
            Role::Shacl => SchemaRole::Shacl,
            Role::Shex => SchemaRole::Shex,
        }
    }
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Output {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Load a schema document into a graph and register it.
    Add {
        role: Role,
        /// The graph to hold it: an IRI, or DEFAULT.
        graph: String,
        /// The document (Turtle, N-Triples, RDF/XML…, by extension).
        #[arg(short, long)]
        file: PathBuf,
        /// A data graph it applies to (repeatable); ALL or none: every graph.
        #[arg(short = 't', long = "applies-to")]
        applies_to: Vec<String>,
        /// A version to pin (default: the ontology's owl:versionIRI / owl:versionInfo).
        #[arg(long)]
        version: Option<String>,
        /// An ontology it imports, recorded in the registry (repeatable).
        #[arg(long = "import")]
        imports: Vec<String>,
        /// Register it inactive.
        #[arg(long)]
        inactive: bool,
    },
    /// Load data into a graph.
    Load {
        graph: String,
        #[arg(short, long)]
        file: PathBuf,
        /// Replace the graph's content instead of adding to it.
        #[arg(long)]
        replace: bool,
    },
    /// List the registrations.
    List {
        #[arg(long, value_enum, default_value_t = Output::Text)]
        output: Output,
    },
    /// The schemas that apply to a data graph (ontology imports resolved).
    Scope { graph: String },
    /// Set the data graphs a registration applies to.
    Map {
        graph: String,
        /// Targets (graph IRIs, DEFAULT, or ALL).
        #[arg(long = "to", required = true, num_args = 1..)]
        to: Vec<String>,
    },
    /// Make a registration contribute again.
    Activate { graph: String },
    /// Keep a registration but stop it contributing.
    Deactivate { graph: String },
    /// Remove a registration (the graph's triples stay unless --drop).
    Remove {
        graph: String,
        #[arg(long)]
        drop: bool,
    },
    /// Check the registry for structural problems.
    Check,
    /// Compare a registered graph's recorded SHA-256 with a file.
    Verify {
        graph: String,
        #[arg(short, long)]
        file: PathBuf,
    },
    /// Validate a data graph with SHACL against the shapes mapped to it.
    Validate {
        graph: String,
        /// Shapes graphs to use instead of the registry's (repeatable).
        #[arg(long)]
        shapes: Vec<String>,
        /// Do not add the ontologies in scope to the data graph.
        #[arg(long)]
        no_ontologies: bool,
        #[arg(long, value_enum, default_value_t = Output::Text)]
        output: Output,
    },
    /// Validate nodes of a data graph with ShEx.
    Shex {
        graph: String,
        /// The ShExC schema.
        #[arg(short, long)]
        schema: PathBuf,
        /// The shape map, e.g. '<http://ex.org/alice>@<http://ex.org/PersonShape>'.
        #[arg(short, long)]
        map: String,
        /// Base IRI of the schema.
        #[arg(long, default_value = "http://example.org/")]
        base: String,
    },
    /// Run Cypher against the schema registry, printing rows as TSV.
    Cypher { cypher: String },
    /// Print the dataset as TriG, or one graph as N-Triples.
    Export { graph: Option<String> },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn graph_label(g: &GraphName) -> String {
    match g {
        GraphName::DefaultGraph => "DEFAULT".into(),
        g => g.to_string(),
    }
}

/// A graph for JSON: its IRI, or `DEFAULT`.
fn plain(g: &GraphName) -> String {
    match g {
        GraphName::NamedNode(n) => n.as_str().to_owned(),
        g => graph_label(g),
    }
}

fn labels(gs: &[GraphName]) -> String {
    gs.iter().map(graph_label).collect::<Vec<_>>().join(" ")
}

fn entry_json(e: &SchemaGraph) -> Value {
    json!({
        "graph": plain(&e.graph),
        "role": e.role.name(),
        "active": e.active,
        "appliesTo": e.applies_to.iter().map(plain).collect::<Vec<_>>(),
        "ontologyIri": e.iri.as_ref().map(|i| i.as_str()),
        "version": e.version,
        "sha256": e.sha256,
        "imports": e.imports.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
        "loadedAt": e.loaded_at,
    })
}

fn print_entry(e: &SchemaGraph) {
    let targets = if e.applies_to.is_empty() {
        "ALL".to_owned()
    } else {
        labels(&e.applies_to)
    };
    print!(
        "{:<8} {:<8} {} -> {}",
        e.role.name(),
        if e.active { "active" } else { "inactive" },
        graph_label(&e.graph),
        targets
    );
    if let Some(v) = &e.version {
        print!("  version={v}");
    }
    if let Some(i) = &e.iri {
        print!("  ontology={i}");
    }
    for i in &e.imports {
        print!("  imports={i}");
    }
    println!();
}

fn validation_json(v: &Validation) -> Value {
    json!({
        "dataGraph": plain(&v.data_graph),
        "shapesGraphs": v.shapes_graphs.iter().map(plain).collect::<Vec<_>>(),
        "ontologyGraphs": v.ontology_graphs.iter().map(plain).collect::<Vec<_>>(),
        "conforms": v.report.conforms,
        "results": v.report.results.iter().map(|r| json!({
            "severity": r.severity,
            "focusNode": r.focus,
            "path": r.path,
            "value": r.value,
            "component": r.component,
            "sourceShape": r.source,
            "message": r.message,
        })).collect::<Vec<_>>(),
    })
}

fn run(cli: Cli) -> ontobug::Result<ExitCode> {
    let registry = cli
        .ladybug
        .clone()
        .unwrap_or_else(|| default_registry_path(&cli.dataset));
    let c = Catalog::open(&cli.dataset, &registry)?;
    let save = |c: &Catalog| c.save(&cli.dataset);
    match cli.command {
        Command::Add {
            role,
            graph,
            file,
            applies_to,
            version,
            imports,
            inactive,
        } => {
            let g = parse_graph(&graph)?;
            let options = AddOptions {
                applies_to: parse_targets(&applies_to)?,
                version,
                imports: imports
                    .iter()
                    .map(|i| NamedNode::new(i.trim_matches(['<', '>'])))
                    .collect::<Result<_, _>>()?,
                inactive,
            };
            let doc = std::fs::read(&file)?;
            let e = c.add(role.into(), g.as_ref(), &doc, format_of(&file)?, &options)?;
            save(&c)?;
            println!(
                "registered {} ({} triples)",
                graph_label(&e.graph),
                c.graph_len(e.graph.as_ref())?
            );
            print_entry(&e);
        }
        Command::Load {
            graph,
            file,
            replace,
        } => {
            let g = parse_graph(&graph)?;
            c.load(
                g.as_ref(),
                &std::fs::read(&file)?,
                format_of(&file)?,
                replace,
            )?;
            save(&c)?;
            println!("{}: {} triples", graph_label(&g), c.graph_len(g.as_ref())?);
        }
        Command::List { output } => {
            let entries = c.list()?;
            if output == Output::Json {
                let all: Vec<Value> = entries.iter().map(entry_json).collect();
                println!("{}", serde_json::to_string_pretty(&all).expect("json"));
            } else if entries.is_empty() {
                println!("nothing registered");
            } else {
                entries.iter().for_each(print_entry);
            }
        }
        Command::Scope { graph } => {
            let g = parse_graph(&graph)?;
            for role in SchemaRole::ALL {
                for e in c.schemas_for(g.as_ref(), role)? {
                    println!("{:<8} {}", role.name(), graph_label(&e.graph));
                }
            }
        }
        Command::Map { graph, to } => {
            let g = parse_graph(&graph)?;
            c.map(g.as_ref(), &parse_targets(&to)?)?;
            save(&c)?;
            println!("mapped {}", graph_label(&g));
        }
        Command::Activate { graph } => {
            let g = parse_graph(&graph)?;
            c.set_active(g.as_ref(), true)?;
            save(&c)?;
            println!("activated {}", graph_label(&g));
        }
        Command::Deactivate { graph } => {
            let g = parse_graph(&graph)?;
            c.set_active(g.as_ref(), false)?;
            save(&c)?;
            println!("deactivated {}", graph_label(&g));
        }
        Command::Remove { graph, drop } => {
            let g = parse_graph(&graph)?;
            if drop {
                c.drop_graph(g.as_ref())?;
            } else {
                c.unregister(g.as_ref())?;
            }
            save(&c)?;
            let what = if drop { "dropped" } else { "unregistered" };
            println!("{what} {}", graph_label(&g));
        }
        Command::Check => {
            let problems = c.problems()?;
            for p in &problems {
                println!("{p}");
            }
            if !problems.is_empty() {
                return Ok(ExitCode::from(1));
            }
            println!("registry ok ({} registrations)", c.list()?.len());
        }
        Command::Verify { graph, file } => {
            let g = parse_graph(&graph)?;
            match c.verify(g.as_ref(), &std::fs::read(&file)?)? {
                Drift::Same => println!("unchanged"),
                Drift::Changed { recorded, actual } => {
                    println!("changed: recorded {recorded}, file {actual}");
                    return Ok(ExitCode::from(1));
                }
                Drift::Unrecorded { actual } => {
                    println!("no digest recorded; file {actual}");
                    return Ok(ExitCode::from(1));
                }
            }
        }
        Command::Validate {
            graph,
            shapes,
            no_ontologies,
            output,
        } => {
            let g = parse_graph(&graph)?;
            let options = ValidateOptions {
                shapes: shapes
                    .iter()
                    .map(|s| parse_graph(s))
                    .collect::<Result<_, _>>()?,
                with_ontologies: !no_ontologies,
                ..Default::default()
            };
            let v = c.validate(g.as_ref(), &options)?;
            if output == Output::Json {
                let j = validation_json(&v);
                println!("{}", serde_json::to_string_pretty(&j).expect("json"));
            } else {
                println!("data:       {}", graph_label(&v.data_graph));
                println!("shapes:     {}", labels(&v.shapes_graphs));
                if !v.ontology_graphs.is_empty() {
                    println!("ontologies: {}", labels(&v.ontology_graphs));
                }
                print!("{}", v.report);
            }
            if !v.report.conforms {
                return Ok(ExitCode::from(1));
            }
        }
        Command::Shex {
            graph,
            schema,
            map,
            base,
        } => {
            let g = parse_graph(&graph)?;
            let shexc = std::fs::read_to_string(&schema)?;
            let results = c.validate_shex(g.as_ref(), &shexc, &base, &map)?;
            let mut ok = true;
            for r in &results {
                ok &= r.conformant;
                if r.conformant {
                    println!("ok   {}@{}", r.node, r.shape);
                } else {
                    println!("FAIL {}@{}\n    {}", r.node, r.shape, r.reason);
                }
            }
            if !ok {
                return Ok(ExitCode::from(1));
            }
        }
        Command::Cypher { cypher } => {
            for row in c.cypher(&cypher)? {
                println!("{}", row.join("\t"));
            }
        }
        Command::Export { graph } => match graph {
            Some(g) => print!("{}", c.graph_ntriples(parse_graph(&g)?.as_ref())?),
            None => {
                let tmp =
                    std::env::temp_dir().join(format!("ontobug-{}.trig", std::process::id()));
                c.save(&tmp)?;
                print!("{}", std::fs::read_to_string(&tmp)?);
                std::fs::remove_file(&tmp)?;
            }
        },
    }
    Ok(ExitCode::SUCCESS)
}
