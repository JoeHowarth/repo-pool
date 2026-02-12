use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Data types mirroring main.rs (read-only, for assertions)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct Config {
    #[allow(dead_code)]
    clones_root: PathBuf,
    pools: HashMap<String, PoolConfig>,
}

#[derive(Debug, serde::Deserialize)]
struct PoolConfig {
    repo_url: String,
    #[allow(dead_code)]
    github_repo: Option<String>,
    #[allow(dead_code)]
    build_command: String,
}

#[derive(Debug, serde::Deserialize)]
struct State {
    pools: HashMap<String, PoolState>,
}

#[derive(Debug, serde::Deserialize)]
struct PoolState {
    clones: HashMap<String, CloneState>,
}

#[derive(Debug, serde::Deserialize)]
struct CloneState {
    path: PathBuf,
    assigned_branch: Option<String>,
    #[allow(dead_code)]
    last_used: String,
    #[serde(default)]
    pinned: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TestEnv {
    #[allow(dead_code)]
    tmpdir: TempDir,
    config_dir: PathBuf,  // XDG_CONFIG_HOME value
    rpool_dir: PathBuf,   // config_dir/rpool  (where config.json lives)
    home_dir: PathBuf,    // fake HOME
    bare_repo: PathBuf,   // path to the bare "origin" repo
    clones_root: PathBuf, // home_dir/rppool
}

impl TestEnv {
    /// Build a Command for the rpool binary with isolated env.
    fn cmd(&self, args: &[&str]) -> Command {
        let bin = env!("CARGO_BIN_EXE_rpool");
        let mut cmd = Command::new(bin);
        cmd.args(args);
        cmd.env("XDG_CONFIG_HOME", &self.config_dir);
        cmd.env("HOME", &self.home_dir);
        // Avoid git prompting for credentials
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    fn load_config(&self) -> Config {
        let path = self.rpool_dir.join("config.json");
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read config.json at {}: {}", path.display(), e));
        serde_json::from_str(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse config.json: {}", e))
    }

    fn load_state(&self) -> State {
        let path = self.rpool_dir.join("state.json");
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read state.json at {}: {}", path.display(), e));
        serde_json::from_str(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse state.json: {}", e))
    }
}

/// Run a `&mut Command` and assert it succeeds, returning stdout as a string.
fn run_ok(cmd: &mut Command) -> String {
    let output = cmd.output().expect("Failed to execute command");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "Command failed: {:?}\nstdout: {}\nstderr: {}",
            cmd, stdout, stderr
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Run a `&mut Command` and return (exit success, stdout, stderr).
fn run_capture(cmd: &mut Command) -> (bool, String, String) {
    let output = cmd.output().expect("Failed to execute command");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn git_current_branch(path: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()
        .expect("git rev-parse failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Create an isolated test environment with a bare git repo containing
/// a default branch (master/main) and two feature branches.
fn setup_test_env() -> TestEnv {
    let tmpdir = TempDir::new().expect("create tempdir");
    let base = tmpdir.path();

    let config_dir = base.join("config");
    let home_dir = base.join("home");
    let bare_repo = base.join("origin.git");
    let clones_root = home_dir.join("rppool");

    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();

    // Create a bare repository
    run_ok(Command::new("git").args(["init", "--bare"]).arg(&bare_repo));

    // Create a temporary working clone to push initial commits and branches
    let work = base.join("work");
    run_ok(Command::new("git").arg("clone").arg(&bare_repo).arg(&work));

    // Configure git identity in the working clone
    run_ok(
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&work),
    );
    run_ok(
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&work),
    );

    // Create initial commit
    std::fs::write(work.join("README.md"), "hello").unwrap();
    run_ok(
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&work),
    );
    run_ok(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&work),
    );

    // Determine the default branch name (master or main depending on git config)
    let default_branch = git_current_branch(&work);

    run_ok(
        Command::new("git")
            .args(["push", "origin", &default_branch])
            .current_dir(&work),
    );

    // Create feature-a branch
    run_ok(
        Command::new("git")
            .args(["checkout", "-b", "feature-a"])
            .current_dir(&work),
    );
    std::fs::write(work.join("a.txt"), "feature a").unwrap();
    run_ok(Command::new("git").args(["add", "a.txt"]).current_dir(&work));
    run_ok(
        Command::new("git")
            .args(["commit", "-m", "feature a"])
            .current_dir(&work),
    );
    run_ok(
        Command::new("git")
            .args(["push", "origin", "feature-a"])
            .current_dir(&work),
    );

    // Create feature-b branch (from the default branch)
    run_ok(
        Command::new("git")
            .args(["checkout", &default_branch])
            .current_dir(&work),
    );
    run_ok(
        Command::new("git")
            .args(["checkout", "-b", "feature-b"])
            .current_dir(&work),
    );
    std::fs::write(work.join("b.txt"), "feature b").unwrap();
    run_ok(Command::new("git").args(["add", "b.txt"]).current_dir(&work));
    run_ok(
        Command::new("git")
            .args(["commit", "-m", "feature b"])
            .current_dir(&work),
    );
    run_ok(
        Command::new("git")
            .args(["push", "origin", "feature-b"])
            .current_dir(&work),
    );

    let rpool_dir = config_dir.join("rpool");

    TestEnv {
        tmpdir,
        config_dir,
        rpool_dir,
        home_dir,
        bare_repo,
        clones_root,
    }
}

/// Initialize a pool with one clone already in the pool directory.
/// Returns the name of the pre-existing clone.
fn init_pool_with_clone(env: &TestEnv, pool_name: &str) -> String {
    let clone_name = format!("{}-1", pool_name);
    let clone_path = env.clones_root.join(pool_name).join(&clone_name);
    std::fs::create_dir_all(clone_path.parent().unwrap()).unwrap();

    // Clone the bare repo into the pool directory
    run_ok(
        Command::new("git")
            .arg("clone")
            .arg(&env.bare_repo)
            .arg(&clone_path),
    );

    // Init the pool, pointing at the bare repo with the right clones_root
    let repo_str = env.bare_repo.to_string_lossy().to_string();
    let cr_str = env.clones_root.to_string_lossy().to_string();
    run_ok(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        pool_name,
        "--clones-root",
        &cr_str,
    ]));

    clone_name
}

/// Initialize a pool with N clones. Returns their names.
fn init_pool_with_n_clones(env: &TestEnv, pool_name: &str, n: usize) -> Vec<String> {
    let pool_path = env.clones_root.join(pool_name);
    std::fs::create_dir_all(&pool_path).unwrap();

    let mut clone_names = Vec::new();
    for i in 1..=n {
        let clone_name = format!("{}-{}", pool_name, i);
        let clone_path = pool_path.join(&clone_name);
        run_ok(
            Command::new("git")
                .arg("clone")
                .arg(&env.bare_repo)
                .arg(&clone_path),
        );
        clone_names.push(clone_name);
    }

    // Init the pool -- it will discover the clones in the pool directory
    let repo_str = env.bare_repo.to_string_lossy().to_string();
    let cr_str = env.clones_root.to_string_lossy().to_string();
    run_ok(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        pool_name,
        "--clones-root",
        &cr_str,
    ]));

