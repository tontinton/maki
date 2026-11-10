use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use maki_config::providers::{self, Protocol, ProvidersConfig, slugify};
use maki_storage::auth::{ProviderCredentials, save_provider_credentials};
use maki_storage::model::persist_model;

use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::Modal;
use crate::components::provider_auth::{Auth, provider_entries};
use crate::components::{Overlay, chevron_span, field_line, painted_caret};
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Login ";
const CATALOG_UNAVAILABLE_SLUG: &str = "catalog-unavailable";
const WRAP: Wrap = Wrap { trim: true };

const PROTOCOLS: &[(&str, &str)] = &[
    ("openai", "OpenAI-compatible"),
    ("anthropic", "Anthropic"),
    ("google", "Google Gemini"),
];

#[derive(Clone)]
struct ProviderItem {
    slug: String,
    display_name: String,
    has_key: bool,
    has_env: bool,
    configured: bool,
    /// Explicitly declared in `providers.toml`: the user asked for this
    /// provider, so it lists even when its key is missing.
    from_config: bool,
    section: Option<&'static str>,
}

impl PickerItem for ProviderItem {
    fn label(&self) -> &str {
        &self.display_name
    }

    fn section(&self) -> Option<&str> {
        self.section
    }

    fn detail(&self) -> Option<&str> {
        if self.has_key {
            Some("saved")
        } else if self.has_env {
            Some("env")
        } else if self.configured {
            Some("configured")
        } else {
            None
        }
    }
}

struct PlanItem {
    key: String,
    display_name: String,
    base_url: String,
}

impl PickerItem for PlanItem {
    fn label(&self) -> &str {
        &self.display_name
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.base_url)
    }
}

struct ProtocolItem(&'static str, &'static str);

impl PickerItem for ProtocolItem {
    fn label(&self) -> &str {
        self.1
    }
}

struct CustomInfo {
    base_url: String,
    protocol: String,
}

enum Step {
    Closed,
    PickProvider(ListPicker<ProviderItem>),
    PickPlan {
        picker: ListPicker<PlanItem>,
        slug: String,
    },
    CustomName {
        input: TextBuffer,
    },
    CustomProtocol {
        picker: ListPicker<ProtocolItem>,
        slug: String,
    },
    CustomUrl {
        input: TextBuffer,
        slug: String,
        protocol: String,
    },
    EditEndpoint {
        input: TextBuffer,
        slug: String,
        name: String,
        api_key_optional: bool,
    },
    BuiltinUrl {
        input: TextBuffer,
        slug: String,
        display_name: String,
    },
    EnterKey {
        input: TextBuffer,
        slug: String,
        plan: Option<String>,
        display_name: String,
        custom: Option<CustomInfo>,
        builtin_url: Option<String>,
        api_key_optional: bool,
    },
    Done {
        message: String,
    },
}

enum StepAction {
    None,
    GoEnterKey {
        slug: String,
        plan: Option<String>,
        display_name: String,
        custom: Option<CustomInfo>,
        builtin_url: Option<String>,
        api_key_optional: bool,
    },
    GoPickPlan {
        slug: String,
    },
    GoCustomName,
    GoCustomProtocol {
        slug: String,
    },
    GoCustomUrl {
        slug: String,
        protocol: String,
    },
    GoEditEndpoint {
        slug: String,
        name: String,
        api_key_optional: bool,
    },
    GoBuiltinUrl {
        slug: String,
        display_name: String,
    },
    GoDone {
        message: String,
        model_spec: Option<String>,
        slug: String,
    },
    Back,
    Close,
}

pub struct LoginPicker {
    step: Step,
    provider_items: Vec<ProviderItem>,
    storage: Option<maki_storage::StateDir>,
    /// Opened from the model picker: backing out returns there.
    from_picker: bool,
}

impl LoginPicker {
    pub fn new() -> Self {
        Self {
            step: Step::Closed,
            provider_items: Vec::new(),
            storage: None,
            from_picker: false,
        }
    }

