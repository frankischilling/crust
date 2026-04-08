use semver::Version;
use serde::Deserialize;

#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::path::{Path, PathBuf};

const RELEASE_API_URL: &str = "https://api.github.com/repos/frankischilling/crust/releases/latest";
const GITHUB_ACCEPT_HEADER: &str = "application/vnd.github+json";

#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub version: String,
    pub tag_name: String,
    pub release_url: String,
    pub asset_name: String,
    pub asset_download_url: String,
    pub asset_sha256: String,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone)]
pub enum UpdateCheckOutcome {
    UpToDate { current_version: String },
    UpdateAvailable(AvailableUpdate),
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

pub fn platform_supported() -> bool {
    #[cfg(target_os = "windows")]
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        return is_debian_like_linux();
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn install_update(_update: &AvailableUpdate, _current_pid: u32) -> Result<(), String> {
    Err(
        "auto-update install is only supported on Windows and Debian-based Linux distributions"
            .to_owned(),
    )
}

#[cfg(target_os = "windows")]
pub async fn install_update(update: &AvailableUpdate, current_pid: u32) -> Result<(), String> {
    let staged = stage_update(update).await?;
    launch_installer(&staged, current_pid)
}

#[cfg(target_os = "linux")]
pub async fn install_update(update: &AvailableUpdate, _current_pid: u32) -> Result<(), String> {
    if !is_debian_like_linux() {
        return Err(
            "auto-update install is only supported on Debian-based Linux distributions"
                .to_owned(),
        );
    }

    let staged = stage_debian_update(update).await?;
    launch_debian_installer(&staged)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn check_for_update(current_version: &str) -> Result<UpdateCheckOutcome, String> {
    let current = normalize_version(current_version)?;
    Ok(UpdateCheckOutcome::UpToDate {
        current_version: current.to_string(),
    })
}

#[cfg(target_os = "windows")]
pub async fn check_for_update(current_version: &str) -> Result<UpdateCheckOutcome, String> {
    let current = normalize_version(current_version)?;
    let release = fetch_latest_release().await?;

    if release.draft {
        return Err("latest release is a draft".to_owned());
    }
    if release.prerelease {
        return Err("latest release is a prerelease".to_owned());
    }

    let latest = normalize_version(&release.tag_name)?;
    if latest <= current {
        return Ok(UpdateCheckOutcome::UpToDate {
            current_version: current.to_string(),
        });
    }

    let asset = select_windows_asset(&release.assets)
        .ok_or_else(|| "no Windows x64 zip asset found in latest release".to_owned())?;

    let sha256 = parse_sha256_digest(asset.digest.as_deref())
        .ok_or_else(|| "latest release asset is missing a valid sha256 digest".to_owned())?;

    Ok(UpdateCheckOutcome::UpdateAvailable(AvailableUpdate {
        version: latest.to_string(),
        tag_name: release.tag_name,
        release_url: release.html_url,
        asset_name: asset.name.clone(),
        asset_download_url: asset.browser_download_url.clone(),
        asset_sha256: sha256.to_owned(),
        published_at: release.published_at,
    }))
}

#[cfg(target_os = "linux")]
pub async fn check_for_update(current_version: &str) -> Result<UpdateCheckOutcome, String> {
    if !is_debian_like_linux() {
        return Err(
            "auto-update checks are only supported on Debian-based Linux distributions"
                .to_owned(),
        );
    }

    let current = normalize_version(current_version)?;
    let release = fetch_latest_release().await?;

    if release.draft {
        return Err("latest release is a draft".to_owned());
    }
    if release.prerelease {
        return Err("latest release is a prerelease".to_owned());
    }

    let latest = normalize_version(&release.tag_name)?;
    if latest <= current {
        return Ok(UpdateCheckOutcome::UpToDate {
            current_version: current.to_string(),
        });
    }

    let asset = select_debian_asset(&release.assets)
        .ok_or_else(|| "no Debian .deb asset found for this Linux architecture".to_owned())?;

    let sha256 = parse_sha256_digest(asset.digest.as_deref())
        .ok_or_else(|| "latest release asset is missing a valid sha256 digest".to_owned())?;

    Ok(UpdateCheckOutcome::UpdateAvailable(AvailableUpdate {
        version: latest.to_string(),
        tag_name: release.tag_name,
        release_url: release.html_url,
        asset_name: asset.name.clone(),
        asset_download_url: asset.browser_download_url.clone(),
        asset_sha256: sha256.to_owned(),
        published_at: release.published_at,
    }))
}

async fn fetch_latest_release() -> Result<GithubRelease, String> {
    use std::time::Duration;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("failed to build update HTTP client: {e}"))?;

    let response = client
        .get(RELEASE_API_URL)
        .header(reqwest::header::USER_AGENT, format!("crust/{}", env!("CARGO_PKG_VERSION")))
        .header(reqwest::header::ACCEPT, GITHUB_ACCEPT_HEADER)
        .send()
        .await
        .map_err(|e| format!("failed to query GitHub releases: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("GitHub releases API returned {}", response.status()));
    }

    response
        .json()
        .await
        .map_err(|e| format!("failed to parse GitHub release payload: {e}"))
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
struct StagedUpdate {
    target_exe: PathBuf,
    staged_exe: PathBuf,
    installer_script: PathBuf,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct StagedDebianUpdate {
    package_path: PathBuf,
}

#[cfg(target_os = "windows")]
async fn stage_update(update: &AvailableUpdate) -> Result<StagedUpdate, String> {
    let stage_dir = make_stage_dir(&update.version).await?;
    let archive_path = stage_dir.join("release.zip");
    let extract_dir = stage_dir.join("extracted");
    let installer_script = stage_dir.join("apply_update.ps1");

    download_asset(update, &archive_path).await?;

    let actual_sha = sha256_hex(&archive_path).await?;
    if !actual_sha.eq_ignore_ascii_case(update.asset_sha256.as_str()) {
        return Err(format!(
            "sha256 mismatch for downloaded update (expected {}, got {})",
            update.asset_sha256, actual_sha
        ));
    }

    extract_zip_archive(&archive_path, &extract_dir).await?;
    let staged_exe = find_named_file(&extract_dir, "crust.exe")
        .ok_or_else(|| "extracted update does not contain crust.exe".to_owned())?;
    let target_exe = std::env::current_exe()
        .map_err(|e| format!("failed to resolve current executable path: {e}"))?;

    write_installer_script(&installer_script)?;

    Ok(StagedUpdate {
        target_exe,
        staged_exe,
        installer_script,
    })
}

#[cfg(target_os = "linux")]
async fn stage_debian_update(update: &AvailableUpdate) -> Result<StagedDebianUpdate, String> {
    let stage_dir = make_stage_dir(&update.version).await?;
    let package_path = stage_dir.join("release.deb");

    download_asset(update, &package_path).await?;

    let actual_sha = sha256_hex(&package_path).await?;
    if !actual_sha.eq_ignore_ascii_case(update.asset_sha256.as_str()) {
        return Err(format!(
            "sha256 mismatch for downloaded update (expected {}, got {})",
            update.asset_sha256, actual_sha
        ));
    }

    Ok(StagedDebianUpdate { package_path })
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
async fn make_stage_dir(version: &str) -> Result<PathBuf, String> {
    let project_dirs = directories::ProjectDirs::from("dev", "crust", "crust")
        .ok_or_else(|| "failed to resolve platform update directory".to_owned())?;
    let root = project_dirs.data_local_dir().join("updater");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| format!("failed to create updater root directory: {e}"))?;

    let sanitized_version = version
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stage_dir = root.join(format!("{}-{}", sanitized_version, ts));
    tokio::fs::create_dir_all(&stage_dir)
        .await
        .map_err(|e| format!("failed to create update staging directory: {e}"))?;
    Ok(stage_dir)
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
async fn download_asset(update: &AvailableUpdate, archive_path: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| format!("failed to build updater download client: {e}"))?;

    let mut response = client
        .get(&update.asset_download_url)
        .header(
            reqwest::header::USER_AGENT,
            format!("crust/{}", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(|e| format!("failed to download update asset: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "update asset download returned {}",
            response.status()
        ));
    }

    let mut file = tokio::fs::File::create(archive_path)
        .await
        .map_err(|e| format!("failed to create staged archive file: {e}"))?;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("failed while downloading update asset: {e}"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("failed to write update archive chunk: {e}"))?;
    }

    file.flush()
        .await
        .map_err(|e| format!("failed to flush update archive file: {e}"))?;
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
async fn sha256_hex(path: &Path) -> Result<String, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        use std::io::Read;

        let mut file = std::fs::File::open(&path)
            .map_err(|e| format!("failed to open update archive for hashing: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buf)
                .map_err(|e| format!("failed while hashing update archive: {e}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        Ok::<String, String>(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| format!("update hash task failed: {e}"))?
}

#[cfg(target_os = "windows")]
async fn extract_zip_archive(zip_path: &Path, extract_dir: &Path) -> Result<(), String> {
    let zip_path = zip_path.to_path_buf();
    let extract_dir = extract_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&extract_dir)
            .map_err(|e| format!("failed to create extraction directory: {e}"))?;

        let file = std::fs::File::open(&zip_path)
            .map_err(|e| format!("failed to open downloaded update archive: {e}"))?;
        let mut archive = zip::read::ZipArchive::new(file)
            .map_err(|e| format!("failed to read update zip archive: {e}"))?;

        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|e| format!("failed to read zip entry: {e}"))?;
            let Some(rel_path) = entry.enclosed_name().map(|p| p.to_owned()) else {
                continue;
            };
            let out_path = extract_dir.join(rel_path);

            if entry.name().ends_with('/') {
                std::fs::create_dir_all(&out_path)
                    .map_err(|e| format!("failed to create extracted directory: {e}"))?;
                continue;
            }

            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create extracted parent directory: {e}"))?;
            }

            let mut out_file = std::fs::File::create(&out_path)
                .map_err(|e| format!("failed to create extracted file: {e}"))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("failed to extract zip entry: {e}"))?;
        }

        Ok::<(), String>(())
    })
    .await
    .map_err(|e| format!("update extraction task failed: {e}"))?
}

#[cfg(target_os = "windows")]
fn find_named_file(root: &Path, file_name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read_dir = std::fs::read_dir(&dir).ok()?;
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().and_then(OsStr::to_str)?;
            if name.eq_ignore_ascii_case(file_name) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn write_installer_script(path: &Path) -> Result<(), String> {
    const SCRIPT: &str = r#"
param(
    [Parameter(Mandatory=$true)][int]$PidToWait,
    [Parameter(Mandatory=$true)][string]$TargetExe,
    [Parameter(Mandatory=$true)][string]$StagedExe
)

$ErrorActionPreference = 'Stop'
$backupExe = "$TargetExe.bak"

for ($i = 0; $i -lt 480; $i++) {
    if (-not (Get-Process -Id $PidToWait -ErrorAction SilentlyContinue)) {
        break
    }
    Start-Sleep -Milliseconds 250
}

for ($attempt = 0; $attempt -lt 40; $attempt++) {
    try {
        if (Test-Path -LiteralPath $backupExe) {
            Remove-Item -LiteralPath $backupExe -Force
        }
        if (Test-Path -LiteralPath $TargetExe) {
            Move-Item -LiteralPath $TargetExe -Destination $backupExe -Force
        }
        Move-Item -LiteralPath $StagedExe -Destination $TargetExe -Force
        Start-Process -FilePath $TargetExe | Out-Null
        if (Test-Path -LiteralPath $backupExe) {
            Remove-Item -LiteralPath $backupExe -Force -ErrorAction SilentlyContinue
        }
        exit 0
    }
    catch {
        Start-Sleep -Milliseconds 250
    }
}

if ((Test-Path -LiteralPath $backupExe) -and -not (Test-Path -LiteralPath $TargetExe)) {
    Move-Item -LiteralPath $backupExe -Destination $TargetExe -Force
}

exit 1
"#;

    std::fs::write(path, SCRIPT).map_err(|e| format!("failed to write installer script: {e}"))
}

#[cfg(target_os = "windows")]
fn launch_installer(staged: &StagedUpdate, current_pid: u32) -> Result<(), String> {
    let candidates = powershell_candidates();

    for candidate in &candidates {
        let result = std::process::Command::new(candidate)
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&staged.installer_script)
            .arg("-PidToWait")
            .arg(current_pid.to_string())
            .arg("-TargetExe")
            .arg(staged.target_exe.as_os_str())
            .arg("-StagedExe")
            .arg(staged.staged_exe.as_os_str())
            .spawn();

        match result {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(format!(
                    "failed to launch updater installer process via '{}': {err}",
                    candidate
                ))
            }
        }
    }

    Err(format!(
        "failed to launch updater installer process: no PowerShell executable found (tried: {})",
        candidates.join(", ")
    ))
}

#[cfg(target_os = "windows")]
fn powershell_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    if let Some(system_root) = std::env::var("SystemRoot")
        .ok()
        .or_else(|| std::env::var("WINDIR").ok())
    {
        let win_ps = Path::new(&system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if win_ps.exists() {
            candidates.push(win_ps.to_string_lossy().into_owned());
        }
    }

    candidates.push("powershell.exe".to_owned());
    candidates.push("powershell".to_owned());
    candidates.push("pwsh.exe".to_owned());
    candidates.push("pwsh".to_owned());

    candidates.dedup();
    candidates
}

#[cfg(target_os = "linux")]
fn launch_debian_installer(staged: &StagedDebianUpdate) -> Result<(), String> {
    let package = staged.package_path.as_os_str();

    let xdg_result = std::process::Command::new("xdg-open").arg(package).spawn();
    match xdg_result {
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to launch Debian installer via 'xdg-open': {err}"
            ))
        }
    }

    let gio_result = std::process::Command::new("gio")
        .arg("open")
        .arg(package)
        .spawn();
    match gio_result {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(
            "failed to launch Debian installer: no desktop opener found (tried: xdg-open, gio open)"
                .to_owned(),
        ),
        Err(err) => Err(format!(
            "failed to launch Debian installer via 'gio open': {err}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn is_debian_like_linux() -> bool {
    let content = match std::fs::read_to_string("/etc/os-release") {
        Ok(value) => value,
        Err(_) => return false,
    };

    let (id, id_like) = parse_os_release_id_fields(&content);
    is_debian_family_distro(&id, &id_like)
}

#[cfg(any(target_os = "linux", test))]
fn parse_os_release_id_fields(content: &str) -> (String, String) {
    let mut id = String::new();
    let mut id_like = String::new();

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("ID=") {
            id = value.trim().trim_matches('"').to_ascii_lowercase();
            continue;
        }
        if let Some(value) = line.strip_prefix("ID_LIKE=") {
            id_like = value.trim().trim_matches('"').to_ascii_lowercase();
        }
    }

    (id, id_like)
}

#[cfg(any(target_os = "linux", test))]
fn is_debian_family_distro(id: &str, id_like: &str) -> bool {
    const KNOWN_DEBIAN_DERIVATIVE_IDS: &[&str] = &[
        "debian",
        "ubuntu",
        "linuxmint",
        "pop",
        "kali",
        "raspbian",
        "neon",
        "elementary",
        "zorin",
        "mx",
        "deepin",
        "devuan",
        "parrot",
        "pureos",
        "peppermint",
    ];

    let matches = |value: &str| {
        value
            .split(|ch: char| ch.is_ascii_whitespace() || ch == ',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(|token| token.trim_matches('"').to_ascii_lowercase())
            .any(|token| {
                token == "debian"
                    || token == "ubuntu"
                    || KNOWN_DEBIAN_DERIVATIVE_IDS
                        .iter()
                        .any(|known| *known == token)
            })
    };

    matches(&id) || matches(&id_like)
}

fn normalize_version(raw: &str) -> Result<Version, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty version string".to_owned());
    }
    let normalized = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    Version::parse(normalized).map_err(|e| format!("invalid semantic version '{raw}': {e}"))
}

