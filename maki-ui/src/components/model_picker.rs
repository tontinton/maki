use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use unicode_width::UnicodeWidthStr;

use maki_providers::Model;
use maki_providers::ModelTier;
use maki_providers::model_registry;
use maki_providers::models_cache::ModelList;
use maki_providers::spec::{Build, ProviderRegistry};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::provider_auth::ProviderEntry;
use crate::repaint::{Cadence, Dirty, Watch};

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";
const FREE_SEGMENT: &str = "free";
const DETAIL_SEPARATOR: &str = " · ";
/// "compaction", the longest tier name.
const TIER_COL: usize = 10;
/// What `format_pricing` pads a price pair out to.
const PRICE_COL: usize = 15;
const PRICE_WIDTH: usize = 7;
const LOADING_NOTICE: &str = "loading models...";
const OFFLINE_LABEL: &str = "offline";
const FALLBACK_LABEL: &str = "fallback";
const EMPTY_LABEL: &str = "no models";
const ALL_PROVIDERS: &str = "all providers";
const CURRENT_MARK: &str = "\u{25cf} ";
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

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    /// `(slug, folded)`. The picker has no state dir, so the caller saves the
    /// new set.
    Collapse(String, bool),
    Close,
}

struct ModelEntry {
    /// The qualified spec, or the bare slug on a provider row.
    spec: String,
    label: String,
    id: String,
    provider: String,
    provider_display: String,
    suffix: Option<String>,
    detail: String,
    override_tiers: Vec<ModelTier>,
    free: bool,
    is_provider: bool,
    spinning: bool,
    hidden: bool,
    /// The model in use; marked on whichever row lists it.
    current: bool,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.label
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

    fn is_spinning(&self) -> bool {
        self.spinning
    }

    fn is_hidden(&self) -> bool {
        self.hidden
    }

