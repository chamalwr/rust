// Copyright 2018 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod environment;

use rustc::hir::def_id::DefId;
use rustc::hir::intravisit::{self, NestedVisitorMap, Visitor};
use rustc::hir::map::definitions::DefPathData;
use rustc::hir::{self, ImplPolarity};
use rustc::traits::{
    Clause,
    Clauses,
    DomainGoal,
    FromEnv,
    GoalKind,
    PolyDomainGoal,
    ProgramClause,
    ProgramClauseCategory,
    WellFormed,
    WhereClause,
};
use rustc::ty::query::Providers;
use rustc::ty::{self, List, TyCtxt};
use rustc::ty::subst::{Subst, Substs};
use syntax::ast;

use std::iter;

crate fn provide(p: &mut Providers) {
    *p = Providers {
        program_clauses_for,
        program_clauses_for_env: environment::program_clauses_for_env,
        environment: environment::environment,
        ..*p
    };
}

crate trait Lower<T> {
    /// Lower a rustc construct (e.g. `ty::TraitPredicate`) to a chalk-like type.
    fn lower(&self) -> T;
}

impl<T, U> Lower<Vec<U>> for Vec<T>
where
    T: Lower<U>,
{
    fn lower(&self) -> Vec<U> {
        self.iter().map(|item| item.lower()).collect()
    }
}

impl<'tcx> Lower<WhereClause<'tcx>> for ty::TraitPredicate<'tcx> {
    fn lower(&self) -> WhereClause<'tcx> {
        WhereClause::Implemented(*self)
    }
}

impl<'tcx> Lower<WhereClause<'tcx>> for ty::ProjectionPredicate<'tcx> {
    fn lower(&self) -> WhereClause<'tcx> {
        WhereClause::ProjectionEq(*self)
    }
}

impl<'tcx> Lower<WhereClause<'tcx>> for ty::RegionOutlivesPredicate<'tcx> {
    fn lower(&self) -> WhereClause<'tcx> {
        WhereClause::RegionOutlives(*self)
    }
}

impl<'tcx> Lower<WhereClause<'tcx>> for ty::TypeOutlivesPredicate<'tcx> {
    fn lower(&self) -> WhereClause<'tcx> {
        WhereClause::TypeOutlives(*self)
    }
}

impl<'tcx, T> Lower<DomainGoal<'tcx>> for T
where
    T: Lower<WhereClause<'tcx>>,
{
    fn lower(&self) -> DomainGoal<'tcx> {
        DomainGoal::Holds(self.lower())
    }
}

/// `ty::Binder` is used for wrapping a rustc construction possibly containing generic
/// lifetimes, e.g. `for<'a> T: Fn(&'a i32)`. Instead of representing higher-ranked things
/// in that leaf-form (i.e. `Holds(Implemented(Binder<TraitPredicate>))` in the previous
/// example), we model them with quantified domain goals, e.g. as for the previous example:
/// `forall<'a> { T: Fn(&'a i32) }` which corresponds to something like
/// `Binder<Holds(Implemented(TraitPredicate))>`.
impl<'tcx, T> Lower<PolyDomainGoal<'tcx>> for ty::Binder<T>
where
    T: Lower<DomainGoal<'tcx>> + ty::fold::TypeFoldable<'tcx>,
{
    fn lower(&self) -> PolyDomainGoal<'tcx> {
        self.map_bound_ref(|p| p.lower())
    }
}

impl<'tcx> Lower<PolyDomainGoal<'tcx>> for ty::Predicate<'tcx> {
    fn lower(&self) -> PolyDomainGoal<'tcx> {
        use rustc::ty::Predicate;

        match self {
            Predicate::Trait(predicate) => predicate.lower(),
            Predicate::RegionOutlives(predicate) => predicate.lower(),
            Predicate::TypeOutlives(predicate) => predicate.lower(),
            Predicate::Projection(predicate) => predicate.lower(),

            Predicate::WellFormed(..) |
            Predicate::ObjectSafe(..) |
            Predicate::ClosureKind(..) |
            Predicate::Subtype(..) |
            Predicate::ConstEvaluatable(..) => {
                bug!("unexpected predicate {}", self)
            }
        }
    }
}

