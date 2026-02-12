use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Advisory file lock around state.json read-modify-write cycles.
/// The lock is held for the lifetime of this struct and released on Drop.
struct StateLock {
    _file: File,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // unlock is best-effort; the OS releases the lock when the fd is closed anyway
        let _ = FileExt::unlock(&self._file);
    }
}

/// Acquire an exclusive advisory lock on `~/.config/rpool/state.lock`.
/// Blocks until the lock is available. Returns a guard that releases
/// the lock when dropped.
fn lock_state() -> Result<StateLock> {
    let lock_path = config_dir()?.join("state.lock");
    let file = File::create(&lock_path)
        .with_context(|| format!("Failed to create lock file {}", lock_path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("Failed to acquire lock on {}", lock_path.display()))?;
    Ok(StateLock { _file: file })
}

#[derive(Parser)]
#[command(name = "rpool", about = "Manage a pool of repository clones")]
struct Cli {
    /// Operate on a specific pool (instead of auto-detecting from cwd)
    #[arg(short, long, global = true)]
    pool: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a pool for a repository
    Init {
        /// Repository URL or path to clone from
        #[arg(short, long)]
        repo: Option<String>,
        /// Global root directory for all pool clones (default: ~/rppool)
        #[arg(long)]
        clones_root: Option<PathBuf>,
        /// Name for this pool (default: repo name)
        #[arg(short, long)]
        name: Option<String>,
        /// Build command to run after checkout (default: cargo build)
        #[arg(long)]
        build_cmd: Option<String>,
    },
    /// Checkout a branch (assigns a clone, checks out, updates submodules)
    #[command(alias = "ck")]
    Checkout {
        /// Branch name to checkout
        branch: String,
        /// Don't update submodules
        #[arg(long)]
        no_submodules: bool,
    },
    /// Show pool status
    #[command(alias = "st")]
    Status,
    /// Unassign a clone so it can be reused
    Drop {
        /// Clone name to drop (default: current)
        clone: Option<String>,
    },
    /// Checkout a PR by number
    Pr {
        /// PR number
        number: u64,
    },
    /// Fetch all remotes in all clones
    Sync,
    /// Add a new clone to the pool
    New {
        /// Name for the new clone
        name: Option<String>,
    },
    /// List available pools
    Pools,
    /// Remove a pool configuration
    RmPool {
        /// Pool name to remove
        name: String,
    },
    /// Run the configured build command for the current pool
    Build,
    /// Print the path of a clone (for shell cd integration)
    Cd {
        /// Clone name to navigate to
        name: String,
    },
    /// Generate shell completion script
    Completions {
        /// Shell to generate for (bash, zsh, fish)
        shell: String,
    },
    /// Hidden: emit completion candidates for shell integration
    #[command(hide = true)]
    Complete {
        /// What to complete: pools, clones, branches
        kind: String,
    },
    /// Migrate clones into the structured clones_root directory
    Migrate {
        /// Clone to keep in its original location (default: clone named same as pool)
        #[arg(long)]
        keep: Option<String>,
        /// Global root directory for all pool clones (default: ~/rppool)
        #[arg(long)]
        clones_root: Option<PathBuf>,
    },
    /// Pin a clone so it is deprioritized for automatic assignment
    Pin {
        /// Clone name to pin (default: current)
        clone: Option<String>,
    },
    /// Unpin a clone so it can be freely assigned again
    Unpin {
        /// Clone name to unpin (default: current)
        clone: Option<String>,
    },
    /// Diagnose and optionally fix inconsistencies between state and filesystem
    Doctor {
        /// Automatically fix problems that can be resolved
        #[arg(long)]
        fix: bool,
    },
}

fn default_clones_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rppool")
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    #[serde(default = "default_clones_root")]
    clones_root: PathBuf,
    pools: HashMap<String, PoolConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            clones_root: default_clones_root(),
            pools: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoolConfig {
    /// Origin repository URL
    repo_url: String,
    /// Legacy field kept for backward-compat deserialization; not written on save
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    base_dir: Option<PathBuf>,
    /// GitHub owner/repo for PR lookups (e.g., "category-labs/monad-bft")
    github_repo: Option<String>,
    /// Build command to run after checkout
    #[serde(default = "default_build_command")]
    build_command: String,
}

fn default_build_command() -> String {
    "cargo build".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
#[derive(Default)]
struct State {
    pools: HashMap<String, PoolState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PoolState {
    clones: HashMap<String, CloneState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CloneState {
    path: PathBuf,
    assigned_branch: Option<String>,
    last_used: DateTime<Utc>,
    #[serde(default)]
    pinned: bool,
}

/// Compute the directory for a pool's clones under the global clones_root.
fn pool_dir(config: &Config, pool_name: &str) -> PathBuf {
    config.clones_root.join(pool_name)
}

/// Write to a temp file then rename, so readers never see a partial/corrupt file.
fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let pid = std::process::id();
    let tmp = path.with_extension(format!("tmp.{}", pid));
    let mut f = std::fs::File::create(&tmp)
        .with_context(|| format!("Failed to create temp file {}", tmp.display()))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Validate that a user-supplied name is safe to use as a path component
/// and in shell contexts (completion, output parsing).
fn validate_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{} name cannot be empty", kind);
    }
    if name == ".." || name == "." {
        bail!("{} name cannot be '{}' ", kind, name);
    }
    if name.starts_with('-') {
        bail!("{} name '{}' must not start with '-'", kind, name);
    }
    // Reject path separators, whitespace, control chars, and shell glob chars
    let bad: &[char] = &['/', '\\', ' ', '\t', '\n', '\r', '*', '?', '[', ']', '{', '}'];
    if let Some(c) = name.chars().find(|c| bad.contains(c) || c.is_control()) {
        bail!(
            "{} name '{}' contains invalid character '{}'",
            kind,
            name,
            c.escape_default()
        );
    }
    Ok(())
}

fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("Could not determine config directory"))?
        .join("rpool");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn load_config() -> Result<Config> {
    let path = config_dir()?.join("config.json");
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&contents)?)
    } else {
        Ok(Config::default())
    }
}

fn save_config(config: &Config) -> Result<()> {
    let path = config_dir()?.join("config.json");
    let contents = serde_json::to_string_pretty(config)?;
    atomic_write(&path, &contents)?;
    Ok(())
}

fn load_state() -> Result<State> {
    let path = config_dir()?.join("state.json");
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&contents)?)
    } else {
        Ok(State::default())
    }
}