#[cfg(any(target_os = "windows", test))]
fn select_windows_asset(assets: &[GithubReleaseAsset]) -> Option<&GithubReleaseAsset> {
    assets
        .iter()
        .filter(|asset| is_windows_x64_zip_asset_name(&asset.name))
        .min_by_key(|asset| asset.name.to_ascii_lowercase())
}

fn select_debian_asset(assets: &[GithubReleaseAsset]) -> Option<&GithubReleaseAsset> {
    let arch_candidates = debian_arch_candidates();
    assets
        .iter()
        .filter(|asset| {
            arch_candidates
                .iter()
                .any(|arch| is_debian_arch_asset_name(&asset.name, arch))
        })
        .min_by_key(|asset| asset.name.to_ascii_lowercase())
}

#[cfg(any(target_os = "windows", test))]
fn is_windows_x64_zip_asset_name(asset_name: &str) -> bool {
    let name = asset_name.to_ascii_lowercase();
    name.ends_with(".zip") && name.contains("windows") && name.contains("x64")
}

fn is_debian_arch_asset_name(asset_name: &str, arch: &str) -> bool {
    let name = asset_name.to_ascii_lowercase();
    let arch = arch.to_ascii_lowercase();
    name.ends_with(&format!("-debian-{arch}.deb"))
}

