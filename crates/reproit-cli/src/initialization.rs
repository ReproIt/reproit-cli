use std::collections::BTreeSet;

use reproit_core::{
    Error, ErrorCode,
    identity::{OrganizationId, ProjectId, ServiceId},
    model::ProcessingMode,
};

const MAX_SERVICES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializationService {
    pub organization_id: OrganizationId,
    pub processing_mode: ProcessingMode,
    pub project_id: ProjectId,
    pub qualified_name: String,
    pub repository_id: String,
    pub service_id: ServiceId,
}

pub struct InitializationDirectory {
    pub services: Vec<InitializationService>,
}

impl InitializationDirectory {
    pub fn new(
        repository_id: &str,
        mut services: Vec<InitializationService>,
    ) -> Result<Self, Error> {
        services.sort_by(|left, right| left.qualified_name.cmp(&right.qualified_name));
        let directory = Self { services };
        directory.validate(repository_id)?;
        Ok(directory)
    }

    pub fn select(&self, qualified_name: Option<&str>) -> Result<InitializationService, Error> {
        match qualified_name {
            Some(name) => self
                .services
                .iter()
                .find(|service| service.qualified_name == name)
                .cloned()
                .ok_or_else(configuration_invalid),
            None if self.services.len() == 1 => Ok(self.services[0].clone()),
            None => Err(Error::new(
                ErrorCode::ConfigConflict,
                "Initialization requires one explicit service selection.",
            )),
        }
    }

    fn validate(&self, repository_id: &str) -> Result<(), Error> {
        if self.services.is_empty() || self.services.len() > MAX_SERVICES {
            return Err(configuration_invalid());
        }
        let mut names = BTreeSet::new();
        let mut service_ids = BTreeSet::new();
        for service in &self.services {
            if service.processing_mode != ProcessingMode::Managed
                || service.repository_id != repository_id
                || !valid_qualified_name(&service.qualified_name, &service.service_id)
                || !names.insert(&service.qualified_name)
                || !service_ids.insert(service.service_id.to_string())
            {
                return Err(configuration_invalid());
            }
        }
        if !self
            .services
            .windows(2)
            .all(|pair| pair[0].qualified_name < pair[1].qualified_name)
        {
            return Err(configuration_invalid());
        }
        Ok(())
    }
}

fn valid_qualified_name(value: &str, service_id: &ServiceId) -> bool {
    let Some((path, suffix)) = value.split_once('@') else {
        return false;
    };
    value.len() <= 320
        && !suffix.contains('@')
        && suffix == service_id.to_string()
        && path.split('/').count() == 3
        && path.split('/').all(|part| {
            !part.is_empty()
                && part.len() <= 80
                && part.bytes().enumerate().all(|(index, byte)| {
                    byte.is_ascii_lowercase()
                        || (byte.is_ascii_digit() && index > 0)
                        || (byte == b'-' && index > 0)
                })
        })
}

fn configuration_invalid() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "The available Cloud service catalog is invalid.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_deterministic_and_requires_a_name_when_ambiguous() {
        let first = InitializationService {
            organization_id: "org_01890f3e-7b1c-7cc0-8a1b-123456789abd".parse().unwrap(),
            processing_mode: ProcessingMode::Managed,
            project_id: "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe".parse().unwrap(),
            qualified_name: concat!(
                "acme/commerce/orders@",
                "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf"
            )
            .to_owned(),
            repository_id: "git.example/acme/commerce".to_owned(),
            service_id: "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf".parse().unwrap(),
        };
        let mut second = first.clone();
        second.qualified_name = concat!(
            "acme/commerce/payments@",
            "svc_01890f3e-7b1c-7cc0-8a1b-123456789ac0"
        )
        .to_owned();
        second.service_id = "svc_01890f3e-7b1c-7cc0-8a1b-123456789ac0".parse().unwrap();
        let directory =
            InitializationDirectory::new("git.example/acme/commerce", vec![second, first]).unwrap();
        assert!(directory.select(None).is_err());
        assert_eq!(
            directory
                .select(Some(concat!(
                    "acme/commerce/orders@",
                    "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf"
                )))
                .unwrap()
                .qualified_name,
            concat!(
                "acme/commerce/orders@",
                "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf"
            )
        );
    }

    #[test]
    fn directory_rejects_private_and_cross_repository_services() {
        let service = InitializationService {
            organization_id: "org_01890f3e-7b1c-7cc0-8a1b-123456789abd".parse().unwrap(),
            processing_mode: ProcessingMode::Managed,
            project_id: "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe".parse().unwrap(),
            qualified_name: concat!(
                "acme/commerce/orders@",
                "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf"
            )
            .to_owned(),
            repository_id: "git.example/acme/commerce".to_owned(),
            service_id: "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf".parse().unwrap(),
        };

        let mut private = service.clone();
        private.processing_mode = ProcessingMode::Private;
        assert!(InitializationDirectory::new("git.example/acme/commerce", vec![private]).is_err());

        let mut other_repository = service;
        other_repository.repository_id = "git.example/acme/other".to_owned();
        assert!(
            InitializationDirectory::new("git.example/acme/commerce", vec![other_repository],)
                .is_err()
        );
    }

    #[test]
    fn directory_rejects_a_qualified_name_with_the_wrong_service_id() {
        let service = InitializationService {
            organization_id: "org_01890f3e-7b1c-7cc0-8a1b-123456789abd".parse().unwrap(),
            processing_mode: ProcessingMode::Managed,
            project_id: "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe".parse().unwrap(),
            qualified_name: concat!(
                "acme/commerce/orders@",
                "svc_01890f3e-7b1c-7cc0-8a1b-123456789ac0"
            )
            .to_owned(),
            repository_id: "git.example/acme/commerce".to_owned(),
            service_id: "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf".parse().unwrap(),
        };
        assert!(InitializationDirectory::new("git.example/acme/commerce", vec![service]).is_err());
    }
}