fn save_state(state: &State) -> Result<()> {
    let path = config_dir()?.join("state.json");
    let contents = serde_json::to_string_pretty(state)?;
    atomic_write(&path, &contents)?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BranchCache {
    pools: HashMap<String, Vec<String>>,
}

fn load_branch_cache() -> Result<BranchCache> {
    let path = config_dir()?.join("branch_cache.json");
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&contents)?)
    } else {
        Ok(BranchCache::default())
    }
}

fn save_branch_cache(cache: &BranchCache) -> Result<()> {
    let path = config_dir()?.join("branch_cache.json");
    let contents = serde_json::to_string_pretty(cache)?;
    atomic_write(&path, &contents)?;
    Ok(())
}

/// Collect remote branch names from a clone (strips origin/ prefix).
fn collect_remote_branches(clone_path: &Path) -> Result<Vec<String>> {
    let output = run_git_output(
        &["branch", "-r", "--format=%(refname:short)"],
        clone_path,
    )?;
    let branches: Vec<String> = output
        .lines()
        .filter_map(|line| line.strip_prefix("origin/"))
        .filter(|b| *b != "HEAD")
        .map(|b| b.to_string())
        .collect();
    Ok(branches)
}

/// Resolve which pool to operate on.
///
/// Priority: 1) explicit -p flag, 2) cwd under pool_dir, 3) cwd under a known clone path.
fn resolve_pool(config: &Config, state: &State, cli_pool: Option<&str>) -> Result<String> {
    // 1. Explicit flag
    if let Some(name) = cli_pool {
        if config.pools.contains_key(name) {
            return Ok(name.to_string());
        }
        let available: Vec<_> = config.pools.keys().cloned().collect();
        bail!(
            "Pool '{}' not found. Available pools: {}",
            name,
            available.join(", ")
        );
    }

    let cwd = std::env::current_dir()?;

    // 2a. Check if cwd is under pool_dir(config, name) for any pool
    for name in config.pools.keys() {
        let pd = pool_dir(config, name);
        if cwd.starts_with(&pd) {
            return Ok(name.clone());
        }
    }

    // 2b. Check if cwd starts_with any clone's path in state
    for (pool_name, pool_state) in &state.pools {
        if !config.pools.contains_key(pool_name) {
            continue;
        }
        for clone_state in pool_state.clones.values() {
            if cwd.starts_with(&clone_state.path) {
                return Ok(pool_name.clone());
            }
        }
    }

    // 2c. Bail with helpful message
    bail!(
        "Could not detect pool from current directory.\n\
         Use -p <pool> to specify a pool, or cd to a pool directory.\n\
         Current dir: {}",
        cwd.display()
    )
}

fn get_remote_url(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(path)
        .output()
        .context("Failed to run git remote get-url")?;

    if !output.status.success() {
        bail!("Failed to get remote URL");
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn extract_github_repo(url: &str) -> Option<String> {
    // Handle SSH format: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let repo = rest.trim_end_matches(".git");
        return Some(repo.to_string());
    }
    // Handle HTTPS format: https://github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        let repo = rest.trim_end_matches(".git");
        return Some(repo.to_string());
    }
    None
}

