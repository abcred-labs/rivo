//! UI-only Fluent language resources for Rivo.

mod local;
mod remote;

pub use local::{
    InstalledLanguage, LanguageDiscovery, LanguageFileSource, LocalLanguageError,
    LocalLanguageManager, ReloadableLanguageCatalog,
};
pub use remote::{
    RemoteLanguageClient, RemoteLanguageEntry, RemoteLanguageError, RemoteLanguagePackage,
    RemoteLanguageReceipt, RemoteLanguageRegistry, MAX_LANGUAGE_REGISTRY_BYTES,
    MAX_LANGUAGE_REGISTRY_ENTRIES,
};

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use fluent_syntax::ast::{Entry, Expression, InlineExpression, Pattern, PatternElement};
use fluent_syntax::parser;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use unic_langid::LanguageIdentifier;

pub const LANGUAGE_SCHEMA_VERSION: u32 = 1;
pub const MAX_LANGUAGE_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_DISPLAY_NAME_CHARS: usize = 128;
pub const MAX_LOCALE_CHARS: usize = 64;
pub const MAX_REVISION_CHARS: usize = 128;
pub const MAX_SCHEMA_CHARS: usize = 16;
pub const MAX_VERSION_CHARS: usize = 64;
pub const EMBEDDED_EN_US: &str = include_str!("../../../languages/en-US.ftl");
pub const EMBEDDED_ZH_CN: &str = include_str!("../../../languages/zh-CN.ftl");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocaleId(LanguageIdentifier);

impl LocaleId {
    pub fn parse(value: &str) -> Result<Self, LanguageError> {
        let value = value.trim();
        let locale = LanguageIdentifier::from_str(value)
            .map_err(|_| LanguageError::InvalidLocale(value.to_owned()))?;
        Ok(Self(locale))
    }

    pub fn as_language_identifier(&self) -> &LanguageIdentifier {
        &self.0
    }
}

