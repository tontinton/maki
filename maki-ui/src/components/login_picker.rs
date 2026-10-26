use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Wrap;

use maki_config::providers::{self, Protocol, ProviderDef, ProvidersConfig, slugify};
use maki_storage::auth::{ProviderCredentials, save_provider_credentials};
use maki_storage::model::persist_model;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::Modal;
use crate::components::provider_auth::{Auth, provider_entries};
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Login ";
const CATALOG_SECTION: &str = "Providers from Models.dev";
const CATALOG_UNAVAILABLE_SLUG: &str = "catalog-unavailable";

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
        name: String,
    },
    CustomUrl {
        input: TextBuffer,
        slug: String,
        name: String,
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
        name: String,
    },
    GoCustomUrl {
        slug: String,
        name: String,
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
    return_to_picker: bool,
}

impl LoginPicker {
    pub fn new() -> Self {
        Self {
            step: Step::Closed,
            provider_items: Vec::new(),
            storage: None,
            return_to_picker: false,
        }
    }

    /// Opens the wizard already advanced into `slug`'s flow, so a model
    /// picker row for a provider that cannot serve models lands straight on
    /// its setup. Backing out closes the wizard and returns to the picker.
    pub fn open_for(&mut self, storage: maki_storage::StateDir, slug: &str) {
        self.open(storage);
        let Some(item) = self.provider_items.iter().find(|i| i.slug == slug).cloned() else {
            return;
        };
        if let Step::PickProvider(picker) = &mut self.step {
            picker.select_item_by(|i| i.slug == slug);
        }
        self.return_to_picker = true;
        self.transition(choose_provider(&item, true));
    }