fn run_git(args: &[&str], cwd: &Path) -> Result<()> {
    // Capture git's stdout and forward it to stderr so it doesn't pollute
    // the stdout path that the shell integration captures for cd.
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;

    if !output.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn run_git_output(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn has_uncommitted_changes(path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .with_context(|| format!("Failed to run git status in {}", path.display()))?;

    if !output.status.success() {
        bail!(
            "git status failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(!output.stdout.is_empty())
}

/// Files/directories to copy between clones (often gitignored dev configs)
const DEV_CONFIG_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];
const DEV_CONFIG_DIRS: &[&str] = &[".cargo"];

/// Copy dev config files from source clone to target clone
fn copy_dev_configs(source: &Path, target: &Path) -> Result<()> {
    // Copy individual files
    for file in DEV_CONFIG_FILES {
        let src = source.join(file);
        let dst = target.join(file);
        if src.exists() && src.is_file() {
            std::fs::copy(&src, &dst).with_context(|| format!("Failed to copy {}", file))?;
            eprintln!("  Copied {}", file);
        }
    }

    // Copy directories
    for dir in DEV_CONFIG_DIRS {
        let src = source.join(dir);
        let dst = target.join(dir);
        if src.exists() && src.is_dir() {
            copy_dir_recursive(&src, &dst).with_context(|| format!("Failed to copy {}", dir))?;
            eprintln!("  Copied {}/", dir);
        }
    }

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Find a clone that has dev config files to copy from
fn find_dev_config_source(pool_state: &PoolState, exclude: &Path) -> Option<PathBuf> {
    for clone_state in pool_state.clones.values() {
        if clone_state.path == exclude {
            continue;
        }
        // Check if this clone has any dev config files
        for file in DEV_CONFIG_FILES {
            if clone_state.path.join(file).exists() {
                return Some(clone_state.path.clone());
            }
        }
        for dir in DEV_CONFIG_DIRS {
            if clone_state.path.join(dir).exists() {
                return Some(clone_state.path.clone());
            }
        }
    }
    None
}

fn cmd_init(
    repo: Option<String>,
    clones_root_override: Option<PathBuf>,
    name: Option<String>,
    build_cmd: Option<String>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;

    // Determine repo URL
    let repo_url = match repo {
        Some(url) => url,
        None => get_remote_url(&cwd).context("No --repo specified and not in a git repository")?,
    };

    // Determine pool name
    let pool_name = match name {
        Some(n) => n,
        None => {
            // Extract from repo URL or current dir name
            let url_name = repo_url
                .rsplit('/')
                .next()
                .unwrap_or("pool")
                .trim_end_matches(".git");
            url_name.to_string()
        }
    };

    validate_name(&pool_name, "Pool")?;

    let github_repo = extract_github_repo(&repo_url);
    let build_command = build_cmd.unwrap_or_else(default_build_command);

    let mut config = load_config()?;

    if config.pools.contains_key(&pool_name) {
        bail!("Pool '{}' already exists", pool_name);
    }

    // Set clones_root if overridden
    if let Some(root) = clones_root_override {
        config.clones_root = root;
    }

    // Compute and create pool directory
    let pd = pool_dir(&config, &pool_name);
    std::fs::create_dir_all(&pd)?;

    config.pools.insert(
        pool_name.clone(),
        PoolConfig {
            repo_url: repo_url.clone(),
            base_dir: None,
            github_repo: github_repo.clone(),
            build_command: build_command.clone(),
        },
    );

    save_config(&config)?;

    // Initialize state for this pool
    let mut state = load_state()?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    // If run from within an existing git repo, register it as first clone
    if cwd.join(".git").exists() {
        if let Ok(url) = get_remote_url(&cwd) {
            if url == repo_url {
                let clone_name = cwd.file_name().unwrap().to_string_lossy().to_string();
                if !pool_state.clones.contains_key(&clone_name) {
                    let branch =
                        run_git_output(&["rev-parse", "--abbrev-ref", "HEAD"], &cwd).ok();
                    pool_state.clones.insert(
                        clone_name.clone(),
                        CloneState {
                            path: cwd.clone(),
                            assigned_branch: branch,
                            last_used: Utc::now(),
                            pinned: false,
                        },
                    );
                    eprintln!("  Registered current repo as clone: {}", clone_name);
                }
            }
        }
    }

    // Scan pool dir for additional existing clones
    if pd.exists() {
        for entry in std::fs::read_dir(&pd)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join(".git").exists() {
                if let Ok(url) = get_remote_url(&path) {
                    if url == repo_url {
                        let clone_name = path.file_name().unwrap().to_string_lossy().to_string();
                        if !pool_state.clones.contains_key(&clone_name) {
                            let branch =
                                run_git_output(&["rev-parse", "--abbrev-ref", "HEAD"], &path).ok();
                            pool_state.clones.insert(
                                clone_name.clone(),
                                CloneState {
                                    path: path.clone(),
                                    assigned_branch: branch,
                                    last_used: Utc::now(),
                                    pinned: false,
                                },
                            );
                            eprintln!("  Found existing clone: {}", clone_name);
                        }
                    }
                }
            }
        }
    }

    let clone_count = pool_state.clones.len();
    save_state(&state)?;

    eprintln!("Initialized pool '{}'", pool_name);
    eprintln!("  Clones root: {}", config.clones_root.display());
    eprintln!("  Pool directory: {}", pd.display());
    if let Some(gh) = github_repo {
        eprintln!("  GitHub repo: {}", gh);
    }
    eprintln!("  Build command: {}", build_command);
    eprintln!("  Clones found: {}", clone_count);

    Ok(())
}

fn cmd_build(pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let pool_config = &config.pools[&pool_name];

    eprintln!("Running: {}", pool_config.build_command);

    let status = if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", &pool_config.build_command])
            .status()
    } else {
        Command::new("sh")
            .args(["-c", &pool_config.build_command])
            .status()
    }
    .context("Failed to run build command")?;

    if !status.success() {
        bail!("Build command failed");
    }

    Ok(())
}

fn cmd_checkout(branch: String, no_submodules: bool, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let mut state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let _pool_config = &config.pools[&pool_name];
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    // Check if branch is already assigned to a clone
    let existing = pool_state
        .clones
        .iter()
        .find(|(_, cs)| cs.assigned_branch.as_ref() == Some(&branch))
        .map(|(name, cs)| (name.clone(), cs.path.clone()));

    if let Some((clone_name, path)) = existing {
        // Verify the clone is actually on the recorded branch
        let actual_branch =
            run_git_output(&["rev-parse", "--abbrev-ref", "HEAD"], &path).ok();

        if actual_branch.as_ref() == Some(&branch) {
            eprintln!("Branch '{}' already on clone: {}", branch, clone_name);

            // Pull latest before returning
            eprintln!("Pulling latest...");
            if let Err(e) = run_git(&["fetch", "origin"], &path) {
                eprintln!("Warning: fetch failed: {}", e);
            }
            if let Err(e) = run_git(&["pull", "--ff-only"], &path) {
                eprintln!("Warning: pull failed: {}", e);
            }

            // Update submodules on fast path too
            if !no_submodules {
                eprintln!("Updating submodules...");
                if let Err(e) = run_git(&["submodule", "update", "--init", "--recursive"], &path) {
                    eprintln!("Warning: submodule update failed: {}", e);
                }
            }

            // Update last_used
            pool_state.clones.get_mut(&clone_name).unwrap().last_used = Utc::now();
            save_state(&state)?;

            // Output path for shell integration
            println!("{}", path.display());
            return Ok(());
        }

        // State is stale: clone is actually on a different branch. Correct it.
        eprintln!(
            "Clone '{}' was recorded as '{}' but is actually on '{}'. Correcting state.",
            clone_name,
            branch,
            actual_branch.as_deref().unwrap_or("unknown"),
        );
        pool_state.clones.get_mut(&clone_name).unwrap().assigned_branch =
            actual_branch;
    }

    // Find a clean (unassigned or LRU) clone, skipping dirty ones
    let clone_name = select_clean_clone(pool_state)?;
    let path = pool_state.clones[&clone_name].path.clone();

    // Find dev config source before we start modifying things
    let dev_config_source = find_dev_config_source(pool_state, &path);

    eprintln!("Using clone: {}", clone_name);

    // Fetch and checkout
    eprintln!("Fetching...");
    run_git(&["fetch", "origin"], &path)?;

    eprintln!("Checking out {}...", branch);
    // Try to checkout, creating tracking branch if needed
    let checkout_result = run_git(&["checkout", &branch], &path);
    if checkout_result.is_err() {
        // Try to create tracking branch
        run_git(
            &["checkout", "-b", &branch, &format!("origin/{}", branch)],
            &path,
        )?;
    } else {
        // Pull latest
        let _ = run_git(&["pull", "--ff-only"], &path);
    }

    // Update state now that checkout succeeded (before submodules, so state is
    // correct even if submodule update fails).
    let clone_state = pool_state.clones.get_mut(&clone_name).unwrap();
    clone_state.assigned_branch = Some(branch);
    clone_state.last_used = Utc::now();
    save_state(&state)?;

    if !no_submodules {
        eprintln!("Updating submodules...");
        run_git(&["submodule", "update", "--init", "--recursive"], &path)?;
    }

    // Copy dev config files from another clone if available
    if let Some(source) = dev_config_source {
        eprintln!("Copying dev configs from {}...", source.display());
        if let Err(e) = copy_dev_configs(&source, &path) {
            eprintln!("  Warning: {}", e);
        }
    }

    // Output just the path for shell integration
    println!("{}", path.display());
    Ok(())
}

fn cmd_status(pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let state = load_state()?;

    if config.pools.is_empty() {
        eprintln!("No pools configured. Run 'rpool init' in a repository.");
        return Ok(());
    }

    // If -p given, filter to that pool; otherwise show all
    let filter_pool = pool
        .as_ref()
        .map(|p| resolve_pool(&config, &state, Some(p)))
        .transpose()?;

    for (pool_name, _pool_config) in &config.pools {
        if let Some(ref fp) = filter_pool {
            if pool_name != fp {
                continue;
            }
        }

        println!(
            "Pool: {} ({})",
            pool_name,
            pool_dir(&config, pool_name).display()
        );

        if let Some(pool_state) = state.pools.get(pool_name) {
            if pool_state.clones.is_empty() {
                println!("  No clones");
            } else {
                let mut clones: Vec<_> = pool_state.clones.iter().collect();
                clones.sort_by_key(|(_, cs)| std::cmp::Reverse(cs.last_used));

                for (clone_name, clone_state) in clones {
                    let recorded = clone_state
                        .assigned_branch
                        .as_deref()
                        .unwrap_or("(unassigned)");
                    // Show actual branch if it differs from recorded state
                    let actual = run_git_output(
                        &["rev-parse", "--abbrev-ref", "HEAD"],
                        &clone_state.path,
                    )
                    .ok();
                    let branch_display = match actual.as_deref() {
                        Some(actual) if clone_state.assigned_branch.is_some() && actual != recorded => {
                            format!("{} (actual: {})", recorded, actual)
                        }
                        _ => recorded.to_string(),
                    };
                    let pinned_marker = if clone_state.pinned {
                        " [pinned]"
                    } else {
                        ""
                    };
                    let dirty = match has_uncommitted_changes(&clone_state.path) {
                        Ok(true) => " [dirty]",
                        Ok(false) => "",
                        Err(_) => " [error reading clone]",
                    };
                    println!(
                        "  {} -> {}{}{}",
                        clone_name, branch_display, pinned_marker, dirty
                    );
                }
            }
        } else {
            println!("  No state");
        }
        println!();
    }

    Ok(())
}

fn cmd_drop(clone: Option<String>, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let mut state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    let clone_name = match clone {
        Some(name) => name,
        None => {
            // Detect from cwd
            let cwd = std::env::current_dir()?;
            pool_state
                .clones
                .iter()
                .find(|(_, cs)| cwd.starts_with(&cs.path))
                .map(|(name, _)| name.clone())
                .ok_or_else(|| anyhow!("Not in a clone directory. Specify clone name."))?
        }
    };

    let clone_state = pool_state
        .clones
        .get_mut(&clone_name)
        .ok_or_else(|| anyhow!("Clone '{}' not found", clone_name))?;

    let old_branch = clone_state.assigned_branch.take();
    save_state(&state)?;

    if let Some(branch) = old_branch {
        eprintln!("Dropped assignment: {} -> {}", clone_name, branch);
    } else {
        eprintln!("Clone '{}' was not assigned", clone_name);
    }

    Ok(())
}

fn cmd_pr(number: u64, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let pool_config = &config.pools[&pool_name];

    let github_repo = pool_config
        .github_repo
        .as_ref()
        .ok_or_else(|| anyhow!("No GitHub repo configured for pool '{}'", pool_name))?;

    // Fetch PR info from GitHub API
    eprintln!("Fetching PR #{}...", number);
    let url = format!(
        "https://api.github.com/repos/{}/pulls/{}",
        github_repo, number
    );

    let mut curl_args = vec![
        "-s".to_string(),
        "-L".to_string(),
        "--fail".to_string(),
        "-H".to_string(),
        "Accept: application/vnd.github+json".to_string(),
    ];

    // Use GITHUB_TOKEN if available (required for private repos and avoids rate limits)
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        curl_args.push("-H".to_string());
        curl_args.push(format!("Authorization: Bearer {}", token));
    }

    curl_args.push(url);

    let output = Command::new("curl")
        .args(&curl_args)
        .output()
        .context("Failed to fetch PR info")?;

    if !output.status.success() {
        bail!(
            "Failed to fetch PR #{} (HTTP error). Check that the PR exists and the repo is correct.",
            number
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    // Check for API error message
    if let Some(msg) = json["message"].as_str() {
        bail!("GitHub API error for PR #{}: {}", number, msg);
    }

    let branch = json["head"]["ref"]
        .as_str()
        .ok_or_else(|| anyhow!("Could not find branch name in PR response"))?;

    // Detect fork PRs: head repo differs from base repo
    let is_fork = json["head"]["repo"]["full_name"] != json["base"]["repo"]["full_name"];

    if is_fork {
        eprintln!("PR #{} is a fork PR on branch: {}", number, branch);
        // For fork PRs, fetch via refs/pull/<n>/head since origin won't have the branch
        checkout_pr_ref(number, branch, &pool_name)
    } else {
        eprintln!("PR #{} is on branch: {}", number, branch);
        cmd_checkout(branch.to_string(), false, Some(pool_name))
    }
}

/// Select a clean (no uncommitted changes) clone, preferring unassigned, then LRU.
fn select_clean_clone(pool_state: &PoolState) -> Result<String> {
    if pool_state.clones.is_empty() {
        bail!("No clones in pool. Run 'rpool new' first.");
    }

    // Prefer unassigned clones, then LRU assigned clones.
    // Within each group, deprioritize pinned clones (unpinned first).
    let mut candidates: Vec<(&String, &CloneState)> = pool_state.clones.iter().collect();
    candidates.sort_by(|(_, a), (_, b)| {
        let a_assigned = a.assigned_branch.is_some() as u8;
        let b_assigned = b.assigned_branch.is_some() as u8;
        a_assigned
            .cmp(&b_assigned)
            .then((a.pinned as u8).cmp(&(b.pinned as u8)))
            .then(a.last_used.cmp(&b.last_used))
    });

    for (name, cs) in &candidates {
        match has_uncommitted_changes(&cs.path) {
            Ok(false) => return Ok((*name).clone()),
            Ok(true) => continue,
            Err(_) => continue,
        }
    }

    bail!(
        "All clones have uncommitted changes. Commit or stash changes in at least one clone."
    );
}

/// Checkout a fork PR by fetching refs/pull/<n>/head directly.
fn checkout_pr_ref(pr_number: u64, _branch: &str, pool_name: &str) -> Result<()> {
    let local_branch = format!("pr/{}", pr_number);

    // Reuse existing assignment if this PR is already checked out
    let mut state = load_state()?;
    let pool_state = state.pools.entry(pool_name.to_string()).or_default();

    let existing = pool_state
        .clones
        .iter()
        .find(|(_, cs)| cs.assigned_branch.as_deref() == Some(local_branch.as_str()))
        .map(|(name, cs)| (name.clone(), cs.path.clone()));

    if let Some((clone_name, path)) = existing {
        eprintln!("PR #{} already on clone: {}", pr_number, clone_name);
        pool_state.clones.get_mut(&clone_name).unwrap().last_used = Utc::now();
        save_state(&state)?;
        println!("{}", path.display());
        return Ok(());
    }

    // Find an unassigned clone or LRU, skipping dirty clones
    let clone_name = select_clean_clone(pool_state)?;

    let path = pool_state.clones[&clone_name].path.clone();
    let dev_config_source = find_dev_config_source(pool_state, &path);

    eprintln!("Using clone: {}", clone_name);
    eprintln!("Fetching PR #{}...", pr_number);

    let refspec = format!("refs/pull/{}/head:{}", pr_number, local_branch);
    run_git(&["fetch", "origin", &refspec], &path)?;
    run_git(&["checkout", &local_branch], &path)?;

    let clone_state = pool_state.clones.get_mut(&clone_name).unwrap();
    clone_state.assigned_branch = Some(local_branch);
    clone_state.last_used = Utc::now();
    save_state(&state)?;

    eprintln!("Updating submodules...");
    run_git(&["submodule", "update", "--init", "--recursive"], &path)?;

    if let Some(source) = dev_config_source {
        eprintln!("Copying dev configs from {}...", source.display());
        if let Err(e) = copy_dev_configs(&source, &path) {
            eprintln!("  Warning: {}", e);
        }
    }

    println!("{}", path.display());
    Ok(())
}

fn cmd_sync() -> Result<()> {
    let config = load_config()?;
    let state = load_state()?;
    let mut cache = load_branch_cache()?;
    let mut had_errors = false;

    for (pool_name, pool_state) in &state.pools {
        if !config.pools.contains_key(pool_name) {
            continue;
        }

        eprintln!("Syncing pool: {}", pool_name);
        let mut first_ok_clone: Option<&Path> = None;
        for (clone_name, clone_state) in &pool_state.clones {
            eprint!("  {} ... ", clone_name);
            match run_git(&["fetch", "--all", "--prune"], &clone_state.path) {
                Ok(_) => {
                    eprintln!("ok");
                    if first_ok_clone.is_none() {
                        first_ok_clone = Some(&clone_state.path);
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    had_errors = true;
                }
            }
        }

        // Update branch cache from the first successfully-fetched clone
        if let Some(clone_path) = first_ok_clone {
            match collect_remote_branches(clone_path) {
                Ok(branches) => {
                    eprintln!("  Cached {} branches for {}", branches.len(), pool_name);
                    cache.pools.insert(pool_name.clone(), branches);
                }
                Err(e) => eprintln!("  Warning: could not cache branches: {}", e),
            }
        }
    }

    save_branch_cache(&cache)?;

    if had_errors {
        bail!("Some fetches failed (see errors above)");
    }

    Ok(())
}

fn cmd_new(name: Option<String>, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let mut state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let pool_config = &config.pools[&pool_name];
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    // Generate clone name
    let clone_name = match name {
        Some(n) => {
            validate_name(&n, "Clone")?;
            n
        }
        None => {
            let mut i = pool_state.clones.len() + 1;
            loop {
                let candidate = format!("{}-{}", pool_name, i);
                if !pool_state.clones.contains_key(&candidate) {
                    break candidate;
                }
                i += 1;
            }
        }
    };

    if pool_state.clones.contains_key(&clone_name) {
        bail!("Clone '{}' already exists", clone_name);
    }

    let clone_path = pool_dir(&config, &pool_name).join(&clone_name);
    if clone_path.exists() {
        bail!("Directory already exists: {}", clone_path.display());
    }

    eprintln!("Creating clone: {}", clone_name);
    eprintln!("  Path: {}", clone_path.display());

    // Find a reference repo for --reference
    let reference = pool_state.clones.values().next().map(|cs| cs.path.clone());

    // Clone with --reference if possible
    let ref_path_str = reference.as_ref().map(|p| p.to_string_lossy().into_owned());
    let clone_path_str = clone_path.to_string_lossy().into_owned();

    let mut args = vec!["clone"];
    if let Some(ref rps) = ref_path_str {
        args.push("--reference");
        args.push(rps);
        eprintln!("  Using reference: {}", rps);
    }
    args.push(&pool_config.repo_url);
    args.push(&clone_path_str);

    // Capture stdout and forward to stderr so it doesn't pollute the path
    // that the shell integration captures for cd.
    let clone_output = Command::new("git")
        .args(&args)
        .output()
        .context("Failed to run git clone")?;

    if !clone_output.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&clone_output.stdout));
    }
    if !clone_output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&clone_output.stderr));
    }

    if !clone_output.status.success() {
        bail!("git clone failed");
    }

    // Initialize submodules
    eprintln!("Initializing submodules...");
    run_git(
        &["submodule", "update", "--init", "--recursive"],
        &clone_path,
    )?;

    // Add to state
    pool_state.clones.insert(
        clone_name.clone(),
        CloneState {
            path: clone_path.clone(),
            assigned_branch: None,
            last_used: Utc::now(),
            pinned: false,
        },
    );
    save_state(&state)?;

    eprintln!("Clone '{}' created successfully", clone_name);
    println!("{}", clone_path.display());

    Ok(())
}

