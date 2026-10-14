# Ontobug guide

How to keep OWL ontologies, SHACL shapes and ShEx schemas next to your data in one RDF dataset, say which schema governs which data, and validate data graphs against them.

Every example below is runnable: the files are in [`examples/hr`](../examples/hr), and the outputs are the ones Ontobug prints.

- [1. Concepts](#1-concepts)
- [2. Install and first run](#2-install-and-first-run)
- [3. The registry (Ladybug)](#3-the-registry-ladybug)
- [4. Adding an OWL ontology](#4-adding-an-owl-ontology)
- [5. Imports between ontologies](#5-imports-between-ontologies)
- [6. Adding SHACL shapes](#6-adding-shacl-shapes)
- [7. Mapping schemas to data graphs](#7-mapping-schemas-to-data-graphs)
- [8. Validating a graph with SHACL](#8-validating-a-graph-with-shacl)
- [9. Validating with ShEx](#9-validating-with-shex)
- [10. Checking the registry](#10-checking-the-registry)
- [11. Lifecycle: versions, drift, deactivation, removal](#11-lifecycle-versions-drift-deactivation-removal)
- [12. Cypher recipes](#12-cypher-recipes)
- [13. Using another SHACL processor](#13-using-another-shacl-processor)
- [14. Rust API](#14-rust-api)
- [15. Registry schema reference](#15-registry-schema-reference)
- [16. Compatibility and limits](#16-compatibility-and-limits)

---

## 1. Concepts

A **dataset** is a set of named graphs. Ontobug gives each graph a job:

| Kind of graph | Holds | Example |
|---|---|---|
| **Data graph** | Instance data | `<https://example.org/data/staff>` |
| **Ontology graph** | RDFS / OWL axioms: classes, properties, hierarchies | `<https://example.org/onto/hr>` |
| **Shapes graph** | SHACL shapes: constraints on the data | `<https://example.org/shapes/hr>` |
| **ShEx graph** | A ShEx schema | `<https://example.org/shex/hr>` |
| **Registry** | One `SchemaEntry` node per schema graph, in Ladybug | `MATCH (e:SchemaEntry) RETURN e.graph, e.role` |

The **registry** lives in Ladybug. It records, for each schema graph:

- its **role**: ontology, SHACL or ShEx;
- the data graphs it **applies to**, or all graphs;
- whether it is **active**;
- the ontology IRI, a pinned **version**, the **SHA-256** of the file it came from, **imports**, and when it was loaded.

Because the registry is a property graph, you query and change it with Cypher — `ontobug cypher` runs it, or use any Ladybug client against the `.lbdb` file.

```
             ┌────────────────── Ladybug registry ──────────────────┐
             │ (:SchemaEntry {graph: onto/core,  role: ontology,   │
             │                 appliesTo: [data/other]})           │
             │ (:SchemaEntry {graph: onto/hr,    role: ontology,   │
             │                 appliesTo: [data/staff],            │
             │                 ontologyIri: hr#})                  │
             │ (:SchemaEntry {graph: shapes/hr, role: shacl,       │
             │                 appliesTo: [data/staff]})           │
             └─────────────────────────────────────────────────────┘
                     │ scope of data/staff = hr + core (import) + shapes/hr
                     ▼
   data/staff ──validate──▶ SHACL engine (rudof) ──▶ report
```

Registering a graph never moves, copies or rewrites its triples. The registry only describes it.

## 2. Install and first run

```sh
git clone <this repo> ontobug && cd ontobug
cargo install --path .          # installs the `ontobug` binary
```

Every command works on one dataset, and the dataset is the Ladybug database, given with `--ladybug` (`-l`) or `ONTOBUG_LADYBUG` (default: the dataset stem with `.lbdb`, so `hr.trig` goes with `hr.lbdb`). It holds every triple losslessly as `DataTriple` rows, plus the registry and the projected schemas. `--dataset` (`-d`, `ONTOBUG_DATASET`, default `dataset.trig`) is the TriG/N-Quads snapshot: each command that changes content rewrites it atomically, and opening with an empty database seeds from it.

The whole HR walkthrough in one go:

```sh
cd examples/hr
export ONTOBUG_DATASET=hr.trig
# …which puts the registry in hr.lbdb next to it.

ontobug add ontology https://example.org/onto/core --file core.ttl \
    --applies-to https://example.org/data/other
ontobug add ontology https://example.org/onto/hr --file hr.ttl \
    -t https://example.org/data/staff -t https://example.org/data/staff-broken
ontobug add shacl https://example.org/shapes/hr --file hr-shapes.ttl \
    -t https://example.org/data/staff -t https://example.org/data/staff-broken
ontobug load https://example.org/data/staff        --file staff.ttl
ontobug load https://example.org/data/staff-broken --file staff-broken.ttl

ontobug list
ontobug validate https://example.org/data/staff          # conforms: yes, exit 0
ontobug validate https://example.org/data/staff-broken   # 10 violations, exit 1
```

Graph arguments take an IRI, or `DEFAULT` for the default graph. In `--applies-to` and `map --to`, `ALL` means every graph.

## 3. The registry (Ladybug)

After the walkthrough, the registry holds one `SchemaEntry` node per registration:

```sh
$ ontobug list
ontology active   <https://example.org/onto/hr> -> <https://example.org/data/staff> <https://example.org/data/staff-broken>  version=https://example.org/hr/2.1.0  ontology=<https://example.org/hr#>
shacl    active   <https://example.org/shapes/hr> -> <https://example.org/data/staff> <https://example.org/data/staff-broken>

$ ontobug cypher "MATCH (e:SchemaEntry) RETURN e.graph, e.role, e.appliesTo, e.version, e.active ORDER BY e.graph, e.role"
https://example.org/onto/core\tontology\t["https://example.org/data/other"]\t1.0.0\tTrue
https://example.org/onto/hr\tontology\t["https://example.org/data/staff","https://example.org/data/staff-broken"]\thttps://example.org/hr/2.1.0\tTrue
https://example.org/shapes/hr\tshacl\t["https://example.org/data/staff","https://example.org/data/staff-broken"]\t\tTrue
```

Each node carries the registration: `graph`, `role` (`ontology`, `shacl` or `shex`),
`ontologyIri`, `version`, `sha256`, `active`, `loadedAt`, and the `appliesTo` and
`imports` lists as JSON arrays. An empty `appliesTo` means every graph.

The schema's content stays in its RDF graph; the registry only describes it:

```trig
<https://example.org/onto/hr> {
  <https://example.org/hr#> a owl:Ontology ; owl:imports <https://example.org/core#> .
  hr:Employee a owl:Class ; rdfs:subClassOf core:Person .
  # …
}
```

**One graph, several roles.** An ontology that carries its own SHACL shapes gets one node
per role, so scoping counts it once per role. `ontobug add` writes one role; register the
same graph again with another role to add the second:

```sh
ontobug add ontology https://example.org/onto/hr --file hr.ttl -t https://example.org/data/staff
ontobug add shacl    https://example.org/onto/hr --file hr-shapes.ttl -t https://example.org/data/staff
```

## 3.1 Schema content as nodes and edges

The registry says *which* graphs hold schemas. The schemas *themselves* are projected
into Ladybug as a property graph — `add` does this automatically for ontology and shapes
graphs, and `ontobug project GRAPH` rebuilds a graph's projection:

```sh
$ ontobug project https://example.org/onto/hr
projected <https://example.org/onto/hr>: 5 classes, 6 properties, 10 edges, 0 shapes, 0 constraints, 8 links
```

(The 5 classes include stubs for `core:Person` and `core:Organization`, which this
graph references but only the core graph defines — project it too and the stubs give
way to shared nodes.)

- Ontology classes and properties become `OntologyClass` / `OntologyProperty` nodes
  (IRI, label, comment, owning graph), with `SUBCLASS_OF`, `EQUIVALENT_CLASS`, `DOMAIN`
  and `RANGE` edges. References to classes no graph defines (an `owl:equivalentClass` to
  an external ontology) become labelled-empty stubs, so edges stay connected. Datatype
  ranges (`xsd:string`, …) are kept as a `rangeDatatypes` list on the property.
- SHACL shapes become `ShaclShape` nodes with `ShaclProperty` constraint nodes
  (path, datatype, class, counts, pattern, message, …) over `HAS_CONSTRAINT` edges.
- `TARGETS` links shapes to the `OntologyClass` nodes of their target classes (whatever
graph defined them), and `ON_PROPERTY` links constraints to their `OntologyProperty`
nodes — rebuilt over all shapes after every projection, since rebuilding a graph
deletes the class nodes its shapes pointed at.

So the class hierarchy and the shape-to-property paths are queryable (see [§12](#12-cypher-recipes)).
Unregistering keeps a graph's projection with its triples; `remove --drop` deletes both.

Namespaces partition the projection the way they partition RDF: every content node
carries its `namespace` (the term IRI up to its last `#` or `/`), and `Namespace` nodes
inventory each one with its prefix, term count and source graphs:

```sh
$ ontobug namespaces
core       https://example.org/core#                         5 terms  https://example.org/onto/core https://example.org/shapes/hr
hr         https://example.org/hr#                          14 terms  https://example.org/onto/hr https://example.org/shapes/hr
hrsh       https://example.org/hr/shapes#                    3 terms  https://example.org/shapes/hr
```

Prefixes come from the documents' `@prefix` declarations when present
(`@prefix core:` above; the shapes' own `@prefix hrsh:` similarly wins over a derived
name), and are derived from the IRI otherwise. Query a namespace as a subgraph
([§12](#12-cypher-recipes)). Databases written before namespaces existed are migrated
automatically on open.

## 4. Adding an OWL ontology

### 4.1 Write the ontology

Any RDFS or OWL document works: Turtle, N-Triples, RDF/XML (`.rdf`, `.owl`), N3. Give it an `owl:Ontology` header. Ontobug reads the IRI and version from it. [`examples/hr/core.ttl`](../examples/hr/core.ttl):

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
$ ontobug add ontology https://example.org/onto/hr --file hr.ttl \
      -t https://example.org/data/staff -t https://example.org/data/staff-broken
registered <https://example.org/onto/hr> (41 triples)
ontology active   <https://example.org/onto/hr> -> <https://example.org/data/staff-broken> <https://example.org/data/staff>  version=https://example.org/hr/2.1.0  ontology=<https://example.org/hr#>
```

`add` does four things in one go:

1. **Clears** the graph and loads the file into it.
2. **Reads the header.** If the graph declares exactly one `owl:Ontology` whose IRI differs from the graph name, it records it as `ontologyIri`. If you give no `--version`, it pins `owl:versionIRI`, or else `owl:versionInfo`.
3. **Records the file's SHA-256** as `sha256`, for drift detection (see [§11](#11-lifecycle-versions-drift-deactivation-removal)).
4. **Writes the registration**, replacing any earlier description of the graph.

Options:

| Option | Effect |
|---|---|
| `-t, --applies-to G` | A data graph it governs; repeatable. `ALL`, or no option at all, means every graph |
| `--version V` | Pin a version instead of the one read from the header |
| `--import IRI` | Record an import in the registry (`imports`); repeatable |
| `--inactive` | Register it switched off |

**Naming the graph.** The graph name and the ontology IRI can be the same (`ontobug add ontology https://example.org/hr# …`). A separate graph name such as `…/onto/hr` lets you load two versions of one ontology side by side, as `…/onto/hr/2.0` and `…/onto/hr/2.1`.

### 4.3 What an ontology does in Ontobug

During SHACL validation, the ontologies in scope for a data graph join the data graph. Ontobug also writes out the `rdf:type` triples they entail through `rdfs:subClassOf` and `owl:equivalentClass`. That is what makes `sh:targetClass core:Person` reach an `hr:Manager` (see [§8.3](#83-why-ontologies-take-part-in-validation)).

Ontobug does not run a full OWL reasoner and does not check OWL consistency. To use the ontologies for query-time reasoning, export the graphs (`ontobug export`) to an OWL reasoner.

## 5. Imports between ontologies

An ontology's scope includes the **active, registered** ontologies it imports, transitively. Import cycles are safe.

**Where imports come from:**

- `owl:imports` asserted inside the ontology's own graph, as in `hr.ttl` above;
- `imports` recorded in the registry (`--import`);
- `owl:imports` recorded in the registry. Older registries of vocabulary 2 used it, and it is still read.

**What an import matches:** a registered ontology's graph name, or its `ontologyIri`. `hr.ttl` imports `<https://example.org/core#>`. That is the `ontologyIri` of the graph `<https://example.org/onto/core>`, so core joins the scope:

```sh
$ ontobug scope https://example.org/data/staff
ontology <https://example.org/onto/hr>
ontology <https://example.org/onto/core>
shacl    <https://example.org/shapes/hr>
```

Core itself applies only to `data/other`. It reaches `data/staff` only through the import.

**What an import does not do:** fetch anything from the web. An import that matches no registered ontology is recorded and ignored. Load and register the imported ontology yourself:

```sh
ontobug add ontology http://xmlns.com/foaf/0.1/ --file foaf.rdf -t https://example.org/data/other
```

**An inactive ontology is never imported:**

```sh
$ ontobug deactivate https://example.org/onto/core
$ ontobug scope https://example.org/data/staff
ontology <https://example.org/onto/hr>
shacl    <https://example.org/shapes/hr>
```

**Why the registry uses its own `imports` and not `owl:imports`.** The domain of `owl:imports` is `owl:Ontology`, so in an RDF registry every registration would read as an ontology. Ladybug nodes carry no such entailment, but the split is kept: `owl:imports` asserted *inside* an ontology's graph, `imports` recorded *about* it in the registry.

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
$ ontobug add shacl https://example.org/shapes/hr --file hr-shapes.ttl \
      -t https://example.org/data/staff -t https://example.org/data/staff-broken
registered <https://example.org/shapes/hr> (38 triples)
shacl    active   <https://example.org/shapes/hr> -> <https://example.org/data/staff-broken> <https://example.org/data/staff>
```

Several shapes graphs can apply to one data graph: Ontobug validates against their union. A common setup is one organisation-wide shapes graph applied to every graph, plus a stricter one per dataset:

```sh
ontobug add shacl https://example.org/shapes/common --file common.ttl            # every graph
ontobug add shacl https://example.org/shapes/payroll --file payroll.ttl -t https://example.org/data/payroll
```

## 7. Mapping schemas to data graphs

`appliesTo` says which data a schema describes:

- **An empty `appliesTo` list:** every graph (`ALL` on the CLI).
- **One or more graph IRIs, or `DEFAULT`:** only those graphs.

Change only the targets, keeping roles, version and everything else:

```sh
ontobug map https://example.org/shapes/hr --to https://example.org/data/staff DEFAULT
ontobug map https://example.org/shapes/hr --to ALL
```

Per-graph mapping lets conflicting schemas live side by side:

```cypher
MATCH (e:SchemaEntry)
WHERE e.graph IN ['https://ex.org/onto/zoo', 'https://ex.org/onto/garden',
                  'https://ex.org/onto/common']
RETURN e.graph, e.appliesTo;

// https://ex.org/onto/zoo       ["https://ex.org/data/zoo"]
// https://ex.org/onto/garden    ["https://ex.org/data/garden"]
// https://ex.org/onto/common    []   <- every graph
```

Here a `ex:Dog` in the zoo graph is read with the zoo and common ontologies. A `ex:Dog` in the garden graph is read with the garden and common ones.

## 8. Validating a graph with SHACL

### 8.1 Run it

```sh
$ ontobug validate https://example.org/data/staff
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
$ ontobug validate https://example.org/data/staff-broken
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
ontobug -d release.trig validate https://example.org/data/staff --output json > report.json
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

1. **Shapes:** every *active* shapes graph whose `appliesTo` includes `D`, or is empty. Their union is the shapes graph. Pass `--shapes G` (repeatable) to use named shapes graphs instead, registered or not. If no shapes apply, `validate` fails with exit code 2. It never guesses.
2. **Ontologies:** every active ontology that applies to `D`, plus the active ontologies they import, transitively. `--no-ontologies` leaves them out.
3. **Data graph:** `D`'s triples, plus the ontologies' triples, plus the `rdf:type` triples they entail (see §8.3).
4. **Engine:** [rudof](https://rudof-project.github.io/)'s native SHACL engine.

`ontobug scope D` prints steps 1 and 2 without validating.

### 8.3 Why ontologies take part in validation

The SHACL spec defines "instance of `C`" through `rdf:type/rdfs:subClassOf*` **in the data graph**. A shape with `sh:targetClass core:Person` must therefore check every `hr:Manager`, but only if the data graph knows that `hr:Manager rdfs:subClassOf hr:Employee` and `hr:Employee rdfs:subClassOf core:Person`. Those axioms live in the ontology graphs, not in the data.

So Ontobug adds the ontologies in scope to the data graph. It also writes out the entailed types: for every `?x a ?c` with `?c (rdfs:subClassOf|owl:equivalentClass|^owl:equivalentClass)+ ?super`, it adds `?x a ?super`. Every engine then agrees, whatever its own class index does.

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
use ontobug::{Catalog, ValidateOptions, parse_graph};

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
$ ontobug shex https://example.org/data/staff --schema person.shex \
    --map '<https://example.org/people/bob>@<https://example.org/hr/shex#Employee>,<https://example.org/people/acme>@<https://example.org/hr/shex#Employee>'
FAIL https://example.org/people/acme@https://example.org/hr/shex#Employee
    Shape :Employee failed for node <https://example.org/people/acme>: no candidates matched the expression
└── Candidate [<https://example.org/core#name> "ACME Corp"] rejected: predicate <https://example.org/hr#worksFor> required cardinality {1, 1} but got 0
ok   https://example.org/people/bob@https://example.org/hr/shex#Employee
```

The exit code is `1` when any node fails. To keep a ShEx schema in the dataset, register its RDF form (ShExR, in Turtle for example) with `ontobug add shex …`. It is then recorded and mapped like the other roles. The `shex` command reads the ShExC text from `--schema`.

## 10. Checking the registry

A registration written out of band can be wrong: a digest that is not 64 lowercase
hex digits, or a graph registered twice in one role. `ontobug check` runs the
structural checks over the Ladybug registry and exits `1` on problems:

```sh
$ ontobug check
registry ok (3 registrations)
```

Break one on purpose through Cypher (Ladybug has no constraints of its own, so
anything goes until `check` runs):

```sh
$ ontobug cypher "MATCH (e:SchemaEntry) WHERE e.graph = 'https://example.org/onto/hr' SET e.sha256 = 'ABC' RETURN e.graph"

$ ontobug check
<https://example.org/onto/hr>: sha256 ABC is not 64 lowercase hex digits
```

Fix it by re-adding the graph (`add` rewrites the registration and re-projects the
content), then `check` is green again. Run `check` after any hand edit.

## 11. Lifecycle: versions, drift, deactivation, removal

**Drift detection.** `add` records the file's SHA-256. `verify` compares the file you have now with that digest:

```sh
$ ontobug verify https://example.org/onto/hr --file hr.ttl
unchanged
$ ontobug verify https://example.org/onto/hr --file hr-edited.ttl
changed: recorded 1aae19e3…, file 658695ac…        # exit 1
```

To pick up the change, run `add` again: it reloads the graph and rewrites the description.

**Upgrading an ontology.** Load the new version into its own graph, switched off, so both versions stay in the dataset. Then swap them, validate, and swap back if the data does not fit:

```sh
ontobug add ontology https://example.org/onto/hr/2.2 --file hr-2.2.ttl --inactive \
    -t https://example.org/data/staff
ontobug deactivate https://example.org/onto/hr        # still registered, contributes nothing
ontobug activate   https://example.org/onto/hr/2.2
ontobug validate https://example.org/data/staff \
  || { ontobug deactivate https://example.org/onto/hr/2.2; ontobug activate https://example.org/onto/hr; }
```

**Deactivate or remove:**

| Command | Registration | Graph's triples |
|---|---|---|
| `deactivate G` | Kept, `active` false. Out of every scope and never imported | Kept |
| `activate G` | `active` true | Kept |
| `remove G` | Deleted | Kept, as an ordinary graph |
| `remove G --drop` | Deleted | Deleted |

`map`, `activate`, `deactivate` and `remove` fail with exit code 2 on a graph that is not registered.

## 12. Cypher recipes

The registry and the projected schemas are plain Ladybug nodes and edges. Run these
with `ontobug cypher`, or any Ladybug client against the `.lbdb` file.

Registrations: the active shapes graphs and what they govern:

```cypher
MATCH (e:SchemaEntry)
WHERE e.role = 'shacl' AND e.active = true
RETURN e.graph, e.appliesTo ORDER BY e.graph;
```

Class hierarchy: superclasses of a class, transitively:

```cypher
MATCH (c:OntologyClass)-[:SUBCLASS_OF*1..5]->(s:OntologyClass)
WHERE c.iri = 'https://example.org/hr#Manager'
RETURN s.iri;
```

Properties of a class: everything with a domain (or range) on it:

```cypher
MATCH (p:OntologyProperty)-[:DOMAIN]->(c:OntologyClass)
WHERE c.iri = 'https://example.org/hr#Employee'
RETURN p.iri, p.kind;
```

Shapes to data, through the ontology: which shapes target a class, and which
properties their constraints check:

```cypher
MATCH (s:ShaclShape)-[:TARGETS]->(c:OntologyClass)<-[:DOMAIN]-(p:OntologyProperty)
WHERE c.iri = 'http://schema.org/Person'
RETURN s.shape, p.iri;
```

Constraint details for one path:

```cypher
MATCH (p:ShaclProperty) WHERE p.path = 'https://example.org/hr#salary'
RETURN p.datatype, p.minCount, p.maxCount, p.minInclusive, p.message;
```

Stubs: classes referenced but defined nowhere (external imports worth registering):

```cypher
MATCH (c:OntologyClass) WHERE c.label = '' RETURN c.graph, c.iri;
```

Namespaces: terms of one vocabulary — the subgraph view:

```cypher
MATCH (c:OntologyClass) WHERE c.namespace = 'http://schema.org/' RETURN count(c);
```

Namespace inventory, same as `ontobug namespaces`:

```cypher
MATCH (n:Namespace) RETURN n.prefix, n.iri, n.termCount ORDER BY n.prefix;
```

## 13. Using another SHACL processor

The dataset is standard RDF, so any SHACL processor can validate it. Export the graphs that `ontobug scope` names:

```sh
ontobug export https://example.org/data/staff-broken > data.nt
ontobug export https://example.org/shapes/hr        > shapes.nt
ontobug export https://example.org/onto/hr          > hr.nt
ontobug export https://example.org/onto/core        > core.nt
```

**pySHACL.** `-e` adds an ontology to the data graph, and `-i rdfs` turns on RDFS inference:

```sh
cat hr.nt core.nt > ontologies.nt
pyshacl -s shapes.nt -e ontologies.nt -i rdfs -f human data.nt
```

## 14. Rust API

```toml
[dependencies]
ontobug = { git = "…", default-features = false }   # no CLI dependencies
```

```rust
use ontobug::oxigraph::io::RdfFormat;
use ontobug::{AddOptions, Catalog, SchemaRole, ValidateOptions, parse_graph, parse_targets};

let c = Catalog::open("hr.trig", "hr.lbdb")?;   // or Catalog::new() for empty in-memory state

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
| `new`, `open(data, registry)`, `save(data)`, `registry()` | Create the dataset in Ladybug; seed an empty one from a snapshot; write snapshots; reach the database underneath |
| `default_registry_path(dataset)` | The `.lbdb` path for a dataset file |
| `add(role, graph, bytes, format, &AddOptions)` | Load a schema document as triples, register it, and project it as nodes and edges |
| `register(&SchemaGraph)` | Write a registration as given |
| `project_graph_content(g)` | (Re)build a graph's nodes and edges; returns counts |
| `load(graph, bytes, format, replace)` | Load triples (data or schema) |
| `graph_ntriples(g)`, `graph_len(g)` | A graph's triples / count, reconstructed from its rows |
| `list()`, `is_registered(g)`, `schemas_for(g, role)` | Read the registry; resolve scopes, imports included |
| `map(g, &targets)`, `set_active(g, bool)`, `unregister(g)`, `drop_graph(g)` | Change registrations (`drop_graph` also removes the projection and the triples) |
| `problems()` | Check the registry structurally |
| `verify(g, bytes)` | Detect drift |
| `validate(g, &ValidateOptions)`, `validate_shex(g, shexc, base, map)` | Validate from the triple rows |
| `cypher(cypher)`, `namespaces()` | Run Cypher against the database; list the namespace subgraphs |

## 15. Registry schema reference

One `SchemaEntry` node per registration (`id` is graph plus role): `graph`, `role`
(`ontology`, `shacl`, `shex`), `ontologyIri`, `version`, `sha256`, `active`, `loadedAt`,
and the `appliesTo` / `imports` lists as JSON arrays (empty `appliesTo` is every graph).

Projected content, rebuilt by `add` and `ontobug project` (every content node also
carries its `namespace`):

| Table | Kind | Properties / meaning |
|---|---|---|
| `OntologyClass` | node | `iri`, `label`, `comment`, owning `graph`; empty label is a stub: referenced, defined nowhere |
| `OntologyProperty` | node | `iri`, `kind` (`object`, `datatype`, `annotation`, `property`), `label`, `comment`, `rangeDatatypes` (JSON), `graph` |
| `ShaclShape` | node | `shape`, `targetClasses` / `targetNodes` / `targetSubjectsOf` / `targetObjectsOf` (JSON), `message`, `severity`, `deactivated`, `graph` |
| `ShaclProperty` | node | `shapeId`, `path`, `datatype`, `class`, `nodeKind`, `minCount`, `maxCount`, `pattern`, `minInclusive`, `maxInclusive`, `message`, `severity`, `deactivated`, `graph` |
| `SUBCLASS_OF`, `EQUIVALENT_CLASS` | rel | Class to class |
| `DOMAIN`, `RANGE` | rel | Property to class |
| `HAS_CONSTRAINT` | rel | Shape to its property constraints |
| `TARGETS` | rel | Shape to the classes of its target classes, whatever graph defined them |
| `ON_PROPERTY` | rel | Constraint to the property of its path |
| `Namespace` | node | `iri`, `prefix`, `sourceGraphs` (JSON), `termCount`, `triples` |
| `DataTriple` | node | `subject`, `predicate`, `object` as N-Triples tokens, `lex` (unescaped literal value), `namespace`, `graph`; `id` is graph plus zero-padded ordinal, so `ORDER BY id` reproduces document order |
| `Namespace` | node | `iri`, `prefix`, `sourceGraphs` (JSON), `termCount`; one per namespace in the projected content |

## 16. Compatibility and limits

**Origin.** Ontobug began as the schema registry of [oxilite](https://oxilitedb.com)
(SPARQL on SQLite). The registry has since moved to [Ladybug](https://ladybugdb.com):
registrations are `SchemaEntry` nodes queried with Cypher, and there is no oxilite wire
compatibility anymore. RDF content stays standard TriG/N-Quads, so any SHACL processor
still validates it ([§13](#13-using-another-shacl-processor)).

**Limits:**

- **Storage.** Everything lives in a Ladybug `.lbdb` database: triple rows, registry
and projections. TriG/N-Quads files are snapshots — written by save/export, read only
to seed an empty database.
- **Projection is one-way.** Ladybug mirrors the RDF graphs for querying; edit the
documents, `add` them again, and the projection rebuilds.
- **No OWL reasoner.** Ontologies take part in validation through class membership only (§8.3). Ontobug checks neither OWL consistency nor property characteristics.
- **No fetching.** Imports resolve only against registered ontologies.
- **No blank-node graphs.** A graph named by a blank node cannot be registered, because no other graph can name it.
- **Projection limits.** Complex SHACL paths (`sh:inversePath` aside) are stored as written, and `sh:in` / `sh:node` references are not followed.
- **SHACL coverage** is rudof's: SHACL Core. Check rudof's documentation for SHACL-SPARQL and newer features.
