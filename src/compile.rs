use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use crc32fast::Hasher as Crc32Hasher;

use crate::format::*;

// ── public API ───────────────────────────────────────────────────────────────

pub struct CompileOptions {
    pub rf2_dir: PathBuf,
    pub output: PathBuf,
    pub release_date: u32,
    pub edition: u32,
}

pub fn compile(opts: CompileOptions) -> Result<()> {
    eprintln!("Searching for RF2 files in {:?} ...", opts.rf2_dir);

    let concept_file = find_rf2_file(&opts.rf2_dir, "sct2_Concept_")
        .context("Locating concept file")?;
    let rel_file = find_rf2_file(&opts.rf2_dir, "sct2_Relationship_")
        .context("Locating relationship file")?;

    eprintln!("  concepts:       {}", concept_file.display());
    eprintln!("  relationships:  {}", rel_file.display());

    eprintln!("Parsing concepts ...");
    let concepts = parse_concepts(&concept_file)?;
    eprintln!("  {} active concepts", concepts.len());

    eprintln!("Parsing relationships ...");
    let edges = parse_relationships(&rel_file)?;
    eprintln!("  {} active is-a edges", edges.len());

    eprintln!("Building graph ...");
    let (parents_adj, children_adj) = build_adjacency(&concepts, &edges);

    eprintln!("Assigning internal IDs (DFS pre-order from root) ...");
    let (id_to_sctid, sctid_to_id) = assign_ids(&concepts, &children_adj);
    let n = id_to_sctid.len();
    eprintln!("  {} concepts assigned IDs", n);

    eprintln!("Building CSR ...");
    let parents_csr = build_csr(n, &parents_adj, &sctid_to_id, &id_to_sctid);
    let children_csr = build_csr(n, &children_adj, &sctid_to_id, &id_to_sctid);

    let concept_table: Vec<ConceptRecord> = id_to_sctid
        .iter()
        .map(|&sctid| {
            let mut flags = CONCEPT_FLAG_ACTIVE;
            if let Some(meta) = concepts.get(&sctid) {
                if meta.definition_status == FULLY_DEFINED_SCTID {
                    flags |= CONCEPT_FLAG_FULLY_DEFINED;
                }
            }
            ConceptRecord { sctid, flags, _pad: 0 }
        })
        .collect();

    let mut sctid_index: Vec<SctidEntry> = id_to_sctid
        .iter()
        .enumerate()
        .map(|(i, &sctid)| SctidEntry { sctid, internal_id: i as u32, _pad: 0 })
        .collect();
    sctid_index.sort_by_key(|e| e.sctid);

    eprintln!("Writing {} ...", opts.output.display());
    write_artifact(&opts, n as u32, &concept_table, &sctid_index, &parents_csr, &children_csr)?;

    let path = &opts.output;
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    eprintln!("Done. Artifact size: {:.1} MB", size as f64 / 1_048_576.0);
    Ok(())
}

// ── RF2 parsing ──────────────────────────────────────────────────────────────

struct ConceptMeta {
    definition_status: u64,
}

/// For each concept ID track (effectiveTime, meta) so we handle both Snapshot
/// and Full releases correctly: in a Full release the same ID appears multiple
/// times; we keep only the most recent active state.
fn parse_concepts(path: &Path) -> Result<HashMap<u64, ConceptMeta>> {
    // (effectiveTime, Option<meta>) — None means the most-recent row was inactive.
    let mut state: HashMap<u64, (u32, Option<ConceptMeta>)> = HashMap::new();

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(path)
        .with_context(|| format!("Opening {}", path.display()))?;

    // Columns: id  effectiveTime  active  moduleId  definitionStatusId
    for result in rdr.records() {
        let rec = result.context("Reading concept record")?;
        let id: u64 = rec[0].parse().context("concept id")?;
        let et: u32 = rec[1].replace('-', "").parse().unwrap_or(0); // "20250101" or "2025-01-01"
        let active: u8 = rec[2].parse().context("active flag")?;
        let def_status: u64 = rec[4].parse().context("definitionStatusId")?;

        let entry = state.entry(id).or_insert((0, None));
        if et >= entry.0 {
            entry.0 = et;
            entry.1 = if active == 1 { Some(ConceptMeta { definition_status: def_status }) } else { None };
        }
    }

    Ok(state
        .into_iter()
        .filter_map(|(id, (_, meta))| meta.map(|m| (id, m)))
        .collect())
}

