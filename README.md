# SNOMED CT Compiler in Rust — Design Notes

A design document for a build-time compiler that ingests SNOMED CT RF2 releases and emits a packed binary artifact for ultrafast in-process lookups. Scope is the **compiler and query layer only** — ECL parsing is covered separately.

## Goals

- Ingest a SNOMED CT RF2 release (Snapshot or Full) once per release cycle.
- Emit a single immutable binary artifact, memory-mappable, suitable for zero-copy lookups.
- Provide sub-microsecond descendant/ancestor membership tests and fast set operations.
- Be deployable as an embedded library, a CLI, or a small service — not a heavyweight server.
- Play well with Snowflake (closure table export) and Python (PyO3 bindings) for downstream consumers.

## Non-goals

- Authoring/editing SNOMED content.
- FHIR terminology service compliance (could be layered on top later).
- ECL parsing and evaluation (separate module).
- Description text search / autocomplete (separate module; may share the artifact).

---

## Why a compiled artifact

SNOMED is **read-only between releases** and updated monthly. This is the ideal shape for ahead-of-time compilation: pay the compute cost once per release, ship an immutable artifact, get effectively free queries forever after.

Comparison points for context:

- **Snowstorm (Elasticsearch)**: 20–200 ms per descendant query, multi-GB JVM, minutes to warm up.
- **Postgres closure table**: 1–20 ms per descendant query, requires a database.
- **Snowflake closure table**: 100 ms – 2 s per query; high per-query overhead.
- **Compiled artifact (this design)**: 100 ns – 10 µs per query, ~80–500 MB on disk, instant startup via `mmap`.

Realistic claim: **100–1000× speedup over Snowflake closure-table patterns for cohort/classification workloads**, and roughly an order of magnitude less compute spend.

---

## High-level architecture

```
┌──────────────────────┐      ┌──────────────────────┐      ┌──────────────────────┐
│  RF2 release files   │ ───▶ │  snomed-compile      │ ───▶ │  snomed.bin (mmap)   │
│  (Snapshot/Full)     │      │  (Rust CLI)          │      │  packed artifact     │
└──────────────────────┘      └──────────────────────┘      └──────────────────────┘
                                                                       │
                                                                       ▼
                                                            ┌──────────────────────┐
                                                            │  snomed-query        │
                                                            │  (Rust library)      │
                                                            │  - mmap loader       │
                                                            │  - descendant ops    │
                                                            │  - bitmap set ops    │
                                                            └──────────────────────┘
                                                                       │
                                            ┌──────────────────────────┼──────────────────────────┐
                                            ▼                          ▼                          ▼
                                   ┌────────────────┐        ┌────────────────┐         ┌────────────────┐
                                   │  CLI tool      │        │  PyO3 bindings │         │  HTTP/gRPC svc │
                                   │  (ad-hoc)      │        │  (notebooks)   │         │  (pipelines)   │
                                   └────────────────┘        └────────────────┘         └────────────────┘
```

Two binaries, one library. The compiler is heavy and runs once per release. The query library is lightweight and embedded everywhere.

---

## RF2 ingestion

### Files we care about

Standard RF2 Snapshot release contains:

- `sct2_Concept_Snapshot_*.txt` — concept IDs and active flags.
- `sct2_Description_Snapshot_*.txt` — FSN, synonyms, language.
- `sct2_Relationship_Snapshot_*.txt` — inferred relationships (use this, not stated).
- `sct2_TextDefinition_Snapshot_*.txt` — optional, for clinical definitions.
- `der2_*Refset*_Snapshot_*.txt` — refset memberships (language refset for preferred terms is essential; others optional).

For descendant queries, the **inferred is-a relationships** (typeId = `116680003`) are the key edges.

### Parsing

RF2 files are tab-separated UTF-8 with a header row. Each row has an `effectiveTime` and `active` flag. For Snapshot files, you just take rows where `active = 1`. For Full files, you need to compute the latest state per ID (group by ID, take max `effectiveTime`).

Use `csv` crate with `b'\t'` delimiter, or hand-roll a tab splitter for speed. Parsing all of SNOMED International (~1.5M descriptions, ~3M relationships in Full) takes 10–30s with a naive `csv` reader; can be brought under 5s with `memchr`-based line splitting and parallel chunk parsing via `rayon`.

### Validation

The compiler should fail loudly on:

