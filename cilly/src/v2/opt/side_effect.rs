use std::collections::HashMap;

use crate::v2::{Assembly, CILNode, NodeIdx};
#[derive(Default)]
pub struct SideEffectInfoCache {
    side_effects: HashMap<NodeIdx, bool>,
}
impl SideEffectInfoCache {
    /// Checks if a node may have side effects(if dupilcating it and poping the result would change the way a program runs).
    #[allow(clippy::match_same_arms)]
    pub fn has_side_effects(&mut self, node: NodeIdx, asm: &Assembly) -> bool {
        if let Some(side_effect) = self.side_effects.get(&node) {
            return *side_effect;
        }
        let side_effect = match asm.get_node(node) {
            CILNode::LdTypeToken(_)
            | CILNode::LdFtn(_)
            | CILNode::Const(_)
            | CILNode::SizeOf(_) => false, // Constant, can't have side effects
            CILNode::BinOp(lhs, rhs, _) => {
                self.has_side_effects(*lhs, asm) || self.has_side_effects(*rhs, asm)
            }
            CILNode::UnOp(arg, _) => self.has_side_effects(*arg, asm), // UnOp, only has side effects if its arg has side effects
            CILNode::LdLoc(_) | CILNode::LdArg(_) => false, // Reading a variable has no side effects
            CILNode::LdLocA(_) | CILNode::LdArgA(_) => false, // Getting the address of something has no side effects.
            CILNode::Call(_) => true, // For now, we assume all calls have side effects.
            CILNode::RefToPtr(input)
            | CILNode::IntCast { input, .. }
            | CILNode::FloatCast { input, .. }
            | CILNode::PtrCast(input, _) => self.has_side_effects(*input, asm), // Casts don't have side effects, unless their input has one.
            CILNode::LdFieldAdress { addr, .. }
            | CILNode::LdField { addr, .. }
            | CILNode::LdInd { addr, .. } => self.has_side_effects(*addr, asm), // Reading a pointer or a field never has side effects.
            CILNode::GetException => true, // This is a low-level, unsafe operation, which manipulates the runtime stack, and can't be preformed twice. It for sure has side effects.
            CILNode::UnboxAny { object, .. }
            | CILNode::IsInst(object, _)
            | CILNode::CheckedCast(object, _) => {
                self.has_side_effects(*object, asm) // Class checks / casts / unboxes have no side effects.
            }
            CILNode::CallI(_) => true, // Indidrect calls may have side effects
            CILNode::LocAllocAlgined { .. } | CILNode::LocAlloc { .. } => true, // Allocation has side effects
            CILNode::LdStaticField(_) => false, // Loading static fields has no side effects.
            CILNode::LdLen(arr) => self.has_side_effects(*arr, asm), // Loading a length only has side effects if the index has array.
            CILNode::LdElelemRef { array, index } => {
                self.has_side_effects(*array, asm) || self.has_side_effects(*index, asm)
                // Indexing only has side effects if the index or array address has side effects.
            }
        };
        self.side_effects.insert(node, side_effect);
        side_effect
    }
}
#[test]
fn const_no_side_effect() {
    use crate::v2::{
        hashable::{HashableF32, HashableF64},
        Const,
    };
    let consts = [
        Const::Bool(true),
        Const::Bool(false),
        Const::F32(HashableF32(std::f32::consts::PI)),
        Const::F64(HashableF64(std::f64::consts::PI)),
        Const::I8(5),
        Const::U8(5),
        Const::I16(5),
        Const::U16(5),
        Const::I32(5),
        Const::U32(5),
        Const::I64(5),
        Const::U64(5),
        //Const::I128(5),
        //Const::U128(5),
    ];
    let mut asm = Assembly::default();
    let mut cache = SideEffectInfoCache::default();
    for cst in consts {
        let node = asm.alloc_node(cst);
        assert!(!cache.has_side_effects(node, &asm));
        let node = asm.biop(cst, cst, crate::v2::BinOp::Add);
        let node = asm.alloc_node(node);
        assert!(!cache.has_side_effects(node, &asm));
        let node = asm.biop(CILNode::LocAlloc { size: node }, cst, crate::v2::BinOp::Add);
        let node = asm.alloc_node(node);
        assert!(cache.has_side_effects(node, &asm));
        let node = asm.biop(cst, CILNode::LocAlloc { size: node }, crate::v2::BinOp::Add);
        let node = asm.alloc_node(node);
        assert!(cache.has_side_effects(node, &asm));
    }
}
