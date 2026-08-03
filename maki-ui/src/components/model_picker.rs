use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use maki_providers::ModelTier;
use maki_providers::dynamic;
use maki_providers::model_registry;
use maki_providers::provider::ProviderKind;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::theme;

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";

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
    ])
}

fn tier_for_shortcut(key: KeyEvent) -> Option<ModelTier> {
    let digit = match (key.code, key.modifiers.contains(KeyModifiers::SHIFT)) {
        // Kitty protocol: Shift+digit reported with base key + SHIFT modifier
        (KeyCode::Char(c @ '1'..='4'), true) => c,
        // Legacy terminals: Shift+digit reported as the resulting character
        (KeyCode::Char('!' | '¡'), false) => '1', // US, ES
        (KeyCode::Char('@' | '"' | '™'), false) => '2', // US, UK/DE
        (KeyCode::Char('#' | '§' | '£'), false) => '3', // US, DE, UK
        (KeyCode::Char('$' | '€' | '¤'), false) => '4', // US, EU, Nordic
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
    Close,
}

struct ModelEntry {
    spec: String,
    id: String,
    provider_display: String,
    suffix: Option<String>,
    tier: String,
    override_tiers: Vec<ModelTier>,
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
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    recents: Vec<String>,
    current_spec: String,
    last_spec_count: usize,
    dirty: bool,
    /// User-moved entry to restore on refresh: `(was_recent, spec)`.
    anchor: Option<(bool, String)>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(footer_line),
            models,
            recents: Vec::new(),
            current_spec: String::new(),
            last_spec_count: 0,
            dirty: false,
            anchor: None,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.dirty = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        self.current_spec = current_spec.to_owned();
        self.anchor = None;
        self.dirty = false;
        let entries = self.load_entries();
        self.picker.open(entries, TITLE);
        self.preselect_current_model();
    }

    fn try_refresh(&mut self) {
        if !self.picker.is_open() {
            return;
        }
        let guard = self.models.load();
        let spec_count = guard.as_deref().map_or(0, Vec::len);
        if spec_count == self.last_spec_count && !self.dirty {
            return;
        }
        drop(guard);
        self.dirty = false;
        let entries = self.load_entries();
        self.picker.replace_items(entries);
        if let Some((was_recent, spec)) = &self.anchor {
            self.picker
                .select_item_by(|e| e.spec == *spec && e.suffix().is_some() == *was_recent);
        } else {
            self.preselect_current_model();
        }
    }

    fn load_entries(&mut self) -> Vec<ModelEntry> {
        let guard = self.models.load();
        let specs = guard.as_deref();
        self.last_spec_count = specs.map_or(0, Vec::len);
        let mut entries = Vec::new();
        for spec in &self.recents {
            if let Some(mut e) = parse_model_entry(spec) {
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = specs
            .map(|s| s.iter().filter_map(|s| parse_model_entry(s)).collect())
            .unwrap_or_default();
        full.sort_by(|a, b| {
            a.provider_display
                .cmp(&b.provider_display)
                .then_with(|| a.id.cmp(&b.id))
        });
        entries.extend(full);
        entries
    }

    fn preselect_current_model(&mut self) {
        if !self
            .picker
            .select_item_by(|e| e.spec == self.current_spec && e.suffix().is_none())
        {
            self.picker.select_item_by(|e| e.spec == self.current_spec);
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
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
        {
            let spec = entry.spec.clone();
            self.dirty = true;
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
        self.try_refresh();
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
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    let provider_display = if let Ok(kind) = provider_str.parse::<ProviderKind>() {
        kind.display_name().to_string()
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

    let map = model_registry::model_registry().read().unwrap();
    let override_tiers: Vec<ModelTier> = [
        ModelTier::Strong,
        ModelTier::Medium,
        ModelTier::Weak,
        ModelTier::Compaction,
    ]
    .into_iter()
    .filter(|&t| map.has_override(spec, t))
    .collect();
    let override_label = map.override_tier_label(spec);
    drop(map);
    let tier = override_label.unwrap_or_else(|| match maki_providers::Model::from_spec(spec) {
        Ok(m) => m.tier.to_string(),
        Err(_) => String::new(),
    });
    let id = model_id.to_string();
    Some(ModelEntry {
        spec: spec.to_string(),
        id,
        provider_display,
        suffix: None,
        tier,
        override_tiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        models
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
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let mut p = ModelPicker::new(test_models());
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
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test_case(key(KeyCode::Char('!')),           ModelTier::Strong     ; "legacy_bang_strong")]
    #[test_case(key(KeyCode::Char('$')),           ModelTier::Compaction ; "legacy_dollar_compaction")]
    #[test_case(key(KeyCode::Char('€')),           ModelTier::Compaction ; "legacy_euro_compaction")]
    #[test_case(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT), ModelTier::Strong     ; "kitty_shift_1_strong")]
    #[test_case(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::SHIFT), ModelTier::Compaction ; "kitty_shift_4_compaction")]
    fn tier_shortcut_assigns_and_keeps_picker_open(k: KeyEvent, want: ModelTier) {
        let mut p = ModelPicker::new(test_models());
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
        let mut p = ModelPicker::new(models.clone());
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

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
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
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
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Char('!')));

        p.try_refresh();

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
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");

        models.store(None);
        p.try_refresh();
        let entry = p.picker.selected_item().expect("selection during collapse");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(entry.section(), Some("Recent"));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

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
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        models.store(None);
        p.try_refresh();
        p.handle_key(key(KeyCode::Down));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

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
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_key(key(KeyCode::Char('g')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('m')));

        models.store(None);
        p.try_refresh();
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(entry.section(), Some("Z.AI"));
    }
}