/// Used for implied bounds related rules (see rustc guide).
trait IntoFromEnvGoal {
    /// Transforms an existing goal into a `FromEnv` goal.
    fn into_from_env_goal(self) -> Self;
}

/// Used for well-formedness related rules (see rustc guide).
trait IntoWellFormedGoal {
    /// Transforms an existing goal into a `WellFormed` goal.
    fn into_well_formed_goal(self) -> Self;
}

impl<'tcx> IntoFromEnvGoal for DomainGoal<'tcx> {
    fn into_from_env_goal(self) -> DomainGoal<'tcx> {
        use self::WhereClause::*;

        match self {
            DomainGoal::Holds(Implemented(trait_ref)) => {
                DomainGoal::FromEnv(FromEnv::Trait(trait_ref))
            }
            other => other,
        }
    }
}

impl<'tcx> IntoWellFormedGoal for DomainGoal<'tcx> {
    fn into_well_formed_goal(self) -> DomainGoal<'tcx> {
        use self::WhereClause::*;

        match self {
            DomainGoal::Holds(Implemented(trait_ref)) => {
                DomainGoal::WellFormed(WellFormed::Trait(trait_ref))
            }
            other => other,
        }
    }
}

crate fn program_clauses_for<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    def_id: DefId,
) -> Clauses<'tcx> {
    match tcx.def_key(def_id).disambiguated_data.data {
        DefPathData::Trait(_) => program_clauses_for_trait(tcx, def_id),
        DefPathData::Impl => program_clauses_for_impl(tcx, def_id),
        DefPathData::AssocTypeInImpl(..) => program_clauses_for_associated_type_value(tcx, def_id),
        DefPathData::AssocTypeInTrait(..) => program_clauses_for_associated_type_def(tcx, def_id),
        DefPathData::TypeNs(..) => program_clauses_for_type_def(tcx, def_id),
        _ => List::empty(),
    }
}

fn program_clauses_for_trait<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    def_id: DefId,
) -> Clauses<'tcx> {
    // `trait Trait<P1..Pn> where WC { .. } // P0 == Self`

    // Rule Implemented-From-Env (see rustc guide)
    //
    // ```
    // forall<Self, P1..Pn> {
    //   Implemented(Self: Trait<P1..Pn>) :- FromEnv(Self: Trait<P1..Pn>)
    // }
    // ```

    let bound_vars = Substs::bound_vars_for_item(tcx, def_id);

    // `Self: Trait<P1..Pn>`
    let trait_pred = ty::TraitPredicate {
        trait_ref: ty::TraitRef {
            def_id,
            substs: bound_vars,
        },
    };

    // `Implemented(Self: Trait<P1..Pn>)`
    let impl_trait: DomainGoal = trait_pred.lower();

    // `FromEnv(Self: Trait<P1..Pn>)`
    let from_env_goal = tcx.mk_goal(impl_trait.into_from_env_goal().into_goal());
    let hypotheses = tcx.intern_goals(&[from_env_goal]);

    // `Implemented(Self: Trait<P1..Pn>) :- FromEnv(Self: Trait<P1..Pn>)`
    let implemented_from_env = ProgramClause {
        goal: impl_trait,
        hypotheses,
        category: ProgramClauseCategory::ImpliedBound,
    };

    let implemented_from_env = Clause::ForAll(ty::Binder::bind(implemented_from_env));

    let predicates = &tcx.predicates_defined_on(def_id).predicates;
    let where_clauses = &predicates
        .iter()
        .map(|(wc, _)| wc.lower())
        .map(|wc| wc.subst(tcx, bound_vars))
        .collect::<Vec<_>>();

    // Rule Implied-Bound-From-Trait
    //
    // For each where clause WC:
    // ```
    // forall<Self, P1..Pn> {
    //   FromEnv(WC) :- FromEnv(Self: Trait<P1..Pn)
    // }
    // ```

    // `FromEnv(WC) :- FromEnv(Self: Trait<P1..Pn>)`, for each where clause WC
    let implied_bound_clauses = where_clauses
        .iter()
        .cloned()

        // `FromEnv(WC) :- FromEnv(Self: Trait<P1..Pn>)`
        .map(|wc| {
            // we move binders to the left
            wc.map_bound(|goal| ProgramClause {
                goal: goal.into_from_env_goal(),

                // FIXME: As where clauses can only bind lifetimes for now,
                // and that named bound regions have a def-id, it is safe
                // to just inject `hypotheses` (which contains named vars bound at index `0`)
                // into this binding level. This may change if we ever allow where clauses
                // to bind types (e.g. for GATs things), because bound types only use a `BoundVar`
                // index (no def-id).
                hypotheses,

                category: ProgramClauseCategory::ImpliedBound,
            })
        })
        .map(Clause::ForAll);

    // Rule WellFormed-TraitRef
    //
    // Here `WC` denotes the set of all where clauses:
    // ```
    // forall<Self, P1..Pn> {
    //   WellFormed(Self: Trait<P1..Pn>) :- Implemented(Self: Trait<P1..Pn>) && WellFormed(WC)
    // }
    // ```

    // `WellFormed(WC)`
    let wf_conditions = where_clauses
        .into_iter()
        .map(|wc| wc.map_bound(|goal| goal.into_well_formed_goal()));

    // `WellFormed(Self: Trait<P1..Pn>) :- Implemented(Self: Trait<P1..Pn>) && WellFormed(WC)`
    let wf_clause = ProgramClause {
        goal: DomainGoal::WellFormed(WellFormed::Trait(trait_pred)),
        hypotheses: tcx.mk_goals(
            iter::once(tcx.mk_goal(GoalKind::DomainGoal(impl_trait))).chain(
                wf_conditions.map(|wc| tcx.mk_goal(GoalKind::from_poly_domain_goal(wc, tcx)))
            )
        ),
        category: ProgramClauseCategory::WellFormed,
    };
    let wf_clause = Clause::ForAll(ty::Binder::bind(wf_clause));

    tcx.mk_clauses(
        iter::once(implemented_from_env)
            .chain(implied_bound_clauses)
            .chain(iter::once(wf_clause))
    )
}

