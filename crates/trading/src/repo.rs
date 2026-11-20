//! Systems kept in a git repository: a sparse, read-only copy of just the
//! folder that holds them (Settings → Sistemas). Git downloads only that
//! folder's files (`--filter=blob:none` plus a cone sparse checkout); Aurea
//! never commits or pushes. What the bots write in their folders
//! (`registro.log`, `panel.pid`, …) stays out of git.

use std::path::{Path, PathBuf};
use std::process::Command;

use zeron_proto::trading::{RepoSource, RepoSyncReport};

use crate::pool;

/// Where the copy lives under the desk's data folder.
pub fn checkout_dir(trading_dir: &Path) -> PathBuf {
    trading_dir.join("repo")
}

/// Files the bots write next to their code: never tracked, so an update can
/// neither overwrite them nor stop on them.
const LOCAL_ONLY: &[&str] = &["registro.log", "*.log", "panel.pid", "*.pid", "__pycache__/"];

/// Checks what a person typed before anything runs git with it.
pub fn validate(source: &RepoSource) -> Result<RepoSource, String> {
    let url = source.url.trim().trim_end_matches('/').to_string();
    let remote = url.starts_with("https://") || url.starts_with("git@") || url.starts_with("ssh://");
    if !remote || url.chars().any(|c| c.is_whitespace() || c.is_control()) || url.starts_with('-') {
        return Err("La URL del repositorio debe empezar por https://, git@ o ssh://".into());
    }
    let branch = source
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_string);
    if let Some(branch) = &branch
        && (branch.starts_with('-')
            || branch.contains("..")
            || branch.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
    {
        return Err(format!("Nombre de rama no válido: {branch}"));
    }
    let subdir = source
        .subdir
        .trim()
        .replace('\\', "/")
        .trim_matches('/')
        .to_string();
    if subdir
        .split('/')
        .any(|part| part == ".." || part == "." || part.starts_with('-'))
        || subdir.contains(':')
        || subdir.chars().any(char::is_control)
    {
        return Err("La carpeta debe ser una ruta dentro del repositorio, como bots/sistemas".into());
    }
    Ok(RepoSource { url, branch, subdir })
}

/// The local systems folder a source provides.
pub fn systems_dir(trading_dir: &Path, source: &RepoSource) -> PathBuf {
    let mut dir = checkout_dir(trading_dir);
    for part in source.subdir.split('/').filter(|part| !part.is_empty()) {
        dir.push(part);
    }
    dir
}

/// Download (first time, or when the source changed) or update the copy, and
/// report what it brought. Blocking: runs git.
pub fn sync(trading_dir: &Path, source: &RepoSource) -> Result<RepoSyncReport, String> {
    sync_validated(trading_dir, &validate(source)?)
}

fn sync_validated(trading_dir: &Path, source: &RepoSource) -> Result<RepoSyncReport, String> {
    let source = source.clone();
    let checkout = checkout_dir(trading_dir);
    let reusable = checkout.join(".git").is_dir()
        && git(&checkout, &["remote", "get-url", "origin"]).is_ok_and(|url| url.trim() == source.url)
        && current_branch_matches(&checkout, source.branch.as_deref());
    let changed = if reusable {
        let before = git(&checkout, &["rev-parse", "HEAD"])?.trim().to_string();
        sparse_set(&checkout, &source.subdir)?;
        git(&checkout, &["pull", "--ff-only", "--quiet"])?;
        let after = git(&checkout, &["rev-parse", "HEAD"])?.trim().to_string();
        if before == after {
            Vec::new()
        } else {
            let mut args = vec!["diff", "--name-only", before.as_str(), after.as_str()];
            if !source.subdir.is_empty() {
                args.extend(["--", source.subdir.as_str()]);
            }
            git(&checkout, &args)?
                .lines()
                .map(str::to_string)
                .filter(|line| !line.is_empty())
                .collect()
        }
    } else {
        clone(trading_dir, &checkout, &source)?;
        Vec::new()
    };
    let dir = systems_dir(trading_dir, &source);
    if !dir.is_dir() {
        return Err(format!(
            "La carpeta «{}» no existe en el repositorio",
            if source.subdir.is_empty() { "/" } else { &source.subdir }
        ));
    }
    let systems = pool::scan_dirs(&dir).len() as u32;
    if systems == 0 {
        return Err(format!(
            "No hay sistemas (carpetas NN_nombre) en «{}»",
            if source.subdir.is_empty() { "/" } else { &source.subdir }
        ));
    }
    Ok(RepoSyncReport {
        dir: dir.to_string_lossy().into_owned(),
        systems,
        commit: git(&checkout, &["rev-parse", "--short", "HEAD"])?.trim().to_string(),
        changed,
        at: chrono::Utc::now().timestamp_millis(),
    })
}

/// A fresh sparse copy, built aside and swapped in only once it worked, so a
/// failed download leaves the previous copy usable.
fn clone(trading_dir: &Path, checkout: &Path, source: &RepoSource) -> Result<(), String> {
    std::fs::create_dir_all(trading_dir).map_err(|e| e.to_string())?;
    let staging = trading_dir.join("repo.incoming");
    let _ = std::fs::remove_dir_all(&staging);
    let staging_arg = staging.to_string_lossy().into_owned();
    let mut args = vec!["clone", "--filter=blob:none", "--no-checkout", "--quiet"];
    if let Some(branch) = &source.branch {
        args.extend(["--branch", branch.as_str()]);
    }
    args.extend(["--", source.url.as_str(), staging_arg.as_str()]);
    let built = git(trading_dir, &args)
        .and_then(|_| sparse_set(&staging, &source.subdir))
        .and_then(|_| git(&staging, &["checkout", "--quiet"]).map(|_| ()))
        .and_then(|_| exclude_local_files(&staging));
    if let Err(error) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if checkout.exists() {
        std::fs::remove_dir_all(checkout).map_err(|e| {
            format!("No se pudo reemplazar la copia anterior (¿hay un sistema en marcha?): {e}")
        })?;
    }
    std::fs::rename(&staging, checkout).map_err(|e| e.to_string())
}

