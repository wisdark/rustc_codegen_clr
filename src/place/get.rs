use crate::r#type::Type;
use crate::utilis::monomorphize;
use cilly::call_site::CallSite;
use cilly::cil_node::CILNode;
use cilly::field_desc::FieldDescriptor;
use cilly::fn_sig::FnSig;
use cilly::{call, conv_usize, ld_field};

use rustc_middle::mir::{Place, PlaceElem};
use rustc_middle::ty::{Instance, TyCtxt, TyKind};

pub(super) fn local_get(local: usize, method: &rustc_middle::mir::Body) -> CILNode {
    if let Some(spread_arg) = method.spread_arg
        && local == spread_arg.as_usize()
    {
        return CILNode::LDLoc(
            (method.local_decls.len() - method.arg_count)
                .try_into()
                .unwrap(),
        );
    }
    if local == 0 {
        CILNode::LDLoc(0)
    } else if local > method.arg_count {
        CILNode::LDLoc(
            u32::try_from(local - method.arg_count)
                .expect("Method has more than 2^32 local varaibles"),
        )
    } else {
        CILNode::LDArg(u32::try_from(local - 1).expect("Method has more than 2^32 local variables"))
    }
}
/// Returns the ops for getting the value of place.
pub fn place_get<'tyctx>(
    place: &Place<'tyctx>,
    tyctx: TyCtxt<'tyctx>,
    method: &rustc_middle::mir::Body<'tyctx>,
    method_instance: Instance<'tyctx>,
    type_cache: &mut crate::r#type::TyCache,
) -> CILNode {
    if place.projection.is_empty() {
        local_get(place.local.as_usize(), method)
    } else {
        let (mut op, mut ty) =
            super::local_body(place.local.as_usize(), method, tyctx, &method_instance);
        ty = crate::utilis::monomorphize(&method_instance, ty, tyctx);
        let mut ty = ty.into();

        let (head, body) = super::slice_head(place.projection);
        for elem in body {
            let (curr_ty, curr_ops) =
                super::place_elem_body(elem, ty, tyctx, method_instance, method, type_cache, op);
            ty = curr_ty.monomorphize(&method_instance, tyctx);
            op = curr_ops;
        }
        place_elem_get(head, ty, tyctx, method_instance, method, type_cache, op)
    }
}

