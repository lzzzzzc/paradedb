// Copyright (c) 2023-2025 ParadeDB, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

pub mod score;
pub mod snippet;

use crate::api::operator::ReturnedNodePointer;
use crate::api::FieldName;
use crate::api::HashMap;
use crate::api::Varno;
use crate::nodecast;
use crate::postgres::customscan::pdbscan::projections::snippet::{
    snippet_funcoid, snippet_positions_funcoid, SnippetType,
};
use crate::postgres::customscan::score_funcoid;
use crate::postgres::var::{find_one_var, find_one_var_and_fieldname, find_vars};
use pgrx::pg_sys::expression_tree_walker;
use pgrx::{pg_extern, pg_guard, pg_sys, Internal, PgList};
use std::ptr::{addr_of_mut, NonNull};
use tantivy::snippet::SnippetGenerator;

#[pg_extern(immutable, parallel_safe)]
pub unsafe fn placeholder_support(arg: Internal) -> ReturnedNodePointer {
    // we will "simply" calls to `paradedb.score(<anyelement>)` by wrapping (a copy of) its `FuncExpr`
    // node in a `PlaceHolderVar`.  This ensures that Postgres won't lose the scores when they're
    // emitted by our custom scan from underneath JOIN nodes (Hash Join, Merge Join, etc).
    if let Some(srs) = nodecast!(
        SupportRequestSimplify,
        T_SupportRequestSimplify,
        arg.unwrap().unwrap().cast_mut_ptr::<pg_sys::Node>()
    ) {
        if (*srs).root.is_null() {
            return ReturnedNodePointer(None);
        }

        if !(*(*srs).root).hasJoinRTEs {
            // however, if the query does not do joins, then using a `PlaceHolderVar` will lead
            // to a crash -- it wouldn't provide any additional value anyways
            return ReturnedNodePointer(None);
        }

        let mut vars = find_vars((*srs).fcall.cast());
        assert!(vars.len() == 1, "function is improperly defined or called");
        let var = vars.pop().unwrap();

        let phrels = pg_sys::bms_make_singleton((*var).varno as _);
        let phv = pg_sys::submodules::ffi::pg_guard_ffi_boundary(|| {
            #[allow(improper_ctypes)]
            #[rustfmt::skip]
            extern "C-unwind" {
                fn make_placeholder_expr(root: *mut pg_sys::PlannerInfo, expr: *mut pg_sys::Expr, phrels: pg_sys::Relids) -> *mut pg_sys::PlaceHolderVar;
            }

            make_placeholder_expr(
                (*srs).root,
                pg_sys::copyObjectImpl((*srs).fcall.cast()).cast(),
                phrels,
            )
        });

        // copy these properties up from the Var to its placeholder
        (*phv).phlevelsup = (*var).varlevelsup;
        #[cfg(not(any(feature = "pg14", feature = "pg15")))]
        {
            (*phv).phnullingrels = (*var).varnullingrels;
        }

        return ReturnedNodePointer(NonNull::new(phv.cast()));
    }

    ReturnedNodePointer(None)
}

pub unsafe fn maybe_needs_const_projections(node: *mut pg_sys::Node) -> bool {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(
        node: *mut pg_sys::Node,
        data: *mut core::ffi::c_void,
    ) -> bool {
        if node.is_null() {
            return false;
        }

        if let Some(funcexpr) = nodecast!(FuncExpr, T_FuncExpr, node) {
            let data = &*data.cast::<Data>();
            if (*funcexpr).funcid == data.score_funcoid
                || (*funcexpr).funcid == data.snipped_funcoid
                || (*funcexpr).funcid == data.snippet_positions_funcoid
            {
                return true;
            }
        }

        expression_tree_walker(node, Some(walker), data)
    }

    struct Data {
        score_funcoid: pg_sys::Oid,
        snipped_funcoid: pg_sys::Oid,
        snippet_positions_funcoid: pg_sys::Oid,
    }

    let mut data = Data {
        score_funcoid: score_funcoid(),
        snipped_funcoid: snippet_funcoid(),
        snippet_positions_funcoid: snippet_positions_funcoid(),
    };

    let data = addr_of_mut!(data).cast();
    walker(node, data)
}