fn program_clauses_for_impl<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) -> Clauses<'tcx> {
    if let ImplPolarity::Negative = tcx.impl_polarity(def_id) {
        return List::empty();
    }

    // Rule Implemented-From-Impl (see rustc guide)
    //
    // `impl<P0..Pn> Trait<A1..An> for A0 where WC { .. }`
    //
    // ```
    // forall<P0..Pn> {
    //   Implemented(A0: Trait<A1..An>) :- WC
    // }
    // ```

    let bound_vars = Substs::bound_vars_for_item(tcx, def_id);

    let trait_ref = tcx.impl_trait_ref(def_id)
        .expect("not an impl")
        .subst(tcx, bound_vars);

    // `Implemented(A0: Trait<A1..An>)`
    let trait_pred = ty::TraitPredicate { trait_ref }.lower();

    // `WC`
    let predicates = &tcx.predicates_of(def_id).predicates;
    let where_clauses = predicates
        .iter()
        .map(|(wc, _)| wc.lower())
        .map(|wc| wc.subst(tcx, bound_vars));

    // `Implemented(A0: Trait<A1..An>) :- WC`
    let clause = ProgramClause {
        goal: trait_pred,
        hypotheses: tcx.mk_goals(
            where_clauses
                .map(|wc| tcx.mk_goal(GoalKind::from_poly_domain_goal(wc, tcx))),
        ),
        category: ProgramClauseCategory::Other,
    };
    tcx.mk_clauses(iter::once(Clause::ForAll(ty::Binder::bind(clause))))
}