    /// Opens the wizard already advanced into `slug`'s flow, so a model
    /// picker row for a provider that cannot serve models lands straight on
    /// its setup. Backing out closes the wizard and returns to the picker.
    pub fn open_for(&mut self, storage: maki_storage::StateDir, slug: &str) {
        self.open(storage);
        if !self.provider_items.iter().any(|i| i.slug == slug) {
            return;
        }
        if let Step::PickProvider(picker) = &mut self.step {
            picker.select_item_by(|i| i.slug == slug);
        }
        self.from_picker = true;
        self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    pub fn open(&mut self, storage: maki_storage::StateDir) {
        self.from_picker = false;
        let (entries, catalog_ready) = provider_entries(&storage);
        let mut items: Vec<ProviderItem> = entries
            .iter()
            .map(|e| ProviderItem {
                slug: e.slug.clone(),
                display_name: e.display_name.clone(),
                has_key: e.auth == Auth::Saved,
                has_env: e.auth == Auth::Env,
                configured: e.configured,
                from_config: e.from_config,
                section: None,
            })
            .collect();

        if !catalog_ready {
            items.push(ProviderItem {
                slug: CATALOG_UNAVAILABLE_SLUG.to_string(),
                display_name: "Models.dev providers (not yet downloaded)".to_string(),
                has_key: false,
                has_env: false,
                configured: false,
                from_config: false,
                section: Some("Providers from Models.dev"),
            });
        }

        items.push(ProviderItem {
            slug: "custom".to_string(),
            display_name: "Custom provider...".to_string(),
            has_key: false,
            has_env: false,
            configured: false,
            from_config: false,
            section: None,
        });

        self.provider_items = items.clone();
        let mut picker = ListPicker::new();
        picker.open(items, TITLE);
        self.storage = Some(storage);
        self.step = Step::PickProvider(picker);
    }

    /// A list step filters on what is pasted, as typing does. A step with no
    /// field drops it.
    pub fn handle_paste(&mut self, text: &str) {
        match &mut self.step {
            Step::PickProvider(picker) => picker.handle_paste(text),
            Step::PickPlan { picker, .. } => picker.handle_paste(text),
            Step::CustomProtocol { picker, .. } => picker.handle_paste(text),
            Step::EnterKey { input, .. }
            | Step::CustomName { input }
            | Step::CustomUrl { input, .. }
            | Step::EditEndpoint { input, .. }
            | Step::BuiltinUrl { input, .. } => input.insert_text(text),
            Step::Closed | Step::Done { .. } => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> LoginPickerAction {
        let action = match &mut self.step {
            Step::Closed => return LoginPickerAction::Consumed,
            Step::PickProvider(picker) => match picker.handle_key(key) {
                PickerAction::Select(item) => {
                    if item.slug == CATALOG_UNAVAILABLE_SLUG {
                        StepAction::None
                    } else if item.slug == "custom" {
                        StepAction::GoCustomName
                    } else {
                        let slug = item.slug.clone();
                        let config = providers::ProvidersConfig::load();
                        let def = config.get(&slug);
                        let display_name =
                            if providers::builtin_provider(&slug).is_some() || def.is_some() {
                                providers::resolve_display_name(&slug, def)
                            } else {
                                item.display_name.clone()
                            };
                        // A declared provider entered from the model picker
                        // edits its endpoint before pasting the key. An
                        // existing key or env var can stay, so the prompt
                        // accepts an empty answer.
                        if self.from_picker && item.from_config {
                            return self.transition(StepAction::GoEditEndpoint {
                                slug,
                                name: display_name,
                                api_key_optional: item.has_key || item.has_env,
                            });
                        }
                        let has_plans = providers::builtin_provider(&slug)
                            .and_then(|b| b.plans)
                            .is_some_and(|p| p.len() > 1);
                        if has_plans {
                            StepAction::GoPickPlan { slug }
                        } else {
                            let needs_url =
                                providers::builtin_provider(&slug).is_some_and(|b| b.needs_url);
                            if needs_url {
                                StepAction::GoBuiltinUrl { slug, display_name }
                            } else {
                                StepAction::GoEnterKey {
                                    slug,
                                    plan: None,
                                    display_name,
                                    custom: None,
                                    builtin_url: None,
                                    api_key_optional: false,
                                }
                            }
                        }
                    }
                }
                PickerAction::Close => StepAction::Close,
                PickerAction::Consumed | PickerAction::Toggle(..) => {
                    return LoginPickerAction::Consumed;
                }
            },
            Step::PickPlan { picker, slug } => match picker.handle_key(key) {
                PickerAction::Select(item) => {
                    let config = providers::ProvidersConfig::load();
                    StepAction::GoEnterKey {
                        slug: slug.clone(),
                        plan: Some(item.key.clone()),
                        display_name: providers::resolve_display_name(slug, config.get(slug)),
                        custom: None,
                        builtin_url: None,
                        api_key_optional: false,
                    }
                }
                PickerAction::Close => StepAction::Back,
                PickerAction::Consumed | PickerAction::Toggle(..) => {
                    return LoginPickerAction::Consumed;
                }
            },
            Step::EditEndpoint {
                input,
                slug,
                name,
                api_key_optional,
            } => match key.code {
                KeyCode::Enter => {
                    let base_url = input.value().trim().to_string();
                    if base_url.is_empty() {
                        return LoginPickerAction::Consumed;
                    }
                    // The entry already exists in providers.toml; only its
                    // base_url changes here, so hand the key step no custom
                    // info and let it update the existing entry.
                    StepAction::GoEnterKey {
                        slug: slug.clone(),
                        plan: None,
                        display_name: name.clone(),
                        custom: None,
                        builtin_url: Some(base_url),
                        api_key_optional: *api_key_optional,
                    }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
                    return LoginPickerAction::Consumed;
                }
            },
            Step::CustomName { input } => match key.code {
                KeyCode::Enter => {
                    let name = input.value().trim().to_string();
                    let slug = slugify(&name);
                    if slug.is_empty() {
                        return LoginPickerAction::Consumed;
                    }
                    StepAction::GoCustomProtocol { slug }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
                    return LoginPickerAction::Consumed;
                }
            },
            Step::CustomProtocol { picker, slug } => match picker.handle_key(key) {
                PickerAction::Select(item) => StepAction::GoCustomUrl {
                    slug: slug.clone(),
                    protocol: item.0.to_string(),
                },
                PickerAction::Close => StepAction::Back,
                PickerAction::Consumed | PickerAction::Toggle(..) => {
                    return LoginPickerAction::Consumed;
                }
            },
            Step::CustomUrl {
                input,
                slug,
                protocol,
            } => match key.code {
                KeyCode::Enter => {
                    let base_url = input.value().trim().to_string();
                    if base_url.is_empty() {
                        return LoginPickerAction::Consumed;
                    }
                    StepAction::GoEnterKey {
                        slug: slug.clone(),
                        plan: None,
                        display_name: format!("Custom ({})", slug),
                        custom: Some(CustomInfo {
                            base_url,
                            protocol: protocol.clone(),
                        }),
                        builtin_url: None,
                        api_key_optional: false,
                    }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
                    return LoginPickerAction::Consumed;
                }
            },
            Step::EnterKey {
                input,
                slug,
                plan,
                display_name,
                custom,
                builtin_url,
                api_key_optional,
            } => match key.code {
                KeyCode::Enter => {
                    let api_key = input.value().trim().to_string();
                    if api_key.is_empty() && !*api_key_optional {
                        return LoginPickerAction::Consumed;
                    }
                    let Some(storage) = self.storage.as_ref() else {
                        self.step = Step::Closed;
                        return LoginPickerAction::Close;
                    };
                    let slug_c = slug.clone();
                    let plan_c = plan.clone();
                    let dn_c = display_name.clone();
                    let storage = storage.clone();
                    let custom_c = custom.take();
                    let builtin_url_c = builtin_url.take();

                    let has_key = !api_key.is_empty();
                    if has_key {
                        let creds = ProviderCredentials {
                            api_key,
                            host: None,
                        };
                        if let Err(e) = save_provider_credentials(&storage, &slug_c, &creds) {
                            return self.transition(StepAction::GoDone {
                                message: format!("Error: {e}"),
                                model_spec: None,
                                slug: slug_c.clone(),
                            });
                        }
                    }

                    let mut config = ProvidersConfig::load();

                    if let Some(info) = &custom_c {
                        // Load the existing entry and set only the fields the
                        // setup flow owns, so protocol, models, headers and
                        // api_key survive an endpoint edit.
                        let mut def = config.get(&slug_c).cloned().unwrap_or_default();
                        if def.display_name.is_none() {
                            def.display_name = Some(dn_c.clone());
                        }
                        def.protocol = Some(
                            info.protocol
                                .parse::<Protocol>()
                                .unwrap_or(Protocol::Openai),
                        );
                        def.base_url = Some(info.base_url.clone());
                        if def.api_key_env.is_none() {
                            def.api_key_env = Some(format!(
                                "{}_API_KEY",
                                slug_c.to_uppercase().replace('-', "_")
                            ));
                        }
                        config.upsert(slug_c.clone(), def);
                        if let Err(e) = config.save() {
                            return self.transition(StepAction::GoDone {
                                message: format!("Error saving config: {e}"),
                                model_spec: None,
                                slug: slug_c.clone(),
                            });
                        }
                    } else {
                        let needs_url =
                            providers::builtin_provider(&slug_c).is_some_and(|b| b.needs_url);
                        if plan_c.is_some() || builtin_url_c.is_some() || needs_url {
                            let mut def = config.get(&slug_c).cloned().unwrap_or_default();
                            def.plan = plan_c.clone();
                            if let Some(url) = builtin_url_c {
                                def.base_url = Some(url);
                            }
                            config.upsert(slug_c.clone(), def);
                            if let Err(e) = config.save() {
                                return self.transition(StepAction::GoDone {
                                    message: format!("Error saving config: {e}"),
                                    model_spec: None,
                                    slug: slug_c.clone(),
                                });
                            }
                        }
                    }

                    let needs_url =
                        providers::builtin_provider(&slug_c).is_some_and(|b| b.needs_url);
                    let default_model = if needs_url {
                        None
                    } else {
                        providers::resolve_default_model(&slug_c, config.get(&slug_c))
                    };
                    if let Some(model) = &default_model {
                        persist_model(&storage, model);
                    }

                    let verb = if has_key {
                        "Authenticated"
                    } else {
                        "Configured"
                    };
                    let msg = format!(
                        "{}: {}{}",
                        verb,
                        dn_c,
                        plan_c
                            .as_deref()
                            .map(|p| format!(" ({p})"))
                            .unwrap_or_default()
                    );
                    StepAction::GoDone {
                        message: msg,
                        model_spec: default_model,
                        slug: slug_c.clone(),
                    }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
                    return LoginPickerAction::Consumed;
                }
            },
            Step::BuiltinUrl {
                input,
                slug,
                display_name,
            } => match key.code {
                KeyCode::Enter => {
                    let mut base_url = input.value().trim().to_string();
                    if base_url.is_empty() {
                        base_url = providers::resolve_base_url(slug, None).unwrap_or_default();
                    }
                    StepAction::GoEnterKey {
                        slug: slug.clone(),
                        plan: None,
                        display_name: display_name.clone(),
                        custom: None,
                        builtin_url: Some(base_url),
                        api_key_optional: true,
                    }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
                    return LoginPickerAction::Consumed;
                }
            },
            Step::Done { .. } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    StepAction::Close
                } else {
                    StepAction::None
                }
            }
        };

        self.transition(action)
    }

    fn transition(&mut self, action: StepAction) -> LoginPickerAction {
        match action {
            StepAction::None => LoginPickerAction::Consumed,
            StepAction::GoEnterKey {
                slug,
                plan,
                display_name,
                custom,
                builtin_url,
                api_key_optional,
            } => {
                if custom.is_none()
                    && let Some(url) = providers::resolve_login_url(&slug, plan.as_deref())
                    && let Err(e) = open::that(&url)
                {
                    tracing::warn!(error = %e, url, "failed to open browser");
                }
                self.step = Step::EnterKey {
                    input: TextBuffer::new(String::new()),
                    slug,
                    plan,
                    display_name,
                    custom,
                    builtin_url,
                    api_key_optional,
                };
                LoginPickerAction::Consumed
            }
            StepAction::GoBuiltinUrl { slug, display_name } => {
                let config = providers::ProvidersConfig::load();
                let default =
                    providers::resolve_base_url(&slug, config.get(&slug)).unwrap_or_default();
                self.step = Step::BuiltinUrl {
                    input: TextBuffer::new(default),
                    slug,
                    display_name,
                };
                LoginPickerAction::Consumed
            }
            StepAction::GoPickPlan { slug } => {
                let builtin = providers::builtin_provider(&slug);
                let plans = builtin.and_then(|b| b.plans).unwrap_or(&[]);
                let plan_items: Vec<PlanItem> = plans
                    .iter()
                    .map(|(key, plan)| PlanItem {
                        key: key.to_string(),
                        display_name: plan.display_name.to_string(),
                        base_url: plan.base_url.to_string(),
                    })
                    .collect();
                let display = providers::resolve_display_name(&slug, None);
                let mut plan_picker = ListPicker::new();
                plan_picker.open(plan_items, format!(" {display} plan "));
                self.step = Step::PickPlan {
                    picker: plan_picker,
                    slug,
                };
                LoginPickerAction::Consumed
            }
            StepAction::GoCustomName => {
                self.step = Step::CustomName {
                    input: TextBuffer::new(String::new()),
                };
                LoginPickerAction::Consumed
            }
            StepAction::GoCustomProtocol { slug } => {
                let items: Vec<ProtocolItem> = PROTOCOLS
                    .iter()
                    .map(|(name, display)| ProtocolItem(name, display))
                    .collect();
                let mut picker = ListPicker::new();
                picker.open(items, " Protocol ");
                self.step = Step::CustomProtocol { picker, slug };
                LoginPickerAction::Consumed
            }
            StepAction::GoCustomUrl { slug, protocol } => {
                self.step = Step::CustomUrl {
                    input: TextBuffer::new(String::new()),
                    slug,
                    protocol,
                };
                LoginPickerAction::Consumed
            }
            StepAction::GoEditEndpoint {
                slug,
                name,
                api_key_optional,
            } => {
                let url = providers::ProvidersConfig::load()
                    .get(&slug)
                    .and_then(|d| d.base_url.clone())
                    .unwrap_or_default();
                let mut input = TextBuffer::new(url);
                input.move_end();
                self.step = Step::EditEndpoint {
                    input,
                    slug,
                    name,
                    api_key_optional,
                };
                LoginPickerAction::Consumed
            }
            StepAction::GoDone {
                message,
                model_spec,
                slug,
            } => {
                self.step = Step::Done { message };
                if let Some(spec) = model_spec {
                    LoginPickerAction::Authenticated { model_spec: spec }
                } else {
                    LoginPickerAction::Configured { slug }
                }
            }
            StepAction::Back if self.from_picker => {
                self.from_picker = false;
                self.step = Step::Closed;
                LoginPickerAction::BackToPicker
            }
            StepAction::Close if self.from_picker => {
                self.from_picker = false;
                self.step = Step::Closed;
                LoginPickerAction::BackToPicker
            }
            StepAction::Back => {
                let mut picker = ListPicker::new();
                picker.open(self.provider_items.clone(), TITLE);
                self.step = Step::PickProvider(picker);
                LoginPickerAction::Consumed
            }
            StepAction::Close => {
                self.step = Step::Closed;
                LoginPickerAction::Close
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> (Rect, Option<Position>) {
        match &mut self.step {
            Step::Closed => (Rect::default(), None),
            Step::PickProvider(picker) => picker.view(frame, area),
            Step::PickPlan { picker, .. } => picker.view(frame, area),
            Step::CustomName { input } => input_step_view(
                frame,
                area,
                " Provider name ",
                "Enter provider name, then Enter",
                input,
            ),
            Step::CustomProtocol { picker, .. } => picker.view(frame, area),
            Step::EditEndpoint { input, .. } => input_step_view(
                frame,
                area,
                " Endpoint ",
                "Edit the base URL, then Enter",
                input,
            ),
            Step::CustomUrl { input, .. } => input_step_view(
                frame,
                area,
                " Base URL ",
                "Enter base URL, then Enter",
                input,
            ),
            Step::BuiltinUrl {
                input,
                display_name,
                ..
            } => {
                let title = format!(" {display_name} ");
                input_step_view(
                    frame,
                    area,
                    &title,
                    "Edit host URL, or Enter to confirm",
                    input,
                )
            }
            Step::EnterKey {
                input,
                display_name,
                api_key_optional,
                ..
            } => {
                let title = format!(" {display_name} ");
                let hint = if *api_key_optional {
                    "Paste API key, or Enter to skip"
                } else {
                    "Paste API key, then Enter"
                };
                input_step_view(frame, area, &title, hint, input)
            }
            Step::Done { message } => {
                let modal = Modal {
                    title: " Login ",
                    width_percent: 50,
                    min_width: 0,
                    max_height_percent: 30,
                };
                let (popup, inner) = modal.render(frame, area, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(message.clone()))
                        .style(Style::new().bg(theme::current().background)),
                    inner,
                );
                (popup, None)
            }
        }
    }
}

impl Overlay for LoginPicker {
    fn is_prompting(&self) -> bool {
        false
    }

    fn is_open(&self) -> bool {
        !matches!(self.step, Step::Closed)
    }

    fn close(&mut self) {
        self.step = Step::Closed;
        self.from_picker = false;
    }
}

fn step_modal(title: &str) -> Modal<'_> {
    Modal {
        title,
        width_percent: 65,
        min_width: 0,
        max_height_percent: 40,
    }
}

fn input_step_view(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    hint: &'static str,
    input: &TextBuffer,
) -> (Rect, Option<Position>) {
    let (popup, inner) = step_modal(title).render(frame, area, 2);
    let t = theme::current();
    let hint_rows = Paragraph::new(hint).wrap(WRAP).line_count(inner.width);
    let (input_line, caret) = field_line(
        vec![chevron_span()],
        input.value(),
        input.cursor_byte(),
        t.cursor,
        Style::default(),
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(hint, t.input_placeholder)),
            input_line,
        ])
        .style(Style::new().bg(t.background))
        .wrap(WRAP),
        inner,
    );
    (
        popup,
        painted_caret(frame.buffer_mut(), inner, &caret, hint_rows),
    )
}

