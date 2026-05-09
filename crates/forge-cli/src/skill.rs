//! `forge skill` subcommands (F-590): install, list, remove.
//!
//! Installs skills from external sources into Forge's on-disk skill scopes,
//! using [`forge_agents::skill_loader::parse_skill_file`] (F-589) as the
//! gatekeeper. Refuses to install if the source `SKILL.md` does not parse.
//!
//! # Sources
//!
//! Two resolver shapes are supported, distinguished by URL prefix at the CLI:
//!
//! - **Local path** (relative or absolute) — copied into the target scope.
//! - **Git URL** (HTTPS or SSH) — cloned to `~/.cache/forge/skills/<sha256>/`,
//!   then copied into the target scope.
//!
//! # Scopes
//!
//! - `user` (default): `<home>/.skills/<id>/` — cross-workspace.
//! - `workspace`: `<cwd>/.skills/<id>/` — per-project, checked into git.
//!
//! Scopes match the layout in `docs/architecture/skills.md`.
//!
//! # Validation
//!
//! Every install runs `parse_skill_file` on the resolved `SKILL.md` *before*
//! anything is written to the target scope. A parse failure aborts the
//! install with the loader error verbatim, so the user sees which field is
//! malformed.
//!
//! # Cache
//!
//! Git resolves clone (or fetch + reset) into `~/.cache/forge/skills/<hash>/`
//! where `<hash>` is the lowercase hex sha256 of the source URL. The cache is
//! the *source*; the install copies a fresh tree out of it. Re-installing the
//! same URL re-uses the cache and runs `git fetch + reset --hard origin/HEAD`
//! to pull the latest commit.

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use forge_agents::skill_loader::{parse_skill_file, SKILL_FILENAME};
use forge_core::Skill;

/// Where an installed skill should land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// `<home>/.skills/<id>/` — visible across every workspace.
    User,
    /// `<cwd>/.skills/<id>/` — per-project, checked into git.
    Workspace,
}

impl SkillScope {
    /// Path that the F-589 loader expects: the *parent* of `.skills/`, not
    /// the `.skills/` directory itself. `load_user_skills` and
    /// `load_workspace_skills` both append `.skills/` internally — passing
    /// them a path that already ends in `.skills` would make them look for
    /// `.skills/.skills/` and silently return empty.
    ///
    /// Pinning this contract by name: if you need the *directory containing
    /// the installed skill folders* (where each skill lives at
    /// `<scope_root>/.skills/<id>/SKILL.md`), call [`Self::skills_dir`]
    /// instead.
    fn scope_root(&self, workspace_root: &Path, home: &Path) -> PathBuf {
        match self {
            SkillScope::User => home.to_path_buf(),
            SkillScope::Workspace => workspace_root.to_path_buf(),
        }
    }

    /// `<scope_root>/.skills/` — the directory whose immediate children are
    /// per-skill folders. Used by install/remove which write directly under
    /// `.skills/`. Distinct from [`Self::scope_root`], which is what the
    /// F-589 loader expects.
    fn skills_dir(&self, workspace_root: &Path, home: &Path) -> PathBuf {
        self.scope_root(workspace_root, home).join(".skills")
    }

    fn label(&self) -> &'static str {
        match self {
            SkillScope::User => "user",
            SkillScope::Workspace => "workspace",
        }
    }
}

impl fmt::Display for SkillScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What [`Resolver::resolve`] hands back: a fully-parsed [`Skill`] together
/// with the on-disk directory containing the `SKILL.md` and any side files.
///
/// `source_dir` is what the install step copies; `skill` is the validated
/// shape used to derive the target folder name (`skill.id`).
///
/// # F-656 — TOCTOU between resolve and install
///
/// `source_dir` is canonicalized at resolve time, but the install step that
/// follows runs later and re-opens the path from a string. Without a pinned
/// fingerprint, an attacker who can write inside `source_dir`'s parent could
/// rename the validated tree away and replace it with a symlink to an
/// attacker-controlled tree between the two calls. `source_fingerprint`
/// records the validated directory's `(dev, ino)` so [`install_resolved`]
/// can detect the substitution and refuse.
#[derive(Debug)]
pub struct ResolvedSkill {
    /// Parsed skill (validated via F-589).
    pub skill: Skill,
    /// Folder containing the `SKILL.md` file. Whatever sits next to it
    /// (`scripts/`, `references/`, etc.) is copied along.
    pub source_dir: PathBuf,
    /// Filesystem identity of `source_dir` at resolve time. The installer
    /// re-stats the path and refuses on mismatch — see F-656.
    pub(crate) source_fingerprint: SourceFingerprint,
}