pub fn program_clauses_for_type_def<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    def_id: DefId,
) -> Clauses<'tcx> {
    // Rule WellFormed-Type
    //
    // `struct Ty<P1..Pn> where WC1, ..., WCm`
    //
    // ```
    // forall<P1..Pn> {
    //   WellFormed(Ty<...>) :- WC1, ..., WCm`
    // }
    // ```

    let bound_vars = Substs::bound_vars_for_item(tcx, def_id);

    // `Ty<...>`
    let ty = tcx.type_of(def_id).subst(tcx, bound_vars);

    // `WC`
    let where_clauses = tcx.predicates_of(def_id).predicates
        .iter()
        .map(|(wc, _)| wc.lower())
        .map(|wc| wc.subst(tcx, bound_vars))
        .collect::<Vec<_>>();

    // `WellFormed(Ty<...>) :- WC1, ..., WCm`
    let well_formed_clause = ProgramClause {
        goal: DomainGoal::WellFormed(WellFormed::Ty(ty)),
        hypotheses: tcx.mk_goals(
            where_clauses
                .iter()
                .cloned()
                .map(|wc| tcx.mk_goal(GoalKind::from_poly_domain_goal(wc, tcx))),
        ),
        category: ProgramClauseCategory::WellFormed,
    };
    let well_formed_clause = Clause::ForAll(ty::Binder::bind(well_formed_clause));

    // Rule Implied-Bound-From-Type
    //
    // For each where clause `WC`:
    // ```
    // forall<P1..Pn> {
    //   FromEnv(WC) :- FromEnv(Ty<...>)
    // }
    // ```

    // `FromEnv(Ty<...>)`
    let from_env_goal = tcx.mk_goal(DomainGoal::FromEnv(FromEnv::Ty(ty)).into_goal());
    let hypotheses = tcx.intern_goals(&[from_env_goal]);

    // For each where clause `WC`:
    let from_env_clauses = where_clauses
        .into_iter()

        // `FromEnv(WC) :- FromEnv(Ty<...>)`
        .map(|wc| {
            // move the binders to the left
            wc.map_bound(|goal| ProgramClause {
                goal: goal.into_from_env_goal(),

                // FIXME: we inject `hypotheses` into this binding level,
                // which may be incorrect in the future: see the FIXME in
                // `program_clauses_for_trait`
                hypotheses,

                category: ProgramClauseCategory::ImpliedBound,
            })
        })

        .map(Clause::ForAll);

    tcx.mk_clauses(iter::once(well_formed_clause).chain(from_env_clauses))
}