fn cmd_pools() -> Result<()> {
    let config = load_config()?;

    if config.pools.is_empty() {
        eprintln!("No pools configured.");
        return Ok(());
    }

    println!("Clones root: {}", config.clones_root.display());
    println!();

    for (name, pool_config) in &config.pools {
        println!("{}", name);
        println!("  URL: {}", pool_config.repo_url);
        println!("  Pool dir: {}", pool_dir(&config, name).display());
        if let Some(gh) = &pool_config.github_repo {
            println!("  GitHub: {}", gh);
        }
        println!("  Build: {}", pool_config.build_command);
        println!();
    }

    Ok(())
}

fn cmd_cd(name: String, pool: Option<String>) -> Result<()> {
    let state = load_state()?;

    // If -p given, scope to that pool
    if let Some(ref pool_name) = pool {
        let pool_state = state
            .pools
            .get(pool_name)
            .ok_or_else(|| anyhow!("Pool '{}' not found", pool_name))?;
        let cs = pool_state
            .clones
            .get(&name)
            .ok_or_else(|| anyhow!("No clone '{}' in pool '{}'", name, pool_name))?;
        println!("{}", cs.path.display());
        return Ok(());
    }

    let mut matches: Vec<(&str, &Path)> = Vec::new();
    for (pool_name, pool_state) in &state.pools {
        if let Some(cs) = pool_state.clones.get(&name) {
            matches.push((pool_name, &cs.path));
        }
    }

    match matches.len() {
        0 => bail!("No clone named '{}' found in any pool", name),
        1 => {
            println!("{}", matches[0].1.display());
            Ok(())
        }
        _ => {
            let options: Vec<String> = matches
                .iter()
                .map(|(pool, path)| format!("  {} (pool: {})", path.display(), pool))
                .collect();
            bail!(
                "Clone '{}' exists in multiple pools. Use -p <pool> to disambiguate:\n{}",
                name,
                options.join("\n")
            );
        }
    }
}

