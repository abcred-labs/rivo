/// Resolve the version embedded in the llm HTTP identity.
///
/// Release builds pass the already-resolved `RIVO_RELEASE_VERSION` from CI.
/// Local builds intentionally use the package version and do not reproduce
/// the release-number calculation here.
pub fn resolve_release_version(
    package_version: &str,
    explicit_version: Option<&str>,
) -> Result<String, String> {
    match explicit_version {
        Some(version) if version.trim().is_empty() => {
            Err("RIVO_RELEASE_VERSION must not be empty".to_owned())
        }
        Some(version) => Ok(version.to_owned()),
        None => Ok(package_version.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_release_version;

    #[test]
    fn local_build_falls_back_to_package_version() {
        assert_eq!(
            resolve_release_version("0.1.0", None).unwrap(),
            "0.1.0".to_owned()
        );
    }

    #[test]
    fn ci_release_version_takes_precedence() {
        assert_eq!(
            resolve_release_version("0.1.0", Some("0.1.132")).unwrap(),
            "0.1.132".to_owned()
        );
    }

    #[test]
    fn empty_release_version_is_rejected() {
        assert_eq!(
            resolve_release_version("0.1.0", Some("  ")).unwrap_err(),
            "RIVO_RELEASE_VERSION must not be empty"
        );
    }
}
