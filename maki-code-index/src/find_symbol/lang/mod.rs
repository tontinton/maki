use super::language::RefLanguage;
use crate::Language;

#[cfg(feature = "lang-c")]
pub(crate) mod c;
mod c_family;
#[cfg(feature = "lang-cpp")]
pub(crate) mod cpp;
#[cfg(feature = "lang-rust")]
pub(crate) mod rust;

pub fn ref_language_for(lang: Language) -> Option<&'static dyn RefLanguage> {
    match lang {
        #[cfg(feature = "lang-rust")]
        Language::Rust => Some(&*rust::RUST_LANGUAGE),
        #[cfg(feature = "lang-c")]
        Language::C => Some(&*c::C_LANGUAGE),
        #[cfg(feature = "lang-cpp")]
        Language::Cpp => Some(&*cpp::CPP_LANGUAGE),
        _ => None,
    }
}