    clone_names
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_init_from_local_repo() {
    let env = setup_test_env();
    let clone_name = init_pool_with_clone(&env, "testrepo");

    // Verify config.json was created
    assert!(env.rpool_dir.join("config.json").exists());
    let config = env.load_config();
    assert!(config.pools.contains_key("testrepo"));
    assert_eq!(
        config.pools["testrepo"].repo_url,
        env.bare_repo.to_string_lossy()
    );

    // Verify state.json was created
    assert!(env.rpool_dir.join("state.json").exists());
    let state = env.load_state();
    assert!(state.pools.contains_key("testrepo"));
    assert!(state.pools["testrepo"].clones.contains_key(&clone_name));
}

#[test]
fn test_checkout_and_status() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Checkout a feature branch
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));
    let checkout_path = stdout.trim();
    assert!(!checkout_path.is_empty(), "checkout should output a path");
    assert!(
        Path::new(checkout_path).exists(),
        "checkout path should exist on disk"
    );

    // Verify git is on the right branch
    let actual_branch = git_current_branch(Path::new(checkout_path));
    assert_eq!(actual_branch, "feature-a");

    // Verify status shows the assignment -- status outputs to stdout
    let (ok, stdout, _stderr) = run_capture(&mut env.cmd(&["-p", "testrepo", "status"]));
    assert!(ok, "status should succeed");
    assert!(
        stdout.contains("feature-a"),
        "status stdout should mention feature-a, got: {}",
        stdout
    );
}

