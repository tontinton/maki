use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use maki_providers::Model;
use maki_providers::ModelTier;
use maki_providers::model_registry;
use maki_providers::models_cache::ModelList;
use maki_providers::provider::ProviderStatus;
use maki_providers::spec::{Build, ProviderRegistry};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::{Cadence, Dirty, Watch};

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";
const FREE_LABEL: &str = "Free";
const FREE_PREFIX: &str = "Free · ";
const PRICE_SEPARATOR: &str = " · ";
const PRICE_WIDTH: usize = 7;
const LOADING_NOTICE: &str = "loading models...";
const NO_KEY_LABEL: &str = "no key";
const OFFLINE_LABEL: &str = "offline";
const FALLBACK_LABEL: &str = "fallback";
const CURRENT_MARK: &str = "● ";
const FOOTER: &[(&str, &str)] = &[
    ("Enter", "select"),
    ("!", "strong"),
    ("@", "medium"),
    ("#", "weak"),
    ("$", "compaction"),
    ("%", "fold"),
    ("*", "fold all"),
];
const FOLD_KEY: KeyCode = KeyCode::Char('%');
const FOLD_ALL_KEY: KeyCode = KeyCode::Char('*');

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

/// A provider's one row, which its models hide behind while it is folded. It
/// says how many it has, and what discovery said when that needs fixing.
fn provider_row(
    slug: String,
    display: String,
    count: usize,
    status: Option<&ProviderStatus>,
) -> ModelEntry {
    let (status, needs_setup) = match status {
        Some(ProviderStatus::MissingKey) => (Some(NO_KEY_LABEL), true),
        Some(ProviderStatus::Failed(_)) if count == 0 => (Some(OFFLINE_LABEL), true),
        Some(ProviderStatus::Failed(_)) => (Some(FALLBACK_LABEL), false),
        Some(ProviderStatus::Live) | None => (None, false),
    };
    let plural = if count == 1 { "model" } else { "models" };
    ModelEntry {
        spec: slug.clone(),
        id: display.clone(),
        provider: slug,
        provider_display: display,
        suffix: status.map(str::to_string),
        detail: format!("{count} {plural}"),
        override_tiers: Vec::new(),
        free: false,
        is_provider: true,
        hidden: false,
        needs_setup,
    }
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    /// The unfolded set changed. The picker has no state dir, so the caller
    /// saves [`ModelPicker::expanded_providers`].
    Folded,
    /// Enter on a provider that cannot serve models until it is set up.
    Login(String),
    Close,
}

struct ModelEntry {
    /// The qualified spec, or the bare slug on a provider row.
    spec: String,
    id: String,
    provider: String,
    provider_display: String,
    suffix: Option<String>,
    detail: String,
    override_tiers: Vec<ModelTier>,
    free: bool,
    is_provider: bool,
    /// A model of a folded provider, listed only while searching.
    hidden: bool,
    needs_setup: bool,
}

impl ModelEntry {
    fn is_recent(&self) -> bool {
        self.provider_display == RECENT_SECTION
    }
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.id
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
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