pub fn program_clauses_for_associated_type_def<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    item_id: DefId,
) -> Clauses<'tcx> {
    // Rule ProjectionEq-Placeholder
    //
    // ```
    // trait Trait<P1..Pn> {
    //     type AssocType<Pn+1..Pm>;
    // }
    // ```
    //
    // `ProjectionEq` can succeed by skolemizing, see "associated type"
    // chapter for more:
    // ```
    // forall<Self, P1..Pn, Pn+1..Pm> {
    //     ProjectionEq(
    //         <Self as Trait<P1..Pn>>::AssocType<Pn+1..Pm> =
    //         (Trait::AssocType)<Self, P1..Pn, Pn+1..Pm>
    //     )
    // }
    // ```

    let item = tcx.associated_item(item_id);
    debug_assert_eq!(item.kind, ty::AssociatedKind::Type);
    let trait_id = match item.container {
        ty::AssociatedItemContainer::TraitContainer(trait_id) => trait_id,
        _ => bug!("not an trait container"),
    };

    let trait_bound_vars = Substs::bound_vars_for_item(tcx, trait_id);
    let trait_ref = ty::TraitRef {
        def_id: trait_id,
        substs: trait_bound_vars,
    };

    let projection_ty = ty::ProjectionTy::from_ref_and_name(tcx, trait_ref, item.ident);
    let placeholder_ty = tcx.mk_ty(ty::UnnormalizedProjection(projection_ty));
    let projection_eq = WhereClause::ProjectionEq(ty::ProjectionPredicate {
        projection_ty,
        ty: placeholder_ty,
    });

    let projection_eq_clause = ProgramClause {
        goal: DomainGoal::Holds(projection_eq),
        hypotheses: ty::List::empty(),
        category: ProgramClauseCategory::Other,
    };
    let projection_eq_clause = Clause::ForAll(ty::Binder::bind(projection_eq_clause));

    // Rule WellFormed-AssocTy
    // ```
    // forall<Self, P1..Pn, Pn+1..Pm> {
    //     WellFormed((Trait::AssocType)<Self, P1..Pn, Pn+1..Pm>)
    //         :- Implemented(Self: Trait<P1..Pn>)
    // }
    // ```

    let trait_predicate = ty::TraitPredicate { trait_ref };
    let hypothesis = tcx.mk_goal(
        DomainGoal::Holds(WhereClause::Implemented(trait_predicate)).into_goal()
    );

    let wf_clause = ProgramClause {
        goal: DomainGoal::WellFormed(WellFormed::Ty(placeholder_ty)),
        hypotheses: tcx.mk_goals(iter::once(hypothesis)),
        category: ProgramClauseCategory::WellFormed,
    };
    let wf_clause = Clause::ForAll(ty::Binder::bind(wf_clause));

    // Rule Implied-Trait-From-AssocTy
    // ```
    // forall<Self, P1..Pn, Pn+1..Pm> {
    //     FromEnv(Self: Trait<P1..Pn>)
    //         :- FromEnv((Trait::AssocType)<Self, P1..Pn, Pn+1..Pm>)
    // }
    // ```

    let hypothesis = tcx.mk_goal(
        DomainGoal::FromEnv(FromEnv::Ty(placeholder_ty)).into_goal()
    );

    let from_env_clause = ProgramClause {
        goal: DomainGoal::FromEnv(FromEnv::Trait(trait_predicate)),
        hypotheses: tcx.mk_goals(iter::once(hypothesis)),
        category: ProgramClauseCategory::ImpliedBound,
    };
    let from_env_clause = Clause::ForAll(ty::Binder::bind(from_env_clause));

    // Rule ProjectionEq-Normalize
    //
    // ProjectionEq can succeed by normalizing:
    // ```
    // forall<Self, P1..Pn, Pn+1..Pm, U> {
    //   ProjectionEq(<Self as Trait<P1..Pn>>::AssocType<Pn+1..Pm> = U) :-
    //       Normalize(<Self as Trait<P1..Pn>>::AssocType<Pn+1..Pm> -> U)
    // }
    // ```

    let offset = tcx.generics_of(trait_id).params
        .iter()
        .map(|p| p.index)
        .max()
        .unwrap_or(0);
    // Add a new type param after the existing ones (`U` in the comment above).
    let ty_var = ty::Bound(
        ty::BoundTy::new(ty::INNERMOST, ty::BoundVar::from_u32(offset + 1))
    );

    // `ProjectionEq(<Self as Trait<P1..Pn>>::AssocType<Pn+1..Pm> = U)`
    let projection = ty::ProjectionPredicate {
        projection_ty,
        ty: tcx.mk_ty(ty_var),
    };

    // `Normalize(<A0 as Trait<A1..An>>::AssocType<Pn+1..Pm> -> U)`
    let hypothesis = tcx.mk_goal(
        DomainGoal::Normalize(projection).into_goal()
    );

    //  ProjectionEq(<Self as Trait<P1..Pn>>::AssocType<Pn+1..Pm> = U) :-
    //      Normalize(<Self as Trait<P1..Pn>>::AssocType<Pn+1..Pm> -> U)
    let normalize_clause = ProgramClause {
        goal: DomainGoal::Holds(WhereClause::ProjectionEq(projection)),
        hypotheses: tcx.mk_goals(iter::once(hypothesis)),
        category: ProgramClauseCategory::Other,
    };
    let normalize_clause = Clause::ForAll(ty::Binder::bind(normalize_clause));

    let clauses = iter::once(projection_eq_clause)
        .chain(iter::once(wf_clause))
        .chain(iter::once(from_env_clause))
        .chain(iter::once(normalize_clause));

    tcx.mk_clauses(clauses)
}

