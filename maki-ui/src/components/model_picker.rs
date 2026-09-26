use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use maki_config::ModelPolicy;
use maki_providers::ModelTier;
use maki_providers::dynamic;
use maki_providers::model_registry;
use maki_providers::spec::ProviderRegistry;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::{Cadence, Dirty, Watch};
use crate::theme;

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";
const FREE_LABEL: &str = "Free";
const FREE_PREFIX: &str = "Free · ";
const OFF_LABEL: &str = "off";
const OFF_PREFIX: &str = "off · ";

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" select", t.tool_dim),
        Span::styled("  !", t.keybind_key),
        Span::styled(" strong", t.tool_dim),
        Span::styled("  @", t.keybind_key),
        Span::styled(" medium", t.tool_dim),
        Span::styled("  #", t.keybind_key),
        Span::styled(" weak", t.tool_dim),
        Span::styled("  $", t.keybind_key),
        Span::styled(" compaction", t.tool_dim),
        Span::styled("  %", t.keybind_key),
        Span::styled(" on/off", t.tool_dim),
    ])
}

fn tier_for_shortcut(key: KeyEvent) -> Option<ModelTier> {
    // Shift+digit arrives as the character it types, with the SHIFT bit
    // already folded into it by key normalization.
    let digit = match key.code {
        KeyCode::Char('!' | '¡') => '1',       // US, ES
        KeyCode::Char('@' | '"' | '™') => '2', // US, UK/DE
        KeyCode::Char('#' | '§' | '£') => '3', // US, DE, UK
        KeyCode::Char('$' | '€' | '¤') => '4', // US, EU, Nordic
        _ => return None,
    };
    match digit {
        '1' => Some(ModelTier::Strong),
        '2' => Some(ModelTier::Medium),
        '3' => Some(ModelTier::Weak),
        '4' => Some(ModelTier::Compaction),
        _ => None,
    }
}

/// Shift+5, reported by the same two routes as the tier shortcuts above.
fn is_toggle_shortcut(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers.contains(KeyModifiers::SHIFT)),
        (KeyCode::Char('5'), true) | (KeyCode::Char('%'), false)
    )
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    /// Never leaves the session: `disabled_models` in the config is what
    /// survives a restart.
    Toggle(String, bool),
    ToggleProvider(String, bool),
    Close,
}

