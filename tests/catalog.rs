//! End-to-end tests over the HR example in `examples/hr`.

use ontolith::oxigraph::io::RdfFormat;
use ontolith::oxigraph::model::{GraphName, NamedNode};
use ontolith::{AddOptions, Catalog, Drift, SchemaRole, ValidateOptions};

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
    assert!(c.install_system_graphs().unwrap());
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
        Some(ontolith::sha256_hex(HR.as_bytes()).as_str())
    );
    assert!(c.problems().unwrap().is_empty());
    assert!(c.check_registry().unwrap().conforms);
    assert!(!c.install_system_graphs().unwrap(), "already current");
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
fn hand_written_registry_problems_agree_with_shacl() {
    let c = catalog();
    c.update(
        "PREFIX oxl: <https://oxilite.dev/ns#>
         INSERT DATA { GRAPH <oxilite:schema> {
           <https://ex.org/onto/hr> oxl:active \"no\" ; oxl:sha256 \"ABC\" .
           <https://ex.org/x> oxl:appliesTo oxl:AllGraphs } }",
    )
    .unwrap();
    let problems = c.problems().unwrap();
    let report = c.check_registry().unwrap();
    assert_eq!(problems.len(), 5, "{problems:#?}");
    assert_eq!(report.violations(), problems.len(), "{report}");
}

#[test]
fn dataset_round_trips_through_trig() {
    let c = catalog();
    let dir = std::env::temp_dir().join(format!("ontolith-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ds.trig");
    c.save(&path).unwrap();
    let back = Catalog::open(&path).unwrap();
    assert_eq!(back.list().unwrap(), c.list().unwrap());
    assert_eq!(back.store().len().unwrap(), c.store().len().unwrap());
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
