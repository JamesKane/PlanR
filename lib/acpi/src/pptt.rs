//! Processor Properties Topology Table (ACPI 6.5 §5.2.30): the processor
//! hierarchy (packages, clusters, cores, threads) and the caches each
//! level owns.
//!
//! Nodes refer to each other by their byte offset from the start of the
//! table. Every walk is bounded, so a cyclic table ends the walk instead of
//! looping.

use crate::{Sdt, u16_at, u32_at};

/// The deepest hierarchy (or cache chain) a walk follows.
const MAX_DEPTH: usize = 16;

/// A processor hierarchy node (type 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessorNode<'a> {
    /// Byte offset of the node in the table; other nodes refer to it by this.
    pub offset: u32,
    pub flags: ProcessorFlags,
    /// The parent node's offset; `None` at the root.
    pub parent: Option<u32>,
    /// Matches a MADT processor's UID, when [`ProcessorFlags::id_valid`].
    pub acpi_id: u32,
    resources: &'a [u8],
}

impl ProcessorNode<'_> {
    /// Offsets of the node's private resources (caches, in practice).
    pub fn resources(&self) -> impl Iterator<Item = u32> + '_ {
        self.resources.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c))
    }
}

/// A processor hierarchy node's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessorFlags(pub u32);

impl ProcessorFlags {
    /// The node is a physical package (a socket).
    #[must_use]
    pub fn physical_package(self) -> bool {
        self.0 & 1 != 0
    }

    /// `acpi_id` names a processor.
    #[must_use]
    pub fn id_valid(self) -> bool {
        self.0 & 2 != 0
    }

    /// The node is a hardware thread (an SMT sibling).
    #[must_use]
    pub fn thread(self) -> bool {
        self.0 & 4 != 0
    }

    /// The node has no children.
    #[must_use]
    pub fn leaf(self) -> bool {
        self.0 & 8 != 0
    }

    /// Every child implements the same processor (the same core class).
    #[must_use]
    pub fn identical(self) -> bool {
        self.0 & 16 != 0
    }
}

/// What a cache holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Data,
    Instruction,
    Unified,
}

/// A cache type structure (type 1). Each field is `None` unless the
/// table's flags mark it valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheNode {
    pub offset: u32,
    /// The next cache out from the processor, by offset.
    pub next_level: Option<u32>,
    /// Bytes.
    pub size: Option<u32>,
    pub sets: Option<u32>,
    pub associativity: Option<u8>,
    pub kind: Option<CacheKind>,
    pub write_back: Option<bool>,
    pub line_size: Option<u16>,
    /// A unique cache ID (table revision 3).
    pub id: Option<u32>,
}

/// One PPTT structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpttNode<'a> {
    Processor(ProcessorNode<'a>),
    Cache(CacheNode),
    /// A structure type this parser does not know, such as the deprecated
    /// ID structure (type 2).
    Other {
        offset: u32,
        ty: u8,
    },
}

/// Processor Properties Topology Table.
#[derive(Debug, Clone, Copy)]
pub struct Pptt<'a>(Sdt<'a>);

impl<'a> Pptt<'a> {
    pub(crate) fn new(t: Sdt<'a>) -> Self {
        Pptt(t)
    }

    /// Every structure, in table order.
    pub fn nodes(&self) -> impl Iterator<Item = PpttNode<'a>> + 'a {
        let t = *self;
        let mut off = 36usize;
        core::iter::from_fn(move || {
            let len = usize::from(*t.0.bytes().get(off + 1)?);
            if len < 4 {
                return None;
            }
            let node = t.node_at(u32::try_from(off).ok()?)?;
            off += len;
            Some(node)
        })
    }