fn debian_arch_candidates() -> &'static [&'static str] {
    match std::env::consts::ARCH {
        "x86_64" => &["amd64", "x86_64"],
        "aarch64" => &["arm64", "aarch64"],
        "arm" => &["armhf", "arm"],
        "x86" => &["i386", "x86"],
        _ => &[std::env::consts::ARCH],
    }
}

fn parse_sha256_digest(digest: Option<&str>) -> Option<&str> {
    let value = digest?.strip_prefix("sha256:")?;
    if value.len() != 64 {
        return None;
    }
    if !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_with_or_without_prefix() {
        assert_eq!(
            normalize_version("v0.3.0").expect("v-prefixed semver should parse"),
            Version::new(0, 3, 0)
        );
        assert_eq!(
            normalize_version("0.3.0").expect("plain semver should parse"),
            Version::new(0, 3, 0)
        );
    }

    #[test]
    fn reject_invalid_digest() {
        assert!(parse_sha256_digest(Some("sha256:xyz")).is_none());
        assert!(parse_sha256_digest(Some("sha256:1234")).is_none());
        assert!(parse_sha256_digest(None).is_none());
    }

    #[test]
    fn windows_asset_detection_is_case_insensitive() {
        assert!(is_windows_x64_zip_asset_name("crust-v0.3.0-windows-x64.zip"));
        assert!(is_windows_x64_zip_asset_name("CRUST-V0.3.0-WINDOWS-X64.ZIP"));
        assert!(!is_windows_x64_zip_asset_name("crust-v0.3.0-linux-x64.tar.gz"));
        assert!(!is_windows_x64_zip_asset_name("source.zip"));
    }

    #[test]
    fn debian_asset_detection_matches_expected_pattern() {
        assert!(is_debian_arch_asset_name(
            "crust-v0.4.3-debian-amd64.deb",
            "amd64"
        ));
        assert!(is_debian_arch_asset_name(
            "CRUST-V0.4.3-DEBIAN-ARM64.DEB",
            "arm64"
        ));
        assert!(!is_debian_arch_asset_name(
            "crust-v0.4.3-linux-amd64.tar.gz",
            "amd64"
        ));
        assert!(!is_debian_arch_asset_name("source.zip", "amd64"));
    }

    #[test]
    fn debian_family_detection_accepts_derivative_ids() {
        assert!(is_debian_family_distro("linuxmint", ""));
        assert!(is_debian_family_distro("kali", ""));
        assert!(is_debian_family_distro("pop", ""));
    }

    #[test]
    fn debian_family_detection_accepts_id_like_tokens() {
        assert!(is_debian_family_distro("zorin", "ubuntu debian"));
        assert!(is_debian_family_distro("some-distro", "debian"));
        assert!(is_debian_family_distro("some-distro", "ubuntu,debian"));
    }

    #[test]
    fn debian_family_detection_rejects_non_debian_distros() {
        assert!(!is_debian_family_distro("fedora", "rhel"));
        assert!(!is_debian_family_distro("arch", ""));
    }

    #[test]
    fn os_release_parser_reads_id_fields() {
        let payload = r#"
NAME="Ubuntu"
ID=ubuntu
ID_LIKE="debian"
"#;
        let (id, id_like) = parse_os_release_id_fields(payload);
        assert_eq!(id, "ubuntu");
        assert_eq!(id_like, "debian");
    }

    #[test]
    fn digest_requires_sha256_prefix_and_hex() {
        let ok = format!("sha256:{}", "a".repeat(64));
        assert_eq!(parse_sha256_digest(Some(ok.as_str())), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(parse_sha256_digest(Some("SHA256:abc")).is_none());
        assert!(parse_sha256_digest(Some("sha1:abc")).is_none());
    }
}
