//! Namespaces as Ladybug subgraphs: term IRIs partitioned by namespace, inventoried
//! as `Namespace` nodes with harvested or derived prefixes.

use ontobug::oxigraph::io::RdfFormat;
use ontobug::oxigraph::model::{GraphName, NamedNode};
use ontobug::{AddOptions, Catalog, SchemaRole};

const CORE: &str = include_str!("../examples/hr/core.ttl");
const HR: &str = include_str!("../examples/hr/hr.ttl");
const SHAPES: &str = include_str!("../examples/hr/hr-shapes.ttl");

fn g(iri: &str) -> GraphName {
    NamedNode::new(iri).unwrap().into()
}

fn catalog() -> Catalog {
    let c = Catalog::new().unwrap();
    let staff = vec![g("https://ex.org/data/staff")];
    for (role, graph, doc) in [
        (SchemaRole::Ontology, "https://ex.org/onto/core", CORE),
        (SchemaRole::Ontology, "https://ex.org/onto/hr", HR),
        (SchemaRole::Shacl, "https://ex.org/shapes/hr", SHAPES),
    ] {
        c.add(
            role,
            g(graph).as_ref(),
            doc.as_bytes(),
            RdfFormat::Turtle,
            &AddOptions {
                applies_to: staff.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    }
    c
}

fn prefixes(c: &Catalog) -> Vec<(String, String, usize)> {
    let mut out: Vec<(String, String, usize)> = c
        .namespaces()
        .unwrap()
        .into_iter()
        .map(|n| (n.prefix, n.iri, n.terms))
        .collect();
    out.sort();
    out
}

#[test]
fn namespaces_partition_the_content() {
    let c = catalog();
    // core#, hr# and hr/shapes# (the shapes' own IRIs); xsd: terms live in property
    // columns, not nodes, so no xsd namespace. The shapes namespace takes its prefix
    // from the document's own `@prefix hrsh:` declaration.
    assert_eq!(
        prefixes(&c),
        [
            ("core".to_owned(), "https://example.org/core#".to_owned(), 5),
            ("hr".to_owned(), "https://example.org/hr#".to_owned(), 14),
            (
                "hrsh".to_owned(),
                "https://example.org/hr/shapes#".to_owned(),
                3
            ),
        ]
    );
    // Every content node of a namespace is its subgraph: scoped counts agree.
    let scoped: usize = c
        .cypher(
            "MATCH (c:OntologyClass) WHERE c.namespace = 'https://example.org/hr#' RETURN count(c)",
        )
        .unwrap()[0][0]
        .parse()
        .unwrap();
    assert_eq!(scoped, 3);
}

#[test]
fn harvested_prefixes_win_over_derived_ones() {
    let c = Catalog::new().unwrap();
    c.add(
        SchemaRole::Ontology,
        g("https://ex.org/onto/extra").as_ref(),
        b"@prefix ex: <https://ex.org/extra#> .\n\
          @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
          ex:Worker a owl:Class .",
        RdfFormat::Turtle,
        &AddOptions::default(),
    )
    .unwrap();
    assert!(prefixes(&c).contains(&("ex".to_owned(), "https://ex.org/extra#".to_owned(), 1)));
}

#[test]
fn dropping_a_graph_shrinks_its_namespaces() {
    let c = catalog();
    c.drop_graph(g("https://ex.org/onto/hr").as_ref()).unwrap();
    let iris: Vec<String> = c.namespaces().unwrap().into_iter().map(|n| n.iri).collect();
    // hr# terms remain (shapes reference them), but the hr ontology graph is gone.
    let hr = c
        .namespaces()
        .unwrap()
        .into_iter()
        .find(|n| n.iri == "https://example.org/hr#")
        .unwrap();
    assert!(
        !hr.graphs.contains(&"https://ex.org/onto/hr".to_owned()),
        "{hr:?}"
    );
    assert!(iris.contains(&"https://example.org/core#".to_owned()));
}
