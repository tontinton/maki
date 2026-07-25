use maki_agent::{CancelMap, CancelTrigger};

pub(super) type RunCancelMap = CancelMap<u64>;

pub(super) fn new_run_cancel_map(run_id: u64, trigger: CancelTrigger) -> RunCancelMap {
    let map = RunCancelMap::new();
    // A run holds one trigger and clears the whole key when it ends, so
    // there is no sibling to tell apart and no slot to keep.
    let _ = map.insert(run_id, trigger);
    map
}