- Inactive concepts referenced by active relationships.
- Cycles in the is-a graph (shouldn't happen but worth asserting).
- Concept IDs that fail the SCTID Verhoeff check digit.
- Duplicate active rows for the same component (indicates a malformed release).

---

## Internal representation (during compilation)

```rust
// Working data structures, before serialisation

struct ConceptMeta {
    id: u64,            // SCTID
    active: bool,
    module_id: u64,
    definition_status: DefinitionStatus,  // primitive / fully defined
    effective_time: u32, // packed YYYYMMDD
}

struct IsaEdge {
    source: u64,  // child
    target: u64,  // parent
}

struct Description {
    concept_id: u64,
    term: String,
    type_id: u64,       // FSN / synonym
    language_code: String,
    case_significance: u64,
    is_preferred_in: Vec<u64>,  // language refsets where this is preferred
}
```

Resolve SCTIDs to dense `u32` internal IDs early — this saves space everywhere downstream and makes adjacency lists much smaller. Keep a `Vec<u64>` lookup for SCTID-by-internal-id and a `HashMap<u64, u32>` for the reverse.

---

## The is-a graph

After ingestion you have a directed acyclic graph (multi-parent — SNOMED is a polyhierarchy, not a tree). Root is `138875005` (SNOMED CT Concept).

### Transitive closure

For every concept, you want to be able to ask: "what are all my descendants?" and "is X a descendant of Y?"

Three encodings to consider, in increasing cleverness:

#### 1. CSR adjacency + on-demand BFS

Store edges in compressed sparse row format:

```
parents_offsets: [u32; N+1]   // offsets into parents_data
parents_data:    [u32; E]     // parent internal IDs, sorted
children_offsets: [u32; N+1]
children_data:   [u32; E]
```

Descendants of X = BFS from X following `children_*`. Memory ~12 MB for SNOMED International (E ≈ 1.3 M is-a edges, plus headers). Per-query cost is proportional to result size — microseconds for narrow concepts, low milliseconds for "Clinical finding".

**Good baseline.** Always include this; even if you add fancier indices on top, BFS is the source of truth.

#### 2. Precomputed descendant bitmaps (Roaring)

For each concept, store a `RoaringBitmap` of all descendants:

```rust
descendants: Vec<RoaringBitmap>  // indexed by internal concept ID
```

- O(1) "is X a descendant of Y?" via `descendants[Y].contains(X)`.
- Fast set operations: intersection of two descendant sets via `&`.
- Memory: highly variable. Leaf concepts have empty bitmaps. Roots have ~360k members. Roaring compresses small dense ranges very well; expect total size 50–200 MB depending on aggression.

This is the **killer feature for cohort work**. ECL like `<< Diabetes AND << Insulin therapy` becomes two array lookups and a bitmap intersection.

Trade-off: bitmaps are scattered across the file. If your workload touches many of them, the resident working set grows. For workloads concentrated on a few hierarchies (most healthcare use cases), this is fine.

#### 3. Interval labelling (nested set / pre-post order)

DFS the hierarchy from the root, assign each node a `(pre, post)` interval. Then X is a descendant of Y iff `Y.pre < X.pre < X.post < Y.post`.

For a tree this is one interval per node and gives O(1) ancestor checks with ~8 bytes per node — ~3 MB total. Fits in L2 cache.

**SNOMED is a DAG, not a tree.** Each node gets multiple intervals, one per path from the root. Total intervals is bounded but can be large for deep multi-parent concepts. Worth measuring on the actual data. Practical implementations cap the interval count and fall back to BFS for over-cap concepts.

### Recommendation

Build all three, and let the query layer choose:

- **CSR + BFS** as the always-available fallback and source of truth.
- **Interval labels** for fast `is_descendant_of` membership where the small-cache footprint matters.
- **Roaring bitmaps** for set operations and value-set evaluation.

Total artifact size: ~200–400 MB. All three pay for themselves on different workloads.

---

## Packed file format

Single file, mmap-friendly, little-endian, alignment-aware. Design principles:

- Fixed header at offset 0 with magic bytes, version, section table.
- All sections are 4 KB aligned for clean page faulting.
- All offsets are byte offsets into the file (not pointers — keeps it relocatable).
- All integers are `u32` or `u64` natural-endian on a tagged target architecture.
- Use `rkyv`, `zerocopy`, or hand-rolled `repr(C)` structs for zero-copy access.

### Header layout

```
Offset  Size  Field
0       8     Magic: b"SNOMEDRS"
8       4     Version: u32 (e.g. 0x00010000 for v1.0)
12      4     Release date: u32 (packed YYYYMMDD)
16      4     Edition: u32 (e.g. enum InternationalEdition / UKEdition)
20      4     Concept count: u32
24      4     Section count: u32
28      36    Reserved / padding to 64 bytes
64      ...   Section table: array of SectionEntry
```

```rust
#[repr(C)]
struct SectionEntry {
    section_id: u32,   // enum: ConceptTable, IsaCSR, Bitmaps, Intervals, ...
    offset: u64,       // byte offset in file
    length: u64,       // byte length
    checksum: u32,     // CRC32 or xxhash for integrity
}
```

### Sections (typical layout)

1. **Concept table** — array of fixed-size records indexed by internal ID. Fields: SCTID, flags, definition status, interval-label range start. ~12 bytes per concept × 360k ≈ 4 MB.
2. **SCTID → internal ID hash table** — perfect hash (e.g. `boomphf` or `phf`) so concept lookup is one hash + one comparison. ~4 MB.
3. **Is-a parents CSR** — offsets + parent ID arrays.
4. **Is-a children CSR** — offsets + child ID arrays.
5. **Interval labels** — per-concept `(pre, post)` pairs, possibly multiple per concept.
6. **Descendant Roaring bitmaps** — per-concept serialised bitmaps with an offset table.
7. **Description table** — FSN + preferred term per concept (for human-readable output). Variable length, with offset table.
8. **Refset memberships** — sparse `(refset_id, concept_id)` index if needed.

### Layout ordering

Order concepts in the concept table by **DFS pre-order from the root** of the is-a hierarchy. This gives huge cache locality wins:

- Sibling concepts land on the same pages.
- Subtree iteration is sequential reads.
- Adjacent concepts in the table tend to share parents.

This single optimisation can halve cold-query latency.

---

## Query layer API

The library exposes a small, focused surface. Sketch:

```rust
pub struct SnomedArtifact {
    mmap: memmap2::Mmap,
    header: &'static Header,
    concepts: &'static [Concept],
    // ... typed views over the mmap'd file
}

impl SnomedArtifact {
    pub fn open(path: &Path) -> Result<Self>;

    // Identity
    pub fn concept(&self, sctid: u64) -> Option<ConceptRef>;
    pub fn fsn(&self, sctid: u64) -> Option<&str>;
    pub fn preferred_term(&self, sctid: u64, lang_refset: u64) -> Option<&str>;

    // Hierarchy
    pub fn parents(&self, sctid: u64) -> impl Iterator<Item = u64> + '_;
    pub fn children(&self, sctid: u64) -> impl Iterator<Item = u64> + '_;
    pub fn ancestors(&self, sctid: u64) -> impl Iterator<Item = u64> + '_;
    pub fn descendants(&self, sctid: u64) -> impl Iterator<Item = u64> + '_;

    // Fast membership
    pub fn is_descendant_of(&self, child: u64, ancestor: u64) -> bool;
    pub fn is_a(&self, sctid: u64, ancestor: u64) -> bool;  // descendant-or-self

    // Set operations (returns RoaringBitmap over internal IDs)
    pub fn descendant_set(&self, sctid: u64) -> RoaringBitmap;
    pub fn descendant_or_self_set(&self, sctid: u64) -> RoaringBitmap;

    // Bulk classification — the hot path for cohort work
    pub fn classify_batch(&self, concept_ids: &[u64], value_set: &RoaringBitmap) -> Vec<bool>;
}
```

The `&'static` lifetimes are achievable because the mmap region outlives the references; use a `self_cell` or `ouroboros` if the borrow checker complains, or just leak the mmap intentionally.

---

## Memory behaviour

The artifact is `mmap`'d; the OS pages in regions as they're touched. Practical implications:

- **Virtual size = file size** (~80–500 MB depending on which sections you include).
- **Resident size grows with working set.** A typical cohort workload settles into 50–150 MB resident after warm-up.
- **Multiple processes share pages.** 20 parallel workers all opening the same artifact use one copy of the physical memory.
- **Startup is microseconds.** No deserialisation, no index building.
- **Cold-start latency**: first query after process start may take 100 µs – 1 ms due to page faults; warm queries are sub-microsecond.

For very large workloads with random access patterns, call `madvise(MADV_RANDOM)`; for sequential walks (e.g. exporting the closure), `MADV_SEQUENTIAL`. For long-running services, `MADV_HUGEPAGE` reduces TLB pressure.

---

## Build/deployment

### Compiler invocation

```bash
snomed-compile \
    --rf2-dir ./SnomedCT_InternationalRF2_PRODUCTION_20250501T120000Z \
    --output ./snomed-int-20250501.bin \
    --edition international \
    --include-bitmaps \
    --include-descriptions en-us,en-gb
```

Runtime: 1–5 minutes for SNOMED International on a modern laptop. Most time is in transitive closure + bitmap construction.

### Versioning the artifact

The header carries the SNOMED release date and edition. Consumers should refuse to load artifacts whose `version` major number doesn't match.

Ship artifacts to S3/Azure Blob keyed by edition + release date. Consumers pin a version in their config and pull on startup (cached locally; mmap from local disk).

### CI

The compiler runs on every new RF2 release. Output is published to your artifact store. A small test suite exercises:

- Known descendant counts (e.g. `<< 73211009` should yield ~N concepts).
- Spot-check ECL-equivalent queries against Snowstorm output.
- Roundtrip: open the artifact, dump it to text, compare to RF2 source.

---

## Snowflake integration

Three patterns, increasing in sophistication:

### 1. Export the closure as Parquet

Compiler emits a Parquet file with `(ancestor_id, descendant_id)` rows. Load into a Snowflake table. Existing dbt models join against it.

This is the **highest-value first step** — gives you most of the speedup with zero changes to query patterns. The 10–30M closure rows fit comfortably in a Snowflake table and benefit from clustering on `ancestor_id`.

### 2. Snowflake external function backed by the Rust library

Stand up a small HTTP service (axum / actix) wrapping the artifact. Expose `is_descendant_of(child, ancestor) -> bool` and `descendants(ancestor) -> array<bigint>`. Register as a Snowflake external function.

Per-row latency includes network — fine for batch UDF use, less good for highly interactive queries.

### 3. Native UDF via Snowpark / Java JNI

Bundle the Rust library as a `.so` and call from a Java/Snowpark UDF. Lowest latency but most operational complexity.

For your context: **start with (1), graduate to (2) when you need ECL evaluation outside SQL.**

---

## Crate selections

Recommended dependencies:

- `memmap2` — mmap.
- `rkyv` or `zerocopy` — zero-copy deserialisation. Pick `zerocopy` if you want to hand-roll the format; `rkyv` if you want it more automatic.
- `roaring` — compressed bitmaps.
- `boomphf` or `phf` — perfect hashing for SCTID lookup.
- `csv` and `memchr` — RF2 parsing.
- `rayon` — parallel ingestion and closure computation.
- `xxhash-rust` — fast checksums.
- `bytemuck` — safe `repr(C)` casting where needed.
- `pyo3` + `maturin` — Python bindings.

Avoid: anything that requires runtime allocation per query, anything with a JIT, anything that wants `tokio` in the query path (the artifact is sync and that's a feature).

---

## Open design questions

1. **Stated vs inferred relationships.** Default to inferred (post-classifier) — that's what clinical queries want. Stated relationships are for editing tools.
2. **Module filtering at compile time vs query time.** If you only care about the International + UK Clinical Extension, filter at compile time and shrink the artifact. If you serve multi-tenant with different module sets, filter at query time.
3. **Refset handling.** Refsets can be modelled as bitmaps too — `refset_members: HashMap<RefsetId, RoaringBitmap>` makes `^ 447562003` (members of refset) into a single bitmap fetch. Cheap to include; worth doing.
4. **Concrete domain values (ECL 2.0).** SNOMED CT has numeric and string properties on some concepts. The compiler should extract these into a separate section if you plan to support ECL 2.0 concrete comparisons.
5. **Historical relationships.** "Was-a", "moved-to" associations let you answer "what is the modern equivalent of this retired concept?". Store as a small association table in a dedicated section.
6. **Description search.** Out of scope here, but worth designing space for: a separate file with a FST (fst crate) or tantivy-style inverted index over preferred terms.


## Suggested phasing

**Phase 1 — minimum viable artifact (2–3 weeks)**

- RF2 ingestion (concepts + inferred is-a only).
- Internal ID assignment + perfect hash.
- CSR adjacency.
- BFS-based descendant iteration.
- Packed file format + mmap loader.
- CLI tool exposing `descendants`, `ancestors`, `is-a`.

Goal: prove the read path works end-to-end, faster than Snowstorm on a representative workload.

**Phase 2 — bitmap acceleration (1–2 weeks)**

- Transitive closure via BFS.
- Per-concept Roaring bitmaps.
- Set operations API.
- Parquet export of the closure.

Goal: deploy the closure table into Snowflake, deprecate the existing closure computation if there is one.

**Phase 3 — descriptions and refsets (1–2 weeks)**

- Description ingestion + FSN/PT lookup.
- Language refset handling.
- Refset membership bitmaps.

Goal: artifact is self-contained for human-readable output and value-set evaluation.

**Phase 4 — ergonomics (1 week)**

- PyO3 bindings.
- Documentation, example notebooks.
- CI for monthly release ingestion.

Goal: usable by the wider data team, not just the Rust author.

Total: roughly **6–8 weeks** to a production-grade artifact, with usable output after week 3.