/// Returns active inferred is-a edges as (child_sctid, parent_sctid).
fn parse_relationships(path: &Path) -> Result<Vec<(u64, u64)>> {
    let mut state: HashMap<u64, (u32, Option<(u64, u64)>)> = HashMap::new();

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(path)
        .with_context(|| format!("Opening {}", path.display()))?;

    // Columns: id  effectiveTime  active  moduleId  sourceId  destinationId
    //          relationshipGroup  typeId  characteristicTypeId  modifierId
    for result in rdr.records() {
        let rec = result.context("Reading relationship record")?;
        let id: u64 = rec[0].parse().context("relationship id")?;
        let et: u32 = rec[1].replace('-', "").parse().unwrap_or(0);
        let active: u8 = rec[2].parse().context("active flag")?;
        let type_id: u64 = rec[7].parse().context("typeId")?;

        if type_id != IS_A_TYPE {
            continue;
        }

        let source: u64 = rec[4].parse().context("sourceId")?;
        let dest: u64 = rec[5].parse().context("destinationId")?;

        let entry = state.entry(id).or_insert((0, None));
        if et >= entry.0 {
            entry.0 = et;
            entry.1 = if active == 1 { Some((source, dest)) } else { None };
        }
    }

    Ok(state.into_values().filter_map(|(_, e)| e).collect())
}

// ── graph construction ───────────────────────────────────────────────────────

fn build_adjacency(
    concepts: &HashMap<u64, ConceptMeta>,
    edges: &[(u64, u64)],
) -> (HashMap<u64, Vec<u64>>, HashMap<u64, Vec<u64>>) {
    let mut parents: HashMap<u64, Vec<u64>> = concepts.keys().map(|&k| (k, vec![])).collect();
    let mut children: HashMap<u64, Vec<u64>> = concepts.keys().map(|&k| (k, vec![])).collect();

    for &(child, parent) in edges {
        if concepts.contains_key(&child) && concepts.contains_key(&parent) {
            parents.entry(child).or_default().push(parent);
            children.entry(parent).or_default().push(child);
        }
    }

    // Sort for deterministic output.
    for v in parents.values_mut() {
        v.sort_unstable();
    }
    for v in children.values_mut() {
        v.sort_unstable();
    }

    (parents, children)
}

/// Assigns dense u32 internal IDs starting with a DFS pre-order walk from the
/// SNOMED root.  Any concepts not reachable from the root are appended sorted.
fn assign_ids(
    concepts: &HashMap<u64, ConceptMeta>,
    children_adj: &HashMap<u64, Vec<u64>>,
) -> (Vec<u64>, HashMap<u64, u32>) {
    let mut id_to_sctid: Vec<u64> = Vec::with_capacity(concepts.len());
    let mut sctid_to_id: HashMap<u64, u32> = HashMap::with_capacity(concepts.len());
    let mut visited: HashSet<u64> = HashSet::with_capacity(concepts.len());

    // Iterative DFS pre-order.  Push children in reverse sorted order so the
    // first child is popped next, preserving sorted sibling order.
    let mut stack = vec![SNOMED_ROOT];
    while let Some(sctid) = stack.pop() {
        if visited.contains(&sctid) {
            continue;
        }
        if !concepts.contains_key(&sctid) {
            continue;
        }
        visited.insert(sctid);
        let id = id_to_sctid.len() as u32;
        sctid_to_id.insert(sctid, id);
        id_to_sctid.push(sctid);

        if let Some(children) = children_adj.get(&sctid) {
            for &child in children.iter().rev() {
                if !visited.contains(&child) {
                    stack.push(child);
                }
            }
        }
    }

    // Concepts unreachable from root (e.g. in subset releases without the root).
    let mut orphans: Vec<u64> = concepts
        .keys()
        .filter(|&&s| !sctid_to_id.contains_key(&s))
        .copied()
        .collect();
    orphans.sort_unstable();

    if !orphans.is_empty() {
        eprintln!(
            "  {} concepts not reachable from root (orphans); appending after DFS concepts",
            orphans.len()
        );
        for sctid in orphans {
            let id = id_to_sctid.len() as u32;
            sctid_to_id.insert(sctid, id);
            id_to_sctid.push(sctid);
        }
    }

    (id_to_sctid, sctid_to_id)
}

/// Builds a CSR representation for the given adjacency map.
/// Returns (offsets: Vec<u32> of length n+1, data: Vec<u32> of length E).
fn build_csr(
    n: usize,
    adj: &HashMap<u64, Vec<u64>>,
    sctid_to_id: &HashMap<u64, u32>,
    id_to_sctid: &[u64],
) -> (Vec<u32>, Vec<u32>) {
    let mut offsets = vec![0u32; n + 1];
    let mut data: Vec<u32> = Vec::new();

    for i in 0..n {
        let sctid = id_to_sctid[i];
        if let Some(neighbors) = adj.get(&sctid) {
            for &nb_sctid in neighbors {
                if let Some(&nb_id) = sctid_to_id.get(&nb_sctid) {
                    data.push(nb_id);
                }
            }
        }
        offsets[i + 1] = data.len() as u32;
    }

    (offsets, data)
}

// ── binary output ────────────────────────────────────────────────────────────

