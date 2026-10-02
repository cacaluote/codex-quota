//! Locates query backends separately from the standalone CLI used by presence tracking.

use std::cmp::Reverse;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::Storage::Packaging::Appx::{
    FindPackagesByPackageFamily, GetPackagePathByFullName, PACKAGE_FILTER_HEAD,
};
use windows::core::{PCWSTR, PWSTR};

use crate::error::AppError;

pub(crate) const CODEX_APP_PACKAGE_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackendSource {
    Desktop,
    Cli,
}

impl BackendSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Codex Desktop",
            Self::Cli => "Codex CLI",
        }
    }
}

#[derive(Debug)]
pub(crate) struct AppServerBackend {
    pub(crate) path: PathBuf,
    pub(crate) source: BackendSource,
}

/// Resolves a fresh backend for each pull; runtime failures do not select another backend.
pub(crate) fn find_app_server_backend() -> Result<AppServerBackend, AppError> {
    select_backend(find_desktop_backend(), find_cli_executable)
}

fn select_backend(
    desktop: Result<Option<PathBuf>, AppError>,
    find_cli: impl FnOnce() -> Option<PathBuf>,
) -> Result<AppServerBackend, AppError> {
    match desktop {
        Ok(Some(path)) => {
            return Ok(AppServerBackend {
                path,
                source: BackendSource::Desktop,
            });
        }
        Err(error) => crate::logging::log(&format!("无法定位 Codex Desktop 后端：{error}")),
        Ok(None) => {}
    }
    find_cli()
        .map(|path| AppServerBackend {
            path,
            source: BackendSource::Cli,
        })
        .ok_or(AppError::CliNotFound)
}

/// Finds only a standalone CLI, preserving presence tracking's existing selection order.
pub(crate) fn find_cli_executable() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA");
    let path = std::env::var_os("PATH");
    find_cli_in(local_app_data.as_deref().map(Path::new), path.as_deref())
}

fn find_cli_in(local_app_data: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let installed = local_app_data.map(|directory| {
        directory
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("codex.exe")
    });
    let on_path = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join("codex.exe"));
    installed.into_iter().chain(on_path).find_map(|candidate| {
        if candidate.is_file() {
            std::path::absolute(candidate).ok()
        } else {
            None
        }
    })
}

fn find_desktop_backend() -> Result<Option<PathBuf>, AppError> {
    desktop_backend_in(desktop_packages()?, package_path)
}