fn cmd_complete(kind: String, pool: Option<String>) -> Result<()> {
    match kind.as_str() {
        "pools" => {
            let config = load_config()?;
            for name in config.pools.keys() {
                println!("{}", name);
            }
        }
        "clones" => {
            let config = load_config()?;
            let state = load_state()?;
            // If -p given, scope to that pool; otherwise list all clones
            let pools_to_show: Vec<&String> = if let Some(ref p) = pool {
                config.pools.keys().filter(|k| *k == p).collect()
            } else {
                config.pools.keys().collect()
            };
            for pool_name in pools_to_show {
                if let Some(pool_state) = state.pools.get(pool_name) {
                    for clone_name in pool_state.clones.keys() {
                        println!("{}", clone_name);
                    }
                }
            }
        }
        "branches" => {
            let config = load_config()?;
            let state = load_state()?;
            let cache = load_branch_cache()?;

            // Determine which pool(s) to show branches for
            let pool_name = if let Some(ref p) = pool {
                Some(p.clone())
            } else {
                resolve_pool(&config, &state, None).ok()
            };

            if let Some(ref pn) = pool_name {
                // Try cache first
                if let Some(branches) = cache.pools.get(pn) {
                    for b in branches {
                        println!("{}", b);
                    }
                    return Ok(());
                }
                // Fallback: read local branches from first available clone
                if let Some(pool_state) = state.pools.get(pn) {
                    if let Some(cs) = pool_state.clones.values().next() {
                        if let Ok(output) = run_git_output(
                            &["branch", "--format=%(refname:short)"],
                            &cs.path,
                        ) {
                            for line in output.lines() {
                                println!("{}", line);
                            }
                        }
                    }
                }
            } else {
                // No pool resolved; dump all cached branches
                for branches in cache.pools.values() {
                    for b in branches {
                        println!("{}", b);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn cmd_completions(shell: String) -> Result<()> {
    match shell.as_str() {
        "bash" => print!("{}", bash_completion_script()),
        "zsh" => print!("{}", zsh_completion_script()),
        "fish" => bail!("Fish completions not yet implemented"),
        _ => bail!("Unknown shell '{}'. Supported: bash, zsh", shell),
    }
    Ok(())
}

fn bash_completion_script() -> &'static str {
    r#"_rpool() {
    local cur prev words cword
    _init_completion || return

    local subcmd="" pool_arg=""
    local i
    for ((i=1; i < cword; i++)); do
        case "${words[i]}" in
            -p|--pool) pool_arg="${words[$((i+1))]}"; ((i++)) ;;
            -*) ;;
            *) [[ -z "$subcmd" ]] && subcmd="${words[i]}" ;;
        esac
    done

    if [[ "$prev" == "-p" || "$prev" == "--pool" ]]; then
        COMPREPLY=($(compgen -W "$(rpool complete pools 2>/dev/null)" -- "$cur"))
        return
    fi

    if [[ -z "$subcmd" ]]; then
        COMPREPLY=($(compgen -W "init checkout ck status st drop pr sync new pools rm-pool build cd migrate pin unpin doctor completions" -- "$cur"))
        return
    fi

    local -a pool_flag=()
    [[ -n "$pool_arg" ]] && pool_flag=(-p "$pool_arg")

    case "$subcmd" in
        checkout|ck) COMPREPLY=($(compgen -W "$(rpool "${pool_flag[@]}" complete branches 2>/dev/null)" -- "$cur")) ;;
        cd)          COMPREPLY=($(compgen -W "$(rpool complete clones 2>/dev/null)" -- "$cur")) ;;
        drop)        COMPREPLY=($(compgen -W "$(rpool "${pool_flag[@]}" complete clones 2>/dev/null)" -- "$cur")) ;;
        pin)         COMPREPLY=($(compgen -W "$(rpool "${pool_flag[@]}" complete clones 2>/dev/null)" -- "$cur")) ;;
        unpin)       COMPREPLY=($(compgen -W "$(rpool "${pool_flag[@]}" complete clones 2>/dev/null)" -- "$cur")) ;;
        rm-pool)     COMPREPLY=($(compgen -W "$(rpool complete pools 2>/dev/null)" -- "$cur")) ;;
        completions) COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur")) ;;
    esac
}
complete -F _rpool rpool
complete -F _rpool rp
"#
}

