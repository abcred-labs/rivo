use crate::{
    LanguageCatalog, LanguageDiagnostic, LanguageError, LanguageMetadata, LanguageResource,
    LocaleId, RemoteLanguagePackage, RemoteLanguageReceipt, EMBEDDED_EN_US, EMBEDDED_ZH_CN,
    MAX_LANGUAGE_FILE_BYTES,
};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageFileSource {
    User,
    Bundled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledLanguage {
    pub locale: Option<LocaleId>,
    pub display_name: Option<String>,
    pub revision: Option<String>,
    pub source: LanguageFileSource,
    pub path: Option<PathBuf>,
    pub diagnostics: Vec<LanguageDiagnostic>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageDiscovery {
    languages: Vec<InstalledLanguage>,
}

impl LanguageDiscovery {
    pub fn languages(&self) -> &[InstalledLanguage] {
        &self.languages
    }

    pub fn find(&self, locale: &LocaleId) -> Option<&InstalledLanguage> {
        self.languages
            .iter()
            .rev()
            .find(|language| language.locale.as_ref() == Some(locale) && language.error.is_none())
    }

    fn into_languages(self) -> Vec<InstalledLanguage> {
        self.languages
    }
}

#[derive(Debug, Clone)]
pub struct LocalLanguageManager {
    user_dir: PathBuf,
    bundled_dir: Option<PathBuf>,
}

pub struct ReloadableLanguageCatalog {
    discovery: LanguageDiscovery,
    catalog: LanguageCatalog,
}

impl ReloadableLanguageCatalog {
    pub fn load(
        manager: &LocalLanguageManager,
        locale: &LocaleId,
    ) -> Result<Self, LocalLanguageError> {
        let discovery = manager.discover()?;
        Ok(Self {
            catalog: manager.load_catalog(locale)?,
            discovery,
        })
    }

    pub fn discovery(&self) -> &LanguageDiscovery {
        &self.discovery
    }

    pub fn catalog(&self) -> &LanguageCatalog {
        &self.catalog
    }

    pub fn reload(
        &mut self,
        manager: &LocalLanguageManager,
        locale: &LocaleId,
    ) -> Result<(), LocalLanguageError> {
        let discovery = manager.discover()?;
        let candidate = manager.load_catalog(locale)?;
        self.catalog = candidate;
        self.discovery = discovery;
        Ok(())
    }
}

impl LocalLanguageManager {
    pub fn from_environment() -> Result<Self, LocalLanguageError> {
        if let Some(path) = std::env::var_os("RIVO_LANGUAGE_DIR").filter(|value| !value.is_empty())
        {
            return Ok(Self::new(PathBuf::from(path)));
        }
        let data_root = std::env::var_os("RIVO_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::data_local_dir)
            .or_else(dirs::data_dir)
            .ok_or(LocalLanguageError::DataDirectoryUnavailable)?;
        Ok(Self::from_data_root(data_root))
    }

    pub fn from_data_root(data_root: impl Into<PathBuf>) -> Self {
        Self::new(data_root.into().join("rivo").join("languages"))
    }

    pub fn new(user_dir: impl Into<PathBuf>) -> Self {
        Self {
            user_dir: user_dir.into(),
            bundled_dir: installed_languages_dir(),
        }
    }

    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    pub fn language_path(&self, locale: &LocaleId) -> PathBuf {
        self.user_dir.join(format!("{locale}.ftl"))
    }

    pub fn receipt_path(&self, locale: &LocaleId) -> PathBuf {
        self.user_dir.join(format!("{locale}.receipt.json"))
    }

    pub fn load_catalog(&self, locale: &LocaleId) -> Result<LanguageCatalog, LocalLanguageError> {
        let english = canonical_resource()?;
        let schema = english.message_schema().clone();
        let mut resources = Vec::new();
        let user_path = self.language_path(locale);
        if user_path.exists() {
            let bytes = read_language_file(&user_path)?;
            resources.push(LanguageResource::parse(&bytes, locale, Some(&schema))?);
        }
        if let Some(directory) = &self.bundled_dir {
            let bundled_path = directory.join(format!("{locale}.ftl"));
            if bundled_path.is_file() {
                let bytes = read_language_file(&bundled_path)?;
                resources.push(LanguageResource::parse(&bytes, locale, Some(&schema))?);
            }
        }
        match locale.to_string().as_str() {
            "en-US" => {}
            "zh-CN" if resources.is_empty() => resources.push(LanguageResource::parse(
                EMBEDDED_ZH_CN.as_bytes(),
                locale,
                Some(&schema),
            )?),
            _ if resources.is_empty() => {
                return Err(LocalLanguageError::NotInstalled(locale.clone()))
            }
            _ => {}
        }
        resources.push(english);
        Ok(LanguageCatalog::from_resources(
            locale.clone(),
            &schema,
            resources,
        )?)
    }

    pub fn validate_file(&self, path: &Path) -> Result<InstalledLanguage, LocalLanguageError> {
        let locale = locale_from_path(path)?;
        let bytes = read_language_file(path)?;
        let canonical = canonical_resource()?;
        let resource = LanguageResource::parse(&bytes, &locale, Some(canonical.message_schema()))?;
        Ok(installed_from_resource(
            &resource,
            LanguageFileSource::User,
            Some(path.to_path_buf()),
        ))
    }

    pub fn inspect_install(&self, path: &Path) -> Result<InstalledLanguage, LocalLanguageError> {
        let bytes = read_language_file(path)?;
        let canonical = canonical_resource()?;
        let resource = LanguageResource::parse_detected(&bytes, Some(canonical.message_schema()))?;
        Ok(installed_from_resource(
            &resource,
            LanguageFileSource::User,
            Some(self.language_path(&resource.metadata().locale)),
        ))
    }

    pub fn install(
        &self,
        source: &Path,
        replace: bool,
    ) -> Result<InstalledLanguage, LocalLanguageError> {
        let bytes = read_language_file(source)?;
        self.install_bytes(&bytes, replace)
    }

    pub fn install_bytes(
        &self,
        bytes: &[u8],
        replace: bool,
    ) -> Result<InstalledLanguage, LocalLanguageError> {
        let canonical = canonical_resource()?;
        let resource = LanguageResource::parse_detected(bytes, Some(canonical.message_schema()))?;
        let destination = self.language_path(&resource.metadata().locale);
        if destination.exists() && !replace {
            return Err(LocalLanguageError::AlreadyExists(destination));
        }

        fs::create_dir_all(&self.user_dir).map_err(|source| {
            LocalLanguageError::CreateDirectory {
                path: self.user_dir.clone(),
                source,
            }
        })?;
        if destination.exists() {
            let backup = backup_path(&destination);
            fs::copy(&destination, &backup).map_err(|source| LocalLanguageError::Backup {
                path: destination.clone(),
                backup,
                source,
            })?;
        }
        atomic_write(&destination, bytes, replace)?;
        Ok(installed_from_resource(
            &resource,
            LanguageFileSource::User,
            Some(destination),
        ))
    }

    pub fn install_remote(
        &self,
        package: &RemoteLanguagePackage,
        receipt: &RemoteLanguageReceipt,
        replace: bool,
    ) -> Result<InstalledLanguage, LocalLanguageError> {
        if package.entry().locale != receipt.locale_id()? {
            return Err(LocalLanguageError::ReceiptLocaleMismatch);
        }
        let locale = package.entry().locale.clone();
        let destination = self.language_path(&locale);
        let receipt_path = self.receipt_path(&locale);
        let previous_language = read_optional_regular_file(&destination)?;
        let previous_receipt = read_optional_regular_file(&receipt_path)?;
        let receipt_bytes = receipt.to_bytes()?;

        let installed = self.install_bytes(package.bytes(), replace)?;
        if let Err(error) = atomic_write(&receipt_path, &receipt_bytes, true) {
            restore_optional_file(&destination, previous_language.as_deref())?;
            restore_optional_file(&receipt_path, previous_receipt.as_deref())?;
            return Err(error);
        }
        Ok(installed)
    }

    pub fn receipt(&self, locale: &LocaleId) -> Result<RemoteLanguageReceipt, LocalLanguageError> {
        let path = self.receipt_path(locale);
        let bytes = read_language_file_bytes(&path, MAX_LANGUAGE_FILE_BYTES)?;
        let receipt = RemoteLanguageReceipt::parse(&bytes)?;
        if receipt.locale_id()? != *locale {
            return Err(LocalLanguageError::ReceiptLocaleMismatch);
        }
        Ok(receipt)
    }

    pub fn rollback(&self, locale: &LocaleId) -> Result<InstalledLanguage, LocalLanguageError> {
        let destination = self.language_path(locale);
        let backup = backup_path(&destination);
        let bytes = read_language_file_bytes(&backup, MAX_LANGUAGE_FILE_BYTES)?;
        let canonical = canonical_resource()?;
        LanguageResource::parse(&bytes, locale, Some(canonical.message_schema()))?;
        let receipt_path = self.receipt_path(locale);
        let previous_receipt = read_optional_regular_file(&receipt_path)?;
        if previous_receipt.is_some() {
            fs::remove_file(&receipt_path).map_err(|source| LocalLanguageError::Remove {
                path: receipt_path.clone(),
                source,
            })?;
        }
        match self.install_bytes(&bytes, true) {
            Ok(installed) => Ok(installed),
            Err(error) => {
                restore_optional_file(&receipt_path, previous_receipt.as_deref())?;
                Err(error)
            }
        }
    }

    pub fn remove(&self, locale: &LocaleId) -> Result<PathBuf, LocalLanguageError> {
        let path = self.language_path(locale);
        fs::remove_file(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                LocalLanguageError::NotInstalled(locale.clone())
            } else {
                LocalLanguageError::Remove {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        Ok(path)
    }

    pub fn discover(&self) -> Result<LanguageDiscovery, LocalLanguageError> {
        let canonical = canonical_resource()?;
        let chinese = LanguageResource::parse(
            EMBEDDED_ZH_CN.as_bytes(),
            &LocaleId::parse("zh-CN")?,
            Some(canonical.message_schema()),
        )?;
        let mut languages = vec![
            installed_from_resource(&canonical, LanguageFileSource::Bundled, None),
            installed_from_resource(&chinese, LanguageFileSource::Bundled, None),
        ];
        if !self.user_dir.exists() {
            return Ok(LanguageDiscovery { languages });
        }

        let entries =
            fs::read_dir(&self.user_dir).map_err(|source| LocalLanguageError::ReadDirectory {
                path: self.user_dir.clone(),
                source,
            })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| LocalLanguageError::ReadDirectory {
                path: self.user_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension() == Some(OsStr::new("ftl")) {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            match self.validate_file(&path) {
                Ok(language) => languages.push(language),
                Err(error) => languages.push(InstalledLanguage {
                    locale: path
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .and_then(|value| LocaleId::parse(value).ok()),
                    display_name: None,
                    revision: None,
                    source: LanguageFileSource::User,
                    path: Some(path),
                    diagnostics: Vec::new(),
                    error: Some(error.to_string()),
                }),
            }
        }
        Ok(LanguageDiscovery { languages })
    }

    pub fn list(&self) -> Result<Vec<InstalledLanguage>, LocalLanguageError> {
        Ok(self.discover()?.into_languages())
    }

    pub fn show(&self, locale: &LocaleId) -> Result<InstalledLanguage, LocalLanguageError> {
        let user_path = self.language_path(locale);
        if user_path.exists() {
            return self.validate_file(&user_path);
        }
        let canonical = canonical_resource()?;
        match locale.to_string().as_str() {
            "en-US" => Ok(installed_from_resource(
                &canonical,
                LanguageFileSource::Bundled,
                None,
            )),
            "zh-CN" => {
                let resource = LanguageResource::parse(
                    EMBEDDED_ZH_CN.as_bytes(),
                    locale,
                    Some(canonical.message_schema()),
                )?;
                Ok(installed_from_resource(
                    &resource,
                    LanguageFileSource::Bundled,
                    None,
                ))
            }
            _ => Err(LocalLanguageError::NotInstalled(locale.clone())),
        }
    }
}

fn installed_languages_dir() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable
        .parent()?
        .join("share")
        .join("rivo")
        .join("languages");
    directory.is_dir().then_some(directory)
}

#[derive(Debug, Error)]
pub enum LocalLanguageError {
    #[error(transparent)]
    Language(#[from] LanguageError),
    #[error(transparent)]
    Remote(#[from] crate::RemoteLanguageError),
    #[error("cannot determine the Rivo data directory")]
    DataDirectoryUnavailable,
    #[error("language file name must be a canonical BCP 47 locale followed by .ftl: {0}")]
    InvalidFileName(PathBuf),
    #[error("language file is {actual} bytes; maximum is {maximum}: {path}")]
    FileTooLarge {
        path: PathBuf,
        actual: u64,
        maximum: usize,
    },
    #[error("cannot read language file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("language path must be a regular file and cannot be a symbolic link: {0}")]
    UnsupportedFileType(PathBuf),
    #[error("cannot create language directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("language {0} is not installed")]
    NotInstalled(LocaleId),
    #[error("language file already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("language receipt locale does not match the downloaded package")]
    ReceiptLocaleMismatch,
    #[error("cannot back up language file {path} to {backup}: {source}")]
    Backup {
        path: PathBuf,
        backup: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create temporary language file in {path}: {source}")]
    CreateTemporary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write temporary language file for {path}: {source}")]
    WriteTemporary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot replace language file {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read language directory {path}: {source}")]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot remove language file {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn canonical_resource() -> Result<LanguageResource, LanguageError> {
    LanguageResource::parse(EMBEDDED_EN_US.as_bytes(), &LocaleId::parse("en-US")?, None)
}

fn locale_from_path(path: &Path) -> Result<LocaleId, LocalLanguageError> {
    if path.extension() != Some(OsStr::new("ftl")) {
        return Err(LocalLanguageError::InvalidFileName(path.to_path_buf()));
    }
    let value = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| LocalLanguageError::InvalidFileName(path.to_path_buf()))?;
    let locale = LocaleId::parse(value)
        .map_err(|_| LocalLanguageError::InvalidFileName(path.to_path_buf()))?;
    if locale.to_string() != value {
        return Err(LocalLanguageError::InvalidFileName(path.to_path_buf()));
    }
    Ok(locale)
}

fn read_language_file(path: &Path) -> Result<Vec<u8>, LocalLanguageError> {
    read_language_file_bytes(path, MAX_LANGUAGE_FILE_BYTES)
}

fn read_language_file_bytes(path: &Path, maximum: usize) -> Result<Vec<u8>, LocalLanguageError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalLanguageError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LocalLanguageError::UnsupportedFileType(path.to_path_buf()));
    }
    if metadata.len() > maximum as u64 {
        return Err(LocalLanguageError::FileTooLarge {
            path: path.to_path_buf(),
            actual: metadata.len(),
            maximum,
        });
    }
    let file = fs::File::open(path).map_err(|source| LocalLanguageError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(maximum));
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| LocalLanguageError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > maximum {
        return Err(LocalLanguageError::FileTooLarge {
            path: path.to_path_buf(),
            actual: bytes.len() as u64,
            maximum,
        });
    }
    Ok(bytes)
}

fn read_optional_regular_file(path: &Path) -> Result<Option<Vec<u8>>, LocalLanguageError> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_language_file_bytes(path, MAX_LANGUAGE_FILE_BYTES).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(LocalLanguageError::ReadFile {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn restore_optional_file(path: &Path, bytes: Option<&[u8]>) -> Result<(), LocalLanguageError> {
    match bytes {
        Some(bytes) => atomic_write(path, bytes, true),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(LocalLanguageError::Remove {
                path: path.to_path_buf(),
                source,
            }),
        },
    }
}

fn installed_from_resource(
    resource: &LanguageResource,
    source: LanguageFileSource,
    path: Option<PathBuf>,
) -> InstalledLanguage {
    let metadata: &LanguageMetadata = resource.metadata();
    InstalledLanguage {
        locale: Some(metadata.locale.clone()),
        display_name: Some(metadata.display_name.clone()),
        revision: Some(metadata.revision.clone()),
        source,
        path,
        diagnostics: resource.diagnostics().to_vec(),
        error: None,
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

fn atomic_write(path: &Path, bytes: &[u8], replace: bool) -> Result<(), LocalLanguageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| {
        LocalLanguageError::CreateTemporary {
            path: parent.to_path_buf(),
            source,
        }
    })?;
    temporary
        .write_all(bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| LocalLanguageError::WriteTemporary {
            path: path.to_path_buf(),
            source,
        })?;
    let result = if replace {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    };
    result.map_err(|error| {
        if !replace && error.error.kind() == std::io::ErrorKind::AlreadyExists {
            LocalLanguageError::AlreadyExists(path.to_path_buf())
        } else {
            LocalLanguageError::Replace {
                path: path.to_path_buf(),
                source: error.error,
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(locale: &str, message: &str) -> String {
        format!(
            "### rivo-language-schema: 1\n### locale: {locale}\n### display-name: Test {locale}\n### revision: test.1\n\ncommon-ok = {message}\n"
        )
    }

    fn remote_package(bytes: Vec<u8>) -> (RemoteLanguagePackage, RemoteLanguageReceipt) {
        let manifest = format!(
            r#"{{"schema":1,"languages":[{{"locale":"fr-FR","display_name":"Test fr-FR","revision":"test.1","download_url":"https://languages.example/fr-FR.ftl?token=secret","bytes":{},"sha256":"{}"}}]}}"#,
            bytes.len(),
            crate::remote::sha256_hex(&bytes)
        );
        let registry = crate::RemoteLanguageRegistry::parse(manifest.as_bytes()).unwrap();
        let entry = registry.entries()[0].clone();
        let receipt = RemoteLanguageReceipt::new(
            "https://languages.example/index.json?token=secret",
            &entry,
            42,
        )
        .unwrap();
        let package = RemoteLanguagePackage::validate(entry, bytes).unwrap();
        (package, receipt)
    }

    #[test]
    fn data_root_uses_product_language_directory() {
        let manager = LocalLanguageManager::from_data_root("root");
        assert_eq!(manager.user_dir(), Path::new("root/rivo/languages"));
    }

    #[test]
    fn install_normalizes_name_and_preserves_backup_on_replace() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path().join("languages"));
        let source = directory.path().join("source.ftl");
        fs::write(&source, custom("fr-FR", "D'accord")).unwrap();

        let installed = manager.install(&source, false).unwrap();
        let destination = manager.language_path(&LocaleId::parse("fr-FR").unwrap());
        assert_eq!(installed.path.as_deref(), Some(destination.as_path()));
        assert!(destination.exists());
        assert!(matches!(
            manager.install(&source, false),
            Err(LocalLanguageError::AlreadyExists(_))
        ));

        fs::write(&source, custom("fr-FR", "Oui")).unwrap();
        manager.install(&source, true).unwrap();
        assert!(backup_path(&destination).exists());
        assert!(fs::read_to_string(&destination).unwrap().contains("Oui"));
        assert!(fs::read_to_string(backup_path(&destination))
            .unwrap()
            .contains("D'accord"));
    }

    #[test]
    fn remote_install_writes_receipt_and_rollback_restores_valid_backup() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path().join("languages"));
        let source = directory.path().join("source.ftl");
        fs::write(&source, custom("fr-FR", "D'accord")).unwrap();
        manager.install(&source, false).unwrap();

        let locale = LocaleId::parse("fr-FR").unwrap();
        let (package, receipt) = remote_package(custom("fr-FR", "Oui").into_bytes());
        manager.install_remote(&package, &receipt, true).unwrap();
        assert!(manager.receipt_path(&locale).is_file());
        assert_eq!(manager.receipt(&locale).unwrap(), receipt);
        assert!(fs::read_to_string(manager.language_path(&locale))
            .unwrap()
            .contains("Oui"));

        manager.rollback(&locale).unwrap();
        assert!(!manager.receipt_path(&locale).exists());
        assert!(fs::read_to_string(manager.language_path(&locale))
            .unwrap()
            .contains("D'accord"));
        assert!(
            fs::read_to_string(backup_path(&manager.language_path(&locale)))
                .unwrap()
                .contains("Oui")
        );
    }

    #[test]
    fn invalid_receipt_destination_does_not_change_installed_language() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path().join("languages"));
        let source = directory.path().join("source.ftl");
        fs::write(&source, custom("fr-FR", "D'accord")).unwrap();
        manager.install(&source, false).unwrap();

        let locale = LocaleId::parse("fr-FR").unwrap();
        fs::create_dir(manager.receipt_path(&locale)).unwrap();
        let (package, receipt) = remote_package(custom("fr-FR", "Oui").into_bytes());
        assert!(matches!(
            manager.install_remote(&package, &receipt, true),
            Err(LocalLanguageError::UnsupportedFileType(_))
        ));
        assert!(fs::read_to_string(manager.language_path(&locale))
            .unwrap()
            .contains("D'accord"));
    }

    #[test]
    fn no_clobber_install_preserves_a_racing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("fr-FR.ftl");
        fs::write(&destination, "existing").unwrap();
        let error = atomic_write(&destination, b"replacement", false).unwrap_err();
        assert!(matches!(error, LocalLanguageError::AlreadyExists(_)));
        assert_eq!(fs::read_to_string(destination).unwrap(), "existing");
    }

    #[test]
    fn validate_requires_canonical_file_name_and_matching_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let noncanonical = directory.path().join("FR-fr.ftl");
        fs::write(&noncanonical, custom("fr-FR", "Oui")).unwrap();
        assert!(matches!(
            manager.validate_file(&noncanonical),
            Err(LocalLanguageError::InvalidFileName(_))
        ));

        let mismatch = directory.path().join("de-DE.ftl");
        fs::write(&mismatch, custom("fr-FR", "Oui")).unwrap();
        assert!(matches!(
            manager.validate_file(&mismatch),
            Err(LocalLanguageError::Language(
                LanguageError::LocaleMismatch { .. }
            ))
        ));
    }

    #[test]
    fn list_reports_invalid_user_files_without_hiding_bundled_languages() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        fs::write(directory.path().join("fr-FR.ftl"), "broken").unwrap();
        let languages = manager.list().unwrap();
        assert_eq!(languages.len(), 3);
        assert_eq!(languages[0].locale.as_ref().unwrap().to_string(), "en-US");
        assert_eq!(languages[1].locale.as_ref().unwrap().to_string(), "zh-CN");
        assert!(languages[2].error.is_some());
    }

    #[test]
    fn show_prefers_user_file_and_remove_only_deletes_user_layer() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let source = directory.path().join("source.ftl");
        fs::write(&source, custom("zh-CN", "自定义")).unwrap();
        manager.install(&source, false).unwrap();

        let locale = LocaleId::parse("zh-CN").unwrap();
        assert_eq!(
            manager.show(&locale).unwrap().source,
            LanguageFileSource::User
        );
        manager.remove(&locale).unwrap();
        assert_eq!(
            manager.show(&locale).unwrap().source,
            LanguageFileSource::Bundled
        );
        assert!(matches!(
            manager.remove(&locale),
            Err(LocalLanguageError::NotInstalled(_))
        ));
    }

    #[test]
    fn failed_reload_keeps_the_previous_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let locale = LocaleId::parse("fr-FR").unwrap();
        let path = manager.language_path(&locale);
        fs::write(&path, custom("fr-FR", "D'accord")).unwrap();
        let mut active = ReloadableLanguageCatalog::load(&manager, &locale).unwrap();
        assert_eq!(
            active.catalog().format("common-ok", None).as_deref(),
            Some("D'accord")
        );

        fs::write(&path, "broken").unwrap();
        assert!(active.reload(&manager, &locale).is_err());
        assert_eq!(
            active.catalog().format("common-ok", None).as_deref(),
            Some("D'accord")
        );
    }

    #[test]
    fn discovery_changes_only_after_explicit_reload() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let english = LocaleId::parse("en-US").unwrap();
        let french = LocaleId::parse("fr-FR").unwrap();
        let mut active = ReloadableLanguageCatalog::load(&manager, &english).unwrap();
        assert!(active.discovery().find(&french).is_none());

        fs::write(manager.language_path(&french), custom("fr-FR", "Oui")).unwrap();
        assert!(active.discovery().find(&french).is_none());

        active.reload(&manager, &french).unwrap();
        assert!(active.discovery().find(&french).is_some());
        assert_eq!(
            active.catalog().format("common-ok", None).as_deref(),
            Some("Oui")
        );
    }

    #[test]
    fn unrelated_invalid_file_does_not_block_valid_locale_reload() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let french = LocaleId::parse("fr-FR").unwrap();
        fs::write(manager.language_path(&french), custom("fr-FR", "Oui")).unwrap();
        fs::write(directory.path().join("de-DE.ftl"), "broken").unwrap();

        let active = ReloadableLanguageCatalog::load(&manager, &french).unwrap();
        assert!(active.discovery().find(&french).is_some());
        assert!(active
            .discovery()
            .languages()
            .iter()
            .any(|language| language.error.is_some()));
        assert_eq!(
            active.catalog().format("common-ok", None).as_deref(),
            Some("Oui")
        );
    }

    #[test]
    fn discovery_prefers_valid_user_metadata_over_bundled_locale() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let chinese = LocaleId::parse("zh-CN").unwrap();
        fs::write(manager.language_path(&chinese), custom("zh-CN", "自定义")).unwrap();

        let discovery = manager.discover().unwrap();
        let effective = discovery.find(&chinese).unwrap();
        assert_eq!(effective.source, LanguageFileSource::User);
        assert_eq!(effective.revision.as_deref(), Some("test.1"));
    }

    #[test]
    fn catalog_rejects_an_uninstalled_external_locale() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let french = LocaleId::parse("fr-FR").unwrap();

        assert!(matches!(
            manager.load_catalog(&french),
            Err(LocalLanguageError::NotInstalled(locale)) if locale == french
        ));
    }
}