/// Filesystem identity of a directory at a specific point in time, used to
/// detect TOCTOU substitution between resolve and install (F-656).
///
/// On Unix this is `(st_dev, st_ino)` from `symlink_metadata`. On other
/// platforms the fingerprint degrades to a structural check (the path must
/// still resolve to a real directory, not a symlink) since stable inodes are
/// not portably available through `std`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceFingerprint {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl SourceFingerprint {
    /// Capture the fingerprint of `path`. Caller has already verified
    /// `path` points at a real directory (post-canonicalize).
    fn capture(path: &Path) -> Result<Self> {
        // `symlink_metadata` does not follow a final-component symlink. Since
        // `path` came out of `fs::canonicalize`, no component should be a
        // symlink — but stat-ing without follow is the right primitive here:
        // it pins exactly the inode we validated.
        let meta = fs::symlink_metadata(path)
            .with_context(|| format!("capturing source fingerprint for {}", path.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "source path {} is a symlink after canonicalize — refusing",
                path.display()
            );
        }
        if !meta.is_dir() {
            bail!(
                "source path {} is not a directory — refusing",
                path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Re-stat `path` and verify it still names the directory captured at
    /// resolve time. Returns an error if the path is now missing, a symlink,
    /// not a directory, or a different inode.
    fn verify(&self, path: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(path)
            .with_context(|| format!("re-stating source root {} (TOCTOU check)", path.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "source root {} became a symlink between resolve and install \
                 (TOCTOU; refusing)",
                path.display()
            );
        }
        if !meta.is_dir() {
            bail!(
                "source root {} is no longer a directory (TOCTOU; refusing)",
                path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.dev() != self.dev || meta.ino() != self.ino {
                bail!(
                    "source root {} dev/ino changed between resolve and install \
                     (TOCTOU; refusing)",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

/// Anything that produces a [`ResolvedSkill`] from a CLI-supplied source.
///
/// Resolvers own validation: by the time `resolve` returns `Ok`, the skill
/// has parsed cleanly and the install step can run without further checks.
pub trait Resolver {
    fn resolve(&self) -> Result<ResolvedSkill>;
}

/// Resolves a local-path source — relative paths are anchored at the
/// caller-supplied CWD; absolute paths are taken as-is.
///
/// Path canonicalization runs through [`fs::canonicalize`] which resolves
/// symlinks and normalizes `..` segments. Symlink loops surface as a
/// canonicalize IO error and are treated as a refusal.
///
/// The install step that follows (see [`install_resolved`]) additionally
/// refuses to copy any symlink inside the resolved directory whose target
/// escapes the source root, so a hostile skill folder cannot smuggle a
/// link to e.g. `/etc/passwd` into the installed scope.
pub struct LocalPathResolver {
    pub source: PathBuf,
    pub cwd: PathBuf,
}

impl LocalPathResolver {
    pub fn new(source: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            cwd: cwd.into(),
        }
    }
}

impl Resolver for LocalPathResolver {
    fn resolve(&self) -> Result<ResolvedSkill> {
        let raw = if self.source.is_absolute() {
            self.source.clone()
        } else {
            self.cwd.join(&self.source)
        };

        let canonical = fs::canonicalize(&raw)
            .with_context(|| format!("resolving local skill path {}", raw.display()))?;

        let metadata = fs::metadata(&canonical)
            .with_context(|| format!("statting {}", canonical.display()))?;
        if !metadata.is_dir() {
            bail!(
                "local skill source must be a directory containing {SKILL_FILENAME}: {}",
                canonical.display()
            );
        }

        let skill_md = canonical.join(SKILL_FILENAME);
        if !skill_md.exists() {
            bail!("no {SKILL_FILENAME} found in {}", canonical.display());
        }

        // F-589 is the gatekeeper — refuse on parse failure.
        let skill = parse_skill_file(&skill_md)
            .map_err(|e| anyhow!("skill at {} failed validation: {e}", skill_md.display()))?;

        // F-656: pin (dev, ino) so `install_resolved` can detect a swap of
        // the source root between now and the copy.
        let source_fingerprint = SourceFingerprint::capture(&canonical)?;

        Ok(ResolvedSkill {
            skill,
            source_dir: canonical,
            source_fingerprint,
        })
    }
}

/// Abstraction over `git` invocation so unit tests can supply a fake
/// runner. Mirrors the `CommandRunner` pattern used in F-595 (forge-oci).
pub trait CommandRunner {
    /// Run `program` with `args` in `cwd`. Returns Ok on exit-status success;
    /// otherwise an error describing the failed command.
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()>;
}

/// Default [`CommandRunner`] that shells out via `std::process::Command`.
///
/// # F-665 — Hardened git invocation environment
///
/// Every spawn goes through three layers of hardening:
///
/// 1. **Env scrub.** The parent process environment is *not* inherited.
///    Only an allowlist of well-known vars (`PATH`, `HOME`, `LANG`, `LC_*`,
///    `SSH_AUTH_SOCK`, `TERM`) passes through; everything else — including
///    every `GIT_*` the parent might be carrying (`GIT_TRACE`, malicious
///    `GIT_SSH_COMMAND`, `GIT_CONFIG_NOSYSTEM=0`, etc.) — is dropped before
///    Forge sets its own. This blocks both data exfiltration (a hostile
///    parent enabling git's tracing to leak credentials) and behavior
///    redirection (a parent overriding our SSH/system-config policy).
///
/// 2. **Forge-controlled git knobs.** After scrubbing, we set
///    `GIT_TERMINAL_PROMPT=0` (no stdin credential prompts that would hang
///    a non-TTY install), `GIT_SSH_COMMAND` with `BatchMode=yes` and
///    `StrictHostKeyChecking=accept-new` (never wait on TTY confirmation,
///    enforce TOFU instead of blanket-accepting host keys), and
///    `GIT_CONFIG_NOSYSTEM=1` (ignore `/etc/gitconfig` so a tampered
///    system config cannot redirect the clone).
///
/// 3. **Wall-clock timeout.** Each spawn runs under a finite deadline (60s
///    by default). A non-responsive remote that would otherwise hang
///    `git clone` forever is killed and surfaces as an error so the CLI
///    can return control to the user.
pub struct StdCommandRunner {
    timeout: Duration,
}

/// Names of parent-process env vars the runner allows through to the child.
/// Anything not on this list is dropped. `GIT_*` is intentionally absent —
/// Forge controls git's environment exclusively (see field docs above).
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TERM",
    // SSH-based clones depend on the agent socket; dropping this would
    // break legitimate dev flows that rely on `ssh-agent`.
    "SSH_AUTH_SOCK",
    // `LC_*` is a family — handled by prefix below in addition to this
    // umbrella entry.
    "LC_ALL",
];

/// Locked-down SSH options for `GIT_SSH_COMMAND`:
/// - `BatchMode=yes` — never prompt on the TTY (no passphrase / yes-no).
/// - `StrictHostKeyChecking=accept-new` — TOFU: trust on first use, then
///   pin. This is stricter than `no` (which accepts any key silently) and
///   safer than `yes` (which would refuse first-time hosts and break
///   legitimate clones).
/// - `ConnectTimeout=5` — fail fast on unresponsive endpoints; the outer
///   wall-clock timeout still backstops this.
const HARDENED_SSH_COMMAND: &str =
    "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5";

/// Default per-spawn timeout. Sized so a healthy network call to a
/// well-known host completes comfortably; an unresponsive endpoint trips
/// the kill path long before a human user notices.
const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(60);

impl StdCommandRunner {
    /// Construct a runner with the default 60s spawn timeout.
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
        }
    }

    /// Construct a runner with a custom spawn timeout. Primarily for tests
    /// that need the watchdog to fire quickly.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Per-spawn wall-clock timeout currently in effect. Exposed so callers
    /// (and tests) can confirm a finite, non-zero bound is configured.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Default for StdCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply the F-665 hardened environment to `cmd`: scrub parent env down to
/// the allowlist, then set Forge-controlled git knobs. Pulled out of the
/// trait impl so the policy lives in one place — anyone adding a new
/// runner gets the same hardening for free by calling this.
fn apply_hardened_env(cmd: &mut Command) {
    cmd.env_clear();
    for (key, value) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            // Non-UTF8 names are not on the allowlist; drop them.
            continue;
        };
        if ENV_ALLOWLIST.contains(&name) || name.starts_with("LC_") {
            cmd.env(key, value);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_SSH_COMMAND", HARDENED_SSH_COMMAND);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
}

impl CommandRunner for StdCommandRunner {
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        apply_hardened_env(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {program} {}", args.join(" ")))?;

        // Watchdog: poll `try_wait` until the child exits or the deadline
        // passes. Polling is simpler than a second thread + channel and
        // avoids the join overhead — granularity of 50ms is fine because
        // the timeout is measured in seconds.
        let deadline = std::time::Instant::now() + self.timeout;
        let poll = Duration::from_millis(50);
        let status = loop {
            match child
                .try_wait()
                .with_context(|| format!("waiting on {program} {}", args.join(" ")))?
            {
                Some(s) => break s,
                None => {
                    if std::time::Instant::now() >= deadline {
                        // Best-effort kill; if the kill itself fails the
                        // child is likely already gone, so we still want to
                        // surface the timeout, not the kill error.
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!(
                            "{program} {} timed out after {:?}",
                            args.join(" "),
                            self.timeout,
                        );
                    }
                    std::thread::sleep(poll);
                }
            }
        };

        if !status.success() {
            bail!("{program} {} failed with status {status}", args.join(" "));
        }
        Ok(())
    }
}

/// Resolves a git source: clones (or fetch + reset) into the cache, then
/// returns the cached working tree as the source for install.
///
/// The cache directory is `<cache_root>/<sha256(url)>/`. Re-using the same
/// URL is idempotent: a cache hit triggers `git fetch origin` followed by
/// `git reset --hard origin/HEAD`, so the next install reflects the latest
/// remote commit without re-cloning.
pub struct GitResolver<'a> {
    pub url: String,
    pub cache_root: PathBuf,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> GitResolver<'a> {
    pub fn new(
        url: impl Into<String>,
        cache_root: impl Into<PathBuf>,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            url: url.into(),
            cache_root: cache_root.into(),
            runner,
        }
    }

    /// Lower-cased hex sha256 of `url` — used as the cache subdirectory.
    pub fn cache_subdir(url: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        let digest = hasher.finalize();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn cache_dir(&self) -> PathBuf {
        self.cache_root.join(Self::cache_subdir(&self.url))
    }
}

impl Resolver for GitResolver<'_> {
    fn resolve(&self) -> Result<ResolvedSkill> {
        let cache_dir = self.cache_dir();

        if cache_dir.join(".git").exists() {
            // Cache hit — refresh.
            self.runner
                .run("git", &["fetch", "--quiet", "origin"], Some(&cache_dir))
                .context("refreshing cached skill clone")?;
            self.runner
                .run(
                    "git",
                    &["reset", "--quiet", "--hard", "origin/HEAD"],
                    Some(&cache_dir),
                )
                .context("resetting cached skill clone to origin/HEAD")?;
        } else {
            // Cache miss — clone fresh.
            if cache_dir.exists() {
                fs::remove_dir_all(&cache_dir).with_context(|| {
                    format!("removing stale cache directory {}", cache_dir.display())
                })?;
            }
            if let Some(parent) = cache_dir.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating cache parent {}", parent.display()))?;
            }
            let cache_dir_str = cache_dir
                .to_str()
                .ok_or_else(|| anyhow!("cache path is not valid UTF-8"))?;
            // `--` separator before the URL: defense-in-depth against
            // flag-injection payloads slipping through `looks_like_git_url`.
            // Anything after `--` is treated by `git` as a positional arg,
            // not a flag, so a hostile URL like `--upload-pack=/bin/evil`
            // cannot be reinterpreted as a git option.
            self.runner
                .run(
                    "git",
                    &[
                        "clone",
                        "--quiet",
                        "--depth=1",
                        "--",
                        &self.url,
                        cache_dir_str,
                    ],
                    None,
                )
                .with_context(|| format!("cloning {}", self.url))?;
        }

        let skill_md = cache_dir.join(SKILL_FILENAME);
        if !skill_md.exists() {
            bail!(
                "cloned repository at {} does not contain {SKILL_FILENAME} at its root",
                cache_dir.display()
            );
        }

        let skill = parse_skill_file(&skill_md)
            .map_err(|e| anyhow!("cloned skill failed validation: {e}"))?;

        // F-656: clones land in a Forge-owned cache directory, but pin the
        // fingerprint anyway so `install_resolved`'s TOCTOU check has a
        // single contract that covers every resolver.
        let source_fingerprint = SourceFingerprint::capture(&cache_dir)?;

        Ok(ResolvedSkill {
            skill,
            source_dir: cache_dir,
            source_fingerprint,
        })
    }
}

/// Default cache root: `~/.cache/forge/skills/`.
pub fn default_cache_root(home: &Path) -> PathBuf {
    home.join(".cache").join("forge").join("skills")
}

/// Treat a CLI source string as a git URL when it looks like one.
///
/// **F-641 (CVE-2017-1000117 family):** the accepted scheme set is a strict
/// allowlist of `https://` and `ssh://`. SCP-style URLs (`user@host:path`),
/// `git://`, and `http://` are *all* refused — SCP because its colon-suffix
/// path can be smuggled past git's flag parser as `--upload-pack=…`, and the
/// unauthenticated schemes because they invite MITM-injected payloads. Users
/// who need SSH must spell out `ssh://git@host/owner/repo`.
///
/// We also do not heuristically treat `<owner>/<repo>` as GitHub shorthand;
/// the user must spell out the full URL.
pub fn looks_like_git_url(source: &str) -> bool {
    // Defense-in-depth against flag injection (e.g. `--upload-pack=/bin/evil`):
    // a URL that begins with `-` is never a real URL, and even if `--` would
    // separate it from `git clone`'s flags, classifying it as a URL would
    // route it through the git resolver where it has no business going.
    // Reject explicitly so the classification is independent of the prefix
    // list below — a future addition there can't accidentally let a
    // leading-dash payload through.
    if source.starts_with('-') {
        return false;
    }
    source.starts_with("https://") || source.starts_with("ssh://")
}

/// Install a resolved skill into `target`. Returns the destination directory
/// it landed in.
///
/// Refuses to overwrite an existing skill with the same id in the same
/// scope. Callers that want force-replace must first call [`remove_skill`].
///
/// # F-656 — TOCTOU guard
///
/// Re-validates `resolved.source_dir` against the fingerprint captured by
/// [`Resolver::resolve`] before copying. If the path was swapped for a
/// symlink or replaced with a different directory in the interval, the
/// install refuses rather than copying the attacker tree.
pub fn install_resolved(
    resolved: &ResolvedSkill,
    scope: SkillScope,
    workspace_root: &Path,
    home: &Path,
) -> Result<PathBuf> {
    let target_root = scope.skills_dir(workspace_root, home);
    fs::create_dir_all(&target_root)
        .with_context(|| format!("creating {}", target_root.display()))?;

    let target = target_root.join(resolved.skill.id.as_str());
    if target.exists() {
        bail!(
            "skill {} already installed at {} (run `forge skill remove {} --scope {}` first)",
            resolved.skill.id,
            target.display(),
            resolved.skill.id,
            scope
        );
    }

    // F-656: confirm the source root is still the directory we validated.
    // Without this, an attacker can substitute the tree (or a symlink to
    // an attacker tree) at the same path between resolve and install.
    resolved
        .source_fingerprint
        .verify(&resolved.source_dir)
        .with_context(|| {
            format!(
                "source root {} changed between resolve and install",
                resolved.source_dir.display()
            )
        })?;

    // Copy into the target. On any failure, roll back the partial copy so
    // a refused install (e.g. an escape-symlink) leaves no trace behind.
    //
    // The traversal treats `resolved.source_dir` as already-canonical (the
    // resolver validated it) and uses the captured fingerprint as the
    // anchor for symlink-escape checks — this avoids re-canonicalizing the
    // root, which would silently re-follow a swap-in symlink.
    if let Err(err) =
        copy_dir_recursive(&resolved.source_dir, &target, &resolved.source_fingerprint)
            .with_context(|| {
                format!(
                    "copying {} -> {}",
                    resolved.source_dir.display(),
                    target.display()
                )
            })
    {
        // Best-effort cleanup; if removal itself fails (e.g. permissions),
        // surface the original install error rather than the cleanup error.
        let _ = fs::remove_dir_all(&target);
        return Err(err);
    }

    Ok(target)
}

/// Remove an installed skill from the given scope.
///
/// Returns `Ok(true)` if a directory was removed, `Ok(false)` if no skill
/// with that id was installed in that scope.
pub fn remove_skill(
    id: &str,
    scope: SkillScope,
    workspace_root: &Path,
    home: &Path,
) -> Result<bool> {
    let target = scope.skills_dir(workspace_root, home).join(id);
    if !target.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&target).with_context(|| format!("removing {}", target.display()))?;
    Ok(true)
}

/// One row of `forge skill list` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkillRow {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub scope: SkillScope,
    pub source_path: PathBuf,
}