fn zsh_completion_script() -> &'static str {
    r#"#compdef rpool rp

_rpool() {
    local -a subcmds
    subcmds=(
        'init:Initialize a pool for a repository'
        'checkout:Checkout a branch'
        'status:Show pool status'
        'drop:Unassign a clone'
        'pr:Checkout a PR by number'
        'sync:Fetch all remotes'
        'new:Add a new clone'
        'pools:List available pools'
        'rm-pool:Remove a pool'
        'build:Run build command'
        'cd:Navigate to a clone'
        'migrate:Migrate clones to structured directory'
        'pin:Pin a clone to deprioritize it for assignment'
        'unpin:Unpin a clone for free assignment'
        'doctor:Diagnose and fix state inconsistencies'
        'completions:Generate shell completions'
    )

    local pool_flag=""
    local -i i
    for ((i=1; i < CURRENT; i++)); do
        case "${words[i]}" in
            -p|--pool) pool_flag="-p ${words[$((i+1))]}" ;;
        esac
    done

    _arguments -C \
        '(-p --pool)'{-p,--pool}'[Pool name]:pool:->pool_arg' \
        '1:command:->subcmd' \
        '*::arg:->args'

    case $state in
        pool_arg)
            local -a pools
            pools=(${(f)"$(rpool complete pools 2>/dev/null)"})
            compadd -a pools
            ;;
        subcmd)
            _describe 'command' subcmds
            ;;
        args)
            case ${words[1]} in
                checkout|ck)
                    local -a branches
                    branches=(${(f)"$(rpool ${=pool_flag} complete branches 2>/dev/null)"})
                    compadd -a branches
                    ;;
                cd)
                    local -a clones
                    clones=(${(f)"$(rpool complete clones 2>/dev/null)"})
                    compadd -a clones
                    ;;
                drop|pin|unpin)
                    local -a clones
                    clones=(${(f)"$(rpool ${=pool_flag} complete clones 2>/dev/null)"})
                    compadd -a clones
                    ;;
                rm-pool)
                    local -a pools
                    pools=(${(f)"$(rpool complete pools 2>/dev/null)"})
                    compadd -a pools
                    ;;
                completions)
                    compadd bash zsh fish
                    ;;
            esac
            ;;
    esac
}