    pub fn open(&mut self, storage: maki_storage::StateDir) {
        let (entries, catalog_available) = provider_entries(&storage);
        let mut items: Vec<ProviderItem> = entries
            .into_iter()
            .map(|e| ProviderItem {
                slug: e.slug,
                display_name: e.display_name,
                has_key: e.auth == Auth::Saved,
                has_env: e.auth == Auth::Env,
                configured: e.configured,
                from_config: e.from_config,
                section: e.from_catalog.then_some(CATALOG_SECTION),
            })
            .collect();

        if !catalog_available {
            items.push(ProviderItem {
                slug: CATALOG_UNAVAILABLE_SLUG.to_string(),
                display_name: "Models.dev providers (not yet downloaded)".to_string(),
                has_key: false,
                has_env: false,
                configured: false,
                from_config: false,
                section: Some(CATALOG_SECTION),
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
        self.return_to_picker = false;
        self.step = Step::PickProvider(picker);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        match &mut self.step {
            Step::EnterKey { input, .. }
            | Step::CustomName { input }
            | Step::CustomUrl { input, .. }
            | Step::EditEndpoint { input, .. }
            | Step::BuiltinUrl { input, .. } => {
                input.insert_text(text);
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> LoginPickerAction {
        let action = match &mut self.step {
            Step::Closed => return LoginPickerAction::Consumed,
            Step::PickProvider(picker) => match picker.handle_key(key) {
                PickerAction::Select(item) => choose_provider(&item, false),
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
            Step::CustomName { input } => match key.code {
                KeyCode::Enter => {
                    let name = input.value().trim().to_string();
                    let slug = slugify(&name);
                    if slug.is_empty() {
                        return LoginPickerAction::Consumed;
                    }
                    StepAction::GoCustomProtocol { slug, name }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
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
                    let slug_c = slug.clone();
                    let name_c = name.clone();
                    let mut config = providers::ProvidersConfig::load();
                    let mut def = config.get(&slug_c).cloned().unwrap_or_default();
                    def.base_url = Some(base_url);
                    config.upsert(slug_c.clone(), def);
                    if let Err(e) = config.save() {
                        return self.transition(StepAction::GoDone {
                            message: format!("Error saving config: {e}"),
                            model_spec: None,
                            slug: slug_c,
                        });
                    }
                    StepAction::GoEnterKey {
                        slug: slug_c,
                        plan: None,
                        display_name: name_c,
                        custom: None,
                        builtin_url: None,
                        api_key_optional: *api_key_optional,
                    }
                }
                KeyCode::Esc => StepAction::Back,
                _ => {
                    input.handle_key(key);
                    return LoginPickerAction::Consumed;
                }
            },
            Step::CustomProtocol { picker, slug, name } => match picker.handle_key(key) {
                PickerAction::Select(item) => StepAction::GoCustomUrl {
                    slug: slug.clone(),
                    name: name.clone(),
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
                name,
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
                        display_name: name.clone(),
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
                        let api_key_env =
                            format!("{}_API_KEY", slug_c.to_uppercase().replace('-', "_"));
                        let provider_def = ProviderDef {
                            display_name: Some(dn_c.clone()),
                            protocol: Some(
                                info.protocol
                                    .parse::<Protocol>()
                                    .unwrap_or(Protocol::Openai),
                            ),
                            base_url: Some(info.base_url.clone()),
                            api_key_env: Some(api_key_env),
                            discover_models: true,
                            ..Default::default()
                        };
                        config.upsert(slug_c.clone(), provider_def);
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
            StepAction::GoCustomProtocol { slug, name } => {
                let items: Vec<ProtocolItem> = PROTOCOLS
                    .iter()
                    .map(|(name, display)| ProtocolItem(name, display))
                    .collect();
                let mut picker = ListPicker::new();
                picker.open(items, " Protocol ");
                self.step = Step::CustomProtocol { picker, slug, name };
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
            StepAction::GoCustomUrl {
                slug,
                name,
                protocol,
            } => {
                self.step = Step::CustomUrl {
                    input: TextBuffer::new(String::new()),
                    slug,
                    name,
                    protocol,
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
            StepAction::Back if self.return_to_picker => {
                self.return_to_picker = false;
                self.step = Step::Closed;
                LoginPickerAction::BackToPicker
            }
            StepAction::Close if self.return_to_picker => {
                self.return_to_picker = false;
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

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        match &mut self.step {
            Step::Closed => Rect::default(),
            Step::PickProvider(picker) => picker.view(frame, area),
            Step::PickPlan { picker, .. } => picker.view(frame, area),
            Step::CustomName { input } => {
                let modal = step_modal(" Provider name ");
                let (popup, inner) = modal.render(frame, area, 2);
                let t = theme::current();
                let hint = Span::styled("Enter provider name, then Enter", t.input_placeholder);
                let input_line = input_line_with_cursor(input);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(vec![Line::from(hint), input_line])
                        .style(Style::new().bg(t.background))
                        .wrap(Wrap { trim: true }),
                    inner,
                );
                popup
            }
            Step::CustomProtocol { picker, .. } => picker.view(frame, area),
            Step::EditEndpoint { input, .. } => {
                let modal = step_modal(" Endpoint ");
                let (popup, inner) = modal.render(frame, area, 2);
                let t = theme::current();
                let hint = Span::styled("Edit the base URL, then Enter", t.input_placeholder);
                let input_line = input_line_with_cursor(input);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(vec![Line::from(hint), input_line])
                        .style(Style::new().bg(t.background))
                        .wrap(Wrap { trim: true }),
                    inner,
                );
                popup
            }
            Step::CustomUrl { input, .. } => {
                let modal = step_modal(" Base URL ");
                let (popup, inner) = modal.render(frame, area, 2);
                let t = theme::current();
                let hint = Span::styled("Enter base URL, then Enter", t.input_placeholder);
                let input_line = input_line_with_cursor(input);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(vec![Line::from(hint), input_line])
                        .style(Style::new().bg(t.background))
                        .wrap(Wrap { trim: true }),
                    inner,
                );
                popup
            }
            Step::BuiltinUrl {
                input,
                display_name,
                ..
            } => {
                let title = format!(" {display_name} ");
                let modal = step_modal(&title);
                let (popup, inner) = modal.render(frame, area, 2);
                let t = theme::current();
                let hint = Span::styled("Edit host URL, or Enter to confirm", t.input_placeholder);
                let input_line = input_line_with_cursor(input);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(vec![Line::from(hint), input_line])
                        .style(Style::new().bg(t.background))
                        .wrap(Wrap { trim: true }),
                    inner,
                );
                popup
            }
            Step::EnterKey {
                input,
                display_name,
                api_key_optional,
                ..
            } => {
                let title = format!(" {display_name} ");
                let modal = step_modal(&title);
                let (popup, inner) = modal.render(frame, area, 2);
                let t = theme::current();
                let hint_text = if *api_key_optional {
                    "Paste API key, or Enter to skip"
                } else {
                    "Paste API key, then Enter"
                };
                let hint = Span::styled(hint_text, t.input_placeholder);
                let input_line = input_line_with_cursor(input);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(vec![Line::from(hint), input_line])
                        .style(Style::new().bg(t.background))
                        .wrap(Wrap { trim: true }),
                    inner,
                );
                popup
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
                    ratatui::widgets::Paragraph::new(Line::from(message.clone()))
                        .style(Style::new().bg(theme::current().background)),
                    inner,
                );
                popup
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
    }
}

/// The step that follows picking `item` in the provider list. Free-standing
/// so `open_for` can reuse it without the key-handling borrow alive.
fn choose_provider(item: &ProviderItem, from_picker: bool) -> StepAction {
    if item.slug == CATALOG_UNAVAILABLE_SLUG {
        return StepAction::None;
    }
    if item.slug == "custom" {
        return StepAction::GoCustomName;
    }
    let slug = item.slug.clone();
    let config = providers::ProvidersConfig::load();
    let def = config.get(&slug);
    let display_name = if providers::builtin_provider(&slug).is_some() || def.is_some() {
        providers::resolve_display_name(&slug, def)
    } else {
        item.display_name.clone()
    };
    // A declared provider entered from the model picker edits its endpoint
    // before pasting the key. An existing key or env var can stay, so the
    // prompt accepts an empty answer.
    if from_picker && item.from_config {
        return StepAction::GoEditEndpoint {
            slug,
            name: display_name,
            api_key_optional: item.has_key || item.has_env,
        };
    }
    let has_plans = providers::builtin_provider(&slug)
        .and_then(|b| b.plans)
        .is_some_and(|p| p.len() > 1);
    if has_plans {
        return StepAction::GoPickPlan { slug };
    }
    let needs_url = providers::builtin_provider(&slug).is_some_and(|b| b.needs_url);
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

fn step_modal(title: &str) -> Modal<'_> {
    Modal {
        title,
        width_percent: 65,
        min_width: 0,
        max_height_percent: 40,
    }
}

fn input_line_with_cursor(input: &TextBuffer) -> Line<'static> {
    let t = theme::current();
    let value = input.value();
    let cursor_byte = TextBuffer::char_to_byte(value, input.x());
    let (before, rest) = value.split_at(cursor_byte);
    let mut chars = rest.chars();
    let cursor_char = chars.next().unwrap_or(' ');
    let after = chars.as_str();
    Line::from(vec![
        super::chevron_span(),
        Span::raw(before.to_string()),
        Span::styled(cursor_char.to_string(), t.cursor),
        Span::raw(after.to_string()),
    ])
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
    use crossterm::event::KeyModifiers;

    use super::*;

    const DECLARED_SLUG: &str = "declared";

    fn declared_provider(has_env: bool) -> ProviderItem {
        ProviderItem {
            slug: DECLARED_SLUG.to_string(),
            display_name: "Declared".to_string(),
            has_key: false,
            has_env,
            configured: false,
            from_config: true,
            section: None,
        }
    }

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn endpoint_editor(api_key_optional: bool) -> LoginPicker {
        let mut input = TextBuffer::new("https://example.test/v1".to_string());
        input.move_end();
        let mut picker = LoginPicker::new();
        picker.step = Step::EditEndpoint {
            input,
            slug: DECLARED_SLUG.to_string(),
            name: "Declared".to_string(),
            api_key_optional,
        };
        picker
    }

    #[test]
    fn a_declared_provider_entered_from_the_picker_edits_its_endpoint_first() {
        let action = choose_provider(&declared_provider(true), true);
        assert!(
            matches!(
                &action,
                StepAction::GoEditEndpoint {
                    slug,
                    api_key_optional: true,
                    ..
                } if slug == DECLARED_SLUG
            ),
            "an existing key stays usable, so the key prompt accepts Enter"
        );

        let action = choose_provider(&declared_provider(false), false);
        assert!(
            matches!(
                &action,
                StepAction::GoEnterKey { slug, .. } if slug == DECLARED_SLUG
            ),
            "the /login list keeps its usual key prompt"
        );
    }

    #[test]
    fn backing_out_of_a_picker_flow_returns_to_the_model_picker() {
        let mut picker = endpoint_editor(false);
        picker.return_to_picker = true;

        assert!(matches!(
            picker.handle_key(esc()),
            LoginPickerAction::BackToPicker
        ));
        assert!(!picker.is_open());
        assert!(
            !picker.return_to_picker,
            "the flag does not leak to the next open"
        );
    }

    #[test]
    fn backing_out_of_the_login_list_closes_the_wizard() {
        let mut picker = LoginPicker::new();
        let mut list = ListPicker::new();
        list.open(Vec::<ProviderItem>::new(), TITLE);
        picker.step = Step::PickProvider(list);

        assert!(matches!(picker.handle_key(esc()), LoginPickerAction::Close));
        assert!(!picker.is_open());
    }

    #[test]
    fn pasting_into_the_endpoint_editor_reaches_the_url() {
        let mut picker = endpoint_editor(false);
        assert!(picker.handle_paste("https://other.test/v1"));

        let Step::EditEndpoint { input, .. } = &picker.step else {
            panic!("still editing the endpoint");
        };
        assert!(
            input.value().contains("https://other.test/v1"),
            "the paste lands in the endpoint field"
        );
    }
}
