# Ontolith

**Vocabulary and ontology management for RDF datasets.** Keep OWL ontologies, SHACL shapes and ShEx schemas in named graphs next to your data. Record in plain RDF which schema governs which graph. Validate any graph against exactly the shapes and ontologies that apply to it.

*onto* + *lithos*, stone: your schemas, set in stone and kept with the data they describe.

```sh
ontolith add ontology https://example.org/onto/hr     --file hr.ttl        -t https://example.org/data/staff
ontolith add shacl    https://example.org/shapes/hr   --file hr-shapes.ttl -t https://example.org/data/staff
ontolith load         https://example.org/data/staff  --file staff.ttl
ontolith validate     https://example.org/data/staff        # exit 0 conforms, 1 violations
```

## Why

Most stores keep the link between schema and data in configuration that SPARQL cannot see and a dump loses. Ontolith keeps it as RDF in a registry graph, `<oxilite:schema>`:

```trig
<oxilite:schema> {
  <https://example.org/onto/hr> a oxl:OntologyGraph ;
      oxl:appliesTo <https://example.org/data/staff> ;
      oxl:ontologyIri <https://example.org/hr#> ;
      oxl:version "https://example.org/hr/2.1.0" ;
      oxl:sha256 "1aae19e3…" ;
      oxl:active true .
  <https://example.org/shapes/hr> a oxl:ShapesGraph ;
      oxl:appliesTo <https://example.org/data/staff> ; oxl:active true .
}
```

- **Per-graph scoping.** Each data graph gets its own ontologies and shapes, so conflicting schemas can live side by side.
- **Imports resolved.** `owl:imports` between registered ontologies is followed transitively, and cycles are safe.
- **Ontology-aware SHACL.** The ontologies in scope join the data graph, so `sh:targetClass core:Person` also checks every `hr:Manager`.
- **Self-validating registry.** The registry ships its own SHACL shapes, and `ontolith check` runs them.
- **Drift detection.** `ontolith verify` compares a file with the SHA-256 recorded when it was loaded.
- **Portable.** Every operation is plain SPARQL 1.1. It runs unchanged on Oxigraph, [oxilite](https://oxilitedb.com) or any SPARQL store, and the format is oxilite's `oxl:` vocabulary 2.1.

## Install

```sh
cargo install --path .
```

## Commands

| Command | Does |
|---|---|
| `init` | Install the registry and vocabulary system graphs |
| `add ROLE GRAPH --file F [-t G]… [--version V] [--import IRI]… [--inactive]` | Load a schema file into a graph and register it; `ROLE` is `ontology`, `shacl` or `shex` |
| `load GRAPH --file F [--replace]` | Load data |
| `list [--output json]` | List the registrations |
| `scope GRAPH` | Show the schemas that apply to a data graph, imports included |
| `map GRAPH --to G…` | Change the targets (`ALL`, `DEFAULT` or graph IRIs) |
| `activate GRAPH`, `deactivate GRAPH` | Switch a registration on or off |
| `remove GRAPH [--drop]` | Unregister it, and with `--drop` delete its triples too |
| `check` | Validate the registry against its own shapes |
| `verify GRAPH --file F` | Detect drift from the recorded SHA-256 |
| `validate GRAPH [--shapes G]… [--no-ontologies] [--sparql] [--output json]` | Validate with SHACL |
| `shex GRAPH --schema F --map M` | Validate a shape map with ShEx |
| `query SPARQL`, `update SPARQL`, `export [GRAPH]` | Plain SPARQL access, and export |

The dataset file is set with `-d FILE` or `ONTOLITH_DATASET` (default `dataset.trig`; TriG or N-Quads).

## Documentation

**[docs/guide.md](docs/guide.md)** covers the registry format, adding OWL ontologies and SHACL shapes with full RDF samples, imports, mapping, validation (including why ontologies take part), ShEx, registry self-checks, versioning and drift, SPARQL recipes for any store, other SHACL processors, and the Rust API.

The runnable example is in [examples/hr](examples/hr).

## Library

```rust
use ontolith::{Catalog, ValidateOptions, parse_graph};

let c = Catalog::open("hr.trig")?;
let v = c.validate(parse_graph("https://example.org/data/staff")?.as_ref(), &ValidateOptions::default())?;
println!("{}", v.report);
```

Use `default-features = false` to leave out the CLI dependencies.

## Origin

Ontolith is the schema registry of [oxilite](https://github.com/Volland/oxilite) (SPARQL on SQLite), extracted into a store-independent tool. Graph and namespace IRIs are unchanged, so datasets move between the two in both directions.

SHACL and ShEx validation is done by [rudof](https://rudof-project.github.io/), and storage and SPARQL by [Oxigraph](https://github.com/oxigraph/oxigraph).

## License

MIT or Apache-2.0, at your option.
