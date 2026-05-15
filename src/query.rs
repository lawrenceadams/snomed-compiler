use std::collections::VecDeque;
use std::path::Path;

use anyhow::{Context, Result, bail};
use memmap2::Mmap;

use crate::format::*;

// ── public types ─────────────────────────────────────────────────────────────

pub struct SnomedDb {
    mmap: Mmap,
    concept_count: usize,
    sec_concept_table: SecRange,
    sec_sctid_index: SecRange,
    sec_parents_csr: SecRange,
    sec_children_csr: SecRange,
}

#[derive(Copy, Clone)]
struct SecRange {
    offset: usize,
    length: usize,
}

pub struct ConceptInfo {
    pub sctid: u64,
    pub active: bool,
    pub fully_defined: bool,
}

// ── SnomedDb ─────────────────────────────────────────────────────────────────

impl SnomedDb {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("Opening {}", path.display()))?;
        // SAFETY: we treat the mmap as read-only and the file is not modified
        // while the Mmap is live.
        let mmap = unsafe { Mmap::map(&file) }.context("Memory-mapping artifact")?;

        let hdr_size = std::mem::size_of::<FileHeader>();
        if mmap.len() < hdr_size {
            bail!("File too small to be a valid artifact");
        }
        let header: &FileHeader = bytemuck::from_bytes(&mmap[..hdr_size]);

        if header.magic != MAGIC {
            bail!("Invalid magic bytes — not a snomed-compile artifact");
        }
        if header.version >> 16 != VERSION >> 16 {
            bail!("Incompatible artifact version {:#010x} (expected major {:04x})",
                header.version, VERSION >> 16);
        }

        let concept_count = header.concept_count as usize;
        let section_count = header.section_count as usize;
        let sec_entry_size = std::mem::size_of::<SectionEntry>();

        let mut sec_concept_table = None;
        let mut sec_sctid_index = None;
        let mut sec_parents_csr = None;
        let mut sec_children_csr = None;

        for i in 0..section_count {
            let start = hdr_size + i * sec_entry_size;
            let end = start + sec_entry_size;
            if end > mmap.len() {
                bail!("Section table extends beyond file at entry {}", i);
            }
            let entry: &SectionEntry = bytemuck::from_bytes(&mmap[start..end]);
            let range = SecRange { offset: entry.offset as usize, length: entry.length as usize };
            match entry.section_id {
                SECTION_CONCEPT_TABLE => sec_concept_table = Some(range),
                SECTION_SCTID_INDEX   => sec_sctid_index   = Some(range),
                SECTION_PARENTS_CSR   => sec_parents_csr   = Some(range),
                SECTION_CHILDREN_CSR  => sec_children_csr  = Some(range),
                other => eprintln!("Unknown section id {} — skipping", other),
            }
        }

        Ok(Self {
            mmap,
            concept_count,
            sec_concept_table: sec_concept_table.context("Missing concept table section")?,
            sec_sctid_index:   sec_sctid_index  .context("Missing SCTID index section")?,
            sec_parents_csr:   sec_parents_csr  .context("Missing parents CSR section")?,
            sec_children_csr:  sec_children_csr .context("Missing children CSR section")?,
        })
    }

    // ── typed views into the mmap ──────────────────────────────────────────

    fn concept_table(&self) -> &[ConceptRecord] {
        let r = self.sec_concept_table;
        bytemuck::cast_slice(&self.mmap[r.offset..r.offset + r.length])
    }

    fn sctid_index(&self) -> &[SctidEntry] {
        let r = self.sec_sctid_index;
        bytemuck::cast_slice(&self.mmap[r.offset..r.offset + r.length])
    }

    fn parents_csr(&self) -> Csr<'_> {
        let r = self.sec_parents_csr;
        Csr::new(&self.mmap[r.offset..r.offset + r.length], self.concept_count)
    }

    fn children_csr(&self) -> Csr<'_> {
        let r = self.sec_children_csr;
        Csr::new(&self.mmap[r.offset..r.offset + r.length], self.concept_count)
    }

    // ── SCTID ↔ internal ID ────────────────────────────────────────────────

    pub fn sctid_to_id(&self, sctid: u64) -> Option<usize> {
        let idx = self.sctid_index();
        let pos = idx.partition_point(|e| e.sctid < sctid);
        if pos < idx.len() && idx[pos].sctid == sctid {
            Some(idx[pos].internal_id as usize)
        } else {
            None
        }
    }

    fn id_to_sctid(&self, id: usize) -> u64 {
        self.concept_table()[id].sctid
    }

    // ── public query API ───────────────────────────────────────────────────

    pub fn concept(&self, sctid: u64) -> Option<ConceptInfo> {
        let id = self.sctid_to_id(sctid)?;
        let rec = &self.concept_table()[id];
        Some(ConceptInfo {
            sctid: rec.sctid,
            active: rec.flags & CONCEPT_FLAG_ACTIVE != 0,
            fully_defined: rec.flags & CONCEPT_FLAG_FULLY_DEFINED != 0,
        })
    }

    pub fn parents(&self, sctid: u64) -> Vec<u64> {
        let Some(id) = self.sctid_to_id(sctid) else { return vec![]; };
        let csr = self.parents_csr();
        csr.neighbors(id).iter().map(|&p| self.id_to_sctid(p as usize)).collect()
    }

    pub fn children(&self, sctid: u64) -> Vec<u64> {
        let Some(id) = self.sctid_to_id(sctid) else { return vec![]; };
        let csr = self.children_csr();
        csr.neighbors(id).iter().map(|&c| self.id_to_sctid(c as usize)).collect()
    }

    /// All ancestors via BFS up the parent edges (does not include `sctid`).
    pub fn ancestors(&self, sctid: u64) -> Vec<u64> {
        let Some(start) = self.sctid_to_id(sctid) else { return vec![]; };
        let csr = self.parents_csr();
        self.bfs_collect(start, &csr)
    }

    /// All descendants via BFS down the children edges (does not include `sctid`).
    pub fn descendants(&self, sctid: u64) -> Vec<u64> {
        let Some(start) = self.sctid_to_id(sctid) else { return vec![]; };
        let csr = self.children_csr();
        self.bfs_collect(start, &csr)
    }

    /// True iff `child` is a strict descendant of `ancestor`.
    pub fn is_descendant_of(&self, child: u64, ancestor: u64) -> bool {
        let Some(child_id) = self.sctid_to_id(child) else { return false; };
        let Some(anc_id) = self.sctid_to_id(ancestor) else { return false; };
        let csr = self.parents_csr();
        self.bfs_find(child_id, anc_id, &csr)
    }

    // ── internal BFS helpers ───────────────────────────────────────────────

    fn bfs_collect(&self, start: usize, csr: &Csr<'_>) -> Vec<u64> {
        let n = self.concept_count;
        let mut visited = vec![false; n];
        let mut queue = VecDeque::new();
        let mut result = Vec::new();

        visited[start] = true;
        queue.push_back(start);

        while let Some(id) = queue.pop_front() {
            for &nb in csr.neighbors(id) {
                let nb = nb as usize;
                if !visited[nb] {
                    visited[nb] = true;
                    result.push(self.id_to_sctid(nb));
                    queue.push_back(nb);
                }
            }
        }

        result
    }

    fn bfs_find(&self, start: usize, target: usize, csr: &Csr<'_>) -> bool {
        let n = self.concept_count;
        let mut visited = vec![false; n];
        let mut queue = VecDeque::new();

        visited[start] = true;
        queue.push_back(start);

        while let Some(id) = queue.pop_front() {
            for &nb in csr.neighbors(id) {
                let nb = nb as usize;
                if nb == target {
                    return true;
                }
                if !visited[nb] {
                    visited[nb] = true;
                    queue.push_back(nb);
                }
            }
        }

        false
    }
}

// ── Csr view ─────────────────────────────────────────────────────────────────

struct Csr<'a> {
    offsets: &'a [u32], // length N+1
    data: &'a [u32],    // length E
}

impl<'a> Csr<'a> {
    fn new(bytes: &'a [u8], n: usize) -> Self {
        let offsets_bytes = (n + 1) * 4;
        let offsets: &[u32] = bytemuck::cast_slice(&bytes[..offsets_bytes]);
        let data: &[u32] = bytemuck::cast_slice(&bytes[offsets_bytes..]);
        Self { offsets, data }
    }

    fn neighbors(&self, id: usize) -> &[u32] {
        let s = self.offsets[id] as usize;
        let e = self.offsets[id + 1] as usize;
        &self.data[s..e]
    }
}
