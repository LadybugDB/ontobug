//! Schema content in Ladybug as nodes and edges: ontology classes, properties and
//! their relationships, plus SHACL shapes linked back into the ontology.

use ontobug::oxigraph::io::RdfFormat;
use ontobug::oxigraph::model::{GraphName, NamedNode};
use ontobug::{AddOptions, Catalog, SchemaRole};

const CORE: &str = include_str!("../examples/hr/core.ttl");
const HR: &str = include_str!("../examples/hr/hr.ttl");
const SHAPES: &str = include_str!("../examples/hr/hr-shapes.ttl");

fn g(iri: &str) -> GraphName {
    NamedNode::new(iri).unwrap().into()
}

fn one(c: &Catalog, cypher: &str) -> String {
    c.cypher(cypher)
        .unwrap()
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .unwrap_or_default()
}

/// The HR example, with content projected (core first so its classes exist before hr's
/// edges resolve to them).
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

#[test]
fn ontology_becomes_nodes_and_edges() {
    let c = catalog();
    // 3 core classes + 3 hr classes. hr resolves to core's nodes: no per-graph
    // duplicates.
    assert_eq!(one(&c, "MATCH (c:OntologyClass) RETURN count(c)"), "6");
    // External references become labelled-empty stubs that keep edges connected.
    c.add(
        SchemaRole::Ontology,
        g("https://ex.org/onto/extra").as_ref(),
        b"@prefix ex: <https://ex.org/extra#> . \
         @prefix owl: <http://www.w3.org/2002/07/owl#> . \
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> . \
         ex:Worker a owl:Class ; \
             rdfs:label \"Worker\" ; \
             owl:equivalentClass <https://ex.org/outside#Thing> .",
        RdfFormat::Turtle,
        &AddOptions::default(),
    )
    .unwrap();
    assert_eq!(one(&c, "MATCH (c:OntologyClass) RETURN count(c)"), "8");
    let stub = c
        .cypher(
            "MATCH (c:OntologyClass) WHERE c.iri = 'https://ex.org/outside#Thing' \
             RETURN c.graph, c.label",
        )
        .unwrap();
    assert_eq!(
        stub,
        vec![vec!["https://ex.org/onto/extra".to_owned(), String::new()]]
    );
    assert_eq!(
        one(&c, "MATCH ()-[r:EQUIVALENT_CLASS]->() RETURN count(r)"),
        "1"
    );
    // Person -> Agent, Organization -> Agent, Employee -> Person, Manager -> Employee,
    // Contractor -> Person.
    assert_eq!(one(&c, "MATCH ()-[r:SUBCLASS_OF]->() RETURN count(r)"), "5");
    // hr:Manager's ancestry walks across graphs through shared class IRIs.
    let supers = c
        .cypher(
            "MATCH (m:OntologyClass)-[:SUBCLASS_OF*1..5]->(s:OntologyClass) \
             WHERE m.iri = 'https://example.org/hr#Manager' RETURN s.iri ORDER BY s.iri",
        )
        .unwrap()
        .into_iter()
        .map(|r| r.into_iter().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        supers,
        [
            "https://example.org/core#Agent",
            "https://example.org/core#Person",
            "https://example.org/hr#Employee",
        ]
    );
    // worksFor is an object property with its domain recorded.
    assert_eq!(
        one(
            &c,
            "MATCH (p:OntologyProperty) WHERE p.iri = 'https://example.org/hr#worksFor' \
             RETURN p.kind"
        ),
        "object"
    );
    assert_eq!(
        one(
            &c,
            "MATCH (:OntologyProperty {iri: 'https://example.org/hr#worksFor'})-[:DOMAIN]->(c) \
             RETURN c.iri"
        ),
        "https://example.org/hr#Employee"
    );
}

#[test]
fn shapes_link_back_into_the_ontology() {
    let c = catalog();
    assert_eq!(one(&c, "MATCH (s:ShaclShape) RETURN count(s)"), "3");
    assert!(
        one(&c, "MATCH ()-[r:HAS_CONSTRAINT]->() RETURN count(r)")
            .parse::<usize>()
            .unwrap()
            >= 6
    );
    // Every shape targets a projected class…
    assert_eq!(one(&c, "MATCH ()-[r:TARGETS]->() RETURN count(r)"), "3");
    // …and every property path resolves to a projected property.
    let dangling = one(
        &c,
        "MATCH (p:ShaclProperty) WHERE NOT (p)-[:ON_PROPERTY]->() RETURN count(p)",
    );
    assert_eq!(dangling, "0", "every constraint path links to a property");
    // A constraint carries its components as properties.
    let row = c
        .cypher(
            "MATCH (p:ShaclProperty) WHERE p.path = 'https://example.org/hr#salary' \
             RETURN p.datatype, p.minInclusive ORDER BY p.id",
        )
        .unwrap();
    assert!(
        row.iter().any(|r| r
            == &vec![
                "http://www.w3.org/2001/XMLSchema#decimal".to_owned(),
                "0".to_owned()
            ]),
        "{row:?}"
    );
}

#[test]
fn projection_is_idempotent_and_drops_with_the_graph() {
    let c = catalog();
    let before = c
        .project_graph_content(g("https://ex.org/onto/hr").as_ref())
        .unwrap();
    let again = c
        .project_graph_content(g("https://ex.org/onto/hr").as_ref())
        .unwrap();
    assert_eq!(before, again);
    assert_eq!(
        one(
            &c,
            "MATCH (c:OntologyClass) WHERE c.graph = 'https://ex.org/onto/hr' RETURN count(c)"
        ),
        "3"
    );
    c.drop_graph(g("https://ex.org/onto/hr").as_ref()).unwrap();
    assert_eq!(
        one(
            &c,
            "MATCH (c:OntologyClass) WHERE c.graph = 'https://ex.org/onto/hr' RETURN count(c)"
        ),
        "0"
    );
    // The shapes' links into the dropped graph are gone with its nodes.
    assert_eq!(one(&c, "MATCH ()-[r:TARGETS]->() RETURN count(r)"), "1");
}

#[test]
fn projecting_an_unregistered_graph_fails() {
    let c = Catalog::new().unwrap();
    assert!(c
        .project_graph_content(g("https://ex.org/onto/hr").as_ref())
        .is_err());
}
