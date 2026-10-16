pub fn resolve_release_version(
    base_version: &str,
    explicit_version: Option<String>,
    build_number: Option<String>,
) -> Result<String, String> {
    if let Some(version) = explicit_version {
        if version.trim().is_empty() {
            return Err("RIVO_RELEASE_VERSION must not be empty".to_owned());
        }
        if let Some(build_number) = build_number {
            let expected = compute_ci_release_version(base_version, &build_number)?;
            if version != expected {
                return Err(format!(
                    "RIVO_RELEASE_VERSION {version} does not match build version {expected}"
                ));
            }
        }
        return Ok(version);
    }

    let Some(build_number) = build_number else {
        return Ok(base_version.to_owned());
    };
    compute_ci_release_version(base_version, &build_number)
}

fn compute_ci_release_version(base_version: &str, build_number: &str) -> Result<String, String> {
    let mut parts = base_version.split('.');
    let major = parts
        .next()
        .ok_or_else(|| format!("base version must be major.minor.0: {base_version}"))?
        .parse::<u32>()
        .map_err(|_| format!("base version must be numeric major.minor.0: {base_version}"))?;
    let minor = parts
        .next()
        .ok_or_else(|| format!("base version must be major.minor.0: {base_version}"))?
        .parse::<u32>()
        .map_err(|_| format!("base version must be numeric major.minor.0: {base_version}"))?;
    let patch = parts
        .next()
        .ok_or_else(|| format!("base version must be major.minor.0: {base_version}"))?
        .parse::<u32>()
        .map_err(|_| format!("base version must be numeric major.minor.0: {base_version}"))?;
    if parts.next().is_some() || patch != 0 {
        return Err(format!(
            "base version must be numeric major.minor.0: {base_version}"
        ));
    }

    let build = build_number
        .parse::<u16>()
        .map_err(|_| format!("Build number must be a positive integer: {build_number}"))?;
    if !(1..=255).contains(&build) {
        return Err(format!(
            "Build number must be between 1 and 255: {build_number}"
        ));
    }
    Ok(format!("{major}.{minor}.{build}"))
}

#[cfg(test)]
mod tests {
    use super::{compute_ci_release_version, resolve_release_version};

    #[test]
    fn ci_build_number_becomes_patch_version() {
        assert_eq!(
            compute_ci_release_version("0.1.0", "121").unwrap(),
            "0.1.121"
        );
    }

    #[test]
    fn local_build_without_ci_environment_keeps_base_version() {
        assert_eq!(
            resolve_release_version("0.1.0", None, None).unwrap(),
            "0.1.0"
        );
    }

    #[test]
    fn matching_explicit_release_version_is_accepted() {
        assert_eq!(
            resolve_release_version("0.1.0", Some("0.1.121".to_owned()), Some("121".to_owned()))
                .unwrap(),
            "0.1.121"
        );
    }

    #[test]
    fn mismatched_explicit_release_version_is_rejected() {
        assert_eq!(
            resolve_release_version("0.1.0", Some("0.1.122".to_owned()), Some("121".to_owned()))
                .unwrap_err(),
            "RIVO_RELEASE_VERSION 0.1.122 does not match build version 0.1.121"
        );
    }

    #[test]
    fn invalid_ci_versions_are_rejected() {
        for (base, build) in [
            ("0.1.1", "121"),
            ("0.1.0", "0"),
            ("0.1.0", "256"),
            ("0.1.0", "abc"),
        ] {
            assert!(compute_ci_release_version(base, build).is_err());
        }
    }
}
