# How the SNOMED CT Compiler Works

A bottom-up walkthrough for people who do not already know what any of this means.

---

## Part 1 — What is SNOMED CT, and why is it annoying to query?

SNOMED CT is a massive vocabulary of medical concepts. Things like "Type 2 diabetes mellitus", "fracture of femur", "paracetamol", "left kidney". Each concept has a unique numeric ID called an **SCTID** — for example, `73211009` is "Diabetes mellitus type 2".

The interesting part is that concepts are **organised in a hierarchy**. "Type 2 diabetes mellitus" is a kind of "Diabetes mellitus", which is a kind of "Endocrine, nutritional and metabolic disease", and so on all the way up to a single root concept (`138875005`, imaginatively named "SNOMED CT Concept").

The relationships that express this are called **is-a** relationships. They mean exactly what you would expect:

```
73211009 (Type 2 diabetes mellitus)  →is-a→  73211009's parent →is-a→ ... →is-a→ 138875005 (root)
```

The most common query in clinical informatics is: **"give me every concept that is a kind of X"**, i.e., all descendants of X. For example, "all types of diabetes" means "all descendants of 73211009 plus 73211009 itself".

### Why is this slow with conventional tools?

The obvious implementation is a database. You store all the is-a edges in a table and run a recursive query to walk the hierarchy. This works, but it is slow for the same reason that any tree traversal is slow in a database: every step of the recursion is another query, and you are paying disk I/O and query planning overhead at each level.

