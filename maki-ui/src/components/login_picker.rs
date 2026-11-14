use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use maki_config::providers::{self, Protocol, ProviderDef, ProvidersConfig, slugify};
use maki_providers::catalog_providers_if_available;
use maki_storage::StateDir;
use maki_storage::auth::{
    ProviderCredentials, load_provider_credentials, load_tokens, save_provider_credentials,
};
use maki_storage::model::persist_model;

use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::Modal;
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
    /// Explicitly declared in `providers.toml`: Enter edits its endpoint
    /// before asking for the key.
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
        /// The endpoint was written to providers.toml, so the running
        /// provider still points at the old URL until it is rebuilt.
        url_changed: bool,
    },
    Back,
    Close,
}

pub struct LoginPicker {
    step: Step,
    provider_items: Vec<ProviderItem>,
    storage: Option<maki_storage::StateDir>,
}

impl LoginPicker {
    pub fn new() -> Self {
        Self {
            step: Step::Closed,
            provider_items: Vec::new(),
            storage: None,
        }
    }

    pub fn open(&mut self, storage: maki_storage::StateDir) {
        let builtins = providers::all_builtins();
        let config = providers::ProvidersConfig::load();
        let mut items: Vec<ProviderItem> = builtins
            .iter()
            .map(|b| {
                let has_key = load_provider_credentials(&storage, b.slug).is_some()
                    || load_tokens(&storage, b.slug).is_some();
                let has_env = std::env::var(b.default_api_key_env).is_ok();
                let configured = !has_key
                    && !has_env
                    && config.get(b.slug).is_some_and(|d| d.base_url.is_some());
                ProviderItem {
                    slug: b.slug.to_string(),
                    display_name: b.display_name.to_string(),
                    has_key,
                    has_env,
                    configured,
                    from_config: false,
                    section: None,
                }
            })
            .collect();

        for (slug, def) in &config.providers {
            if slug == "opencode" || providers::builtin_provider(slug).is_some() {
                continue;
            }
            let has_key =
                load_provider_credentials(&storage, slug).is_some() || def.api_key.is_some();
            let has_env = def
                .api_key_env
                .as_deref()
                .is_some_and(|e| std::env::var(e).is_ok());
            items.push(ProviderItem {
                slug: slug.clone(),
                display_name: def.display_name.clone().unwrap_or_else(|| slug.clone()),
                has_key,
                has_env,
                configured: false,
                from_config: true,
                section: None,
            });
        }

        if let Some(catalog) = catalog_providers_if_available() {
            let state_dir = StateDir::resolve().ok();
            for cat in catalog {
                let has_key = state_dir
                    .as_ref()
                    .and_then(|s| cat.load_key_from_storage(s))
                    .is_some();
                let has_env = cat.env_key_set().is_some();
                items.push(ProviderItem {
                    slug: cat.slug.clone(),
                    display_name: cat.display_name.clone(),
                    has_key,
                    has_env,
                    configured: false,
                    from_config: false,
                    section: Some("Providers from Models.dev"),
                });
            }
        } else {
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

    /// Opens on `slug`'s own setup, as Enter on its row would.
    pub fn open_for(&mut self, storage: StateDir, slug: &str) -> LoginPickerAction {
        self.open(storage);
        if let Step::PickProvider(picker) = &mut self.step
            && picker.select_item_by(|item| item.slug == slug)
        {
            self.handle_key(KeyEvent::from(KeyCode::Enter))
        } else {
            LoginPickerAction::Consumed
        }
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
                    } else if item.from_config {
                        let slug = item.slug.clone();
                        let config = providers::ProvidersConfig::load();
                        let display_name =
                            providers::resolve_display_name(&item.slug, config.get(&item.slug));
                        return self.transition(StepAction::GoEditEndpoint {
                            slug,
                            name: display_name,
                            api_key_optional: item.has_key || item.has_env,
                        });
                    } else {
                        let slug = item.slug.clone();
                        let config = providers::ProvidersConfig::load();
                        let def = config.get(&slug);
                        let has_plans = providers::builtin_provider(&slug)
                            .and_then(|b| b.plans)
                            .is_some_and(|p| p.len() > 1);
                        if has_plans {
                            StepAction::GoPickPlan { slug }
                        } else {
                            let display_name =
                                if providers::builtin_provider(&slug).is_some() || def.is_some() {
                                    providers::resolve_display_name(&slug, def)
                                } else {
                                    item.display_name.clone()
                                };
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
                    // An empty field keeps the endpoint the entry already has
                    // (or the protocol default), and an unchanged URL is not
                    // handed on: the key step then leaves providers.toml
                    // alone, comments and key order included.
                    let saved = providers::ProvidersConfig::load()
                        .get(slug)
                        .and_then(|d| d.base_url.clone());
                    let builtin_url = if base_url.is_empty() || Some(&base_url) == saved.as_ref() {
                        None
                    } else {
                        Some(base_url)
                    };
                    // The entry already exists in providers.toml; only its
                    // base_url changes here, so hand the key step no custom
                    // info and let it update the existing entry.
                    StepAction::GoEnterKey {
                        slug: slug.clone(),
                        plan: None,
                        display_name: name.clone(),
                        custom: None,
                        builtin_url,
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
                    let url_changed = builtin_url_c.is_some();

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
                                url_changed: false,
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
                                url_changed: false,
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
                                    url_changed: false,
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
                        url_changed,
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
                let login_url = if custom.is_none() {
                    providers::resolve_login_url(&slug, plan.as_deref())
                } else {
                    None
                };
                self.step = Step::EnterKey {
                    input: TextBuffer::new(String::new()),
                    slug,
                    plan,
                    display_name,
                    custom,
                    builtin_url,
                    api_key_optional,
                };
                match login_url {
                    Some(url) => LoginPickerAction::OpenUrl(url),
                    None => LoginPickerAction::Consumed,
                }
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
            StepAction::GoDone {
                message,
                model_spec,
                slug,
                url_changed,
            } => {
                self.step = Step::Done { message };
                if let Some(spec) = model_spec {
                    LoginPickerAction::Authenticated {
                        model_spec: spec,
                        slug,
                        url_changed,
                    }
                } else {
                    LoginPickerAction::Configured { slug }
                }
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
            Step::CustomUrl { input, .. } => input_step_view(
                frame,
                area,
                " Base URL ",
                "Enter base URL, then Enter",
                input,
            ),
            Step::EditEndpoint { input, .. } => input_step_view(
                frame,
                area,
                " Base URL ",
                "Edit base URL, then Enter",
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

#[derive(Debug)]
pub enum LoginPickerAction {
    Consumed,
    Close,
    /// The provider's keys page should open in the browser.
    OpenUrl(String),
    Authenticated {
        model_spec: String,
        slug: String,
        /// The endpoint was written, so the provider has to be rebuilt
        /// before a model switch reuses the cached one.
        url_changed: bool,
    },
    Configured {
        slug: String,
    },
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use maki_providers::test_support::isolated_home;
    use test_case::test_case;

    use super::*;

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

    const SETUP_SLUG: &str = "declared-gateway";
    const OLD_URL: &str = "https://old.example/v1";
    const NEW_URL: &str = "https://new.example/v1";
    const PRESERVED_ENV: &str = "DECLARED_GATEWAY_API_KEY";
    const PRESERVED_PROTOCOL: &str = "anthropic";

    /// A declared provider keeps protocol, api_key_env, models and headers
    /// when the setup flow edits only its endpoint, and a written URL asks
    /// for a provider rebuild so the model switch does not reuse the cached
    /// one still pointing at the old address.
    #[test]
    fn setup_edits_only_the_endpoint_of_an_existing_entry() {
        let _home = isolated_home();
        let config_dir = maki_storage::paths::config_dir().expect("config dir");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("providers.toml"),
            format!(
                "[{SETUP_SLUG}]\nprotocol = \"{PRESERVED_PROTOCOL}\"\nbase_url = \"{OLD_URL}\"\napi_key_env = \"{PRESERVED_ENV}\"\ndefault_model = \"kept-model\"\ndiscover_models = true\n\n[[{SETUP_SLUG}.models]]\nid = \"kept-model\"\ntier = \"strong\"\n"
            ),
        )
        .expect("seed providers.toml");

        let storage = StateDir::from_path(_home.path().to_path_buf());
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
        let action = picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let LoginPickerAction::Authenticated {
            model_spec,
            slug,
            url_changed,
        } = &action
        else {
            panic!("a default model ends the setup authenticated, got {action:?}");
        };
        assert_eq!(model_spec, "kept-model");
        assert_eq!(slug, SETUP_SLUG);
        assert!(url_changed, "a written URL must ask for a provider rebuild");

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

    /// An entry that leaves `base_url` to the protocol default or a
    /// `<SLUG>_BASE_URL` env must still be set up: an empty endpoint field
    /// keeps the current endpoint instead of swallowing Enter, and the key
    /// step then leaves providers.toml byte-identical.
    #[test]
    fn empty_endpoint_field_keeps_the_current_endpoint() {
        let _home = isolated_home();
        let config_dir = maki_storage::paths::config_dir().expect("config dir");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let seeded = format!(
            "# keep this comment\n[{SETUP_SLUG}]\nprotocol = \"openai\"\napi_key_env = \"{PRESERVED_ENV}\"\n"
        );
        std::fs::write(config_dir.join("providers.toml"), &seeded).expect("seed providers.toml");

        let storage = StateDir::from_path(_home.path().to_path_buf());
        let mut picker = LoginPicker::new();
        picker.storage = Some(storage);
        picker.step = Step::EditEndpoint {
            input: TextBuffer::new(String::new()),
            slug: SETUP_SLUG.to_owned(),
            name: "Declared Gateway".to_owned(),
            api_key_optional: true,
        };
        picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let Step::EnterKey { builtin_url, .. } = &picker.step else {
            panic!("an empty endpoint must still advance to the key step");
        };
        assert!(
            builtin_url.is_none(),
            "nothing changed, no URL is handed on"
        );
        let action = picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(action, LoginPickerAction::Configured { .. }),
            "no URL change must not end in a model switch, got {action:?}"
        );

        let after = std::fs::read_to_string(config_dir.join("providers.toml")).unwrap();
        assert_eq!(after, seeded, "providers.toml must stay byte-identical");
    }

    /// Re-entering the endpoint that is already saved is not a change, so it
    /// must not rewrite providers.toml either.
    #[test]
    fn unchanged_endpoint_is_not_written_back() {
        let _home = isolated_home();
        let config_dir = maki_storage::paths::config_dir().expect("config dir");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let seeded = format!(
            "# keep this comment\n[{SETUP_SLUG}]\nprotocol = \"openai\"\nbase_url = \"{OLD_URL}\"\n"
        );
        std::fs::write(config_dir.join("providers.toml"), &seeded).expect("seed providers.toml");

        let storage = StateDir::from_path(_home.path().to_path_buf());
        let mut picker = LoginPicker::new();
        picker.storage = Some(storage);
        picker.step = Step::EditEndpoint {
            input: TextBuffer::new(OLD_URL.to_owned()),
            slug: SETUP_SLUG.to_owned(),
            name: "Declared Gateway".to_owned(),
            api_key_optional: true,
        };
        picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let Step::EnterKey { builtin_url, .. } = &picker.step else {
            panic!("the endpoint step must advance to the key step");
        };
        assert!(builtin_url.is_none(), "the saved URL is not a change");
        let action = picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(action, LoginPickerAction::Configured { .. }),
            "no URL change must not end in a model switch, got {action:?}"
        );

        let after = std::fs::read_to_string(config_dir.join("providers.toml")).unwrap();
        assert_eq!(after, seeded, "providers.toml must stay byte-identical");
    }

    /// An inline `api_key` in providers.toml counts as a key, so the
    /// setup flow accepts an empty answer instead of forcing a re-paste.
    /// No storage credential exists: only the file's `api_key` can satisfy
    /// the check, so the test fails if the fix is reverted.
    #[test]
    fn inline_api_key_counts_as_saved_auth() {
        let _home = isolated_home();
        let config_dir = maki_storage::paths::config_dir().expect("config dir");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("providers.toml"),
            format!("[{SETUP_SLUG}]\nprotocol = \"openai\"\nbase_url = \"{OLD_URL}\"\napi_key = \"inline-secret\"\n"),
        )
        .expect("seed providers.toml");

        let storage = StateDir::from_path(_home.path().to_path_buf());
        assert!(
            load_provider_credentials(&storage, SETUP_SLUG).is_none(),
            "no storage credential exists for the slug"
        );
        let mut picker = LoginPicker::new();
        picker.open(storage);
        let item = picker
            .provider_items
            .iter()
            .find(|i| i.slug == SETUP_SLUG)
            .expect("listed");
        assert!(item.has_key, "an inline api_key counts as a saved key");
    }
}