fn desktop_backend_in(
    mut packages: Vec<String>,
    mut resolve_path: impl FnMut(&str) -> Result<PathBuf, AppError>,
) -> Result<Option<PathBuf>, AppError> {
    packages.sort_unstable_by_key(|name| Reverse(package_version(name)));
    for name in packages {
        let candidate = resolve_path(&name)?
            .join("app")
            .join("resources")
            .join("codex.exe");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn desktop_packages() -> Result<Vec<String>, AppError> {
    let family: Vec<u16> = CODEX_APP_PACKAGE_FAMILY
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut count = 0_u32;
    let mut length = 0_u32;
    // SAFETY: family is NUL-terminated; null output buffers query the required capacities.
    let status = unsafe {
        FindPackagesByPackageFamily(
            PCWSTR(family.as_ptr()),
            PACKAGE_FILTER_HEAD,
            &raw mut count,
            None,
            &raw mut length,
            None,
            None,
        )
    };
    if status == ERROR_SUCCESS && count == 0 {
        return Ok(Vec::new());
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(package_error(
            "FindPackagesByPackageFamily 长度查询",
            status,
        ));
    }
    let mut names = vec![PWSTR::null(); count as usize];
    let mut buffer = vec![0_u16; length as usize];
    // SAFETY: both output arrays have the capacities returned by the preceding query.
    // On success, name pointers refer to NUL-terminated strings inside the live buffer.
    let status = unsafe {
        FindPackagesByPackageFamily(
            PCWSTR(family.as_ptr()),
            PACKAGE_FILTER_HEAD,
            &raw mut count,
            Some(names.as_mut_ptr()),
            &raw mut length,
            Some(PWSTR(buffer.as_mut_ptr())),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(package_error("FindPackagesByPackageFamily", status));
    }
    names
        .into_iter()
        .take(count as usize)
        .map(|name| {
            // SAFETY: the successful call populated each pointer; buffer remains alive here.
            unsafe { name.to_string() }
                .map_err(|error| AppError::Windows(format!("安装包名称不是有效 UTF-16：{error}")))
        })
        .collect()
}

fn package_path(name: &str) -> Result<PathBuf, AppError> {
    let full_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut length = 0_u32;
    // SAFETY: full_name is NUL-terminated; no output buffer queries the required capacity.
    let status =
        unsafe { GetPackagePathByFullName(PCWSTR(full_name.as_ptr()), &raw mut length, None) };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(package_error("GetPackagePathByFullName 长度查询", status));
    }
    let mut buffer = vec![0_u16; length as usize];
    // SAFETY: buffer has the queried UTF-16 capacity and lives for the entire call.
    let status = unsafe {
        GetPackagePathByFullName(
            PCWSTR(full_name.as_ptr()),
            &raw mut length,
            Some(PWSTR(buffer.as_mut_ptr())),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(package_error("GetPackagePathByFullName", status));
    }
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Ok(PathBuf::from(OsString::from_wide(&buffer[..end])))
}

fn package_version(name: &str) -> Option<[u16; 4]> {
    let mut parts = name.split('_').nth(1)?.split('.');
    let mut version = [0; 4];
    for component in &mut version {
        *component = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

fn package_error(operation: &str, status: WIN32_ERROR) -> AppError {
    AppError::Windows(format!("{operation} 失败：{}", status.0))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "codex-quota-install-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn create_file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, []).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn backend_selection_preserves_priority_and_fallback() {
        let cases = [
            (
                "both",
                Ok(Some("desktop")),
                Some("cli"),
                Some((BackendSource::Desktop, "desktop")),
            ),
            (
                "desktop only",
                Ok(Some("desktop")),
                None,
                Some((BackendSource::Desktop, "desktop")),
            ),
            (
                "cli only",
                Ok(None),
                Some("cli"),
                Some((BackendSource::Cli, "cli")),
            ),
            ("neither", Ok(None), None, None),
            (
                "package query failed",
                Err(AppError::Windows("query failed".into())),
                Some("cli"),
                Some((BackendSource::Cli, "cli")),
            ),
            (
                "query failed without cli",
                Err(AppError::Windows("query failed".into())),
                None,
                None,
            ),
        ];
        for (label, desktop, cli, expected) in cases {
            let mut cli_lookups = 0;
            let result = select_backend(desktop.map(|path| path.map(PathBuf::from)), || {
                cli_lookups += 1;
                cli.map(PathBuf::from)
            });
            match expected {
                Some((source, path)) => {
                    let backend = result.unwrap();
                    assert_eq!(
                        (backend.source, backend.path),
                        (source, PathBuf::from(path)),
                        "{label}"
                    );
                    assert_eq!(
                        cli_lookups,
                        usize::from(source == BackendSource::Cli),
                        "{label}"
                    );
                }
                None => assert!(matches!(result, Err(AppError::CliNotFound)), "{label}"),
            }
        }
    }

    #[test]
    fn cli_search_preserves_install_and_path_order() {
        let directory = TestDirectory::new();
        let installed = directory.create_file("Programs/OpenAI/Codex/bin/codex.exe");
        let first = directory.create_file("first/codex.exe");
        let second = directory.create_file("second/codex.exe");
        let path = std::env::join_paths([
            directory.0.join("missing"),
            first.parent().unwrap().to_owned(),
            second.parent().unwrap().to_owned(),
        ])
        .unwrap();
        let cases = [
            (
                "installed wins",
                Some(directory.0.as_path()),
                Some(path.as_os_str()),
                Some(installed.clone()),
            ),
            (
                "first path entry",
                None,
                Some(path.as_os_str()),
                Some(first),
            ),
            (
                "installed without path",
                Some(directory.0.as_path()),
                None,
                Some(installed),
            ),
            ("no locations", None, None, None),
        ];
        for (label, local_app_data, path, expected) in cases {
            assert_eq!(find_cli_in(local_app_data, path), expected, "{label}");
        }
    }

    #[test]
    fn desktop_packages_use_numeric_version_order() {
        let directory = TestDirectory::new();
        let older = "OpenAI.Codex_26.9.100.0_x64__2p2nqsd0c76g0";
        let newer = "OpenAI.Codex_26.100.1.0_x64__2p2nqsd0c76g0";
        directory.create_file(&format!("{older}/app/resources/codex.exe"));
        let expected = directory.create_file(&format!("{newer}/app/resources/codex.exe"));
        let packages = vec![older.into(), newer.into()];
        let result = desktop_backend_in(packages, |name| Ok(directory.0.join(name))).unwrap();
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn missing_desktop_binary_allows_cli_fallback() {
        let directory = TestDirectory::new();
        let desktop = desktop_backend_in(
            vec!["OpenAI.Codex_26.100.1.0_x64__2p2nqsd0c76g0".into()],
            |_| Ok(directory.0.clone()),
        );
        let cli = directory.create_file("cli/codex.exe");
        let backend = select_backend(desktop, || Some(cli.clone())).unwrap();
        assert_eq!((backend.source, backend.path), (BackendSource::Cli, cli));
    }

    #[test]
    fn desktop_lookup_observes_binary_updates() {
        let directory = TestDirectory::new();
        let older = "OpenAI.Codex_26.9.100.0_x64__2p2nqsd0c76g0";
        let newer = "OpenAI.Codex_26.100.1.0_x64__2p2nqsd0c76g0";
        let expected = directory.create_file(&format!("{older}/app/resources/codex.exe"));
        let packages = vec![older.into(), newer.into()];
        let result =
            desktop_backend_in(packages.clone(), |name| Ok(directory.0.join(name))).unwrap();
        assert_eq!(result, Some(expected));

        let updated = directory.create_file(&format!("{newer}/app/resources/codex.exe"));
        let result = desktop_backend_in(packages, |name| Ok(directory.0.join(name))).unwrap();
        assert_eq!(result, Some(updated));
    }
}
