use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::Path;
use std::path::PathBuf;

const CODEX_DESKTOP_WSL_NATIVE_CODEX_HOME_ENV_VAR: &str = "CODEX_DESKTOP_WSL_NATIVE_CODEX_HOME";

/// Returns the path to the Codex configuration directory, which can be
/// specified by the `CODEX_HOME` environment variable. If not set, defaults to
/// `~/.codex`.
///
/// - If `CODEX_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `CODEX_HOME` is not set, this function does not verify that the
///   directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let codex_home_env = std::env::var("CODEX_HOME")
        .ok()
        .filter(|val| !val.is_empty());
    let use_native_wsl_codex_home = env_var_truthy(CODEX_DESKTOP_WSL_NATIVE_CODEX_HOME_ENV_VAR);
    find_codex_home_from_env_and_context(
        codex_home_env.as_deref(),
        is_wsl_runtime(),
        use_native_wsl_codex_home,
        home_dir(),
    )
}

/// Returns a WSL-native directory for runtime caches when CODEX_HOME points at
/// a Windows-backed mount. Persistent state such as auth, config, and threads
/// should continue to use CODEX_HOME unless the caller explicitly opts into a
/// native WSL CODEX_HOME.
pub fn runtime_cache_home_for_codex_home(codex_home: &Path) -> PathBuf {
    runtime_cache_home_for_codex_home_from_context(
        codex_home,
        is_wsl_runtime(),
        home_dir(),
        std::env::var("WSL_DISTRO_NAME").ok(),
    )
}

#[cfg(test)]
fn find_codex_home_from_env(codex_home_env: Option<&str>) -> std::io::Result<AbsolutePathBuf> {
    find_codex_home_from_env_and_context(
        codex_home_env,
        is_wsl_runtime(),
        /*use_native_wsl_codex_home*/ false,
        home_dir(),
    )
}

