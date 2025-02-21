// This exporter is WIP.
#![allow(dead_code, unused_imports, unused_variables, clippy::let_unit_value)]
use std::{collections::HashSet, io::Write, path::Path};
// Strips debuginfo: `#line [0-9]+ "[a-z/.\-0-9_]+"`
use fxhash::{hash64, FxHashSet, FxHasher};

use crate::{
    config, typecheck,
    utilis::{assert_unique, encode},
    v2::{asm::LINKER_RECOVER, BiMap, MethodImpl, StringIdx},
};
config!(NO_SFI, bool, false);
config!(ANSI_C, bool, false);
config!(NO_OPT, bool, false);

config!(UB_CHECKS, bool, true);
config!(SHORT_TYPENAMES, bool, false);
config!(PARTS, u32, 1);
use super::{
    asm::MAIN_MODULE,
    bimap::IntoBiMapIndex,
    cilnode::{ExtendKind, PtrCastRes},
    cilroot::BranchCond,
    method::LocalDef,
    tpe::simd::SIMDVector,
    typecheck::TypeCheckError,
    Assembly, BinOp, CILIter, CILIterElem, CILNode, CILRoot, ClassDefIdx, ClassRef, ClassRefIdx,
    Const, Exporter, Int, MethodDef, MethodRef, NodeIdx, RootIdx, SigIdx, Type,
};
fn local_name(locals: &[LocalDef], asm: &Assembly, loc: u32) -> String {
    // If the name of this local repeats, use the L form.
    if locals
        .iter()
        .filter(|(name, _)| *name == locals[loc as usize].0)
        .count()
        > 1
    {
        return format!("L{loc}");
    }
    match locals[loc as usize].0 {
        Some(local_name) => {
            let ident = escape_ident(&asm[local_name]);
            match ident.as_str() {
                "socket" => {
                    format!("i{}", encode(hash64(&ident)))
                }
                _ => ident,
            }
        }
        None => format!("L{loc}"),
    }
}
fn escape_ident(ident: &str) -> String {
    let mut escaped = ident
        .replace(['.', ' '], "_")
        .replace('~', "_tilda_")
        .replace('=', "_eq_")
        .replace("#", "_pound_")
        .replace(":", "_col_")
        .replace("[", "_srpar_")
        .replace("]", "_slpar_")
        .replace("(", "_rpar_")
        .replace(")", "_lpar_")
        .replace("{", "_rbra_")
        .replace("}", "_lbra_");
    if escaped.chars().next().unwrap().is_numeric() {
        escaped = format!("p{escaped}");
    }
    // Check if reserved.
    match escaped.as_str() {
        "int" | "default" | "float" | "double" | "long" | "short" | "register" | "stderr"
        | "environ" | "struct" | "union" | "linux" | "inline" | "asm" | "signed" | "unsigned"
        | "bool" | "char" | "case" | "switch" | "volatile" | "auto" | "void" | "unix" => {
            format!("i{}", encode(hash64(&escaped)))
        }
        _ => escaped,
    }
}
fn nonvoid_c_type(field_tpe: Type, asm: &Assembly) -> String {
    match field_tpe {
        Type::Void => "RustVoid".into(),
        _ => c_tpe(field_tpe, asm),
    }
}
fn c_tpe(field_tpe: Type, asm: &Assembly) -> String {
    match field_tpe {
        Type::Ptr(type_idx) | Type::Ref(type_idx) => format!("{}*", c_tpe(asm[type_idx], asm)),
        Type::Int(int) => match int {
            Int::U8 => "uint8_t".into(),
            Int::U16 => "uint16_t".into(),
            Int::U32 => "uint32_t".into(),
            Int::U64 => "uint64_t".into(),
            Int::U128 => "__uint128_t".into(),
            Int::USize => "uintptr_t".into(),
            Int::I8 => "int8_t".into(),
            Int::I16 => "int16_t".into(),
            Int::I32 => "int32_t".into(),
            Int::I64 => "int64_t".into(),
            Int::I128 => "__int128".into(),
            Int::ISize => "intptr_t".into(),
        },
        Type::ClassRef(class_ref_idx) => {
            format!("union {}", escape_ident(&asm[asm[class_ref_idx].name()]))
        }
        Type::Float(float) => match float {
            super::Float::F16 => "_Float16".into(),
            super::Float::F32 => "float".into(),
            super::Float::F64 => "double".into(),
            super::Float::F128 => "_Float128".into(),
        },
        Type::PlatformString => "char*".into(),
        Type::PlatformChar => "char".into(),
        Type::PlatformGeneric(_, generic_kind) => todo!(),
        Type::PlatformObject => "void*".into(),
        Type::Bool => "bool".into(),
        Type::Void => "void".into(),
        Type::PlatformArray { elem, dims } => format!(
            "{elem}{dims}",
            elem = c_tpe(asm[elem], asm),
            dims = "*".repeat(dims.get() as usize)
        ),
        Type::FnPtr(_) => "void*".into(),
        Type::SIMDVector(vec) => {
            format!(
                "__simdvec{elem}{count}",
                elem = std::convert::Into::<Type>::into(vec.elem()).mangle(asm),
                count = vec.count()
            )
        }
    }
}
fn mref_to_name(mref: &MethodRef, asm: &Assembly) -> String {
    let class = &asm[mref.class()];
    let class_name = escape_ident(&asm[class.name()]);
    let mname = escape_ident(&asm[mref.name()]);
    if class.asm().is_some()
        || matches!(mref.output(asm), Type::SIMDVector(_))
        || mref
            .stack_inputs(asm)
            .iter()
            .any(|tpe| matches!(tpe, Type::SIMDVector(_)))
        || mname == "transmute"
        || mname == "create_slice"
        || mname == "_Unwind_Backtrace"
    {
        let mangled = escape_ident(
            &asm[mref.sig()]
                .iter_types()
                .map(|tpe| tpe.mangle(asm))
                .collect::<String>(),
        );

        let stem = class_member_name(&class_name, &mname);
        format!("{stem}{mangled}")
    } else {
        class_member_name(&class_name, &mname)
    }
}
fn class_member_name(class_name: &str, method_name: &str) -> String {
    if class_name == MAIN_MODULE {
        method_name.into()
    } else {
        format!("{class_name}_{method_name}")
    }
}
pub struct CExporter {
    is_lib: bool,
}
impl CExporter {
    #[must_use]
    pub fn new(is_lib: bool) -> Self {
        Self { is_lib }
    }
    fn export_method_decl(
        asm: &Assembly,
        mref: &MethodRef,
        method_decls: &mut impl Write,
    ) -> std::io::Result<()> {
        let method_name = mref_to_name(mref, asm);
        if method_name == "malloc" || method_name == "realloc" || method_name == "free" {
            return Ok(());
        }
        let output = c_tpe(mref.output(asm), asm);
        let inputs = mref
            .stack_inputs(asm)
            .iter()
            .map(|i| nonvoid_c_type(*i, asm))
            .intersperse(",".into())
            .collect::<String>();

        writeln!(method_decls, "{output} {method_name}({inputs});")
    }
    #[allow(clippy::too_many_arguments)]
    fn binop_to_string(
        lhs: CILNode,
        rhs: CILNode,
        op: BinOp,
        tpe: Type,
        asm: &mut Assembly,
        locals: &[LocalDef],
        inputs: &[(Type, Option<StringIdx>)],
        sig: SigIdx,
    ) -> Result<String, TypeCheckError> {
        let lhs = Self::node_to_string(lhs, asm, locals, inputs, sig)?;
        let rhs = Self::node_to_string(rhs, asm, locals, inputs, sig)?;
        Ok(match op {
            BinOp::Add => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => format!(
                    "({tpe}*)((void*)({lhs}) + (uintptr_t)({rhs}))",
                    tpe = c_tpe(asm[type_idx], asm)
                ),
                Type::FnPtr(_) => format!("({lhs}) + ({rhs})"),
                Type::Float(_) => format!("({lhs}) + ({rhs})"),
                Type::Int(Int::ISize) => {
                    format!("(intptr_t)((uintptr_t)({lhs}) + (uintptr_t)({rhs}))")
                }
                Type::Int(Int::I128) => {
                    format!("(__int128)((__uint128_t)({lhs}) + (__uint128_t)({rhs}))")
                }
                Type::Int(Int::I64) => format!("(int64_t)((uint64_t)({lhs}) + (uint64_t)({rhs}))"),
                Type::Int(Int::I32) => format!("(int32_t)((uint32_t)({lhs}) + (uint32_t)({rhs}))"),
                Type::Int(Int::I16) => format!("(int16_t)((uint16_t)({lhs}) + (uint16_t)({rhs}))"),
                Type::Int(Int::I8) => format!("(int8_t)((uint8_t)({lhs}) + (uint8_t)({rhs}))"),

                Type::Int(_) => format!("({lhs}) + ({rhs})"),
                _ => todo!("can't add {}", tpe.mangle(asm)),
            },
            BinOp::Eq => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) == (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) == ({rhs})"),
                Type::Bool | Type::Float(_) | Type::Int(_) => format!("({lhs}) == ({rhs})"),
                _ => todo!(),
            },
            BinOp::Sub => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => format!(
                    "({tpe}*)((void*)({lhs}) - (uintptr_t)({rhs}))",
                    tpe = c_tpe(asm[type_idx], asm)
                ),
                Type::FnPtr(_) => format!("({lhs}) - ({rhs})"),
                Type::Int(Int::I128) => {
                    format!("(__int128)((__uint128_t)({lhs}) - (__uint128_t)({rhs}))")
                }
                Type::Int(Int::I64) => {
                    format!("(int64_t)((uint64_t)({lhs}) - (uint64_t)({rhs}))")
                }
                Type::Int(Int::I32) => {
                    format!("(int32_t)((uint32_t)({lhs}) - (uint32_t)({rhs}))")
                }
                Type::Int(Int::I16) => {
                    format!("(int16_t)((uint16_t)({lhs}) - (uint16_t)({rhs}))")
                }
                Type::Int(Int::I8) => {
                    format!("(int8_t)((uint8_t)({lhs}) - (uint8_t)({rhs}))")
                }
                Type::Int(Int::ISize) => {
                    format!("(intptr_t)((uintptr_t)({lhs}) - (uintptr_t)({rhs}))")
                }
                Type::Float(_) | Type::Int(_) => format!("({lhs}) - ({rhs})"),
                _ => todo!(),
            },
            BinOp::Mul => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => format!(
                    "({tpe}*)((void*)({lhs}) * (uintptr_t)({rhs}))",
                    tpe = c_tpe(asm[type_idx], asm)
                ),
                Type::FnPtr(_) => format!("({lhs}) * ({rhs})"),
                Type::Float(_) => format!("({lhs}) * ({rhs})"),
                Type::Int(int) => match int {
                    // Signed multiply is seemingly equivalent to unsigned multiply, looking at the assembly: TODO: check this.
                    Int::I8 => format!("(int8_t)((uint8_t)({lhs}) * (uint8_t)({rhs}))"),
                    Int::I16 => {
                        format!("(int16_t)(uint16_t)(((uint32_t)({lhs})) * ((uint32_t)({rhs})))")
                    }
                    Int::I32 => format!("(int32_t)((uint32_t)({lhs}) * (uint32_t)({rhs}))"),
                    Int::I64 => format!("(int64_t)((uint64_t)({lhs}) * (uint64_t)({rhs}))"),
                    Int::I128 => format!("(__int128)((__uint128_t)({lhs}) * (__uint128_t)({rhs}))"),
                    Int::ISize => format!("(intptr_t)((uintptr_t)({lhs}) * (uintptr_t)({rhs}))"),
                    Int::U16 => format!("(uint16_t)(((uint32_t)({lhs})) * ((uint32_t)({rhs})))"),
                    _ => format!("({lhs}) * ({rhs})"),
                },
                _ => todo!(),
            },
            BinOp::Lt => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) < (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) < ({rhs})"),
                Type::Bool | Type::Float(_) | Type::Int(_) => format!("({lhs}) < ({rhs})"),
                _ => todo!(),
            },
            BinOp::LtUn => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) < (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) < ({rhs})"),
                Type::Bool | Type::Int(_) => format!("({lhs}) < ({rhs})"),
                Type::Float(_) => format!("!(({lhs}) >= ({rhs}))"),
                _ => todo!(),
            },
            BinOp::Gt => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) > (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) > ({rhs})"),
                Type::Bool | Type::Float(_) | Type::Int(_) => format!("({lhs}) > ({rhs})"),
                _ => todo!(),
            },
            BinOp::GtUn => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) > (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) > ({rhs})"),
                Type::Bool | Type::Int(_) => format!("({lhs}) > ({rhs})"),
                Type::Float(_) => format!("!(({lhs}) <= ({rhs}))"),
                _ => todo!(),
            },
            BinOp::Or => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) | (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) | ({rhs})"),
                Type::Int(_) => format!("({lhs}) | ({rhs})"),
                Type::Bool => format!("({lhs}) || ({rhs})"),
                _ => todo!(),
            },
            BinOp::XOr => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) ^ (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) ^ ({rhs})"),
                Type::Int(_) => format!("({lhs}) ^ ({rhs})"),
                Type::Bool => format!("({lhs}) != ({rhs})"),
                _ => todo!(),
            },
            BinOp::And => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => {
                    format!("(void*)({lhs}) & (void*)({rhs})",)
                }
                Type::FnPtr(_) => format!("({lhs}) & ({rhs})"),
                Type::Int(_) => format!("({lhs}) & ({rhs})"),
                Type::Bool => format!("({lhs}) && ({rhs})"),
                _ => todo!(),
            },
            BinOp::Rem | BinOp::RemUn => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => format!(
                    "({tpe}*)((void*)({lhs}) % (uintptr_t)({rhs}))",
                    tpe = c_tpe(asm[type_idx], asm)
                ),
                Type::FnPtr(_) => format!("({lhs}) % ({rhs})"),
                Type::Int(_) => format!("({lhs}) % ({rhs})"),
                Type::Float(flt) => match flt {
                    super::Float::F16 => todo!(),
                    super::Float::F32 => format!("(float)fmod((double)({lhs}),((double)({rhs}))"),
                    super::Float::F64 => format!("fmod(({lhs}),({rhs}))"),
                    super::Float::F128 => todo!(),
                },
                // TODO: reminder of a bool can only be false or a segfault. Is this a valid operation?
                Type::Bool => "false".into(),

                _ => todo!("can't rem {tpe:?}"),
            },
            BinOp::Shl => match tpe {
                // Signed shift is equivalent to Rust unsinged shift, but it is well defined in C
                Type::Int(Int::I8) => format!("(int8_t)((uint8_t)({lhs}) << ({rhs}))"),
                Type::Int(Int::I16) => format!("(int16_t)((uint16_t)({lhs}) << ({rhs}))"),
                Type::Int(Int::I32) => format!("(int32_t)((uint32_t)({lhs}) << ({rhs}))"),
                Type::Int(Int::I64) => format!("(int64_t)((uint64_t)({lhs}) << ({rhs}))"),
                Type::Int(Int::I128) => format!("(__int128)((__uint128_t)({lhs}) << ({rhs}))"),
                Type::Int(Int::ISize) => format!("(intptr_t)((uintptr_t)({lhs}) << ({rhs}))"),
                Type::Int(_) => format!("({lhs}) << ({rhs})"),
                _ => todo!("can't shl {tpe:?}"),
            },
            BinOp::Shr | BinOp::ShrUn => match tpe {
                Type::Int(_) => format!("({lhs}) >> ({rhs})"),
                _ => todo!("can't shr {tpe:?}"),
            },
            BinOp::DivUn | BinOp::Div => match tpe {
                Type::Ptr(type_idx) | Type::Ref(type_idx) => format!(
                    "({tpe}*)((void*)({lhs}) / (uintptr_t)({rhs}))",
                    tpe = c_tpe(asm[type_idx], asm)
                ),
                Type::FnPtr(_) => format!("({lhs}) / ({rhs})"),
                Type::Float(_) | Type::Int(_) => format!("({lhs}) / ({rhs})"),
                _ => todo!(),
            },
        })
    }
    fn node_to_string(
        node: CILNode,
        asm: &mut Assembly,
        locals: &[LocalDef],
        inputs: &[(Type, Option<StringIdx>)],
        sig: SigIdx,
    ) -> Result<String, TypeCheckError> {
        Ok(match node {
            CILNode::Const(cst) => match cst.as_ref() {
                Const::I8(v) => format!("(int8_t)0x{v:x}"),
                Const::I16(v) => format!("(int16_t)0x{v:x}"),
                Const::I32(v) => format!("((int32_t)0x{v:x})"),
                Const::I64(v) => format!("((int64_t)0x{v:x}L)"),
                Const::I128(v) => {
                    let low = *v as u128 as u64;
                    let high = ((*v as u128) >> 64) as u64;
                    format!("(__int128)((unsigned __int128)(0x{low:x}) | ((unsigned __int128)(0x{high:x}) << 64))")
                }
                Const::ISize(v) => format!("(intptr_t)0x{v:x}L"),
                Const::U8(v) => format!("(uint8_t)0x{v:x}"),
                Const::U16(v) => format!("(uint16_t)0x{v:x}"),
                Const::U32(v) => format!("0x{v:x}u"),
                Const::U64(v) => format!("0x{v:x}uL"),
                Const::U128(v) => {
                    let low = *v as u64;
                    let high = ({ *v } >> 64) as u64;
                    format!("((unsigned __int128)(0x{low:x}) | ((unsigned __int128)(0x{high:x}) << 64))")
                }
                Const::USize(v) => format!("(uintptr_t)0x{v:x}uL"),
                Const::PlatformString(string_idx) => format!("{:?}", &asm[*string_idx]),
                Const::Bool(val) => {
                    if *val {
                        "true".into()
                    } else {
                        "false".into()
                    }
                }
                Const::F32(hashable_f32) => {
                    if !hashable_f32.0.is_nan() {
                        format!("{:?}f", hashable_f32.0)
                    } else {
                        "NAN".into()
                    }
                }
                Const::F64(hashable_f64) => {
                    if !hashable_f64.0.is_nan() {
                        format!("{:?}", hashable_f64.0)
                    } else {
                        "NAN".into()
                    }
                }
                Const::Null(class_ref_idx) => todo!(),
            },
            CILNode::BinOp(lhs, rhs, bin_op) => {
                let tpe = node.typecheck(sig, locals, asm)?;
                Self::binop_to_string(
                    asm[lhs].clone(),
                    asm[rhs].clone(),
                    bin_op,
                    tpe,
                    asm,
                    locals,
                    inputs,
                    sig,
                )?
            }
            CILNode::UnOp(node_idx, ref un_op) => match un_op {
                super::cilnode::UnOp::Not => format!(
                    "~({})",
                    Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                ),
                super::cilnode::UnOp::Neg => {
                    let tpe = node.typecheck(sig, locals, asm)?;
                    match tpe {
                        Type::Ptr(_) | Type::Ref(_) => format!(
                            "-({})",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::FnPtr(_) => format!(
                            "-({})",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Float(_) => format!(
                            "-({})",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(Int::I8) => format!(
                            "(int8_t)(0 - ((uint8_t)({})))",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(Int::I16) => format!(
                            "(int16_t)(0 - ((uint16_t)({})))",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(Int::I32) => format!(
                            "(int32_t)(0 - ((uint32_t)({})))",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(Int::I64) => format!(
                            "(int64_t)(0 - ((uint64_t)({})))",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(Int::I128) => format!(
                            "(__int128_t)(0 - ((__uint128_t)({})))",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(Int::ISize) => format!(
                            "(intptr_t)(0 - ((uintptr_t)({})))",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        Type::Int(_) => format!(
                            "-({})",
                            Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
                        ),
                        _ => todo!("can't neg {}", tpe.mangle(asm)),
                    }
                }
            },
            CILNode::LdLoc(loc) => local_name(locals, asm, loc),
            CILNode::LdArg(arg) => match inputs[arg as usize].1 {
                Some(arg_name) => escape_ident(&asm[arg_name]),
                None => format!("A{arg}",),
            },
            CILNode::LdArgA(arg) => match inputs[arg as usize].1 {
                Some(arg_name) => format!("&{}", escape_ident(&asm[arg_name])),
                None => format!("&A{arg}",),
            },
            CILNode::LdLocA(loc) => format!("&{}", local_name(locals, asm, loc),),
            CILNode::Call(info) => {
                let (method, args) = info.as_ref();
                let method = asm[*method].clone();
                let call_args = args
                    .iter()
                    .map(|arg| {
                        format!(
                            "({})",
                            Self::node_to_string(asm[*arg].clone(), asm, locals, inputs, sig)
                                .unwrap()
                        )
                    })
                    .intersperse(",".into())
                    .collect::<String>();
                let class = &asm[method.class()];
                let class_name = escape_ident(&asm[class.name()]);
                let mname = escape_ident(&asm[method.name()]);
                let method_name = mref_to_name(&method, asm);
                format!("{method_name}({call_args})")
            }
            CILNode::IntCast {
                input,
                target,
                extend,
            } => {
                let input = Self::node_to_string(asm[input].clone(), asm, locals, inputs, sig)?;
                match (target, extend) {
                    (Int::U8, ExtendKind::ZeroExtend) => format!("(uint8_t)({input})"),
                    (Int::U8, ExtendKind::SignExtend) => todo!(),
                    (Int::U16, ExtendKind::ZeroExtend) => format!("(uint16_t)({input})"),
                    (Int::U16, ExtendKind::SignExtend) => todo!(),
                    (Int::U32, ExtendKind::ZeroExtend) => format!("(uint32_t)({input})"),
                    (Int::U32, ExtendKind::SignExtend) => format!("(uint32_t)(int32_t)({input})"),
                    (Int::U64, ExtendKind::ZeroExtend) => format!("(uint64_t)({input})"),
                    (Int::U64, ExtendKind::SignExtend) => format!("(uint64_t)(int64_t)({input})"),
                    (Int::U128, ExtendKind::ZeroExtend) => format!("(__uint128_t)({input})"),
                    (Int::U128, ExtendKind::SignExtend) => todo!(),
                    (Int::USize, ExtendKind::ZeroExtend) => format!("(uintptr_t)({input})"),
                    (Int::USize, ExtendKind::SignExtend) => {
                        format!("(uintptr_t)(intptr_t)({input})")
                    }
                    (Int::I8, ExtendKind::ZeroExtend) => todo!(),
                    (Int::I8, ExtendKind::SignExtend) => format!("(int8_t)({input})"),
                    (Int::I16, ExtendKind::ZeroExtend) => todo!(),
                    (Int::I16, ExtendKind::SignExtend) => format!("(int16_t)({input})"),
                    (Int::I32, ExtendKind::ZeroExtend) => format!("(int32_t)(uint32_t)({input})"),
                    (Int::I32, ExtendKind::SignExtend) => format!("(int32_t)({input})"),
                    (Int::I64, ExtendKind::ZeroExtend) => format!("(int64_t)(uint64_t)({input})"),
                    (Int::I64, ExtendKind::SignExtend) => format!("(int64_t)({input})"),
                    (Int::I128, ExtendKind::ZeroExtend) => todo!(),
                    (Int::I128, ExtendKind::SignExtend) => todo!(),
                    (Int::ISize, ExtendKind::ZeroExtend) => {
                        format!("(intptr_t)(uintptr_t)({input})")
                    }
                    (Int::ISize, ExtendKind::SignExtend) => format!("(intptr_t)({input})"),
                }
            }
            CILNode::FloatCast {
                input,
                target,
                is_signed,
            } => {
                let input = Self::node_to_string(asm[input].clone(), asm, locals, inputs, sig)?;
                match target {
                    super::Float::F16 => todo!(),
                    super::Float::F32 => format!("(float)({input})"),
                    super::Float::F64 => format!("(double)({input})"),
                    super::Float::F128 => todo!(),
                }
            }
            CILNode::RefToPtr(node_idx) => {
                Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
            }
            CILNode::PtrCast(node_idx, ptr_cast_res) => {
                let node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?;
                match ptr_cast_res.as_ref() {
                    PtrCastRes::Ptr(type_idx) | PtrCastRes::Ref(type_idx) => {
                        format!("({tpe}*)({node})", tpe = c_tpe(asm[*type_idx], asm),)
                    }
                    PtrCastRes::FnPtr(_) => format!("(void*)({node})"),
                    PtrCastRes::USize => format!("(uintptr_t)({node})"),
                    PtrCastRes::ISize => format!("(intptr_t)({node})"),
                }
            }
            CILNode::LdFieldAdress { addr, field } => {
                let addr = asm[addr].clone();
                let addr = Self::node_to_string(addr, asm, locals, inputs, sig)?;
                let field = asm[field];
                let name = escape_ident(&asm[field.name()]);
                format!("&({addr})->{name}.f")
            }
            CILNode::LdField { addr, field } => {
                let addr = asm[addr].clone();
                let addr_tpe = addr.typecheck(sig, locals, asm)?;
                let addr = Self::node_to_string(addr, asm, locals, inputs, sig)?;
                let field = asm[field];
                let name = escape_ident(&asm[field.name()]);
                match addr_tpe {
                    Type::Ref(_) | Type::Ptr(_) => format!("({addr})->{name}.f"),
                    Type::ClassRef(_) => format!("({addr}).{name}.f"),
                    _ => panic!(),
                }
            }
            CILNode::LdInd {
                addr,
                tpe,
                volatile,
            } => {
                if volatile {
                    format!(
                        "*(volatile {tpe}*)({addr})",
                        tpe = c_tpe(asm[tpe], asm),
                        addr = Self::node_to_string(asm[addr].clone(), asm, locals, inputs, sig)?
                    )
                } else {
                    format!(
                        "*({addr})",
                        addr = Self::node_to_string(asm[addr].clone(), asm, locals, inputs, sig)?
                    )
                }
            }
            CILNode::SizeOf(type_idx) => format!("sizeof({tpe})", tpe = c_tpe(asm[type_idx], asm)),
            CILNode::GetException => todo!(),
            CILNode::IsInst(node_idx, type_idx) => todo!(),
            CILNode::CheckedCast(node_idx, type_idx) => todo!(),
            CILNode::CallI(info) => {
                let (fn_ptr, fn_ptr_sig, args) = info.as_ref();
                let fn_ptr_sig = asm[*fn_ptr_sig].clone();
                let call_args = args
                    .iter()
                    .map(|arg| {
                        format!(
                            "({})",
                            Self::node_to_string(asm[*arg].clone(), asm, locals, inputs, sig)
                                .unwrap()
                        )
                    })
                    .intersperse(",".into())
                    .collect::<String>();
                let ret = c_tpe(*fn_ptr_sig.output(), asm);
                let args = fn_ptr_sig
                    .inputs()
                    .iter()
                    .map(|i| nonvoid_c_type(*i, asm))
                    .intersperse(",".into())
                    .collect::<String>();
                let fn_ptr = Self::node_to_string(asm[*fn_ptr].clone(), asm, locals, inputs, sig)?;
                format!("((*({ret}(*)({args}))({fn_ptr})))({call_args})")
            }
            CILNode::LocAlloc { size } => format!(
                "((uint8_t*)alloca({}))",
                Self::node_to_string(asm[size].clone(), asm, locals, inputs, sig)?
            ),
            CILNode::LdStaticField(static_field_idx) => {
                let field = asm[static_field_idx];
                let class = asm[field.owner()].clone();
                let fname = class_member_name(&asm[class.name()], &asm[field.name()]);
                fname.to_string()
            }
            CILNode::LdStaticFieldAdress(static_field_idx) => {
                let field = asm[static_field_idx];
                let class = asm[field.owner()].clone();
                let fname = class_member_name(&asm[class.name()], &asm[field.name()]);
                format!("&{}", fname)
            }
            CILNode::LdFtn(method) => mref_to_name(&asm[method], asm),
            CILNode::LdTypeToken(type_idx) => format!("{}", type_idx.as_bimap_index()),
            //TODO: ld len is not really supported in C, and is only there due to the argc emulation.
            CILNode::LdLen(node_idx) => format!(
                "ld_len({arr})",
                arr = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
            ),
            // TODO: loc alloc aligned does not respect the aligement ATM.
            CILNode::LocAllocAlgined { tpe, align } => {
                format!(
                    "({tpe}*)(alloca(sizeof({tpe})))",
                    tpe = c_tpe(asm[tpe], asm)
                )
            }
            //TODO: ld elem ref is not really supported in C, and is only there due to the argc emulation.
            CILNode::LdElelemRef { array, index } => {
                let tpe = node.typecheck(sig, locals, asm)?;
                let array = Self::node_to_string(asm[array].clone(), asm, locals, inputs, sig)?;
                let index = Self::node_to_string(asm[index].clone(), asm, locals, inputs, sig)?;
                format!("({array})[{index}]")
            }
            CILNode::UnboxAny { object, tpe } => format!(
                "({object})",
                object = Self::node_to_string(asm[object].clone(), asm, locals, inputs, sig)?
            ),
        })
    }
    fn root_to_string(
        root: CILRoot,
        asm: &mut Assembly,
        locals: &[LocalDef],
        inputs: &[(Type, Option<StringIdx>)],
        sig: SigIdx,
    ) -> Result<String, TypeCheckError> {
        Ok(match root {
            CILRoot::StLoc(id, node_idx) => {

                let name = local_name(locals, asm, id);
                return Ok(format!("{name} = {node};", node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?,));
            },
            CILRoot::StArg(arg, node_idx) =>match inputs[arg as usize].1 {
                Some(name) => format!(
                    "{name} = {node};",
                    node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?,
                    name = escape_ident(&asm[name]),
                ),
                None => format!(
                    "A{arg} = {node};",
                    node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?,
                ),
            },
            CILRoot::Ret(node_idx) => format!(
                "return {node};",
                node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
            ),
            CILRoot::Pop(node_idx) => format!(
                "{node};",
                node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
            ),
            CILRoot::Throw(node_idx) =>  format!(
                "eprintf(\"An error was encoutrered in %s, at %s:%d\\n\",__func__,__FILE__,__LINE__);eprintf(\"%s\\n\",{node}); abort();",
                node = Self::node_to_string(asm[node_idx].clone(), asm, locals, inputs, sig)?
            ),
            CILRoot::VoidRet => "return;".into(),
            CILRoot::Break => "".into(),
            CILRoot::Nop => "".into(),
            CILRoot::Branch(binfo) => {
                let (target, sub_target, cond) = binfo.as_ref();
                let target = if *sub_target != 0{
                    sub_target
                }else {target};
                let Some(cond) = cond else {
                    return Ok(format!("goto bb{target};"));
                };
                match cond {
                    BranchCond::True(node_idx) => format!(
                        "if({node}) goto bb{target};",
                        node =
                            Self::node_to_string(asm[*node_idx].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::False(node_idx) => format!(
                        "if(!({node})) goto bb{target};",
                        node =
                            Self::node_to_string(asm[*node_idx].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::Eq(lhs, rhs) => format!(
                        "if(({lhs}) == ({rhs})) goto bb{target};",
                        lhs = Self::node_to_string(asm[*lhs].clone(), asm, locals, inputs, sig)?,
                        rhs = Self::node_to_string(asm[*rhs].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::Ne(lhs, rhs) => format!(
                        "if(({lhs}) != ({rhs})) goto bb{target};",
                        lhs = Self::node_to_string(asm[*lhs].clone(), asm, locals, inputs, sig)?,
                        rhs = Self::node_to_string(asm[*rhs].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::Lt(lhs, rhs, cmp_kind) => format!(
                        "if(({lhs}) < ({rhs})) goto bb{target};",
                        lhs = Self::node_to_string(asm[*lhs].clone(), asm, locals, inputs, sig)?,
                        rhs = Self::node_to_string(asm[*rhs].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::Gt(lhs, rhs, _cmp_kind) => format!(
                        "if(({lhs}) > ({rhs})) goto bb{target};",
                        lhs = Self::node_to_string(asm[*lhs].clone(), asm, locals, inputs, sig)?,
                        rhs = Self::node_to_string(asm[*rhs].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::Le(lhs, rhs, _cmp_kind) => format!(
                        "if(({lhs}) <= ({rhs})) goto bb{target};",
                        lhs = Self::node_to_string(asm[*lhs].clone(), asm, locals, inputs, sig)?,
                        rhs = Self::node_to_string(asm[*rhs].clone(), asm, locals, inputs, sig)?
                    ),
                    BranchCond::Ge(lhs, rhs, _cmp_kind) => format!(
                        "if(({lhs}) >= ({rhs})) goto bb{target};",
                        lhs = Self::node_to_string(asm[*lhs].clone(), asm, locals, inputs, sig)?,
                        rhs = Self::node_to_string(asm[*rhs].clone(), asm, locals, inputs, sig)?
                    ),
                }
            }
            CILRoot::SourceFileInfo { line_start, line_len, col_start, col_len, file  } =>{
                if !*NO_SFI{
                    format!("#line {line_start} {file:?}", file = &asm[file])
                }else{
                    "".into()
                }
            },
            CILRoot::SetField(info) =>{
                let (field,addr,value) = info.as_ref();
                let addr = Self::node_to_string(asm[*addr].clone(), asm, locals, inputs, sig)?;
                let value = Self::node_to_string(asm[*value].clone(), asm, locals, inputs, sig)?;
                let field = asm[*field];
                let name = escape_ident(&asm[field.name()]);
                format!("({addr})->{name}.f = ({value});")
            }
            CILRoot::Call(info) => {
                let (method, args) = info.as_ref();
                let method = asm[*method].clone();
                let call_args = args
                    .iter()
                    .map(|arg| {
                        format!(
                            "({})",
                            Self::node_to_string(asm[*arg].clone(), asm, locals, inputs, sig).unwrap()
                        )
                    })
                    .intersperse(",".into())
                    .collect::<String>();
                let method_name = mref_to_name(&method, asm);
                format!("{method_name}({call_args});")
            }
            CILRoot::StInd(info) => {
                let (addr, value, tpe, is_volitle) = info.as_ref();
                let addr = Self::node_to_string(asm[*addr].clone(), asm, locals, inputs, sig)?;
                let value = Self::node_to_string(asm[*value].clone(), asm, locals, inputs, sig)?;
                if *is_volitle {
                    format!(
                        "*((volatile {tpe}*)({addr})) = ({value});",
                        tpe = c_tpe(*tpe, asm)
                    )
                } else {
                    format!("*({addr}) = ({value});")
                }
            }
            CILRoot::InitBlk(blk) => {
                let (dst, val, count) = blk.as_ref();
                let dst = Self::node_to_string(asm[*dst].clone(), asm, locals, inputs, sig)?;
                let val = Self::node_to_string(asm[*val].clone(), asm, locals, inputs, sig)?;
                let count = Self::node_to_string(asm[*count].clone(), asm, locals, inputs, sig)?;
                format!("memset(({dst}),({val}),({count}));")
            }
            CILRoot::CpBlk(blk) => {
                let (dst, src, len) = blk.as_ref();
                let dst = Self::node_to_string(asm[*dst].clone(), asm, locals, inputs, sig)?;
                let src = Self::node_to_string(asm[*src].clone(), asm, locals, inputs, sig)?;
                let len = Self::node_to_string(asm[*len].clone(), asm, locals, inputs, sig)?;
                format!("memcpy(({dst}),({src}),({len}));")
            }
            CILRoot::CallI(info) => {
                let (fn_ptr, fn_ptr_sig, args) = info.as_ref();
                let fn_ptr_sig = asm[*fn_ptr_sig].clone();
                let call_args = args
                    .iter()
                    .map(|arg| {
                        format!(
                            "({})",
                            Self::node_to_string(asm[*arg].clone(), asm, locals, inputs, sig).unwrap()
                        )
                    })
                    .intersperse(",".into())
                    .collect::<String>();
                let ret = c_tpe(*fn_ptr_sig.output(), asm);
                let args = fn_ptr_sig
                    .inputs()
                    .iter()
                    .map(|i| nonvoid_c_type(*i, asm))
                    .intersperse(",".into())
                    .collect::<String>();
                let fn_ptr = Self::node_to_string(asm[*fn_ptr].clone(), asm, locals, inputs, sig)?;
                format!("((*({ret}(*)({args}))({fn_ptr})))({call_args});")
            }
            CILRoot::ExitSpecialRegion { target, source } => format!("goto bb{target};"),
            CILRoot::ReThrow => todo!(),
            CILRoot::SetStaticField { field, val } => {
                let field = asm[field];
                let class = asm[field.owner()].clone();
                let fname = class_member_name(&asm[class.name()], &asm[field.name()]);
                let val = Self::node_to_string(asm[val].clone(), asm, locals, inputs, sig)?;
                format!("{fname} = {val};")
            }
            CILRoot::CpObj { src, dst, tpe } => todo!(),
            CILRoot::Unreachable(string_idx) => todo!(),
        })
    }
    fn export_method_def(
        asm: &mut Assembly,
        def: &MethodDef,
        method_defs: &mut impl Write,
        method_decls: &mut impl Write,
    ) -> std::io::Result<()> {
        let class = &asm[def.class()];
        let class_name = escape_ident(&asm[class.name()]);
        let mname = escape_ident(&asm[def.name()]);
        // Workaround for `get_environ` - a .NET specific function, irrelevant to our use case.
        if mname == "get_environ" || mname == "malloc" || mname == "realloc" || mname == "free" {
            return Ok(());
        }
        let method_name = mref_to_name(&def.ref_to(), asm);
        let output = c_tpe(def.ref_to().output(asm), asm);
        match def.resolved_implementation(asm) {
            MethodImpl::MethodBody { blocks, locals } => (),
            MethodImpl::Extern {
                lib,
                preserve_errno,
            } => match mname.as_str() {
                "printf"
                | "puts"
                | "memcmp"
                | "memcpy"
                | "strlen"
                | "rename"
                | "realpath"
                | "unsetenv"
                | "setenv"
                | "getenv"
                | "syscall"
                | "fcntl"
                | "execvp"
                | "pthread_create_wrapper"
                | "pthread_getattr_np"
                | "ioctl"
                | "pthread_attr_destroy"
                | "pthread_attr_init"
                | "pthread_attr_getstack"
                | "sched_getaffinity"
                | "poll" => return Ok(()),
                _ => {
                    let inputs = def
                        .ref_to()
                        .stack_inputs(asm)
                        .iter()
                        .map(|i| nonvoid_c_type(*i, asm))
                        .intersperse(",".into())
                        .collect::<String>();
                    writeln!(method_decls, "{output} {method_name}({inputs});")?;
                    return Ok(());
                }
            },
            MethodImpl::Missing => {
                let inputs = def
                    .ref_to()
                    .stack_inputs(asm)
                    .iter()
                    .map(|i| nonvoid_c_type(*i, asm))
                    .intersperse(",".into())
                    .collect::<String>();
                writeln!(
                    method_defs,
                    "{output} {method_name}({inputs}){{eprintf(\"Missing method {method_name}\\n\");abort();}}"
                )?;
                return Ok(());
            }
            MethodImpl::AliasFor(method_ref_idx) => panic!("Impossible: unrechable reached."),
        }
        let sig = def.sig();
        let stack_inputs = def.stack_inputs(asm);
        let inputs = stack_inputs
            .iter()
            .enumerate()
            .map(|(idx, (tpe, name))| match name {
                Some(name) => format!(
                    "{} {name}",
                    nonvoid_c_type(*tpe, asm),
                    name = escape_ident(&asm[*name]),
                ),
                None => format!("{} A{idx} ", nonvoid_c_type(*tpe, asm)),
            })
            .intersperse(",".into())
            .collect::<String>();
        writeln!(method_defs, "{output} {method_name}({inputs}){{")?;
        let locals: Vec<_> = def.iter_locals(asm).copied().collect();
        for (idx, (lname, local_type)) in locals.iter().enumerate() {
            // If the name of this local is found multiple times, use the L form.

            writeln!(
                method_defs,
                "{local_type} {lname};",
                lname = local_name(&locals, asm, idx as u32),
                local_type = nonvoid_c_type(asm[*local_type], asm),
            )?;
        }
        let blocks = def.blocks(asm).unwrap().to_vec();
        for block in blocks {
            writeln!(method_defs, "bb{}:", block.block_id())?;
            for root_idx in block.roots() {
                if let Err(err) = asm[*root_idx].clone().typecheck(sig, &locals, asm) {
                    eprintln!("Typecheck error:{err:?}");
                    writeln!(method_defs, "fprintf(stderr,\"Attempted to execute a statement which failed to compile.\" {err:?}); abort();",err = format!("{err:?}"))?;
                    continue;
                }

                let root = Self::root_to_string(
                    asm[*root_idx].clone(),
                    asm,
                    &locals[..],
                    &stack_inputs[..],
                    sig,
                );

                match root {
                    Ok(root) => {
                        if root.is_empty() {
                            continue;
                        }
                        writeln!(method_defs, "{root}")?
                    }
                    Err(err) => {
                        eprintln!("Typecheck error:{err:?}");
                        writeln!(method_defs, "fprintf(stderr,\"Attempted to execute a statement which failed to compile.\" {err:?}); abort();",err = format!("{err:?}"))?
                    }
                }
            }
        }
        writeln!(method_defs, "}}")
    }
    #[allow(clippy::too_many_arguments)]
    fn export_class(
        &self,
        asm: &mut super::Assembly,
        defid: ClassDefIdx,
        method_decls: &mut impl Write,
        method_defs: &mut impl Write,
        type_defs: &mut impl Write,
        defined_types: &mut FxHashSet<ClassDefIdx>,
        delayed_defs: &mut FxHashSet<ClassDefIdx>,
        extrn: bool,
    ) -> std::io::Result<()> {
        let class = asm[defid].clone();
        // Checks if this def needs to be delayed, if one of its fields is not yet defined
        if !class
            .fields()
            .iter()
            .filter_map(|(tpe, _, _)| tpe.as_class_ref())
            .filter_map(|cref| asm.class_ref_to_def(cref))
            .all(|cdef| defined_types.contains(&cdef))
        {
            delayed_defs.insert(defid);
            return Ok(());
        }
        let class_name = escape_ident(&asm[class.name()]);
        writeln!(type_defs, "typedef union {class_name}{{")?;
        for (field_tpe, fname, offset) in class.fields() {
            let fname = escape_ident(&asm[*fname]);
            let Some(offset) = offset else {
                eprintln!(
                    "ERR: Can't export field {fname} of {class_name}, becuase it has no offset."
                );
                continue;
            };
            let field_tpe = c_tpe(*field_tpe, asm);
            let pad = if *offset != 0 {
                format!("char pad[{offset}];")
            } else {
                "".into()
            };
            writeln!(type_defs, "struct {{{pad} {field_tpe} f;}}{fname};")?;
        }
        if let Some(size) = class.explict_size() {
            writeln!(type_defs, "char force_size[{size}];", size = size.get())?;
        }
        writeln!(type_defs, "}} {class_name};")?;
        for (sfield_tpe, sfname, is_thread_local) in class.static_fields() {
            let fname = escape_ident(&asm[*sfname]);
            let field_tpe = c_tpe(*sfield_tpe, asm);
            let fname = class_member_name(&class_name, &fname);
            let extrn = if extrn { "extern" } else { "" };
            if *is_thread_local {
                writeln!(type_defs, "{extrn} _Thread_local {field_tpe} {fname};")?;
            } else {
                writeln!(type_defs, "{extrn} {field_tpe} {fname};")?;
            }
        }
        for method in class.methods() {
            let mref = &asm[method.0].clone();
            let def = asm[*method].clone();
            let is_extern = def.resolved_implementation(asm).is_extern();
            Self::export_method_def(asm, &def, method_defs, method_decls)?;
            if !is_extern {
                Self::export_method_decl(asm, mref, method_decls)?;
            }
        }
        defined_types.insert(defid);
        Ok(())
    }
    fn export_to_write(
        &self,
        asm: &super::Assembly,
        out: &mut impl Write,
        lib: bool,
        extrn: bool,
    ) -> std::io::Result<()> {
        let mut asm = asm.clone();

        let mut method_defs = Vec::new();
        let mut method_decls = Vec::new();
        let mut type_defs = Vec::new();
        let mut defined_types: FxHashSet<ClassDefIdx> = FxHashSet::default();
        let mut delayed_defs: FxHashSet<ClassDefIdx> = asm.iter_class_def_ids().cloned().collect();
        let mut delayed_defs_copy: FxHashSet<ClassDefIdx> = FxHashSet::default();
        while !delayed_defs.is_empty() {
            std::mem::swap(&mut delayed_defs, &mut delayed_defs_copy);
            for class_def in &delayed_defs_copy {
                self.export_class(
                    &mut asm,
                    *class_def,
                    &mut method_decls,
                    &mut method_defs,
                    &mut type_defs,
                    &mut defined_types,
                    &mut delayed_defs,
                    extrn,
                )?;
            }
            delayed_defs_copy.clear();
        }
        let mut header: String = include_str!("c_header.h").into();
        if !asm.has_tcctor() {
            header = header.replace("void _tcctor();", "");
            header = header.replace("_tcctor();", "");
        }

        out.write_all(header.as_bytes())?;
        out.write_all(b"\n/*END OF BUILTIN HEADER*/\n")?;
        out.write_all(&type_defs)?;
        out.write_all(b"\n/*END OF TYPEDEFS*/\n")?;
        out.write_all(&method_decls)?;
        out.write_all(b"\n/*END OF METHODECLS*/\n")?;
        out.write_all(&method_defs)?;
        if !lib {
            call_entry(out, &asm)?;
        }
        Ok(())
    }
}
fn call_entry(out: &mut impl Write, asm: &Assembly) -> Result<(), std::io::Error> {
    let cctor_call = if asm.has_cctor() { "_cctor();" } else { "" };

    writeln!(out,"int main(int argc_input, char** argv_input){{argc = argc_input; argv = argv_input; {cctor_call}entrypoint((void *)0); return 0;}}")?;
    Ok(())
}
impl CExporter {
    fn export_to_file(
        &self,
        c_path: &Path,
        asm: &Assembly,
        target: &Path,
        lib: bool,
        extrn: bool,
    ) -> Result<(), std::io::Error> {
        let mut c_out = std::io::BufWriter::new(std::fs::File::create(c_path)?);
        println!("Exporting {c_path:?}");

        self.export_to_write(asm, &mut c_out, lib, extrn)?;
        println!("Exported {c_path:?}");
        // Needed to ensure the IL file is valid!
        c_out.flush().unwrap();
        drop(c_out);

        let mut cmd = std::process::Command::new(std::env::var("CC").unwrap_or("cc".to_owned()));
        cmd.arg(c_path).arg("-o").arg(target).arg("-g");

        if *UB_CHECKS && *PARTS == 1 {
            cmd.args([
                "-fsanitize=undefined,alignment",
                "-fno-sanitize=leak",
                "-fno-sanitize-recover",
                "-O0",
            ]);
        } else if !*NO_OPT {
            cmd.arg("-Ofast");
        } else {
            cmd.arg("-O0");
        };
        if lib {
            cmd.arg("-c");
        } else {
            cmd.arg("-lm");
        }
        if *ANSI_C {
            cmd.arg("-std=c89");
        }
        println!("Compiling {c_path:?}");
        let out = cmd.output().unwrap();
        println!("Compiled {c_path:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !*LINKER_RECOVER {
            assert!(
                !(stderr.contains("error") || stderr.contains("fatal")),
                "stdout:{} stderr:{} cmd:{cmd:?}",
                stdout,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        Ok(())
    }
}
impl Exporter for CExporter {
    type Error = std::io::Error;

    fn export(&self, asm: &super::Assembly, target: &std::path::Path) -> Result<(), Self::Error> {
        if *PARTS == 1 {
            // The IL file should be next to the target
            let c_path = target.with_extension("c");
            self.export_to_file(&c_path, asm, target, self.is_lib, false)
        } else {
            let mut parts = vec![];
            for (id, part) in asm.split_to_parts(*PARTS).enumerate() {
                let name = target.file_stem().unwrap().to_string_lossy().into_owned();
                let target = target
                    .with_file_name(format!("{name}_{id}"))
                    .with_extension("o");
                let c_path = target.with_extension("c");
                self.export_to_file(&c_path, &part, &target, true, true)?;
                parts.push(target);
            }

            let mut cmd =
                std::process::Command::new(std::env::var("CC").unwrap_or("cc".to_owned()));

            cmd.args(parts);
            cmd.arg("-o").arg(target).arg("-g").arg("-lm");

            let c_path = target.with_extension("c");
            let only_statics = asm.only_statics();

            self.export_to_file(&c_path, &only_statics, target, true, false)?;
            let mut option = std::fs::OpenOptions::new();
            option.read(true);
            option.append(true);
            if !self.is_lib {
                let mut c_file = option.open(&c_path).unwrap();
                call_entry(&mut c_file, asm).unwrap();
            } else {
                cmd.arg("-c");
            }
            cmd.arg(c_path);

            println!("Linking {target:?}");
            let out = cmd.output().unwrap();
            println!("Linked {target:?}");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !*LINKER_RECOVER {
                assert!(
                    !(stderr.contains("error") || stderr.contains("fatal")),
                    "stdout:{} stderr:{} cmd:{cmd:?}",
                    stdout,
                    String::from_utf8_lossy(&out.stderr)
                );
            }

            Ok(())
        }
    }
}

#[must_use]
pub fn class_to_mangled(class: &super::ClassRef, asm: &Assembly) -> String {
    let assembly = match class.asm() {
        Some(asm_idx) => &asm[asm_idx],
        None => "",
    };
    format!("{assembly}{name}", name = escape_ident(&asm[class.name()]))
}
#[must_use]
pub fn name_sig_class_to_mangled(
    name: &str,
    sig: super::SigIdx,
    class: Option<ClassRefIdx>,
    asm: &Assembly,
) -> String {
    let class = match class {
        Some(_) => todo!(),
        None => todo!(),
    };
}