_rpool "$@"
"#
}

fn cmd_pin(clone: Option<String>, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let mut state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    let clone_name = match clone {
        Some(name) => name,
        None => {
            let cwd = std::env::current_dir()?;
            pool_state
                .clones
                .iter()
                .find(|(_, cs)| cwd.starts_with(&cs.path))
                .map(|(name, _)| name.clone())
                .ok_or_else(|| anyhow!("Not in a clone directory. Specify clone name."))?
        }
    };

    let clone_state = pool_state
        .clones
        .get_mut(&clone_name)
        .ok_or_else(|| anyhow!("Clone '{}' not found", clone_name))?;

    if clone_state.pinned {
        eprintln!("Clone '{}' is already pinned", clone_name);
    } else {
        clone_state.pinned = true;
        save_state(&state)?;
        eprintln!("Pinned clone '{}'", clone_name);
    }

    Ok(())
}

fn cmd_unpin(clone: Option<String>, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let mut state = load_state()?;
    let pool_name = resolve_pool(&config, &state, pool.as_deref())?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    let clone_name = match clone {
        Some(name) => name,
        None => {
            let cwd = std::env::current_dir()?;
            pool_state
                .clones
                .iter()
                .find(|(_, cs)| cwd.starts_with(&cs.path))
                .map(|(name, _)| name.clone())
                .ok_or_else(|| anyhow!("Not in a clone directory. Specify clone name."))?
        }
    };

    let clone_state = pool_state
        .clones
        .get_mut(&clone_name)
        .ok_or_else(|| anyhow!("Clone '{}' not found", clone_name))?;

    if !clone_state.pinned {
        eprintln!("Clone '{}' is not pinned", clone_name);
    } else {
        clone_state.pinned = false;
        save_state(&state)?;
        eprintln!("Unpinned clone '{}'", clone_name);
    }

    Ok(())
}

fn cmd_rm_pool(name: String) -> Result<()> {
    let mut config = load_config()?;
    let mut state = load_state()?;
    let mut cache = load_branch_cache()?;

    if config.pools.remove(&name).is_none() {
        bail!("Pool '{}' not found", name);
    }
    state.pools.remove(&name);
    cache.pools.remove(&name);

    save_config(&config)?;
    save_state(&state)?;
    save_branch_cache(&cache)?;

    eprintln!("Removed pool '{}' (clones on disk are preserved)", name);
    Ok(())
}

fn cmd_migrate(keep: Option<String>, clones_root_override: Option<PathBuf>) -> Result<()> {
    let mut config = load_config()?;
    let mut state = load_state()?;

    // Set clones_root if overridden
    if let Some(root) = clones_root_override {
        config.clones_root = root;
    }

    let clones_root = config.clones_root.clone();
    std::fs::create_dir_all(&clones_root)?;

    eprintln!("Migrating to clones root: {}", clones_root.display());

    // Collect rename operations: (pool_name, clone_name, new_path, needs_rename)
    let mut ops: Vec<(String, String, PathBuf, bool)> = Vec::new();

    for (pool_name, pool_state) in &state.pools {
        if !config.pools.contains_key(pool_name) {
            continue;
        }

        let pd = clones_root.join(pool_name);
        std::fs::create_dir_all(&pd)?;

        let keep_name = keep.clone().unwrap_or_else(|| pool_name.clone());

        eprintln!("\nPool: {}", pool_name);

        for (clone_name, clone_state) in &pool_state.clones {
            if *clone_name == keep_name {
                eprintln!(
                    "  {} -> kept in place ({})",
                    clone_name,
                    clone_state.path.display()
                );
                continue;
            }

            let new_path = pd.join(clone_name);

            if clone_state.path == new_path {
                eprintln!(
                    "  {} -> already at {}",
                    clone_name,
                    new_path.display()
                );
                continue;
            }

            if new_path.exists() {
                eprintln!(
                    "  {} -> WARNING: target already exists, skipping ({})",
                    clone_name,
                    new_path.display()
                );
                continue;
            }

            if !clone_state.path.exists() {
                eprintln!(
                    "  {} -> WARNING: source does not exist, updating path only ({})",
                    clone_name,
                    clone_state.path.display()
                );
                ops.push((pool_name.clone(), clone_name.clone(), new_path, false));
                continue;
            }

            eprintln!(
                "  {} -> moving {} -> {}",
                clone_name,
                clone_state.path.display(),
                new_path.display()
            );
            ops.push((pool_name.clone(), clone_name.clone(), new_path, true));
        }
    }

    // Execute renames, saving state after each so partial failures are recoverable
    for (pool_name, clone_name, new_path, needs_rename) in ops {
        if needs_rename {
            let old_path = &state.pools[&pool_name].clones[&clone_name].path;
            std::fs::rename(old_path, &new_path).with_context(|| {
                format!(
                    "Failed to move {} -> {}",
                    old_path.display(),
                    new_path.display()
                )
            })?;
        }
        state
            .pools
            .get_mut(&pool_name)
            .unwrap()
            .clones
            .get_mut(&clone_name)
            .unwrap()
            .path = new_path;
        save_state(&state)?;
    }

    // base_dir is dropped on save via skip_serializing
    save_config(&config)?;

    eprintln!("\nMigration complete.");
    Ok(())
}

