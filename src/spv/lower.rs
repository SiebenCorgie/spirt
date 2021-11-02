//! SPIR-V to SPIR-T lowering.

use crate::spv::{self, spec};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::Path;
use std::rc::Rc;
use std::{io, iter};

/// SPIR-T definition of a SPIR-V ID.
enum IdDef {
    SpvExtInstImport(Rc<String>),
    SpvDebugString(Rc<String>),
}

// FIXME(eddyb) stop abusing `io::Error` for error reporting.
fn invalid(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("malformed SPIR-V module ({})", reason),
    )
}

impl crate::Module {
    pub fn lower_from_spv_file(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::lower_from_spv_module_parser(spv::read::ModuleParser::read_from_spv_file(path)?)
    }

    pub fn lower_from_spv_module_parser(mut parser: spv::read::ModuleParser) -> io::Result<Self> {
        let spv_spec = spec::Spec::get();
        let wk = &spv_spec.well_known;

        let mut dialect = {
            let [magic, version, generator_magic, id_bound, reserved_inst_schema] = parser.header;

            // Ensured above (this is the value after any endianness swapping).
            assert_eq!(magic, spv_spec.magic);

            let [version_reserved_hi, version_major, version_minor, version_reserved_lo] =
                version.to_be_bytes();

            if (version_reserved_lo, version_reserved_hi) != (0, 0) {
                return Err(invalid(&format!(
                    "version 0x{:08x} is not in expected (0.major.minor.0) form",
                    version
                )));
            }

            if reserved_inst_schema != 0 {
                return Err(invalid(&format!(
                    "unknown instruction schema {} - only 0 is supported",
                    reserved_inst_schema
                )));
            }

            spv::Dialect {
                version_major,
                version_minor,

                original_generator_magic: generator_magic,
                original_id_bound: NonZeroU32::new(id_bound)
                    .ok_or_else(|| invalid("ID bound 0"))?,

                capabilities: BTreeSet::new(),
                extensions: BTreeSet::new(),

                addressing_model: 0,
                memory_model: 0,
            }
        };

        #[derive(Copy, Clone, Debug, PartialEq, PartialOrd)]
        enum Seq {
            Capability,
            Extension,
            ExtInstImport,
            MemoryModel,
            EntryPoint,
            ExecutionMode,
            DebugStringAndSource,
            Other,
        }
        let mut seq = None;

        let mut has_memory_model = false;
        let mut pending_attrs = FxHashMap::<spv::Id, BTreeSet<_>>::default();
        let mut id_defs = FxHashMap::default();
        let mut top_level = vec![];
        while let Some(inst) = parser.next().transpose()? {
            let opcode = inst.opcode;

            let invalid = |msg: &str| {
                let inst_name = spv_spec.instructions.get_named(opcode).unwrap().0;
                invalid(&format!("in {}: {}", inst_name, msg))
            };

            let next_seq = if opcode == wk.OpCapability {
                assert!(inst.result_type_id.is_none() && inst.result_id.is_none());
                let cap = match inst.operands[..] {
                    [spv::Operand::Imm(spv::Imm::Short(kind, cap))] => {
                        assert!(kind == wk.Capability);
                        cap
                    }
                    _ => unreachable!(),
                };

                dialect.capabilities.insert(cap);

                Seq::Capability
            } else if opcode == wk.OpExtension {
                assert!(inst.result_type_id.is_none() && inst.result_id.is_none());
                let ext = spv::extract_literal_string(&inst.operands)
                    .map_err(|e| invalid(&format!("{} in {:?}", e, e.as_bytes())))?;

                dialect.extensions.insert(ext);

                Seq::Extension
            } else if opcode == wk.OpExtInstImport {
                assert!(inst.result_type_id.is_none());
                let id = inst.result_id.unwrap();
                let name = spv::extract_literal_string(&inst.operands)
                    .map_err(|e| invalid(&format!("{} in {:?}", e, e.as_bytes())))?;

                id_defs.insert(id, IdDef::SpvExtInstImport(Rc::new(name)));

                Seq::ExtInstImport
            } else if opcode == wk.OpMemoryModel {
                assert!(inst.result_type_id.is_none() && inst.result_id.is_none());
                let (addressing_model, memory_model) = match inst.operands[..] {
                    [spv::Operand::Imm(spv::Imm::Short(am_kind, am)), spv::Operand::Imm(spv::Imm::Short(mm_kind, mm))] =>
                    {
                        assert!(am_kind == wk.AddressingModel && mm_kind == wk.MemoryModel);
                        (am, mm)
                    }
                    _ => unreachable!(),
                };

                if has_memory_model {
                    return Err(invalid("duplicate OpMemoryModel"));
                }
                has_memory_model = true;

                dialect.addressing_model = addressing_model;
                dialect.memory_model = memory_model;

                Seq::MemoryModel
            } else if opcode == wk.OpString {
                assert!(inst.result_type_id.is_none());
                let id = inst.result_id.unwrap();
                let s = spv::extract_literal_string(&inst.operands)
                    .map_err(|e| invalid(&format!("{} in {:?}", e, e.as_bytes())))?;

                id_defs.insert(id, IdDef::SpvDebugString(Rc::new(s)));

                // NOTE(eddyb) debug instructions are handled earlier in the code
                // for organizatory purposes, see `Seq` for the in-module order.
                Seq::DebugStringAndSource
            } else if opcode == wk.OpEntryPoint {
                assert!(inst.result_type_id.is_none() && inst.result_id.is_none());

                let target_id = match inst.operands[1] {
                    spv::Operand::ForwardIdRef(kind, id) | spv::Operand::Id(kind, id) => {
                        assert!(kind == wk.IdRef);
                        id
                    }
                    _ => unreachable!(),
                };

                let mut params = SmallVec::new();
                let mut interface_ids = SmallVec::new();
                for operand in iter::once(&inst.operands[0]).chain(&inst.operands[2..]) {
                    match *operand {
                        spv::Operand::Imm(imm) => {
                            assert!(interface_ids.is_empty());
                            params.push(imm);
                        }
                        spv::Operand::ForwardIdRef(kind, id) | spv::Operand::Id(kind, id) => {
                            assert!(kind == wk.IdRef);
                            interface_ids.push(id);
                        }
                    }
                }

                pending_attrs
                    .entry(target_id)
                    .or_default()
                    .insert(crate::Attr::SpvEntryPoint {
                        params,
                        interface_ids,
                    });

                Seq::EntryPoint
            } else if [
                wk.OpExecutionMode,
                wk.OpExecutionModeId, // FIXME(eddyb) not actually supported
                wk.OpName,
                wk.OpMemberName,
                wk.OpDecorate,
                wk.OpMemberDecorate,
                wk.OpDecorateId, // FIXME(eddyb) not actually supported
                wk.OpDecorateString,
                wk.OpMemberDecorateString,
            ]
            .contains(&opcode)
            {
                assert!(inst.result_type_id.is_none() && inst.result_id.is_none());

                let target_id = match inst.operands[0] {
                    spv::Operand::ForwardIdRef(kind, id) | spv::Operand::Id(kind, id) => {
                        assert!(kind == wk.IdRef);
                        id
                    }
                    _ => unreachable!(),
                };
                let params = inst.operands[1..]
                    .iter()
                    .map(|operand| match *operand {
                        spv::Operand::Imm(imm) => Ok(imm),
                        spv::Operand::ForwardIdRef(..) | spv::Operand::Id(..) => {
                            Err(invalid("unsupported decoration with ID"))
                        }
                    })
                    .collect::<Result<_, _>>()?;
                pending_attrs
                    .entry(target_id)
                    .or_default()
                    .insert(crate::Attr::SpvAnnotation { opcode, params });

                if [wk.OpExecutionMode, wk.OpExecutionModeId].contains(&opcode) {
                    Seq::ExecutionMode
                } else {
                    // FIXME(eddyb) separate out early debug instructions to allow
                    // this to be accurate.
                    Seq::Other
                }
            } else {
                top_level.push(crate::TopLevel::Misc(crate::Misc {
                    kind: crate::MiscKind::SpvInst {
                        opcode: inst.opcode,
                    },
                    output: inst
                        .result_id
                        .map(|result_id| crate::MiscOutput::SpvResult {
                            result_type_id: inst.result_type_id,
                            result_id,
                        }),
                    inputs: inst
                        .operands
                        .iter()
                        .map(|operand| match *operand {
                            spv::Operand::Imm(imm) => crate::MiscInput::SpvImm(imm),
                            spv::Operand::Id(_, id) | spv::Operand::ForwardIdRef(_, id) => {
                                match id_defs.get(&id) {
                                    Some(IdDef::SpvExtInstImport(name)) => {
                                        crate::MiscInput::SpvExtInstImport(name.clone())
                                    }
                                    Some(IdDef::SpvDebugString(s)) => {
                                        crate::MiscInput::SpvDebugString(s.clone())
                                    }
                                    None => crate::MiscInput::SpvUntrackedId(id),
                                }
                            }
                        })
                        .collect(),
                    attrs: inst
                        .result_id
                        .and_then(|id| pending_attrs.remove(&id))
                        .map(Rc::new),
                }));

                Seq::Other
            };
            if !(seq <= Some(next_seq)) {
                return Err(invalid(&format!(
                    "out of order: {:?} instructions must precede {:?} instructions",
                    next_seq, seq
                )));
            }
            seq = Some(next_seq);
        }

        if !has_memory_model {
            return Err(invalid("missing OpMemoryModel"));
        }

        if !pending_attrs.is_empty() {
            let ids = pending_attrs.keys().collect::<BTreeSet<_>>();
            return Err(invalid(&format!("decorated IDs never defined: {:?}", ids)));
        }

        Ok(Self {
            dialect: crate::ModuleDialect::Spv(dialect),
            top_level,
        })
    }
}
