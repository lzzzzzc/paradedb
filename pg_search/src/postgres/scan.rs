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

use crate::index::fast_fields_helper::{FFHelper, FastFieldType};
use crate::index::mvcc::MvccSatisfies;
use crate::index::reader::index::{MultiSegmentSearchResults, SearchIndexReader};
use crate::postgres::parallel::list_segment_ids;
use crate::postgres::rel::PgSearchRelation;
use crate::postgres::{parallel, ScanStrategy};
use crate::query::SearchQueryInput;
use pgrx::pg_sys::IndexScanDesc;
use pgrx::*;

pub struct Bm25ScanState {
    fast_fields: FFHelper,
    reader: SearchIndexReader,
    results: Option<MultiSegmentSearchResults>,
    itup: (Vec<pg_sys::Datum>, Vec<bool>),
    key_field_oid: PgOid,
}

#[pg_guard]
pub extern "C-unwind" fn ambeginscan(
    indexrel: pg_sys::Relation,
    nkeys: ::std::os::raw::c_int,
    norderbys: ::std::os::raw::c_int,
) -> pg_sys::IndexScanDesc {
    unsafe {
        let scandesc = pg_sys::RelationGetIndexScan(indexrel, nkeys, norderbys);

        // we may or may not end up doing an Index Only Scan, but regardless we only need to do
        // this one time
        (*scandesc).xs_hitupdesc = (*indexrel).rd_att;

        scandesc
    }
}

// An annotation to guard the function for PostgreSQL's threading model.
#[pg_guard]
pub extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: ::std::os::raw::c_int,
    _orderbys: pg_sys::ScanKey,
    _norderbys: ::std::os::raw::c_int,
) {
    fn key_to_search_query_input(key: &pg_sys::ScanKeyData) -> SearchQueryInput {
        match ScanStrategy::try_from(key.sk_strategy).expect("`key.sk_strategy` is unrecognized") {
            ScanStrategy::TextQuery => unsafe {
                let query_string = String::from_datum(key.sk_argument, false)
                    .expect("ScanKey.sk_argument must not be null");
                SearchQueryInput::Parse {
                    query_string,
                    lenient: None,
                    conjunction_mode: None,
                }
            },
            ScanStrategy::SearchQueryInput => unsafe {
                SearchQueryInput::from_datum(key.sk_argument, false)
                    .expect("ScanKey.sk_argument must not be null")
            },
        }
    }

    let (indexrel, keys) = unsafe {
        // SAFETY:  assert the pointers we're going to use are non-null
        assert!(!scan.is_null());
        assert!(!(*scan).indexRelation.is_null());
        assert!(!keys.is_null());
        assert!(nkeys > 0); // Ensure there's at least one key provided for the search.

        let indexrel = (*scan).indexRelation;
        let keys = std::slice::from_raw_parts(keys as *const pg_sys::ScanKeyData, nkeys as usize);

        ((PgSearchRelation::from_pg(indexrel)), keys)
    };

    // build a Boolean "must" clause of all the ScanKeys
    let mut search_query_input = key_to_search_query_input(&keys[0]);
    for key in &keys[1..] {
        let key = key_to_search_query_input(key);

        search_query_input = SearchQueryInput::Boolean {
            must: vec![search_query_input, key],
            should: vec![],
            must_not: vec![],
        };
    }

    // Create the index and scan state
    let search_reader = SearchIndexReader::open(&indexrel, search_query_input, false, unsafe {
        if pg_sys::ParallelWorkerNumber == -1 || (*scan).parallel_scan.is_null() {
            // the leader only sees snapshot-visible segments.
            // we're the leader because our WorkerNumber is -1
            // alternatively, we're not actually a parallel scan because (*scan).parallen_scan is null
            MvccSatisfies::Snapshot
        } else {
            // the workers have their own rules, which is literally every segment
            // this is because the workers pick a specific segment to query that
            // is known to be held open/pinned by the leader but might not pass a ::Snapshot
            // visibility test due to concurrent merges/garbage collects
            MvccSatisfies::ParallelWorker(
                list_segment_ids(scan).expect("IndexScan parallel state should have segments"),
            )
        }
    })
    .expect("amrescan: should be able to open a SearchIndexReader");
    unsafe {
        parallel::maybe_init_parallel_scan(scan, &search_reader);

        let results = if (*scan).parallel_scan.is_null() {
            // not a parallel scan
            Some(search_reader.search(None))
        } else {
            // a parallel scan: see if there is another segment to query
            parallel::maybe_claim_segment(scan)
                .map(|segment_number| search_reader.search_segments([segment_number].into_iter()))
        };

        let natts = (*(*scan).xs_hitupdesc).natts as usize;
        let scan_state = if (*scan).xs_want_itup {
            let schema = indexrel.schema().expect("indexrel should have a schema");
            Bm25ScanState {
                fast_fields: FFHelper::with_fields(
                    &search_reader,
                    &[(
                        schema.key_field_name(),
                        FastFieldType::from(schema.key_field_type()),
                    )
                        .into()],
                ),
                reader: search_reader,
                results,
                itup: (vec![pg_sys::Datum::null(); natts], vec![true; natts]),
                key_field_oid: PgOid::from(
                    (*(*scan).xs_hitupdesc).attrs.as_slice(natts)[0].atttypid,
                ),
            }
        } else {
            Bm25ScanState {
                fast_fields: FFHelper::empty(),
                reader: search_reader,
                results,
                itup: (vec![], vec![]),
                key_field_oid: PgOid::Invalid,
            }
        };

        (*scan).opaque = PgMemoryContexts::CurrentMemoryContext
            .leak_and_drop_on_delete(Some(scan_state))
            .cast();
    }
}