#[test]
fn test_drop_clone() {
    let env = setup_test_env();
    let clone_name = init_pool_with_clone(&env, "testrepo");

    // Checkout a branch
    run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));

    // Verify state has an assignment
    let state = env.load_state();
    assert!(
        state.pools["testrepo"].clones[&clone_name]
            .assigned_branch
            .is_some(),
        "clone should be assigned after checkout"
    );

    // Drop the clone
    run_ok(&mut env.cmd(&["-p", "testrepo", "drop", &clone_name]));

    // Verify assignment is cleared
    let state = env.load_state();
    assert!(
        state.pools["testrepo"].clones[&clone_name]
            .assigned_branch
            .is_none(),
        "clone should be unassigned after drop"
    );
}

#[test]
fn test_new_clone() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Add a new clone
    let stdout = run_ok(&mut env.cmd(&["-p", "testrepo", "new"]));
    let new_path = stdout.trim();
    assert!(!new_path.is_empty(), "new should output a path");
    assert!(
        Path::new(new_path).exists(),
        "new clone path should exist on disk"
    );
    assert!(
        Path::new(new_path).join(".git").exists(),
        "new clone should be a git repo"
    );

    // Verify it appears in state
    let state = env.load_state();
    let pool = &state.pools["testrepo"];
    assert!(
        pool.clones.len() >= 2,
        "pool should have at least 2 clones after 'new'"
    );
}

#[test]
fn test_cd_outputs_path() {
    let env = setup_test_env();
    let clone_name = init_pool_with_clone(&env, "testrepo");

    // Checkout so the clone is registered
    run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));

    // cd should output the clone's path on stdout
    let stdout = run_ok(&mut env.cmd(&["-p", "testrepo", "cd", &clone_name]));
    let cd_path = stdout.trim();
    assert!(!cd_path.is_empty(), "cd should output a path");
    assert!(Path::new(cd_path).exists(), "cd path should exist on disk");
}

#[test]
fn test_pools_list() {
    let env = setup_test_env();

    // Init two pools
    let repo_str = env.bare_repo.to_string_lossy().to_string();
    let cr_str = env.clones_root.to_string_lossy().to_string();

    run_ok(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "pool-alpha",
        "--clones-root",
        &cr_str,
    ]));
    run_ok(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "pool-beta",
        "--clones-root",
        &cr_str,
    ]));

    // List pools
    let (ok, stdout, _stderr) = run_capture(&mut env.cmd(&["pools"]));
    assert!(ok, "pools should succeed");
    assert!(
        stdout.contains("pool-alpha"),
        "pools should list pool-alpha, got: {}",
        stdout
    );
    assert!(
        stdout.contains("pool-beta"),
        "pools should list pool-beta, got: {}",
        stdout
    );
}

#[test]
fn test_rm_pool() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Verify pool exists
    let config = env.load_config();
    assert!(config.pools.contains_key("testrepo"));

    // Remove pool
    run_ok(&mut env.cmd(&["rm-pool", "testrepo"]));

    // Verify pool is gone
    let config = env.load_config();
    assert!(
        !config.pools.contains_key("testrepo"),
        "pool should be removed from config"
    );

    // Verify state is also cleaned
    let state = env.load_state();
    assert!(
        !state.pools.contains_key("testrepo"),
        "pool should be removed from state"
    );
}

#[test]
fn test_pin_unpin() {
    let env = setup_test_env();
    let clones = init_pool_with_n_clones(&env, "testrepo", 2);

    // Pin the first clone
    run_ok(&mut env.cmd(&["-p", "testrepo", "pin", &clones[0]]));

    // Verify it is pinned
    let state = env.load_state();
    assert!(
        state.pools["testrepo"].clones[&clones[0]].pinned,
        "clone should be pinned"
    );

    // Checkout a branch -- should prefer the unpinned clone (clones[1])
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));
    let checkout_path = PathBuf::from(stdout.trim());

    let state = env.load_state();
    let assigned_clone = state.pools["testrepo"]
        .clones
        .iter()
        .find(|(_, cs)| cs.assigned_branch.as_deref() == Some("feature-a"))
        .map(|(name, _)| name.clone())
        .expect("some clone should be assigned feature-a");
    assert_eq!(
        assigned_clone, clones[1],
        "unpinned clone should be preferred, but got {}",
        assigned_clone
    );

    // Verify checkout path matches the unpinned clone
    let expected_path = &state.pools["testrepo"].clones[&clones[1]].path;
    assert_eq!(&checkout_path, expected_path);

    // Unpin
    run_ok(&mut env.cmd(&["-p", "testrepo", "unpin", &clones[0]]));
    let state = env.load_state();
    assert!(
        !state.pools["testrepo"].clones[&clones[0]].pinned,
        "clone should be unpinned"
    );
}