fn place_elem_get<'a>(
    place_elem: &PlaceElem<'a>,
    curr_type: super::PlaceTy<'a>,
    tyctx: TyCtxt<'a>,
    method_instance: Instance<'a>,
    method: &rustc_middle::mir::Body<'a>,
    type_cache: &mut crate::r#type::TyCache,
    addr_calc: CILNode,
) -> CILNode {
    match place_elem {
        PlaceElem::Deref => super::deref_op(
            super::pointed_type(curr_type).into(),
            tyctx,
            &method_instance,
            type_cache,
            addr_calc,
        ),
        PlaceElem::Field(field_index, _field_type) => match curr_type {
            super::PlaceTy::Ty(curr_type) => {
                let curr_type = crate::utilis::monomorphize(&method_instance, curr_type, tyctx);
                let _field_type = crate::utilis::monomorphize(&method_instance, curr_type, tyctx);

                let field_desc = crate::utilis::field_descrptor(
                    curr_type,
                    (*field_index).into(),
                    tyctx,
                    method_instance,
                    type_cache,
                );
                CILNode::LDField {
                    addr: addr_calc.into(),
                    field: field_desc.into(),
                }
            }
            super::PlaceTy::EnumVariant(enm, var_idx) => {
                let owner = crate::utilis::monomorphize(&method_instance, enm, tyctx);
                let field_desc = crate::utilis::enum_field_descriptor(
                    owner,
                    field_index.as_u32(),
                    var_idx,
                    tyctx,
                    method_instance,
                    type_cache,
                );
                CILNode::LDField {
                    addr: addr_calc.into(),
                    field: field_desc.into(),
                }
            }
        },
        PlaceElem::Index(index) => {
            let curr_ty = curr_type
                .as_ty()
                .expect("INVALID PLACE: Indexing into enum variant???");

            let index_type = monomorphize(&method_instance, method.local_decls[*index].ty, tyctx);
            let index = crate::place::local_get(index.as_usize(), method);

            match curr_ty.kind() {
                TyKind::Slice(inner) => {
                    let inner = crate::utilis::monomorphize(&method_instance, *inner, tyctx);
                    let inner_type = type_cache.type_from_cache(inner, tyctx, method_instance);
                    let slice = type_cache
                        .slice_ty(inner, tyctx, method_instance)
                        .as_dotnet()
                        .unwrap();

                    let index_type = type_cache.type_from_cache(index_type, tyctx, method_instance);
                    let desc = FieldDescriptor::new(
                        slice,
                        Type::Ptr(Type::Void.into()),
                        "data_pointer".into(),
                    );
                    let size = crate::casts::int_to_int(
                        Type::I32,
                        &index_type,
                        CILNode::SizeOf(inner_type.clone().into()),
                    );
                    let addr = CILNode::Add(
                        Box::new(CILNode::TransmutePtr {
                            val: CILNode::LDField {
                                addr: addr_calc.into(),
                                field: desc.into(),
                            }
                            .into(),
                            new_ptr: Box::new(Type::Ptr(Box::new(inner_type))),
                        }),
                        CILNode::Mul(index.into(), size.into()).into(),
                    );
                    super::deref_op(
                        super::PlaceTy::Ty(inner),
                        tyctx,
                        &method_instance,
                        type_cache,
                        addr,
                    )
                }
                TyKind::Array(element, _length) => {
                    let element = crate::utilis::monomorphize(&method_instance, *element, tyctx);
                    let element = type_cache.type_from_cache(element, tyctx, method_instance);
                    let array_type = type_cache.type_from_cache(curr_ty, tyctx, method_instance);
                    let array_dotnet = array_type.as_dotnet().expect("Non array type");
                    call!(
                        CallSite::new(
                            Some(array_dotnet),
                            "get_Item".into(),
                            FnSig::new(&[Type::Ptr(array_type.into()), Type::USize], element),
                            false,
                        ),
                        [addr_calc, CILNode::ZeroExtendToUSize(index.into())]
                    )
                }
                _ => {
                    rustc_middle::ty::print::with_no_trimmed_paths! {todo!("Can't index into {curr_ty}!")}
                }
            }
        }
        PlaceElem::ConstantIndex {
            offset,
            min_length,
            from_end,
        } => {
            let _ = min_length;
            let curr_ty = curr_type
                .as_ty()
                .expect("INVALID PLACE: Indexing into enum variant???");
            let index = CILNode::LdcU64(*offset);
            assert!(!from_end, "Indexing slice form end");
            match curr_ty.kind() {
                TyKind::Slice(inner) => {
                    let inner = crate::utilis::monomorphize(&method_instance, *inner, tyctx);
                    let inner_type = type_cache.type_from_cache(inner, tyctx, method_instance);
                    let slice = type_cache
                        .slice_ty(inner, tyctx, method_instance)
                        .as_dotnet()
                        .unwrap();
                    let data_pointer = FieldDescriptor::new(
                        slice.clone(),
                        Type::Ptr(Type::Void.into()),
                        "data_pointer".into(),
                    );
                    let metadata = FieldDescriptor::new(slice, Type::USize, "metadata".into());

                    let addr = CILNode::TransmutePtr {
                        val: Box::new(ld_field!(addr_calc.clone(), data_pointer)),
                        new_ptr: Box::new(Type::Ptr(Box::new(inner_type.clone()))),
                    } + call!(
                        CallSite::new(
                            None,
                            "bounds_check".into(),
                            FnSig::new(&[Type::USize, Type::USize], Type::USize),
                            true
                        ),
                        [conv_usize!(index), ld_field!(addr_calc, metadata),]
                    ) * CILNode::ZeroExtendToUSize(
                        CILNode::SizeOf(inner_type.into()).into(),
                    );
                    super::deref_op(
                        super::PlaceTy::Ty(inner),
                        tyctx,
                        &method_instance,
                        type_cache,
                        addr,
                    )
                }
                TyKind::Array(element, _length) => {
                    let element = crate::utilis::monomorphize(&method_instance, *element, tyctx);
                    let element = type_cache.type_from_cache(element, tyctx, method_instance);
                    let array_type = type_cache.type_from_cache(curr_ty, tyctx, method_instance);
                    let array_dotnet = array_type.as_dotnet().expect("Non array type");
                    //eprintln!("WARNING: ConstantIndex has required min_length of {min_length}, but bounds checking on const access not supported yet!");
                    call!(
                        CallSite::new(
                            Some(array_dotnet),
                            "get_Item".into(),
                            FnSig::new(&[Type::Ptr(array_type.into()), Type::USize], element),
                            false,
                        ),
                        [addr_calc, CILNode::ZeroExtendToUSize(index.into())]
                    )
                }
                _ => {
                    rustc_middle::ty::print::with_no_trimmed_paths! { todo!("Can't index into {curr_ty}!")}
                }
            }
        }
        PlaceElem::Subtype(tpe) => {
            let tpe = crate::utilis::monomorphize(&method_instance, *tpe, tyctx);
            super::deref_op(
                super::PlaceTy::Ty(tpe),
                tyctx,
                &method_instance,
                type_cache,
                addr_calc,
            )
        }
        _ => todo!("Can't handle porojection {place_elem:?} in get"),
    }
}