    /// The structure at byte offset `offset`.
    #[must_use]
    pub fn node_at(&self, offset: u32) -> Option<PpttNode<'a>> {
        let b = self.0.bytes();
        let start = usize::try_from(offset).ok().filter(|&o| o >= 36)?;
        let len = usize::from(*b.get(start + 1)?);
        let n = b.get(start..start + len).filter(|_| len >= 4)?;
        let nonzero = |v: u32| (v != 0).then_some(v);
        Some(match n[0] {
            0 => {
                let count = u32_at(n, 16)? as usize;
                let resources = n.get(20..20 + count.checked_mul(4)?)?;
                PpttNode::Processor(ProcessorNode {
                    offset,
                    flags: ProcessorFlags(u32_at(n, 4)?),
                    parent: nonzero(u32_at(n, 8)?),
                    acpi_id: u32_at(n, 12)?,
                    resources,
                })
            }
            1 => {
                let flags = u32_at(n, 4)?;
                let valid = |bit: u32| flags & (1 << bit) != 0;
                let attrs = *n.get(21)?;
                PpttNode::Cache(CacheNode {
                    offset,
                    next_level: nonzero(u32_at(n, 8)?),
                    size: valid(0).then(|| u32_at(n, 12)).flatten(),
                    sets: valid(1).then(|| u32_at(n, 16)).flatten(),
                    associativity: valid(2).then_some(n[20]),
                    kind: valid(4).then_some(match (attrs >> 2) & 3 {
                        0 => CacheKind::Data,
                        1 => CacheKind::Instruction,
                        _ => CacheKind::Unified,
                    }),
                    write_back: valid(5).then_some(attrs & 0x10 == 0),
                    line_size: valid(6).then(|| u16_at(n, 22)).flatten(),
                    id: valid(7).then(|| u32_at(n, 24)).flatten(),
                })
            }
            ty => PpttNode::Other { offset, ty },
        })
    }

    /// The processor node at `offset`.
    #[must_use]
    pub fn processor_at(&self, offset: u32) -> Option<ProcessorNode<'a>> {
        match self.node_at(offset)? {
            PpttNode::Processor(p) => Some(p),
            _ => None,
        }
    }

    /// The cache node at `offset`.
    #[must_use]
    pub fn cache_at(&self, offset: u32) -> Option<CacheNode> {
        match self.node_at(offset)? {
            PpttNode::Cache(c) => Some(c),
            _ => None,
        }
    }

    /// The leaf node for the processor whose MADT UID is `acpi_id`.
    #[must_use]
    pub fn processor(&self, acpi_id: u32) -> Option<ProcessorNode<'a>> {
        self.nodes().find_map(|n| match n {
            PpttNode::Processor(p) if p.flags.id_valid() && p.acpi_id == acpi_id && p.flags.leaf() => Some(p),
            _ => None,
        })
    }

    /// `node` and its ancestors, innermost first: thread, core, cluster,
    /// package, as the table nests them.
    pub fn path(&self, node: ProcessorNode<'a>) -> impl Iterator<Item = ProcessorNode<'a>> + 'a {
        let t = *self;
        core::iter::successors(Some(node), move |n| t.processor_at(n.parent?)).take(MAX_DEPTH)
    }

    /// The physical package containing `node`.
    #[must_use]
    pub fn package(&self, node: ProcessorNode<'a>) -> Option<ProcessorNode<'a>> {
        self.path(node).find(|n| n.flags.physical_package())
    }

    /// Every cache a processor can use, with its level: the caches each
    /// node on its path owns and the chains they link to, each once.
    /// Levels count outwards. A node's own caches sit one level beyond the
    /// outermost found nearer the processor, unless a nearer cache links to
    /// it, which places it one level beyond that cache.
    pub fn caches(&self, node: ProcessorNode<'a>) -> impl Iterator<Item = (u8, CacheNode)> + 'a {
        let mut found: [(u8, Option<CacheNode>); MAX_DEPTH * 2] = [(0, None); MAX_DEPTH * 2];
        let mut n = 0;
        let mut outer = 0u8;
        for p in self.path(node) {
            let base = outer;
            for r in p.resources() {
                let mut level = base + 1;
                let mut next = self.cache_at(r);
                for _ in 0..MAX_DEPTH {
                    let Some(c) = next else { break };
                    match found[..n]
                        .iter()
                        .position(|(_, f)| f.is_some_and(|f| f.offset == c.offset))
                    {
                        // Already placed through a nearer link: carry on
                        // from its level.
                        Some(i) => level = found[i].0,
                        None if n < found.len() => {
                            found[n] = (level, Some(c));
                            n += 1;
                        }
                        None => break,
                    }
                    outer = outer.max(level);
                    level += 1;
                    next = c.next_level.and_then(|o| self.cache_at(o));
                }
            }
        }
        found.into_iter().take(n).filter_map(|(l, c)| Some((l, c?)))
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures are tiny
mod tests {
    use super::*;
    use crate::Acpi;
    use crate::testmem::*;

    /// A PPTT structure for [`pptt`]. References are indices into the
    /// structure list, resolved to table offsets.
    enum S {
        Proc {
            flags: u32,
            parent: Option<usize>,
            id: u32,
            res: Vec<usize>,
        },
        Cache {
            next: Option<usize>,
            size: u32,
            kind: u8,
            line: u16,
        },
    }

    fn len(s: &S) -> usize {
        match s {
            S::Proc { res, .. } => 20 + 4 * res.len(),
            S::Cache { .. } => 28,
        }
    }

    fn pptt(structs: &[S]) -> Vec<u8> {
        let mut offs = vec![];
        let mut at = 36;
        for s in structs {
            offs.push(at as u32);
            at += len(s);
        }
        let mut body = vec![];
        for s in structs {
            match s {
                S::Proc { flags, parent, id, res } => {
                    body.extend([0, len(s) as u8, 0, 0]);
                    body.extend(flags.to_le_bytes());
                    body.extend(parent.map_or(0, |p| offs[p]).to_le_bytes());
                    body.extend(id.to_le_bytes());
                    body.extend((res.len() as u32).to_le_bytes());
                    for r in res {
                        body.extend(offs[*r].to_le_bytes());
                    }
                }
                S::Cache { next, size, kind, line } => {
                    body.extend([1, 28, 0, 0]);
                    body.extend(0xffu32.to_le_bytes()); // every field valid
                    body.extend(next.map_or(0, |n| offs[n]).to_le_bytes());
                    body.extend(size.to_le_bytes());
                    body.extend(64u32.to_le_bytes()); // sets
                    body.extend([8, kind << 2]); // associativity, attributes
                    body.extend(line.to_le_bytes());
                    body.extend(0u32.to_le_bytes()); // cache ID
                }
            }
        }
        sdt(*b"PPTT", 3, &body)
    }

    const PACKAGE: u32 = 1;
    const ID_VALID: u32 = 2;
    const LEAF: u32 = 8;

    /// One package of two clusters, each of two cores with private L1I/L1D
    /// that link to a per-cluster L2; the package owns an L3.
    fn two_clusters() -> Vec<u8> {
        use S::{Cache, Proc};
        pptt(&[
            /* 0 */
            Proc {
                flags: PACKAGE,
                parent: None,
                id: 0,
                res: vec![1],
            },
            /* 1 */
            Cache {
                next: None,
                size: 8 << 20,
                kind: 2,
                line: 64,
            },
            /* 2 */
            Proc {
                flags: 0,
                parent: Some(0),
                id: 0,
                res: vec![3],
            },
            /* 3 */
            Cache {
                next: None,
                size: 512 << 10,
                kind: 2,
                line: 64,
            },
            /* 4 */
            Cache {
                next: Some(3),
                size: 64 << 10,
                kind: 0,
                line: 64,
            },
            /* 5 */
            Cache {
                next: Some(3),
                size: 64 << 10,
                kind: 1,
                line: 64,
            },
            /* 6 */
            Proc {
                flags: ID_VALID | LEAF,
                parent: Some(2),
                id: 0,
                res: vec![4, 5],
            },
            /* 7 */
            Proc {
                flags: ID_VALID | LEAF,
                parent: Some(2),
                id: 1,
                res: vec![4, 5],
            },
            /* 8 */
            Proc {
                flags: 0,
                parent: Some(0),
                id: 1,
                res: vec![],
            },
            /* 9 */
            Proc {
                flags: ID_VALID | LEAF,
                parent: Some(8),
                id: 2,
                res: vec![],
            },
        ])
    }

    #[test]
    fn processor_path_and_package() {
        let m = system(&[two_clusters()]);
        let p = Acpi::new(&m, 0x1000).unwrap().pptt().unwrap();
        assert_eq!(p.nodes().count(), 10);
        let cpu1 = p.processor(1).unwrap();
        assert!(cpu1.flags.leaf() && !cpu1.flags.thread());
        let path: Vec<_> = p.path(cpu1).map(|n| n.offset).collect();
        assert_eq!(path.len(), 3);
        let package = p.package(cpu1).unwrap();
        assert_eq!(package.offset, *path.last().unwrap());
        assert_eq!(package.parent, None);
        // CPU 2 is in the other cluster of the same package.
        let cpu2 = p.processor(2).unwrap();
        assert_ne!(p.path(cpu2).nth(1), p.path(cpu1).nth(1));
        assert_eq!(p.package(cpu2), Some(package));
        assert!(p.processor(9).is_none());
    }

    #[test]
    fn cache_levels() {
        let m = system(&[two_clusters()]);
        let p = Acpi::new(&m, 0x1000).unwrap().pptt().unwrap();
        let caches: Vec<_> = p
            .caches(p.processor(0).unwrap())
            .map(|(level, c)| (level, c.kind.unwrap(), c.size.unwrap()))
            .collect();
        assert_eq!(
            caches,
            [
                (1, CacheKind::Data, 64 << 10),
                (2, CacheKind::Unified, 512 << 10),
                (1, CacheKind::Instruction, 64 << 10),
                (3, CacheKind::Unified, 8 << 20),
            ]
        );
        // A core with no private caches still sees the package's L3, at
        // level 1 since nothing nearer exists.
        let l: Vec<_> = p.caches(p.processor(2).unwrap()).map(|(l, _)| l).collect();
        assert_eq!(l, [1]);
        let c = p.caches(p.processor(0).unwrap()).next().unwrap().1;
        assert_eq!(
            (c.line_size, c.associativity, c.write_back),
            (Some(64), Some(8), Some(true))
        );
    }

    #[test]
    fn cyclic_parent_ends_walk() {
        use S::Proc;
        // Two nodes that name each other as parent.
        let m = system(&[pptt(&[
            Proc {
                flags: ID_VALID | LEAF,
                parent: Some(1),
                id: 7,
                res: vec![],
            },
            Proc {
                flags: 0,
                parent: Some(0),
                id: 0,
                res: vec![],
            },
        ])]);
        let p = Acpi::new(&m, 0x1000).unwrap().pptt().unwrap();
        let cpu = p.processor(7).unwrap();
        assert_eq!(p.path(cpu).count(), MAX_DEPTH);
        assert!(p.package(cpu).is_none());
    }

    #[test]
    fn bad_references_are_ignored() {
        let m = system(&[two_clusters()]);
        let p = Acpi::new(&m, 0x1000).unwrap().pptt().unwrap();
        assert!(p.node_at(0).is_none()); // inside the header
        assert!(p.node_at(0x10_0000).is_none()); // past the end
        assert!(p.cache_at(36).is_none()); // a processor, not a cache
    }
}