Tools like Snowstorm (the NHS's SNOMED server, built on Elasticsearch) typically take **20–200 milliseconds per descendant query**. For batch classification work — classifying thousands of patient records by condition — this adds up quickly.

SNOMED CT is **completely static between releases**. It updates roughly every few months, and between releases nothing changes. This is the key insight: if nothing changes, you can pay the cost of setting up a fast data structure **once** at release time, and then answer every query for free.

That is what this compiler does.

---

## Part 2 — The input: RF2 files

When you download a SNOMED release from TRUD, you get a zip that unpacks to a directory tree. Inside, under `Snapshot/Terminology/`, you will find files like:

```
sct2_Concept_Snapshot_INT_20250501.txt
sct2_Relationship_Snapshot_INT_20250501.txt
sct2_Description_Snapshot_INT_20250501.txt
```

These are plain tab-separated text files. "RF2" is just the name for this format — Release Format 2.

### The Concept file

Each row is one concept. The columns are:

```
id              effectiveTime  active  moduleId             definitionStatusId
73211009        20020131       1       900000000000207008   900000000000074008
```

- `id` — the SCTID, a large integer.
- `active` — 1 means this concept is currently in use; 0 means it has been retired.
- We only care about `active = 1` rows.

### The Relationship file

Each row is one directional edge between two concepts. The columns we care about:

```
id    effectiveTime  active  moduleId  sourceId   destinationId  ...  typeId       ...
...   20250501       1       ...       73211009   44054006       ...  116680003    ...
```

- `sourceId` — the child concept.
- `destinationId` — the parent concept.
- `typeId = 116680003` means "is-a". There are other relationship types (e.g., "has finding site") but we only use is-a for the hierarchy.
- Again, `active = 1` only.

### Snapshot vs Full

A **Snapshot** release contains the current state only — one row per component, representing the world right now.

A **Full** release contains every historical change ever made — many rows per component at different `effectiveTime` values. To get the current state from a Full file you have to find the most recent row per ID.

The compiler handles both: for each component ID it tracks the highest `effectiveTime` seen and only uses that row.

---

## Part 3 — Why remap IDs?

SCTIDs are large 64-bit integers. They are not sequential — they are scattered across a huge number space (there are about 360,000 active concepts, but the IDs run into the hundreds of millions). This matters because:

1. If you want an array indexed by concept, you cannot use SCTIDs directly — you would need an array with hundreds of millions of slots, almost all empty.
2. Doing arithmetic on SCTIDs wastes space and cache.

The solution is to assign each concept a **dense internal ID** — just integers from 0 to N−1, where N is the number of active concepts. Internally everything is done with these small integers. SCTIDs only appear when reading input or writing output.

You need two lookups:
- **SCTID → internal ID**: used when translating user input. Implemented as a sorted array with binary search (fast, compact, no hash collisions).
- **internal ID → SCTID**: just an array indexed by internal ID.

### DFS pre-order — why the ordering of IDs matters

We do not assign internal IDs in SCTID order or in random order. We assign them in **depth-first pre-order** starting from the root of the hierarchy.

To understand why, think about cache behaviour. When you query "all descendants of X", you walk a subtree. If all the concepts in that subtree have consecutive internal IDs, they are stored consecutively in memory. Reading them is fast because the CPU can prefetch ahead — it fetches one page of RAM and finds many of the concepts it needs there.

If instead the IDs were random, the concepts in a subtree would be scattered across memory. Each access would likely be in a different memory page, causing many **cache misses** — slow trips out to RAM. On a typical machine the difference between a cache hit and a cache miss is 100×.

**DFS pre-order** means: assign the root ID 0, then immediately assign IDs to its first child's entire subtree, then its second child's entire subtree, and so on. This guarantees that every subtree occupies a contiguous (or near-contiguous) range of IDs.

Here is a concrete example on a tiny hierarchy:

```
       A (id=0)
      / \
     B   C
    / \    \
   D   E    F

DFS pre-order: A=0, B=1, D=2, E=3, C=4, F=5
```

D and E are children of B, and they get IDs 2 and 3 — consecutive. If you want all descendants of B, you access ids 2 and 3 — right next to each other in any array indexed by internal ID.

The compiler does this by running an iterative DFS (using an explicit stack) from root `138875005`, assigning IDs in the order concepts are first visited.

---

## Part 4 — The graph data structure: CSR

Once we have dense internal IDs, we need to store the edges of the is-a graph efficiently. The structure used is called a **Compressed Sparse Row** (CSR) adjacency.

Imagine a very simple hierarchy:

```
Concept 0: parents = [none]       (root)
Concept 1: parents = [0]
Concept 2: parents = [0]
Concept 3: parents = [1, 2]      (multi-parent — SNOMED allows this)
```

The naive approach is to store an array of arrays:

```
parents[0] = []
parents[1] = [0]
parents[2] = [0]
parents[3] = [1, 2]
```

This is easy to work with but wasteful in memory — you need separate heap allocations for each inner array, and pointers between them. It is slow to cache and awkward to memory-map from disk.

CSR flattens this into two flat arrays:

```
offsets: [0, 0, 1, 2, 4]       (length N+1)
data:    [0, 0, 1, 2]           (all the parent IDs packed end-to-end)
```

To find the parents of concept `i`, you look at `data[offsets[i] .. offsets[i+1]]`.

For concept 3: `offsets[3]=2`, `offsets[4]=4`, so parents are `data[2..4] = [1, 2]`. Correct.

The whole structure is just two flat arrays of 32-bit integers. No pointers. No heap fragmentation. It serialises to disk by literally writing the bytes of the two arrays, and you can read it back with zero parsing overhead.

We build **two** CSR structures:
- **parents CSR** — for each concept, its direct parents (used by ancestor queries).
- **children CSR** — for each concept, its direct children (used by descendant queries).

---

## Part 5 — The binary file format

The compiler writes a single binary file. This file is designed to be **memory-mapped** and read with zero parsing overhead. Let's understand what that means.

### What is memory-mapping?

Normally when you open a file, you call `read()` which copies bytes from the file into a buffer in your program's memory. You then parse that buffer into data structures. For a 200 MB file this takes time.

**Memory-mapping** (`mmap`) is different. You ask the operating system to map the file directly into your program's address space. Your program gets a pointer to the start of the file, and the OS maps pages from the file into memory as you access them. You never explicitly read the file — you just access memory addresses, and the OS handles bringing in pages lazily as needed.

The advantages:
- **Startup is effectively instant** — no upfront reading.
- **Multiple processes share the same physical pages** — if 20 workers open the same artifact, there is only one copy in RAM.
- **The OS page cache does the caching** — if the file was recently accessed by another process, it is already in memory.

For this to work, the data structures in the file must be **directly usable** without any parsing or transformation. That means:
- Using fixed-size C-style structs (no variable-length encoding).
- Using array indices instead of pointers (pointers become invalid when the file is mapped at a different address).
- Aligning structures on natural boundaries so the CPU can read them without penalty.

### The file layout

```
Offset 0:       FileHeader (64 bytes)
Offset 64:      SectionEntry × 4  (4 × 32 = 128 bytes)
Offset 192:     padding zeros to reach the next 4096-byte boundary
Offset 4096:    Section 1 — Concept Table
Offset ???:     padding to 4096-byte boundary
Offset ???:     Section 2 — SCTID Index
...and so on
```

**Why 4096-byte alignment?** 4096 bytes is the size of a memory page on x86. By aligning each section to a page boundary, we ensure that no section straddles a page boundary unnecessarily, and that each section starts at a clean page — good for both performance and for `bytemuck`'s alignment requirements.

### The FileHeader struct

```rust
#[repr(C)]          // laid out in memory exactly as written below, no reordering
struct FileHeader {
    magic: [u8; 8],     // always b"SNOMEDRS" — lets us detect corrupted files
    version: u32,       // e.g. 0x00010000 for v1.0
    release_date: u32,  // e.g. 20250501
    edition: u32,       // 1=international, 2=uk
    concept_count: u32, // N, the number of active concepts
    section_count: u32, // how many sections follow
    _reserved: [u32; 9],
}
// Total: 8+4+4+4+4+4+36 = 64 bytes exactly.
```

The `#[repr(C)]` attribute tells Rust to lay out the struct exactly as specified, with no reordering or extra padding. Without it, Rust is free to rearrange fields for its own reasons, which would mean the bytes on disk no longer match the struct.

`bytemuck::Pod` ("Plain Old Data") is a trait we derive that proves the struct has no invalid bit patterns — i.e., any arbitrary sequence of bytes is a valid instance of this struct. That guarantee is what lets us take a raw slice of bytes from the mmap and reinterpret it directly as `&FileHeader` without any copying.

### The section table

Immediately after the header is a fixed table of `SectionEntry` structs, one per section in the file:

```rust
struct SectionEntry {
    section_id: u32,  // which section is this? (1=concept table, 2=index, etc.)
    _pad0: u32,
    offset: u64,      // where in the file does this section start?
    length: u64,      // how many bytes long is it?
    checksum: u32,    // CRC32 of the section's bytes, for integrity checking
    _pad1: u32,
}
// Total: 4+4+8+8+4+4 = 32 bytes exactly.
```

The padding fields (`_pad0`, `_pad1`) exist purely to keep the struct size a clean power of two and to keep the `u64` fields 8-byte aligned. Without the padding, `offset` would start at byte 4, which is only 4-byte aligned, and on some architectures reading misaligned `u64` values is either slow or illegal.

When the query library opens the file, it reads the section table and records the offset and length of each section it cares about. It does not yet access the section data — it just notes the addresses.

### The four sections

**Section 1: Concept Table**

An array of `ConceptRecord` structs, one per concept, indexed by internal ID:

```rust
struct ConceptRecord {
    sctid: u64,   // the original SCTID
    flags: u32,   // bit 0 = active, bit 1 = fully defined
    _pad: u32,
}
// 16 bytes each × ~360,000 concepts ≈ 5.5 MB
```

To get the SCTID of concept with internal ID 42: `concept_table[42].sctid`. This is a single array access — as fast as it gets.

**Section 2: SCTID Index**

An array of `(sctid, internal_id)` pairs, sorted by `sctid`:

```rust
struct SctidEntry {
    sctid: u64,
    internal_id: u32,
    _pad: u32,
}
// 16 bytes each × ~360,000 concepts ≈ 5.5 MB
```

To translate a user-supplied SCTID to an internal ID, we binary search this array. Binary search on a sorted array of N entries takes at most log₂(N) comparisons. For 360,000 entries that is at most 19 comparisons — always fast.

**Section 3: Parents CSR**

The flat arrays for the parents-of-X relationship, as described above. `(N+1 + E) × 4` bytes where E is the number of is-a edges (≈1.3 million for International).

**Section 4: Children CSR**

Same format, but stores the children-of-X relationship.

---

## Part 6 — The query layer: how lookups actually work

When the query library opens an artifact, it calls `mmap()` and records the section offsets. No data has been read yet — we just have a pointer to the start of the file in memory.

### Finding a concept by SCTID

```
sctid_to_id(73211009):
1. Binary search the SCTID index for 73211009.
2. Found at index pos. Return index[pos].internal_id.
```

This costs: one binary search (≈19 comparisons), all on data likely already in cache. Sub-microsecond.

### Getting all descendants

```
descendants(73211009):
1. sctid_to_id(73211009) → internal ID, say 12345.
2. BFS from 12345 using the children CSR:
   - queue = [12345], visited = {12345}
   - Pop 12345, look up children_csr.neighbors(12345) → [12346, 12400, ...]
   - Push each unvisited child, record their SCTIDs
   - Repeat until queue is empty
3. Return the collected SCTIDs.
```

The `visited` array is a plain `Vec<bool>` of length N (about 360,000 bytes — fits comfortably in CPU cache). Each concept is visited at most once. Total work is proportional to the size of the result subtree.

Because concepts in a subtree have consecutive (or near-consecutive) internal IDs (due to the DFS pre-order assignment), the children CSR accesses are largely sequential reads — cache-friendly.

### Is X a descendant of Y?

Same as above, but we stop as soon as we find Y in the traversal rather than collecting all results. If Y is an ancestor, we find it quickly. If X is in a completely different branch of the hierarchy, we exhaust the traversal and return false.

### Casting bytes to structs — how `bytemuck` works

When the query library accesses a section, it does something like:

```rust
let concept_table: &[ConceptRecord] =
    bytemuck::cast_slice(&mmap[section_offset .. section_offset + section_length]);
```

`bytemuck::cast_slice` reinterprets a `&[u8]` (a slice of raw bytes) as a `&[ConceptRecord]` (a slice of concept structs). This is valid because:
1. `ConceptRecord` is `#[repr(C)]` — its layout is fixed and predictable.
2. `ConceptRecord: Pod` — any bit pattern is valid.
3. The slice starts at a 4096-byte aligned address — satisfying alignment requirements.
4. The length is a multiple of `size_of::<ConceptRecord>()` — no partial structs at the end.

The reinterpretation is **zero cost** — no copying, no parsing. We are literally just telling Rust "treat these bytes as an array of ConceptRecord". The bytes on disk are already in exactly the right format because the compiler wrote them that way.

---

## Part 7 — End to end: what happens when you run `snomed-compile compile`?

Here is the complete sequence:

```
1.  Walk the RF2 directory tree to find sct2_Concept_* and sct2_Relationship_* files.

2.  Parse the concept file (CSV with tab delimiter):
    - For each row, record (sctid → definition_status).
    - If the same sctid appears multiple times (Full release), keep the most recent.
    - Result: HashMap<sctid, ConceptMeta> with ~360,000 entries.

3.  Parse the relationship file:
    - Keep only rows where active=1 and typeId=116680003 (is-a).
    - Result: Vec<(child_sctid, parent_sctid)> with ~1.3 million entries.

4.  Build adjacency maps:
    - parents: HashMap<sctid, Vec<sctid>>  (for each concept, its parents)
    - children: HashMap<sctid, Vec<sctid>> (for each concept, its children)
    - Both initialised for every active concept (so leaf nodes have empty children lists).

5.  Assign internal IDs:
    - DFS from root 138875005, pushing children in sorted order.
    - Assign IDs 0, 1, 2, ... in the order concepts are first popped.
    - Any concepts not reachable from the root get IDs at the end (sorted).
    - Result: id_to_sctid: Vec<u64>, sctid_to_id: HashMap<u64, u32>

6.  Build CSR arrays:
    - For each internal ID i (in order 0..N):
        parents_data.extend(parent SCTIDs of concept i, translated to internal IDs)
        parents_offsets[i+1] = parents_data.len()
    - Same for children.

7.  Build concept table: Vec<ConceptRecord> indexed by internal ID.

8.  Build SCTID index: Vec<SctidEntry> sorted by sctid.

9.  Compute CRC32 checksums for each section (for future integrity checks).

10. Compute section offsets (each starts at next 4096-byte boundary after previous ends).

11. Write the file:
    - FileHeader (64 bytes)
    - 4 × SectionEntry (128 bytes)
    - Padding zeros to reach offset 4096
    - Concept table bytes
    - Padding to next 4096-byte boundary
    - SCTID index bytes
    - Padding
    - Parents CSR bytes (offsets array then data array)
    - Padding
    - Children CSR bytes

Done. Total file size: roughly 50–100 MB for a Phase 1 artifact.
```

---

## Part 8 — What does the CLI actually do?

```bash
# Compile
snomed-compile compile \
    --rf2-dir ./SnomedCT_InternationalRF2_PRODUCTION_20250501T120000Z \
    --output  ./snomed-int-20250501.bin \
    --date    20250501

# Query: all descendants of "Diabetes mellitus" (73211009)
snomed-compile query --db ./snomed-int-20250501.bin descendants 73211009

# Is "Type 2 diabetes mellitus" a descendant of "Endocrine disease"?
snomed-compile query --db ./snomed-int-20250501.bin is-a 73211009 362969004

# Direct parents
snomed-compile query --db ./snomed-int-20250501.bin parents 73211009

# Concept info
snomed-compile query --db ./snomed-int-20250501.bin concept 73211009
```

The `query` subcommands open the artifact with `mmap`, read the header and section table, and then run the appropriate operation. The `descendants` and `ancestors` operations print one SCTID per line to stdout. Everything goes to a terminal fast.

---

## Part 9 — What is missing (future phases)

The Phase 1 artifact is already much faster than conventional tools for hierarchy traversal, but it is not complete. What comes next:

**Phase 2 — Roaring bitmaps**

Currently, "is X a descendant of Y?" requires a BFS that visits every ancestor of X. For a concept deep in the hierarchy this is fast, but for a concept near the root it could touch hundreds of thousands of concepts.

A faster approach: precompute a **bitmap of all descendants** for every concept, and store it in the artifact. Then "is X a descendant of Y?" becomes `descendant_bitmap[Y].contains(X)` — a single lookup. And "all concepts that are both a kind of Diabetes AND a kind of Insulin-treated condition" becomes `descendant_bitmap[diabetes] AND descendant_bitmap[insulin_treated]` — a bitwise AND over two compressed arrays.

Roaring bitmaps store sets of integers very efficiently, especially when the integers are clustered (which they are, thanks to the DFS pre-order ID assignment).

**Phase 3 — Descriptions and refsets**

The current artifact has SCTIDs but no human-readable names. Phase 3 adds:
- The preferred term for each concept (e.g., "Diabetes mellitus type 2" for `73211009`).
- Refset memberships — SNOMED groups concepts into named subsets (e.g., "UK SNOMED CT Clinical edition reference set"). Knowing which refset a concept belongs to is important for clinical decision support.

**Phase 4 — Python bindings**

Wrap the Rust query library in Python using PyO3, so data scientists can call it from Jupyter notebooks without leaving Python.

---

## Summary

The key ideas, in order of importance:

1. **Compile once, query forever.** SNOMED does not change between releases. Pay the setup cost once.

2. **Dense internal IDs in DFS pre-order.** Converts large sparse SCTIDs into small contiguous integers, and arranges them in memory so subtree access is sequential and cache-friendly.

3. **CSR adjacency.** Two flat arrays of 32-bit integers encoding the graph. No pointers, no heap allocations, trivially serialisable to disk.

4. **Memory-mapped binary format.** The file is the data structure. Opening it costs microseconds. Multiple processes share the same physical pages.

5. **Zero-copy struct access via `bytemuck`.** `#[repr(C)]` + `Pod` lets us reinterpret raw bytes directly as typed structs, with no parsing step.

6. **BFS on CSR.** Simple, correct, fast for the sizes involved. The foundation that fancier data structures (bitmaps, interval labels) are built on top of.
