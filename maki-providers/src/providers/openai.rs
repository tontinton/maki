use std::borrow::Cow;

pub(crate) mod api;
pub(crate) mod auth_state;
pub(crate) mod plan;

pub(crate) use api::OpenAi;
pub(crate) use plan::OpenAiCodingPlan;

pub(crate) fn effective_system<'a>(
    system_prefix: &'a Option<String>,
    system: &'a str,
) -> Cow<'a, str> {
    if let Some(prefix) = system_prefix {
        Cow::Owned(format!("{prefix}\n\n{system}"))
    } else {
        Cow::Borrowed(system)
    }
}
