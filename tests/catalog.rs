//! End-to-end tests over the HR example in `examples/hr`.
//!
//! RDF content lives in memory (or TriG); the schema registry lives in Ladybug and speaks
//! Cypher.

use ontobug::oxigraph::io::RdfFormat;
use ontobug::oxigraph::model::{GraphName, NamedNode};
use ontobug::{AddOptions, Catalog, Drift, SchemaGraph, SchemaRole, ValidateOptions};

const CORE: &str = include_str!("../examples/hr/core.ttl");
const HR: &str = include_str!("../examples/hr/hr.ttl");
const SHAPES: &str = include_str!("../examples/hr/hr-shapes.ttl");
const STAFF: &str = include_str!("../examples/hr/staff.ttl");
const BROKEN: &str = include_str!("../examples/hr/staff-broken.ttl");
const SHEX: &str = include_str!("../examples/hr/person.shex");

fn g(iri: &str) -> GraphName {
    NamedNode::new(iri).unwrap().into()
}

/// The HR example: core (mapped elsewhere, imported by hr), hr and its shapes mapped to the two
/// staff graphs.
fn catalog() -> Catalog {
    let c = Catalog::new().unwrap();
    let staff = vec![
        g("https://ex.org/data/staff"),
        g("https://ex.org/data/broken"),
    ];
    let add = |role, graph: &str, doc: &str, applies_to: Vec<GraphName>| {
        let options = AddOptions {
            applies_to,
            ..Default::default()
        };
        c.add(
            role,
            g(graph).as_ref(),
            doc.as_bytes(),
            RdfFormat::Turtle,
            &options,
        )
        .unwrap()
    };
    add(
        SchemaRole::Ontology,
        "https://ex.org/onto/core",
        CORE,
        vec![g("https://ex.org/data/other")],
    );
    add(
        SchemaRole::Ontology,
        "https://ex.org/onto/hr",
        HR,
        staff.clone(),
    );
    add(SchemaRole::Shacl, "https://ex.org/shapes/hr", SHAPES, staff);
    for (graph, doc) in [
        ("https://ex.org/data/staff", STAFF),
        ("https://ex.org/data/broken", BROKEN),
    ] {
        c.load(g(graph).as_ref(), doc.as_bytes(), RdfFormat::Turtle, true)
            .unwrap();
    }
    c
}

#[test]
fn add_records_ontology_header_and_digest() {
    let c = catalog();
    let list = c.list().unwrap();
    assert_eq!(list.len(), 3);
    let hr = list
        .iter()
        .find(|e| e.graph == g("https://ex.org/onto/hr"))
        .unwrap();
    assert_eq!(hr.iri.as_ref().unwrap().as_str(), "https://example.org/hr#");
    assert_eq!(hr.version.as_deref(), Some("https://example.org/hr/2.1.0"));
    assert_eq!(
        hr.sha256.as_deref(),
        Some(ontobug::sha256_hex(HR.as_bytes()).as_str())
    );
    assert!(c.problems().unwrap().is_empty());
}

#[test]
fn registry_speaks_cypher() {
    let c = catalog();
    let rows = c
        .cypher("MATCH (e:SchemaEntry) WHERE e.role = 'ontology' RETURN e.graph ORDER BY e.graph")
        .unwrap();
    let graphs: Vec<_> = rows
        .into_iter()
        .map(|r| r.into_iter().next().unwrap())
        .collect();
    assert_eq!(
        graphs,
        [
            "https://ex.org/onto/core".to_owned(),
            "https://ex.org/onto/hr".to_owned()
        ]
    );
}

#[test]
fn imports_bring_ontologies_into_scope() {
    let c = catalog();
    let staff = g("https://ex.org/data/staff");
    let scope = |c: &Catalog| -> Vec<String> {
        c.schemas_for(staff.as_ref(), SchemaRole::Ontology)
            .unwrap()
            .into_iter()
            .map(|e| e.graph.to_string())
            .collect()
    };
    assert_eq!(
        scope(&c),
        ["<https://ex.org/onto/hr>", "<https://ex.org/onto/core>"]
    );
    // An inactive ontology is never imported.
    c.set_active(g("https://ex.org/onto/core").as_ref(), false)
        .unwrap();
    assert_eq!(scope(&c), ["<https://ex.org/onto/hr>"]);
}

