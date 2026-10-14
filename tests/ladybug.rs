//! The Ladybug registry behind a catalog: registrations written through one handle
//! are visible through a catalog opened on the same database, and Cypher sees them too.

use ontobug::oxigraph::io::RdfFormat;
use ontobug::oxigraph::model::{GraphName, NamedNode};
use ontobug::{AddOptions, Catalog, LadybugSchemaStore, SchemaRole};

const HR: &str = include_str!("../examples/hr/hr.ttl");

fn g(iri: &str) -> GraphName {
    NamedNode::new(iri).unwrap().into()
}

#[test]
fn catalog_and_store_share_one_registry() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("ds.trig");
    let registry = dir.path().join("ds.lbdb");

    let c = Catalog::open(&data, &registry).unwrap();
    c.add(
        SchemaRole::Ontology,
        g("https://ex.org/onto/hr").as_ref(),
        HR.as_bytes(),
        RdfFormat::Turtle,
        &AddOptions {
            applies_to: vec![g("https://ex.org/data/staff")],
            ..Default::default()
        },
    )
    .unwrap();

    // The same Ladybug database, opened directly, holds the registration…
    let store = LadybugSchemaStore::open(&registry).unwrap();
    let entries = store.load_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries, c.list().unwrap());

    // …and Cypher reads it back.
    let rows = store
        .cypher("MATCH (e:SchemaEntry) RETURN e.graph, e.role ORDER BY e.graph")
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            "https://ex.org/onto/hr".to_owned(),
            "ontology".to_owned()
        ]]
    );

    // A catalog reopened on both paths sees content and registry alike.
    c.save(&data).unwrap();
    let back = Catalog::open(&data, &registry).unwrap();
    assert_eq!(back.list().unwrap(), c.list().unwrap());
    assert!(back.problems().unwrap().is_empty());
}
