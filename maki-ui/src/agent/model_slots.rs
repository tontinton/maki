use std::collections::HashMap;
use std::sync::Arc;

use maki_config::ModelPolicy;
use maki_providers::provider::from_model;
use maki_providers::{Model, Timeouts};
use tracing::warn;

use super::ModelSlot;

const MODEL_POLICY_ERR: &str = "Model is not allowed by policy";
const INVALID_MODEL_ERR: &str = "Invalid model";
const PROVIDER_INIT_ERR: &str = "Failed to create provider";

/// Every model and provider this process has built, keyed by spec.
///
/// Building one can run a script-backed provider's `resolve`, which is a child
/// process under a file lock (see `providers/dynamic.rs`), so a spec is built
/// once and shared by every session on it. A rebuild swaps in a new `Arc`
/// instead of mutating, so whoever holds the old one still has a model and a
/// provider that match.
pub(crate) struct ModelSlots {
    built: HashMap<String, Arc<ModelSlot>>,
    /// What the process started on. `get_or_fallback` leans on it so opening a
    /// session always ends up with a model that works.
    fallback: Arc<ModelSlot>,
    policy: Arc<ModelPolicy>,
    timeouts: Timeouts,
}

impl ModelSlots {
    pub(crate) fn new(
        startup: Arc<ModelSlot>,
        policy: Arc<ModelPolicy>,
        timeouts: Timeouts,
    ) -> Self {
        let spec = startup.model.spec();
        Self {
            built: HashMap::from([(spec, Arc::clone(&startup))]),
            fallback: startup,
            policy,
            timeouts,
        }
    }

    /// An explicit pick, where a rejected spec has to surface rather than
    /// quietly downgrade the user to something else.
    pub(crate) fn try_get(&mut self, spec: &str) -> Result<Arc<ModelSlot>, String> {
        if let Some(slot) = self.built.get(spec) {
            return Ok(Arc::clone(slot));
        }
        if !self.policy.allows(spec) {
            return Err(format!("{MODEL_POLICY_ERR}: {spec}"));
        }
        let mut model = Model::from_spec(spec).map_err(|e| format!("{INVALID_MODEL_ERR}: {e}"))?;
        let provider = from_model(&mut model, self.timeouts)
            .map_err(|e| format!("{PROVIDER_INIT_ERR}: {e}"))?;
        let slot = Arc::new(ModelSlot {
            model,
            provider: Arc::from(provider),
        });
        // Keyed by the requested spec, not the built model's. `from_model`
        // adjusts the model, and later lookups only have the request in hand.
        self.built.insert(spec.to_owned(), Arc::clone(&slot));
        Ok(slot)
    }

    /// Opening a session always gets a slot, plus the reason it is not the one
    /// that was asked for.
    pub(crate) fn get_or_fallback(&mut self, spec: &str) -> (Arc<ModelSlot>, Option<String>) {
        match self.try_get(spec) {
            Ok(slot) => (slot, None),
            Err(reason) => {
                let chosen = self.fallback.model.spec();
                warn!(requested = spec, %chosen, %reason, "session opened on a fallback model");
                (Arc::clone(&self.fallback), Some(reason))
            }
        }
    }