/// Enumerate every installed skill across both scopes.
///
/// Each scope is listed independently (no precedence merging) — users want
/// to see what is installed where, including shadowed entries.
pub fn list_installed(workspace_root: &Path, home: &Path) -> Result<Vec<InstalledSkillRow>> {
    let mut rows = Vec::new();
    for scope in [SkillScope::User, SkillScope::Workspace] {
        let scope_root = scope.scope_root(workspace_root, home);
        let scope_skills = match scope {
            SkillScope::User => forge_agents::skill_loader::load_user_skills(&scope_root),
            SkillScope::Workspace => forge_agents::skill_loader::load_workspace_skills(&scope_root),
        }
        .map_err(|e| anyhow!("listing {} scope: {e}", scope))?;
        for s in scope_skills {
            rows.push(InstalledSkillRow {
                id: s.id.as_str().to_string(),
                name: s.name,
                version: s.version,
                scope,
                source_path: s.source_path,
            });
        }
    }
    rows.sort_by(|a, b| a.scope.label().cmp(b.scope.label()).then(a.id.cmp(&b.id)));
    Ok(rows)
}

/// Print [`list_installed`] output as a fixed-column table.
pub fn render_list(rows: &[InstalledSkillRow], out: &mut impl Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "no skills installed")?;
        return Ok(());
    }

    let id_w = rows.iter().map(|r| r.id.len()).max().unwrap_or(2).max(2);
    let scope_w = rows
        .iter()
        .map(|r| r.scope.label().len())
        .max()
        .unwrap_or(5)
        .max(5);
    let ver_w = rows
        .iter()
        .map(|r| r.version.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(7)
        .max(7);

    writeln!(
        out,
        "{:<id_w$}  {:<scope_w$}  {:<ver_w$}  SOURCE",
        "ID", "SCOPE", "VERSION",
    )?;
    for r in rows {
        writeln!(
            out,
            "{:<id_w$}  {:<scope_w$}  {:<ver_w$}  {}",
            r.id,
            r.scope.label(),
            r.version.as_deref().unwrap_or("-"),
            r.source_path.display(),
        )?;
    }
    Ok(())
}

