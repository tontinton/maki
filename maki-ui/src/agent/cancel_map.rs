use maki_agent::{CancelMap, CancelTrigger};

pub(super) type RunCancelMap = CancelMap<u64>;

pub(super) fn new_run_cancel_map(run_id: u64, trigger: CancelTrigger) -> RunCancelMap {
    let map = RunCancelMap::new();
    map.insert(run_id, trigger);
    map
}
