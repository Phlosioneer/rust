// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::fmt;
use rustc::hir;
use rustc::mir::*;
use rustc::middle::const_val::{ConstInt, ConstVal};
use rustc::middle::lang_items;
use rustc::ty::{self, Ty};
use rustc::ty::subst::{Kind, Substs};
use rustc::ty::util::IntTypeExt;
use rustc_data_structures::indexed_vec::Idx;
use util::patch::MirPatch;

use std::iter;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum DropFlagState {
    Present, // i.e. initialized
    Absent, // i.e. deinitialized or "moved"
}

impl DropFlagState {
    pub fn value(self) -> bool {
        match self {
            DropFlagState::Present => true,
            DropFlagState::Absent => false
        }
    }
}

#[derive(Debug)]
pub enum DropStyle {
    Dead,
    Static,
    Conditional,
    Open,
}

#[derive(Debug)]
pub enum DropFlagMode {
    Shallow,
    Deep
}

#[derive(Copy, Clone, Debug)]
pub enum Unwind {
    To(BasicBlock),
    InCleanup
}

impl Unwind {
    fn is_cleanup(self) -> bool {
        match self {
            Unwind::To(..) => false,
            Unwind::InCleanup => true
        }
    }

    fn into_option(self) -> Option<BasicBlock> {
        match self {
            Unwind::To(bb) => Some(bb),
            Unwind::InCleanup => None,
        }
    }

    fn map<F>(self, f: F) -> Self where F: FnOnce(BasicBlock) -> BasicBlock {
        match self {
            Unwind::To(bb) => Unwind::To(f(bb)),
            Unwind::InCleanup => Unwind::InCleanup
        }
    }
}

pub trait DropElaborator<'a, 'tcx: 'a> : fmt::Debug {
    type Path : Copy + fmt::Debug;

    fn patch(&mut self) -> &mut MirPatch<'tcx>;
    fn mir(&self) -> &'a Mir<'tcx>;
    fn tcx(&self) -> ty::TyCtxt<'a, 'tcx, 'tcx>;
    fn param_env(&self) -> ty::ParamEnv<'tcx>;

    fn drop_style(&self, path: Self::Path, mode: DropFlagMode) -> DropStyle;
    fn get_drop_flag(&mut self, path: Self::Path) -> Option<Operand<'tcx>>;
    fn clear_drop_flag(&mut self, location: Location, path: Self::Path, mode: DropFlagMode);


    fn field_subpath(&self, path: Self::Path, field: Field) -> Option<Self::Path>;
    fn deref_subpath(&self, path: Self::Path) -> Option<Self::Path>;
    fn downcast_subpath(&self, path: Self::Path, variant: usize) -> Option<Self::Path>;
}

#[derive(Debug)]
struct DropCtxt<'l, 'b: 'l, 'tcx: 'b, D>
    where D : DropElaborator<'b, 'tcx> + 'l
{
    elaborator: &'l mut D,

    source_info: SourceInfo,

    lvalue: &'l Lvalue<'tcx>,
    path: D::Path,
    succ: BasicBlock,
    unwind: Unwind,
}

pub fn elaborate_drop<'b, 'tcx, D>(
    elaborator: &mut D,
    source_info: SourceInfo,
    lvalue: &Lvalue<'tcx>,
    path: D::Path,
    succ: BasicBlock,
    unwind: Unwind,
    bb: BasicBlock)
    where D: DropElaborator<'b, 'tcx>
{
    DropCtxt {
        elaborator, source_info, lvalue, path, succ, unwind
    }.elaborate_drop(bb)
}