pub enum LoginPickerAction {
    Consumed,
    Close,
    /// A model-picker-originated flow was backed out of; the host reopens
    /// the picker instead of staying on the /login provider list.
    BackToPicker,
    Authenticated {
        model_spec: String,
    },
    Configured {
        slug: String,
    },
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use maki_storage::StateDir;

    const FIRST_LABEL: &str = "Alpha";
    const SECOND_LABEL: &str = "Beta";
    const PASTED_FILTER: &str = "bet";
    const PLAN_BASE_URL: &str = "https://plan.example.test";
    const SLUG: &str = "slug";
    const WRONG_SELECTION: &str = "the paste did not filter the list";
    const WRONG_STEP: &str = "the step is not the one built";

    fn opened<T: PickerItem>(items: Vec<T>) -> ListPicker<T> {
        let mut picker = ListPicker::new();
        picker.open(items, TITLE);
        picker
    }

    fn selected_label<T: PickerItem>(picker: &ListPicker<T>) -> Option<String> {
        picker.selected_item().map(|item| item.label().to_owned())
    }

    fn provider_item(label: &str) -> ProviderItem {
        ProviderItem {
            slug: label.to_lowercase(),
            display_name: label.to_owned(),
            has_key: false,
            has_env: false,
            configured: false,
            from_config: false,
            section: None,
        }
    }