#[test]
fn validation_follows_the_ontologies_in_scope() {
    let c = catalog();
    let opts = ValidateOptions::default();
    let good = c
        .validate(g("https://ex.org/data/staff").as_ref(), &opts)
        .unwrap();
    assert!(good.report.conforms, "{}", good.report);

    let bad = c
        .validate(g("https://ex.org/data/broken").as_ref(), &opts)
        .unwrap();
    assert!(!bad.report.conforms);
    let carol: Vec<_> = bad
        .report
        .results
        .iter()
        .filter(|r| r.focus.contains("carol"))
        .map(|r| r.component.rsplit('#').next().unwrap().to_owned())
        .collect();
    // Carol is only typed hr:Manager: EmployeeShape and PersonShape reach her through
    // rdfs:subClassOf.
    assert!(
        carol.iter().any(|c| c == "PatternConstraintComponent"),
        "{carol:?}"
    );
    assert!(
        carol.iter().any(|c| c == "MinCountConstraintComponent"),
        "{carol:?}"
    );
    assert_eq!(bad.report.violations(), 10, "{}", bad.report);

    let without = c
        .validate(
            g("https://ex.org/data/broken").as_ref(),
            &ValidateOptions {
                with_ontologies: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(without.report.violations() < bad.report.violations());
}

#[test]
fn unmapped_graphs_need_explicit_shapes() {
    let c = catalog();
    let other = g("https://ex.org/data/broken-elsewhere");
    assert!(c
        .validate(other.as_ref(), &ValidateOptions::default())
        .is_err());
    let v = c
        .validate(
            other.as_ref(),
            &ValidateOptions {
                shapes: vec![g("https://ex.org/shapes/hr")],
                ..Default::default()
            },
        )
        .unwrap();
    assert!(v.report.conforms, "an empty graph conforms");
}

#[test]
fn mapping_and_removal() {
    let c = catalog();
    let shapes = g("https://ex.org/shapes/hr");
    c.map(shapes.as_ref(), &[g("https://ex.org/data/staff")])
        .unwrap();
    let broken = g("https://ex.org/data/broken");
    assert!(c
        .schemas_for(broken.as_ref(), SchemaRole::Shacl)
        .unwrap()
        .is_empty());
    c.map(shapes.as_ref(), &[]).unwrap();
    assert_eq!(
        c.schemas_for(broken.as_ref(), SchemaRole::Shacl)
            .unwrap()
            .len(),
        1
    );
    c.unregister(shapes.as_ref()).unwrap();
    assert!(
        c.graph_len(shapes.as_ref()).unwrap() > 0,
        "unregistering keeps the triples"
    );
    assert!(c.map(shapes.as_ref(), &[]).is_err());
    c.drop_graph(g("https://ex.org/onto/hr").as_ref()).unwrap();
    assert_eq!(
        c.graph_len(g("https://ex.org/onto/hr").as_ref()).unwrap(),
        0
    );
    assert_eq!(c.list().unwrap().len(), 1);
}

#[test]
fn drift_is_detected() {
    let c = catalog();
    let hr = g("https://ex.org/onto/hr");
    assert_eq!(c.verify(hr.as_ref(), HR.as_bytes()).unwrap(), Drift::Same);
    assert!(matches!(
        c.verify(hr.as_ref(), CORE.as_bytes()).unwrap(),
        Drift::Changed { .. }
    ));
}

#[test]
fn hand_written_registrations_are_checked() {
    let c = catalog();
    // A bad digest recorded out of band is reported; the registry has no SHACL engine
    // anymore, so the structural check is the check.
    let mut bad = SchemaGraph::new(g("https://ex.org/onto/hr"), SchemaRole::Ontology);
    bad.sha256 = Some("ABC".into());
    c.register(&bad).unwrap();
    let problems = c.problems().unwrap();
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("not 64 lowercase hex"));
}

#[test]
fn dataset_round_trips_through_trig_and_lbdb() {
    let dir = std::env::temp_dir().join(format!("ontobug-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("ds.trig");
    let registry = dir.join("ds.lbdb");
    // Build the HR example on disk-backed paths, so both halves are durable.
    let c = Catalog::open(&data, &registry).unwrap();
    let staff = vec![
        g("https://ex.org/data/staff"),
        g("https://ex.org/data/broken"),
    ];
    for (role, graph, doc, targets) in [
        (
            SchemaRole::Ontology,
            "https://ex.org/onto/hr",
            HR,
            staff.clone(),
        ),
        (SchemaRole::Shacl, "https://ex.org/shapes/hr", SHAPES, staff),
    ] {
        c.add(
            role,
            g(graph).as_ref(),
            doc.as_bytes(),
            RdfFormat::Turtle,
            &AddOptions {
                applies_to: targets,
                ..Default::default()
            },
        )
        .unwrap();
    }
    c.load(
        g("https://ex.org/data/staff").as_ref(),
        STAFF.as_bytes(),
        RdfFormat::Turtle,
        true,
    )
    .unwrap();
    c.save(&data).unwrap();
    // Content round-trips through TriG; the registry round-trips through Ladybug.
    let back = Catalog::open(&data, &registry).unwrap();
    assert_eq!(back.list().unwrap(), c.list().unwrap());
    assert_eq!(
        back.cypher("MATCH ()-[r:Statement]->() RETURN count(r)")
            .unwrap(),
        c.cypher("MATCH ()-[r:Statement]->() RETURN count(r)")
            .unwrap()
    );
    // N-Triples dumps are unordered, so compare sorted lines.
    let sorted = |s: String| {
        let mut lines: Vec<_> = s.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    assert_eq!(
        sorted(
            back.graph_ntriples(g("https://ex.org/data/staff").as_ref())
                .unwrap()
        ),
        sorted(
            c.graph_ntriples(g("https://ex.org/data/staff").as_ref())
                .unwrap()
        )
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn shex_validates_a_shape_map() {
    let c = catalog();
    let r = c
        .validate_shex(
            g("https://ex.org/data/staff").as_ref(),
            SHEX,
            "https://example.org/hr/shex#",
            "<https://example.org/people/bob>@<https://example.org/hr/shex#Employee>,\
             <https://example.org/people/acme>@<https://example.org/hr/shex#Employee>",
        )
        .unwrap();
    let ok: Vec<bool> = r.iter().map(|r| r.conformant).collect();
    assert_eq!(ok, [false, true], "{r:#?}");
}