    fn is_hidden(&self) -> bool {
        self.hidden
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<ModelList>>,
    available: Watch<ModelList>,
    recents: Vec<String>,
    current_spec: String,
    needs_rebuild: bool,
    /// Entry to restore on refresh: `(was_recent, spec)`. Seeded from the
    /// preselection at open so it survives the background list arriving, and
    /// moved by user navigation.
    anchor: Option<(bool, String)>,
    /// Providers listed with their models. Every other one is folded.
    expanded: BTreeSet<String>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<ModelList>>) -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER),
            models,
            available: Watch::default(),
            recents: Vec::new(),
            current_spec: String::new(),
            needs_rebuild: false,
            anchor: None,
            expanded: BTreeSet::new(),
        }
    }

    pub fn set_expanded(&mut self, expanded: BTreeSet<String>) {
        self.expanded = expanded;
        self.needs_rebuild = true;
    }

    pub fn expanded_providers(&self) -> &BTreeSet<String> {
        &self.expanded
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
        self.show_loading();
        if self.preselect_current_model() {
            self.anchor = self.selected_anchor();
        }
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
        self.show_loading();
        if let Some((was_recent, spec)) = &self.anchor {
            self.picker
                .select_item_by(|e| e.spec == *spec && e.is_recent() == *was_recent);
        } else {
            self.preselect_current_model();
        }
        Dirty::YES
    }

    /// A slow provider leaves a short list that looks final without this.
    fn show_loading(&mut self) {
        let loading = self.available.get().is_some_and(|list| list.loading);
        self.picker.set_notice(loading.then_some(LOADING_NOTICE));
    }

    fn load_entries(&self) -> Vec<ModelEntry> {
        let list = self.available.get();
        // A model the provider has retired must not linger in Recent. Only a
        // provider that answered live can say it is gone: a failed probe falls
        // back to a short static list, and snapshots taken while discovery is
        // still running are missing the providers that have yet to answer.
        let retired = |spec: &String| {
            list.is_some_and(|l| {
                !l.loading
                    && !l.specs.contains(spec)
                    && spec.split_once('/').is_some_and(|(provider, _)| {
                        l.providers.get(provider) == Some(&ProviderStatus::Live)
                    })
            })
        };
        let mut entries = Vec::new();
        for spec in &self.recents {
            if retired(spec) {
                continue;
            }
            if let Some(mut e) = parse_model_entry(spec) {
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        // Keyed by display name, then slug: the order the providers list in.
        let mut groups: BTreeMap<(String, String), Vec<ModelEntry>> = BTreeMap::new();
        for entry in list
            .iter()
            .flat_map(|l| &l.specs)
            .filter_map(|spec| parse_model_entry(spec))
        {
            let key = (entry.provider_display.clone(), entry.provider.clone());
            groups.entry(key).or_default().push(entry);
        }
        // A provider with nothing to pick still lists when it needs fixing.
        for (slug, status) in list.iter().flat_map(|l| &l.providers) {
            if *status != ProviderStatus::Live
                && let Some(display) = provider_display(slug)
            {
                groups.entry((display, slug.clone())).or_default();
            }
        }
        for ((display, slug), mut models) in groups {
            let status = list.and_then(|l| l.providers.get(&slug));
            let hidden = !self.expanded.contains(&slug);
            entries.push(provider_row(slug, display, models.len(), status));
            models.sort_by(|a, b| b.free.cmp(&a.free).then_with(|| a.id.cmp(&b.id)));
            entries.extend(
                models
                    .into_iter()
                    .map(|model| ModelEntry { hidden, ..model }),
            );
        }
        for entry in &mut entries {
            if entry.spec == self.current_spec {
                entry.id.insert_str(0, CURRENT_MARK);
            }
        }
        entries
    }

    fn preselect_current_model(&mut self) -> bool {
        if self
            .picker
            .select_item_by(|e| e.spec == self.current_spec && !e.is_recent())
            || self.picker.select_item_by(|e| e.spec == self.current_spec)
        {
            return true;
        }
        // The top row might be a folded provider, and Enter there unfolds it
        // instead of picking a model.
        self.picker.select_item_by(|e| !e.is_provider);
        false
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

    fn selected_anchor(&self) -> Option<(bool, String)> {
        self.picker
            .selected_item()
            .map(|e| (e.is_recent(), e.spec.clone()))
    }

    fn track_anchor<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.picker.selected_index();
        let result = f(self);
        if let (Some(before), Some(after)) = (before, self.picker.selected_index())
            && before != after
        {
            self.anchor = self.selected_anchor();
        }
        result
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.track_anchor(|p| p.picker.handle_paste(text));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.track_anchor(|p| p.handle_key_inner(key))
    }

    fn toggle_fold(&mut self, slug: String) -> ModelPickerAction {
        if !self.expanded.remove(&slug) {
            self.expanded.insert(slug.clone());
        }
        // Following the provider row keeps the cursor on it, so the next press
        // undoes this one.
        self.anchor = Some((false, slug));
        self.needs_rebuild = true;
        ModelPickerAction::Folded
    }

    fn toggle_all_folds(&mut self) -> ModelPickerAction {
        self.expanded = if self.expanded.is_empty() {
            self.available
                .get()
                .iter()
                .flat_map(|l| &l.specs)
                .filter_map(|spec| spec.split_once('/'))
                .map(|(slug, _)| slug.to_owned())
                .collect()
        } else {
            BTreeSet::new()
        };
        if let Some(entry) = self.picker.selected_item().filter(|e| !e.is_recent()) {
            self.anchor = Some((false, entry.provider.clone()));
        }
        self.needs_rebuild = true;
        ModelPickerAction::Folded
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ModelPickerAction {
        if key.code == FOLD_ALL_KEY {
            return self.toggle_all_folds();
        }
        let selected = self.picker.selected_item();
        // Enter on a provider row would hand back a bare slug as the model.
        if key.code == FOLD_KEY
            || (key.code == KeyCode::Enter && selected.is_some_and(|e| e.is_provider))
        {
            // Swallowed even on an empty list, so `%` never ends up typed
            // into the search.
            let Some(entry) = selected else {
                return ModelPickerAction::Consumed;
            };
            let slug = entry.provider.clone();
            if key.code == KeyCode::Enter && entry.needs_setup {
                return ModelPickerAction::Login(slug);
            }
            return self.toggle_fold(slug);
        }
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
        {
            if entry.is_provider {
                return ModelPickerAction::Consumed;
            }
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

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> (Rect, Option<Position>) {
        self.picker.view(frame, area)
    }
}

impl Overlay for ModelPicker {
    fn is_prompting(&self) -> bool {
        false
    }

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

fn format_pricing(model: &Model) -> Option<String> {
    let pricing = &model.pricing;
    (model.subsidised_by.is_none() && !pricing.is_zero()).then(|| {
        format!(
            "{:>PRICE_WIDTH$}/{:<PRICE_WIDTH$}",
            format!("${:.2}", pricing.input),
            format!("${:.2}", pricing.output),
        )
    })
}

/// A catalog-backed provider like `opencode-go` takes the name models.dev
/// gives it.
fn provider_display(slug: &str) -> Option<String> {
    if let Some(spec) = ProviderRegistry::get(slug).filter(|s| !matches!(s.build, Build::Catalog)) {
        Some(spec.display_name.to_string())
    } else if let Some(info) = maki_providers::catalog_provider_if_available(slug) {
        Some(info.display_name.clone())
    } else if let Some(builtin) = maki_config::providers::builtin_provider(slug) {
        Some(builtin.display_name.to_string())
    } else {
        let config = maki_config::providers::ProvidersConfig::load();
        let def = config.get(slug)?;
        Some(maki_config::providers::resolve_display_name(
            slug,
            Some(def),
        ))
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;
    let provider_display = provider_display(provider_str)?;

    let override_tiers = model_registry::override_tiers(spec);
    let (tier, free, price) = match Model::from_spec(spec) {
        Ok(m) => (m.tier.to_string(), m.is_free(), format_pricing(&m)),
        Err(_) => (String::new(), false, None),
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
    let detail = match price {
        Some(price) if !tier.is_empty() => format!("{tier}{PRICE_SEPARATOR}{price}"),
        Some(price) => price,
        None => tier,
    };
    let id = model_id.to_string();
    Some(ModelEntry {
        spec: spec.to_string(),
        id,
        provider: provider_str.to_string(),
        provider_display,
        suffix: None,
        detail,
        override_tiers,
        free,
        is_provider: false,
        hidden: false,
        needs_setup: false,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::components::keybindings::key as kb;
    use crate::components::{buffer_text, key};
    use crossterm::event::{KeyCode, KeyEvent};
    use maki_providers::ModelInfo;
    use maki_providers::ModelPricing;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const SAME_SIZED_LIST: &str = "a republished list of the same length is still a new list";
    const SWAPPED_SPEC: &str = "zai/glm-5";
    const RETIRED_SPEC: &str = "anthropic/claude-retired";
    const GLM_FILTER: &str = "glm";

    /// A provider that republishes the same number of specs has still changed
    /// the list. Comparing lengths calls that no change, and the picker goes on
    /// offering models that are gone.
    #[test]
    fn a_same_sized_model_list_owes_a_frame() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = unfolded(Arc::clone(&models));
        p.open("");
        assert_eq!(p.refresh(), Dirty::NO);

        models.store(Some(loaded(vec![SWAPPED_SPEC.into()])));
        assert_eq!(p.refresh(), Dirty::YES, "{SAME_SIZED_LIST}");
        assert_eq!(
            p.picker.selected_item().map(|e| e.spec.as_str()),
            Some(SWAPPED_SPEC),
            "{SAME_SIZED_LIST}"
        );
    }

    fn loaded(specs: Vec<String>) -> Arc<ModelList> {
        let providers = specs
            .iter()
            .filter_map(|spec| spec.split_once('/'))
            .map(|(provider, _)| (provider.to_string(), ProviderStatus::Live))
            .collect();
        Arc::new(ModelList {
            specs,
            providers,
            loading: false,
        })
    }

    /// A picker with every test provider unfolded, for tests about which model
    /// the cursor lands on.
    fn unfolded(models: Arc<ArcSwapOption<ModelList>>) -> ModelPicker {
        let mut p = ModelPicker::new(models);
        p.set_expanded(BTreeSet::from([PROVIDER.to_owned(), ZAI.to_owned()]));
        p
    }

    fn rendered(p: &mut ModelPicker) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| _ = p.view(f, f.area())).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// A slow provider must not look like a finished, short list.
    #[test]
    fn loading_row_shows_until_discovery_finishes() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(ModelList {
            specs: vec![SWAPPED_SPEC.into()],
            providers: HashMap::new(),
            loading: true,
        })));
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("");
        assert!(rendered(&mut p).contains(LOADING_NOTICE));

        models.store(Some(loaded(vec![SWAPPED_SPEC.into()])));
        assert_eq!(p.refresh(), Dirty::YES);
        assert!(!rendered(&mut p).contains(LOADING_NOTICE));
    }

    fn test_models() -> Arc<ArcSwapOption<ModelList>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        models
    }

    const FOLD_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const KEPT_OPEN: &str = "the fold is not a choice, so the picker stays up";
    const PROVIDER: &str = "anthropic";
    const ZAI: &str = "zai";

    fn lists_models_of(p: &ModelPicker, slug: &str) -> bool {
        p.load_entries()
            .iter()
            .any(|e| !e.is_provider && !e.hidden && e.provider == slug)
    }

    fn select_provider(p: &mut ModelPicker, slug: &str) {
        assert!(
            p.picker
                .select_item_by(|e| e.is_provider && e.provider == slug)
        );
    }

    #[test]
    fn providers_open_folded_but_search_still_finds_their_models() {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        assert!(!lists_models_of(&p, PROVIDER));
        assert!(!lists_models_of(&p, ZAI));

        p.handle_paste("glm");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, ModelPickerAction::Select(ref s) if s == SWAPPED_SPEC));
    }

    #[test_case(key(KeyCode::Char('%')) ; "the_fold_key")]
    #[test_case(key(KeyCode::Enter)     ; "enter")]
    fn a_provider_row_unfolds_and_folds_again(toggle: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        select_provider(&mut p, PROVIDER);

        let action = p.handle_key(toggle);
        assert!(matches!(action, ModelPickerAction::Folded));
        assert!(p.is_open(), "{KEPT_OPEN}");
        assert_eq!(
            p.expanded_providers(),
            &BTreeSet::from([PROVIDER.to_owned()])
        );
        let _ = p.refresh();
        assert!(lists_models_of(&p, PROVIDER), "its models come out");
        assert!(!lists_models_of(&p, ZAI), "other providers are untouched");

        p.handle_key(toggle);
        assert!(
            p.expanded_providers().is_empty(),
            "the cursor stayed on the row"
        );
    }

    #[test]
    fn the_fold_key_on_a_model_folds_its_provider() {
        let mut p = unfolded(test_models());
        p.open(FOLD_SPEC);

        p.handle_key(key(KeyCode::Char('%')));
        assert_eq!(p.expanded_providers(), &BTreeSet::from([ZAI.to_owned()]));
        let _ = p.refresh();
        assert_eq!(
            p.picker.selected_item().map(|e| e.spec.as_str()),
            Some(PROVIDER),
            "the cursor follows the fold onto the provider row"
        );
    }

    #[test]
    fn the_fold_all_key_unfolds_everything_then_folds_it_back() {
        let mut p = ModelPicker::new(test_models());
        p.open("");

        assert!(matches!(
            p.handle_key(key(FOLD_ALL_KEY)),
            ModelPickerAction::Folded
        ));
        assert_eq!(
            p.expanded_providers(),
            &BTreeSet::from([PROVIDER.to_owned(), ZAI.to_owned()])
        );

        p.handle_key(key(FOLD_ALL_KEY));
        assert!(p.expanded_providers().is_empty());
    }

    fn reported(specs: &[&str], status: ProviderStatus) -> Arc<ArcSwapOption<ModelList>> {
        Arc::new(ArcSwapOption::from_pointee(ModelList {
            specs: specs.iter().map(ToString::to_string).collect(),
            providers: HashMap::from([(PROVIDER.to_owned(), status)]),
            loading: false,
        }))
    }

    #[test_case(ProviderStatus::MissingKey,          &[],        Some(NO_KEY_LABEL),   true  ; "no_key")]
    #[test_case(ProviderStatus::Failed(String::new()), &[],        Some(OFFLINE_LABEL),  true  ; "offline")]
    #[test_case(ProviderStatus::Failed(String::new()), &[FOLD_SPEC], Some(FALLBACK_LABEL), false ; "fallback")]
    #[test_case(ProviderStatus::Live,                &[FOLD_SPEC], None,                 false ; "live")]
    fn a_provider_row_shows_what_discovery_said(
        status: ProviderStatus,
        specs: &[&str],
        label: Option<&str>,
        opens_setup: bool,
    ) {
        let mut p = ModelPicker::new(reported(specs, status));
        p.open("");
        select_provider(&mut p, PROVIDER);
        assert_eq!(p.picker.selected_item().unwrap().suffix(), label);

        let action = p.handle_key(key(KeyCode::Enter));
        if opens_setup {
            assert!(matches!(action, ModelPickerAction::Login(ref slug) if slug == PROVIDER));
        } else {
            assert!(matches!(action, ModelPickerAction::Folded));
        }
    }

    /// An allow-list can leave a provider that answered with nothing to pick,
    /// and a row for it would only be noise.
    #[test]
    fn a_live_provider_with_no_models_has_no_row() {
        let mut p = ModelPicker::new(reported(&[], ProviderStatus::Live));
        p.open("");
        assert!(p.load_entries().is_empty());
    }

    #[test]
    fn the_current_model_is_marked() {
        let mut p = ModelPicker::new(test_models());
        p.set_recents(vec![FOLD_SPEC.into()]);
        p.open(FOLD_SPEC);
        let marked: Vec<String> = p
            .load_entries()
            .into_iter()
            .filter(|e| e.id.starts_with(CURRENT_MARK))
            .map(|e| e.spec)
            .collect();
        assert_eq!(
            marked,
            [FOLD_SPEC, FOLD_SPEC],
            "its Recent copy and its own row"
        );
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.open("");

        p.handle_paste("opus");

        models.store(Some(loaded(vec![
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
        let mut p = unfolded(test_models());
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
        assert!(!entry.detail.is_empty());
    }

    #[test]
    fn parse_model_entry_paid_model_not_marked_free() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert!(
            !entry.detail.starts_with(FREE_PREFIX),
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
        let mut p = unfolded(test_models());
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
        let mut p = unfolded(models.clone());
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(loaded(vec![
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
        let mut p = unfolded(models);
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
        let mut p = unfolded(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        // Past the Z.AI row.
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
        let mut p = unfolded(models);
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
        let mut p = unfolded(models.clone());
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

        models.store(Some(loaded(vec![
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
    fn arrival_keeps_the_entry_preselected_at_open() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = unfolded(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");

        let entry = p.picker.selected_item().expect("selection on open");
        assert_eq!(
            entry.section(),
            Some("Recent"),
            "only recents are listed before the background fetch finishes",
        );

        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(
            entry.section(),
            Some("Recent"),
            "the entry preselected at open should stay selected when the provider list arrives",
        );
    }

    #[test]
    fn arrival_selects_current_model_missing_from_recents() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = unfolded(models.clone());
        p.set_recents(vec!["zai/glm-5".into()]);
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(loaded(vec![
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "anthropic/claude-opus-4-6-20260101");
    }

    /// Discovery publishes a snapshot per batch, so the preselected entry
    /// has to survive every intermediate list, not just the finished one.
    #[test]
    fn progressive_arrival_keeps_the_entry_preselected_at_open() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = unfolded(models.clone());
        p.set_recents(vec!["anthropic/claude-sonnet-4-20250514".into()]);
        p.open("anthropic/claude-sonnet-4-20250514");

        for specs in [
            vec!["anthropic/claude-sonnet-4-20250514".to_string()],
            vec![
                "anthropic/claude-sonnet-4-20250514".to_string(),
                "zai/glm-5".to_string(),
            ],
        ] {
            models.store(Some(Arc::new(ModelList {
                specs,
                providers: HashMap::new(),
                loading: true,
            })));
            let _ = p.refresh();
            let entry = p.picker.selected_item().expect("selection after a batch");
            assert_eq!(
                entry.section(),
                Some("Recent"),
                "an intermediate snapshot must not move the cursor to the provider entry",
            );
        }
    }

    #[test]
    fn recents_drop_models_the_provider_no_longer_lists() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = unfolded(models.clone());
        p.set_recents(vec![RETIRED_SPEC.into(), SWAPPED_SPEC.into()]);
        p.open(SWAPPED_SPEC);

        models.store(Some(Arc::new(ModelList {
            specs: vec![SWAPPED_SPEC.into()],
            providers: HashMap::from([
                ("anthropic".into(), ProviderStatus::Live),
                ("zai".into(), ProviderStatus::Live),
            ]),
            loading: false,
        })));
        let _ = p.refresh();

        let entries = p.load_entries();
        assert!(!entries.iter().any(|e| e.spec == RETIRED_SPEC));
        assert!(entries.iter().any(|e| e.spec == SWAPPED_SPEC));
    }

    /// A failed probe falls back to the provider's static specs, so a recent
    /// outside that short list is missing without being retired.
    #[test]
    fn recents_survive_a_failed_probe() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = unfolded(models.clone());
        p.set_recents(vec![RETIRED_SPEC.into()]);
        p.open(RETIRED_SPEC);

        models.store(Some(loaded(vec![SWAPPED_SPEC.into()])));
        let _ = p.refresh();

        assert!(p.load_entries().iter().any(|e| e.spec == RETIRED_SPEC));
    }

    /// A snapshot from a discovery still in flight is missing providers, so a
    /// recent absent from it may simply not have answered yet.
    #[test]
    fn recents_survive_an_incomplete_list() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = unfolded(models.clone());
        p.set_recents(vec![RETIRED_SPEC.into()]);
        p.open(RETIRED_SPEC);

        models.store(Some(Arc::new(ModelList {
            specs: vec![SWAPPED_SPEC.into()],
            providers: HashMap::new(),
            loading: true,
        })));
        let _ = p.refresh();

        assert!(p.load_entries().iter().any(|e| e.spec == RETIRED_SPEC));
    }

    #[test]
    fn refresh_preserves_navigation_to_recent_entry() {
        let models = test_models();
        let mut p = unfolded(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        models.store(None);
        let _ = p.refresh();
        p.handle_key(key(KeyCode::Down));

        models.store(Some(loaded(vec![
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

    #[test_case(
        |p: &mut ModelPicker| {
            for c in GLM_FILTER.chars() {
                p.handle_key(key(KeyCode::Char(c)));
            }
        }
        ; "typed"
    )]
    #[test_case(|p: &mut ModelPicker| p.handle_paste(GLM_FILTER) ; "pasted")]
    fn refresh_preserves_selection_with_active_search(filter: fn(&mut ModelPicker)) {
        let models = test_models();
        let mut p = unfolded(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        filter(&mut p);

        models.store(None);
        let _ = p.refresh();
        models.store(Some(loaded(vec![
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

    const OX_SPEC: &str = "ollama/stealth/ox-alpha";
    const PAID_ID: &str = "vendor/paid-model";
    const PAID_PRICING: ModelPricing = ModelPricing::per_million(3.0, 15.0, 0.0, 0.0);
    const PAID_PRICE_LABEL: &str = "  $3.00/$15.00 ";

    fn register_discovered_models() {
        model_registry::set_known_models(
            "ollama",
            vec![
                discovered("stealth/ox-alpha", ModelPricing::ZERO),
                discovered(PAID_ID, PAID_PRICING),
            ],
        );
    }

    #[test]
    fn zero_priced_discovery_marks_entry_free() {
        register_discovered_models();
        let entry = parse_model_entry(OX_SPEC).unwrap();
        assert!(
            entry.detail.starts_with(FREE_PREFIX),
            "zero-priced discovery must mark the entry free"
        );
    }

    #[test]
    fn paid_discovery_not_marked_free() {
        register_discovered_models();
        let entry = parse_model_entry(&format!("ollama/{PAID_ID}")).unwrap();
        assert!(
            !entry.detail.starts_with(FREE_PREFIX),
            "paid discovery must not mark the entry free"
        );
        assert!(
            entry
                .detail
                .ends_with(&format!("{PRICE_SEPARATOR}{PAID_PRICE_LABEL}")),
            "paid discovery must show its price"
        );
    }

    #[test]
    fn free_models_sort_before_paid_within_a_provider() {
        register_discovered_models();
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(loaded(vec![
            format!("ollama/{PAID_ID}"),
            OX_SPEC.into(),
        ])));
        let mut p = unfolded(models);
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