    fn plan_item(label: &str) -> PlanItem {
        PlanItem {
            key: label.to_lowercase(),
            display_name: label.to_owned(),
            base_url: PLAN_BASE_URL.to_owned(),
        }
    }

    fn picker_at(step: Step) -> LoginPicker {
        LoginPicker {
            step,
            ..LoginPicker::new()
        }
    }

    fn provider_step() -> Step {
        Step::PickProvider(opened(vec![
            provider_item(FIRST_LABEL),
            provider_item(SECOND_LABEL),
        ]))
    }

    fn plan_step() -> Step {
        Step::PickPlan {
            picker: opened(vec![plan_item(FIRST_LABEL), plan_item(SECOND_LABEL)]),
            slug: SLUG.to_owned(),
        }
    }

    fn protocol_step() -> Step {
        Step::CustomProtocol {
            picker: opened(vec![
                ProtocolItem(SLUG, FIRST_LABEL),
                ProtocolItem(SLUG, SECOND_LABEL),
            ]),
            slug: SLUG.to_owned(),
        }
    }

    #[test_case(provider_step ; "provider_list")]
    #[test_case(plan_step ; "plan_list")]
    #[test_case(protocol_step ; "protocol_list")]
    fn paste_filters_a_list_step(step: fn() -> Step) {
        let mut picker = picker_at(step());
        picker.handle_paste(PASTED_FILTER);

        let selected = match &picker.step {
            Step::PickProvider(list) => selected_label(list),
            Step::PickPlan { picker, .. } => selected_label(picker),
            Step::CustomProtocol { picker, .. } => selected_label(picker),
            _ => panic!("{WRONG_STEP}"),
        };
        assert_eq!(selected.as_deref(), Some(SECOND_LABEL), "{WRONG_SELECTION}");
    }