#[test]
fn test_doctor_clean() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Doctor on a clean pool should report no issues
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&["-p", "testrepo", "doctor"]));
    assert!(ok, "doctor should succeed on clean pool");
    assert!(
        stderr.contains("No issues found"),
        "doctor should say no issues, got stderr: {}",
        stderr
    );
}

#[test]
fn test_doctor_detects_branch_mismatch() {
    let env = setup_test_env();
    let clone_name = init_pool_with_clone(&env, "testrepo");

    // Checkout feature-a via rpool
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));
    let clone_path = PathBuf::from(stdout.trim());

    // Externally switch the clone to feature-b
    run_ok(
        Command::new("git")
            .args(["checkout", "feature-b"])
            .current_dir(&clone_path),
    );

    // Verify state still says feature-a
    let state = env.load_state();
    assert_eq!(
        state.pools["testrepo"].clones[&clone_name]
            .assigned_branch
            .as_deref(),
        Some("feature-a"),
        "state should still say feature-a before doctor"
    );

    // Doctor should detect the mismatch
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&["-p", "testrepo", "doctor"]));
    assert!(ok, "doctor should succeed");
    assert!(
        stderr.contains("branch mismatch"),
        "doctor should detect branch mismatch, got stderr: {}",
        stderr
    );

    // Run doctor --fix
    let (ok, _stdout, stderr) =
        run_capture(&mut env.cmd(&["-p", "testrepo", "doctor", "--fix"]));
    assert!(ok, "doctor --fix should succeed");
    assert!(
        stderr.contains("FIXED"),
        "doctor --fix should report fixing, got stderr: {}",
        stderr
    );

    // Verify state was corrected
    let state = env.load_state();
    assert_eq!(
        state.pools["testrepo"].clones[&clone_name]
            .assigned_branch
            .as_deref(),
        Some("feature-b"),
        "state should be corrected to feature-b after doctor --fix"
    );
}

#[test]
fn test_validate_name_rejects_bad_names() {
    let env = setup_test_env();
    let repo_str = env.bare_repo.to_string_lossy().to_string();
    let cr_str = env.clones_root.to_string_lossy().to_string();

    // Names with spaces
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "bad name",
        "--clones-root",
        &cr_str,
    ]));
    assert!(!ok, "init with space in name should fail");
    assert!(
        stderr.contains("invalid character"),
        "error should mention invalid character, got: {}",
        stderr
    );

    // Names with path separators
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "foo/bar",
        "--clones-root",
        &cr_str,
    ]));
    assert!(!ok, "init with / in name should fail");
    assert!(
        stderr.contains("invalid character"),
        "error should mention invalid character, got: {}",
        stderr
    );

    // Name starting with dash: use --name=-badname so clap does not parse
    // it as separate short flags.
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name=-badname",
        "--clones-root",
        &cr_str,
    ]));
    assert!(!ok, "init with leading dash should fail");
    assert!(
        stderr.contains("must not start with '-'"),
        "error should mention dash, got: {}",
        stderr
    );

    // Name ".."
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "..",
        "--clones-root",
        &cr_str,
    ]));
    assert!(!ok, "init with .. should fail");
    assert!(
        stderr.contains("cannot be"),
        "error should reject '..', got: {}",
        stderr
    );

    // Names with glob chars
    let (ok, _stdout, _stderr) = run_capture(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "foo*bar",
        "--clones-root",
        &cr_str,
    ]));
    assert!(!ok, "init with * in name should fail");
}