impl<'l, 'b, 'tcx, D> DropCtxt<'l, 'b, 'tcx, D>
    where D: DropElaborator<'b, 'tcx>
{
    fn lvalue_ty(&self, lvalue: &Lvalue<'tcx>) -> Ty<'tcx> {
        lvalue.ty(self.elaborator.mir(), self.tcx()).to_ty(self.tcx())
    }

    fn tcx(&self) -> ty::TyCtxt<'b, 'tcx, 'tcx> {
        self.elaborator.tcx()
    }

    /// This elaborates a single drop instruction, located at `bb`, and
    /// patches over it.
    ///
    /// The elaborated drop checks the drop flags to only drop what
    /// is initialized.
    ///
    /// In addition, the relevant drop flags also need to be cleared
    /// to avoid double-drops. However, in the middle of a complex
    /// drop, one must avoid clearing some of the flags before they
    /// are read, as that would cause a memory leak.
    ///
    /// In particular, when dropping an ADT, multiple fields may be
    /// joined together under the `rest` subpath. They are all controlled
    /// by the primary drop flag, but only the last rest-field dropped
    /// should clear it (and it must also not clear anything else).
    ///
    /// FIXME: I think we should just control the flags externally
    /// and then we do not need this machinery.
    pub fn elaborate_drop<'a>(&mut self, bb: BasicBlock) {
        debug!("elaborate_drop({:?})", self);
        let style = self.elaborator.drop_style(self.path, DropFlagMode::Deep);
        debug!("elaborate_drop({:?}): live - {:?}", self, style);
        match style {
            DropStyle::Dead => {
                self.elaborator.patch().patch_terminator(bb, TerminatorKind::Goto {
                    target: self.succ
                });
            }
            DropStyle::Static => {
                let loc = self.terminator_loc(bb);
                self.elaborator.clear_drop_flag(loc, self.path, DropFlagMode::Deep);
                self.elaborator.patch().patch_terminator(bb, TerminatorKind::Drop {
                    location: self.lvalue.clone(),
                    target: self.succ,
                    unwind: self.unwind.into_option(),
                });
            }
            DropStyle::Conditional => {
                let unwind = self.unwind; // FIXME(#6393)
                let succ = self.succ;
                let drop_bb = self.complete_drop(Some(DropFlagMode::Deep), succ, unwind);
                self.elaborator.patch().patch_terminator(bb, TerminatorKind::Goto {
                    target: drop_bb
                });
            }
            DropStyle::Open => {
                let drop_bb = self.open_drop();
                self.elaborator.patch().patch_terminator(bb, TerminatorKind::Goto {
                    target: drop_bb
                });
            }
        }
    }

    /// Return the lvalue and move path for each field of `variant`,
    /// (the move path is `None` if the field is a rest field).
    fn move_paths_for_fields(&self,
                             base_lv: &Lvalue<'tcx>,
                             variant_path: D::Path,
                             variant: &'tcx ty::VariantDef,
                             substs: &'tcx Substs<'tcx>)
                             -> Vec<(Lvalue<'tcx>, Option<D::Path>)>
    {
        variant.fields.iter().enumerate().map(|(i, f)| {
            let field = Field::new(i);
            let subpath = self.elaborator.field_subpath(variant_path, field);

            let field_ty =
                self.tcx().normalize_associated_type_in_env(
                    &f.ty(self.tcx(), substs),
                    self.elaborator.param_env()
                );
            (base_lv.clone().field(field, field_ty), subpath)
        }).collect()
    }

    fn drop_subpath(&mut self,
                    lvalue: &Lvalue<'tcx>,
                    path: Option<D::Path>,
                    succ: BasicBlock,
                    unwind: Unwind)
                    -> BasicBlock
    {
        if let Some(path) = path {
            debug!("drop_subpath: for std field {:?}", lvalue);

            DropCtxt {
                elaborator: self.elaborator,
                source_info: self.source_info,
                path, lvalue, succ, unwind,
            }.elaborated_drop_block()
        } else {
            debug!("drop_subpath: for rest field {:?}", lvalue);

            DropCtxt {
                elaborator: self.elaborator,
                source_info: self.source_info,
                lvalue, succ, unwind,
                // Using `self.path` here to condition the drop on
                // our own drop flag.
                path: self.path
            }.complete_drop(None, succ, unwind)
        }
    }

    /// Create one-half of the drop ladder for a list of fields, and return
    /// the list of steps in it in reverse order.
    ///
    /// `unwind_ladder` is such a list of steps in reverse order,
    /// which is called if the matching step of the drop glue panics.
    fn drop_halfladder(&mut self,
                       unwind_ladder: &[Unwind],
                       succ: BasicBlock,
                       fields: &[(Lvalue<'tcx>, Option<D::Path>)])
                       -> Vec<BasicBlock>
    {
        let goto = TerminatorKind::Goto { target: succ };
        let mut succ = self.new_block(unwind_ladder[0], goto);

        // Always clear the "master" drop flag at the bottom of the
        // ladder. This is needed because the "master" drop flag
        // protects the ADT's discriminant, which is invalidated
        // after the ADT is dropped.
        let succ_loc = Location { block: succ, statement_index: 0 };
        self.elaborator.clear_drop_flag(succ_loc, self.path, DropFlagMode::Shallow);

        fields.iter().rev().zip(unwind_ladder).map(|(&(ref lv, path), &unwind_succ)| {
            succ = self.drop_subpath(lv, path, succ, unwind_succ);
            succ
        }).collect()
    }

    /// Create a full drop ladder, consisting of 2 connected half-drop-ladders
    ///
    /// For example, with 3 fields, the drop ladder is
    ///
    /// .d0:
    ///     ELAB(drop location.0 [target=.d1, unwind=.c1])
    /// .d1:
    ///     ELAB(drop location.1 [target=.d2, unwind=.c2])
    /// .d2:
    ///     ELAB(drop location.2 [target=`self.succ`, unwind=`self.unwind`])
    /// .c1:
    ///     ELAB(drop location.1 [target=.c2])
    /// .c2:
    ///     ELAB(drop location.2 [target=`self.unwind`])
    fn drop_ladder<'a>(&mut self,
                       fields: Vec<(Lvalue<'tcx>, Option<D::Path>)>)
                       -> (BasicBlock, Unwind)
    {
        debug!("drop_ladder({:?}, {:?})", self, fields);

        let mut fields = fields;
        fields.retain(|&(ref lvalue, _)| {
            self.lvalue_ty(lvalue).needs_drop(self.tcx(), self.elaborator.param_env())
        });

        debug!("drop_ladder - fields needing drop: {:?}", fields);

        let unwind_ladder = vec![Unwind::InCleanup; fields.len() + 1];
        let unwind_ladder: Vec<_> = if let Unwind::To(target) = self.unwind {
            let halfladder = self.drop_halfladder(&unwind_ladder, target, &fields);
            Some(self.unwind).into_iter().chain(halfladder.into_iter().map(Unwind::To))
                .collect()
        } else {
            unwind_ladder
        };

        let succ = self.succ; // FIXME(#6393)
        let normal_ladder =
            self.drop_halfladder(&unwind_ladder, succ, &fields);

        (normal_ladder.last().cloned().unwrap_or(succ),
         unwind_ladder.last().cloned().unwrap_or(self.unwind))
    }

    fn open_drop_for_tuple<'a>(&mut self, tys: &[Ty<'tcx>])
                               -> BasicBlock
    {
        debug!("open_drop_for_tuple({:?}, {:?})", self, tys);

        let fields = tys.iter().enumerate().map(|(i, &ty)| {
            (self.lvalue.clone().field(Field::new(i), ty),
             self.elaborator.field_subpath(self.path, Field::new(i)))
        }).collect();

        self.drop_ladder(fields).0
    }

    fn open_drop_for_box<'a>(&mut self, ty: Ty<'tcx>) -> BasicBlock
    {
        debug!("open_drop_for_box({:?}, {:?})", self, ty);

        let interior = self.lvalue.clone().deref();
        let interior_path = self.elaborator.deref_subpath(self.path);

        let succ = self.succ; // FIXME(#6393)
        let unwind = self.unwind;
        let succ = self.box_free_block(ty, succ, unwind);
        let unwind_succ = self.unwind.map(|unwind| {
            self.box_free_block(ty, unwind, Unwind::InCleanup)
        });

        self.drop_subpath(&interior, interior_path, succ, unwind_succ)
    }

    fn open_drop_for_adt<'a>(&mut self, adt: &'tcx ty::AdtDef, substs: &'tcx Substs<'tcx>)
                             -> BasicBlock {
        debug!("open_drop_for_adt({:?}, {:?}, {:?})", self, adt, substs);
        if adt.variants.len() == 0 {
            return self.elaborator.patch().new_block(BasicBlockData {
                statements: vec![],
                terminator: Some(Terminator {
                    source_info: self.source_info,
                    kind: TerminatorKind::Unreachable
                }),
                is_cleanup: self.unwind.is_cleanup()
            });
        }

        let contents_drop = if adt.is_union() {
            (self.succ, self.unwind)
        } else {
            self.open_drop_for_adt_contents(adt, substs)
        };

        if adt.has_dtor(self.tcx()) {
            self.destructor_call_block(contents_drop)
        } else {
            contents_drop.0
        }
    }

    fn open_drop_for_adt_contents<'a>(&mut self, adt: &'tcx ty::AdtDef,
                                      substs: &'tcx Substs<'tcx>)
                                      -> (BasicBlock, Unwind) {
        match adt.variants.len() {
            1 => {
                let fields = self.move_paths_for_fields(
                    self.lvalue,
                    self.path,
                    &adt.variants[0],
                    substs
                );
                self.drop_ladder(fields)
            }
            _ => {
                let succ = self.succ;
                let unwind = self.unwind; // FIXME(#6393)

                let mut values = Vec::with_capacity(adt.variants.len());
                let mut normal_blocks = Vec::with_capacity(adt.variants.len());
                let mut unwind_blocks = if unwind.is_cleanup() {
                    None
                } else {
                    Some(Vec::with_capacity(adt.variants.len()))
                };
                let mut otherwise = None;
                let mut unwind_otherwise = None;
                for (variant_index, discr) in adt.discriminants(self.tcx()).enumerate() {
                    let subpath = self.elaborator.downcast_subpath(
                        self.path, variant_index);
                    if let Some(variant_path) = subpath {
                        let base_lv = self.lvalue.clone().elem(
                            ProjectionElem::Downcast(adt, variant_index)
                        );
                        let fields = self.move_paths_for_fields(
                            &base_lv,
                            variant_path,
                            &adt.variants[variant_index],
                            substs);
                        values.push(discr);
                        if let Unwind::To(unwind) = unwind {
                            // We can't use the half-ladder from the original
                            // drop ladder, because this breaks the
                            // "funclet can't have 2 successor funclets"
                            // requirement from MSVC:
                            //
                            //           switch       unwind-switch
                            //          /      \         /        \
                            //         v1.0    v2.0  v2.0-unwind  v1.0-unwind
                            //         |        |      /             |
                            //    v1.1-unwind  v2.1-unwind           |
                            //      ^                                |
                            //       \-------------------------------/
                            //
                            // Create a duplicate half-ladder to avoid that. We
                            // could technically only do this on MSVC, but I
                            // I want to minimize the divergence between MSVC
                            // and non-MSVC.

                            let unwind_blocks = unwind_blocks.as_mut().unwrap();
                            let unwind_ladder = vec![Unwind::InCleanup; fields.len() + 1];
                            let halfladder =
                                self.drop_halfladder(&unwind_ladder, unwind, &fields);
                            unwind_blocks.push(halfladder.last().cloned().unwrap_or(unwind));
                        }
                        let (normal, _) = self.drop_ladder(fields);
                        normal_blocks.push(normal);
                    } else {
                        // variant not found - drop the entire enum
                        if let None = otherwise {
                            otherwise = Some(self.complete_drop(
                                Some(DropFlagMode::Shallow),
                                succ,
                                unwind));
                            if let Unwind::To(unwind) = unwind {
                                unwind_otherwise = Some(self.complete_drop(
                                    Some(DropFlagMode::Shallow),
                                    unwind,
                                    Unwind::InCleanup
                                ));
                            }
                        }
                    }
                }
                if let Some(block) = otherwise {
                    normal_blocks.push(block);
                    if let Some(ref mut unwind_blocks) = unwind_blocks {
                        unwind_blocks.push(unwind_otherwise.unwrap());
                    }
                } else {
                    values.pop();
                }

                (self.adt_switch_block(adt, normal_blocks, &values, succ, unwind),
                 unwind.map(|unwind| {
                     self.adt_switch_block(
                         adt, unwind_blocks.unwrap(), &values, unwind, Unwind::InCleanup
                     )
                 }))
            }
        }
    }

    fn adt_switch_block(&mut self,
                        adt: &'tcx ty::AdtDef,
                        blocks: Vec<BasicBlock>,
                        values: &[ConstInt],
                        succ: BasicBlock,
                        unwind: Unwind)
                        -> BasicBlock {
        // If there are multiple variants, then if something
        // is present within the enum the discriminant, tracked
        // by the rest path, must be initialized.
        //
        // Additionally, we do not want to switch on the
        // discriminant after it is free-ed, because that
        // way lies only trouble.
        let discr_ty = adt.repr.discr_type().to_ty(self.tcx());
        let discr = Lvalue::Local(self.new_temp(discr_ty));
        let discr_rv = Rvalue::Discriminant(self.lvalue.clone());
        let switch_block = self.elaborator.patch().new_block(BasicBlockData {
            statements: vec![
                Statement {
                    source_info: self.source_info,
                    kind: StatementKind::Assign(discr.clone(), discr_rv),
                }
                ],
            terminator: Some(Terminator {
                source_info: self.source_info,
                kind: TerminatorKind::SwitchInt {
                    discr: Operand::Consume(discr),
                    switch_ty: discr_ty,
                    values: From::from(values.to_owned()),
                    targets: blocks,
                }
            }),
            is_cleanup: unwind.is_cleanup(),
        });
        self.drop_flag_test_block(switch_block, succ, unwind)
    }

    fn destructor_call_block<'a>(&mut self, (succ, unwind): (BasicBlock, Unwind))
                                 -> BasicBlock
    {
        debug!("destructor_call_block({:?}, {:?})", self, succ);
        let tcx = self.tcx();
        let drop_trait = tcx.lang_items.drop_trait().unwrap();
        let drop_fn = tcx.associated_items(drop_trait).next().unwrap();
        let ty = self.lvalue_ty(self.lvalue);
        let substs = tcx.mk_substs(iter::once(Kind::from(ty)));

        let ref_ty = tcx.mk_ref(tcx.types.re_erased, ty::TypeAndMut {
            ty: ty,
            mutbl: hir::Mutability::MutMutable
        });
        let ref_lvalue = self.new_temp(ref_ty);
        let unit_temp = Lvalue::Local(self.new_temp(tcx.mk_nil()));

        self.elaborator.patch().new_block(BasicBlockData {
            statements: vec![Statement {
                source_info: self.source_info,
                kind: StatementKind::Assign(
                    Lvalue::Local(ref_lvalue),
                    Rvalue::Ref(tcx.types.re_erased, BorrowKind::Mut, self.lvalue.clone())
                )
            }],
            terminator: Some(Terminator {
                kind: TerminatorKind::Call {
                    func: Operand::function_handle(tcx, drop_fn.def_id, substs,
                                                   self.source_info.span),
                    args: vec![Operand::Consume(Lvalue::Local(ref_lvalue))],
                    destination: Some((unit_temp, succ)),
                    cleanup: unwind.into_option(),
                },
                source_info: self.source_info
            }),
            is_cleanup: unwind.is_cleanup(),
        })
    }

    /// create a loop that drops an array:
    ///
    /// loop-block:
    ///    can_go = index < len
    ///    if can_go then drop-block else succ
    /// drop-block:
    ///    ptr = &mut LV[index]
    ///    index = index + 1
    ///    drop(ptr)
    fn drop_loop(&mut self,
                 succ: BasicBlock,
                 index: &Lvalue<'tcx>,
                 length: &Lvalue<'tcx>,
                 ety: Ty<'tcx>,
                 unwind: Unwind)
                 -> BasicBlock
    {
        let use_ = |lv: &Lvalue<'tcx>| Operand::Consume(lv.clone());
        let tcx = self.tcx();

        let ref_ty = tcx.mk_ref(tcx.types.re_erased, ty::TypeAndMut {
            ty: ety,
            mutbl: hir::Mutability::MutMutable
        });
        let ptr = &Lvalue::Local(self.new_temp(ref_ty));
        let can_go = &Lvalue::Local(self.new_temp(tcx.types.bool));

        let one = self.constant_usize(1);
        let drop_block = self.elaborator.patch().new_block(BasicBlockData {
            statements: vec![
                Statement { source_info: self.source_info, kind: StatementKind::Assign(
                    ptr.clone(), Rvalue::Ref(
                        tcx.types.re_erased, BorrowKind::Mut,
                        self.lvalue.clone().index(use_(index))
                    ),
                )},
                Statement { source_info: self.source_info, kind: StatementKind::Assign(
                    index.clone(), Rvalue::BinaryOp(BinOp::Add, use_(index), one)
                )},
            ],
            is_cleanup: unwind.is_cleanup(),
            terminator: Some(Terminator {
                source_info: self.source_info,
                // this gets overwritten by drop elaboration.
                kind: TerminatorKind::Unreachable,
            })
        });

        let loop_block = self.elaborator.patch().new_block(BasicBlockData {
            statements: vec![
                Statement { source_info: self.source_info, kind: StatementKind::Assign(
                    can_go.clone(), Rvalue::BinaryOp(BinOp::Lt, use_(index), use_(length))
                )},
            ],
            is_cleanup: unwind.is_cleanup(),
            terminator: Some(Terminator {
                source_info: self.source_info,
                kind: TerminatorKind::if_(tcx, use_(can_go), drop_block, succ)
            })
        });

        self.elaborator.patch().patch_terminator(drop_block, TerminatorKind::Drop {
            location: ptr.clone().deref(),
            target: loop_block,
            unwind: unwind.into_option()
        });

        loop_block
    }

    fn open_drop_for_array(&mut self, ety: Ty<'tcx>) -> BasicBlock {
        debug!("open_drop_for_array({:?})", ety);
        // FIXME: using an index instead of a pointer to avoid
        // special-casing ZSTs.
        let tcx = self.tcx();
        let index = &Lvalue::Local(self.new_temp(tcx.types.usize));
        let length = &Lvalue::Local(self.new_temp(tcx.types.usize));

        let unwind = self.unwind.map(|unwind| {
            self.drop_loop(unwind, index, length, ety, Unwind::InCleanup)
        });

        let succ = self.succ; // FIXME(#6393)
        let loop_block = self.drop_loop(succ, index, length, ety, unwind);

        let zero = self.constant_usize(0);
        let drop_block = self.elaborator.patch().new_block(BasicBlockData {
            statements: vec![
                Statement { source_info: self.source_info, kind: StatementKind::Assign(
                    length.clone(), Rvalue::Len(self.lvalue.clone())
                )},
                Statement { source_info: self.source_info, kind: StatementKind::Assign(
                    index.clone(), Rvalue::Use(zero),
                )},
            ],
            is_cleanup: unwind.is_cleanup(),
            terminator: Some(Terminator {
                source_info: self.source_info,
                kind: TerminatorKind::Goto { target: loop_block }
            })
        });

        // FIXME(#34708): handle partially-dropped array/slice elements.
        self.drop_flag_test_and_reset_block(
            Some(DropFlagMode::Deep), drop_block, succ, unwind)
    }

    /// The slow-path - create an "open", elaborated drop for a type
    /// which is moved-out-of only partially, and patch `bb` to a jump
    /// to it. This must not be called on ADTs with a destructor,
    /// as these can't be moved-out-of, except for `Box<T>`, which is
    /// special-cased.
    ///
    /// This creates a "drop ladder" that drops the needed fields of the
    /// ADT, both in the success case or if one of the destructors fail.
    fn open_drop<'a>(&mut self) -> BasicBlock {
        let ty = self.lvalue_ty(self.lvalue);
        match ty.sty {
            ty::TyClosure(def_id, substs) => {
                let tys : Vec<_> = substs.upvar_tys(def_id, self.tcx()).collect();
                self.open_drop_for_tuple(&tys)
            }
            ty::TyTuple(tys, _) => {
                self.open_drop_for_tuple(tys)
            }
            ty::TyAdt(def, _) if def.is_box() => {
                self.open_drop_for_box(ty.boxed_ty())
            }
            ty::TyAdt(def, substs) => {
                self.open_drop_for_adt(def, substs)
            }
            ty::TyDynamic(..) => {
                let unwind = self.unwind; // FIXME(#6393)
                let succ = self.succ;
                self.complete_drop(Some(DropFlagMode::Deep), succ, unwind)
            }
            ty::TyArray(ety, _) | ty::TySlice(ety) => {
                self.open_drop_for_array(ety)
            }
            _ => bug!("open drop from non-ADT `{:?}`", ty)
        }
    }

    /// Return a basic block that drop an lvalue using the context
    /// and path in `c`. If `mode` is something, also clear `c`
    /// according to it.
    ///
    /// if FLAG(self.path)
    ///     if let Some(mode) = mode: FLAG(self.path)[mode] = false
    ///     drop(self.lv)
    fn complete_drop<'a>(&mut self,
                         drop_mode: Option<DropFlagMode>,
                         succ: BasicBlock,
                         unwind: Unwind) -> BasicBlock
    {
        debug!("complete_drop({:?},{:?})", self, drop_mode);

        let drop_block = self.drop_block(succ, unwind);
        self.drop_flag_test_and_reset_block(drop_mode, drop_block, succ, unwind)
    }

    fn drop_flag_test_and_reset_block(&mut self,
                                      drop_mode: Option<DropFlagMode>,
                                      drop_block: BasicBlock,
                                      succ: BasicBlock,
                                      unwind: Unwind) -> BasicBlock
    {
        debug!("drop_flag_test_and_reset_block({:?},{:?})", self, drop_mode);

        if let Some(mode) = drop_mode {
            let block_start = Location { block: drop_block, statement_index: 0 };
            self.elaborator.clear_drop_flag(block_start, self.path, mode);
        }

        self.drop_flag_test_block(drop_block, succ, unwind)
    }

    fn elaborated_drop_block<'a>(&mut self) -> BasicBlock {
        debug!("elaborated_drop_block({:?})", self);
        let unwind = self.unwind; // FIXME(#6393)
        let succ = self.succ;
        let blk = self.drop_block(succ, unwind);
        self.elaborate_drop(blk);
        blk
    }

    fn box_free_block<'a>(
        &mut self,
        ty: Ty<'tcx>,
        target: BasicBlock,
        unwind: Unwind,
    ) -> BasicBlock {
        let block = self.unelaborated_free_block(ty, target, unwind);
        self.drop_flag_test_block(block, target, unwind)
    }

    fn unelaborated_free_block<'a>(
        &mut self,
        ty: Ty<'tcx>,
        target: BasicBlock,
        unwind: Unwind
    ) -> BasicBlock {
        let tcx = self.tcx();
        let unit_temp = Lvalue::Local(self.new_temp(tcx.mk_nil()));
        let free_func = tcx.require_lang_item(lang_items::BoxFreeFnLangItem);
        let substs = tcx.mk_substs(iter::once(Kind::from(ty)));

        let call = TerminatorKind::Call {
            func: Operand::function_handle(tcx, free_func, substs, self.source_info.span),
            args: vec![Operand::Consume(self.lvalue.clone())],
            destination: Some((unit_temp, target)),
            cleanup: None
        }; // FIXME(#6393)
        let free_block = self.new_block(unwind, call);

        let block_start = Location { block: free_block, statement_index: 0 };
        self.elaborator.clear_drop_flag(block_start, self.path, DropFlagMode::Shallow);
        free_block
    }

    fn drop_block<'a>(&mut self, target: BasicBlock, unwind: Unwind) -> BasicBlock {
        let block = TerminatorKind::Drop {
            location: self.lvalue.clone(),
            target: target,
            unwind: unwind.into_option()
        };
        self.new_block(unwind, block)
    }

    fn drop_flag_test_block(&mut self,
                            on_set: BasicBlock,
                            on_unset: BasicBlock,
                            unwind: Unwind)
                            -> BasicBlock
    {
        let style = self.elaborator.drop_style(self.path, DropFlagMode::Shallow);
        debug!("drop_flag_test_block({:?},{:?},{:?},{:?}) - {:?}",
               self, on_set, on_unset, unwind, style);

        match style {
            DropStyle::Dead => on_unset,
            DropStyle::Static => on_set,
            DropStyle::Conditional | DropStyle::Open => {
                let flag = self.elaborator.get_drop_flag(self.path).unwrap();
                let term = TerminatorKind::if_(self.tcx(), flag, on_set, on_unset);
                self.new_block(unwind, term)
            }
        }
    }

    fn new_block<'a>(&mut self,
                     unwind: Unwind,
                     k: TerminatorKind<'tcx>)
                     -> BasicBlock
    {
        self.elaborator.patch().new_block(BasicBlockData {
            statements: vec![],
            terminator: Some(Terminator {
                source_info: self.source_info, kind: k
            }),
            is_cleanup: unwind.is_cleanup()
        })
    }

    fn new_temp(&mut self, ty: Ty<'tcx>) -> Local {
        self.elaborator.patch().new_temp(ty, self.source_info.span)
    }

    fn terminator_loc(&mut self, bb: BasicBlock) -> Location {
        let mir = self.elaborator.mir();
        self.elaborator.patch().terminator_loc(mir, bb)
    }

    fn constant_usize(&self, val: u16) -> Operand<'tcx> {
        Operand::Constant(box Constant {
            span: self.source_info.span,
            ty: self.tcx().types.usize,
            literal: Literal::Value { value: ConstVal::Integral(self.tcx().const_usize(val)) }
        })
    }
}
