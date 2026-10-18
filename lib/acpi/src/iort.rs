//! I/O Remapping Table (Arm DEN 0049, revision E.f): how device IDs flow
//! from PCI root complexes and named components through SMMUs to GIC ITS
//! groups.
//!
//! Each node maps ranges of input IDs (requester IDs, stream IDs) to
//! output IDs at another node, named by its byte offset in the table.
//! Following the mappings from a device gives its SMMU stream ID and its
//! ITS device ID. Every walk is bounded, so a cyclic table ends it.

use crate::{Sdt, u16_at, u32_at, u64_at};

/// The longest chain of mappings a walk follows.
const MAX_HOPS: usize = 16;

/// One ID range mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMapping {
    pub input_base: u32,
    /// How many IDs the range holds. (The table stores one less.)
    pub count: u64,
    pub output_base: u32,
    /// The node the IDs map to, by offset.
    pub output: u32,
    /// The mapping is for the node's own ID (an SMMU's or a named
    /// component's MSIs, say), not for IDs passing through it.
    pub single: bool,
}

impl IdMapping {
    fn parse(m: &[u8]) -> Option<IdMapping> {
        Some(IdMapping {
            input_base: u32_at(m, 0)?,
            count: u64::from(u32_at(m, 4)?) + 1,
            output_base: u32_at(m, 8)?,
            output: u32_at(m, 12)?,
            single: u32_at(m, 16)? & 1 != 0,
        })
    }

    /// The output ID for input `id`, if the range covers it.
    #[must_use]
    pub fn map(&self, id: u32) -> Option<u32> {
        let delta = id.checked_sub(self.input_base)?;
        if u64::from(delta) >= self.count {
            return None;
        }
        self.output_base.checked_add(delta)
    }
}

/// The ITS identifiers of an ITS group: they match the MADT's GIC ITS
/// entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItsIds<'a>(&'a [u8]);

impl ItsIds<'_> {
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c))
    }
}

/// What an IORT node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IortKind<'a> {
    ItsGroup(ItsIds<'a>),
    /// A device named in the ACPI namespace.
    NamedComponent {
        /// The device object's path, e.g. `\_SB.GPU0`.
        name: &'a str,
        coherent: bool,
        /// How many address bits the device can drive; 0 if unknown.
        address_bits: u8,
    },
    RootComplex {
        segment: u32,
        coherent: bool,
        /// The root complex supports PCI Express address translation
        /// services.
        ats: bool,
        address_bits: u8,
    },
    SmmuV1V2 {
        base: u64,
        span: u64,
        model: u32,
    },
    SmmuV3 {
        base: u64,
        model: u32,
        coherent: bool,
        event_gsiv: u32,
        pri_gsiv: u32,
        gerr_gsiv: u32,
        sync_gsiv: u32,
    },
    /// A performance monitor counter group for an SMMU.
    Pmcg {
        base: u64,
        overflow_gsiv: u32,
        /// The node it monitors, by offset.
        node: u32,
    },
    Other(u8),
}

/// One IORT node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IortNode<'a> {
    /// Byte offset of the node in the table; mappings refer to it by this.
    pub offset: u32,
    pub revision: u8,
    /// Unique among the table's nodes (revision E.b and later).
    pub identifier: u32,
    pub kind: IortKind<'a>,
    mappings: &'a [u8],
}

impl IortNode<'_> {
    pub fn mappings(&self) -> impl Iterator<Item = IdMapping> + '_ {
        self.mappings
            .as_chunks::<20>()
            .0
            .iter()
            .filter_map(|m| IdMapping::parse(m))
    }

    /// True for SMMUs of either generation.
    #[must_use]
    pub fn is_smmu(&self) -> bool {
        matches!(self.kind, IortKind::SmmuV1V2 { .. } | IortKind::SmmuV3 { .. })
    }

    /// True for an ITS group.
    #[must_use]
    pub fn is_its_group(&self) -> bool {
        matches!(self.kind, IortKind::ItsGroup(_))
    }
}

/// A NUL-terminated name, without the terminator.
fn c_str(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).unwrap_or("")
}

/// I/O Remapping Table.
#[derive(Debug, Clone, Copy)]
pub struct Iort<'a>(Sdt<'a>);

impl<'a> Iort<'a> {
    pub(crate) fn new(t: Sdt<'a>) -> Self {
        Iort(t)
    }

