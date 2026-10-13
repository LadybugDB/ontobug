# Ontolith guide

How to keep OWL ontologies, SHACL shapes and ShEx schemas next to your data in one RDF dataset, say which schema governs which data, and validate data graphs against them.

Every example below is runnable: the files are in [`examples/hr`](../examples/hr), and the outputs are the ones Ontolith prints.

- [1. Concepts](#1-concepts)
- [2. Install and first run](#2-install-and-first-run)
- [3. The registry graph](#3-the-registry-graph)
- [4. Adding an OWL ontology](#4-adding-an-owl-ontology)
- [5. Imports between ontologies](#5-imports-between-ontologies)
- [6. Adding SHACL shapes](#6-adding-shacl-shapes)
- [7. Mapping schemas to data graphs](#7-mapping-schemas-to-data-graphs)
- [8. Validating a graph with SHACL](#8-validating-a-graph-with-shacl)
- [9. Validating with ShEx](#9-validating-with-shex)
- [10. Validating the registry itself](#10-validating-the-registry-itself)
- [11. Lifecycle: versions, drift, deactivation, removal](#11-lifecycle-versions-drift-deactivation-removal)
- [12. SPARQL recipes for any store](#12-sparql-recipes-for-any-store)
- [13. Using another SHACL processor](#13-using-another-shacl-processor)
- [14. Rust API](#14-rust-api)
- [15. Vocabulary reference](#15-vocabulary-reference)
- [16. Compatibility and limits](#16-compatibility-and-limits)

---

## 1. Concepts

A **dataset** is a set of named graphs. Ontolith gives each graph a job:

| Kind of graph | Holds | Example |
|---|---|---|
| **Data graph** | Instance data | `<https://example.org/data/staff>` |
| **Ontology graph** | RDFS / OWL axioms: classes, properties, hierarchies | `<https://example.org/onto/hr>` |
| **Shapes graph** | SHACL shapes: constraints on the data | `<https://example.org/shapes/hr>` |
| **ShEx graph** | A ShEx schema | `<https://example.org/shex/hr>` |
| **Registry graph** | One description per schema graph | `<oxilite:schema>` |
| **Vocabulary graph** | The `oxl:` vocabulary and the registry's own shapes | `<oxilite:vocabulary>` |

The **registry** is plain RDF. It records, for each schema graph:

- its **role**: ontology, SHACL or ShEx;
- the data graphs it **applies to**, or all graphs;
- whether it is **active**;
- the ontology IRI, a pinned **version**, the **SHA-256** of the file it came from, **imports**, and when it was loaded.

Because the registry is data, it travels with the dataset. You can query and change it with plain SPARQL, and any SHACL processor can check it.

```
             ┌──────────────── <oxilite:schema> (registry) ─────────────────┐
             │ onto/core  a OntologyGraph ; appliesTo data/other            │
             │ onto/hr    a OntologyGraph ; appliesTo data/staff ;          │
             │            ontologyIri hr# ;  (hr# owl:imports core#)        │
             │ shapes/hr  a ShapesGraph   ; appliesTo data/staff            │
             └──────────────────────────────────────────────────────────────┘
                     │ scope of data/staff = hr + core (import) + shapes/hr
                     ▼
   data/staff ──validate──▶ SHACL engine (rudof) ──▶ report
```

Registering a graph never moves, copies or rewrites its triples. The registry only describes it.

## 2. Install and first run

```sh
git clone <this repo> ontolith && cd ontolith
cargo install --path .          # installs the `ontolith` binary
```

Every command works on one dataset file, given with `--dataset` (`-d`) or `ONTOLITH_DATASET`; it defaults to `dataset.trig`. The file is TriG (`.trig`) or N-Quads (`.nq`), and it is created when missing. Each command that changes something rewrites the file atomically.

The whole HR walkthrough in one go:

```sh
cd examples/hr
export ONTOLITH_DATASET=hr.trig

ontolith init
ontolith add ontology https://example.org/onto/core --file core.ttl \
    --applies-to https://example.org/data/other
ontolith add ontology https://example.org/onto/hr --file hr.ttl \
    -t https://example.org/data/staff -t https://example.org/data/staff-broken
ontolith add shacl https://example.org/shapes/hr --file hr-shapes.ttl \
    -t https://example.org/data/staff -t https://example.org/data/staff-broken
ontolith load https://example.org/data/staff        --file staff.ttl
ontolith load https://example.org/data/staff-broken --file staff-broken.ttl

ontolith list
ontolith validate https://example.org/data/staff          # conforms: yes, exit 0
ontolith validate https://example.org/data/staff-broken   # 10 violations, exit 1
```

Graph arguments take an IRI, or `DEFAULT` for the default graph. In `--applies-to` and `map --to`, `ALL` means every graph.

## 3. The registry graph

After the walkthrough, `ontolith export` prints the registry graph like this (timestamps and digests will differ):

```trig
PREFIX oxl: <https://oxilite.dev/ns#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>

<oxilite:schema> {
  <https://example.org/onto/core> a oxl:OntologyGraph ;
      oxl:active true ;
      oxl:appliesTo <https://example.org/data/other> ;
      oxl:ontologyIri <https://example.org/core#> ;
      oxl:version "1.0.0" ;
      oxl:sha256 "658695ac5776e2c017455598ff9692c34e8c140496362bb3bdc4f10611fd35d8" ;
      oxl:loadedAt "2026-10-13T07:26:42.43349Z"^^xsd:dateTime .

  <https://example.org/onto/hr> a oxl:OntologyGraph ;
      oxl:active true ;
      oxl:appliesTo <https://example.org/data/staff> , <https://example.org/data/staff-broken> ;
      oxl:ontologyIri <https://example.org/hr#> ;
      oxl:version "https://example.org/hr/2.1.0" ;
      oxl:sha256 "1aae19e3014a5c35f13f0759c3b3670b1add379a3e4a0f25ccc1e74d54c08abb" ;
      oxl:loadedAt "2026-10-13T07:26:42.44659Z"^^xsd:dateTime .

  <https://example.org/shapes/hr> a oxl:ShapesGraph ;
      oxl:active true ;
      oxl:appliesTo <https://example.org/data/staff> , <https://example.org/data/staff-broken> ;
      oxl:sha256 "f2a7e4041fd98b6efccf1243054d6cf63920c5229b010dab4fa5516c8d3bad62" ;
      oxl:loadedAt "2026-10-13T07:28:36.750035Z"^^xsd:dateTime .

  # The two system graphs describe themselves.
  <oxilite:schema>     a oxl:SystemGraph ; rdfs:label "schema registry" .
  <oxilite:vocabulary> a oxl:SystemGraph ; rdfs:label "oxilite vocabulary" ;
      oxl:appliesTo <oxilite:schema> ; oxl:ontologyIri oxl: ; oxl:version "2.1" .
}
```

The subject of each description is **the schema graph's own name**. The schema's content stays in that graph:

```trig
<https://example.org/onto/hr> {
  <https://example.org/hr#> a owl:Ontology ; owl:imports <https://example.org/core#> .
  hr:Employee a owl:Class ; rdfs:subClassOf core:Person .
  # …
}
```

**Canonical and short forms.** Ontolith always writes `oxl:active` as an `xsd:boolean` and at least one `oxl:appliesTo` (`oxl:AllGraphs` for "every graph"). It also reads the short forms, so a hand-written registry can leave both out:

```trig
<oxilite:schema> {
  # No oxl:active: active. No oxl:appliesTo: applies to every graph.
  <https://example.org/shapes/common> a oxl:ShapesGraph .
}
```

Only an `xsd:boolean` false deactivates: `false` or `"0"^^xsd:boolean`. A plain string `"false"` is invalid. Readers ignore it, and `ontolith check` reports it.

**One graph, several roles.** An ontology that carries its own SHACL shapes is typed with both classes. It is then listed once per role, and both roles share one description:

```trig
<oxilite:schema> {
  <https://example.org/onto/hr> a oxl:OntologyGraph , oxl:ShapesGraph ;
      oxl:appliesTo oxl:AllGraphs .
}
```

`ontolith add` writes one role and replaces the whole description. To give a graph a second role, add the type with `ontolith update` (see [§12](#12-sparql-recipes-for-any-store)).

## 4. Adding an OWL ontology

### 4.1 Write the ontology

Any RDFS or OWL document works: Turtle, N-Triples, RDF/XML (`.rdf`, `.owl`), N3. Give it an `owl:Ontology` header. Ontolith reads the IRI and version from it. [`examples/hr/core.ttl`](../examples/hr/core.ttl):

```turtle
@prefix core: <https://example.org/core#> .
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

<https://example.org/core#> a owl:Ontology ;
    rdfs:label "Core ontology" ;
    owl:versionInfo "1.0.0" .

core:Agent        a owl:Class ; rdfs:label "Agent" .
core:Person       a owl:Class ; rdfs:subClassOf core:Agent ; rdfs:label "Person" .
core:Organization a owl:Class ; rdfs:subClassOf core:Agent ; rdfs:label "Organization" .
core:Person owl:disjointWith core:Organization .

core:name a owl:DatatypeProperty ;
    rdfs:domain core:Agent ;
    rdfs:range  xsd:string .
```

[`examples/hr/hr.ttl`](../examples/hr/hr.ttl) builds on it and imports it:

```turtle
@prefix hr:   <https://example.org/hr#> .
@prefix core: <https://example.org/core#> .
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

<https://example.org/hr#> a owl:Ontology ;
    owl:versionIRI  <https://example.org/hr/2.1.0> ;
    owl:versionInfo "2.1.0" ;
    owl:imports     <https://example.org/core#> .

hr:Employee   a owl:Class ; rdfs:subClassOf core:Person .
hr:Manager    a owl:Class ; rdfs:subClassOf hr:Employee .
hr:Contractor a owl:Class ; rdfs:subClassOf core:Person ; owl:disjointWith hr:Employee .

hr:worksFor a owl:ObjectProperty , owl:FunctionalProperty ;
    rdfs:domain hr:Employee ; rdfs:range core:Organization .
hr:manages  a owl:ObjectProperty ;
    rdfs:domain hr:Manager ;  rdfs:range hr:Employee ; owl:inverseOf hr:managedBy .
hr:email    a owl:DatatypeProperty ; rdfs:domain core:Person ; rdfs:range xsd:string .
hr:salary   a owl:DatatypeProperty , owl:FunctionalProperty ;
    rdfs:domain hr:Employee ; rdfs:range xsd:decimal .
hr:hireDate a owl:DatatypeProperty , owl:FunctionalProperty ;
    rdfs:domain hr:Employee ; rdfs:range xsd:date .
```

**Keep ontology and data apart.** Put axioms (`rdfs:subClassOf`, domains, ranges) in the ontology graph and instances in data graphs. Then several datasets can share one ontology, and you can update the ontology without touching the data.

### 4.2 Register it

```sh
$ ontolith add ontology https://example.org/onto/hr --file hr.ttl \
      -t https://example.org/data/staff -t https://example.org/data/staff-broken
registered <https://example.org/onto/hr> (41 triples)
ontology active   <https://example.org/onto/hr> -> <https://example.org/data/staff-broken> <https://example.org/data/staff>  version=https://example.org/hr/2.1.0  ontology=<https://example.org/hr#>
```

`add` does four things in one go:

1. **Clears** the graph and loads the file into it.
2. **Reads the header.** If the graph declares exactly one `owl:Ontology` whose IRI differs from the graph name, it records it as `oxl:ontologyIri`. If you give no `--version`, it pins `owl:versionIRI`, or else `owl:versionInfo`.
3. **Records the file's SHA-256** as `oxl:sha256`, for drift detection (see [§11](#11-lifecycle-versions-drift-deactivation-removal)).
4. **Writes the registration**, replacing any earlier description of the graph.

Options:

| Option | Effect |
|---|---|
| `-t, --applies-to G` | A data graph it governs; repeatable. `ALL`, or no option at all, means every graph |
| `--version V` | Pin a version instead of the one read from the header |
| `--import IRI` | Record an import in the registry (`oxl:imports`); repeatable |
| `--inactive` | Register it switched off |

**Naming the graph.** The graph name and the ontology IRI can be the same (`ontolith add ontology https://example.org/hr# …`). A separate graph name such as `…/onto/hr` lets you load two versions of one ontology side by side, as `…/onto/hr/2.0` and `…/onto/hr/2.1`.

### 4.3 What an ontology does in Ontolith

During SHACL validation, the ontologies in scope for a data graph join the data graph. Ontolith also writes out the `rdf:type` triples they entail through `rdfs:subClassOf` and `owl:equivalentClass`. That is what makes `sh:targetClass core:Person` reach an `hr:Manager` (see [§8.3](#83-why-ontologies-take-part-in-validation)).

Ontolith does not run a full OWL reasoner and does not check OWL consistency. To use the ontologies for query-time reasoning, open the dataset in [oxilite](https://oxilitedb.com), which reads the same registry and scopes RDFS/OWL-QL reasoning per data graph. You can also export the graphs to an OWL reasoner.

## 5. Imports between ontologies

An ontology's scope includes the **active, registered** ontologies it imports, transitively. Import cycles are safe.

**Where imports come from:**

- `owl:imports` asserted inside the ontology's own graph, as in `hr.ttl` above;
- `oxl:imports` recorded in the registry (`--import`);
- `owl:imports` recorded in the registry. Older registries of vocabulary 2 used it, and it is still read.

**What an import matches:** a registered ontology's graph name, or its `oxl:ontologyIri`. `hr.ttl` imports `<https://example.org/core#>`. That is the `oxl:ontologyIri` of the graph `<https://example.org/onto/core>`, so core joins the scope:

```sh
$ ontolith scope https://example.org/data/staff
ontology <https://example.org/onto/hr>
ontology <https://example.org/onto/core>
shacl    <https://example.org/shapes/hr>
```

Core itself applies only to `data/other`. It reaches `data/staff` only through the import.

**What an import does not do:** fetch anything from the web. An import that matches no registered ontology is recorded and ignored. Load and register the imported ontology yourself:

```sh
ontolith add ontology http://xmlns.com/foaf/0.1/ --file foaf.rdf -t https://example.org/data/other
```

**An inactive ontology is never imported:**

```sh
$ ontolith deactivate https://example.org/onto/core
$ ontolith scope https://example.org/data/staff
ontology <https://example.org/onto/hr>
shacl    <https://example.org/shapes/hr>
```

**Why the registry uses `oxl:imports` and not `owl:imports`.** The domain of `owl:imports` is `owl:Ontology`. In the registry, it would make every registration an ontology, shapes graphs included, and an OWL tool loading the registry would try to fetch every import.

## 6. Adding SHACL shapes

### 6.1 Write the shapes

A shapes graph holds `sh:NodeShape`s. Each shape has **targets**, which select focus nodes, and **constraints**, usually on property paths. [`examples/hr/hr-shapes.ttl`](../examples/hr/hr-shapes.ttl):

```turtle
@prefix hrsh: <https://example.org/hr/shapes#> .
@prefix hr:   <https://example.org/hr#> .
@prefix core: <https://example.org/core#> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

# Every person has exactly one name and well-formed e-mail addresses.
hrsh:PersonShape a sh:NodeShape ;
    sh:targetClass core:Person ;
    sh:property [
        sh:path core:name ;
        sh:datatype xsd:string ;
        sh:minCount 1 ;
        sh:maxCount 1 ;
        sh:message "A person has exactly one name (xsd:string)."
    ] ;
    sh:property [
        sh:path hr:email ;
        sh:datatype xsd:string ;
        sh:pattern "^[^@\\s]+@[^@\\s]+\\.[a-z]{2,}$" ;
        sh:message "E-mail addresses look like name@domain.tld."
    ] .

# Every employee works for exactly one organization and has a non-negative salary.
hrsh:EmployeeShape a sh:NodeShape ;
    sh:targetClass hr:Employee ;
    sh:property [
        sh:path hr:worksFor ;
        sh:class core:Organization ;
        sh:minCount 1 ;
        sh:maxCount 1 ;
        sh:message "An employee works for exactly one organization."
    ] ;
    sh:property [
        sh:path hr:salary ;
        sh:datatype xsd:decimal ;
        sh:maxCount 1 ;
        sh:minInclusive 0 ;
        sh:message "Salary is one non-negative xsd:decimal."
    ] ;
    sh:property [ sh:path hr:hireDate ; sh:datatype xsd:date ; sh:maxCount 1 ] .

# A manager manages at least one employee.
hrsh:ManagerShape a sh:NodeShape ;
    sh:targetClass hr:Manager ;
    sh:property [
        sh:path hr:manages ;
        sh:class hr:Employee ;
        sh:minCount 1 ;
        sh:message "A manager manages at least one employee."
    ] .
```

### 6.2 A SHACL cheat sheet

**Targets** say which nodes a shape checks:

| Target | Focus nodes |
|---|---|
| `sh:targetClass C` | Instances of `C` and of its subclasses (through `rdfs:subClassOf` in the data graph) |
| `sh:targetNode n` | The node `n` |
| `sh:targetSubjectsOf p` | Every subject of a `p` triple |
| `sh:targetObjectsOf p` | Every object of a `p` triple |
| A shape that is also an `rdfs:Class` | Its own instances (an implicit class target) |

**Constraints** say what must hold:

| Constraint | Example |
|---|---|
| Cardinality | `sh:minCount 1 ; sh:maxCount 1` |
| Value type | `sh:datatype xsd:date`, `sh:class core:Organization`, `sh:nodeKind sh:IRI` |
| Value range | `sh:minInclusive 0`, `sh:maxExclusive 100` |
| Strings | `sh:pattern "^[A-Z]{2}$"`, `sh:minLength 2`, `sh:languageIn ("en" "de")` |
| Enumerations | `sh:in ( "draft" "final" )`, `sh:hasValue hr:Active` |
| Nested shapes | `sh:node hrsh:AddressShape` |
| Logic | `sh:or ( [ sh:datatype xsd:date ] [ sh:datatype xsd:dateTime ] )`, `sh:not`, `sh:and`, `sh:xone` |
| Pairs | `sh:equals`, `sh:disjoint`, `sh:lessThan` |
| Closed shapes | `sh:closed true ; sh:ignoredProperties ( rdf:type )` |
| Paths | `sh:path ( hr:worksFor core:name )`, `sh:path [ sh:inversePath hr:manages ]` |

**Reporting:**

- `sh:message` sets the text of a result.
- `sh:severity sh:Warning` (or `sh:Info`) turns a violation into a warning. Only results of severity `sh:Violation` make a graph non-conforming.
- `sh:deactivated true` switches a single shape off.

**Gotcha: `sh:datatype` is exact.** In Turtle, `-10` is an `xsd:integer`, and `120000.00` is an `xsd:decimal`. SHACL compares datatype IRIs and does not follow XSD derivation, so `sh:datatype xsd:decimal` rejects an integer. Write `-10.0`, or use `sh:or` over both datatypes.

### 6.3 Register them

```sh
$ ontolith add shacl https://example.org/shapes/hr --file hr-shapes.ttl \
      -t https://example.org/data/staff -t https://example.org/data/staff-broken
registered <https://example.org/shapes/hr> (38 triples)
shacl    active   <https://example.org/shapes/hr> -> <https://example.org/data/staff-broken> <https://example.org/data/staff>
```

Several shapes graphs can apply to one data graph: Ontolith validates against their union. A common setup is one organisation-wide shapes graph applied to every graph, plus a stricter one per dataset:

```sh
ontolith add shacl https://example.org/shapes/common --file common.ttl            # every graph
ontolith add shacl https://example.org/shapes/payroll --file payroll.ttl -t https://example.org/data/payroll
```

## 7. Mapping schemas to data graphs

`oxl:appliesTo` says which data a schema describes:

- **`oxl:AllGraphs`, or no `oxl:appliesTo` at all:** every graph.
- **One or more graph IRIs, or `oxl:DefaultGraph`:** only those graphs. If `oxl:AllGraphs` is among them, it wins.

Change only the targets, keeping roles, version and everything else:

```sh
ontolith map https://example.org/shapes/hr --to https://example.org/data/staff DEFAULT
ontolith map https://example.org/shapes/hr --to ALL
```

Per-graph mapping lets conflicting schemas live side by side:

```trig
<oxilite:schema> {
  <https://ex.org/onto/zoo>    a oxl:OntologyGraph ; oxl:appliesTo <https://ex.org/data/zoo> .
  <https://ex.org/onto/garden> a oxl:OntologyGraph ; oxl:appliesTo <https://ex.org/data/garden> .
  <https://ex.org/onto/common> a oxl:OntologyGraph ; oxl:appliesTo oxl:AllGraphs .
}
```

Here a `ex:Dog` in the zoo graph is read with the zoo and common ontologies. A `ex:Dog` in the garden graph is read with the garden and common ones.

## 8. Validating a graph with SHACL

### 8.1 Run it

```sh
$ ontolith validate https://example.org/data/staff
data:       <https://example.org/data/staff>
shapes:     <https://example.org/shapes/hr>
ontologies: <https://example.org/onto/hr> <https://example.org/onto/core>
conforms: yes
```

[`staff-broken.ttl`](../examples/hr/staff-broken.ttl) has deliberate mistakes:

```turtle
ex:acme a core:Organization ; core:name "ACME Corp" .

# A manager with no explicit hr:Employee type: the ontology makes it one.
ex:carol a hr:Manager ;
    core:name "Carol" ;
    hr:email "carol(at)acme" ;
    hr:salary -10 .

ex:dave a hr:Employee ;
    hr:worksFor ex:acme , ex:globex ;
    hr:salary "a lot" .

ex:globex core:name "Globex" .
```

```sh
$ ontolith validate https://example.org/data/staff-broken
data:       <https://example.org/data/staff-broken>
shapes:     <https://example.org/shapes/hr>
ontologies: <https://example.org/onto/hr> <https://example.org/onto/core>
conforms: no (10 results)
- Violation https://example.org/people/carol https://example.org/hr#email = "carol(at)acme"
    sh:PatternConstraintComponent
    E-mail addresses look like name@domain.tld.
- Violation https://example.org/people/carol https://example.org/hr#manages
    sh:MinCountConstraintComponent
    A manager manages at least one employee.
- Violation https://example.org/people/carol https://example.org/hr#salary = "-10"^^xsd:integer
    sh:DatatypeConstraintComponent
    Salary is one non-negative xsd:decimal.
- Violation https://example.org/people/carol https://example.org/hr#salary = "-10"^^xsd:integer
    sh:MinInclusiveConstraintComponent
    Salary is one non-negative xsd:decimal.
- Violation https://example.org/people/carol https://example.org/hr#worksFor
    sh:MinCountConstraintComponent
    An employee works for exactly one organization.
- Violation https://example.org/people/dave https://example.org/core#name
    sh:MinCountConstraintComponent
    A person has exactly one name (xsd:string).
- Violation https://example.org/people/dave https://example.org/hr#salary = "a lot"
    sh:DatatypeConstraintComponent
    Salary is one non-negative xsd:decimal.
- Violation https://example.org/people/dave https://example.org/hr#salary = "a lot"
    sh:MinInclusiveConstraintComponent
    Salary is one non-negative xsd:decimal.
- Violation https://example.org/people/dave https://example.org/hr#worksFor = https://example.org/people/globex
    sh:ClassConstraintComponent
    An employee works for exactly one organization.
- Violation https://example.org/people/dave https://example.org/hr#worksFor
    sh:MaxCountConstraintComponent
    An employee works for exactly one organization.
```

Each line is one SHACL validation result: severity, focus node, path, the offending value, the constraint component, and the shape's `sh:message`.

**Exit codes:** `0` the graph conforms, `1` it does not, `2` an error (bad arguments, unreadable shapes, no shapes in scope). That makes `validate` usable as a CI gate:

```sh
ontolith -d release.trig validate https://example.org/data/staff --output json > report.json
```

```json
{
  "conforms": false,
  "dataGraph": "https://example.org/data/staff-broken",
  "ontologyGraphs": ["https://example.org/onto/hr", "https://example.org/onto/core"],
  "results": [
    {
      "component": "http://www.w3.org/ns/shacl#PatternConstraintComponent",
      "focusNode": "https://example.org/people/carol",
      "message": "E-mail addresses look like name@domain.tld.",
      "path": "https://example.org/hr#email",
      "severity": "Violation",
      "sourceShape": "_:ea317d33824c3ab49b6453e8975b0fe2",
      "value": "\"carol(at)acme\""
    }
  ],
  "shapesGraphs": ["https://example.org/shapes/hr"]
}
```

### 8.2 How shapes and ontologies are chosen

For a data graph `D`:

1. **Shapes:** every *active* shapes graph whose `oxl:appliesTo` includes `D`, `oxl:AllGraphs`, or nothing. Their union is the shapes graph. Pass `--shapes G` (repeatable) to use named shapes graphs instead, registered or not. If no shapes apply, `validate` fails with exit code 2. It never guesses.
2. **Ontologies:** every active ontology that applies to `D`, plus the active ontologies they import, transitively. `--no-ontologies` leaves them out.
3. **Data graph:** `D`'s triples, plus the ontologies' triples, plus the `rdf:type` triples they entail (see §8.3).
4. **Engine:** [rudof](https://rudof-project.github.io/)'s native SHACL engine. `--sparql` uses its SPARQL-based engine instead.

`ontolith scope D` prints steps 1 and 2 without validating.

### 8.3 Why ontologies take part in validation

The SHACL spec defines "instance of `C`" through `rdf:type/rdfs:subClassOf*` **in the data graph**. A shape with `sh:targetClass core:Person` must therefore check every `hr:Manager`, but only if the data graph knows that `hr:Manager rdfs:subClassOf hr:Employee` and `hr:Employee rdfs:subClassOf core:Person`. Those axioms live in the ontology graphs, not in the data.

So Ontolith adds the ontologies in scope to the data graph. It also writes out the entailed types: for every `?x a ?c` with `?c (rdfs:subClassOf|owl:equivalentClass|^owl:equivalentClass)+ ?super`, it adds `?x a ?super`. Every engine then agrees, whatever its own class index does.

Compare the two runs on `staff-broken`:

| | With ontologies (default) | `--no-ontologies` |
|---|---|---|
| Carol (`a hr:Manager`) checked by `ManagerShape` | yes | yes |
| Carol checked by `EmployeeShape` (needs `hr:worksFor`) | **yes**, through `Manager ⊑ Employee` | no |
| Carol checked by `PersonShape` (e-mail pattern) | **yes**, through `Employee ⊑ Person` | no |
| Dave (`a hr:Employee`) checked by `PersonShape` (needs a name) | **yes** | no |
| Results | 10 | 5 |

Only class membership is entailed. Domains and ranges are not applied. If they were, `hr:salary`'s domain would silently turn any node with a salary into an `hr:Employee`, and a missing `a hr:Employee` would go unnoticed instead of being reported.

### 8.4 Validating from Rust

```rust
use ontolith::{Catalog, ValidateOptions, parse_graph};

let catalog = Catalog::open("hr.trig")?;
let v = catalog.validate(
    parse_graph("https://example.org/data/staff-broken")?.as_ref(),
    &ValidateOptions::default(),
)?;
if !v.report.conforms {
    for r in &v.report.results {
        eprintln!("{} {} {:?}: {:?}", r.severity, r.focus, r.path, r.message);
    }
}
```

`validate::shacl(data_ntriples, shapes, format, mode)` validates arbitrary RDF text without a catalog.

## 9. Validating with ShEx

ShEx validates chosen nodes against chosen shapes, given by a *shape map*. [`examples/hr/person.shex`](../examples/hr/person.shex):

```shex
PREFIX core: <https://example.org/core#>
PREFIX hr:   <https://example.org/hr#>
PREFIX xsd:  <http://www.w3.org/2001/XMLSchema#>
PREFIX :     <https://example.org/hr/shex#>

:Employee {
  core:name   xsd:string ;
  hr:email    xsd:string * ;
  hr:worksFor @:Organization ;
  hr:salary   xsd:decimal ?
}

:Organization {
  core:name xsd:string
}
```

```sh
$ ontolith shex https://example.org/data/staff --schema person.shex \
    --map '<https://example.org/people/bob>@<https://example.org/hr/shex#Employee>,<https://example.org/people/acme>@<https://example.org/hr/shex#Employee>'
FAIL https://example.org/people/acme@https://example.org/hr/shex#Employee
    Shape :Employee failed for node <https://example.org/people/acme>: no candidates matched the expression
└── Candidate [<https://example.org/core#name> "ACME Corp"] rejected: predicate <https://example.org/hr#worksFor> required cardinality {1, 1} but got 0
ok   https://example.org/people/bob@https://example.org/hr/shex#Employee
```

The exit code is `1` when any node fails. To keep a ShEx schema in the dataset, register its RDF form (ShExR, in Turtle for example) with `ontolith add shex …`. It is then recorded and mapped like the other roles. The `shex` command reads the ShExC text from `--schema`.

## 10. Validating the registry itself

A registry written by hand, or by another tool, can be wrong. The vocabulary ships `oxl:RegistrationShape`, so the registry checks itself:

```turtle
oxl:RegistrationShape a sh:NodeShape ;
    sh:targetClass oxl:OntologyGraph , oxl:ShapesGraph , oxl:ShExGraph , oxl:SystemGraph ;
    sh:targetSubjectsOf oxl:appliesTo , oxl:active , oxl:ontologyIri , oxl:version ,
                        oxl:sha256 , oxl:loadedAt , oxl:imports ;
    sh:nodeKind sh:IRI ;
    sh:property [
        sh:path rdf:type ;
        sh:qualifiedValueShape [ sh:in ( oxl:OntologyGraph oxl:ShapesGraph oxl:ShExGraph oxl:SystemGraph ) ] ;
        sh:qualifiedMinCount 1 ;
        sh:message "A registration needs a role class: oxl:OntologyGraph, oxl:ShapesGraph or oxl:ShExGraph."
    ] ;
    sh:property [ sh:path oxl:active ; sh:maxCount 1 ; sh:datatype xsd:boolean ] ;
    sh:property [ sh:path oxl:appliesTo ; sh:nodeKind sh:IRI ] ;
    sh:property [ sh:path oxl:sha256 ; sh:maxCount 1 ; sh:datatype xsd:string ;
                  sh:pattern "^[0-9a-f]{64}$" ] ;
    # … ontologyIri, version, loadedAt, imports
    .
```

`ontolith check` runs two checks: a plain Rust check of the same constraints, which needs no SHACL engine, and the SHACL shape itself through rudof. Break the registry on purpose:

```sh
$ ontolith update 'PREFIX oxl: <https://oxilite.dev/ns#>
    INSERT DATA { GRAPH <oxilite:schema> {
      <https://example.org/onto/hr> oxl:active "no" ; oxl:sha256 "ABC" .
      <https://example.org/x> oxl:appliesTo oxl:AllGraphs } }'

$ ontolith check
<https://example.org/onto/hr>: <https://oxilite.dev/ns#active> has 2 values, at most one is allowed
<https://example.org/onto/hr>: <https://oxilite.dev/ns#sha256> has 2 values, at most one is allowed
<https://example.org/onto/hr>: <https://oxilite.dev/ns#active> "no" is not an <http://www.w3.org/2001/XMLSchema#boolean>
<https://example.org/onto/hr>: <https://oxilite.dev/ns#sha256> "ABC" is not 64 lowercase hex digits
<https://example.org/x>: has registration properties but no role class (oxl:OntologyGraph, oxl:ShapesGraph, oxl:ShExGraph)
conforms: no (5 results)
- Violation https://example.org/onto/hr https://oxilite.dev/ns#active = "no"
    sh:DatatypeConstraintComponent
    Expected Datatype: xsd:boolean
…
```

The two checks find the same five problems, and the exit code is `1`. Readers stay lenient meanwhile: they ignore a value that breaks the shapes and never guess what it meant. Run `check` after any hand edit.

## 11. Lifecycle: versions, drift, deactivation, removal

**Drift detection.** `add` records the file's SHA-256. `verify` compares the file you have now with that digest:

```sh
$ ontolith verify https://example.org/onto/hr --file hr.ttl
unchanged
$ ontolith verify https://example.org/onto/hr --file hr-edited.ttl
changed: recorded 1aae19e3…, file 658695ac…        # exit 1
```

To pick up the change, run `add` again: it reloads the graph and rewrites the description.

**Upgrading an ontology.** Load the new version into its own graph, switched off, so both versions stay in the dataset. Then swap them, validate, and swap back if the data does not fit:

```sh
ontolith add ontology https://example.org/onto/hr/2.2 --file hr-2.2.ttl --inactive \
    -t https://example.org/data/staff
ontolith deactivate https://example.org/onto/hr        # still registered, contributes nothing
ontolith activate   https://example.org/onto/hr/2.2
ontolith validate https://example.org/data/staff \
  || { ontolith deactivate https://example.org/onto/hr/2.2; ontolith activate https://example.org/onto/hr; }
```

**Deactivate or remove:**

| Command | Registration | Graph's triples |
|---|---|---|
| `deactivate G` | Kept, `oxl:active false`. Out of every scope and never imported | Kept |
| `activate G` | `oxl:active true` | Kept |
| `remove G` | Deleted | Kept, as an ordinary graph |
| `remove G --drop` | Deleted | Deleted |

`map`, `activate`, `deactivate` and `remove` fail with exit code 2 on a graph that is not registered.

## 12. SPARQL recipes for any store

These are the exact updates Ontolith runs (`ontolith::registry::*_update`). They work unchanged with `ontolith update`, on Oxigraph, on oxilite, or on any SPARQL 1.1 store.

Register an ontology for two graphs, replacing any earlier description:

```sparql
PREFIX oxl: <https://oxilite.dev/ns#>
CREATE SILENT GRAPH <https://ex.org/onto/hr> ;
DELETE WHERE { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> ?p ?o } } ;
INSERT DATA { GRAPH <oxilite:schema> {
  <https://ex.org/onto/hr> a oxl:OntologyGraph ; oxl:active true ;
      oxl:appliesTo <https://ex.org/data/staff> , <https://ex.org/data/contractors> .
} }
```

Give a graph a second role:

```sparql
PREFIX oxl: <https://oxilite.dev/ns#>
INSERT DATA { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> a oxl:ShapesGraph } }
```

Change only the targets:

```sparql
PREFIX oxl: <https://oxilite.dev/ns#>
DELETE { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> oxl:appliesTo ?t } }
INSERT { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> oxl:appliesTo oxl:AllGraphs } }
WHERE  { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> a ?role
         OPTIONAL { <https://ex.org/onto/hr> oxl:appliesTo ?t } } }
```

Deactivate:

```sparql
PREFIX oxl: <https://oxilite.dev/ns#>
DELETE { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> oxl:active ?a } }
INSERT { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> oxl:active false } }
WHERE  { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> a ?role
         OPTIONAL { <https://ex.org/onto/hr> oxl:active ?a } } }
```

Unregister (the triples stay), or drop with the triples:

```sparql
DELETE WHERE { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> ?p ?o } }
```

```sparql
DELETE WHERE { GRAPH <oxilite:schema> { <https://ex.org/onto/hr> ?p ?o } } ;
DROP SILENT GRAPH <https://ex.org/onto/hr>
```

Find the active shapes graphs for a data graph. `?a = false` compares values, so `"0"^^xsd:boolean` matches and a plain `"false"` does not, just as Ontolith reads it:

```sparql
PREFIX oxl: <https://oxilite.dev/ns#>
SELECT ?shapes WHERE {
  GRAPH <oxilite:schema> {
    ?shapes a oxl:ShapesGraph .
    FILTER NOT EXISTS { ?shapes oxl:active ?a FILTER (?a = false) }
    FILTER (NOT EXISTS { ?shapes oxl:appliesTo ?any }
            || EXISTS { ?shapes oxl:appliesTo <https://example.org/data/staff> }
            || EXISTS { ?shapes oxl:appliesTo oxl:AllGraphs })
  }
}
```

Query the data without the schema graphs:

```sparql
SELECT ?s ?p ?o WHERE {
  GRAPH ?g { ?s ?p ?o }
  FILTER (?g NOT IN (<oxilite:schema>, <oxilite:vocabulary>))
  FILTER NOT EXISTS { GRAPH <oxilite:schema> { ?g a ?role } }
}
```

Derive `sh:shapesGraph` for a SHACL tool that expects it. It is the reverse of `oxl:appliesTo`, and Ontolith does not store it, so the two cannot drift apart:

```sparql
PREFIX oxl: <https://oxilite.dev/ns#>
PREFIX sh:  <http://www.w3.org/ns/shacl#>
CONSTRUCT { ?data sh:shapesGraph ?shapes }
WHERE { GRAPH <oxilite:schema> {
  ?shapes a oxl:ShapesGraph ; oxl:appliesTo ?data .
  FILTER (?data != oxl:AllGraphs)
  FILTER NOT EXISTS { ?shapes oxl:active ?a FILTER (?a = false) }
} }
```

`dcterms:conformsTo` (as in DCAT) is derived the same way, with `?data dcterms:conformsTo ?schema`.

## 13. Using another SHACL processor

The dataset is standard RDF, so any SHACL processor can validate it. Export the graphs that `ontolith scope` names:

```sh
ontolith export https://example.org/data/staff-broken > data.nt
ontolith export https://example.org/shapes/hr        > shapes.nt
ontolith export https://example.org/onto/hr          > hr.nt
ontolith export https://example.org/onto/core        > core.nt
```

**pySHACL.** `-e` adds an ontology to the data graph, and `-i rdfs` turns on RDFS inference:

```sh
cat hr.nt core.nt > ontologies.nt
pyshacl -s shapes.nt -e ontologies.nt -i rdfs -f human data.nt
```

**Apache Jena:**

```sh
cat data.nt hr.nt core.nt > data+ontologies.nt
shacl validate --shapes shapes.nt --data data+ontologies.nt
```

Validate the registry itself with `vocab/oxl.ttl` as the shapes graph:

```sh
ontolith export oxilite:schema > registry.nt
pyshacl -s vocab/oxl.ttl -f human registry.nt
```

## 14. Rust API

```toml
[dependencies]
ontolith = { git = "…", default-features = false }   # no CLI dependencies
```

```rust
use ontolith::oxigraph::io::RdfFormat;
use ontolith::{AddOptions, Catalog, SchemaRole, ValidateOptions, parse_graph, parse_targets};

let c = Catalog::open("hr.trig")?;          // or Catalog::new() for an empty in-memory dataset
c.install_system_graphs()?;

let staff = parse_graph("https://example.org/data/staff")?;
c.add(
    SchemaRole::Ontology,
    parse_graph("https://example.org/onto/hr")?.as_ref(),
    &std::fs::read("hr.ttl")?,
    RdfFormat::Turtle,
    &AddOptions { applies_to: vec![staff.clone()], ..Default::default() },
)?;
c.load(staff.as_ref(), &std::fs::read("staff.ttl")?, RdfFormat::Turtle, true)?;

for e in c.schemas_for(staff.as_ref(), SchemaRole::Ontology)? {
    println!("{} {:?}", e.graph, e.version);
}
let v = c.validate(staff.as_ref(), &ValidateOptions::default())?;
assert!(v.report.conforms, "{}", v.report);
c.save("hr.trig")?;
```

| `Catalog` method | Does |
|---|---|
| `new`, `open(path)`, `save(path)`, `store()` | Create, load and save the dataset; reach the Oxigraph store underneath |
| `install_system_graphs()` | Install or refresh `<oxilite:schema>` and `<oxilite:vocabulary>` |
| `add(role, graph, bytes, format, &AddOptions)` | Load a schema document and register it |
| `register(&SchemaGraph)` | Write a registration as given |
| `load(graph, bytes, format, replace)` | Load data |
| `list()`, `is_registered(g)`, `schemas_for(g, role)` | Read the registry; resolve scopes, imports included |
| `map(g, &targets)`, `set_active(g, bool)`, `unregister(g)`, `drop_graph(g)` | Change registrations |
| `problems()`, `check_registry()` | Check the registry, without and with SHACL |
| `verify(g, bytes)` | Detect drift |
| `validate(g, &ValidateOptions)`, `validate_shex(g, shexc, base, map)` | Validate |
| `query(sparql)`, `update(sparql)` | Plain SPARQL over the dataset |

The `registry` module holds the store-independent part: `register_update`, `remap_update`, `set_active_update`, `unregister_update`, `drop_update`, `entries_query` with `entries_from_rows`, `problems`, `system_graphs_update`, and `update_touches_registry`, which tells whether a SPARQL update can change the registry. Use them to drive any SPARQL endpoint.

## 15. Vocabulary reference

Namespace `oxl:` = `https://oxilite.dev/ns#`, version 2.1. The source is [`vocab/oxl.ttl`](../vocab/oxl.ttl), installed in `<oxilite:vocabulary>`.

| Term | Kind | Meaning |
|---|---|---|
| `oxl:RegisteredGraph` | class ⊑ `sd:Graph`; disjoint union of `oxl:SchemaGraph` and `oxl:SystemGraph` | A graph described in the registry; the domain of every registration property |
| `oxl:SchemaGraph` | class | A named graph holding schema rather than data |
| `oxl:OntologyGraph` | class ⊑ `oxl:SchemaGraph` | RDFS / OWL axioms |
| `oxl:ShapesGraph` | class ⊑ `oxl:SchemaGraph` | SHACL shapes |
| `oxl:ShExGraph` | class ⊑ `oxl:SchemaGraph` | A ShEx schema |
| `oxl:SystemGraph` | class, disjoint with `oxl:SchemaGraph` | `<oxilite:schema>`, `<oxilite:vocabulary>` |
| `oxl:GraphTarget` | class | What `oxl:appliesTo` points at |
| `oxl:appliesTo` | object property | A data graph, `oxl:DefaultGraph` or `oxl:AllGraphs` |
| `oxl:active` | functional, `xsd:boolean` | An `xsd:boolean` false switches a registration off; absent means true |
| `oxl:ontologyIri` | functional, IRI | The `owl:Ontology` IRI, when it differs from the graph name |
| `oxl:version` | functional, string | A pinned version |
| `oxl:sha256` | functional, string | Lowercase hex SHA-256 of the source document |
| `oxl:loadedAt` | functional, `xsd:dateTime`, ⊑ `dcterms:date` | When it was registered |
| `oxl:imports` | IRI | An import recorded in the registry |
| `oxl:DefaultGraph`, `oxl:AllGraphs` | individuals | The default graph; every graph |
| `oxl:RegistrationShape` | `sh:NodeShape` | The shape every registration conforms to |

## 16. Compatibility and limits

**Compatibility with oxilite.** Ontolith was extracted from [oxilite](https://oxilitedb.com)'s schema registry and writes the same wire format: `oxl:` vocabulary 2.1, `<oxilite:schema>`, `<oxilite:vocabulary>`. You can load a dataset that Ontolith manages into oxilite with `oxilite load --location hr.db --file hr.trig`, and oxilite then uses the registrations for query-time RDFS/OWL-QL reasoning and the shape index. A TriG or N-Quads dump of an oxilite store (`Store::dump_to_writer`) opens in Ontolith the same way. The graph and namespace IRIs stay as they are for that reason.

**Limits:**

- **Storage.** The dataset is held in memory and saved to one TriG/N-Quads file. For large datasets, use the `registry` SPARQL builders against a real store (Oxigraph, oxilite, any SPARQL 1.1 endpoint), or validate with oxilite.
- **No OWL reasoner.** Ontologies take part in validation through class membership only (§8.3). Ontolith checks neither OWL consistency nor property characteristics.
- **No fetching.** Imports resolve only against registered ontologies.
- **No blank-node graphs.** A graph named by a blank node cannot be registered, because another graph cannot name it.
- **Shared description.** The roles of one graph share one description: targets, active flag and version.
- **SHACL coverage** is rudof's: SHACL Core. Check rudof's documentation for SHACL-SPARQL and newer features.