/// find all [`pg_sys::FuncExpr`] nodes matching a set of known function Oids that also contain
/// a [`pg_sys::Var`] as an argument that the specified `rti` level.
///
/// Returns a [`Vec`] of the matching `FuncExpr`s and the argument `Var` that finally matched.  If
/// the function has multiple arguments that match, it's returned multiple times.
pub unsafe fn pullout_funcexprs(
    node: *mut pg_sys::Node,
    funcids: &[pg_sys::Oid],
    rti: i32,
    root: *mut pg_sys::PlannerInfo,
) -> Vec<(*mut pg_sys::FuncExpr, *mut pg_sys::Var, FieldName)> {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(
        node: *mut pg_sys::Node,
        data: *mut core::ffi::c_void,
    ) -> bool {
        if node.is_null() {
            return false;
        }

        if let Some(funcexpr) = nodecast!(FuncExpr, T_FuncExpr, node) {
            let data = &mut *data.cast::<Data>();
            if data.funcids.contains(&(*funcexpr).funcid) {
                let args = PgList::<pg_sys::Node>::from_pg((*funcexpr).args);
                for arg in args.iter_ptr() {
                    if let Some((var, fieldname)) = find_one_var_and_fieldname(data.root, arg) {
                        if (*var).varno as i32 == data.rti as i32 {
                            data.matches.push((funcexpr, var, fieldname));
                        }
                    }
                }

                return false;
            }
        }

        expression_tree_walker(node, Some(walker), data)
    }

    struct Data<'a> {
        funcids: &'a [pg_sys::Oid],
        rti: i32,
        root: *mut pg_sys::PlannerInfo,
        matches: Vec<(*mut pg_sys::FuncExpr, *mut pg_sys::Var, FieldName)>,
    }

    let mut data = Data {
        funcids,
        rti,
        root,
        matches: vec![],
    };

    walker(node, addr_of_mut!(data).cast());
    data.matches
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
pub unsafe fn inject_placeholders(
    targetlist: *mut pg_sys::List,
    rti: pg_sys::Index,
    score_funcoid: pg_sys::Oid,
    snippet_funcoid: pg_sys::Oid,
    snippet_positions_funcoid: pg_sys::Oid,
    attname_lookup: &HashMap<(Varno, pg_sys::AttrNumber), FieldName>,
    snippet_generators: &HashMap<SnippetType, Option<(tantivy::schema::Field, SnippetGenerator)>>,
) -> (
    *mut pg_sys::List,
    *mut pg_sys::Const,
    HashMap<SnippetType, Vec<*mut pg_sys::Const>>,
) {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(
        node: *mut pg_sys::Node,
        context: *mut std::ffi::c_void,
    ) -> *mut pg_sys::Node {
        if node.is_null() {
            return std::ptr::null_mut();
        }

        #[inline(always)]
        unsafe fn inner(node: *mut pg_sys::Node, data: &mut Data) -> Option<*mut pg_sys::Node> {
            let funcexpr = nodecast!(FuncExpr, T_FuncExpr, node)?;
            let args = PgList::<pg_sys::Node>::from_pg((*funcexpr).args);

            if (*funcexpr).funcid == data.score_funcoid {
                return Some(data.const_score_node.cast());
            }

            if (*funcexpr).funcid == data.snippet_funcoid
                || (*funcexpr).funcid == data.snippet_positions_funcoid
            {
                let var = find_one_var(args.get_ptr(0)?)?;
                let key = (data.rti as Varno, (*var).varattno);

                if let Some(attname) = data.attname_lookup.get(&key) {
                    for snippet_type in data.snippet_generators.keys() {
                        if snippet_type.field() == attname
                            && snippet_type.funcoid() == (*funcexpr).funcid
                        {
                            let const_ = pg_sys::makeConst(
                                snippet_type.nodeoid(),
                                -1,
                                pg_sys::DEFAULT_COLLATION_OID,
                                -1,
                                pg_sys::Datum::null(),
                                true,
                                false,
                            );

                            data.const_snippet_nodes
                                .entry(snippet_type.clone())
                                .or_default()
                                .push(const_);

                            return Some(const_.cast());
                        }
                    }
                }
            }

            None
        }

        let data = &mut *context.cast::<Data>();
        if let Some(replacement) = inner(node, data) {
            return replacement;
        }

        #[cfg(not(any(feature = "pg16", feature = "pg17")))]
        {
            let fnptr = walker as usize as *const ();
            let walker: unsafe extern "C-unwind" fn() -> *mut pg_sys::Node =
                std::mem::transmute(fnptr);
            pg_sys::expression_tree_mutator(node, Some(walker), context)
        }

        #[cfg(any(feature = "pg16", feature = "pg17"))]
        {
            pg_sys::expression_tree_mutator_impl(node, Some(walker), context)
        }
    }

    struct Data<'a> {
        rti: pg_sys::Index,

        score_funcoid: pg_sys::Oid,
        const_score_node: *mut pg_sys::Const,

        snippet_funcoid: pg_sys::Oid,
        snippet_positions_funcoid: pg_sys::Oid,
        attname_lookup: &'a HashMap<(Varno, pg_sys::AttrNumber), FieldName>,

        snippet_generators:
            &'a HashMap<SnippetType, Option<(tantivy::schema::Field, SnippetGenerator)>>,
        const_snippet_nodes: HashMap<SnippetType, Vec<*mut pg_sys::Const>>,
    }

    let mut data = Data {
        rti,

        score_funcoid,
        const_score_node: pg_sys::makeConst(
            pg_sys::FLOAT4OID,
            -1,
            pg_sys::Oid::INVALID,
            size_of::<f32>() as _,
            pg_sys::Datum::null(),
            true,
            true,
        ),

        snippet_funcoid,
        snippet_positions_funcoid,
        attname_lookup,
        snippet_generators,
        const_snippet_nodes: Default::default(),
    };
    let targetlist = walker(targetlist.cast(), addr_of_mut!(data).cast());
    (
        targetlist.cast(),
        data.const_score_node,
        data.const_snippet_nodes,
    )
}