    /// Discovery landed or a provider was re-authenticated. The next `try_get`
    /// builds again, and only for specs somebody is actually on.
    pub(crate) fn invalidate(&mut self) {
        self.built.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{INVALID_MODEL_ERR, MODEL_POLICY_ERR, ModelSlots};
    use maki_config::ModelPolicy;
    use maki_providers::provider::from_model;
    use maki_providers::{Model, Timeouts};
    use std::sync::Arc;
    use test_case::test_case;

    /// A built-in provider that takes any model name and builds without
    /// credentials, so these tests never touch the catalog or the network.
    const STARTUP_SPEC: &str = "opencode/startup";
    const OTHER_SPEC: &str = "opencode/other";
    const UNPARSEABLE_SPEC: &str = "not-a-spec";
    const CACHED: &str = "the same spec must not be built twice";
    const NOT_REBUILT: &str = "invalidate must force a rebuild";
    const NOT_THE_STARTUP_SLOT: &str = "the startup spec must resolve to the startup slot";
    const FELL_BACK: &str = "an allowed, buildable spec must not fall back";
    const NO_FLOOR: &str = "invalidate must keep the fallback floor";
    const POISONED: &str = "a failed lookup must not leave a cache entry";

    fn startup_slot() -> Arc<super::ModelSlot> {
        let mut model = Model::from_spec(STARTUP_SPEC).unwrap();
        let provider = from_model(&mut model, Timeouts::default()).unwrap();
        Arc::new(super::ModelSlot {
            model,
            provider: Arc::from(provider),
        })
    }

    fn slots(policy: ModelPolicy) -> ModelSlots {
        ModelSlots::new(startup_slot(), Arc::new(policy), Timeouts::default())
    }

    fn strict_policy() -> ModelPolicy {
        ModelPolicy::new(&[STARTUP_SPEC.to_owned()], &[], &[], &[]).unwrap()
    }

    /// Opening ten stored sessions on one model should build one provider, so
    /// session-open has to fill the same cache an explicit pick reads.
    #[test]
    fn an_allowed_spec_is_built_once_and_shared() {
        let mut slots = slots(ModelPolicy::default());

        let (first, reason) = slots.get_or_fallback(OTHER_SPEC);
        assert_eq!(reason, None, "{FELL_BACK}");
        assert_eq!(first.model.spec(), OTHER_SPEC);

        assert!(
            Arc::ptr_eq(&first, &slots.get_or_fallback(OTHER_SPEC).0),
            "{CACHED}"
        );
        assert!(
            Arc::ptr_eq(&first, &slots.try_get(OTHER_SPEC).unwrap()),
            "{CACHED}"
        );
    }

    /// Switching back to the model we started on should reuse the slot already
    /// running instead of building a second provider. That only holds while the
    /// key `new` stores, `model.spec()`, matches the spec callers ask with.
    #[test]
    fn startup_spec_resolves_to_the_startup_slot() {
        let startup = startup_slot();
        let mut slots = ModelSlots::new(
            Arc::clone(&startup),
            Arc::new(ModelPolicy::default()),
            Timeouts::default(),
        );
        assert!(
            Arc::ptr_eq(&slots.try_get(STARTUP_SPEC).unwrap(), &startup),
            "{NOT_THE_STARTUP_SLOT}"
        );
    }

    /// `fallback` lives outside `built` on purpose. A clear that took it down
    /// too would leave session-open with nothing to land on.
    #[test]
    fn invalidate_rebuilds_and_keeps_the_fallback_floor() {
        let mut slots = slots(strict_policy());
        let first = slots.try_get(STARTUP_SPEC).unwrap();
        slots.invalidate();

        assert!(
            !Arc::ptr_eq(&first, &slots.try_get(STARTUP_SPEC).unwrap()),
            "{NOT_REBUILT}"
        );
        let (slot, reason) = slots.get_or_fallback(OTHER_SPEC);
        assert_eq!(slot.model.spec(), STARTUP_SPEC, "{NO_FLOOR}");
        assert!(reason.is_some(), "{NO_FLOOR}");
    }

    /// A cache hit skips both the policy check and the build, so a refusal must
    /// never leave an entry behind or the next caller gets handed a bogus slot.
    /// The strict and unparseable case pins the order, keeping a spec the user
    /// may not run away from `from_model`, which can spawn a child process.
    #[test_case(strict_policy(), OTHER_SPEC, MODEL_POLICY_ERR ; "a spec off the allow list")]
    #[test_case(ModelPolicy::default(), UNPARSEABLE_SPEC, INVALID_MODEL_ERR ; "a spec that will not parse")]
    #[test_case(strict_policy(), UNPARSEABLE_SPEC, MODEL_POLICY_ERR ; "policy is checked before parsing")]
    fn a_rejected_spec_errors_every_time_and_falls_back(
        policy: ModelPolicy,
        spec: &str,
        expected: &str,
    ) {
        let mut slots = slots(policy);

        let err = slots.try_get(spec).err().unwrap();
        assert!(err.starts_with(expected), "{err}");
        assert!(slots.try_get(spec).is_err(), "{POISONED}");

        let (slot, reason) = slots.get_or_fallback(spec);
        assert_eq!(slot.model.spec(), STARTUP_SPEC, "{POISONED}");
        assert_eq!(reason.as_deref(), Some(err.as_str()));
    }
}
