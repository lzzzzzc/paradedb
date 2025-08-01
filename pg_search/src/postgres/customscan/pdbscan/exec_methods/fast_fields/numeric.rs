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

use crate::index::fast_fields_helper::WhichFastField;
use crate::index::reader::index::MultiSegmentSearchResults;
use crate::postgres::customscan::pdbscan::exec_methods::fast_fields::{
    non_string_ff_to_datum, FastFieldExecState,
};
use crate::postgres::customscan::pdbscan::exec_methods::{ExecMethod, ExecState};
use crate::postgres::customscan::pdbscan::is_block_all_visible;
use crate::postgres::customscan::pdbscan::parallel::checkout_segment;
use crate::postgres::customscan::pdbscan::scan_state::PdbScanState;
use pgrx::itemptr::item_pointer_get_block_number;
use pgrx::pg_sys;
use pgrx::pg_sys::CustomScanState;

pub struct NumericFastFieldExecState {
    inner: FastFieldExecState,
    search_results: Option<MultiSegmentSearchResults>,
}

impl NumericFastFieldExecState {
    pub fn new(which_fast_fields: Vec<WhichFastField>) -> Self {
        Self {
            inner: FastFieldExecState::new(which_fast_fields),
            search_results: None,
        }
    }
}

impl ExecMethod for NumericFastFieldExecState {
    fn init(&mut self, state: &mut PdbScanState, cstate: *mut CustomScanState) {
        self.inner.init(state, cstate);
    }

    fn query(&mut self, state: &mut PdbScanState) -> bool {
        if let Some(parallel_state) = state.parallel_state {
            if let Some(segment_id) = unsafe { checkout_segment(parallel_state) } {
                self.search_results = Some(
                    state
                        .search_reader
                        .as_ref()
                        .unwrap()
                        .search_segments([segment_id].into_iter()),
                );
                return true;
            }

            // no more segments to query
            self.search_results = None;
            false
        } else if self.inner.did_query {
            // not parallel, so we're done
            false
        } else {
            // not parallel, first time query
            self.search_results = Some(state.search_reader.as_ref().unwrap().search(state.limit));
            self.inner.did_query = true;
            true
        }
    }

    fn internal_next(&mut self, state: &mut PdbScanState) -> ExecState {
        unsafe {
            match self.search_results.as_mut().and_then(|r| r.next()) {
                None => ExecState::Eof,
                Some((scored, doc_address)) => {
                    let heaprel = self
                        .inner
                        .heaprel
                        .as_ref()
                        .expect("NumericFieldsExecState: heaprel should be initialized");
                    let slot = self.inner.slot;
                    let natts = (*(*slot).tts_tupleDescriptor).natts as usize;

                    crate::postgres::utils::u64_to_item_pointer(scored.ctid, &mut (*slot).tts_tid);
                    (*slot).tts_tableOid = heaprel.oid();

                    let blockno = item_pointer_get_block_number(&(*slot).tts_tid);
                    let is_visible = if blockno == self.inner.blockvis.0 {
                        // we know the visibility of this block because we just checked it last time
                        self.inner.blockvis.1
                    } else {
                        // new block so check its visibility
                        self.inner.blockvis.0 = blockno;
                        self.inner.blockvis.1 =
                            is_block_all_visible(heaprel, &mut self.inner.vmbuff, blockno);
                        self.inner.blockvis.1
                    };

                    if is_visible {
                        self.inner.blockvis = (blockno, true);

                        (*slot).tts_flags &= !pg_sys::TTS_FLAG_EMPTY as u16;
                        (*slot).tts_flags |= pg_sys::TTS_FLAG_SHOULDFREE as u16;
                        (*slot).tts_nvalid = natts as _;

                        let tupdesc = self.inner.tupdesc.as_ref().unwrap();
                        let datums = std::slice::from_raw_parts_mut((*slot).tts_values, natts);
                        let isnull = std::slice::from_raw_parts_mut((*slot).tts_isnull, natts);

                        for (i, att) in tupdesc.iter().enumerate() {
                            match non_string_ff_to_datum(
                                (&self.inner.which_fast_fields[i], i),
                                att.atttypid,
                                scored.bm25,
                                doc_address,
                                &mut self.inner.ffhelper,
                                slot,
                            ) {
                                None => {
                                    datums[i] = pg_sys::Datum::null();
                                    isnull[i] = true;
                                }
                                Some(datum) => {
                                    datums[i] = datum;
                                    isnull[i] = false;
                                }
                            }
                        }

                        ExecState::Virtual { slot }
                    } else {
                        ExecState::RequiresVisibilityCheck {
                            ctid: scored.ctid,
                            score: scored.bm25,
                            doc_address,
                        }
                    }
                }
            }
        }
    }

    fn reset(&mut self, _state: &mut PdbScanState) {
        self.inner.reset(_state);
        self.search_results = None;
    }
}
