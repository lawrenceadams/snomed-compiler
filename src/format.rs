/// Binary artifact format for the SNOMED CT compiled artifact.
///
/// Layout:
///   [0..64]       FileHeader
///   [64..64+32*N] SectionEntry × N  (N = section_count)
///   <pad to 4 KB>
///   sections, each starting on a 4 KB boundary
use bytemuck::{Pod, Zeroable};

// ── constants ───────────────────────────────────────────────────────────────

pub const MAGIC: [u8; 8] = *b"SNOMEDRS";
/// Version 1.0. Major version in upper 16 bits; consumers reject mismatches.
pub const VERSION: u32 = 0x00010000;

pub const SECTION_CONCEPT_TABLE: u32 = 1;
pub const SECTION_SCTID_INDEX: u32 = 2;
pub const SECTION_PARENTS_CSR: u32 = 3;
pub const SECTION_CHILDREN_CSR: u32 = 4;

pub const EDITION_UNKNOWN: u32 = 0;
pub const EDITION_INTERNATIONAL: u32 = 1;
pub const EDITION_UK: u32 = 2;

pub const CONCEPT_FLAG_ACTIVE: u32 = 1 << 0;
pub const CONCEPT_FLAG_FULLY_DEFINED: u32 = 1 << 1;

/// SNOMED CT root concept SCTID.
pub const SNOMED_ROOT: u64 = 138_875_005;
/// Is-a relationship type SCTID.
pub const IS_A_TYPE: u64 = 116_680_003;
/// Fully-defined definition status SCTID.
pub const FULLY_DEFINED_SCTID: u64 = 900_000_000_000_073_002;

pub const SECTION_ALIGNMENT: usize = 4096;

// ── on-disk structs ─────────────────────────────────────────────────────────

/// File header at byte offset 0. Exactly 64 bytes.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct FileHeader {
    pub magic: [u8; 8],
    pub version: u32,
    /// Release date as packed YYYYMMDD decimal, e.g. 20250501.
    pub release_date: u32,
    pub edition: u32,
    pub concept_count: u32,
    pub section_count: u32,
    pub _reserved: [u32; 9],
}

const _: () = assert!(std::mem::size_of::<FileHeader>() == 64);

/// One entry in the section table. Exactly 32 bytes.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct SectionEntry {
    pub section_id: u32,
    pub _pad0: u32,
    pub offset: u64,
    pub length: u64,
    pub checksum: u32,
    pub _pad1: u32,
}

const _: () = assert!(std::mem::size_of::<SectionEntry>() == 32);

/// One concept record, indexed by internal concept ID. Exactly 16 bytes.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct ConceptRecord {
    pub sctid: u64,
    /// Bitmask: CONCEPT_FLAG_ACTIVE | CONCEPT_FLAG_FULLY_DEFINED.
    pub flags: u32,
    pub _pad: u32,
}

const _: () = assert!(std::mem::size_of::<ConceptRecord>() == 16);

/// One entry in the sorted SCTID → internal_id lookup table. Exactly 16 bytes.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Debug)]
pub struct SctidEntry {
    pub sctid: u64,
    pub internal_id: u32,
    pub _pad: u32,
}

const _: () = assert!(std::mem::size_of::<SctidEntry>() == 16);

// ── CSR section layout ───────────────────────────────────────────────────────
//
// A CSR section with N concepts and E edges occupies exactly (N+1+E)*4 bytes:
//   offsets: [u32; N+1]   -- offsets[i]..offsets[i+1] indexes into data
//   data:    [u32; E]     -- neighbour internal IDs
//
// N is read from FileHeader.concept_count, so E = section_length/4 - (N+1).
