# Ontobug

**Vocabulary and ontology management for RDF datasets.** Keep OWL ontologies, SHACL shapes and ShEx schemas in named graphs next to your data. Record in Ladybug which schema governs which graph, and query that registry with Cypher. Validate any graph against exactly the shapes and ontologies that apply to it.

*onto* + *ladybug*: your schemas, set in stone and kept with the data they describe — in a bug built for graphs.

```sh
ontobug add ontology https://example.org/onto/hr     --file hr.ttl        -t https://example.org/data/staff
ontobug add shacl    https://example.org/shapes/hr   --file hr-shapes.ttl -t https://example.org/data/staff
ontobug load         https://example.org/data/staff  --file staff.ttl
ontobug validate     https://example.org/data/staff        # exit 0 conforms, 1 violations
ontobug cypher "MATCH (e:SchemaEntry) RETURN e.graph, e.role"
```

## Why

Most stores keep the link between schema and data in configuration that queries cannot see and a dump loses. Ontobug keeps it in a Ladybug registry, one `SchemaEntry` node per registration:

```cypher
MATCH (e:SchemaEntry) RETURN e.graph, e.role, e.appliesTo, e.version, e.active;

// https://example.org/onto/hr   ontology  [https://example.org/data/staff]  https://example.org/hr/2.1.0  true
// https://example.org/shapes/hr shacl      [https://example.org/data/staff]                          true
```

- **Per-graph scoping.** Each data graph gets its own ontologies and shapes, so conflicting schemas can live side by side.
- **Imports resolved.** `owl:imports` between registered ontologies is followed transitively, and cycles are safe.
- **Ontology-aware SHACL.** The ontologies in scope join the data graph, so `sh:targetClass core:Person` also checks every `hr:Manager`.
- **Checked registry.** `ontobug check` validates the registrations structurally (no duplicate roles, well-formed digests).
- **Drift detection.** `ontobug verify` compares a file with the SHA-256 recorded when it was loaded.
- **Cypher, not SPARQL.** Ladybug is the database: every triple lives there losslessly as a `DataTriple` row, alongside the registry and the projected schemas, all through parameterised Cypher (`lbug` crate). TriG/N-Quads files are snapshots for exchange.
- **Schemas as graphs, not just metadata.** Beyond the registry, the schema content itself is projected into Ladybug: ontology classes and properties with `SUBCLASS_OF`, `EQUIVALENT_CLASS`, `DOMAIN` and `RANGE` edges, SHACL shapes with `HAS_CONSTRAINT` edges linked back via `TARGETS` and `ON_PROPERTY`. Walk a class hierarchy or a shape-to-property path in Cypher.
- **Namespaces as subgraphs.** Every term is partitioned by its RDF namespace into a queryable subgraph: `Namespace` nodes inventory prefix, IRI, term counts and source graphs (prefixes harvested from `@prefix` declarations, derived otherwise). Real schema.org projects to 50 namespaces out of the box.

## Install

```sh
cargo install --path .
```

## Commands

| Command | Does |
|---|---|
| `add ROLE GRAPH --file F [-t G]… [--version V] [--import IRI]… [--inactive]` | Load a schema file into a graph and register it; `ROLE` is `ontology`, `shacl` or `shex` |
| `load GRAPH --file F [--replace]` | Load data |
| `list [--output json]` | List the registrations |
| `scope GRAPH` | Show the schemas that apply to a data graph, imports included |
| `map GRAPH --to G…` | Change the targets (`ALL`, `DEFAULT` or graph IRIs) |
| `activate GRAPH`, `deactivate GRAPH` | Switch a registration on or off |
| `remove GRAPH [--drop]` | Unregister it, and with `--drop` delete its triples too |
| `check` | Check the registry for structural problems |
| `verify GRAPH --file F` | Detect drift from the recorded SHA-256 |
| `validate GRAPH [--shapes G]… [--no-ontologies] [--output json]` | Validate with SHACL |
| `shex GRAPH --schema F --map M` | Validate a shape map with ShEx |
| `cypher CYPHER` | Run Cypher against the schema registry, rows as TSV |
| `namespaces` | List the namespaces in the projected content: prefix, IRI, terms, graphs |
| `project GRAPH` | (Re)build a graph's Ladybug nodes and edges from its RDF content (`add` already does this for ontology and shapes graphs) |
| `export [GRAPH]` | Print the dataset as TriG, or one graph as N-Triples |

The dataset file is set with `-d FILE` or `ONTOBUG_DATASET` (default `dataset.trig`; TriG or N-Quads). The Ladybug database is set with `-l FILE` or `ONTOBUG_LADYBUG` (default: the dataset stem with `.lbdb`, e.g. `dataset.lbdb` next to `dataset.trig`). The `.lbdb` is live — it holds the triples, the registry and the projections; the file is a snapshot written by save/export and read only to seed an empty database.

## Documentation

**[docs/guide.md](docs/guide.md)** covers the registry format, adding OWL ontologies and SHACL shapes with full RDF samples, imports, mapping, validation (including why ontologies take part), ShEx, registry checks, versioning and drift, Cypher recipes, other SHACL processors, and the Rust API.

The runnable example is in [examples/hr](examples/hr).

## Library

```rust
use ontobug::{Catalog, ValidateOptions, parse_graph};

let c = Catalog::open("hr.trig", "hr.lbdb")?;
let v = c.validate(parse_graph("https://example.org/data/staff")?.as_ref(), &ValidateOptions::default())?;
println!("{}", v.report);

// The registry speaks Cypher.
let rows = c.cypher("MATCH (e:SchemaEntry) RETURN e.graph, e.role")?;
```

Use `default-features = false` to leave out the CLI dependencies.

## Origin

Ontobug began as the schema registry of [oxilite](https://github.com/Volland/oxilite) (SPARQL on SQLite). The registry has since moved to [Ladybug](https://ladybugdb.com): registrations are `SchemaEntry` nodes queried with Cypher, and the oxilite compatibility layer is gone.

SHACL and ShEx validation is done by [rudof](https://rudof-project.github.io/), RDF parsing by [Oxigraph](https://github.com/oxigraph/oxigraph), and storage, registry and projections by [lbug](https://crates.io/crates/lbug).

## License

MIT or Apache-2.0, at your option.