impl fmt::Display for LocaleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageMetadata {
    pub schema: u32,
    pub locale: LocaleId,
    pub display_name: String,
    pub revision: String,
    pub min_rivo_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSchema {
    messages: BTreeMap<String, BTreeSet<String>>,
}

impl MessageSchema {
    pub fn message_ids(&self) -> impl Iterator<Item = &str> {
        self.messages.keys().map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageDiagnostic {
    UnknownMessage(String),
}

#[derive(Debug)]
pub struct LanguageResource {
    metadata: LanguageMetadata,
    source: String,
    schema: MessageSchema,
    diagnostics: Vec<LanguageDiagnostic>,
}

impl LanguageResource {
    pub fn parse_detected(
        bytes: &[u8],
        canonical_schema: Option<&MessageSchema>,
    ) -> Result<Self, LanguageError> {
        if bytes.len() > MAX_LANGUAGE_FILE_BYTES {
            return Err(LanguageError::FileTooLarge {
                actual: bytes.len(),
                maximum: MAX_LANGUAGE_FILE_BYTES,
            });
        }
        let source = std::str::from_utf8(bytes).map_err(LanguageError::InvalidUtf8)?;
        validate_source_characters(source)?;
        let locale = parse_metadata(source)?.locale;
        Self::parse(bytes, &locale, canonical_schema)
    }

    pub fn parse(
        bytes: &[u8],
        expected_locale: &LocaleId,
        canonical_schema: Option<&MessageSchema>,
    ) -> Result<Self, LanguageError> {
        if bytes.len() > MAX_LANGUAGE_FILE_BYTES {
            return Err(LanguageError::FileTooLarge {
                actual: bytes.len(),
                maximum: MAX_LANGUAGE_FILE_BYTES,
            });
        }
        let source = std::str::from_utf8(bytes)
            .map_err(LanguageError::InvalidUtf8)?
            .to_owned();
        validate_source_characters(&source)?;
        let metadata = parse_metadata(&source)?;
        if &metadata.locale != expected_locale {
            return Err(LanguageError::LocaleMismatch {
                expected: expected_locale.clone(),
                actual: metadata.locale,
            });
        }

        let ast = parser::parse(source.as_str())
            .map_err(|(_, errors)| LanguageError::FluentSyntax(format!("{errors:?}")))?;
        let schema = schema_from_ast(&ast)?;
        let mut diagnostics = Vec::new();
        if let Some(canonical) = canonical_schema {
            for (message, variables) in &schema.messages {
                match canonical.messages.get(message) {
                    Some(expected) if expected == variables => {}
                    Some(expected) => {
                        return Err(LanguageError::VariableMismatch {
                            message: message.clone(),
                            expected: expected.clone(),
                            actual: variables.clone(),
                        });
                    }
                    None => diagnostics.push(LanguageDiagnostic::UnknownMessage(message.clone())),
                }
            }
        }

        FluentResource::try_new(source.clone())
            .map_err(|(_, errors)| LanguageError::FluentResource(format!("{errors:?}")))?;
        Ok(Self {
            metadata,
            source,
            schema,
            diagnostics,
        })
    }

    pub fn metadata(&self) -> &LanguageMetadata {
        &self.metadata
    }

    pub fn message_schema(&self) -> &MessageSchema {
        &self.schema
    }

    pub fn diagnostics(&self) -> &[LanguageDiagnostic] {
        &self.diagnostics
    }
}

pub struct LanguageCatalog {
    locale: LocaleId,
    allowed_message_ids: BTreeSet<String>,
    layers: Vec<FluentBundle<FluentResource>>,
}

impl LanguageCatalog {
    pub fn from_resources(
        locale: LocaleId,
        canonical_schema: &MessageSchema,
        resources_by_priority: impl IntoIterator<Item = LanguageResource>,
    ) -> Result<Self, LanguageError> {
        let mut layers = Vec::new();
        for resource in resources_by_priority {
            let resource_locale = resource.metadata.locale.clone();
            if resource_locale != locale && resource_locale.to_string() != "en-US" {
                return Err(LanguageError::CatalogLocaleMismatch {
                    catalog: locale,
                    resource: resource_locale,
                });
            }
            let fluent_resource = FluentResource::try_new(resource.source)
                .map_err(|(_, errors)| LanguageError::FluentResource(format!("{errors:?}")))?;
            let mut bundle = FluentBundle::new(vec![resource_locale.0]);
            bundle
                .add_resource(fluent_resource)
                .map_err(|errors| LanguageError::FluentResource(format!("{errors:?}")))?;
            layers.push(bundle);
        }
        Ok(Self {
            locale,
            allowed_message_ids: canonical_schema.messages.keys().cloned().collect(),
            layers,
        })
    }

    pub fn locale(&self) -> &LocaleId {
        &self.locale
    }

    pub fn format(&self, message_id: &str, args: Option<&FluentArgs<'_>>) -> Option<String> {
        if !self.allowed_message_ids.contains(message_id) {
            return None;
        }
        for bundle in &self.layers {
            let Some(message) = bundle.get_message(message_id) else {
                continue;
            };
            let Some(pattern) = message.value() else {
                continue;
            };
            let mut errors = Vec::new();
            let value = bundle.format_pattern(pattern, args, &mut errors);
            if errors.is_empty() {
                return Some(value.into_owned());
            }
        }
        None
    }
}

#[derive(Debug, Error)]
pub enum LanguageError {
    #[error("invalid locale: {0}")]
    InvalidLocale(String),
    #[error("language file is {actual} bytes; maximum is {maximum}")]
    FileTooLarge { actual: usize, maximum: usize },
    #[error("language file is not UTF-8")]
    InvalidUtf8(#[source] std::str::Utf8Error),
    #[error("language file contains a disallowed control character U+{code:04X}")]
    DisallowedControlCharacter { code: u32 },
    #[error("missing language metadata: {0}")]
    MissingMetadata(&'static str),
    #[error("duplicate language metadata: {0}")]
    DuplicateMetadata(String),
    #[error("invalid language metadata {field}: {value}")]
    InvalidMetadata { field: &'static str, value: String },
    #[error("language file requires Rivo {required} or newer; current version is {current}")]
    IncompatibleVersion { required: String, current: String },
    #[error("expected locale {expected}, found {actual}")]
    LocaleMismatch {
        expected: LocaleId,
        actual: LocaleId,
    },
    #[error("language file contains invalid Fluent syntax: {0}")]
    FluentSyntax(String),
    #[error("language resource is invalid: {0}")]
    FluentResource(String),
    #[error("duplicate message id: {0}")]
    DuplicateMessage(String),
    #[error("message has no value: {0}")]
    MissingMessageValue(String),
    #[error("message {message} variables differ: expected {expected:?}, found {actual:?}")]
    VariableMismatch {
        message: String,
        expected: BTreeSet<String>,
        actual: BTreeSet<String>,
    },
    #[error("catalog locale {catalog} cannot contain resource locale {resource}")]
    CatalogLocaleMismatch {
        catalog: LocaleId,
        resource: LocaleId,
    },
}

fn parse_metadata(source: &str) -> Result<LanguageMetadata, LanguageError> {
    let mut values = BTreeMap::new();
    for line in source.lines() {
        let Some(comment) = line.strip_prefix("###") else {
            if line.trim().is_empty() {
                continue;
            }
            break;
        };
        let Some((key, value)) = comment.trim().split_once(':') else {
            continue;
        };
        let key = key.trim();
        if values.insert(key, value.trim()).is_some() {
            return Err(LanguageError::DuplicateMetadata(key.to_owned()));
        }
    }
    let required = |key: &'static str| {
        values
            .get(key)
            .copied()
            .filter(|value| !value.is_empty())
            .ok_or(LanguageError::MissingMetadata(key))
    };
    let schema_value = validate_metadata_value(
        "rivo-language-schema",
        required("rivo-language-schema")?,
        MAX_SCHEMA_CHARS,
    )?;
    let schema = schema_value
        .parse::<u32>()
        .map_err(|_| LanguageError::InvalidMetadata {
            field: "rivo-language-schema",
            value: "value must be the supported numeric schema".to_owned(),
        })?;
    if schema != LANGUAGE_SCHEMA_VERSION {
        return Err(LanguageError::InvalidMetadata {
            field: "rivo-language-schema",
            value: "value must be the supported numeric schema".to_owned(),
        });
    }
    let locale_value = validate_metadata_value("locale", required("locale")?, MAX_LOCALE_CHARS)?;
    let display_name = validate_metadata_value(
        "display-name",
        required("display-name")?,
        MAX_DISPLAY_NAME_CHARS,
    )?;
    let revision = validate_metadata_value("revision", required("revision")?, MAX_REVISION_CHARS)?;
    let min_rivo_version = values
        .get("min-rivo-version")
        .map(|value| validate_metadata_value("min-rivo-version", value, MAX_VERSION_CHARS))
        .transpose()?;
    if let Some(required) = &min_rivo_version {
        let required_core =
            parse_version_core(required).ok_or_else(|| LanguageError::InvalidMetadata {
                field: "min-rivo-version",
                value: "value must be a supported SemVer".to_owned(),
            })?;
        let current = env!("CARGO_PKG_VERSION");
        let current_core = parse_version_core(current).expect("package version is valid SemVer");
        if required_core > current_core {
            return Err(LanguageError::IncompatibleVersion {
                required: required.clone(),
                current: current.to_owned(),
            });
        }
    }
    Ok(LanguageMetadata {
        schema,
        locale: LocaleId::parse(&locale_value)?,
        display_name,
        revision,
        min_rivo_version,
    })
}

fn validate_source_characters(source: &str) -> Result<(), LanguageError> {
    if let Some(character) = source
        .chars()
        .find(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(LanguageError::DisallowedControlCharacter {
            code: character as u32,
        });
    }
    Ok(())
}

fn validate_metadata_value(
    field: &'static str,
    value: &str,
    maximum_chars: usize,
) -> Result<String, LanguageError> {
    if value.chars().count() > maximum_chars || value.chars().any(char::is_control) {
        return Err(LanguageError::InvalidMetadata {
            field,
            value: "value exceeds the allowed length or contains control characters".to_owned(),
        });
    }
    Ok(value.to_owned())
}

fn parse_version_core(value: &str) -> Option<[u64; 3]> {
    if value.starts_with('v') || value.chars().any(char::is_whitespace) {
        return None;
    }
    let mut core_and_suffix = value.splitn(2, ['-', '+']);
    let core = core_and_suffix.next()?;
    let suffix = core_and_suffix.next();
    if suffix
        .is_some_and(|suffix| suffix.is_empty() || suffix.split(['.', '-', '+']).any(str::is_empty))
    {
        return None;
    }
    let mut numbers = core.split('.');
    let version = [
        numbers.next()?.parse().ok()?,
        numbers.next()?.parse().ok()?,
        numbers.next()?.parse().ok()?,
    ];
    if numbers.next().is_some() {
        return None;
    }
    Some(version)
}

fn schema_from_ast(
    resource: &fluent_syntax::ast::Resource<&str>,
) -> Result<MessageSchema, LanguageError> {
    let mut messages = BTreeMap::new();
    for entry in &resource.body {
        let Entry::Message(message) = entry else {
            continue;
        };
        let id = message.id.name.to_owned();
        let value = message
            .value
            .as_ref()
            .ok_or_else(|| LanguageError::MissingMessageValue(id.clone()))?;
        let mut variables = BTreeSet::new();
        collect_pattern_variables(value, &mut variables);
        if messages.insert(id.clone(), variables).is_some() {
            return Err(LanguageError::DuplicateMessage(id));
        }
    }
    Ok(MessageSchema { messages })
}

fn collect_pattern_variables(pattern: &Pattern<&str>, variables: &mut BTreeSet<String>) {
    for element in &pattern.elements {
        if let PatternElement::Placeable { expression } = element {
            collect_expression_variables(expression, variables);
        }
    }
}

fn collect_expression_variables(expression: &Expression<&str>, variables: &mut BTreeSet<String>) {
    match expression {
        Expression::Inline(inline) => collect_inline_variables(inline, variables),
        Expression::Select { selector, variants } => {
            collect_inline_variables(selector, variables);
            for variant in variants {
                collect_pattern_variables(&variant.value, variables);
            }
        }
    }
}

fn collect_inline_variables(expression: &InlineExpression<&str>, variables: &mut BTreeSet<String>) {
    match expression {
        InlineExpression::VariableReference { id } => {
            variables.insert(id.name.to_owned());
        }
        InlineExpression::FunctionReference { arguments, .. }
        | InlineExpression::TermReference {
            arguments: Some(arguments),
            ..
        } => {
            for positional in &arguments.positional {
                collect_inline_variables(positional, variables);
            }
            for named in &arguments.named {
                collect_inline_variables(&named.value, variables);
            }
        }
        InlineExpression::Placeable { expression } => {
            collect_expression_variables(expression, variables);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale(value: &str) -> LocaleId {
        LocaleId::parse(value).expect("valid locale")
    }

    fn resource(source: &str, expected: &str, schema: Option<&MessageSchema>) -> LanguageResource {
        LanguageResource::parse(source.as_bytes(), &locale(expected), schema)
            .expect("valid language resource")
    }

    #[test]
    fn locale_is_canonicalized() {
        assert_eq!(locale("ZH-cn").to_string(), "zh-CN");
        assert!(LocaleId::parse("not_a_locale").is_err());
    }

    #[test]
    fn bundled_resources_share_the_canonical_schema() {
        let english = resource(EMBEDDED_EN_US, "en-US", None);
        let chinese = resource(EMBEDDED_ZH_CN, "zh-CN", Some(english.message_schema()));
        assert_eq!(
            english.message_schema().messages,
            chinese.message_schema().messages
        );
        assert!(chinese.diagnostics().is_empty());
    }

    #[test]
    fn partial_override_falls_back_by_message() {
        let english = resource(EMBEDDED_EN_US, "en-US", None);
        let chinese = resource(EMBEDDED_ZH_CN, "zh-CN", Some(english.message_schema()));
        let custom = resource(
            "### rivo-language-schema: 1\n### locale: zh-CN\n### display-name: 自定义\n### revision: local\n\ncommon-ok = 好\n",
            "zh-CN",
            Some(english.message_schema()),
        );
        let schema = english.message_schema().clone();
        let catalog =
            LanguageCatalog::from_resources(locale("zh-CN"), &schema, [custom, chinese, english])
                .expect("valid catalog");
        assert_eq!(catalog.format("common-ok", None).as_deref(), Some("好"));
        assert_eq!(
            catalog.format("common-cancel", None).as_deref(),
            Some("取消")
        );
    }

    #[test]
    fn variable_mismatch_rejects_whole_resource() {
        let english = resource(EMBEDDED_EN_US, "en-US", None);
        let invalid = "### rivo-language-schema: 1\n### locale: zh-CN\n### display-name: 中文\n### revision: bad\n\nerror-language-invalid = 无效：{ $language }\n";
        let error = LanguageResource::parse(
            invalid.as_bytes(),
            &locale("zh-CN"),
            Some(english.message_schema()),
        )
        .expect_err("variable mismatch must fail");
        assert!(matches!(error, LanguageError::VariableMismatch { .. }));
    }

    #[test]
    fn select_expression_variables_are_validated() {
        let english = resource(
            "### rivo-language-schema: 1\n### locale: en-US\n### display-name: English\n### revision: test\n\ncount = { $count ->\n    [one] One\n   *[other] Other\n}\n",
            "en-US",
            None,
        );
        let invalid = "### rivo-language-schema: 1\n### locale: zh-CN\n### display-name: 中文\n### revision: bad\n\ncount = { $total ->\n    [one] 一个\n   *[other] 多个\n}\n";
        let error = LanguageResource::parse(
            invalid.as_bytes(),
            &locale("zh-CN"),
            Some(english.message_schema()),
        )
        .expect_err("select variable mismatch must fail");
        assert!(matches!(error, LanguageError::VariableMismatch { .. }));
    }

    #[test]
    fn duplicate_message_rejects_whole_resource() {
        let duplicate = "### rivo-language-schema: 1\n### locale: en-US\n### display-name: English\n### revision: bad\n\ncommon-ok = OK\ncommon-ok = Again\n";
        let error = LanguageResource::parse(duplicate.as_bytes(), &locale("en-US"), None)
            .expect_err("duplicate must fail");
        assert!(matches!(error, LanguageError::DuplicateMessage(_)));
    }

    #[test]
    fn rejects_invalid_utf8_and_oversized_files() {
        assert!(matches!(
            LanguageResource::parse(&[0xff], &locale("en-US"), None),
            Err(LanguageError::InvalidUtf8(_))
        ));
        let oversized = vec![b'a'; MAX_LANGUAGE_FILE_BYTES + 1];
        assert!(matches!(
            LanguageResource::parse(&oversized, &locale("en-US"), None),
            Err(LanguageError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_terminal_control_characters_and_oversized_metadata() {
        let escaped = "### rivo-language-schema: 1\n### locale: en-US\n### display-name: English\n### revision: test\n\ncommon-ok = \u{1b}[31mSpoofed\n";
        assert!(matches!(
            LanguageResource::parse(escaped.as_bytes(), &locale("en-US"), None),
            Err(LanguageError::DisallowedControlCharacter { .. })
        ));
        assert!(matches!(
            LanguageResource::parse_detected(escaped.as_bytes(), None),
            Err(LanguageError::DisallowedControlCharacter { .. })
        ));

        let oversized_name = "x".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        let source = format!(
            "### rivo-language-schema: 1\n### locale: en-US\n### display-name: {oversized_name}\n### revision: test\n\ncommon-ok = OK\n"
        );
        assert!(matches!(
            LanguageResource::parse(source.as_bytes(), &locale("en-US"), None),
            Err(LanguageError::InvalidMetadata {
                field: "display-name",
                ..
            })
        ));
    }

    #[test]
    fn validates_minimum_rivo_version() {
        let incompatible = "### rivo-language-schema: 1\n### locale: en-US\n### display-name: English\n### revision: test\n### min-rivo-version: 999.0.0\n\ncommon-ok = OK\n";
        assert!(matches!(
            LanguageResource::parse(incompatible.as_bytes(), &locale("en-US"), None),
            Err(LanguageError::IncompatibleVersion { .. })
        ));

        let invalid = "### rivo-language-schema: 1\n### locale: en-US\n### display-name: English\n### revision: test\n### min-rivo-version: latest\n\ncommon-ok = OK\n";
        assert!(matches!(
            LanguageResource::parse(invalid.as_bytes(), &locale("en-US"), None),
            Err(LanguageError::InvalidMetadata {
                field: "min-rivo-version",
                ..
            })
        ));
    }

    #[test]
    fn rejects_duplicate_metadata() {
        let duplicate = "### rivo-language-schema: 1\n### locale: en-US\n### locale: zh-CN\n### display-name: English\n### revision: bad\n\ncommon-ok = OK\n";
        assert!(matches!(
            LanguageResource::parse(duplicate.as_bytes(), &locale("en-US"), None),
            Err(LanguageError::DuplicateMetadata(_))
        ));
    }

    #[test]
    fn formats_variables_without_exposing_source() {
        let english = resource(EMBEDDED_EN_US, "en-US", None);
        let schema = english.message_schema().clone();
        let catalog = LanguageCatalog::from_resources(locale("en-US"), &schema, [english])
            .expect("valid catalog");
        let mut args = FluentArgs::new();
        args.set("locale", "fr-FR");
        assert_eq!(
            catalog
                .format("error-language-invalid", Some(&args))
                .as_deref(),
            Some("Unsupported language: \u{2068}fr-FR\u{2069}")
        );
    }

    #[test]
    fn unknown_messages_are_diagnostic_only() {
        let english = resource(EMBEDDED_EN_US, "en-US", None);
        let custom = resource(
            "### rivo-language-schema: 1\n### locale: en-US\n### display-name: Custom\n### revision: local\n\ncustom-command = Not allowed\n",
            "en-US",
            Some(english.message_schema()),
        );
        assert_eq!(
            custom.diagnostics(),
            &[LanguageDiagnostic::UnknownMessage(
                "custom-command".to_owned()
            )]
        );
        let schema = english.message_schema().clone();
        let catalog = LanguageCatalog::from_resources(locale("en-US"), &schema, [custom, english])
            .expect("valid catalog");
        assert_eq!(catalog.format("custom-command", None), None);
    }

    #[test]
    fn formatting_error_falls_back_to_next_layer() {
        let english = resource(EMBEDDED_EN_US, "en-US", None);
        let custom = resource(
            "### rivo-language-schema: 1\n### locale: en-US\n### display-name: Custom\n### revision: local\n\ncommon-ok = { missing-message }\n",
            "en-US",
            Some(english.message_schema()),
        );
        let schema = english.message_schema().clone();
        let catalog = LanguageCatalog::from_resources(locale("en-US"), &schema, [custom, english])
            .expect("valid catalog");
        assert_eq!(catalog.format("common-ok", None).as_deref(), Some("OK"));
    }
}