pub fn program_clauses_for_associated_type_value<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    item_id: DefId,
) -> Clauses<'tcx> {
    // Rule Normalize-From-Impl (see rustc guide)
    //
    // ```
    // impl<P0..Pn> Trait<A1..An> for A0 {
    //     type AssocType<Pn+1..Pm> = T;
    // }
    // ```
    //
    // FIXME: For the moment, we don't account for where clauses written on the associated
    // ty definition (i.e. in the trait def, as in `type AssocType<T> where T: Sized`).
    // ```
    // forall<P0..Pm> {
    //   forall<Pn+1..Pm> {
    //     Normalize(<A0 as Trait<A1..An>>::AssocType<Pn+1..Pm> -> T) :-
    //       Implemented(A0: Trait<A1..An>)
    //   }
    // }
    // ```

    let item = tcx.associated_item(item_id);
    debug_assert_eq!(item.kind, ty::AssociatedKind::Type);
    let impl_id = match item.container {
        ty::AssociatedItemContainer::ImplContainer(impl_id) => impl_id,
        _ => bug!("not an impl container"),
    };

    let impl_bound_vars = Substs::bound_vars_for_item(tcx, impl_id);

    // `A0 as Trait<A1..An>`
    let trait_ref = tcx.impl_trait_ref(impl_id)
        .unwrap()
        .subst(tcx, impl_bound_vars);

    // `T`
    let ty = tcx.type_of(item_id);

    // `Implemented(A0: Trait<A1..An>)`
    let trait_implemented: DomainGoal = ty::TraitPredicate { trait_ref }.lower();

    // `<A0 as Trait<A1..An>>::AssocType<Pn+1..Pm>`
    let projection_ty = ty::ProjectionTy::from_ref_and_name(tcx, trait_ref, item.ident);

    // `Normalize(<A0 as Trait<A1..An>>::AssocType<Pn+1..Pm> -> T)`
    let normalize_goal = DomainGoal::Normalize(ty::ProjectionPredicate { projection_ty, ty });

    // `Normalize(... -> T) :- ...`
    let normalize_clause = ProgramClause {
        goal: normalize_goal,
        hypotheses: tcx.mk_goals(
            iter::once(tcx.mk_goal(GoalKind::DomainGoal(trait_implemented)))
        ),
        category: ProgramClauseCategory::Other,
    };
    let normalize_clause = Clause::ForAll(ty::Binder::bind(normalize_clause));

    tcx.mk_clauses(iter::once(normalize_clause))
}

pub fn dump_program_clauses<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>) {
    if !tcx.features().rustc_attrs {
        return;
    }

    let mut visitor = ClauseDumper { tcx };
    tcx.hir
        .krate()
        .visit_all_item_likes(&mut visitor.as_deep_visitor());
}

struct ClauseDumper<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
}

impl<'a, 'tcx> ClauseDumper<'a, 'tcx> {
    fn process_attrs(&mut self, node_id: ast::NodeId, attrs: &[ast::Attribute]) {
        let def_id = self.tcx.hir.local_def_id(node_id);
        for attr in attrs {
            let mut clauses = None;

            if attr.check_name("rustc_dump_program_clauses") {
                clauses = Some(self.tcx.program_clauses_for(def_id));
            }

            if attr.check_name("rustc_dump_env_program_clauses") {
                let environment = self.tcx.environment(def_id);
                clauses = Some(self.tcx.program_clauses_for_env(*environment.skip_binder()));
            }

            if let Some(clauses) = clauses {
                let mut err = self
                    .tcx
                    .sess
                    .struct_span_err(attr.span, "program clause dump");

                let mut strings: Vec<_> = clauses
                    .iter()
                    .map(|clause| clause.to_string())
                    .collect();

                strings.sort();

                for string in strings {
                    err.note(&string);
                }

                err.emit();
            }
        }
    }
}

impl<'a, 'tcx> Visitor<'tcx> for ClauseDumper<'a, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'tcx> {
        NestedVisitorMap::OnlyBodies(&self.tcx.hir)
    }

    fn visit_item(&mut self, item: &'tcx hir::Item) {
        self.process_attrs(item.id, &item.attrs);
        intravisit::walk_item(self, item);
    }

    fn visit_trait_item(&mut self, trait_item: &'tcx hir::TraitItem) {
        self.process_attrs(trait_item.id, &trait_item.attrs);
        intravisit::walk_trait_item(self, trait_item);
    }

    fn visit_impl_item(&mut self, impl_item: &'tcx hir::ImplItem) {
        self.process_attrs(impl_item.id, &impl_item.attrs);
        intravisit::walk_impl_item(self, impl_item);
    }

    fn visit_struct_field(&mut self, s: &'tcx hir::StructField) {
        self.process_attrs(s.id, &s.attrs);
        intravisit::walk_struct_field(self, s);
    }
}
