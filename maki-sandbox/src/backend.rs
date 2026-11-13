use crate::bwrap;
use crate::error::SandboxError;
use crate::namespace;

/// How the sandbox isolates the child process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Use the native namespace sandbox, falling back to bubblewrap when the
    /// host refuses it and `bwrap` is installed.
    #[default]
    Auto,
    /// Create the namespaces directly with `unshare`.
    Native,
    /// Let bubblewrap create the namespaces and filesystem.
    Bwrap,
}

pub const ALL: [Backend; 3] = [Backend::Auto, Backend::Native, Backend::Bwrap];

impl Backend {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Native => "native",
            Self::Bwrap => "bwrap",
        }
    }

    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        ALL.into_iter().find(|backend| backend.label() == label)
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Auto => Self::Native,
            Self::Native => Self::Bwrap,
            Self::Bwrap => Self::Auto,
        }
    }

    /// Check that this backend can isolate on the current host.
    ///
    /// `Auto` only falls back to bubblewrap when it is installed, so a host
    /// without either backend reports why the native one failed.
    ///
    /// # Errors
    ///
    /// Returns a [`SandboxError`] describing the first backend that cannot
    /// run.
    pub fn probe(self) -> Result<(), SandboxError> {
        match self {
            Self::Native => namespace::probe(),
            Self::Bwrap => bwrap::probe(),
            Self::Auto => match namespace::probe() {
                Ok(()) => Ok(()),
                Err(native) if bwrap::installed() => bwrap::probe().map_err(|bwrap| {
                    SandboxError::IsolationUnavailable(format!(
                        "native backend unavailable: {native}\nbwrap backend unavailable: {bwrap}"
                    ))
                }),
                Err(native) => Err(native),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_label_roundtrips() {
        for backend in ALL {
            assert_eq!(Backend::from_label(backend.label()), Some(backend));
        }
    }

    #[test]
    fn next_cycles_through_every_backend() {
        assert_eq!(Backend::Auto.next(), Backend::Native);
        assert_eq!(Backend::Native.next(), Backend::Bwrap);
        assert_eq!(Backend::Bwrap.next(), Backend::Auto);
    }

    #[test]
    fn unknown_label_has_no_backend() {
        assert_eq!(Backend::from_label("firejail"), None);
    }
}