fn find_codex_home_from_env_and_context(
    codex_home_env: Option<&str>,
    is_wsl: bool,
    use_native_wsl_codex_home: bool,
    default_home_dir: Option<PathBuf>,
) -> std::io::Result<AbsolutePathBuf> {
    let codex_home_env = codex_home_env
        .filter(|val| !should_use_native_wsl_codex_home(*val, is_wsl, use_native_wsl_codex_home));

    // Honor the `CODEX_HOME` environment variable when it is set to allow users
    // (and tests) to override the default location.
    match codex_home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("CODEX_HOME points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read CODEX_HOME {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("CODEX_HOME points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize CODEX_HOME {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = default_home_dir.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".codex");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

fn runtime_cache_home_for_codex_home_from_context(
    codex_home: &Path,
    is_wsl: bool,
    default_home_dir: Option<PathBuf>,
    distro_name: Option<String>,
) -> PathBuf {
    if !is_wsl || !path_looks_like_windows_path_in_wsl(codex_home) {
        return codex_home.to_path_buf();
    }

    let Some(native_home) = default_home_dir else {
        return codex_home.to_path_buf();
    };

    native_home
        .join(".codex/runtime/wsl")
        .join(sanitize_namespace_component(
            distro_name.as_deref().unwrap_or("default"),
        ))
}

fn should_use_native_wsl_codex_home(
    codex_home_env: &str,
    is_wsl: bool,
    use_native_wsl_codex_home: bool,
) -> bool {
    is_wsl && use_native_wsl_codex_home && looks_like_windows_path_in_wsl(codex_home_env)
}

fn env_var_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
}

fn path_looks_like_windows_path_in_wsl(path: &Path) -> bool {
    path.to_str().is_some_and(looks_like_windows_path_in_wsl)
}

fn looks_like_windows_path_in_wsl(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() && bytes[2] == b'/' {
        return true;
    }

    let mut components = normalized.split('/').filter(|part| !part.is_empty());
    let Some(mnt) = components.next() else {
        return false;
    };
    if !mnt.eq_ignore_ascii_case("mnt") {
        return false;
    }
    let Some(drive) = components.next() else {
        return false;
    };
    let drive = drive.as_bytes();
    drive.len() == 1 && drive[0].is_ascii_alphabetic()
}

fn sanitize_namespace_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

fn is_wsl_runtime() -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WSL_DISTRO_NAME").is_some() {
            return true;
        }
        match std::fs::read_to_string("/proc/version") {
            Ok(version) => version.to_lowercase().contains("microsoft"),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use super::find_codex_home_from_env_and_context;
    use super::runtime_cache_home_for_codex_home_from_context;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(missing_str)).expect_err("missing CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(file_str)).expect_err("file CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str)).expect("valid CODEX_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved =
            find_codex_home_from_env(/*codex_home_env*/ None).expect("default CODEX_HOME");
        let mut expected = home_dir().expect("home dir");
        expected.push(".codex");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn desktop_wsl_keeps_windows_mount_codex_home_without_native_home_opt_in() {
        let native_home = TempDir::new().expect("native home");
        let codex_home = TempDir::new().expect("codex home");
        let codex_home_str = codex_home
            .path()
            .to_str()
            .expect("codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env_and_context(
            Some(codex_home_str),
            /*is_wsl*/ true,
            /*use_native_wsl_codex_home*/ false,
            Some(native_home.path().to_path_buf()),
        )
        .expect("desktop WSL existing codex home");

        let expected =
            AbsolutePathBuf::from_absolute_path(codex_home.path().canonicalize().unwrap())
                .expect("existing codex home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn desktop_wsl_uses_native_home_when_explicitly_opted_in() {
        let native_home = TempDir::new().expect("native home");
        let windows_home = TempDir::new().expect("windows home");
        let windows_codex_home = windows_home.path().join("Users/alice/.codex");
        fs::create_dir_all(&windows_codex_home).expect("windows codex home");

        let resolved = find_codex_home_from_env_and_context(
            Some("/mnt/c/Users/alice/.codex"),
            /*is_wsl*/ true,
            /*use_native_wsl_codex_home*/ true,
            Some(native_home.path().to_path_buf()),
        )
        .expect("desktop WSL native codex home");

        let expected = AbsolutePathBuf::from_absolute_path(native_home.path().join(".codex"))
            .expect("native codex home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn wsl_native_home_opt_in_does_not_require_originator_override() {
        let native_home = TempDir::new().expect("native home");

        let resolved = find_codex_home_from_env_and_context(
            Some("/mnt/c/Users/alice/.codex"),
            /*is_wsl*/ true,
            /*use_native_wsl_codex_home*/ true,
            Some(native_home.path().to_path_buf()),
        )
        .expect("native codex home");

        let expected = AbsolutePathBuf::from_absolute_path(native_home.path().join(".codex"))
            .expect("native codex home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn wsl_honors_explicit_windows_mount_codex_home_without_native_home_opt_in() {
        let native_home = TempDir::new().expect("native home");
        let codex_home = TempDir::new().expect("codex home");
        let codex_home_str = codex_home
            .path()
            .to_str()
            .expect("codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env_and_context(
            Some(codex_home_str),
            /*is_wsl*/ true,
            /*use_native_wsl_codex_home*/ false,
            Some(native_home.path().to_path_buf()),
        )
        .expect("explicit codex home");

        let expected =
            AbsolutePathBuf::from_absolute_path(codex_home.path().canonicalize().unwrap())
                .expect("explicit codex home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn runtime_cache_home_moves_windows_backed_wsl_codex_home_to_native_home() {
        let native_home = TempDir::new().expect("native home");
        let resolved = runtime_cache_home_for_codex_home_from_context(
            "/mnt/c/Users/alice/.codex".as_ref(),
            /*is_wsl*/ true,
            Some(native_home.path().to_path_buf()),
            Some("Ubuntu 24.04".to_string()),
        );

        assert_eq!(
            resolved,
            native_home.path().join(".codex/runtime/wsl/Ubuntu-24.04")
        );
    }

    #[test]
    fn runtime_cache_home_keeps_non_windows_backed_codex_home() {
        let native_home = TempDir::new().expect("native home");
        let codex_home = PathBuf::from("/home/alice/.codex");

        let resolved = runtime_cache_home_for_codex_home_from_context(
            codex_home.as_path(),
            /*is_wsl*/ true,
            Some(native_home.path().to_path_buf()),
            Some("Ubuntu".to_string()),
        );

        assert_eq!(resolved, codex_home);
    }
}
