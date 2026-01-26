use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(name = "rpool", about = "Manage a pool of repository clones")]
struct Cli {
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
        /// Base directory for the pool (default: parent of current dir)
        #[arg(short, long)]
        base: Option<PathBuf>,
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
}

#[derive(Debug, Serialize, Deserialize)]
#[derive(Default)]
struct Config {
    pools: HashMap<String, PoolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoolConfig {
    /// Origin repository URL
    repo_url: String,
    /// Base directory containing the clones
    base_dir: PathBuf,
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
    std::fs::write(path, contents)?;
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
    std::fs::write(path, contents)?;
    Ok(())
}

fn detect_pool_from_cwd(config: &Config) -> Result<String> {
    let cwd = std::env::current_dir()?;

    for (name, pool_config) in &config.pools {
        // Check if cwd is under the pool's base_dir
        if cwd.starts_with(&pool_config.base_dir) {
            return Ok(name.clone());
        }
    }

    bail!(
        "Not in a known pool directory. Run 'rpool init' first or cd to a pool directory.\nCurrent dir: {}",
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
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;

    if !status.success() {
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
        .output()?;

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
    base: Option<PathBuf>,
    name: Option<String>,
    build_cmd: Option<String>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;

    // Determine repo URL
    let repo_url = match repo {
        Some(url) => url,
        None => get_remote_url(&cwd).context("No --repo specified and not in a git repository")?,
    };

    // Determine base directory
    let base_dir = match base {
        Some(b) => b,
        None => cwd
            .parent()
            .ok_or_else(|| anyhow!("Could not determine parent directory"))?
            .to_path_buf(),
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

    let github_repo = extract_github_repo(&repo_url);
    let build_command = build_cmd.unwrap_or_else(default_build_command);

    let mut config = load_config()?;

    if config.pools.contains_key(&pool_name) {
        bail!("Pool '{}' already exists", pool_name);
    }

    config.pools.insert(
        pool_name.clone(),
        PoolConfig {
            repo_url,
            base_dir: base_dir.clone(),
            github_repo: github_repo.clone(),
            build_command: build_command.clone(),
        },
    );

    save_config(&config)?;

    // Initialize state for this pool, detecting existing clones
    let mut state = load_state()?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    // Scan base_dir for existing clones
    if base_dir.exists() {
        for entry in std::fs::read_dir(&base_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join(".git").exists() {
                // Check if it's a clone of the same repo
                if let Ok(url) = get_remote_url(&path) {
                    if url.contains(&pool_name) || config.pools[&pool_name].repo_url == url {
                        let clone_name = path.file_name().unwrap().to_string_lossy().to_string();

                        // Get current branch
                        let branch =
                            run_git_output(&["rev-parse", "--abbrev-ref", "HEAD"], &path).ok();

                        pool_state.clones.insert(
                            clone_name.clone(),
                            CloneState {
                                path: path.clone(),
                                assigned_branch: branch,
                                last_used: Utc::now(),
                            },
                        );
                        eprintln!("  Found existing clone: {}", clone_name);
                    }
                }
            }
        }
    }

    let clone_count = pool_state.clones.len();
    save_state(&state)?;

    eprintln!("Initialized pool '{}'", pool_name);
    eprintln!("  Base directory: {}", base_dir.display());
    if let Some(gh) = github_repo {
        eprintln!("  GitHub repo: {}", gh);
    }
    eprintln!("  Build command: {}", build_command);
    eprintln!("  Clones found: {}", clone_count);

    Ok(())
}

fn cmd_build() -> Result<()> {
    let config = load_config()?;
    let pool_name = detect_pool_from_cwd(&config)?;
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

fn cmd_checkout(branch: String, no_submodules: bool) -> Result<()> {
    let config = load_config()?;
    let pool_name = detect_pool_from_cwd(&config)?;
    let _pool_config = &config.pools[&pool_name];

    let mut state = load_state()?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    // Check if branch is already assigned to a clone
    let existing = pool_state
        .clones
        .iter()
        .find(|(_, cs)| cs.assigned_branch.as_ref() == Some(&branch))
        .map(|(name, cs)| (name.clone(), cs.path.clone()));

    if let Some((clone_name, path)) = existing {
        eprintln!("Branch '{}' already on clone: {}", branch, clone_name);

        // Update last_used
        pool_state.clones.get_mut(&clone_name).unwrap().last_used = Utc::now();
        save_state(&state)?;

        // Output path for shell integration
        println!("{}", path.display());
        return Ok(());
    }

    // Find an unassigned clone or the LRU one
    let clone_name = {
        // First, look for unassigned clone
        let unassigned = pool_state
            .clones
            .iter()
            .find(|(_, cs)| cs.assigned_branch.is_none())
            .map(|(name, _)| name.clone());

        if let Some(name) = unassigned {
            name
        } else if !pool_state.clones.is_empty() {
            // Find LRU clone
            pool_state
                .clones
                .iter()
                .min_by_key(|(_, cs)| cs.last_used)
                .map(|(name, _)| name.clone())
                .unwrap()
        } else {
            bail!("No clones in pool. Run 'rpool new' first.");
        }
    };

    let path = pool_state.clones[&clone_name].path.clone();

    // Check for uncommitted changes
    if has_uncommitted_changes(&path)? {
        bail!(
            "Clone '{}' has uncommitted changes. Commit or stash them first.",
            clone_name
        );
    }

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

    // Update state
    let clone_state = pool_state.clones.get_mut(&clone_name).unwrap();
    clone_state.assigned_branch = Some(branch);
    clone_state.last_used = Utc::now();
    save_state(&state)?;

    // Output just the path for shell integration
    println!("{}", path.display());
    Ok(())
}

fn cmd_status() -> Result<()> {
    let config = load_config()?;
    let state = load_state()?;

    if config.pools.is_empty() {
        eprintln!("No pools configured. Run 'rpool init' in a repository.");
        return Ok(());
    }

    for (pool_name, pool_config) in &config.pools {
        println!("Pool: {} ({})", pool_name, pool_config.base_dir.display());

        if let Some(pool_state) = state.pools.get(pool_name) {
            if pool_state.clones.is_empty() {
                println!("  No clones");
            } else {
                let mut clones: Vec<_> = pool_state.clones.iter().collect();
                clones.sort_by_key(|(_, cs)| std::cmp::Reverse(cs.last_used));

                for (clone_name, clone_state) in clones {
                    let branch_display = clone_state
                        .assigned_branch
                        .as_deref()
                        .unwrap_or("(unassigned)");
                    let dirty = if has_uncommitted_changes(&clone_state.path).unwrap_or(false) {
                        " [dirty]"
                    } else {
                        ""
                    };
                    println!("  {} -> {}{}", clone_name, branch_display, dirty);
                }
            }
        } else {
            println!("  No state");
        }
        println!();
    }

    Ok(())
}

fn cmd_drop(clone: Option<String>) -> Result<()> {
    let config = load_config()?;
    let pool_name = detect_pool_from_cwd(&config)?;

    let mut state = load_state()?;
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

fn cmd_pr(number: u64) -> Result<()> {
    let config = load_config()?;
    let pool_name = detect_pool_from_cwd(&config)?;
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

    let output = Command::new("curl")
        .args([
            "-s",
            "-L",
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ])
        .output()
        .context("Failed to fetch PR info")?;

    if !output.status.success() {
        bail!("Failed to fetch PR info");
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let branch = json["head"]["ref"]
        .as_str()
        .ok_or_else(|| anyhow!("Could not find branch name in PR response"))?;

    eprintln!("PR #{} is on branch: {}", number, branch);

    // Delegate to checkout
    cmd_checkout(branch.to_string(), false)
}

fn cmd_sync() -> Result<()> {
    let config = load_config()?;
    let state = load_state()?;

    for (pool_name, pool_state) in &state.pools {
        if !config.pools.contains_key(pool_name) {
            continue;
        }

        eprintln!("Syncing pool: {}", pool_name);
        for (clone_name, clone_state) in &pool_state.clones {
            eprint!("  {} ... ", clone_name);
            match run_git(&["fetch", "--all", "--prune"], &clone_state.path) {
                Ok(_) => eprintln!("ok"),
                Err(e) => eprintln!("error: {}", e),
            }
        }
    }

    Ok(())
}

fn cmd_new(name: Option<String>) -> Result<()> {
    let config = load_config()?;
    let pool_name = detect_pool_from_cwd(&config)?;
    let pool_config = &config.pools[&pool_name];

    let mut state = load_state()?;
    let pool_state = state.pools.entry(pool_name.clone()).or_default();

    // Generate clone name
    let clone_name = match name {
        Some(n) => n,
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

    let clone_path = pool_config.base_dir.join(&clone_name);
    if clone_path.exists() {
        bail!("Directory already exists: {}", clone_path.display());
    }

    eprintln!("Creating clone: {}", clone_name);
    eprintln!("  Path: {}", clone_path.display());

    // Find a reference repo for --reference
    let reference = pool_state.clones.values().next().map(|cs| cs.path.clone());

    // Clone with --reference if possible
    let mut args = vec!["clone"];
    if let Some(ref_path) = &reference {
        args.push("--reference");
        args.push(ref_path.to_str().unwrap());
        eprintln!("  Using reference: {}", ref_path.display());
    }
    args.push(&pool_config.repo_url);
    args.push(clone_path.to_str().unwrap());

    let status = Command::new("git")
        .args(&args)
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
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

    for (name, pool_config) in &config.pools {
        println!("{}", name);
        println!("  URL: {}", pool_config.repo_url);
        println!("  Base: {}", pool_config.base_dir.display());
        if let Some(gh) = &pool_config.github_repo {
            println!("  GitHub: {}", gh);
        }
        println!("  Build: {}", pool_config.build_command);
        println!();
    }

    Ok(())
}

fn cmd_rm_pool(name: String) -> Result<()> {
    let mut config = load_config()?;
    let mut state = load_state()?;

    if config.pools.remove(&name).is_none() {
        bail!("Pool '{}' not found", name);
    }
    state.pools.remove(&name);

    save_config(&config)?;
    save_state(&state)?;

    eprintln!("Removed pool '{}' (clones on disk are preserved)", name);
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init {
            repo,
            base,
            name,
            build_cmd,
        } => cmd_init(repo, base, name, build_cmd),
        Commands::Checkout {
            branch,
            no_submodules,
        } => cmd_checkout(branch, no_submodules),
        Commands::Status => cmd_status(),
        Commands::Drop { clone } => cmd_drop(clone),
        Commands::Pr { number } => cmd_pr(number),
        Commands::Sync => cmd_sync(),
        Commands::New { name } => cmd_new(name),
        Commands::Pools => cmd_pools(),
        Commands::RmPool { name } => cmd_rm_pool(name),
        Commands::Build => cmd_build(),
    }
}