/// Copy `src` into `dst`, refusing any symlink whose canonical target
/// escapes the original source root.
///
/// Path-traversal hardening (F-590 review): a malicious skill folder could
/// contain `evil -> /etc/passwd`. Without an explicit escape check, the
/// copy would happily exfiltrate the linked file into the user's installed
/// scope.
///
/// `src` is the resolver's already-canonical source directory, used directly
/// as the symlink-escape boundary — re-canonicalizing here would silently
/// re-follow a TOCTOU swap-in symlink at the root (F-656). The fingerprint
/// is verified once more inside this call as defense-in-depth.
fn copy_dir_recursive(src: &Path, dst: &Path, fingerprint: &SourceFingerprint) -> Result<()> {
    // F-656 belt-and-suspenders: even though `install_resolved` verified the
    // fingerprint already, repeating the check here means any future caller
    // of `copy_dir_recursive` inherits the TOCTOU guard automatically.
    fingerprint.verify(src).with_context(|| {
        format!(
            "source root {} no longer matches resolver fingerprint",
            src.display()
        )
    })?;
    copy_dir_recursive_inner(src, dst, src)
}

fn copy_dir_recursive_inner(src: &Path, dst: &Path, source_root: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());

        // Skip the `.git` directory if we ever copy out of a clone — Forge
        // doesn't need the history and committing a nested `.git` into a
        // workspace would confuse the surrounding repo.
        if entry.file_name() == ".git" {
            continue;
        }

        if file_type.is_symlink() {
            // Resolve symlinks at copy time. A skill source that uses a
            // symlink should land as a regular file in the target so the
            // installed copy is self-contained — but only if the target
            // stays within the original source root. Otherwise we refuse
            // loudly rather than silently skip, so a hostile skill can't
            // smuggle `/etc/passwd` past install by hiding it behind a link.
            let resolved = fs::canonicalize(&from)
                .with_context(|| format!("resolving symlink {}", from.display()))?;
            if !resolved.starts_with(source_root) {
                bail!(
                    "skill contains symlink that escapes source dir: {} -> {} (source root: {})",
                    from.display(),
                    resolved.display(),
                    source_root.display(),
                );
            }
            let resolved_meta = fs::metadata(&resolved)?;
            if resolved_meta.is_dir() {
                copy_dir_recursive_inner(&resolved, &to, source_root)?;
            } else {
                fs::copy(&resolved, &to)?;
            }
        } else if file_type.is_dir() {
            copy_dir_recursive_inner(&from, &to, source_root)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::Duration;
    use tempfile::tempdir;

    fn write_skill_md(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(SKILL_FILENAME), body).unwrap();
    }

    fn good_frontmatter() -> &'static str {
        "---\nname: Tester\nversion: 0.1.0\ndescription: t\n---\nbody"
    }

    #[test]
    fn looks_like_git_url_accepts_only_https_and_ssh() {
        // F-641: SCP-style URLs (e.g. `git@host:path`) and unauthenticated
        // schemes (`http://`, `git://`) are no longer accepted. Only the
        // authenticated `https://` and `ssh://` schemes pass classification.
        assert!(looks_like_git_url("https://github.com/x/y.git"));
        assert!(looks_like_git_url("ssh://git@github.com/x/y"));
    }

    #[test]
    fn looks_like_git_url_rejects_local_paths() {
        assert!(!looks_like_git_url("./fixtures/skill"));
        assert!(!looks_like_git_url("/abs/path/to/skill"));
        assert!(!looks_like_git_url("relative/path"));
        assert!(!looks_like_git_url("C:\\windows\\path"));
        assert!(!looks_like_git_url(""));
    }

    #[test]
    fn looks_like_git_url_rejects_scp_style_and_legacy_schemes() {
        // F-641: SCP-style `user@host:path` form is the carrier for the
        // CVE-2017-1000117 family (flag injection via `git@host:--upload-pack=`).
        // We refuse the entire SCP form — users must spell out `ssh://` if
        // they need SSH-based clones.
        assert!(!looks_like_git_url("git@github.com:x/y.git"));
        assert!(!looks_like_git_url("user@host.example:owner/repo"));
        // Unauthenticated schemes are also out: `git://` is unencrypted and
        // `http://` is plaintext. Both encourage MITM-injected payloads.
        assert!(!looks_like_git_url("git://github.com/x/y.git"));
        assert!(!looks_like_git_url("http://example.com/x.git"));
    }

    #[test]
    fn looks_like_git_url_rejects_scp_style_flag_injection_poc() {
        // F-641 PoC from issue #677. Pre-fix this returned `true`, which
        // routed the string into `GitResolver` where `git clone` would
        // interpret `--upload-pack=/tmp/evil_script` as a clone option and
        // execute the named binary on the local filesystem.
        assert!(!looks_like_git_url(
            "git@github.com:--upload-pack=/tmp/evil_script/repo.git"
        ));
        // Same threat class with a different SCP host portion.
        assert!(!looks_like_git_url(
            "user@host.example:--config=core.gitProxy=http://evil"
        ));
    }

    #[test]
    fn cache_subdir_is_deterministic_hex() {
        let h1 = GitResolver::cache_subdir("https://example.com/x.git");
        let h2 = GitResolver::cache_subdir("https://example.com/x.git");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
        // Different urls -> different hashes.
        assert_ne!(h1, GitResolver::cache_subdir("https://example.com/y.git"));
    }

    #[test]
    fn local_resolver_accepts_directory_with_skill_md() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("planner");
        write_skill_md(&skill_dir, good_frontmatter());

        let r = LocalPathResolver::new(&skill_dir, dir.path());
        let resolved = r.resolve().unwrap();
        assert_eq!(resolved.skill.id.as_str(), "planner");
        assert_eq!(resolved.skill.name, "Tester");
        assert_eq!(resolved.source_dir, fs::canonicalize(&skill_dir).unwrap());
    }

    #[test]
    fn local_resolver_resolves_relative_paths_against_cwd() {
        let dir = tempdir().unwrap();
        let cwd = dir.path();
        let skill_dir = cwd.join("subdir").join("relskill");
        write_skill_md(&skill_dir, good_frontmatter());

        let r = LocalPathResolver::new("subdir/relskill", cwd);
        let resolved = r.resolve().unwrap();
        assert_eq!(resolved.skill.id.as_str(), "relskill");
    }

    #[test]
    fn local_resolver_rejects_when_skill_md_missing() {
        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let r = LocalPathResolver::new(&empty, dir.path());
        let err = r.resolve().unwrap_err();
        assert!(
            err.to_string().contains("SKILL.md"),
            "expected SKILL.md mention, got: {err}",
        );
    }

    #[test]
    fn local_resolver_rejects_invalid_frontmatter() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("bad");
        // Type-mismatched name (sequence into Option<String>).
        write_skill_md(&skill_dir, "---\nname:\n  - a\n  - b\n---\nbody");
        let r = LocalPathResolver::new(&skill_dir, dir.path());
        let err = r.resolve().unwrap_err();
        assert!(
            err.to_string().contains("validation")
                || err.to_string().to_lowercase().contains("frontmatter"),
            "expected validation error, got: {err}",
        );
    }

    #[test]
    fn local_resolver_rejects_file_source() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let r = LocalPathResolver::new(&file, dir.path());
        let err = r.resolve().unwrap_err();
        assert!(
            err.to_string().contains("must be a directory"),
            "expected directory check, got: {err}",
        );
    }

    #[test]
    fn install_copies_resolved_skill_into_workspace_scope() {
        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("planner");
        write_skill_md(&skill_dir, good_frontmatter());
        // Side file alongside SKILL.md should be copied.
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(skill_dir.join("scripts").join("helper.sh"), "echo hi").unwrap();

        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        let installed = install_resolved(
            &resolved,
            SkillScope::Workspace,
            workspace.path(),
            home.path(),
        )
        .unwrap();

        assert!(installed.join("SKILL.md").exists());
        assert!(installed.join("scripts").join("helper.sh").exists());
        // Workspace scope went under workspace_root, not home.
        assert!(installed.starts_with(workspace.path()));
    }

    #[test]
    fn install_defaults_to_user_scope_under_home() {
        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("home-skill");
        write_skill_md(&skill_dir, good_frontmatter());

        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        let installed =
            install_resolved(&resolved, SkillScope::User, workspace.path(), home.path()).unwrap();
        assert!(installed.starts_with(home.path()));
    }

    #[test]
    #[cfg(unix)]
    fn install_refuses_symlink_pointing_outside_source_dir() {
        // Path-traversal hardening: a malicious skill folder that contains
        // `evil -> /etc/passwd` (or any target outside the source root) must
        // not let `forge skill install` exfiltrate that file into the user's
        // installed scope. The DoD calls for "safe canonicalization", which
        // we read as "refuse symlinks that escape the source dir."
        use std::os::unix::fs::symlink;

        let src = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "should never be copied").unwrap();

        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("evil");
        write_skill_md(&skill_dir, good_frontmatter());
        // Symlink inside the skill folder pointing at a file *outside* it.
        symlink(&secret, skill_dir.join("evil")).unwrap();

        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        let err = install_resolved(&resolved, SkillScope::User, workspace.path(), home.path())
            .unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("symlink") && (msg.contains("escape") || msg.contains("outside")),
            "expected escape/outside symlink error, got: {err:#}",
        );

        // Nothing leaked into the user scope.
        let installed_root = home.path().join(".skills").join("evil");
        assert!(
            !installed_root.exists(),
            "install must be transactional or refuse cleanly; nothing should be copied",
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_refuses_symlink_pointing_at_directory_outside_source_dir() {
        use std::os::unix::fs::symlink;

        let src = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir_all(outside.path().join("loot")).unwrap();
        fs::write(outside.path().join("loot").join("a"), "x").unwrap();

        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("evil-dir");
        write_skill_md(&skill_dir, good_frontmatter());
        symlink(outside.path().join("loot"), skill_dir.join("loot")).unwrap();

        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        let err = install_resolved(&resolved, SkillScope::User, workspace.path(), home.path())
            .unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("symlink") && (msg.contains("escape") || msg.contains("outside")),
            "expected escape/outside symlink error, got: {err:#}",
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_allows_symlink_that_stays_inside_source_dir() {
        // Negative-control: symlinks whose canonical target stays within the
        // source dir are still allowed, so the guard does not regress
        // legitimate skills that use internal symlinks (e.g. a stable
        // alias for a versioned reference file).
        use std::os::unix::fs::symlink;

        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("alias");
        write_skill_md(&skill_dir, good_frontmatter());
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        let real = skill_dir.join("references").join("v1.txt");
        fs::write(&real, "real contents").unwrap();
        symlink(&real, skill_dir.join("latest.txt")).unwrap();

        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        let installed =
            install_resolved(&resolved, SkillScope::User, workspace.path(), home.path())
                .expect("internal symlink must still install");
        assert_eq!(
            fs::read_to_string(installed.join("latest.txt")).unwrap(),
            "real contents",
        );
    }

    /// F-656 regression: an attacker who can write inside the source-root's
    /// parent directory may rename the validated tree away and replace it with
    /// a symlink pointing at a different tree, between
    /// `LocalPathResolver::resolve` and `install_resolved`. The installer must
    /// either still install the originally-validated tree or refuse. It must
    /// NOT install the attacker's tree.
    #[cfg(unix)]
    #[test]
    fn install_refuses_when_source_root_swapped_for_symlink_after_resolve() {
        use std::os::unix::fs::symlink;

        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        // The benign tree the user intends to install.
        let skill_dir = src.path().join("planner");
        write_skill_md(&skill_dir, good_frontmatter());
        fs::write(skill_dir.join("note.txt"), "BENIGN").unwrap();

        // The attacker tree the swap would silently install instead.
        let attacker_dir = src.path().join("attacker");
        write_skill_md(&attacker_dir, good_frontmatter());
        fs::write(attacker_dir.join("note.txt"), "ATTACKER").unwrap();
        // A "secret" file the attacker wants smuggled into the install scope.
        fs::write(attacker_dir.join("loot.txt"), "ATTACKER_OWNED").unwrap();

        let resolver = LocalPathResolver::new(&skill_dir, src.path());
        let resolved = resolver.resolve().expect("resolve must succeed");

        // Simulate the TOCTOU swap: rename the validated tree away and
        // replace it with a symlink to the attacker tree at the same path.
        let stash = src.path().join("planner.stash");
        fs::rename(&skill_dir, &stash).unwrap();
        symlink(&attacker_dir, &skill_dir).unwrap();

        let result = install_resolved(&resolved, SkillScope::User, workspace.path(), home.path());

        // Acceptable outcomes: refuse loudly, or install the originally-
        // validated tree. Installing the attacker tree is the regression.
        let installed_root = home.path().join(".skills").join("planner");
        match result {
            Err(err) => {
                let msg = format!("{err:#}").to_lowercase();
                assert!(
                    msg.contains("toctou")
                        || msg.contains("changed")
                        || msg.contains("symlink")
                        || msg.contains("source root"),
                    "expected TOCTOU/symlink error, got: {err:#}",
                );
                assert!(
                    !installed_root.exists(),
                    "refusal must roll back the partial copy",
                );
            }
            Ok(_) => {
                let installed_note = fs::read_to_string(installed_root.join("note.txt"))
                    .expect("install claimed success but note.txt missing");
                assert_eq!(
                    installed_note, "BENIGN",
                    "installer copied the attacker tree instead of the validated one",
                );
                assert!(
                    !installed_root.join("loot.txt").exists(),
                    "attacker file leaked into install scope",
                );
            }
        }
    }

    /// F-656 regression: even if the swap puts a *real* directory at the
    /// validated path (rather than a symlink), the inode fingerprint must
    /// surface the substitution. This is the bare TOCTOU case.
    #[cfg(unix)]
    #[test]
    fn install_refuses_when_source_root_replaced_with_different_real_dir_after_resolve() {
        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("planner");
        write_skill_md(&skill_dir, good_frontmatter());
        fs::write(skill_dir.join("note.txt"), "BENIGN").unwrap();

        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .expect("resolve must succeed");

        // Swap the validated dir for a different real dir at the same path.
        let stash = src.path().join("planner.stash");
        fs::rename(&skill_dir, &stash).unwrap();
        fs::create_dir_all(&skill_dir).unwrap();
        write_skill_md(&skill_dir, good_frontmatter());
        fs::write(skill_dir.join("note.txt"), "ATTACKER").unwrap();

        let result = install_resolved(&resolved, SkillScope::User, workspace.path(), home.path());

        let installed_root = home.path().join(".skills").join("planner");
        match result {
            Err(_) => {
                assert!(
                    !installed_root.exists(),
                    "refusal must leave nothing behind",
                );
            }
            Ok(_) => {
                let installed_note = fs::read_to_string(installed_root.join("note.txt"))
                    .expect("install claimed success but note.txt missing");
                assert_eq!(
                    installed_note, "BENIGN",
                    "installer must install the originally-validated tree",
                );
            }
        }
    }

    /// F-656 DoD: race a symlink swap against `install_resolved`. The install
    /// must either fail or land the originally-validated tree — never the
    /// attacker tree.
    #[cfg(unix)]
    #[test]
    fn install_race_against_symlink_swap_never_installs_attacker_tree() {
        use std::os::unix::fs::symlink;
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::{Duration, Instant};

        for iter in 0..16 {
            let src = tempdir().unwrap();
            let workspace = tempdir().unwrap();
            let home = tempdir().unwrap();

            let skill_dir = src.path().join("racer");
            write_skill_md(&skill_dir, good_frontmatter());
            fs::write(skill_dir.join("note.txt"), "BENIGN").unwrap();

            let attacker_dir = src.path().join("attacker");
            write_skill_md(&attacker_dir, good_frontmatter());
            fs::write(attacker_dir.join("note.txt"), "ATTACKER").unwrap();
            fs::write(attacker_dir.join("loot.txt"), "ATTACKER_OWNED").unwrap();

            let resolved = LocalPathResolver::new(&skill_dir, src.path())
                .resolve()
                .unwrap();

            let barrier = Arc::new(Barrier::new(2));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

            let swapper_skill = skill_dir.clone();
            let swapper_attacker = attacker_dir.clone();
            let swapper_src = src.path().to_path_buf();
            let swapper_barrier = Arc::clone(&barrier);
            let swapper_stop = Arc::clone(&stop);
            let swapper = thread::spawn(move || {
                swapper_barrier.wait();
                let stash = swapper_src.join("racer.stash");
                let deadline = Instant::now() + Duration::from_millis(50);
                while !swapper_stop.load(std::sync::atomic::Ordering::SeqCst)
                    && Instant::now() < deadline
                {
                    // Best-effort swap: rename benign away, plant symlink.
                    if fs::rename(&swapper_skill, &stash).is_ok() {
                        let _ = symlink(&swapper_attacker, &swapper_skill);
                        let _ = fs::remove_file(&swapper_skill);
                        let _ = fs::rename(&stash, &swapper_skill);
                    }
                }
            });

            barrier.wait();
            let result =
                install_resolved(&resolved, SkillScope::User, workspace.path(), home.path());
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            swapper.join().unwrap();
            // Drop the swap symlink/leftover regardless of state.
            let _ = fs::remove_file(src.path().join("racer.stash"));

            let installed_root = home.path().join(".skills").join("racer");
            match result {
                Ok(_) => {
                    let installed_note =
                        fs::read_to_string(installed_root.join("note.txt")).unwrap_or_default();
                    assert_eq!(
                        installed_note, "BENIGN",
                        "iter {iter}: race installed wrong note.txt",
                    );
                    assert!(
                        !installed_root.join("loot.txt").exists(),
                        "iter {iter}: attacker file leaked",
                    );
                }
                Err(_) => {
                    assert!(
                        !installed_root.exists(),
                        "iter {iter}: error must leave no partial copy",
                    );
                }
            }
            // Avoid colliding "already installed" with the next iteration.
            let _ = fs::remove_dir_all(&installed_root);
        }
    }

    #[test]
    fn install_refuses_to_overwrite_existing_skill() {
        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("dup");
        write_skill_md(&skill_dir, good_frontmatter());
        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        install_resolved(
            &resolved,
            SkillScope::Workspace,
            workspace.path(),
            home.path(),
        )
        .unwrap();
        let err = install_resolved(
            &resolved,
            SkillScope::Workspace,
            workspace.path(),
            home.path(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("already installed"),
            "expected already-installed error, got: {err}",
        );
    }

    #[test]
    fn remove_skill_succeeds_when_present_and_returns_false_when_absent() {
        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let skill_dir = src.path().join("removable");
        write_skill_md(&skill_dir, good_frontmatter());
        let resolved = LocalPathResolver::new(&skill_dir, src.path())
            .resolve()
            .unwrap();
        install_resolved(
            &resolved,
            SkillScope::Workspace,
            workspace.path(),
            home.path(),
        )
        .unwrap();

        assert!(remove_skill(
            "removable",
            SkillScope::Workspace,
            workspace.path(),
            home.path()
        )
        .unwrap());
        assert!(!remove_skill(
            "removable",
            SkillScope::Workspace,
            workspace.path(),
            home.path()
        )
        .unwrap());
    }

    #[test]
    fn list_installed_reports_both_scopes() {
        let src = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let ws_skill = src.path().join("ws-only");
        write_skill_md(&ws_skill, good_frontmatter());
        let user_skill = src.path().join("user-only");
        write_skill_md(&user_skill, good_frontmatter());

        install_resolved(
            &LocalPathResolver::new(&ws_skill, src.path())
                .resolve()
                .unwrap(),
            SkillScope::Workspace,
            workspace.path(),
            home.path(),
        )
        .unwrap();
        install_resolved(
            &LocalPathResolver::new(&user_skill, src.path())
                .resolve()
                .unwrap(),
            SkillScope::User,
            workspace.path(),
            home.path(),
        )
        .unwrap();

        let rows = list_installed(workspace.path(), home.path()).unwrap();
        let labels: Vec<(String, &'static str)> = rows
            .iter()
            .map(|r| (r.id.clone(), r.scope.label()))
            .collect();
        assert!(labels.contains(&("ws-only".into(), "workspace")));
        assert!(labels.contains(&("user-only".into(), "user")));
    }

    #[test]
    fn render_list_emits_no_skills_message_when_empty() {
        let mut out = Vec::new();
        render_list(&[], &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("no skills"));
    }

    #[test]
    fn render_list_emits_columns() {
        let rows = vec![
            InstalledSkillRow {
                id: "alpha".into(),
                name: "Alpha".into(),
                version: Some("0.1.0".into()),
                scope: SkillScope::User,
                source_path: PathBuf::from("/u/.skills/alpha/SKILL.md"),
            },
            InstalledSkillRow {
                id: "beta".into(),
                name: "Beta".into(),
                version: None,
                scope: SkillScope::Workspace,
                source_path: PathBuf::from("/w/.skills/beta/SKILL.md"),
            },
        ];
        let mut out = Vec::new();
        render_list(&rows, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("ID"));
        assert!(s.contains("alpha"));
        assert!(s.contains("user"));
        assert!(s.contains("0.1.0"));
        assert!(s.contains("beta"));
        assert!(s.contains("workspace"));
    }

    /// Records every command the runner sees so tests can assert the exact
    /// `git` invocation sequence without spawning a real process.
    #[derive(Default)]
    struct RecordingRunner {
        log: RefCell<Vec<String>>,
        /// Per-call argv (excluding the program), so tests can assert the
        /// position of specific tokens like `--` without splitting strings.
        argv: RefCell<Vec<Vec<String>>>,
        // Hooks that prep the cache dir as if `git` had run, so the resolver
        // can find a `.git` and a `SKILL.md`.
        clone_writes_skill: bool,
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
            let line = format!(
                "{} {} (cwd={:?})",
                program,
                args.join(" "),
                cwd.map(|p| p.display().to_string())
            );
            self.log.borrow_mut().push(line);
            self.argv
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if self.clone_writes_skill && program == "git" && args.first() == Some(&"clone") {
                let target = PathBuf::from(args.last().unwrap());
                fs::create_dir_all(target.join(".git")).unwrap();
                fs::write(target.join(SKILL_FILENAME), good_frontmatter()).unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn git_resolver_clones_on_cache_miss_and_returns_parsed_skill() {
        let cache = tempdir().unwrap();
        let runner = RecordingRunner {
            clone_writes_skill: true,
            ..Default::default()
        };
        let url = "https://example.com/skills/planner.git";
        let resolver = GitResolver::new(url, cache.path(), &runner);

        let resolved = resolver.resolve().unwrap();
        assert_eq!(resolved.skill.name, "Tester");
        let log = runner.log.borrow();
        assert_eq!(log.len(), 1);
        assert!(log[0].starts_with("git clone --quiet --depth=1 "));
        assert!(log[0].contains(url));
        // Cache directory was hashed.
        assert!(resolved
            .source_dir
            .starts_with(cache.path().join(GitResolver::cache_subdir(url))));
    }

    #[test]
    fn git_resolver_refreshes_on_cache_hit() {
        let cache = tempdir().unwrap();
        let url = "https://example.com/skills/planner.git";
        let cache_dir = cache.path().join(GitResolver::cache_subdir(url));
        fs::create_dir_all(cache_dir.join(".git")).unwrap();
        fs::write(cache_dir.join(SKILL_FILENAME), good_frontmatter()).unwrap();

        let runner = RecordingRunner::default();
        let resolver = GitResolver::new(url, cache.path(), &runner);

        let _resolved = resolver.resolve().unwrap();
        let log = runner.log.borrow();
        assert_eq!(log.len(), 2, "expected fetch + reset, got: {log:?}");
        assert!(log[0].contains("fetch"));
        assert!(log[1].contains("reset --quiet --hard origin/HEAD"));
    }

    #[test]
    fn git_clone_passes_double_dash_before_url_to_block_flag_injection() {
        // Hardening (post-merge review): a user-supplied URL like
        // `--upload-pack=/bin/evil` would be parsed as a flag by `git clone`
        // unless we pass `--` before it. `looks_like_git_url()` blocks the
        // most obvious payloads, but a defense-in-depth `--` separator
        // costs nothing and matches F-595's pattern.
        let cache = tempdir().unwrap();
        let runner = RecordingRunner {
            clone_writes_skill: true,
            ..Default::default()
        };
        let url = "https://example.com/skills/planner.git";
        let resolver = GitResolver::new(url, cache.path(), &runner);

        resolver.resolve().unwrap();

        let argv = runner.argv.borrow();
        assert_eq!(argv.len(), 1, "expected exactly one git invocation");
        let clone_argv = &argv[0];
        let dash_dash = clone_argv
            .iter()
            .position(|a| a == "--")
            .expect("git clone argv must contain `--`");
        let url_pos = clone_argv
            .iter()
            .position(|a| a == url)
            .expect("git clone argv must contain the URL");
        assert_eq!(
            dash_dash + 1,
            url_pos,
            "`--` must immediately precede the URL: {clone_argv:?}",
        );
    }

    #[test]
    fn looks_like_git_url_rejects_leading_dash_payloads() {
        // Defense-in-depth (post-merge review): URLs that begin with `-`
        // are flag-injection payloads, not real URLs. Reject them at the
        // classification step so they never reach the git argv.
        assert!(!looks_like_git_url("-evil"));
        assert!(!looks_like_git_url("--upload-pack=/bin/evil"));
        assert!(!looks_like_git_url("--config=core.gitProxy=http://evil"));
    }

    #[test]
    fn default_cache_root_is_under_dot_cache_forge_skills() {
        let home = PathBuf::from("/home/test");
        assert_eq!(
            default_cache_root(&home),
            PathBuf::from("/home/test/.cache/forge/skills")
        );
    }

    // ----------------------------------------------------------------------
    // F-665 — Hardened git invocation environment.
    //
    // `StdCommandRunner` is the only place git is spawned. Asserting the
    // hardening at this layer covers every git call (clone, fetch, reset)
    // without per-call duplication. The tests below use real subprocess
    // invocations against `/usr/bin/env` and `/usr/bin/sleep` because the
    // contract is about what reaches the OS-level child process — a mock
    // runner would defeat the purpose.
    // ----------------------------------------------------------------------

    /// Spawn `/usr/bin/env` via `StdCommandRunner` and return the child's
    /// observed environment as a `HashMap`. Caller seeds parent-side env vars
    /// before invoking; the helper writes them back into a temp file the
    /// child produces, then parses.
    #[cfg(unix)]
    fn capture_child_env(
        runner: &StdCommandRunner,
        dump_path: &Path,
    ) -> std::collections::HashMap<String, String> {
        // `/usr/bin/env` with no args writes `KEY=VALUE` lines to stdout. We
        // shell out via `sh -c` so we can redirect to a file the test owns —
        // capturing stdout via the runner trait is not available.
        let cmd = format!("/usr/bin/env > {}", dump_path.display());
        runner
            .run("sh", &["-c", &cmd], None)
            .expect("env-capturing child must succeed");
        let raw = fs::read_to_string(dump_path).expect("env dump should be readable");
        raw.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// F-665 DoD: parent-process `GIT_*` env vars must not reach the git
    /// child. A hostile or curious parent that sets `GIT_TRACE=1` or any
    /// other `GIT_*` should be invisible to the spawned process.
    #[cfg(unix)]
    #[test]
    fn std_runner_strips_parent_git_env() {
        // Set a `GIT_*` and an arbitrary unrelated var in the parent. Both
        // must be absent from the child's view.
        //
        // Safety: env mutation in tests is process-wide; we restore on drop
        // via a guard so parallel tests do not see leftover state.
        struct EnvGuard {
            keys: Vec<&'static str>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for k in &self.keys {
                    // SAFETY: see Rust 1.85 docs — env mutation in tests is
                    // accepted as a known hazard; we serialize via the test
                    // mutex (see below) so concurrent tests cannot race.
                    unsafe { std::env::remove_var(k) };
                }
            }
        }
        let _lock = env_test_lock().lock().unwrap();
        unsafe {
            std::env::set_var("GIT_TRACE", "1");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "0");
            std::env::set_var("FORGE_TEST_LEAK", "should-not-reach-child");
        }
        let _guard = EnvGuard {
            keys: vec!["GIT_TRACE", "GIT_CONFIG_NOSYSTEM", "FORGE_TEST_LEAK"],
        };

        let dir = tempdir().unwrap();
        let dump = dir.path().join("env.txt");
        let runner = StdCommandRunner::new();
        let child_env = capture_child_env(&runner, &dump);

        // The hostile parent values must not reach the child.
        assert!(
            !child_env.contains_key("GIT_TRACE"),
            "GIT_TRACE leaked from parent into git child env: {:?}",
            child_env.get("GIT_TRACE"),
        );
        assert!(
            !child_env.contains_key("FORGE_TEST_LEAK"),
            "arbitrary parent var leaked into child: {:?}",
            child_env.get("FORGE_TEST_LEAK"),
        );
        // Forge-controlled hardening must be in effect, overriding any
        // parent value the user set.
        assert_eq!(
            child_env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            child_env.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
            Some("1")
        );
        let ssh = child_env
            .get("GIT_SSH_COMMAND")
            .expect("GIT_SSH_COMMAND must be set");
        assert!(
            ssh.contains("BatchMode=yes"),
            "GIT_SSH_COMMAND missing BatchMode: {ssh}"
        );
        assert!(
            ssh.contains("StrictHostKeyChecking=accept-new"),
            "GIT_SSH_COMMAND missing StrictHostKeyChecking=accept-new: {ssh}",
        );
    }

    /// F-665 DoD: legitimate dev-flow vars in the parent's allowlist
    /// (`PATH`, `HOME`, `LANG`, `LC_*`, `SSH_AUTH_SOCK`, `TERM`) must pass
    /// through. SSH-based clones rely on `SSH_AUTH_SOCK` for agent auth and
    /// breaking that breaks legitimate users.
    #[cfg(unix)]
    #[test]
    fn std_runner_passes_through_allowlisted_parent_env() {
        let _lock = env_test_lock().lock().unwrap();
        struct EnvGuard {
            keys: Vec<&'static str>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for k in &self.keys {
                    unsafe { std::env::remove_var(k) };
                }
            }
        }
        unsafe {
            std::env::set_var("SSH_AUTH_SOCK", "/tmp/forge-test-ssh-agent.sock");
            std::env::set_var("LC_ALL", "C.UTF-8");
        }
        let _guard = EnvGuard {
            keys: vec!["SSH_AUTH_SOCK", "LC_ALL"],
        };

        let dir = tempdir().unwrap();
        let dump = dir.path().join("env.txt");
        let runner = StdCommandRunner::new();
        let child_env = capture_child_env(&runner, &dump);

        // PATH must pass through or `sh` itself would not have been findable
        // by the runner — but assert it explicitly to pin the contract.
        assert!(
            child_env.contains_key("PATH"),
            "PATH must pass through the env scrub",
        );
        assert_eq!(
            child_env.get("SSH_AUTH_SOCK").map(String::as_str),
            Some("/tmp/forge-test-ssh-agent.sock"),
            "SSH_AUTH_SOCK must pass through for agent-based SSH clones",
        );
        assert_eq!(
            child_env.get("LC_ALL").map(String::as_str),
            Some("C.UTF-8"),
            "LC_* locale vars must pass through",
        );
    }

    /// F-665 DoD: a non-responsive remote must not hang the CLI. Surrogate:
    /// `sleep 99` exceeds the configured timeout and must be killed.
    #[cfg(unix)]
    #[test]
    fn std_runner_kills_child_after_timeout() {
        use std::time::Instant;
        let runner = StdCommandRunner::with_timeout(Duration::from_millis(200));
        let started = Instant::now();
        let result = runner.run("sleep", &["99"], None);
        let elapsed = started.elapsed();
        assert!(result.is_err(), "expected timeout error, got Ok");
        let msg = format!("{:#}", result.unwrap_err()).to_lowercase();
        assert!(
            msg.contains("timeout") || msg.contains("timed out"),
            "expected timeout-flavored error, got: {msg}",
        );
        // Generous upper bound: a healthy timeout fires well under 5s.
        // If this trips the suite is hung on the kill path.
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout took {elapsed:?} to fire — kill path is broken",
        );
    }

    /// F-665 DoD: `GIT_TERMINAL_PROMPT=0` is set so a clone that would
    /// otherwise prompt for credentials cannot hang waiting on stdin. The
    /// previous test asserts the env var is set; this test pins the
    /// configured timeout default (60s) so a regression that drops the
    /// timeout entirely surfaces here even if `GIT_TERMINAL_PROMPT` survives.
    #[test]
    fn std_runner_default_timeout_is_bounded() {
        let runner = StdCommandRunner::new();
        // Inspect via the public timeout accessor — we expose it precisely
        // so callers and tests can confirm a non-zero, finite bound is in
        // place, defending against `Duration::ZERO` or `Duration::MAX`
        // regressions that would defeat the "no infinite hang" property.
        let t = runner.timeout();
        assert!(t > Duration::from_secs(0), "default timeout must be > 0");
        assert!(
            t <= Duration::from_secs(300),
            "default timeout must be a sane upper bound (<= 5min), got {t:?}",
        );
    }

    /// Process-wide env mutation in `std_runner_strips_parent_git_env` and
    /// `std_runner_passes_through_allowlisted_parent_env` would race if the
    /// suite runs them in parallel. Serialize them via a static mutex.
    fn env_test_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }
    // ── F-681: GitResolver + cache-FS failure paths ──────────────────
    //
    // These tests pin error propagation on the four uncovered paths
    // identified in the audit: `git clone` failure, `git fetch` failure
    // on cache-hit refresh, `fs::remove_dir_all` failure on stale cache,
    // and `fs::create_dir_all` failure on read-only parent. Pattern
    // mirrors F-655 (MockRuntime → fail_start) — a configurable test
    // double that returns a caller-supplied error on the matching call.

    /// Decision for a single `(program, args)` invocation: `Some(msg)`
    /// fails the call with `msg`, `None` lets it succeed.
    type FailurePredicate = dyn Fn(&str, &[&str]) -> Option<String>;

    /// `CommandRunner` test double that fails commands matching a
    /// caller-supplied predicate. Reusable across every git
    /// failure-path test so each test only spells out the failure shape
    /// it cares about.
    struct FailingRunner {
        should_fail: Box<FailurePredicate>,
        log: RefCell<Vec<String>>,
    }

    impl FailingRunner {
        fn new<F>(should_fail: F) -> Self
        where
            F: Fn(&str, &[&str]) -> Option<String> + 'static,
        {
            Self {
                should_fail: Box::new(should_fail),
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FailingRunner {
        fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
            self.log.borrow_mut().push(format!(
                "{program} {} (cwd={:?})",
                args.join(" "),
                cwd.map(|p| p.display().to_string())
            ));
            if let Some(msg) = (self.should_fail)(program, args) {
                bail!("{msg}");
            }
            Ok(())
        }
    }

    #[test]
    fn git_clone_failure_propagates_with_context() {
        // `git clone` exiting non-zero must surface the resolver's
        // "cloning <url>" context — without it, callers see a bare
        // command-failed message and cannot tell which URL failed.
        let cache = tempdir().unwrap();
        let url = "https://example.com/skills/planner.git";
        let runner = FailingRunner::new(|program, args| {
            (program == "git" && args.first() == Some(&"clone"))
                .then(|| "git clone exited 128: repository not found".to_string())
        });
        let resolver = GitResolver::new(url, cache.path(), &runner);

        let err = resolver.resolve().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(url),
            "expected URL in clone-failure context, got: {msg}",
        );
        assert!(
            msg.contains("cloning"),
            "expected `cloning` context, got: {msg}",
        );
        assert!(
            msg.contains("repository not found"),
            "expected underlying runner error to propagate, got: {msg}",
        );
    }

    #[test]
    fn git_fetch_failure_on_cache_hit_propagates() {
        // Cache hit path: a populated cache dir triggers `git fetch`
        // followed by `git reset`. A `fetch` failure (e.g. transient
        // network blip) must surface the "refreshing cached skill
        // clone" context so users can distinguish refresh failures
        // from initial-clone failures.
        let cache = tempdir().unwrap();
        let url = "https://example.com/skills/planner.git";
        // Pre-populate the cache so the resolver takes the cache-hit
        // branch.
        let cache_dir = cache.path().join(GitResolver::cache_subdir(url));
        fs::create_dir_all(cache_dir.join(".git")).unwrap();
        fs::write(cache_dir.join(SKILL_FILENAME), good_frontmatter()).unwrap();

        let runner = FailingRunner::new(|program, args| {
            (program == "git" && args.first() == Some(&"fetch"))
                .then(|| "git fetch exited 1: network unreachable".to_string())
        });
        let resolver = GitResolver::new(url, cache.path(), &runner);

        let err = resolver.resolve().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("refreshing cached skill clone"),
            "expected refresh context, got: {msg}",
        );
        assert!(
            msg.contains("network unreachable"),
            "expected underlying runner error to propagate, got: {msg}",
        );
        // Reset must not have been called once fetch failed.
        let log = runner.log.borrow();
        assert_eq!(log.len(), 1, "expected only fetch attempt, got: {log:?}");
        assert!(log[0].contains("fetch"));
    }

    #[test]
    #[cfg(unix)]
    fn cache_dir_remove_failure_propagates() {
        // Stale cache cleanup: if a cache subdir exists *without* a
        // `.git` (interrupted prior clone, manual mess, etc.), the
        // resolver wipes it before re-cloning. If the wipe fails — e.g.
        // the parent is read-only so directory entries can't be
        // unlinked — the resolver must surface the
        // "removing stale cache directory" context rather than silently
        // continuing into a broken clone.
        use std::os::unix::fs::PermissionsExt;

        let cache = tempdir().unwrap();
        let url = "https://example.com/skills/planner.git";
        let cache_dir = cache.path().join(GitResolver::cache_subdir(url));
        // Stale dir without `.git` — triggers the remove path. Drop a
        // file inside so `remove_dir_all` actually has to unlink an
        // entry (whose unlink will be denied by the read-only parent).
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("stale.txt"), "stale").unwrap();

        // Make the cache parent read-only so unlinking entries inside
        // `cache_dir` fails. We restore permissions in a guard so the
        // tempdir cleanup at end-of-test still works.
        let parent = cache_dir.parent().unwrap().to_path_buf();
        let original = fs::metadata(&parent).unwrap().permissions();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        struct RestorePerms(PathBuf, fs::Permissions);
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, self.1.clone());
            }
        }
        let _guard = RestorePerms(parent.clone(), original);

        // The runner shouldn't be reached — remove_dir_all fails first.
        let runner =
            FailingRunner::new(|_, _| panic!("git must not be invoked once cache cleanup fails"));
        let resolver = GitResolver::new(url, cache.path(), &runner);

        let err = resolver.resolve().unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("removing stale cache directory"),
            "expected stale-cache context, got: {msg}",
        );
    }

    #[test]
    #[cfg(unix)]
    fn cache_dir_create_failure_propagates() {
        // Cache parent creation: when the cache root is rooted under a
        // read-only directory, `fs::create_dir_all(parent)` fails. The
        // resolver must surface the "creating cache parent" context so
        // users see *what* couldn't be created rather than a generic
        // permission-denied error.
        use std::os::unix::fs::PermissionsExt;

        let outer = tempdir().unwrap();
        // Read-only outer dir → cache_root inside it cannot be created.
        let original = fs::metadata(outer.path()).unwrap().permissions();
        fs::set_permissions(outer.path(), fs::Permissions::from_mode(0o500)).unwrap();
        struct RestorePerms(PathBuf, fs::Permissions);
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, self.1.clone());
            }
        }
        let _guard = RestorePerms(outer.path().to_path_buf(), original);

        let cache_root = outer.path().join("cache");
        let url = "https://example.com/skills/planner.git";

        let runner = FailingRunner::new(|_, _| {
            panic!("git must not be invoked once cache-parent creation fails")
        });
        let resolver = GitResolver::new(url, &cache_root, &runner);

        let err = resolver.resolve().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("creating cache parent"),
            "expected cache-parent context, got: {msg}",
        );
    }
}
