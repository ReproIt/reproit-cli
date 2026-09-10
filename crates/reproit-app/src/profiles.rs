use reproit_core::{Error, ErrorCode};

pub const AUTHORED_REPRO_CAPABILITY: &str = "authored-repro";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct InstalledProfile {
    pub capabilities: &'static [&'static str],
    pub profile: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ProfileCapabilityRegistry {
    profiles: &'static [InstalledProfile],
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
const INSTALLED_PROFILES: [InstalledProfile; 1] = [InstalledProfile {
    capabilities: &[AUTHORED_REPRO_CAPABILITY],
    profile: "experiments",
}];

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const INSTALLED_PROFILES: [InstalledProfile; 0] = [];

impl ProfileCapabilityRegistry {
    pub const fn installed() -> Self {
        Self {
            profiles: &INSTALLED_PROFILES,
        }
    }

    pub const fn empty() -> Self {
        Self { profiles: &[] }
    }

    #[must_use]
    pub fn has_capability(&self, capability: &str) -> bool {
        self.profiles
            .iter()
            .any(|profile| profile.capabilities.contains(&capability))
    }

    pub fn require(&self, profile: &str, capability: &str) -> Result<(), Error> {
        if self.profiles.iter().any(|installed| {
            installed.profile == profile && installed.capabilities.contains(&capability)
        }) {
            return Ok(());
        }
        Err(Error::new(
            ErrorCode::UnsupportedCapabilitySet,
            "The installed profile does not authorize authored Repros.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    fn only_experiments_declares_authored_repro() {
        let registry = ProfileCapabilityRegistry::installed();
        assert!(registry.has_capability(AUTHORED_REPRO_CAPABILITY));
        registry
            .require("experiments", AUTHORED_REPRO_CAPABILITY)
            .unwrap();
        for profile in ["backend", "games", "ml", "os", "security", "ui"] {
            assert!(
                registry
                    .require(profile, AUTHORED_REPRO_CAPABILITY)
                    .is_err()
            );
        }
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    fn an_unsupported_platform_declares_no_authored_repro() {
        let registry = ProfileCapabilityRegistry::installed();
        assert!(!registry.has_capability(AUTHORED_REPRO_CAPABILITY));
        assert!(
            registry
                .require("experiments", AUTHORED_REPRO_CAPABILITY)
                .is_err()
        );
    }

    #[test]
    fn an_empty_installation_has_no_authored_surface() {
        assert!(!ProfileCapabilityRegistry::empty().has_capability(AUTHORED_REPRO_CAPABILITY));
    }
}
