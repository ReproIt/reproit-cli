#![forbid(unsafe_code)]

pub mod agent;
pub mod cloud;
pub mod executor_control;
pub mod initialization;
mod login;
pub mod mcp;
pub mod render;
mod source;
mod source_package;
pub use login::{
    AuthorizationResult, LoginAttempt, NativeCredentialStore, discover_oauth_metadata,
    exchange_authorization_code,
};
pub use source::{
    GitCheckout, GitRepositoryIdentity, GitSourceWorkspace, ManagedSource, SourceCheckout,
    checkout_git, current_git_repository, current_project_source,
};

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
};

use reproit_app::{KeptReferenceStore, ProjectStore};
use reproit_backend::config::ProjectConfig;
use reproit_core::{
    Error, ErrorCode,
    identity::ReproId,
    model::{KeptReference, Validate},
};

const MAX_REFERENCE_BYTES: u64 = 65_536;

pub struct FilesystemRepository {
    root: PathBuf,
}

impl FilesystemRepository {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn read_project(&self) -> Result<ProjectConfig, Error> {
        let path = self.config_root().join("project.toml");
        let metadata = fs::symlink_metadata(&path).map_err(config_read_error)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_REFERENCE_BYTES {
            return Err(config_invalid());
        }
        let text = fs::read_to_string(path).map_err(config_read_error)?;
        let config: ProjectConfig = toml::from_str(&text).map_err(|_| config_invalid())?;
        config.validate()?;
        Ok(config)
    }

    fn config_root(&self) -> PathBuf {
        self.root.join(".reproit")
    }

    fn references_root(&self) -> PathBuf {
        self.config_root().join("repros")
    }

    fn read_reference(path: &Path) -> Result<KeptReference, Error> {
        let metadata = fs::symlink_metadata(path).map_err(config_read_error)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_REFERENCE_BYTES {
            return Err(config_invalid());
        }
        let text = fs::read_to_string(path).map_err(config_read_error)?;
        let reference: KeptReference = toml::from_str(&text).map_err(|_| config_invalid())?;
        reference.validate()?;
        Ok(reference)
    }

    fn reference_paths(&self, limit: usize) -> Result<Vec<PathBuf>, Error> {
        let root = self.references_root();
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(config_invalid()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(config_read_error(error)),
        }
        let mut paths = fs::read_dir(root)
            .map_err(config_read_error)?
            .take(limit)
            .map(|entry| entry.map(|entry| entry.path()).map_err(config_read_error))
            .collect::<Result<Vec<_>, _>>()?;
        if paths
            .iter()
            .any(|path| path.extension().is_none_or(|extension| extension != "toml"))
        {
            return Err(config_invalid());
        }
        paths.sort();
        Ok(paths)
    }
}

impl KeptReferenceStore for FilesystemRepository {
    fn list_kept(&self, limit: usize) -> Result<Vec<KeptReference>, Error> {
        self.reference_paths(limit)?
            .iter()
            .map(|path| Self::read_reference(path))
            .collect()
    }

    fn remove_kept(&mut self, repro_id: ReproId) -> Result<bool, Error> {
        let matches = self
            .reference_paths(reproit_app::MAX_KEPT_REFERENCES + 1)?
            .into_iter()
            .map(|path| Ok((Self::read_reference(&path)?, path)))
            .filter_map(|result: Result<_, Error>| match result {
                Ok((reference, path)) if reference.repro_id == repro_id => Some(Ok(path)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        match matches.as_slice() {
            [] => Ok(false),
            [path] => {
                fs::remove_file(path).map_err(config_write_error)?;
                Ok(true)
            }
            _ => Err(Error::new(
                ErrorCode::ConfigConflict,
                "More than one kept reference has the same Repro ID.",
            )),
        }
    }

    fn write_kept(&mut self, reference: &KeptReference) -> Result<(), Error> {
        reference.validate()?;
        let root = self.references_root();
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&root).map_err(config_read_error)?;
                if !metadata.file_type().is_dir() {
                    return Err(config_invalid());
                }
            }
            Err(error) => return Err(config_write_error(error)),
        }
        let path = root.join(format!("{}.toml", reference.repro_id));
        match fs::symlink_metadata(&path) {
            Ok(_) if Self::read_reference(&path)? == *reference => Ok(()),
            Ok(_) => Err(Error::new(
                ErrorCode::ConfigConflict,
                "The kept Repro reference already has different content.",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_new_config(&path, reference)
            }
            Err(error) => Err(config_read_error(error)),
        }
    }
}

impl ProjectStore<ProjectConfig> for FilesystemRepository {
    fn is_initialized(&self) -> Result<bool, Error> {
        match fs::symlink_metadata(self.config_root().join("project.toml")) {
            Ok(metadata) => Ok(metadata.file_type().is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(config_read_error(error)),
        }
    }

    fn read_project(&self) -> Result<Option<ProjectConfig>, Error> {
        if !self.is_initialized()? {
            return Ok(None);
        }
        FilesystemRepository::read_project(self).map(Some)
    }

    fn write_project(&mut self, config: &ProjectConfig) -> Result<(), Error> {
        config.validate()?;
        let root = self.config_root();
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&root).map_err(config_read_error)?;
                if !metadata.file_type().is_dir() {
                    return Err(config_invalid());
                }
            }
            Err(error) => return Err(config_write_error(error)),
        }
        let path = root.join("project.toml");
        let result = write_project_config(&path, config);
        if result.is_err() && !path.exists() {
            let _ = fs::remove_dir(&root);
        }
        result
    }
}

fn write_project_config(path: &Path, config: &ProjectConfig) -> Result<(), Error> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err(config_invalid());
    }
    let text = toml::to_string_pretty(config).map_err(|_| config_invalid())?;
    let mut temporary = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(config_write_error_without_source)?,
    )
    .map_err(config_write_error)?;
    temporary
        .write_all(text.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(config_write_error)?;
    temporary
        .persist(path)
        .map_err(|error| config_write_error(error.error))?;
    Ok(())
}

fn write_new_config(path: &Path, config: &impl serde::Serialize) -> Result<(), Error> {
    let text = toml::to_string_pretty(config).map_err(|_| config_invalid())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(config_write_error)?;
    file.write_all(text.as_bytes())
        .map_err(config_write_error)?;
    file.sync_all().map_err(config_write_error)
}

fn config_invalid() -> Error {
    Error::new(
        ErrorCode::SchemaInvalid,
        "The repository configuration is invalid.",
    )
}

fn config_read_error(_error: std::io::Error) -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not read the repository configuration.",
    )
}

fn config_write_error(_error: std::io::Error) -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not update the repository configuration.",
    )
}

fn config_write_error_without_source() -> Error {
    config_write_error(std::io::Error::other("missing configuration parent"))
}