    const HOME_VARS: &[&str] = &[
        "HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ];

    const SETUP_SLUG: &str = "declared-gateway";
    const OLD_URL: &str = "https://old.example/v1";
    const NEW_URL: &str = "https://new.example/v1";
    const PRESERVED_ENV: &str = "DECLARED_GATEWAY_API_KEY";
    const PRESERVED_PROTOCOL: &str = "anthropic";

    /// Points every base directory at a throwaway tree, so neither the real
    /// providers.toml nor this machine's credentials reach the setup flow.
    fn isolated_home() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("no temporary home");
        for var in HOME_VARS {
            // SAFETY: nextest runs one test per process, so no other thread
            // reads the environment while this test holds it.
            unsafe { std::env::set_var(var, dir.path()) };
        }
        dir
    }

    /// A declared provider keeps protocol, api_key_env, models and headers
    /// when the model picker's setup flow edits only its endpoint.
    #[test]
    fn setup_from_picker_updates_only_the_endpoint_of_an_existing_entry() {
        let _home = isolated_home();
        let config_dir = maki_storage::paths::config_dir().expect("config dir");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("providers.toml"),
            format!(
                "[{SETUP_SLUG}]\nprotocol = \"{PRESERVED_PROTOCOL}\"\nbase_url = \"{OLD_URL}\"\napi_key_env = \"{PRESERVED_ENV}\"\ndiscover_models = true\n\n[[{SETUP_SLUG}.models]]\nid = \"kept-model\"\ntier = \"strong\"\n"
            ),
        )
        .expect("seed providers.toml");

