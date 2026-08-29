use std::collections::HashMap;

use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua};

use crate::runtime::{TaskHandle, lock_cell, strip_traceback};

pub const MAX_HOOK_DEPTH: u8 = 8;

/// Tasks are numbered from 1, so this one means "outside any task". Plugin
/// load and the other startup paths live there, one at a time.
const NO_TASK: u64 = 0;

/// What a depth count is scoped to. A seam either keeps its callbacks in the
/// caller's task or hands each one a fresh task, and only the seam knows
/// which.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Reentry {
    /// One count for the whole VM, for seams that detach every callback
    /// (autocmds). A refire shares no task with the fire it came from, so
    /// nothing narrower could see it.
    Vm,
    /// One count per Lua task, for seams whose chain runs inside the caller's
    /// task (slots). Re-entering stays in one task while two chains at once
    /// are two tasks, and counting those together would read a wide `batch` as
    /// recursion and drop layers it should have run. So this bounds re-entry
    /// within one chain, never a cycle across calls: a chain fired again by a
    /// fresh tool call is a fresh task, bounded by that call's window instead.
    Task,
}

impl Reentry {
    fn task(self, lua: &Lua) -> u64 {
        match self {
            Self::Vm => NO_TASK,
            Self::Task => lua
                .app_data_ref::<TaskHandle>()
                .map_or(NO_TASK, |handle| lock_cell(&handle).id),
        }
    }
}

#[derive(Default)]
pub(crate) struct DepthStore {
    depths: HashMap<(&'static str, String, u64), u8>,
}

#[derive(Debug)]
pub(crate) struct DepthExceeded;

/// Reentrancy bound as RAII: `Drop` is the only decrement, so an error
/// path can never leave the depth stuck high.
pub(crate) struct DepthGuard {
    lua: Lua,
    key: (&'static str, String, u64),
}

impl DepthGuard {
    pub(crate) fn enter(
        lua: &Lua,
        kind: &'static str,
        name: &str,
        scope: Reentry,
    ) -> Result<Self, DepthExceeded> {
        if lua.app_data_ref::<DepthStore>().is_none() {
            lua.set_app_data(DepthStore::default());
        }
        let key = (kind, name.to_owned(), scope.task(lua));
        let mut store = lua
            .app_data_mut::<DepthStore>()
            .expect("DepthStore just ensured");
        let depth = store.depths.entry(key.clone()).or_insert(0);
        if *depth >= MAX_HOOK_DEPTH {
            return Err(DepthExceeded);
        }
        *depth += 1;
        Ok(Self {
            lua: lua.clone(),
            key,
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        if let Some(mut store) = self.lua.app_data_mut::<DepthStore>()
            && let Some(depth) = store.depths.get_mut(&self.key)
        {
            *depth -= 1;
            if *depth == 0 {
                store.depths.remove(&self.key);
            }
        }
    }
}

/// Calls a plugin callback so its failure stays its own: errors are logged
/// with plugin and seam name, then swallowed.
///
/// Runs in the caller's task, not a detached one. Every seam using this waits
/// for the answer, so the caller's cancellation and deadline belong on the
/// callback producing it, and [`Reentry::Task`] only tells nesting from
/// concurrency while nesting stays inside one task.
pub(crate) async fn call_swallowing<R: FromLuaMulti>(
    func: &Function,
    args: impl IntoLuaMulti,
    seam: &str,
    plugin: &str,
) -> Option<R> {
    match func.call_async::<R>(args).await {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(seam, plugin, error = %strip_traceback(&e), "plugin callback failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime::TaskScope;

    use super::*;

    const SEAM: &str = "seam";

    #[test_case::test_case(Reentry::Vm ; "vm")]
    #[test_case::test_case(Reentry::Task ; "task")]
    fn depth_guard_enforces_max_and_unwinds(scope: Reentry) {
        let lua = Lua::new();
        let guards: Vec<_> = (0..MAX_HOOK_DEPTH)
            .map(|_| DepthGuard::enter(&lua, "test", SEAM, scope).expect("within bound"))
            .collect();
        assert!(DepthGuard::enter(&lua, "test", SEAM, scope).is_err());
        assert!(DepthGuard::enter(&lua, "test", "other", scope).is_ok());
        drop(guards);
        assert!(DepthGuard::enter(&lua, "test", SEAM, scope).is_ok());
        let store = lua.app_data_ref::<DepthStore>().unwrap();
        assert!(store.depths.is_empty(), "zero entries removed");
    }

    /// Chains running at once are separate tasks and never add up, while
    /// nesting inside one of them still trips the bound.
    #[test]
    fn task_scoped_depth_separates_concurrency_from_nesting() {
        let lua = Lua::new();
        let concurrent: Vec<_> = (0..=MAX_HOOK_DEPTH)
            .map(|_| {
                let scope = TaskScope::detached(&lua);
                let guard = DepthGuard::enter(&lua, "test", SEAM, Reentry::Task)
                    .expect("a fresh task starts at zero");
                (scope, guard)
            })
            .collect();
        let nested: Vec<_> = (1..MAX_HOOK_DEPTH)
            .map(|_| {
                DepthGuard::enter(&lua, "test", SEAM, Reentry::Task).expect("within the bound")
            })
            .collect();
        assert!(DepthGuard::enter(&lua, "test", SEAM, Reentry::Task).is_err());
        drop((concurrent, nested));
    }

    #[test]
    fn call_swallowing_hides_errors_but_not_values() {
        let lua = Lua::new();
        let bad: Function = lua.load("error('boom')").into_function().unwrap();
        let ok: Function = lua.load("return 7").into_function().unwrap();
        smol::block_on(async {
            assert!(call_swallowing::<i64>(&bad, (), SEAM, "p").await.is_none());
            assert_eq!(call_swallowing::<i64>(&ok, (), SEAM, "p").await, Some(7));
        });
    }
}