    fn is_section_row(&self) -> bool {
        self.is_provider
    }
}
pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<ModelList>>,
    available: Watch<ModelList>,
    recents: Vec<String>,
    current_spec: String,
    needs_rebuild: bool,
    /// `(is_provider, is_recent_copy, spec)` — which row kind the cursor was
    /// on, so a rebuild finds the same kind again.
    anchor: Option<(bool, bool, String)>,
    /// Provider groups the user unfolded; every other group is a single
    /// row with its model count.
    expanded: BTreeSet<String>,
    /// The provider universe as of the last `open`, so rows exist for
    /// providers discovery has nothing to say about.
    providers: Vec<ProviderEntry>,
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
            providers: Vec::new(),
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

    pub fn open(&mut self, current_spec: &str, providers: &[ProviderEntry]) {
        self.current_spec = current_spec.to_owned();
        self.providers = providers.to_vec();
        self.anchor = None;
        self.needs_rebuild = false;
        let _ = self.available.poll(self.models.load_full());
        let entries = self.load_entries();
        self.picker.open(entries, TITLE);
        self.show_loading();
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
        if let Some((was_provider, was_recent, spec)) = &self.anchor {
            // A model row exists as a Recent copy and in its provider
            // section; the anchor stays on the kind the user navigated to.
            self.picker.select_item_by(|e| {
                e.spec == *spec
                    && e.is_provider == *was_provider
                    && (e.suffix.is_some() == *was_recent || e.is_provider)
            });
        }
        Dirty::YES
    }

    /// A slow provider leaves a short list that looks final without this.
    /// Once specs have arrived the per-provider spinners take over, and the
    /// notice would only deny the list that is already there.
    fn show_loading(&mut self) {
        let loading = self
            .available
            .get()
            .is_some_and(|list| list.loading && list.specs.is_empty());
        self.picker.set_notice(loading.then_some(LOADING_NOTICE));
    }

    fn load_entries(&self) -> Vec<ModelEntry> {
        let list = self.available.get();
        let mut entries = Vec::new();
        for spec in &self.recents {
            if let Some(mut e) = parse_model_entry(spec) {
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = list
            .map(|l| {
                l.specs
                    .iter()
                    .filter_map(|s| parse_model_entry(s))
                    .collect()
            })
            .unwrap_or_default();
        full.sort_by(|a, b| {
            a.provider_display
                .cmp(&b.provider_display)
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| b.free.cmp(&a.free))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut groups: HashMap<String, Vec<ModelEntry>> = HashMap::new();
        for entry in full {
            let slug = entry.provider.clone();
            groups.entry(slug).or_default().push(entry);
        }

        // One row per provider: the universe supplies auth state and covers
        // providers discovery says nothing about; a slug only discovery knows
        // (plugin-owned) still gets its row.
        let mut rows: Vec<(String, String)> = self
            .providers
            .iter()
            .filter(|p| p.usable())
            .map(|p| (p.slug.clone(), p.display_name.clone()))
            .collect();
        for slug in groups.keys() {
            if rows.iter().any(|(known, _)| known == slug) {
                continue;
            }
            let display = groups[slug][0].provider_display.clone();
            rows.push((slug.clone(), display));
        }
        rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let loading = list.is_some_and(|l| l.loading);
        for (slug, display) in rows {
            let count = groups.get(&slug).map_or(0, Vec::len);
            entries.push(self.provider_row(&slug, &display, count, loading));
            // A folded group keeps its models in the list, hidden: they must
            // not show, but a search has to still find them.
            let hidden = !self.expanded.contains(&slug);
            if let Some(mut models) = groups.remove(&slug) {
                for model in &mut models {
                    model.provider_display = display.clone();
                    model.hidden = hidden;
                }
                entries.extend(models);
            }
        }
        for entry in &mut entries {
            entry.current = !entry.is_provider && entry.spec == self.current_spec;
            if entry.current {
                entry.label = format!("{CURRENT_MARK}{label}", label = entry.label);
            }
        }
        entries
    }

    fn provider_row(&self, slug: &str, display: &str, count: usize, loading: bool) -> ModelEntry {
        let answered = self.available.get().and_then(|l| l.providers.get(slug));
        let (suffix, spinning) = if answered.is_none() && loading {
            (None, true)
        } else if answered.is_none() {
            (Some(OFFLINE_LABEL), false)
        } else if answered.is_some_and(|s| s.warning.is_some()) {
            (Some(FALLBACK_LABEL), false)
        } else if count == 0 {
            (Some(EMPTY_LABEL), false)
        } else {
            (None, false)
        };
        let plural = if count == 1 { "model" } else { "models" };
        ModelEntry {
            label: display.to_string(),
            spec: slug.to_string(),
            id: slug.to_string(),
            provider: slug.to_string(),
            provider_display: display.to_string(),
            suffix: suffix.map(str::to_string),
            detail: if count > 0 {
                format!("{count} {plural}")
            } else {
                String::new()
            },
            override_tiers: Vec::new(),
            free: false,
            is_provider: true,
            spinning,
            hidden: false,
            current: false,
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

    fn selected_anchor(&self) -> Option<(bool, bool, String)> {
        self.picker
            .selected_item()
            .map(|e| (e.is_provider, e.suffix.is_some(), e.spec.clone()))
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

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.track_anchor(|p| p.picker.handle_paste(text))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.track_anchor(|p| p.handle_key_inner(key))
    }

    fn fold(&mut self, slug: String, collapse: bool) -> ModelPickerAction {
        if collapse {
            self.expanded.remove(&slug);
        } else {
            self.expanded.insert(slug.clone());
        }
        // The provider row stays where it was either way, so following it
        // there keeps the cursor on it through the rebuild, and the next
        // press toggles back.
        self.anchor = Some((true, false, slug.clone()));
        self.needs_rebuild = true;
        ModelPickerAction::Collapse(slug, collapse)
    }

    /// One key from anywhere in the list: folded down to just the provider
    /// rows, or back out to every model.
    fn fold_all(&mut self) -> ModelPickerAction {
        let spec_slugs = || {
            self.available
                .get()
                .into_iter()
                .flat_map(|l| &l.specs)
                .filter_map(|s| s.split_once('/'))
                .map(|(slug, _)| slug.to_string())
        };
        let visible_slugs = || {
            self.providers
                .iter()
                .map(|p| p.slug.clone())
                .chain(spec_slugs())
        };
        let any_expanded = visible_slugs().any(|slug| self.expanded.contains(&slug));
        if any_expanded {
            self.expanded.clear();
        } else {
            self.expanded.extend(visible_slugs());
        }
        self.needs_rebuild = true;
        self.anchor = self.selected_anchor();
        ModelPickerAction::Collapse(ALL_PROVIDERS.to_string(), any_expanded)
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ModelPickerAction {
        if key.code == FOLD_ALL_KEY {
            return self.fold_all();
        }
        let on_provider_row = self.picker.selected_item().is_some_and(|e| e.is_provider);
        // Enter on a provider row would hand back a bare slug as the model.
        if key.code == FOLD_KEY || (on_provider_row && key.code == KeyCode::Enter) {
            // Swallowed even on an empty list, so `%` never ends up typed
            // into the search.
            let Some(entry) = self.picker.selected_item() else {
                return ModelPickerAction::Consumed;
            };
            let slug = entry.provider.clone();
            // A model row only shows in an open group, so its key folds;
            // the provider row toggles.
            let collapse = if entry.is_provider {
                self.expanded.contains(&slug)
            } else {
                true
            };
            return self.fold(slug, collapse);
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

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
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

/// The right column of a model row, in fixed fields so rows line up:
/// the tier right-aligned in its column, then `free` or the pricing
/// left-aligned in the value column. Missing fields leave their column
/// blank (trailing padding), which the right-aligned render hides.
fn format_detail(tier: &str, free: bool, price: Option<&str>) -> String {
    let tier_field = (!tier.is_empty()).then(|| format!("{tier:>TIER_COL$}"));
    let value_field = if free {
        Some(format!("{FREE_SEGMENT:>PRICE_COL$}"))
    } else {
        price.map(|p| format!("{:>PRICE_COL$}", p.trim_end()))
    };
    match (tier_field, value_field) {
        (Some(tier), Some(value)) => format!("{tier}{DETAIL_SEPARATOR}{value}"),
        (Some(tier), None) => format!("{tier}{}", " ".repeat(DETAIL_SEPARATOR.width() + PRICE_COL)),
        (None, Some(value)) => {
            format!("{}{value}", " ".repeat(TIER_COL + DETAIL_SEPARATOR.width()))
        }
        (None, None) => String::new(),
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    // A catalog-backed row like `opencode-go` takes the name models.dev gives it.
    let provider_display = if let Some(spec) =
        ProviderRegistry::get(provider_str).filter(|s| !matches!(s.build, Build::Catalog))
    {
        spec.display_name.to_string()
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
    let detail = format_detail(&tier, free, price.as_deref());

    let id = model_id.to_string();
    Some(ModelEntry {
        spec: spec.to_string(),
        label: id.clone(),
        id,
        provider: provider_str.to_string(),
        provider_display,
        suffix: None,
        detail,
        override_tiers,
        free,
        is_provider: false,
        spinning: false,
        hidden: false,
        current: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    /// A provider that republishes the same number of specs has still changed
    /// the list. Comparing lengths calls that no change, and the picker goes on
    /// offering models that are gone.
    #[test]
    fn a_same_sized_model_list_owes_a_frame() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("anthropic/claude-sonnet-4-20250514", &[]);
        assert_eq!(p.refresh(), Dirty::NO);

        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            SWAPPED_SPEC.into(),
        ])));
        assert_eq!(p.refresh(), Dirty::YES, "{SAME_SIZED_LIST}");
        let entries = p.load_entries();
        assert!(
            entries
                .iter()
                .any(|e| !e.is_provider && e.spec == SWAPPED_SPEC),
            "the republished model is listed"
        );
    }

    fn loaded(specs: Vec<String>) -> Arc<ModelList> {
        Arc::new(ModelList {
            specs,
            loading: false,
            providers: BTreeMap::new(),
        })
    }

    fn rendered(p: &mut ModelPicker) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| _ = p.view(f, f.area())).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// A slow provider must not look like a finished, short list. The global
    /// notice covers only the nothing-arrived-yet window; per-provider
    /// spinners take it from there.
    #[test]
    fn loading_row_shows_until_discovery_finishes() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(ModelList {
            specs: Vec::new(),
            loading: true,
            providers: BTreeMap::new(),
        })));
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("", &[]);
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

    fn open_test_models(current_spec: &str) -> ModelPicker {
        let mut p = ModelPicker::new(test_models());
        p.open(current_spec, &[]);
        p
    }

    const FOLD_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const KEPT_OPEN: &str = "the fold is not a choice, so the picker stays up";
    const PROVIDER: &str = "anthropic";

    fn lists_models_of(p: &ModelPicker, slug: &str) -> bool {
        p.load_entries()
            .iter()
            .any(|e| !e.is_provider && !e.hidden && e.provider == slug)
    }

    /// Every group starts folded down to its row, which says how much is
    /// behind it; the key opens it, and the current model's provider comes
    /// up open so the pick has a visible "you are here".
    #[test]
    fn a_provider_row_folds_and_opens_its_group() {
        let mut p = open_test_models(FOLD_SPEC);

        assert!(
            !lists_models_of(&p, PROVIDER) && !lists_models_of(&p, "zai"),
            "every group starts folded down to its row"
        );
        let entries = p.load_entries();
        let row = entries
            .iter()
            .find(|e| e.is_provider && e.provider == "zai")
            .expect("the folded provider keeps a row");
        assert_eq!(row.detail, "1 model", "saying how much is behind it");

        p.picker
            .select_item_by(|e| e.is_provider && e.provider == "zai");
        let action = p.handle_key(key(KeyCode::Char('%')));
        assert!(
            matches!(&action, ModelPickerAction::Collapse(slug, false) if slug == "zai"),
            "the key opens the group of whichever row is selected"
        );
        assert!(p.is_open(), "{KEPT_OPEN}");
        assert!(lists_models_of(&p, "zai"));

        let _ = p.refresh();
        p.handle_key(key(KeyCode::Char('%')));
        assert!(!lists_models_of(&p, "zai"), "the same key folds it back");
    }

    #[test_case(key(KeyCode::Char('%')) ; "the_fold_key")]
    #[test_case(key(KeyCode::Enter)     ; "enter")]
    fn a_saved_expansion_opens_open_and_folds_again(unfold: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.set_expanded(BTreeSet::from([PROVIDER.to_owned()]));
        p.open("zai/glm-5", &[]);
        assert!(lists_models_of(&p, PROVIDER));
        assert!(
            p.picker
                .select_item_by(|e| e.is_provider && e.provider == PROVIDER)
        );

        let action = p.handle_key(unfold);
        assert!(
            matches!(&action, ModelPickerAction::Collapse(slug, true) if slug == PROVIDER),
            "the row answers the key with a fold"
        );
        assert!(p.is_open(), "{KEPT_OPEN}");
        assert!(!lists_models_of(&p, PROVIDER), "its models go away");
    }

    /// The cursor follows the fold onto the provider row, so a second
    /// press undoes the first.
    #[test]
    fn the_fold_key_toggles() {
        let mut p = open_test_models(FOLD_SPEC);

        // Everything folded: the cursor rests on the first provider row.
        p.handle_key(key(KeyCode::Char('%')));
        assert_eq!(
            p.expanded_providers(),
            &BTreeSet::from([PROVIDER.to_owned()])
        );

        // The app rebuilds the list on its next tick.
        let _ = p.refresh();
        p.handle_key(key(KeyCode::Char('%')));
        assert!(p.expanded_providers().is_empty());
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("", &[]);
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
        p.open("", &[]);

        for c in ['o', 'p', 'u', 's'] {
            p.handle_key(key(KeyCode::Char(c)));
        }

        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, the filter should still match opus"
        );
    }

    /// The picker never moves the cursor onto the current model: it opens
    /// at the top of the list, and the model in use carries the mark.
    #[test]
    fn open_marks_the_current_model_without_moving_the_cursor() {
        let mut p = ModelPicker::new(test_models());
        p.set_recents(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101", &[]);

        let entry = p.picker.selected_item().expect("selection on open");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert!(!entry.current, "the cursor did not jump");

        let entries = p.load_entries();
        let current = entries
            .iter()
            .find(|e| e.spec == "anthropic/claude-opus-4-6-20260101")
            .expect("the current model is listed");
        assert!(current.current);
        assert!(current.label.starts_with(CURRENT_MARK));
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
            !entry.detail.trim_end().ends_with(FREE_SEGMENT),
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
        let mut p = ModelPicker::new(test_models());
        p.set_recents(vec!["anthropic/claude-sonnet-4-20250514".into()]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);
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
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec!["anthropic/claude-opus-4-6-20260101".into()]);
        p.open("anthropic/claude-opus-4-6-20260101", &[]);

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
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101", &[]);

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
        p.open("zai/glm-5", &[]);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "current model should be preselected in its provider section",
        );
    }

    /// The default view already is just the provider rows, so the first
    /// press opens every group and the second closes them all again.
    #[test]
    fn fold_all_opens_every_group_and_back() {
        let mut p = open_test_models(FOLD_SPEC);

        let action = p.handle_key(key(KeyCode::Char('*')));
        assert!(
            matches!(&action, ModelPickerAction::Collapse(slug, false) if slug == ALL_PROVIDERS)
        );
        assert!(lists_models_of(&p, PROVIDER));
        assert!(lists_models_of(&p, "zai"));

        let action = p.handle_key(key(KeyCode::Char('*')));
        assert!(
            matches!(&action, ModelPickerAction::Collapse(slug, true) if slug == ALL_PROVIDERS)
        );
        assert!(!lists_models_of(&p, PROVIDER));
        assert!(!lists_models_of(&p, "zai"));
    }

    /// A probe that fell back to static specs says so on the provider row,
    /// not just in a status-bar flash.
    #[test]
    fn a_degraded_probe_marks_its_provider_row() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            maki_providers::models_cache::ProviderStatus {
                models: 2,
                warning: Some("Anthropic: down (using static fallback)".into()),
                missing_credentials: false,
            },
        );
        models.store(Some(Arc::new(ModelList {
            specs: vec![
                "anthropic/claude-sonnet-4-20250514".into(),
                "anthropic/claude-opus-4-6-20260101".into(),
            ],
            loading: false,
            providers,
        })));
        let mut p = ModelPicker::new(models);
        p.open("", &[]);

        let entries = p.load_entries();
        let row = entries
            .iter()
            .find(|e| e.is_provider && e.provider == PROVIDER)
            .expect("the provider keeps a row");
        assert_eq!(row.suffix.as_deref(), Some(FALLBACK_LABEL));
        assert_eq!(row.detail, "2 models");
    }

    /// A provider whose probe never landed reads as offline, not as empty.
    #[test]
    fn an_unanswered_probe_marks_its_provider_row_offline() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(loaded(vec!["zai/glm-5".into()])));
        let mut p = ModelPicker::new(models);
        p.open(
            "",
            &[ProviderEntry {
                slug: "mistral".into(),
                display_name: "Mistral".into(),
                auth: crate::components::provider_auth::Auth::Saved,
                configured: false,
                from_catalog: false,
            }],
        );

        let entries = p.load_entries();
        let row = entries
            .iter()
            .find(|e| e.is_provider && e.provider == "mistral")
            .expect("the universe provider keeps a row");
        assert_eq!(row.suffix.as_deref(), Some(OFFLINE_LABEL));
    }

    #[test]
    fn refresh_keeps_selection_on_provider_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);
        for _ in 0..3 {
            p.handle_key(key(KeyCode::Down));
        }
        p.handle_key(key(KeyCode::Enter));
        let _ = p.refresh();
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
    fn refresh_keeps_the_current_model_on_its_recent_row() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);

        models.store(None);
        let _ = p.refresh();

        models.store(Some(loaded(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(
            entry.spec, "zai/glm-5",
            "the cursor never moved off the top"
        );
        let entries = p.load_entries();
        let current = entries
            .iter()
            .find(|e| e.spec == "anthropic/claude-sonnet-4-20250514")
            .expect("the current model is listed");
        assert!(current.current, "the mark rides its Recent row");
    }

    #[test]
    fn arrival_keeps_the_cursor_where_it_opened() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);

        let entry = p.picker.selected_item().expect("selection on open");
        assert_eq!(
            entry.section(),
            Some("Recent"),
            "only recents are listed before the background fetch finishes",
        );
        assert_eq!(
            entry.spec, "zai/glm-5",
            "the top of the list, not the current model"
        );

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
            "the cursor stays where it opened when the provider list arrives",
        );
    }

    /// The current model hides with its folded group, so the cursor stays
    /// where the user can see it instead of jumping to an invisible row.
    #[test]
    fn arrival_with_hidden_current_model_keeps_the_visible_pick() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec!["zai/glm-5".into()]);
        p.open("anthropic/claude-opus-4-6-20260101", &[]);

        models.store(Some(loaded(vec![
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(entry.section(), Some("Recent"));
    }

    /// Discovery publishes a snapshot per batch, so the preselected entry
    /// has to survive every intermediate list, not just the finished one.
    #[test]
    fn progressive_arrival_keeps_the_entry_preselected_at_open() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec!["anthropic/claude-sonnet-4-20250514".into()]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);

        for specs in [
            vec!["anthropic/claude-sonnet-4-20250514".to_string()],
            vec![
                "anthropic/claude-sonnet-4-20250514".to_string(),
                "zai/glm-5".to_string(),
            ],
        ] {
            models.store(Some(Arc::new(ModelList {
                specs,
                loading: true,
                providers: BTreeMap::new(),
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
    fn refresh_preserves_navigation_to_recent_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);
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
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(entry.section(), Some("Recent"));
    }

    #[test]
    fn refresh_preserves_selection_with_active_search() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514", &[]);
        p.handle_key(key(KeyCode::Char('g')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('m')));

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
        assert_eq!(
            entry.section(),
            Some("Recent"),
            "the filter's first hit keeps the cursor, no preselect required",
        );
    }

    /// Every detail is a fixed two-column layout: the tier right-aligned in
    /// its column, the free flag or pricing left-aligned in the value
    /// column, blanks padding missing fields so rows line up.
    #[test]
    fn detail_columns_are_aligned() {
        assert_eq!(
            format_detail("medium", false, Some("  $3.00/$15.00 ")),
            format!("{:>TIER_COL$} · {:>PRICE_COL$}", "medium", "$3.00/$15.00")
        );
        assert_eq!(
            format_detail("weak", false, None),
            format!(
                "{:>TIER_COL$}{}",
                "weak",
                " ".repeat(DETAIL_SEPARATOR.width() + PRICE_COL)
            )
        );
        assert_eq!(
            format_detail("", true, None),
            format!(
                "{}{FREE_SEGMENT:>PRICE_COL$}",
                " ".repeat(TIER_COL + DETAIL_SEPARATOR.width())
            )
        );
        assert_eq!(format_detail("", false, None), "");
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
            entry.detail.trim_end().ends_with(FREE_SEGMENT),
            "zero-priced discovery must mark the entry free, got {:?}",
            entry.detail
        );
    }

    #[test]
    fn paid_discovery_not_marked_free() {
        register_discovered_models();
        let entry = parse_model_entry(&format!("ollama/{PAID_ID}")).unwrap();
        assert!(
            !entry.detail.trim_end().ends_with(FREE_SEGMENT),
            "paid discovery must not mark the entry free"
        );
        assert!(
            entry
                .detail
                .trim_end()
                .ends_with(PAID_PRICE_LABEL.trim_end()),
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
        let mut p = ModelPicker::new(models);
        p.open("", &[]);
        let entries = p.load_entries();
        let ids: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_provider)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["stealth/ox-alpha", PAID_ID]);
    }
}

#[cfg(test)]
mod layout_dbg {
    use super::*;
    use std::collections::BTreeMap;

    use crate::components::buffer_text;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn dbg_layout() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(ModelList {
            specs: vec![
                "zai/glm-5.3-flash".into(),
                "mistral/zai-glm-5-3-flash".into(),
            ],
            loading: false,
            providers: BTreeMap::new(),
        })));
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "opencode/beam-free".into(),
            "mistral/zai-glm-5-3-flash".into(),
            "zai/glm-5.3-flash".into(),
        ]);
        p.open("opencode/beam-free", &[]);
        let mut t = Terminal::new(TestBackend::new(100, 12)).unwrap();
        t.draw(|f| _ = p.view(f, f.area())).unwrap();
        let text = buffer_text(t.backend().buffer());
        for line in text.lines() {
            if let Some(pos) = line.find("medium").or_else(|| line.find("weak")) {
                let dpos = line.find("$0.15").unwrap_or(line.find("free").unwrap_or(0));
                println!("tier@{pos} value@{dpos} | {}", line.trim_end());
            }
        }
    }
}