    /// Every node, in table order.
    pub fn nodes(&self) -> impl Iterator<Item = IortNode<'a>> + 'a {
        let t = *self;
        let b = self.0.bytes();
        let count = u32_at(b, 36).unwrap_or(0);
        let mut off = u32_at(b, 40).unwrap_or(0);
        (0..count).map_while(move |_| {
            let node = t.node_at(off)?;
            let len = u32::from(u16_at(b, usize::try_from(off).ok()? + 1)?);
            off = off.checked_add(len)?;
            Some(node)
        })
    }

    /// The node at byte offset `offset`.
    #[must_use]
    pub fn node_at(&self, offset: u32) -> Option<IortNode<'a>> {
        let b = self.0.bytes();
        let start = usize::try_from(offset).ok().filter(|&o| o >= 48)?;
        let len = usize::from(u16_at(b, start + 1)?);
        let n = b.get(start..start + len).filter(|_| len >= 16)?;
        let revision = n[3];
        let nmap = u32_at(n, 8)? as usize;
        let map_off = u32_at(n, 12)? as usize;
        let mappings = if nmap == 0 {
            &[][..]
        } else {
            n.get(map_off..map_off.checked_add(nmap.checked_mul(20)?)?)?
        };
        let kind = match n[0] {
            0 => {
                let count = u32_at(n, 16)? as usize;
                IortKind::ItsGroup(ItsIds(n.get(20..20 + count.checked_mul(4)?)?))
            }
            1 => IortKind::NamedComponent {
                coherent: u32_at(n, 20)? == 1,
                address_bits: *n.get(28)?,
                name: c_str(n.get(29..)?),
            },
            2 => IortKind::RootComplex {
                coherent: u32_at(n, 16)? == 1,
                ats: u32_at(n, 24)? & 1 != 0,
                segment: u32_at(n, 28)?,
                address_bits: if revision >= 1 { *n.get(32)? } else { 0 },
            },
            3 => IortKind::SmmuV1V2 {
                base: u64_at(n, 16)?,
                span: u64_at(n, 24)?,
                model: u32_at(n, 32)?,
            },
            4 => IortKind::SmmuV3 {
                base: u64_at(n, 16)?,
                coherent: u32_at(n, 24)? & 1 != 0,
                model: u32_at(n, 40)?,
                event_gsiv: u32_at(n, 44)?,
                pri_gsiv: u32_at(n, 48)?,
                gerr_gsiv: u32_at(n, 52)?,
                sync_gsiv: u32_at(n, 56)?,
            },
            5 => IortKind::Pmcg {
                base: u64_at(n, 16)?,
                overflow_gsiv: u32_at(n, 24)?,
                node: u32_at(n, 28)?,
            },
            ty => IortKind::Other(ty),
        };
        Some(IortNode {
            offset,
            revision,
            identifier: u32_at(n, 4)?,
            kind,
            mappings,
        })
    }

    /// The root complex for PCI segment `segment`.
    #[must_use]
    pub fn root_complex(&self, segment: u32) -> Option<IortNode<'a>> {
        self.nodes()
            .find(|n| matches!(n.kind, IortKind::RootComplex { segment: s, .. } if s == segment))
    }

    /// The named component for the device at ACPI path `name`.
    #[must_use]
    pub fn named_component(&self, name: &str) -> Option<IortNode<'a>> {
        self.nodes()
            .find(|n| matches!(n.kind, IortKind::NamedComponent { name: m, .. } if m == name))
    }

    /// One step: where `node` sends input `id`, by the first mapping that
    /// covers it. On a device (a root complex or named component) a single
    /// mapping covers every input; on an SMMU or PMCG it names the node's
    /// own ID instead ([`Iort::own_id`]) and is skipped.
    #[must_use]
    pub fn map_id(&self, node: &IortNode<'a>, id: u32) -> Option<(IortNode<'a>, u32)> {
        let device = matches!(
            node.kind,
            IortKind::RootComplex { .. } | IortKind::NamedComponent { .. }
        );
        let (out, m) = node.mappings().find_map(|m| {
            if !m.single {
                Some((m.map(id)?, m))
            } else if device {
                Some((m.output_base, m))
            } else {
                None
            }
        })?;
        Some((self.node_at(m.output)?, out))
    }

    /// Where a node's own ID goes (an SMMU's MSIs, say): its single mapping.
    #[must_use]
    pub fn own_id(&self, node: &IortNode<'a>) -> Option<(IortNode<'a>, u32)> {
        let m = node.mappings().find(|m| m.single)?;
        Some((self.node_at(m.output)?, m.output_base))
    }

    /// Follows `id` from `node` until it reaches a node `stop` accepts.
    pub fn resolve(
        &self,
        node: &IortNode<'a>,
        id: u32,
        stop: impl Fn(&IortNode<'a>) -> bool,
    ) -> Option<(IortNode<'a>, u32)> {
        let mut at = (*node, id);
        for _ in 0..MAX_HOPS {
            at = self.map_id(&at.0, at.1)?;
            if stop(&at.0) {
                return Some(at);
            }
        }
        None
    }

    /// The SMMU in front of a device and the device's stream ID there;
    /// `None` if the device bypasses every SMMU.
    #[must_use]
    pub fn stream_id(&self, node: &IortNode<'a>, id: u32) -> Option<(IortNode<'a>, u32)> {
        self.resolve(node, id, IortNode::is_smmu)
    }

    /// The ITS group a device's MSIs reach and its device ID there.
    #[must_use]
    pub fn its_device_id(&self, node: &IortNode<'a>, id: u32) -> Option<(IortNode<'a>, u32)> {
        self.resolve(node, id, IortNode::is_its_group)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures are tiny
mod tests {
    use super::*;
    use crate::Acpi;
    use crate::testmem::*;

    /// A mapping for [`iort`]: `to` is an index into the node list.
    struct Map {
        input: u32,
        count: u32,
        output: u32,
        to: usize,
        single: bool,
    }

    fn map(input: u32, count: u32, output: u32, to: usize) -> Map {
        Map {
            input,
            count,
            output,
            to,
            single: false,
        }
    }

    /// Builds an IORT from (type, revision, type-specific data, mappings)
    /// nodes, resolving mapping targets to offsets.
    fn iort(nodes: &[(u8, u8, Vec<u8>, Vec<Map>)]) -> Vec<u8> {
        let mut offs = vec![];
        let mut at = 48u32;
        for (_, _, data, maps) in nodes {
            offs.push(at);
            at += 16 + data.len() as u32 + 20 * maps.len() as u32;
        }
        let mut body = (nodes.len() as u32).to_le_bytes().to_vec();
        body.extend(48u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        for (i, (ty, rev, data, maps)) in nodes.iter().enumerate() {
            let len = 16 + data.len() + 20 * maps.len();
            body.push(*ty);
            body.extend((len as u16).to_le_bytes());
            body.push(*rev);
            body.extend((i as u32).to_le_bytes()); // identifier
            body.extend((maps.len() as u32).to_le_bytes());
            body.extend((16 + data.len() as u32).to_le_bytes());
            body.extend(data);
            for m in maps {
                body.extend(m.input.to_le_bytes());
                body.extend((m.count - 1).to_le_bytes());
                body.extend(m.output.to_le_bytes());
                body.extend(offs[m.to].to_le_bytes());
                body.extend(u32::from(m.single).to_le_bytes());
            }
        }
        sdt(*b"IORT", 5, &body)
    }

    fn its_group(ids: &[u32]) -> Vec<u8> {
        let mut d = (ids.len() as u32).to_le_bytes().to_vec();
        for i in ids {
            d.extend(i.to_le_bytes());
        }
        d
    }

    fn root_complex(segment: u32) -> Vec<u8> {
        let mut d = 1u32.to_le_bytes().to_vec(); // coherent
        d.extend([0, 0, 0, 0]); // hints, reserved, flags
        d.extend(0u32.to_le_bytes()); // ATS
        d.extend(segment.to_le_bytes());
        d.extend([48, 0, 0, 0]); // address bits, PASID, reserved
        d
    }

    fn smmu_v3(base: u64) -> Vec<u8> {
        let mut d = base.to_le_bytes().to_vec();
        d.extend(1u32.to_le_bytes()); // coherent
        d.extend([0; 12]); // reserved, VATOS
        d.extend(0u32.to_le_bytes()); // model
        for gsiv in [106u32, 107, 108, 109] {
            d.extend(gsiv.to_le_bytes());
        }
        d.extend([0; 8]); // proximity, mapping index
        d
    }

    fn named(name: &str) -> Vec<u8> {
        let mut d = 0u32.to_le_bytes().to_vec(); // flags
        d.extend(0u32.to_le_bytes()); // not coherent
        d.extend([0; 4]);
        d.push(40);
        d.extend(name.as_bytes());
        d.push(0);
        d
    }

    /// An ITS group, an `SMMUv3` that maps every stream to the ITS (and its
    /// own MSIs to device 0x7000), PCI segment 0 behind the SMMU, segment
    /// 1 straight to the ITS, and a GPU with one stream ID behind the SMMU.
    fn system_iort() -> Vec<u8> {
        iort(&[
            /* 0 */ (0, 1, its_group(&[0]), vec![]),
            /* 1 */
            (
                4,
                5,
                smmu_v3(0x0905_0000),
                vec![
                    map(0, 0x1_0000, 0x1_0000, 0),
                    Map {
                        input: 0,
                        count: 1,
                        output: 0x7000,
                        to: 0,
                        single: true,
                    },
                ],
            ),
            /* 2 */ (2, 4, root_complex(0), vec![map(0, 0x1_0000, 0, 1)]),
            /* 3 */ (2, 4, root_complex(1), vec![map(0x100, 0x100, 0x2_0000, 0)]),
            /* 4 */
            (
                1,
                4,
                named("\\_SB.GPU0"),
                vec![Map {
                    input: 0,
                    count: 1,
                    output: 0x500,
                    to: 1,
                    single: true,
                }],
            ),
        ])
    }

    #[test]
    fn nodes_decode() {
        let m = system(&[system_iort()]);
        let t = Acpi::new(&m, 0x1000).unwrap().iort().unwrap();
        let n: Vec<_> = t.nodes().collect();
        assert_eq!(n.len(), 5);
        let IortKind::ItsGroup(ids) = n[0].kind else {
            panic!("{:?}", n[0])
        };
        assert_eq!(ids.iter().collect::<Vec<_>>(), [0]);
        assert!(matches!(
            n[1].kind,
            IortKind::SmmuV3 {
                base: 0x0905_0000,
                coherent: true,
                gerr_gsiv: 108,
                ..
            }
        ));
        assert!(matches!(
            n[2].kind,
            IortKind::RootComplex {
                segment: 0,
                coherent: true,
                address_bits: 48,
                ..
            }
        ));
        assert_eq!(n[4].identifier, 4);
        let gpu = t.named_component("\\_SB.GPU0").unwrap();
        assert_eq!(gpu.offset, n[4].offset);
        assert!(matches!(
            gpu.kind,
            IortKind::NamedComponent {
                coherent: false,
                address_bits: 40,
                ..
            }
        ));
    }

    #[test]
    fn pci_ids_through_smmu_to_its() {
        let m = system(&[system_iort()]);
        let t = Acpi::new(&m, 0x1000).unwrap().iort().unwrap();
        let rc = t.root_complex(0).unwrap();
        // Requester 00:01.0 (RID 0x8).
        let (smmu, sid) = t.stream_id(&rc, 0x8).unwrap();
        assert!(smmu.is_smmu());
        assert_eq!(sid, 0x8);
        let (its, dev) = t.its_device_id(&rc, 0x8).unwrap();
        assert!(its.is_its_group());
        assert_eq!(dev, 0x1_0008);
        // A stream outside the SMMU's range goes nowhere; the single
        // mapping is the SMMU's own MSI device ID, not a catch-all.
        assert!(t.map_id(&smmu, 0x2_0000).is_none());
        let (its, own) = t.own_id(&smmu).unwrap();
        assert!(its.is_its_group());
        assert_eq!(own, 0x7000);
    }

    #[test]
    fn bypass_and_ranges() {
        let m = system(&[system_iort()]);
        let t = Acpi::new(&m, 0x1000).unwrap().iort().unwrap();
        let rc1 = t.root_complex(1).unwrap();
        // Segment 1 skips the SMMU: no stream ID, but an ITS device ID.
        assert!(t.stream_id(&rc1, 0x150).is_none());
        assert_eq!(t.its_device_id(&rc1, 0x150).unwrap().1, 0x2_0050);
        // Outside the mapped range of 0x100-0x1ff.
        assert!(t.its_device_id(&rc1, 0x200).is_none());
        assert!(t.root_complex(2).is_none());
        // A named component's single mapping gives its one stream ID.
        let gpu = t.named_component("\\_SB.GPU0").unwrap();
        assert_eq!(t.stream_id(&gpu, 0).unwrap().1, 0x500);
    }

    #[test]
    fn cyclic_mappings_end_walk() {
        // Two root complexes that map to each other.
        let m = system(&[iort(&[
            (2, 4, root_complex(0), vec![map(0, 16, 0, 1)]),
            (2, 4, root_complex(1), vec![map(0, 16, 0, 0)]),
        ])]);
        let t = Acpi::new(&m, 0x1000).unwrap().iort().unwrap();
        let rc = t.root_complex(0).unwrap();
        assert!(t.its_device_id(&rc, 3).is_none());
    }

    #[test]
    fn id_mapping_edges() {
        let m = IdMapping {
            input_base: 0xffff_fff0,
            count: 0x10,
            output_base: 0xffff_fff8,
            output: 0,
            single: false,
        };
        assert_eq!(m.map(0xffff_fff0), Some(0xffff_fff8));
        assert_eq!(m.map(0xffff_ffef), None);
        // The output would overflow.
        assert_eq!(m.map(0xffff_ffff), None);
    }
}
