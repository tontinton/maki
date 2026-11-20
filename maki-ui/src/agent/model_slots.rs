use std::collections::HashMap;
use std::sync::Arc;

use maki_config::ModelPolicy;
use maki_providers::provider::{Provider, from_model, from_model_async};
use maki_providers::{AgentError, Model, Timeouts};
use tokio::task::{Id, JoinError, JoinSet};
use tracing::warn;

use super::ModelSlot;

pub(crate) const MODEL_POLICY_ERR: &str = "Model is not allowed by policy";
const INVALID_MODEL_ERR: &str = "Invalid model";
const PROVIDER_INIT_ERR: &str = "Failed to create provider";
const BUILD_PANICKED_ERR: &str = "Building the provider failed unexpectedly";

/// Every model and provider this process has built, keyed by spec.
///
/// Building one can run a script-backed provider's `resolve`, which is a child
/// process under a file lock (see `providers/dynamic.rs`), so a spec is built
/// once and shared by every session on it. A rebuild swaps in a new `Arc`
/// instead of mutating, so whoever holds the old one still has a model and a
/// provider that match.
pub(crate) struct ModelSlots {
    built: HashMap<String, Arc<ModelSlot>>,
    /// What the process started on. Opening a session leans on it so it always
    /// ends up with a model that works.
    fallback: Arc<ModelSlot>,
    policy: Arc<ModelPolicy>,
    timeouts: Timeouts,
    /// How many invalidates there were. A build stamped with an older one ran
    /// on credentials or a catalog that are gone.
    epoch: u64,
    /// The builds running off the event loop, one per spec, whoever asked.
    building: JoinSet<Built>,
    /// The spec each one builds and the epoch it started in.
    in_flight: HashMap<Id, (String, u64)>,
}

type Built = Result<Arc<ModelSlot>, String>;

/// What [`ModelSlots::next_built`] hands to [`ModelSlots::land`].
pub(crate) type Landed = Result<(Id, Built), JoinError>;

fn model(policy: &ModelPolicy, spec: &str) -> Result<Model, String> {
    if !policy.allows(spec) {
        return Err(format!("{MODEL_POLICY_ERR}: {spec}"));
    }
    Model::from_spec(spec).map_err(|e| format!("{INVALID_MODEL_ERR}: {e}"))
}