fn write_artifact(
    opts: &CompileOptions,
    concept_count: u32,
    concept_table: &[ConceptRecord],
    sctid_index: &[SctidEntry],
    parents_csr: &(Vec<u32>, Vec<u32>),
    children_csr: &(Vec<u32>, Vec<u32>),
) -> Result<()> {
    let n = concept_count as usize;

    let ct_size = n * size_of::<ConceptRecord>();
    let si_size = n * size_of::<SctidEntry>();
    let pc_size = (parents_csr.0.len() + parents_csr.1.len()) * 4;
    let cc_size = (children_csr.0.len() + children_csr.1.len()) * 4;

    // Header area: FileHeader + 4 × SectionEntry
    let header_end = size_of::<FileHeader>() + 4 * size_of::<SectionEntry>();

    let off1 = align_up(header_end, SECTION_ALIGNMENT) as u64;
    let off2 = align_up(off1 as usize + ct_size, SECTION_ALIGNMENT) as u64;
    let off3 = align_up(off2 as usize + si_size, SECTION_ALIGNMENT) as u64;
    let off4 = align_up(off3 as usize + pc_size, SECTION_ALIGNMENT) as u64;

    let crc_ct = crc32(bytemuck::cast_slice(concept_table));
    let crc_si = crc32(bytemuck::cast_slice(sctid_index));
    let crc_pc = crc32_pair(bytemuck::cast_slice(&parents_csr.0), bytemuck::cast_slice(&parents_csr.1));
    let crc_cc = crc32_pair(bytemuck::cast_slice(&children_csr.0), bytemuck::cast_slice(&children_csr.1));

    let header = FileHeader {
        magic: MAGIC,
        version: VERSION,
        release_date: opts.release_date,
        edition: opts.edition,
        concept_count,
        section_count: 4,
        _reserved: [0; 9],
    };

    let sections = [
        SectionEntry { section_id: SECTION_CONCEPT_TABLE, _pad0: 0, offset: off1, length: ct_size as u64, checksum: crc_ct, _pad1: 0 },
        SectionEntry { section_id: SECTION_SCTID_INDEX,   _pad0: 0, offset: off2, length: si_size as u64, checksum: crc_si, _pad1: 0 },
        SectionEntry { section_id: SECTION_PARENTS_CSR,   _pad0: 0, offset: off3, length: pc_size as u64, checksum: crc_pc, _pad1: 0 },
        SectionEntry { section_id: SECTION_CHILDREN_CSR,  _pad0: 0, offset: off4, length: cc_size as u64, checksum: crc_cc, _pad1: 0 },
    ];

    let file = File::create(&opts.output)
        .with_context(|| format!("Creating {}", opts.output.display()))?;
    let mut w = BufWriter::new(file);

    w.write_all(bytemuck::bytes_of(&header))?;
    for sec in &sections {
        w.write_all(bytemuck::bytes_of(sec))?;
    }

    pad(&mut w, off1 as usize - header_end)?;

    w.write_all(bytemuck::cast_slice(concept_table))?;
    pad(&mut w, off2 as usize - off1 as usize - ct_size)?;

    w.write_all(bytemuck::cast_slice(sctid_index))?;
    pad(&mut w, off3 as usize - off2 as usize - si_size)?;

    write_u32s(&mut w, &parents_csr.0)?;
    write_u32s(&mut w, &parents_csr.1)?;
    pad(&mut w, off4 as usize - off3 as usize - pc_size)?;

    write_u32s(&mut w, &children_csr.0)?;
    write_u32s(&mut w, &children_csr.1)?;

    w.flush()?;
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

fn pad(w: &mut impl Write, n: usize) -> std::io::Result<()> {
    const ZEROS: [u8; 4096] = [0; 4096];
    let mut rem = n;
    while rem > 0 {
        let chunk = rem.min(ZEROS.len());
        w.write_all(&ZEROS[..chunk])?;
        rem -= chunk;
    }
    Ok(())
}

fn write_u32s(w: &mut impl Write, data: &[u32]) -> std::io::Result<()> {
    w.write_all(bytemuck::cast_slice(data))
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = Crc32Hasher::new();
    h.update(data);
    h.finalize()
}

fn crc32_pair(a: &[u8], b: &[u8]) -> u32 {
    let mut h = Crc32Hasher::new();
    h.update(a);
    h.update(b);
    h.finalize()
}

/// Walk a directory tree and return the path of the first file whose name
/// starts with `prefix` and ends with `.txt`.
fn find_rf2_file(root: &Path, prefix: &str) -> Result<PathBuf> {
    let mut found = None;
    walk(root, prefix, &mut found)
        .with_context(|| format!("Searching {}", root.display()))?;
    found.ok_or_else(|| anyhow!("No RF2 file with prefix {:?} found under {}", prefix, root.display()))
}

fn walk(dir: &Path, prefix: &str, found: &mut Option<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(&entry.path(), prefix, found)?;
            if found.is_some() {
                return Ok(());
            }
        } else {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with(prefix) && s.ends_with(".txt") {
                *found = Some(entry.path());
                return Ok(());
            }
        }
    }
    Ok(())
}

fn size_of<T>() -> usize {
    std::mem::size_of::<T>()
}