fn sparse_set(checkout: &Path, subdir: &str) -> Result<(), String> {
    if subdir.is_empty() {
        git(checkout, &["sparse-checkout", "disable"]).map(|_| ())
    } else {
        git(checkout, &["sparse-checkout", "set", "--cone", "--", subdir]).map(|_| ())
    }
}

fn current_branch_matches(checkout: &Path, branch: Option<&str>) -> bool {
    match branch {
        None => true,
        Some(branch) => git(checkout, &["rev-parse", "--abbrev-ref", "HEAD"])
            .is_ok_and(|head| head.trim() == branch),
    }
}

fn exclude_local_files(checkout: &Path) -> Result<(), String> {
    let exclude = checkout.join(".git").join("info").join("exclude");
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    for pattern in LOCAL_ONLY {
        if !text.lines().any(|line| line.trim() == *pattern) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(pattern);
            text.push('\n');
        }
    }
    std::fs::write(&exclude, text).map_err(|e| e.to_string())
}

/// Run git in `dir` without ever prompting in a terminal (a credential
/// helper such as Git Credential Manager may still ask in its own window).
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Git no está instalado (https://git-scm.com)".to_string()
        } else {
            format!("No se pudo ejecutar git: {error}")
        }
    })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .last()
            .unwrap_or("error desconocido");
        Err(format!("git {}: {detail}", args.first().copied().unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(url: &str, subdir: &str) -> RepoSource {
        RepoSource { url: url.into(), branch: None, subdir: subdir.into() }
    }

    #[test]
    fn only_remote_urls_and_paths_inside_the_repository_pass() {
        let ok = validate(&source(" https://github.com/me/bots/ ", "\\bots\\sistemas\\")).unwrap();
        assert_eq!(ok.url, "https://github.com/me/bots");
        assert_eq!(ok.subdir, "bots/sistemas");
        assert!(validate(&source("git@github.com:me/bots.git", "")).is_ok());
        for url in ["C:\\bots", "file:///c/bots", "--upload-pack=x", "https://a b"] {
            assert!(validate(&source(url, "")).is_err(), "{url}");
        }
        for subdir in ["../fuera", "a/../b", "C:/x", "-x"] {
            assert!(validate(&source("https://github.com/me/bots", subdir)).is_err(), "{subdir}");
        }
        let mut branchy = source("https://github.com/me/bots", "");
        branchy.branch = Some("--orphan".into());
        assert!(validate(&branchy).is_err());
    }

    /// A local repository standing in for GitHub: two systems under
    /// `bots/sistemas`, plus a folder that must not be downloaded.
    fn upstream(root: &Path) -> PathBuf {
        let repo = root.join("upstream");
        std::fs::create_dir_all(repo.join("bots/sistemas/01_uno")).unwrap();
        std::fs::create_dir_all(repo.join("bots/sistemas/02_dos")).unwrap();
        std::fs::create_dir_all(repo.join("otros")).unwrap();
        std::fs::write(repo.join("bots/sistemas/01_uno/bot.py"), "print(1)\n").unwrap();
        std::fs::write(repo.join("bots/sistemas/02_dos/bot.py"), "print(2)\n").unwrap();
        std::fs::write(repo.join("otros/grande.bin"), "no me bajes\n").unwrap();
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "uno"],
            // Partial clones need the server to allow filters.
            vec!["config", "uploadpack.allowFilter", "true"],
        ] {
            git(&repo, &args).unwrap();
        }
        repo
    }

    #[test]
    fn downloads_only_the_folder_and_updates_it() {
        let root = tempfile::tempdir().unwrap();
        let repo = upstream(root.path());
        let trading = root.path().join("trading");
        // file:// so the filter applies as it would against GitHub.
        let url = format!("file:///{}", repo.to_string_lossy().replace('\\', "/").trim_start_matches('/'));
        let src = RepoSource { url, branch: None, subdir: "bots/sistemas".into() };

        let first = sync_validated(&trading, &src).unwrap();
        assert_eq!(first.systems, 2);
        assert!(first.changed.is_empty());
        let copy = checkout_dir(&trading);
        assert!(copy.join("bots/sistemas/01_uno/bot.py").is_file());
        assert!(!copy.join("otros").exists(), "a folder outside the selection was checked out");

        // The bots' own files never block an update.
        std::fs::write(copy.join("bots/sistemas/01_uno/registro.log"), "fila\n").unwrap();
        std::fs::write(repo.join("bots/sistemas/02_dos/bot.py"), "print(22)\n").unwrap();
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-am", "dos"]).unwrap();

        let second = sync_validated(&trading, &src).unwrap();
        assert_eq!(second.changed, vec!["bots/sistemas/02_dos/bot.py".to_string()]);
        // (Windows' git may check it out with CRLF.)
        let updated = std::fs::read_to_string(copy.join("bots/sistemas/02_dos/bot.py")).unwrap();
        assert_eq!(updated.trim_end(), "print(22)");
        assert!(copy.join("bots/sistemas/01_uno/registro.log").is_file());

        let missing = RepoSource { subdir: "no/existe".into(), ..src.clone() };
        assert!(sync_validated(&trading, &missing).is_err());
    }
}