fn into_slot(
    model: Model,
    provider: Result<Box<dyn Provider>, AgentError>,
) -> Result<Arc<ModelSlot>, String> {
    let provider = provider.map_err(|e| format!("{PROVIDER_INIT_ERR}: {e}"))?;
    Ok(Arc::new(ModelSlot {
        model,
        provider: Arc::from(provider),
    }))
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
            epoch: 0,
            building: JoinSet::new(),
            in_flight: HashMap::new(),
        }
    }

    pub(crate) fn get(&self, spec: &str) -> Option<Arc<ModelSlot>> {
        self.built.get(spec).map(Arc::clone)
    }

    /// Building reads credentials and `providers.toml` and can run a child
    /// process, so only startup, which has nothing to draw yet, builds here.
    fn try_get(&mut self, spec: &str) -> Result<Arc<ModelSlot>, String> {
        if let Some(slot) = self.get(spec) {
            return Ok(slot);
        }
        let mut model = model(&self.policy, spec)?;
        let provider = from_model(&mut model, self.timeouts);
        let slot = into_slot(model, provider)?;
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
            Err(reason) => self.fall_back(spec, reason),
        }
    }

    pub(crate) fn fall_back(&self, spec: &str, reason: String) -> (Arc<ModelSlot>, Option<String>) {
        let chosen = self.fallback.model.spec();
        warn!(requested = spec, %chosen, %reason, "session opened on a fallback model");
        (Arc::clone(&self.fallback), Some(reason))
    }

    /// Starts a build of `spec` off the loop unless it has a slot or a build
    /// already. [`Self::next_built`] says when it is done.
    pub(crate) fn request(&mut self, spec: &str) {
        if self.built.contains_key(spec)
            || self
                .in_flight
                .values()
                .any(|(building, _)| building == spec)
        {
            return;
        }
        let (policy, timeouts) = (Arc::clone(&self.policy), self.timeouts);
        let requested = spec.to_owned();
        let task = self.building.spawn(async move {
            let mut model = model(&policy, &requested)?;
            let provider = from_model_async(&mut model, timeouts).await;
            into_slot(model, provider)
        });
        self.in_flight
            .insert(task.id(), (spec.to_owned(), self.epoch));
    }

    /// The next build to finish, `None` while none is running.
    pub(crate) async fn next_built(&mut self) -> Option<Landed> {
        self.building.join_next_with_id().await
    }

    /// Takes in a finished build: the spec it was for and its slot, kept for
    /// the next caller, or why there is none. `None` for a build from before
    /// the last invalidate, which runs again.
    pub(crate) fn land(&mut self, landed: Landed) -> Option<(String, Built)> {
        let (id, built) = match landed {
            Ok((id, built)) => (id, Ok(built)),
            Err(panicked) => (panicked.id(), Err(panicked)),
        };
        let (spec, epoch) = self.in_flight.remove(&id)?;
        if epoch != self.epoch {
            self.request(&spec);
            return None;
        }
        let built = built.unwrap_or_else(|panicked| {
            warn!(%spec, error = %panicked, "slot build panicked");
            Err(format!("{BUILD_PANICKED_ERR}: {spec}"))
        });
        let built = built.map(|slot| Arc::clone(self.built.entry(spec.clone()).or_insert(slot)));
        Some((spec, built))
    }

    /// Discovery landed or a provider was re-authenticated, so every spec
    /// builds again the next time somebody asks for it.
    pub(crate) fn invalidate(&mut self) {
        self.built.clear();
        self.epoch += 1;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{INVALID_MODEL_ERR, MODEL_POLICY_ERR, ModelSlots, PROVIDER_INIT_ERR};
    use maki_config::ModelPolicy;
    use maki_config::providers::{Protocol, ProviderDef, ProvidersConfig};
    use maki_providers::provider::from_model;
    use maki_providers::test_support::isolated_home;
    use maki_providers::{Model, Timeouts};
    use std::sync::Arc;
    use test_case::test_case;

    /// A built-in provider that takes any model name and builds without
    /// credentials, so these tests never touch the catalog or the network.
    pub(crate) const STARTUP_SPEC: &str = "opencode/startup";
    pub(crate) const OTHER_SPEC: &str = "opencode/other";
    const UNPARSEABLE_SPEC: &str = "not-a-spec";
    /// A `providers.toml` entry with no key anywhere, so its spec parses and
    /// only building the provider fails.
    const KEYLESS_SLUG: &str = "keyless";
    const KEYLESS_SPEC: &str = "keyless/model";
    const UNSET_KEY_ENV: &str = "MAKI_TEST_KEYLESS_API_KEY";
    const CACHED: &str = "the same spec must not be built twice";
    const NOT_REBUILT: &str = "invalidate must force a rebuild";
    const NOT_THE_STARTUP_SLOT: &str = "the startup spec must resolve to the startup slot";
    const FELL_BACK: &str = "an allowed, buildable spec must not fall back";
    const NO_FLOOR: &str = "invalidate must keep the fallback floor";
    const POISONED: &str = "a failed lookup must not leave a cache entry";
    const STALE_KEPT: &str = "only a slot built since the last invalidate may be kept";
    const NOT_RUNNING: &str = "a requested spec with no slot has a build running";
    const OFF_LOOP_DIFFERS: &str = "a build off the loop must reject what try_get rejects";

    fn startup_slot() -> Arc<super::ModelSlot> {
        let mut model = Model::from_spec(STARTUP_SPEC).unwrap();
        let provider = from_model(&mut model, Timeouts::default()).unwrap();
        Arc::new(super::ModelSlot {
            model,
            provider: Arc::from(provider),
        })
    }

    pub(crate) fn slots(policy: ModelPolicy) -> ModelSlots {
        ModelSlots::new(startup_slot(), Arc::new(policy), Timeouts::default())
    }

    pub(crate) fn strict_policy() -> ModelPolicy {
        ModelPolicy::new(&[STARTUP_SPEC.to_owned()], &[]).unwrap()
    }

    fn declare_keyless_provider() {
        let mut config = ProvidersConfig::default();
        config.upsert(
            KEYLESS_SLUG.to_owned(),
            ProviderDef {
                protocol: Some(Protocol::Openai),
                api_key_env: Some(UNSET_KEY_ENV.to_owned()),
                ..ProviderDef::default()
            },
        );
        config.save().unwrap();
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

    async fn build(
        slots: &mut ModelSlots,
        spec: &str,
    ) -> Option<Result<Arc<super::ModelSlot>, String>> {
        slots.request(spec);
        let landed = slots.next_built().await.expect(NOT_RUNNING);
        slots.land(landed).map(|(landed_spec, built)| {
            assert_eq!(landed_spec, spec);
            built
        })
    }

    #[tokio::test]
    async fn a_spec_has_one_build_however_many_ask() {
        let mut slots = slots(ModelPolicy::default());
        slots.request(OTHER_SPEC);
        let built = build(&mut slots, OTHER_SPEC).await.unwrap().unwrap();

        assert!(
            Arc::ptr_eq(&slots.get(OTHER_SPEC).unwrap(), &built),
            "{CACHED}"
        );
        slots.request(OTHER_SPEC);
        assert!(slots.next_built().await.is_none(), "{CACHED}");
    }

    #[test_case(ModelPolicy::default(), true ; "a_slot")]
    #[test_case(strict_policy(), false ; "a_refusal")]
    #[tokio::test]
    async fn a_build_from_before_an_invalidate_runs_again(policy: ModelPolicy, builds: bool) {
        let mut slots = slots(policy);
        slots.request(OTHER_SPEC);
        slots.invalidate();

        assert!(
            build(&mut slots, OTHER_SPEC).await.is_none(),
            "{STALE_KEPT}"
        );
        assert!(slots.get(OTHER_SPEC).is_none(), "{STALE_KEPT}");
        let rebuilt = build(&mut slots, OTHER_SPEC).await.expect(STALE_KEPT);
        assert_eq!(rebuilt.is_ok(), builds);
        assert_eq!(slots.get(OTHER_SPEC).is_some(), builds, "{CACHED}");
    }

    /// A cache hit skips both the policy check and the build, so a refusal must
    /// never leave an entry behind or the next caller gets handed a bogus slot.
    /// The strict and unparseable case pins the order, keeping a spec the user
    /// may not run away from `from_model`, which can spawn a child process.
    #[test_case(strict_policy(), OTHER_SPEC, MODEL_POLICY_ERR ; "a spec off the allow list")]
    #[test_case(ModelPolicy::default(), UNPARSEABLE_SPEC, INVALID_MODEL_ERR ; "a spec that will not parse")]
    #[test_case(strict_policy(), UNPARSEABLE_SPEC, MODEL_POLICY_ERR ; "policy is checked before parsing")]
    #[test_case(ModelPolicy::default(), KEYLESS_SPEC, PROVIDER_INIT_ERR ; "a provider that will not build")]
    #[tokio::test]
    async fn a_rejected_spec_errors_every_time_and_falls_back(
        policy: ModelPolicy,
        spec: &str,
        expected: &str,
    ) {
        let _home = isolated_home();
        declare_keyless_provider();
        let mut slots = slots(policy);

        let err = slots.try_get(spec).err().unwrap();
        assert!(err.starts_with(expected), "{err}");
        assert!(slots.try_get(spec).is_err(), "{POISONED}");
        let off_loop = build(&mut slots, spec).await.unwrap().err();
        assert_eq!(off_loop.as_ref(), Some(&err), "{OFF_LOOP_DIFFERS}");
        assert!(slots.get(spec).is_none(), "{POISONED}");

        let (slot, reason) = slots.get_or_fallback(spec);
        assert_eq!(slot.model.spec(), STARTUP_SPEC, "{POISONED}");
        assert_eq!(reason.as_deref(), Some(err.as_str()));
    }
}