fn cmd_doctor(fix: bool, pool: Option<String>) -> Result<()> {
    let config = load_config()?;
    let mut state = load_state()?;
    let mut issues: usize = 0;
    let mut fixed: usize = 0;

    // If -p was given, scope checks to that single pool
    let scoped_pool: Option<String> = match pool {
        Some(ref name) => {
            if !config.pools.contains_key(name) {
                bail!("Pool '{}' not found", name);
            }
            Some(name.clone())
        }
        None => None,
    };

    // Track clone names to remove after iteration (cannot mutate while iterating)
    let mut clones_to_remove: Vec<(String, String)> = Vec::new();
    // Track branch updates to apply after iteration
    let mut branch_updates: Vec<(String, String, Option<String>)> = Vec::new();

    // 1. Check for orphaned state entries (pool in state but not in config)
    //    Only when running globally (no -p filter)
    let orphaned_pools: Vec<String> = if scoped_pool.is_none() {
        state
            .pools
            .keys()
            .filter(|p| !config.pools.contains_key(*p))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    for pool_name in &orphaned_pools {
        issues += 1;
        eprintln!(
            "WARN: pool '{}' exists in state but not in config (orphaned)",
            pool_name
        );
        if fix {
            state.pools.remove(pool_name);
            fixed += 1;
            eprintln!("  FIXED: removed orphaned pool '{}' from state", pool_name);
        }
    }

    // 2. Walk all pools in config, check their clones in state
    for (pool_name, pool_config) in &config.pools {
        // If scoped to a single pool, skip others
        if let Some(ref scoped) = scoped_pool {
            if pool_name != scoped {
                continue;
            }
        }
        let pool_state = match state.pools.get(pool_name) {
            Some(ps) => ps,
            None => continue, // no state for this pool, nothing to check
        };

        // Track branches for duplicate detection within this pool
        let mut branch_owners: HashMap<String, Vec<String>> = HashMap::new();

        for (clone_name, clone_state) in &pool_state.clones {
            // 2a. Check if clone path exists on disk
            if !clone_state.path.exists() {
                issues += 1;
                eprintln!(
                    "WARN: [{}/{}] path does not exist: {}",
                    pool_name,
                    clone_name,
                    clone_state.path.display()
                );
                if fix {
                    clones_to_remove.push((pool_name.clone(), clone_name.clone()));
                    fixed += 1;
                    eprintln!(
                        "  FIXED: removed clone '{}' from pool '{}' state",
                        clone_name, pool_name
                    );
                }
                continue; // skip further checks for missing clones
            }

            // 2b. Check actual git branch vs assigned_branch
            if let Some(ref assigned) = clone_state.assigned_branch {
                match run_git_output(&["rev-parse", "--abbrev-ref", "HEAD"], &clone_state.path) {
                    Ok(actual) => {
                        if actual != *assigned {
                            issues += 1;
                            eprintln!(
                                "WARN: [{}/{}] branch mismatch: state says '{}', actual is '{}'",
                                pool_name, clone_name, assigned, actual
                            );
                            if fix {
                                let new_branch = if actual == "HEAD" {
                                    // Detached HEAD, clear the assignment
                                    None
                                } else {
                                    Some(actual)
                                };
                                branch_updates.push((
                                    pool_name.clone(),
                                    clone_name.clone(),
                                    new_branch.clone(),
                                ));
                                fixed += 1;
                                eprintln!(
                                    "  FIXED: updated [{}/{}] branch to '{}'",
                                    pool_name,
                                    clone_name,
                                    new_branch.as_deref().unwrap_or("(unassigned)")
                                );
                            }
                        }
                    }
                    Err(e) => {
                        issues += 1;
                        eprintln!(
                            "WARN: [{}/{}] could not read branch: {}",
                            pool_name, clone_name, e
                        );
                    }
                }
            }

            // 2c. Check clone remote URL matches pool's repo_url
            match get_remote_url(&clone_state.path) {
                Ok(actual_url) => {
                    if actual_url != pool_config.repo_url {
                        issues += 1;
                        eprintln!(
                            "WARN: [{}/{}] remote URL mismatch: pool expects '{}', clone has '{}'",
                            pool_name, clone_name, pool_config.repo_url, actual_url
                        );
                    }
                }
                Err(e) => {
                    issues += 1;
                    eprintln!(
                        "WARN: [{}/{}] could not read remote URL: {}",
                        pool_name, clone_name, e
                    );
                }
            }

            // Collect branch assignments for duplicate detection
            if let Some(ref branch) = clone_state.assigned_branch {
                branch_owners
                    .entry(branch.clone())
                    .or_default()
                    .push(clone_name.clone());
            }
        }

        // 2d. Check for duplicate branch assignments
        for (branch, owners) in &branch_owners {
            if owners.len() > 1 {
                issues += 1;
                eprintln!(
                    "WARN: [{}] branch '{}' is assigned to multiple clones: {}",
                    pool_name,
                    branch,
                    owners.join(", ")
                );
            }
        }
    }

    // Apply deferred mutations
    for (pool_name, clone_name) in clones_to_remove {
        if let Some(ps) = state.pools.get_mut(&pool_name) {
            ps.clones.remove(&clone_name);
        }
    }
    for (pool_name, clone_name, new_branch) in branch_updates {
        if let Some(ps) = state.pools.get_mut(&pool_name) {
            if let Some(cs) = ps.clones.get_mut(&clone_name) {
                cs.assigned_branch = new_branch;
            }
        }
    }

    if fix && fixed > 0 {
        save_state(&state)?;
    }

    // Print summary
    eprintln!();
    if issues == 0 {
        eprintln!("No issues found.");
    } else if fix {
        eprintln!("{} issue(s) found, {} fixed.", issues, fixed);
        let unfixed = issues - fixed;
        if unfixed > 0 {
            eprintln!(
                "{} issue(s) require manual intervention.",
                unfixed
            );
        }
    } else {
        eprintln!(
            "{} issue(s) found. Run with --fix to auto-repair what can be fixed.",
            issues
        );
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pool = cli.pool;

    // Commands that modify state.json acquire an exclusive advisory lock
    // before any load_state() call and hold it until the command finishes.
    // Read-only commands (Status, Complete, Cd, Pools, Build, Completions)
    // skip the lock.
    match cli.command {
        Commands::Init {
            repo,
            clones_root,
            name,
            build_cmd,
        } => {
            let _lock = lock_state()?;
            cmd_init(repo, clones_root, name, build_cmd)
        }
        Commands::Checkout {
            branch,
            no_submodules,
        } => {
            let _lock = lock_state()?;
            cmd_checkout(branch, no_submodules, pool)
        }
        Commands::Status => cmd_status(pool),
        Commands::Drop { clone } => {
            let _lock = lock_state()?;
            cmd_drop(clone, pool)
        }
        Commands::Pr { number } => {
            let _lock = lock_state()?;
            cmd_pr(number, pool)
        }
        Commands::Sync => {
            let _lock = lock_state()?;
            cmd_sync()
        }
        Commands::New { name } => {
            let _lock = lock_state()?;
            cmd_new(name, pool)
        }
        Commands::Pools => cmd_pools(),
        Commands::RmPool { name } => {
            let _lock = lock_state()?;
            cmd_rm_pool(name)
        }
        Commands::Build => cmd_build(pool),
        Commands::Cd { name } => cmd_cd(name, pool),
        Commands::Completions { shell } => cmd_completions(shell),
        Commands::Complete { kind } => cmd_complete(kind, pool),
        Commands::Migrate { keep, clones_root } => {
            let _lock = lock_state()?;
            cmd_migrate(keep, clones_root)
        }
        Commands::Pin { clone } => {
            let _lock = lock_state()?;
            cmd_pin(clone, pool)
        }
        Commands::Unpin { clone } => {
            let _lock = lock_state()?;
            cmd_unpin(clone, pool)
        }
        Commands::Doctor { fix } => {
            let _lock = lock_state()?;
            cmd_doctor(fix, pool)
        }
    }
}