        let storage = StateDir::from_path(std::env::temp_dir().join("maki-login-test-state"));
        let mut picker = LoginPicker::new();
        picker.storage = Some(storage);
        picker.step = Step::EditEndpoint {
            input: TextBuffer::new(NEW_URL.to_owned()),
            slug: SETUP_SLUG.to_owned(),
            name: "Declared Gateway".to_owned(),
            api_key_optional: true,
        };
        picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // The endpoint edit hands the key step no custom info...
        let Step::EnterKey {
            custom,
            builtin_url,
            ..
        } = &picker.step
        else {
            panic!("the endpoint edit did not advance to the key step");
        };
        assert!(
            custom.is_none(),
            "an existing entry is updated, not rewritten"
        );
        assert_eq!(builtin_url.as_deref(), Some(NEW_URL));
        // ...and the key step writes only base_url onto the loaded entry.
        picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let saved = ProvidersConfig::load();
        let def = saved.get(SETUP_SLUG).expect("the entry survives");
        assert_eq!(def.base_url.as_deref(), Some(NEW_URL));
        assert_eq!(
            def.protocol,
            Some(Protocol::Anthropic),
            "the protocol is preserved"
        );
        assert_eq!(def.api_key_env.as_deref(), Some(PRESERVED_ENV));
        assert_eq!(def.models.len(), 1, "the declared models are preserved");
        assert!(def.discover_models, "discover_models is preserved");
    }

    /// Opening or closing the wizard clears the from-picker flag, so a
    /// cancelled setup cannot send the next `/login` back into `/model`.
    #[test]
    fn open_and_close_clear_the_from_picker_flag() {
        let _home = isolated_home();
        let mut picker = LoginPicker::new();
        picker.from_picker = true;
        let storage = StateDir::from_path(std::env::temp_dir().join("maki-login-test-state"));
        picker.open(storage.clone());
        assert!(!picker.from_picker, "open resets the flag");
        picker.from_picker = true;
        picker.close();
        assert!(!picker.from_picker, "close resets the flag");
    }
}