#[pg_guard]
pub extern "C-unwind" fn amendscan(scan: pg_sys::IndexScanDesc) {
    unsafe {
        let scan_state = (*(*scan).opaque.cast::<Option<Bm25ScanState>>()).take();
        drop(scan_state);
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amgettuple(
    scan: pg_sys::IndexScanDesc,
    _direction: pg_sys::ScanDirection::Type,
) -> bool {
    let state = {
        // SAFETY:  We set `scan.opaque` to a leaked pointer of type `PgSearchScanState` above in
        // amrescan, which is always called prior to this function
        (*(*scan).opaque.cast::<Option<Bm25ScanState>>())
            .as_mut()
            .expect("opaque should be a Bm25ScanState")
    };

    (*scan).xs_recheck = false;

    loop {
        match state.results.as_mut().and_then(|r| r.next()) {
            Some((scored, doc_address)) => {
                let ipd = &mut (*scan).xs_heaptid;
                crate::postgres::utils::u64_to_item_pointer(scored.ctid, ipd);

                if (*scan).xs_want_itup {
                    let key = state
                        .fast_fields
                        .value(0, doc_address)
                        .expect("key_field should be a fast_field");
                    match key
                        .try_into_datum(state.key_field_oid)
                        .expect("key_field value should convert to a Datum")
                    {
                        // got a valid Datum
                        Some(key_field_datum) => {
                            state.itup.0[0] = key_field_datum;
                            state.itup.1[0] = false;
                        }

                        // we got a NULL for the key_field.  Highly unlikely but definitely possible
                        None => {
                            state.itup.0[0] = pg_sys::Datum::null();
                            state.itup.1[0] = true;
                        }
                    }

                    let values = state.itup.0.as_mut_ptr();
                    let nulls = state.itup.1.as_mut_ptr();

                    if (*scan).xs_hitup.is_null() {
                        (*scan).xs_hitup =
                            pg_sys::heap_form_tuple((*scan).xs_hitupdesc, values, nulls);
                    } else {
                        pg_sys::ffi::pg_guard_ffi_boundary(|| {
                            extern "C-unwind" {
                                fn heap_compute_data_size(
                                    tupleDesc: pg_sys::TupleDesc,
                                    values: *mut pg_sys::Datum,
                                    isnull: *mut bool,
                                ) -> pg_sys::Size;
                                fn heap_fill_tuple(
                                    tupleDesc: pg_sys::TupleDesc,
                                    values: *mut pg_sys::Datum,
                                    isnull: *mut bool,
                                    data: *mut ::core::ffi::c_char,
                                    data_size: pg_sys::Size,
                                    infomask: *mut pg_sys::uint16,
                                    bit: *mut pg_sys::bits8,
                                );
                            }
                            let data_len =
                                heap_compute_data_size((*scan).xs_hitupdesc, values, nulls);
                            let td = (*(*scan).xs_hitup).t_data;

                            // TODO:  seems like this could crash with a varlena "key_field" of varrying sizes per row
                            heap_fill_tuple(
                                (*scan).xs_hitupdesc,
                                values,
                                nulls,
                                td.cast::<std::ffi::c_char>().add((*td).t_hoff as usize),
                                data_len,
                                &mut (*td).t_infomask,
                                (*td).t_bits.as_mut_ptr(),
                            );
                        });
                    }
                }

                return true;
            }
            None => {
                if search_next_segment(scan, state) {
                    // loop back around to start returning results from this segment
                    continue;
                }

                // we are done returning results
                return false;
            }
        }
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amgetbitmap(
    scan: pg_sys::IndexScanDesc,
    tbm: *mut pg_sys::TIDBitmap,
) -> i64 {
    assert!(!tbm.is_null());
    assert!(!scan.is_null());

    let state = {
        // SAFETY:  We set `scan.opaque` to a leaked pointer of type `PgSearchScanState` above in
        // amrescan, which is always called prior to this function
        (*(*scan).opaque.cast::<Option<Bm25ScanState>>())
            .as_mut()
            .expect("opaque should be a Bm25ScanState")
    };

    let mut cnt = 0i64;
    loop {
        if let Some(search_results) = state.results.as_mut() {
            for (scored, _) in search_results {
                let mut ipd = pg_sys::ItemPointerData::default();
                crate::postgres::utils::u64_to_item_pointer(scored.ctid, &mut ipd);

                // SAFETY:  `tbm` has been asserted to be non-null and our `&mut tid` has been
                // initialized as a stack-allocated ItemPointerData
                pg_sys::tbm_add_tuples(tbm, &mut ipd, 1, false);

                cnt += 1;
            }
        }

        // check if the bitmap scan needs to claim another individual segment
        if search_next_segment(scan, state) {
            continue;
        }

        break;
    }

    cnt
}

// if there's a segment to be claimed for parallel query execution, do that now
unsafe fn search_next_segment(scan: IndexScanDesc, state: &mut Bm25ScanState) -> bool {
    if let Some(segment_number) = parallel::maybe_claim_segment(scan) {
        state.results = Some(state.reader.search_segments([segment_number].into_iter()));
        return true;
    }
    false
}

#[pg_guard]
pub extern "C-unwind" fn amcanreturn(indexrel: pg_sys::Relation, attno: i32) -> bool {
    if attno != 1 {
        // currently, we only support returning the "key_field", which will always be the first
        // index attribute
        return false;
    }

    unsafe {
        assert!(!indexrel.is_null());
        assert!(!(*indexrel).rd_att.is_null());
        let tupdesc = PgTupleDesc::from_pg_unchecked((*indexrel).rd_att);

        let att = tupdesc
            .get((attno - 1) as usize)
            .expect("attno should exist in index tupledesc");

        // we can only return a field if it's one of the below types -- basically pass-by-value (non tokenized) data types
        [
            pg_sys::INT4OID,
            pg_sys::INT8OID,
            pg_sys::FLOAT4OID,
            pg_sys::FLOAT8OID,
            pg_sys::BOOLOID,
            // we index UUID as strings, but it's beneficial to support returning due to Parallel Index Only Scans
            pg_sys::UUIDOID,
        ]
        .contains(&att.atttypid)
    }
}