#[test]
fn test_checkout_stale_state() {
    let env = setup_test_env();
    let _clones = init_pool_with_n_clones(&env, "testrepo", 2);

    // Checkout feature-a -- rpool assigns it to a clone
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));
    let first_path = PathBuf::from(stdout.trim());

    // Find which clone was assigned
    let state = env.load_state();
    let assigned_clone = state.pools["testrepo"]
        .clones
        .iter()
        .find(|(_, cs)| cs.assigned_branch.as_deref() == Some("feature-a"))
        .map(|(name, _)| name.clone())
        .expect("a clone should be assigned feature-a");

    // Externally switch that clone to a different branch
    run_ok(
        Command::new("git")
            .args(["checkout", "feature-b"])
            .current_dir(&first_path),
    );

    // Now checkout feature-a again. rpool should detect the stale state
    // (the clone that was supposedly on feature-a is actually on feature-b)
    // and use the other clone (or re-checkout on the same after correction).
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));
    let second_path = PathBuf::from(stdout.trim());

    // Either way, the result should be on feature-a.
    let actual_branch = git_current_branch(&second_path);
    assert_eq!(
        actual_branch, "feature-a",
        "after stale-state correction, checkout should land on feature-a"
    );

    // Verify the originally assigned clone's state was corrected
    let state = env.load_state();
    let corrected = state.pools["testrepo"].clones[&assigned_clone]
        .assigned_branch
        .as_deref();
    // The originally assigned clone should no longer falsely claim feature-a
    assert_ne!(
        corrected,
        Some("feature-a"),
        "stale clone should have its state corrected away from feature-a"
    );
}

#[test]
fn test_sync_fetches_all() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Sync should succeed
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&["-p", "testrepo", "sync"]));
    assert!(ok, "sync should succeed, stderr: {}", stderr);
    assert!(
        stderr.contains("ok"),
        "sync should report ok for clone, got stderr: {}",
        stderr
    );
}

#[test]
fn test_checkout_alias_ck() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Use the 'ck' alias
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "ck", "feature-a", "--no-submodules"]));
    let path = stdout.trim();
    assert!(!path.is_empty(), "ck alias should output a path");
    let actual_branch = git_current_branch(Path::new(path));
    assert_eq!(actual_branch, "feature-a");
}

#[test]
fn test_status_alias_st() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Use the 'st' alias
    let (ok, stdout, _stderr) = run_capture(&mut env.cmd(&["-p", "testrepo", "st"]));
    assert!(ok, "st alias should succeed");
    assert!(
        stdout.contains("testrepo"),
        "st should show pool name, got: {}",
        stdout
    );
}

#[test]
fn test_init_duplicate_pool_fails() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Trying to init the same pool again should fail
    let repo_str = env.bare_repo.to_string_lossy().to_string();
    let cr_str = env.clones_root.to_string_lossy().to_string();
    let (ok, _stdout, stderr) = run_capture(&mut env.cmd(&[
        "init",
        "--repo",
        &repo_str,
        "--name",
        "testrepo",
        "--clones-root",
        &cr_str,
    ]));
    assert!(!ok, "duplicate init should fail");
    assert!(
        stderr.contains("already exists"),
        "error should mention pool already exists, got: {}",
        stderr
    );
}

#[test]
fn test_checkout_creates_tracking_branch() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // feature-a exists on origin but not locally. Checkout should create a
    // tracking branch.
    let stdout =
        run_ok(&mut env.cmd(&["-p", "testrepo", "checkout", "feature-a", "--no-submodules"]));
    let path = PathBuf::from(stdout.trim());
    let branch = git_current_branch(&path);
    assert_eq!(branch, "feature-a");

    // Verify the file from feature-a is present
    assert!(path.join("a.txt").exists(), "feature-a should have a.txt");
}

#[test]
fn test_new_with_custom_name() {
    let env = setup_test_env();
    let _clone_name = init_pool_with_clone(&env, "testrepo");

    // Add a clone with a custom name
    let stdout = run_ok(&mut env.cmd(&["-p", "testrepo", "new", "my-custom-clone"]));
    let path = stdout.trim();
    assert!(Path::new(path).exists());

    // Verify the custom name appears in state
    let state = env.load_state();
    assert!(
        state.pools["testrepo"]
            .clones
            .contains_key("my-custom-clone"),
        "custom clone name should be in state"
    );
}

#[test]
fn test_drop_without_assignment() {
    let env = setup_test_env();
    let clone_name = init_pool_with_clone(&env, "testrepo");

    // The init registered the clone with its current branch (e.g. "main").
    // Drop it first to clear any initial assignment.
    run_ok(&mut env.cmd(&["-p", "testrepo", "drop", &clone_name]));

    // Now drop again -- clone has no assignment, so it should succeed
    // with a "was not assigned" message.
    let (ok, _stdout, stderr) =
        run_capture(&mut env.cmd(&["-p", "testrepo", "drop", &clone_name]));
    assert!(ok, "drop on unassigned clone should succeed");
    assert!(
        stderr.contains("not assigned"),
        "should mention not assigned, got: {}",
        stderr
    );
}
