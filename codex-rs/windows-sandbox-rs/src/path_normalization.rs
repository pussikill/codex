use std::path::Path;
use std::path::PathBuf;

pub fn canonicalize_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn canonical_path_key(path: &Path) -> String {
    canonicalize_path(path)
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

pub(crate) fn is_acl_unsupported_path(path: &Path) -> bool {
    let key = canonical_path_key(path);
    key.starts_with("//wsl.localhost/")
        || key.starts_with("//wsl$/")
        || key.starts_with("//?/unc/wsl.localhost/")
        || key.starts_with("//?/unc/wsl$/")
}

#[cfg(test)]
mod tests {
    use super::canonical_path_key;
    use super::is_acl_unsupported_path;
    use pretty_assertions::assert_eq;
    use std::path::Path;

    #[test]
    fn canonical_path_key_normalizes_case_and_separators() {
        let windows_style = Path::new(r"C:\Users\Dev\Repo");
        let slash_style = Path::new("c:/users/dev/repo");

        assert_eq!(
            canonical_path_key(windows_style),
            canonical_path_key(slash_style)
        );
    }

    #[test]
    fn acl_unsupported_paths_match_wsl_unc_variants() {
        assert!(is_acl_unsupported_path(Path::new(
            r"\\wsl.localhost\Ubuntu\home\dev\repo"
        )));
        assert!(is_acl_unsupported_path(Path::new(
            r"\\wsl$\Ubuntu\home\dev\repo"
        )));
        assert!(is_acl_unsupported_path(Path::new(
            r"\\?\UNC\wsl.localhost\Ubuntu\home\dev\repo"
        )));
        assert!(is_acl_unsupported_path(Path::new(
            r"\\?\UNC\wsl$\Ubuntu\home\dev\repo"
        )));
        assert!(!is_acl_unsupported_path(Path::new(r"C:\Users\dev\repo")));
        assert!(!is_acl_unsupported_path(Path::new(r"\\server\share\repo")));
    }
}