struct ModelEntry {
    /// The qualified spec, or the bare slug on a provider row.
    spec: String,
    id: String,
    provider: String,
    provider_display: String,
    suffix: Option<String>,
    tier: String,
    override_tiers: Vec<ModelTier>,
    free: bool,
    enabled: bool,
    is_provider: bool,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.id
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.tier)
    }

    fn section(&self) -> Option<&str> {
        Some(self.provider_display.as_str())
    }

    fn is_highlighted(&self) -> bool {
        !self.override_tiers.is_empty()
    }

    fn is_section_row(&self) -> bool {
        self.is_provider
    }

    fn is_dimmed(&self) -> bool {
        !self.enabled
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    available: Watch<Vec<String>>,
    recents: Vec<String>,
    current_spec: String,
    needs_rebuild: bool,
    /// User-moved entry to restore on refresh: `(was_recent, spec)`.
    anchor: Option<(bool, String)>,
    policy: Arc<ModelPolicy>,
    /// Both hold what this session switched away from what the config said,
    /// and neither is persisted: the config file is the durable answer.
    provider_overrides: HashMap<String, bool>,
    overrides: HashMap<String, bool>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>, policy: Arc<ModelPolicy>) -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(footer_line),
            models,
            available: Watch::default(),
            recents: Vec::new(),
            current_spec: String::new(),
            needs_rebuild: false,
            anchor: None,
            policy,
            provider_overrides: HashMap::new(),
            overrides: HashMap::new(),
        }
    }

    /// A provider switched off takes its models with it whatever each one's
    /// own switch says, and leaves those switches alone underneath, so
    /// switching it back on restores what each model was.
    pub fn is_enabled(&self, spec: &str) -> bool {
        let provider_on = match spec.split_once('/') {
            Some((slug, _)) => self.provider_is_enabled(slug),
            None => true,
        };
        provider_on
            && self
                .overrides
                .get(spec)
                .copied()
                .unwrap_or_else(|| !self.policy.disabled_by_default(spec))
    }

    pub fn set_enabled(&mut self, spec: &str, enabled: bool) {
        self.overrides.insert(spec.to_owned(), enabled);
        self.needs_rebuild = true;
    }

    pub fn provider_is_enabled(&self, slug: &str) -> bool {
        self.provider_overrides
            .get(slug)
            .copied()
            .unwrap_or_else(|| !self.policy.provider_disabled_by_default(slug))
    }

    pub fn set_provider_enabled(&mut self, slug: &str, enabled: bool) {
        self.provider_overrides.insert(slug.to_owned(), enabled);
        self.needs_rebuild = true;
    }

    /// Resolved against what discovery found rather than reported as globs,
    /// and read from the shared slot rather than [`Self::available`], because
    /// Lua can ask before the picker has ever been opened to poll it.
    pub fn disabled_provider_slugs(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self
            .models
            .load_full()
            .map(|specs| {
                specs
                    .iter()
                    .filter_map(|spec| spec.split_once('/').map(|(slug, _)| slug))
                    .filter(|slug| !self.provider_is_enabled(slug))
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        slugs.sort();
        slugs.dedup();
        slugs
    }

    /// Config defaults included, and read from the shared slot for the same
    /// reason [`Self::disabled_provider_slugs`] is.
    pub fn disabled_specs(&self) -> Vec<String> {
        let mut specs: Vec<String> = self
            .models
            .load_full()
            .map(|known| {
                known
                    .iter()
                    .filter(|spec| !self.is_enabled(spec))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        specs.sort();
        specs
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.needs_rebuild = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        self.current_spec = current_spec.to_owned();
        self.anchor = None;
        self.needs_rebuild = false;
        let _ = self.available.poll(self.models.load_full());
        let entries = self.load_entries();
        self.picker.open(entries, TITLE);
        self.preselect_current_model();
    }

    /// Providers fetch their model lists in the background and drop them into
    /// a shared slot, which wakes nothing. An open picker has to notice on its
    /// own, so `App::tick` polls this instead of [`Self::view`] reading the
    /// slot mid render.
    pub fn refresh(&mut self) -> Dirty {
        if !self.picker.is_open() {
            return Dirty::NO;
        }
        let arrived = self.available.poll(self.models.load_full());
        if arrived == Dirty::NO && !self.needs_rebuild {
            return Dirty::NO;
        }
        self.needs_rebuild = false;
        let entries = self.load_entries();
        self.picker.replace_items(entries);
        if let Some((was_recent, spec)) = &self.anchor {
            self.picker
                .select_item_by(|e| e.spec == *spec && e.suffix().is_some() == *was_recent);
        } else {
            self.preselect_current_model();
        }
        Dirty::YES
    }

    fn load_entries(&self) -> Vec<ModelEntry> {
        let specs = self.available.get();
        let mut entries = Vec::new();
        for spec in &self.recents {
            if let Some(mut e) = self.parse_entry(spec) {
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = specs
            .map(|s| s.iter().filter_map(|s| self.parse_entry(s)).collect())
            .unwrap_or_default();
        full.sort_by(|a, b| {
            a.provider_display
                .cmp(&b.provider_display)
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| b.free.cmp(&a.free))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in &full {
            *counts.entry(entry.provider.clone()).or_default() += 1;
        }
        let mut last: Option<String> = None;
        for entry in full {
            if last.as_deref() != Some(entry.provider.as_str()) {
                let count = counts.get(&entry.provider).copied().unwrap_or_default();
                entries.push(self.provider_entry(&entry, count));
                last = Some(entry.provider.clone());
            }
            if self.provider_is_enabled(&entry.provider) {
                entries.push(entry);
            }
        }
        entries
    }

    /// The row a provider gets in place of a drawn-on section header, so that
    /// the switch has something to land on and a collapsed provider still says
    /// how much is behind it.
    fn provider_entry(&self, model: &ModelEntry, count: usize) -> ModelEntry {
        let enabled = self.provider_is_enabled(&model.provider);
        let plural = if count == 1 { "model" } else { "models" };
        let tier = if enabled {
            format!("{count} {plural}")
        } else {
            format!("{OFF_PREFIX}{count} {plural}")
        };
        ModelEntry {
            spec: model.provider.clone(),
            id: model.provider_display.clone(),
            provider: model.provider.clone(),
            provider_display: model.provider_display.clone(),
            suffix: None,
            tier,
            override_tiers: Vec::new(),
            free: false,
            enabled,
            is_provider: true,
        }
    }

    fn parse_entry(&self, spec: &str) -> Option<ModelEntry> {
        let mut entry = parse_model_entry(spec)?;
        entry.enabled = self.is_enabled(spec);
        if !entry.enabled {
            entry.tier = if entry.tier.is_empty() {
                OFF_LABEL.to_owned()
            } else {
                format!("{OFF_PREFIX}{}", entry.tier)
            };
        }
        Some(entry)
    }

    fn preselect_current_model(&mut self) {
        if self
            .picker
            .select_item_by(|e| e.spec == self.current_spec && e.suffix().is_none())
            || self.picker.select_item_by(|e| e.spec == self.current_spec)
        {
            return;
        }
        // Nothing to land on by name, and the first row is a provider row.
        if !self.picker.select_item_by(|e| !e.is_provider) {
            self.picker.select(0);
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    fn track_anchor<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.picker.selected_index();
        let result = f(self);
        if let (Some(before), Some(after)) = (before, self.picker.selected_index())
            && before != after
        {
            self.anchor = self
                .picker
                .selected_item()
                .map(|e| (e.suffix().is_some(), e.spec.clone()));
        }
        result
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.track_anchor(|p| p.picker.handle_paste(text))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.track_anchor(|p| p.handle_key_inner(key))
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ModelPickerAction {
        if let Some(slug) = self
            .picker
            .selected_item()
            .filter(|e| e.is_provider)
            .map(|e| e.provider.clone())
        {
            // Enter would otherwise close the picker on a bare slug, and the
            // tier shortcuts have nothing to assign.
            if is_toggle_shortcut(key) || key.code == KeyCode::Enter {
                let enabled = !self.provider_is_enabled(&slug);
                self.set_provider_enabled(&slug, enabled);
                return ModelPickerAction::ToggleProvider(slug, enabled);
            }
            if tier_for_shortcut(key).is_some() {
                return ModelPickerAction::Consumed;
            }
        }
        if is_toggle_shortcut(key) {
            // Swallowed even with nothing to act on, so that an empty or
            // fully-filtered list does not type the shortcut into the search.
            let Some(entry) = self.picker.selected_item() else {
                return ModelPickerAction::Consumed;
            };
            // Read through `is_enabled` rather than the entry: entries are
            // rebuilt on the next refresh, so a second press inside one frame
            // would otherwise flip a stale `enabled` back to the same answer.
            let spec = entry.spec.clone();
            let enabled = !self.is_enabled(&spec);
            self.set_enabled(&spec, enabled);
            return ModelPickerAction::Toggle(spec, enabled);
        }
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
        {
            let spec = entry.spec.clone();
            self.needs_rebuild = true;
            if entry.override_tiers.contains(&tier) {
                ModelPickerAction::UnassignTier(spec, tier)
            } else {
                ModelPickerAction::AssignTier(spec, tier)
            }
        } else {
            match self.picker.handle_key(key) {
                PickerAction::Consumed => ModelPickerAction::Consumed,
                PickerAction::Select(entry) => ModelPickerAction::Select(entry.spec),
                PickerAction::Close => ModelPickerAction::Close,
                PickerAction::Toggle(..) => ModelPickerAction::Consumed,
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for ModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    // `opencode-go` has a spec row but was never a `ProviderKind`, so the
    // catalog named it and still should. That is what `is_native` filters for.
    let provider_display =
        if let Some(spec) = ProviderRegistry::get(provider_str).filter(|s| s.is_native()) {
            spec.display_name.to_string()
        } else if let Some(name) = dynamic::display_name(provider_str) {
            name.to_string()
        } else if let Some(info) = maki_providers::catalog_provider_if_available(provider_str) {
            info.display_name.clone()
        } else if let Some(builtin) = maki_config::providers::builtin_provider(provider_str) {
            builtin.display_name.to_string()
        } else {
            let config = maki_config::providers::ProvidersConfig::load();
            config.get(provider_str)?;
            maki_config::providers::resolve_display_name(provider_str, config.get(provider_str))
        };

    let override_tiers = model_registry::override_tiers(spec);
    let (tier, free) = match maki_providers::Model::from_spec(spec) {
        Ok(m) => (m.tier.to_string(), m.is_free()),
        Err(_) => (String::new(), false),
    };
    let tier = if override_tiers.is_empty() {
        tier
    } else {
        override_tiers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("/")
    };
    let tier = match (free, tier.is_empty()) {
        (true, true) => FREE_LABEL.to_string(),
        (true, false) => format!("{FREE_PREFIX}{tier}"),
        (false, _) => tier,
    };
    let id = model_id.to_string();
    Some(ModelEntry {
        spec: spec.to_string(),
        id,
        provider: provider_str.to_string(),
        provider_display,
        suffix: None,
        tier,
        override_tiers,
        free,
        enabled: true,
        is_provider: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent};
    use maki_providers::ModelInfo;
    use maki_providers::ModelPricing;
    use test_case::test_case;

    const SAME_SIZED_LIST: &str = "a republished list of the same length is still a new list";
    const SWAPPED_SPEC: &str = "zai/glm-5";

    /// A provider that republishes the same number of specs has still changed
    /// the list. Comparing lengths calls that no change, and the picker goes on
    /// offering models that are gone.
    #[test]
    fn a_same_sized_model_list_owes_a_frame() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(Arc::clone(&models), Arc::default());
        p.open("");
        assert_eq!(p.refresh(), Dirty::NO);

        models.store(Some(Arc::new(vec![SWAPPED_SPEC.into()])));
        assert_eq!(p.refresh(), Dirty::YES, "{SAME_SIZED_LIST}");
        assert_eq!(
            p.picker.selected_item().map(|e| e.spec.as_str()),
            Some(SWAPPED_SPEC),
            "{SAME_SIZED_LIST}"
        );
    }

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        models
    }

    const OFF_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const KEPT_OPEN: &str = "the switch is not a choice, so the picker stays up";

    fn policy_disabling(pattern: &str) -> Arc<ModelPolicy> {
        Arc::new(ModelPolicy::new(&[], &[], &[pattern.to_owned()], &[]).unwrap())
    }

    /// Both routes a terminal reports Shift+5 by.
    #[test_case(key(KeyCode::Char('%'))                                 ; "legacy_percent")]
    #[test_case(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::SHIFT)  ; "kitty_shift_5")]
    fn the_switch_flips_the_selected_model_and_says_so(k: KeyEvent) {
        let mut p = ModelPicker::new(test_models(), Arc::default());
        p.open(OFF_SPEC);

        let action = p.handle_key(k);
        assert!(
            matches!(&action, ModelPickerAction::Toggle(spec, false) if spec == OFF_SPEC),
            "the first press switches the selected model off"
        );
        assert!(p.is_open(), "{KEPT_OPEN}");
        assert!(!p.is_enabled(OFF_SPEC));
        assert_eq!(p.disabled_specs(), [OFF_SPEC]);

        let action = p.handle_key(k);
        assert!(
            matches!(&action, ModelPickerAction::Toggle(spec, true) if spec == OFF_SPEC),
            "the second press puts it back"
        );
        assert!(p.is_enabled(OFF_SPEC));
        assert!(p.disabled_specs().is_empty());
    }

    #[test]
    fn a_config_disabled_model_opens_off_but_present() {
        let mut p = ModelPicker::new(test_models(), policy_disabling("anthropic/*"));
        p.open(OFF_SPEC);

        let entry = p
            .load_entries()
            .into_iter()
            .find(|e| e.spec == OFF_SPEC)
            .expect("a switched-off model is listed, not hidden");
        assert!(!entry.enabled);
        assert!(entry.tier.starts_with(OFF_LABEL), "detail says it is off");

        assert!(p.is_enabled("zai/glm-5"), "other providers are untouched");
        p.handle_key(key(KeyCode::Char('%')));
        assert!(p.is_enabled(OFF_SPEC), "the session overrides the config");
    }

    const PROVIDER: &str = "anthropic";
    const OTHER_SPEC: &str = "anthropic/claude-opus-4-6-20260101";

    fn policy_disabling_provider(slug: &str) -> Arc<ModelPolicy> {
        Arc::new(ModelPolicy::new(&[], &[], &[], &[slug.to_owned()]).unwrap())
    }

    fn select_provider_row(p: &mut ModelPicker, slug: &str) {
        assert!(
            p.picker
                .select_item_by(|e| e.is_provider && e.provider == slug),
            "every provider gets a row of its own to put the cursor on"
        );
    }

    #[test]
    fn the_switch_collapses_the_selected_provider() {
        let mut p = ModelPicker::new(test_models(), Arc::default());
        p.open(OFF_SPEC);
        select_provider_row(&mut p, PROVIDER);

        let action = p.handle_key(key(KeyCode::Char('%')));
        assert!(
            matches!(&action, ModelPickerAction::ToggleProvider(slug, false) if slug == PROVIDER),
            "the press switches the selected provider off"
        );
        assert!(p.is_open(), "{KEPT_OPEN}");

        let entries = p.load_entries();
        assert!(
            entries
                .iter()
                .any(|e| e.is_provider && e.provider == PROVIDER && !e.enabled),
            "the provider stays listed, switched off"
        );
        assert!(
            !entries
                .iter()
                .any(|e| !e.is_provider && e.provider == PROVIDER),
            "and its models are collapsed away"
        );
        assert!(
            entries
                .iter()
                .any(|e| !e.is_provider && e.provider == "zai"),
            "other providers are untouched"
        );
        assert_eq!(p.disabled_provider_slugs(), [PROVIDER]);
        assert!(!p.is_enabled(OFF_SPEC), "nothing under it is offered");
    }

    #[test]
    fn a_provider_coming_back_restores_its_models() {
        let mut p = ModelPicker::new(test_models(), Arc::default());
        p.open(OFF_SPEC);
        p.set_enabled(OFF_SPEC, false);

        p.set_provider_enabled(PROVIDER, false);
        assert!(!p.is_enabled(OTHER_SPEC));

        p.set_provider_enabled(PROVIDER, true);
        assert!(p.is_enabled(OTHER_SPEC), "one that was on comes back on");
        assert!(!p.is_enabled(OFF_SPEC), "one that was off stays off");
    }

    /// Lua can ask before the picker has ever been opened to poll the slot.
    #[test]
    fn what_is_switched_off_answers_without_the_picker_open() {
        let p = ModelPicker::new(test_models(), policy_disabling_provider(PROVIDER));

        assert!(!p.is_open());
        assert_eq!(p.disabled_provider_slugs(), [PROVIDER]);
        assert_eq!(p.disabled_specs(), [OTHER_SPEC, OFF_SPEC]);
    }

    #[test]
    fn a_config_disabled_provider_opens_collapsed_and_enter_expands_it() {
        let mut p = ModelPicker::new(test_models(), policy_disabling_provider(PROVIDER));
        p.open("zai/glm-5");

        let entries = p.load_entries();
        let row = entries
            .iter()
            .find(|e| e.is_provider && e.provider == PROVIDER)
            .expect("a switched-off provider is listed, not hidden");
        assert!(!row.enabled);
        assert!(row.tier.starts_with(OFF_LABEL), "detail says it is off");
        assert!(row.tier.contains('2'), "and how much it is hiding");
        assert!(
            !entries
                .iter()
                .any(|e| !e.is_provider && e.provider == PROVIDER),
            "its models start collapsed away"
        );

        select_provider_row(&mut p, PROVIDER);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&action, ModelPickerAction::ToggleProvider(slug, true) if slug == PROVIDER),
            "Enter on a provider row works the switch"
        );
        assert!(p.is_open(), "{KEPT_OPEN}");
        assert!(
            p.load_entries()
                .iter()
                .any(|e| !e.is_provider && e.provider == PROVIDER),
            "the session overrides the config"
        );
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models(), Arc::default());
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(models.clone(), Arc::default());
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let mut p = ModelPicker::new(test_models(), Arc::default());
        p.open("anthropic/claude-opus-4-6-20260101");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101")
        );
    }

    #[test]
    fn parse_model_entry_valid() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert_eq!(entry.id, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert!(!entry.tier.is_empty());
    }

    #[test]
    fn parse_model_entry_paid_model_not_marked_free() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert!(
            !entry.tier.starts_with(FREE_PREFIX),
            "paid anthropic model must not be marked free"
        );
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test_case(key(KeyCode::Char('!')),           ModelTier::Strong     ; "legacy_bang_strong")]
    #[test_case(key(KeyCode::Char('$')),           ModelTier::Compaction ; "legacy_dollar_compaction")]
    #[test_case(key(KeyCode::Char('€')),           ModelTier::Compaction ; "legacy_euro_compaction")]
    fn tier_shortcut_assigns_and_keeps_picker_open(k: KeyEvent, want: ModelTier) {
        let mut p = ModelPicker::new(test_models(), Arc::default());
        p.open("anthropic/claude-sonnet-4-20250514");
        let action = p.handle_key(k);
        assert!(
            matches!(&action, ModelPickerAction::AssignTier(s, t)
                if s == "anthropic/claude-sonnet-4-20250514" && *t == want),
            "expected AssignTier(claude-sonnet, {want:?}), got something else",
        );
        assert!(p.is_open());
    }

    #[test]
    fn refresh_preserves_selection_for_current_model() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone(), Arc::default());
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101"),
            "after async model arrival, current model should still be selected"
        );
    }

    #[test]
    fn recents_include_current_model_preselected() {
        let models = test_models();
        let mut p = ModelPicker::new(models, Arc::default());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101");

        p.picker.select(0);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "first entry should be the most recent model",
        );

        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "current model should be preselected in its provider section",
        );
    }

    #[test]
    fn reopen_preselects_current_model_in_provider_section() {
        let models = test_models();
        let mut p = ModelPicker::new(models, Arc::default());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        // Past the Z.AI row that now heads its own section, onto the model.
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "selecting the provider entry should return its spec",
        );

        p.open("zai/glm-5");

        let entry = p.picker.selected_item().expect("selection on reopen");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            entry.section(),
            Some("Z.AI"),
            "selection should land on the provider entry, not the Recent copy",
        );
    }

    #[test]
    fn refresh_keeps_selection_on_provider_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models, Arc::default());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Char('!')));

        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            entry.section(),
            Some("Z.AI"),
            "selection should stay on the provider entry, not jump to Recent",
        );
    }

    #[test]
    fn refresh_after_collapse_anchors_to_provider_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone(), Arc::default());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");

        models.store(None);
        let _ = p.refresh();
        let entry = p.picker.selected_item().expect("selection during collapse");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(entry.section(), Some("Recent"));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(
            entry.section(),
            Some("Anthropic"),
            "cursor should migrate to the provider entry once it arrives",
        );
    }

    #[test]
    fn refresh_preserves_navigation_to_recent_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone(), Arc::default());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        models.store(None);
        let _ = p.refresh();
        p.handle_key(key(KeyCode::Down));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            entry.section(),
            Some("Recent"),
            "user navigation to a Recent entry should survive refresh",
        );
    }

    #[test]
    fn refresh_preserves_selection_with_active_search() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone(), Arc::default());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_key(key(KeyCode::Char('g')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('m')));

        models.store(None);
        let _ = p.refresh();
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(entry.section(), Some("Z.AI"));
    }

    fn discovered(id: &str, pricing: ModelPricing) -> ModelInfo {
        ModelInfo {
            pricing: Some(pricing),
            ..ModelInfo::id_only(id.into())
        }
    }

    const OX_SPEC: &str = "openrouter/stealth/ox-alpha";
    const PAID_ID: &str = "vendor/paid-model";
    const PAID_PRICING: ModelPricing = ModelPricing::per_million(3.0, 15.0, 0.0, 0.0);

    fn register_openrouter_models() {
        model_registry::set_known_models(
            "openrouter",
            vec![
                discovered("stealth/ox-alpha", ModelPricing::ZERO),
                discovered(PAID_ID, PAID_PRICING),
            ],
        );
    }

    #[test]
    fn zero_priced_discovery_marks_entry_free() {
        register_openrouter_models();
        let entry = parse_model_entry(OX_SPEC).unwrap();
        assert!(
            entry.tier.starts_with(FREE_PREFIX),
            "zero-priced discovery must mark the entry free"
        );
    }

    #[test]
    fn paid_discovery_not_marked_free() {
        register_openrouter_models();
        let entry = parse_model_entry(&format!("openrouter/{PAID_ID}")).unwrap();
        assert!(
            !entry.tier.starts_with(FREE_PREFIX),
            "paid discovery must not mark the entry free"
        );
    }

    #[test]
    fn free_models_sort_before_paid_within_a_provider() {
        register_openrouter_models();
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            format!("openrouter/{PAID_ID}"),
            OX_SPEC.into(),
        ])));
        let mut p = ModelPicker::new(models, Arc::default());
        p.open("");
        let entries = p.load_entries();
        let ids: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_provider)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["stealth/ox-alpha", PAID_ID]);
    }
}
