//! Where the numbers come from.
//!
//! Every provider reads a credential that the vendor's own CLI already left on
//! this machine, asks the vendor's own usage endpoint, and maps the answer to a
//! list of `Quota`s. Copia never writes credentials, never refreshes them and
//! never sends them anywhere except the vendor they belong to.
//!
//! None of these endpoints is a documented public API. They are what the
//! vendors' CLIs call, as catalogued by the MIT-licensed CodexBar project
//! (https://github.com/steipete/CodexBar/tree/main/docs). Expect them to change.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Clone, Debug)]
pub struct Quota {
    /// Short name of the window: "5h", "week", "pro"…
    pub label: String,
    /// Fraction of the quota already used, 0.0 – 1.0.
    pub used: f64,
    /// When it resets: epoch seconds (number) or an ISO-8601 string.
    pub resets_at: Option<Value>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub name: String,
    /// False when no credential for this provider was found on this machine.
    pub configured: bool,
    pub quotas: Vec<Quota>,
    pub error: Option<String>,
    /// True when this provider's request or reply shape is a guess that has
    /// not been confirmed against a live account; the popover says so.
    pub experimental: bool,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct Config {
    pub refresh_minutes: Option<u64>,
    /// Provider ids to skip: "claude", "claude:<folder>", "codex",
    /// "antigravity", "opencode-go".
    pub disabled: Vec<String>,
    pub custom: Vec<Custom>,
    /// Extra Claude Code config folders (`CLAUDE_CONFIG_DIR`) to read a
    /// sign-in from, besides the ones Copia finds itself (see `claude_logins`).
    pub claude_config_dirs: Vec<String>,
    /// Why config.json, or its `custom` part, was not used: it could not be
    /// read or parsed, or it could be changed by someone else. Shown in the
    /// popover so a typo does not silently drop every setting.
    #[serde(skip)]
    pub problem: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct Custom {
    pub name: String,
    /// Program and arguments; must print quota JSON on stdout (see README).
    pub command: Vec<String>,
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("copia").join("config.json"))
    }

    pub fn load() -> Config {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => Config::default(),
        }
    }

    fn load_from(path: &std::path::Path) -> Config {
        use std::io::Read;
        let trouble = |why: String| Config { problem: Some(why), ..Config::default() };

        // Read and check one open handle, so the file that is checked is the
        // file that was read even if the path is swapped in between.
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            Err(e) => return trouble(format!("config.json could not be read: {e}")),
        };
        let mut bytes = Vec::new();
        if let Err(e) = file.read_to_end(&mut bytes) {
            return trouble(format!("config.json could not be read: {e}"));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Config::default();
        }
        let mut config: Config = match serde_json::from_slice(&bytes) {
            Ok(config) => config,
            Err(e) => return trouble(format!("config.json ignored: {e}")),
        };
        // Custom commands run every few minutes, so only take them from a
        // file that nobody but this user (or root) can change.
        if !config.custom.is_empty() {
            if let Err(why) = check_private(path, &file) {
                config.custom.clear();
                config.problem = Some(format!("custom commands not run: {why}"));
            }
        }
        config
    }
}

/// Ok when the open `file`, the folder `path` names it in, and the folder it
/// really lives in (after following links) are all owned by this user or root
/// and cannot be written by group or others.
#[cfg(unix)]
fn check_private(path: &std::path::Path, file: &std::fs::File) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    let private = |what: &str, meta: std::fs::Metadata| {
        if meta.uid() != me && meta.uid() != 0 {
            return Err(format!("{what} belongs to another user"));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!("{what} can be changed by other users (chmod go-w)"));
        }
        Ok(())
    };
    private("config.json", file.metadata().map_err(|e| format!("config.json: {e}"))?)?;

    let named = path.parent().ok_or("config.json has no folder")?;
    let real = std::fs::canonicalize(path).map_err(|e| format!("config.json: {e}"))?;
    let real = real.parent().ok_or("config.json has no folder")?;
    for folder in [named, real] {
        // Name the folder: with a linked config.json there are two of them.
        let what = format!("its folder {}", folder.display());
        private(&what, std::fs::metadata(folder).map_err(|e| format!("{what}: {e}"))?)?;
    }
    Ok(())
}

/// Windows: the per-user %APPDATA% folder is private by default.
#[cfg(not(unix))]
fn check_private(_path: &std::path::Path, _file: &std::fs::File) -> Result<(), String> {
    Ok(())
}

/// Providers nobody has run against a live account yet.
const EXPERIMENTAL: [&str; 0] = [];

/// Ok(None) = not set up on this machine; Err = set up but the fetch failed.
type Fetched = Result<Option<Vec<Quota>>, String>;

pub fn fetch_all(config: &Config) -> Vec<Provider> {
    fetch_all_with(config, claude_logins(config))
}

/// `fetch_all` with the Claude sign-ins already found, so tests can leave
/// out the ones on the machine running them.
fn fetch_all_with(config: &Config, claude_logins: Vec<ClaudeLogin>) -> Vec<Provider> {
    let builtin: [(&str, &str, fn() -> Fetched); 3] = [
        ("codex", "Codex", codex),
        ("antigravity", "Antigravity", antigravity),
        ("opencode-go", "OpenCode Go", opencode_go),
    ];
    let disabled = |id: &str| config.disabled.iter().any(|d| d == id);
    let claude_logins: Vec<ClaudeLogin> =
        claude_logins.into_iter().filter(|l| !disabled(&l.id)).collect();

    std::thread::scope(|scope| {
        // Ids must be unique (the popover keys its bars on them).
        let mut taken: Vec<String> = claude_logins.iter().map(|l| l.id.clone()).collect();
        // Every Claude sign-in comes back from one job, so they stay first
        // and in order however many there are.
        let claude = scope.spawn(move || claude_all(claude_logins));
        let mut jobs = Vec::new();
        for (id, name, fetch) in builtin {
            if disabled(id) {
                continue;
            }
            taken.push(id.to_string());
            jobs.push((id.to_string(), name.to_string(), scope.spawn(fetch)));
        }
        for custom in &config.custom {
            // Even when two entries share a name.
            let base = format!("custom:{}", custom.name);
            let mut id = base.clone();
            let mut n = 1;
            while taken.contains(&id) {
                n += 1;
                id = format!("{base}#{n}");
            }
            taken.push(id.clone());
            jobs.push((id, custom.name.clone(), scope.spawn(|| run_custom(custom))));
        }
        let problem = config.problem.clone().map(|error| Provider {
            id: "config".into(),
            name: "Settings".into(),
            configured: true,
            quotas: Vec::new(),
            error: Some(error),
            experimental: false,
        });
        let claude = claude.join().unwrap_or_default();
        let others = jobs
            .into_iter()
            .map(|(id, name, job)| (id, name, job.join().unwrap_or_else(|_| Err("crashed".into()))));
        claude
            .into_iter()
            .chain(others)
            .map(|(id, name, fetched)| {
                let (configured, quotas, error) = match fetched {
                    Ok(Some(quotas)) if quotas.is_empty() => {
                        (true, quotas, Some("no quota windows in the reply".to_string()))
                    }
                    Ok(Some(quotas)) => (true, quotas, None),
                    Ok(None) => (false, Vec::new(), None),
                    Err(error) => (true, Vec::new(), Some(error)),
                };
                let experimental = EXPERIMENTAL.contains(&id.as_str());
                Provider { id, name, configured, quotas, error, experimental }
            })
            .chain(problem)
            .collect()
    })
}

/// Made-up numbers for screenshots and for trying the UI without any account.
pub fn demo() -> Vec<Provider> {
    let now = now_secs();
    let quota = |label: &str, used: f64, secs: u64| Quota {
        label: label.into(),
        used,
        resets_at: Some(json!(now + secs)),
    };
    let provider = |id: &str, name: &str, quotas: Vec<Quota>| Provider {
        id: id.into(),
        name: name.into(),
        configured: true,
        quotas,
        error: None,
        experimental: EXPERIMENTAL.contains(&id),
    };
    vec![
        provider(
            "claude",
            "Claude",
            vec![quota("5h", 0.38, 2 * 3600 + 700), quota("week", 0.61, 3 * 86400)],
        ),
        provider(
            "codex",
            "Codex",
            vec![quota("5h", 0.12, 4 * 3600), quota("week", 0.87, 5 * 86400)],
        ),
        provider(
            "antigravity",
            "Antigravity",
            vec![
                quota("gemini 5h", 0.08, 4 * 3600 + 3400),
                quota("gemini wk", 0.25, 6 * 86400),
                quota("other 5h", 0.0, 5 * 3600),
                quota("other wk", 0.15, 5 * 86400),
            ],
        ),
        provider(
            "opencode-go",
            "OpenCode Go",
            vec![
                quota("5h", 0.05, 3600),
                quota("week", 0.44, 2 * 86400),
                quota("month", 0.70, 12 * 86400),
            ],
        ),
    ]
}

// ---------------------------------------------------------------- Claude ---

/// One Claude Code sign-in on this machine: the default one (`~/.claude`) or
/// one made with `CLAUDE_CONFIG_DIR=<dir> claude`, which is how people keep a
/// work and a personal account signed in side by side.
#[derive(Debug, Clone, PartialEq)]
struct ClaudeLogin {
    /// "claude" for the default sign-in, "claude:<name>" for the others.
    id: String,
    name: String,
    /// macOS Keychain service names to try, in order. Claude Code names the
    /// item after the `CLAUDE_CONFIG_DIR` text exactly as it was typed, so a
    /// folder has more than one possible name (with or without a trailing /).
    services: Vec<String>,
    /// Where Claude Code keeps the sign-in when there is no Keychain.
    file: PathBuf,
    /// What to run to renew it.
    cli: String,
}

const CLAUDE_SERVICE: &str = "Claude Code-credentials";

/// The default sign-in first, then one per extra Claude Code config folder:
/// `claude_config_dirs` from config.json, `$CLAUDE_CONFIG_DIR` if Copia was
/// started with it, and any `~/.claude-*` or `~/.claude_*` folder that holds
/// a Claude Code `.claude.json`.
fn claude_logins(config: &Config) -> Vec<ClaudeLogin> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let env_dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    claude_logins_in(config, &home, env_dir)
}

fn claude_logins_in(config: &Config, home: &std::path::Path, env_dir: Option<PathBuf>) -> Vec<ClaudeLogin> {
    let default_dir = home.join(".claude");
    let mut logins = vec![ClaudeLogin {
        id: "claude".into(),
        name: "Claude".into(),
        services: vec![CLAUDE_SERVICE.into()],
        file: default_dir.join(".credentials.json"),
        cli: "claude".into(),
    }];

    let mut dirs: Vec<PathBuf> = config.claude_config_dirs.iter().map(|d| expand_home(d, home)).collect();
    dirs.extend(env_dir);
    let mut found: Vec<PathBuf> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (name.starts_with(".claude-") || name.starts_with(".claude_"))
                && entry.path().join(".claude.json").is_file()
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    dirs.extend(found);

    let mut seen = vec![std::fs::canonicalize(&default_dir).unwrap_or(default_dir)];
    for dir in dirs {
        let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        let label = claude_dir_label(&dir);
        let base = format!("claude:{label}");
        let mut id = base.clone();
        let mut n = 1;
        while logins.iter().any(|l| l.id == id) {
            n += 1;
            id = format!("{base}#{n}");
        }
        logins.push(ClaudeLogin {
            id,
            name: format!("Claude ({label})"),
            services: claude_services(&dir),
            file: dir.join(".credentials.json"),
            cli: format!("CLAUDE_CONFIG_DIR={} claude", tilde(&dir, home)),
        });
    }
    logins
}

/// "~/.claude-work" -> "work"; any other folder keeps its own name.
fn claude_dir_label(dir: &std::path::Path) -> String {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let short = name
        .strip_prefix(".claude-")
        .or_else(|| name.strip_prefix(".claude_"))
        .unwrap_or(&name)
        .trim_start_matches('.');
    if short.is_empty() { "other".into() } else { short.into() }
}

/// Claude Code's Keychain service for a `CLAUDE_CONFIG_DIR`:
/// "Claude Code-credentials-" plus the first 8 hex digits of the SHA-256 of
/// the folder text. Both spellings with and without a trailing slash are
/// tried, since Copia cannot know which one was typed.
fn claude_services(dir: &std::path::Path) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let text = dir.to_string_lossy();
    let bare = text.trim_end_matches(['/', '\\']);
    let mut spellings = vec![bare.to_string(), format!("{bare}{}", std::path::MAIN_SEPARATOR)];
    spellings.dedup();
    spellings
        .into_iter()
        .map(|s| {
            let hash = Sha256::digest(s.as_bytes());
            let hex: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("{CLAUDE_SERVICE}-{hex}")
        })
        .collect()
}

/// `dir` with the home folder written as ~, for messages.
fn tilde(dir: &std::path::Path, home: &std::path::Path) -> String {
    match dir.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

fn expand_home(dir: &str, home: &std::path::Path) -> PathBuf {
    match dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None if dir == "~" => home.to_path_buf(),
        None => PathBuf::from(dir),
    }
}

/// The stored sign-in for `login`, from the Keychain or else its file.
/// Ok(None) = no sign-in there.
fn claude_creds(login: &ClaudeLogin) -> ClaudeCreds {
    let file = || read_json(login.file.clone());
    let mut refused = None;
    for service in &login.services {
        match claude_keychain(service) {
            Ok(Some(creds)) => return Ok(Some(creds)),
            Ok(None) => {}
            Err(why) => refused = Some(why),
        }
    }
    match (file(), refused) {
        (Some(creds), _) => Ok(Some(creds)),
        // Keychain refused and no file: say so rather than hide the bar.
        (None, Some(why)) => Err(why),
        (None, None) => Ok(None),
    }
}

/// Which account a sign-in belongs to, so the same account signed in from
/// two folders is shown once.
fn claude_account(creds: &Value) -> Option<(String, String)> {
    let oauth = creds.get("claudeAiOauth")?;
    let account = oauth.get("accountUuid")?.as_str()?;
    let org = oauth.get("organizationUuid").and_then(Value::as_str).unwrap_or("");
    Some((account.to_string(), org.to_string()))
}

/// Fetch every Claude sign-in in `logins`: read each one's credentials (one
/// at a time, so Keychain prompts come one after another), drop sign-ins of
/// an account already listed and extra folders with nothing signed in, then
/// ask for usage for the rest in parallel.
fn claude_all(logins: Vec<ClaudeLogin>) -> Vec<(String, String, Fetched)> {
    let ready = claude_ready(logins);
    std::thread::scope(|scope| {
        let jobs: Vec<_> = ready
            .into_iter()
            .map(|(id, name, cli, creds)| (id, name, scope.spawn(move || claude(creds, &cli))))
            .collect();
        jobs.into_iter()
            .map(|(id, name, job)| (id, name, job.join().unwrap_or_else(|_| Err("crashed".into()))))
            .collect()
    })
}

type ClaudeCreds = Result<Option<Value>, String>;

/// (id, name, renew command, credentials) for each sign-in worth showing.
fn claude_ready(logins: Vec<ClaudeLogin>) -> Vec<(String, String, String, ClaudeCreds)> {
    let mut accounts = Vec::new();
    let mut ready = Vec::new();
    for login in logins {
        let creds = claude_creds(&login);
        if let Ok(Some(account)) = creds.as_ref().map(|c| c.as_ref().and_then(claude_account)) {
            if accounts.contains(&account) {
                continue;
            }
            accounts.push(account);
        }
        if login.id != "claude" && matches!(creds, Ok(None)) {
            continue;
        }
        ready.push((login.id, login.name, login.cli, creds));
    }
    ready
}

fn claude(creds: ClaudeCreds, cli: &str) -> Fetched {
    let creds = creds?;
    let token = match creds
        .as_ref()
        .and_then(|c| c.pointer("/claudeAiOauth/accessToken"))
        .and_then(Value::as_str)
    {
        Some(token) => token,
        None => return Ok(None),
    };

    let reply = send(
        agent()
            .get("https://api.anthropic.com/api/oauth/usage")
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-beta", "oauth-2025-04-20"),
    )
    .map_err(|e| stale(e, cli))?;

    let windows = [
        ("five_hour", "5h"),
        ("seven_day", "week"),
        ("seven_day_opus", "opus"),
        ("seven_day_sonnet", "sonnet"),
    ];
    let mut quotas = Vec::new();
    for (key, label) in windows {
        let window = &reply[key];
        if let Some(percent) = window["utilization"].as_f64() {
            quotas.push(Quota {
                label: label.into(),
                used: fraction(percent / 100.0),
                resets_at: some(&window["resets_at"]),
            });
        }
    }
    Ok(Some(quotas))
}

/// On macOS Claude Code keeps its login in the Keychain. It is read through
/// Security.framework, so macOS asks on Copia's behalf and an "Always Allow"
/// trusts Copia's own binary. (Running `/usr/bin/security` instead would make
/// that approval cover every program that can run `security`.) Each sign-in
/// is its own item, so each is asked about once.
///
/// If the person clicks Deny, asking again on every timed refresh would bring
/// the dialog back every few minutes, so Copia stops asking for that item
/// until they press Refresh (see `ask_keychain_again`).
#[cfg(target_os = "macos")]
fn claude_keychain(service: &str) -> Result<Option<Value>, String> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    const DENIED: &str = "Keychain access denied — press Refresh to be asked again";
    // errSecUserCanceled, errSecAuthFailed: the person said no.
    const REFUSALS: [i32; 2] = [-128, -25293];

    let denied = || KEYCHAIN_DENIED.lock().unwrap_or_else(|e| e.into_inner());
    if denied().iter().any(|s| s == service) {
        return Err(DENIED.into());
    }
    let found = match ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(service)
        .load_data(true)
        .limit(1)
        .search()
    {
        Ok(found) => found,
        Err(e) if REFUSALS.contains(&e.code()) => {
            denied().push(service.to_string());
            return Err(DENIED.into());
        }
        // Not found, locked, no UI available…: try the file instead.
        Err(_) => return Ok(None),
    };
    Ok(match found.into_iter().next() {
        Some(SearchResult::Data(data)) => serde_json::from_slice(&data).ok(),
        _ => None,
    })
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain(_service: &str) -> Result<Option<Value>, String> {
    Ok(None)
}

/// Keychain items the person has refused this session.
#[cfg(target_os = "macos")]
static KEYCHAIN_DENIED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The person asked for a refresh: if they once denied Keychain access, let
/// the next fetch ask again.
pub fn ask_keychain_again() {
    #[cfg(target_os = "macos")]
    KEYCHAIN_DENIED.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ----------------------------------------------------------------- Codex ---

fn codex() -> Fetched {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => match dirs::home_dir() {
            Some(home) => home.join(".codex"),
            None => return Ok(None),
        },
    };
    let Some(auth) = read_json(home.join("auth.json")) else {
        return Ok(None);
    };
    let Some(token) = auth.pointer("/tokens/access_token").and_then(Value::as_str) else {
        return Ok(None);
    };

    let mut request = agent()
        .get("https://chatgpt.com/backend-api/wham/usage")
        .set("Authorization", &format!("Bearer {token}"));
    if let Some(account) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
        request = request.set("ChatGPT-Account-Id", account);
    }
    let reply = send(request).map_err(|e| stale(e, "codex"))?;

    let mut quotas = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        let window = &reply["rate_limit"][key];
        if let Some(percent) = window["used_percent"].as_f64() {
            let resets_at = some(&window["reset_at"]).or_else(|| {
                let secs = window["reset_after_seconds"].as_u64()?;
                Some(json!(now_secs() + secs))
            });
            quotas.push(Quota {
                label: window["limit_window_seconds"]
                    .as_u64()
                    .map(window_label)
                    .unwrap_or_else(|| key.trim_end_matches("_window").to_string()),
                used: fraction(percent / 100.0),
                resets_at,
            });
        }
    }
    Ok(Some(quotas))
}

// ----------------------------------------------------------- Antigravity ---

const ANTIGRAVITY_TIMEOUT: Duration = Duration::from_secs(15);
const ANTIGRAVITY_SIGN_IN_ERR: &str = "sign-in expired — run `agy` once to renew it";

fn antigravity() -> Fetched {
    use std::io::IsTerminal;
    let Some(agy) = find_agy()? else {
        return Ok(None);
    };
    if !antigravity_signed_in() {
        return Ok(None);
    }
    let probe = ProbeDir::create()?;
    let own_group = !std::io::stdin().is_terminal();
    let command = [
        agy.to_string_lossy().into_owned(),
        "-p".into(),
        "/usage".into(),
        "--output-format".into(),
        "json".into(),
    ];
    // `AGY_CLI_DISABLE_AUTO_UPDATE` keeps background refreshes from mutating
    // the binary. `SSH_CONNECTION` puts `agy` into its remote-SSH OAuth path
    // on every platform (macOS, Linux and Windows), so a lapsed sign-in prints
    // an auth URL to stderr instead of opening a browser (`open`, `xdg-open`
    // or `rundll32`). On Unix we also prepend failing `open`/`xdg-open` stubs
    // and set `BROWSER` as defense-in-depth.
    #[cfg(unix)]
    let env = {
        let mut paths = vec![probe.0.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        let path_env =
            std::env::join_paths(paths).unwrap_or_else(|_| probe.0.as_os_str().to_os_string());
        [
            ("AGY_CLI_DISABLE_AUTO_UPDATE", OsString::from("true")),
            ("SSH_CONNECTION", OsString::from("127.0.0.1 0 127.0.0.1 0")),
            ("PATH", path_env),
            ("BROWSER", probe.0.join("open").into_os_string()),
        ]
    };
    #[cfg(not(unix))]
    let env = [
        ("AGY_CLI_DISABLE_AUTO_UPDATE", OsString::from("true")),
        ("SSH_CONNECTION", OsString::from("127.0.0.1 0 127.0.0.1 0")),
    ];

    let result = run_limited_with(
        &command,
        ANTIGRAVITY_TIMEOUT,
        own_group,
        &env,
        Some(&probe.0),
        true,
        Some(antigravity_asks_for_sign_in),
    );
    Ok(Some(antigravity_from_run(result)?))
}

/// `agy 1.5.16` prints "Authentication required..." on stderr once its silent
/// sign-in renewal has failed, then waits 60s for an OAuth code that a
/// background refresh will never type. Stopping there keeps an expired
/// sign-in from holding every refresh for the whole timeout.
fn antigravity_asks_for_sign_in(stderr: &str) -> bool {
    // Only a line that starts with the ask, not a log line that mentions it.
    stderr
        .lines()
        .any(|line| line.trim_start().to_ascii_lowercase().starts_with("authentication required"))
}

fn antigravity_from_run(result: Result<CommandOutput, RunError>) -> Result<Vec<Quota>, String> {
    match result {
        Ok(out) => match serde_json::from_slice::<Value>(&out.stdout) {
            // Parse the JSON envelope even on non-zero exit: `agy` exits 1 with
            // `{"status": "ERROR", "error": "..."}` when a run fails.
            Ok(reply) => antigravity_quotas(&reply),
            Err(_) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if is_antigravity_auth_error(&stderr) {
                    Err(ANTIGRAVITY_SIGN_IN_ERR.into())
                } else {
                    Err(format!("agy exited with {}", out.status))
                }
            }
            Err(e) => Err(format!("unreadable reply: {e}")),
        },
        // Stopped by `antigravity_asks_for_sign_in`: it already saw the ask, so
        // don't depend on the stderr bytes having arrived in time.
        Err(RunError::Interrupted(_)) => Err(ANTIGRAVITY_SIGN_IN_ERR.into()),
        Err(RunError::TimedOut { message, stderr }) => {
            // Timed out after printing some other sign-in complaint?
            let stderr = String::from_utf8_lossy(&stderr);
            if is_antigravity_auth_error(&stderr) {
                Err(ANTIGRAVITY_SIGN_IN_ERR.into())
            } else {
                Err(message)
            }
        }
        Err(RunError::Failed(message)) => Err(message),
    }
}

/// Whether this machine has an Antigravity sign-in to query, so an installed
/// but never-signed-in (or logged-out) `agy` is left out instead of failing.
fn antigravity_signed_in() -> bool {
    if std::env::var("AGY_ADC_AUTH")
        .ok()
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
    {
        return true;
    }
    let home = dirs::home_dir();
    if home.as_deref().is_some_and(antigravity_adc_enabled) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if let Some(present) = antigravity_keychain_exists() {
        return present;
    }
    home.as_deref().is_some_and(antigravity_has_local_state)
}

/// Metadata-only check for `agy`'s login Keychain entry (`service = "gemini"`,
/// `account = "antigravity"`). Does not read secret data and never prompts.
#[cfg(target_os = "macos")]
fn antigravity_keychain_exists() -> Option<bool> {
    use security_framework::item::{ItemClass, ItemSearchOptions};
    // errSecItemNotFound: no stored Antigravity sign-in in the Keychain.
    const ITEM_NOT_FOUND: i32 = -25300;
    match ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service("gemini")
        .account("antigravity")
        .load_attributes(true)
        .limit(1)
        .search()
    {
        Ok(found) => Some(!found.is_empty()),
        Err(e) if e.code() == ITEM_NOT_FOUND => Some(false),
        // Locked or unavailable Keychain: fall back to on-disk state markers.
        Err(_) => None,
    }
}

fn antigravity_adc_enabled(home: &Path) -> bool {
    read_json(home.join(".gemini/config/config.json"))
        .and_then(|v| v.pointer("/userSettings/enableAdc")?.as_bool())
        == Some(true)
}

/// On Linux and Windows (and on macOS if the Keychain is unavailable), check
/// the onboarding marker `agy` writes once signed in. It lives in agy's cache
/// folder, which agy's docs say `/logout` clears. (The Antigravity desktop app's own state
/// file is not a sign-in to `agy`, so it is not counted.)
fn antigravity_has_local_state(home: &Path) -> bool {
    read_json(home.join(".gemini/antigravity-cli/cache/onboarding.json"))
        .and_then(|v| v["onboardingComplete"].as_bool())
        == Some(true)
}

fn find_agy() -> Result<Option<PathBuf>, String> {
    let override_path = std::env::var_os("ANTIGRAVITY_CLI_PATH");
    let path_var = std::env::var_os("PATH");
    let home = dirs::home_dir();

    #[cfg(unix)]
    let fallbacks: Vec<PathBuf> = [
        Some(PathBuf::from("/opt/homebrew/bin/agy")),
        Some(PathBuf::from("/usr/local/bin/agy")),
        home.as_ref().map(|h| h.join(".local/bin/agy")),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Official Windows installer (`install.ps1` / `install.cmd`) places a
    // standalone `agy.exe` at `%LOCALAPPDATA%\agy\bin\agy.exe`.
    #[cfg(windows)]
    let fallbacks: Vec<PathBuf> =
        dirs::data_local_dir().map(|d| d.join("agy").join("bin").join("agy.exe")).into_iter().collect();

    find_agy_in(override_path.as_deref(), path_var.as_deref(), home.as_deref(), &fallbacks)
}

fn find_agy_in(
    override_path: Option<&std::ffi::OsStr>,
    path_var: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
    fallbacks: &[PathBuf],
) -> Result<Option<PathBuf>, String> {
    if let Some(raw) = override_path.filter(|p| !p.is_empty()) {
        let path = match (raw.to_str(), home) {
            (Some(text), Some(home)) => expand_home(text, home),
            _ => PathBuf::from(raw),
        };
        if !is_executable_file(&path) {
            return Err("ANTIGRAVITY_CLI_PATH does not point to agy".into());
        }
        return Ok(Some(std::fs::canonicalize(&path).unwrap_or(path)));
    }

    let exe = if cfg!(windows) { "agy.exe" } else { "agy" };
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join(exe);
            if is_executable_file(&candidate) {
                return Ok(Some(std::fs::canonicalize(&candidate).unwrap_or(candidate)));
            }
        }
    }

    Ok(fallbacks
        .iter()
        .find(|p| is_executable_file(p))
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Private temporary directory used as `current_dir` for `agy` (and on Unix
/// to hold failing `open`/`xdg-open` stubs as defense-in-depth against
/// background browser launches).
struct ProbeDir(PathBuf);

impl ProbeDir {
    fn create() -> Result<Self, String> {
        use std::collections::hash_map::RandomState;
        use std::hash::BuildHasher;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let base = std::env::temp_dir();
        let pid = std::process::id();
        let mut created = None;
        for _ in 0..8 {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let r1 = RandomState::new().hash_one(seq);
            let r2 = RandomState::new().hash_one((pid, seq));
            let path = base.join(format!("copia-agy-{r1:016x}{r2:016x}"));
            #[cfg(unix)]
            let create_res = {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(&path)
            };
            #[cfg(not(unix))]
            let create_res = std::fs::DirBuilder::new().create(&path);
            match create_res {
                Ok(()) => {
                    created = Some(ProbeDir(path));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("could not create temp dir: {e}")),
            }
        }
        let dir = created.ok_or("could not create unique temp dir")?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            for name in ["open", "xdg-open"] {
                let stub = dir.0.join(name);
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o700)
                    .open(&stub)
                    .map_err(|e| format!("could not create {name} stub: {e}"))?;
                file.write_all(b"#!/bin/sh\nexit 1\n")
                    .map_err(|e| format!("could not write {name} stub: {e}"))?;
            }
        }
        Ok(dir)
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn is_antigravity_auth_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "unauthenticated",
        "authentication required",
        "authentication failed",
        "not signed in",
        "not logged in",
        "login required",
        "sign-in expired",
        "sign in expired",
        "token expired",
        "session expired",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

fn antigravity_quotas(reply: &Value) -> Result<Vec<Quota>, String> {
    let ok = reply["status"].as_str() == Some("SUCCESS")
        && reply["command"]["name"].as_str() == Some("usage");
    if !ok {
        let raw = reply["error"]
            .as_str()
            .or_else(|| reply["response"].as_str())
            .unwrap_or("");
        if is_antigravity_auth_error(raw) {
            return Err(ANTIGRAVITY_SIGN_IN_ERR.into());
        }
        let detail: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let detail: String = detail.chars().take(120).collect();
        return Err(if detail.is_empty() {
            "could not read quotas — run `agy` once to check sign-in".into()
        } else {
            format!("agy: {detail}")
        });
    }

    let groups = reply["command"]["data"]["groups"]
        .as_array()
        .ok_or("no quota groups in reply")?;
    let mut quotas = Vec::new();
    for group in groups {
        let group_name = group["name"].as_str().unwrap_or("");
        let mut buckets: Vec<&Value> =
            group["buckets"].as_array().into_iter().flatten().collect();
        buckets.sort_by_key(|b| match b["window"].as_str() {
            Some("5h") => 0,
            Some("weekly") => 1,
            _ => 2,
        });
        for bucket in buckets {
            let Some(remaining) = bucket["remaining_fraction"].as_f64() else {
                continue;
            };
            let id = bucket["id"].as_str().unwrap_or("");
            let window = bucket["window"].as_str().unwrap_or("");
            quotas.push(Quota {
                label: antigravity_label(id, window, group_name),
                used: fraction(1.0 - remaining),
                resets_at: antigravity_reset(&bucket["reset_time"]),
            });
        }
    }
    Ok(quotas)
}

fn antigravity_label(id: &str, window: &str, group_name: &str) -> String {
    match id {
        "gemini-5h" => "gemini 5h".into(),
        "gemini-weekly" => "gemini wk".into(),
        "3p-5h" => "other 5h".into(),
        "3p-weekly" => "other wk".into(),
        _ => {
            let win = if window == "weekly" { "wk" } else { window };
            let prefix = if group_name.to_ascii_lowercase().contains("gemini") {
                "gemini"
            } else if !group_name.is_empty() {
                "other"
            } else {
                ""
            };
            match (prefix.is_empty(), win.is_empty()) {
                (false, false) => format!("{prefix} {win}"),
                (true, false) => win.into(),
                (_, true) if !id.is_empty() => id.into(),
                _ => "quota".into(),
            }
        }
    }
}

fn antigravity_reset(value: &Value) -> Option<Value> {
    let v = some(value)?;
    if let Some(s) = v.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.starts_with("1970-01-01T00:00:00") {
            return None;
        }
    }
    Some(v)
}

// ----------------------------------------------------------- OpenCode Go ---

/// Find the `opencode` CLI: `PATH` first, then the folders a GUI app on
/// macOS (which starts with a minimal `PATH`) would otherwise miss.
fn find_opencode() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH");

    #[cfg(unix)]
    let fallbacks: Vec<PathBuf> = {
        let home = dirs::home_dir();
        [
            Some(PathBuf::from("/opt/homebrew/bin/opencode")),
            Some(PathBuf::from("/usr/local/bin/opencode")),
            home.as_ref().map(|home| home.join(".local/bin/opencode")),
            home.as_ref().map(|home| home.join(".opencode/bin/opencode")),
        ]
        .into_iter()
        .flatten()
        .collect()
    };
    #[cfg(windows)]
    let fallbacks: Vec<PathBuf> = dirs::data_local_dir()
        .map(|dir| dir.join("Programs").join("opencode").join("opencode.exe"))
        .into_iter()
        .collect();

    find_opencode_in(path_var.as_deref(), &fallbacks)
}

fn find_opencode_in(
    path_var: Option<&std::ffi::OsStr>,
    fallbacks: &[PathBuf],
) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "opencode.exe" } else { "opencode" };
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join(exe);
            if is_executable_file(&candidate) {
                return Some(std::fs::canonicalize(&candidate).unwrap_or(candidate));
            }
        }
    }
    fallbacks
        .iter()
        .find(|path| is_executable_file(path))
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
}

/// Endpoint and reply shape are OpenCode's documented ones
/// (https://opencode.ai/docs, "Go" page: `GET /inference/go/v1/usage`). When
/// credentials live in OpenCode v2's own store instead of `auth.json`, ask the
/// v2 CLI, which reports the same meters under `data.go`.
fn opencode_go() -> Fetched {
    // An explicit key always wins: it names the account to ask about.
    if let Some(key) = std::env::var("OPENCODE_API_KEY").ok().filter(|key| !key.is_empty()) {
        return opencode_go_http(&key);
    }

    // OpenCode v2 keeps credentials in a SQLite database Copia does not read,
    // and exposes Go usage through its own CLI.
    let mut cli_error = None;
    if let Some(path) = find_opencode() {
        match opencode_go_cli(&path) {
            Ok(Some(quotas)) => return Ok(Some(quotas)),
            // The CLI is signed in but has no Go subscription; the legacy
            // auth.json may still name one.
            Ok(None) => {}
            Err(error) => cli_error = Some(error),
        }
    }

    if let Some(key) = opencode_legacy_key() {
        return opencode_go_http(&key);
    }

    match cli_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// The Go key OpenCode v1 left in `auth.json`.
fn opencode_legacy_key() -> Option<String> {
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::home_dir()?.join(".local/share"),
    };
    let auth = read_json(data.join("opencode/auth.json"))?;
    let entry = &auth["opencode-go"];
    Some(entry["key"].as_str().or_else(|| entry["access"].as_str())?.to_string())
}

/// Ask the signed-in OpenCode v2 CLI for the same meters.
fn opencode_go_cli(path: &Path) -> Fetched {
    use std::io::IsTerminal;
    // In a terminal (`copia --print`), leave the CLI in the terminal's
    // process group; see `run_limited`.
    let own_group = !std::io::stdin().is_terminal();
    let command = vec![
        path.to_string_lossy().into_owned(),
        "api".into(),
        "get".into(),
        "/api/experimental/usage/limits".into(),
    ];
    let stdout = run_limited(&command, CUSTOM_TIMEOUT, own_group)?;
    let reply: Value = serde_json::from_slice(&stdout)
        .map_err(|error| format!("opencode output is not JSON: {error}"))?;
    if reply["data"]["go"].is_object() {
        Ok(Some(opencode_go_quotas(&reply)))
    } else {
        Ok(None)
    }
}

fn opencode_go_http(key: &str) -> Fetched {
    let reply = send(
        agent()
            .get("https://opencode.ai/inference/go/v1/usage")
            .set("Authorization", &format!("Bearer {key}")),
    )
    .map_err(|error| {
        if error.starts_with("HTTP 401") {
            "OpenCode did not accept this API key".to_string()
        } else if error.starts_with("HTTP 403") {
            "this key has no OpenCode Go subscription".to_string()
        } else {
            error
        }
    })?;
    Ok(Some(opencode_go_quotas(&reply)))
}

/// OpenCode's meters: `rolling`, `weekly`, `monthly`, each with `percent`
/// (0–100 *used*) and `resetsAt`. The v2 CLI nests them under `data.go`; the
/// legacy HTTP endpoint uses `usage`.
fn opencode_go_quotas(reply: &Value) -> Vec<Quota> {
    let meters = match &reply["data"]["go"] {
        go @ Value::Object(_) => go,
        _ => &reply["usage"],
    };
    let windows = [("rolling", "5h"), ("weekly", "week"), ("monthly", "month")];
    let mut quotas = Vec::new();
    for (key, label) in windows {
        let meter = &meters[key];
        if let Some(percent) = meter["percent"].as_f64() {
            quotas.push(Quota {
                label: label.into(),
                used: fraction(percent / 100.0),
                resets_at: opencode_go_reset(&meter["resetsAt"]),
            });
        }
    }
    quotas
}

/// `Quota.resets_at` is epoch seconds or ISO-8601. The legacy HTTP endpoint
/// sends ISO-8601, and the v2 CLI sends epoch milliseconds; convert those to
/// epoch seconds. Zero and negative resets mean "no reset time".
fn opencode_go_reset(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) if !text.is_empty() => Some(value.clone()),
        Value::Number(_) => {
            let seconds = value.as_f64()?;
            if !seconds.is_finite() || seconds <= 0.0 {
                return None;
            }
            let seconds = if seconds >= 1_000_000_000_000.0 { seconds / 1000.0 } else { seconds };
            Some(json!(seconds as u64))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- Custom ---

/// Anything else: run a command the user configured and read quota JSON from
/// its stdout — either `[{label, used | remaining, resets_at?}]` or
/// `{"quotas": [...]}`, with `used` / `remaining` as 0–1 fractions.
fn run_custom(custom: &Custom) -> Fetched {
    use std::io::IsTerminal;
    // In a terminal (`copia --print`), leave the command in the terminal's
    // process group; see `run_limited`.
    let own_group = !std::io::stdin().is_terminal();
    let stdout = run_limited(&custom.command, CUSTOM_TIMEOUT, own_group)?;
    let reply: Value =
        serde_json::from_slice(&stdout).map_err(|e| format!("output is not JSON: {e}"))?;
    let list = match &reply["quotas"] {
        Value::Null => &reply,
        found => found,
    };
    let mut quotas = Vec::new();
    for item in list.as_array().ok_or("expected a list of quotas")? {
        let used = match (item["used"].as_f64(), item["remaining"].as_f64()) {
            (Some(used), _) => used,
            (None, Some(remaining)) => 1.0 - remaining,
            _ => return Err("each quota needs `used` or `remaining`".into()),
        };
        quotas.push(Quota {
            label: item["label"].as_str().unwrap_or("quota").to_string(),
            used: fraction(used),
            resets_at: some(&item["resets_at"]),
        });
    }
    Ok(Some(quotas))
}

/// A custom command gets this long to answer. Every provider is waited for
/// before the popover updates, so one that hangs must not hang the rest.
const CUSTOM_TIMEOUT: Duration = Duration::from_secs(15);
/// More stdout than this is not a quota list.
const CUSTOM_MAX_OUTPUT: u64 = 1024 * 1024;
/// Bounded prefix of stderr kept for diagnostics when `capture_stderr` is set.
const STDERR_CAPTURE_LIMIT: usize = 8 * 1024;

struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: std::process::ExitStatus,
}

enum RunError {
    Failed(String),
    TimedOut { message: String, stderr: Vec<u8> },
    /// Stopped early because its stderr matched the `stop_on_stderr` test.
    Interrupted(String),
}

impl RunError {
    fn into_message(self) -> String {
        match self {
            RunError::Failed(msg)
            | RunError::TimedOut { message: msg, .. }
            | RunError::Interrupted(msg) => msg,
        }
    }
}

/// Run `command` and return its stdout, killing it if it runs past `timeout`
/// or prints more than `CUSTOM_MAX_OUTPUT` bytes.
///
/// `own_group`: on Unix, run it in a process group of its own so that
/// stopping it also stops anything it started (otherwise a background child
/// keeps the stdout pipe, and the reader thread below, alive). Pass false when
/// Copia runs in a terminal, so Ctrl-C still reaches the command and it can
/// still prompt on the terminal (ssh, gpg…). Windows always uses a Job Object.
fn run_limited(command: &[String], timeout: Duration, own_group: bool) -> Result<Vec<u8>, String> {
    let out = run_limited_with(command, timeout, own_group, &[], None, false, None)
        .map_err(RunError::into_message)?;
    if !out.status.success() {
        let program = command.first().map(String::as_str).unwrap_or("command");
        return Err(format!("{program} exited with {}", out.status));
    }
    Ok(out.stdout)
}

/// `run_limited` with extra environment, a working folder, and optionally the
/// first `STDERR_CAPTURE_LIMIT` bytes of stderr. `stop_on_stderr` (which
/// needs `capture_stderr`) stops the command as soon as what it has printed
/// on stderr passes the test, for a command that would otherwise sit waiting
/// for input it will never get.
fn run_limited_with(
    command: &[String],
    timeout: Duration,
    own_group: bool,
    env: &[(&str, OsString)],
    current_dir: Option<&Path>,
    capture_stderr: bool,
    stop_on_stderr: Option<fn(&str) -> bool>,
) -> Result<CommandOutput, RunError> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    /// How often the deadline and `stop_on_stderr` are checked.
    const POLL: Duration = Duration::from_millis(50);
    debug_assert!(stop_on_stderr.is_none() || capture_stderr, "stop_on_stderr needs capture_stderr");

    let (program, args) = command.split_first().ok_or_else(|| RunError::Failed("empty command".into()))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut builder = Command::new(program);
    builder
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr { Stdio::piped() } else { Stdio::null() });
    for (key, value) in env {
        builder.env(key, value);
    }
    if let Some(dir) = current_dir {
        builder.current_dir(dir);
    }
    #[cfg(unix)]
    if own_group {
        std::os::unix::process::CommandExt::process_group(&mut builder, 0);
    }
    #[cfg(not(unix))]
    let _ = own_group;
    let mut child = builder
        .spawn()
        .map_err(|e| RunError::Failed(format!("could not run {program}: {e}")))?;
    let tree = tree::Tree::adopt(&child, own_group);

    // Read stdout on its own thread so the deadline holds even while the
    // command is still printing (or keeps the pipe open and prints nothing).
    let mut pipe = child.stdout.take().ok_or_else(|| RunError::Failed("no stdout".into()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = (&mut pipe).take(CUSTOM_MAX_OUTPUT + 1).read_to_end(&mut buf);
        let _ = tx.send(read.map(|_| buf));
    });

    // When requested, drain stderr on its own thread so the pipe never fills,
    // retaining only the first `STDERR_CAPTURE_LIMIT` bytes for error messages.
    let stop_asked = Arc::new(AtomicBool::new(false));
    let stderr_rx = child.stderr.take().map(|mut err_pipe| {
        let (etx, erx) = mpsc::channel();
        let stop_asked = Arc::clone(&stop_asked);
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err_pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let room = STDERR_CAPTURE_LIMIT.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    if let Some(test) = stop_on_stderr {
                        if test(&String::from_utf8_lossy(&kept)) {
                            stop_asked.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
            let _ = etx.send(kept);
        });
        erx
    });
    let should_stop = || stop_asked.load(Ordering::Relaxed);
    let collect_stderr = || {
        stderr_rx
            .as_ref()
            .and_then(|erx| erx.recv_timeout(Duration::from_millis(200)).ok())
            .unwrap_or_default()
    };

    let stop = |child: &mut std::process::Child, why: String| {
        tree.kill(child);
        let _ = child.wait();
        Err(RunError::Failed(why))
    };
    let stop_timeout = |child: &mut std::process::Child| {
        tree.kill(child);
        let _ = child.wait();
        Err(RunError::TimedOut {
            message: format!("{program} took longer than {}s", timeout.as_secs()),
            stderr: collect_stderr(),
        })
    };
    let stop_interrupted = |child: &mut std::process::Child| {
        tree.kill(child);
        let _ = child.wait();
        Err(RunError::Interrupted(format!("{program} was stopped early")))
    };
    let stdout = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left.min(POLL)) {
            Ok(Ok(buf)) if buf.len() as u64 > CUSTOM_MAX_OUTPUT => {
                return stop(&mut child, format!("{program} printed more than 1 MB"));
            }
            Ok(Ok(buf)) => break buf,
            Ok(Err(e)) => return stop(&mut child, format!("could not read {program}: {e}")),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return stop(&mut child, format!("could not read {program}"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if should_stop() => return stop_interrupted(&mut child),
            Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return stop_timeout(&mut child),
        }
    };

    // Stdout is closed; give the command the rest of its time to exit.
    let status = loop {
        match child.try_wait() {
            // A complete stdout wins over `stop_on_stderr`, so this loop
            // only waits for the exit (still bounded by the deadline).
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => return stop_timeout(&mut child),
            Err(e) => return stop(&mut child, format!("could not wait for {program}: {e}")),
        }
    };
    Ok(CommandOutput { stdout, stderr: collect_stderr(), status })
}

/// A custom command together with everything it starts, so all of it can be
/// stopped at once, even after the command itself has exited.
mod tree {
    use std::process::Child;

    /// Unix: the process group the command leads (see `run_limited`). The
    /// group id stays reserved until the leader is reaped, which `stop` does
    /// only after killing the group.
    #[cfg(unix)]
    pub struct Tree(Option<libc::pid_t>);

    #[cfg(unix)]
    impl Tree {
        pub fn adopt(child: &Child, own_group: bool) -> Tree {
            Tree(own_group.then(|| libc::pid_t::try_from(child.id()).ok()).flatten())
        }

        pub fn kill(&self, child: &mut Child) {
            if let Some(pid) = self.0 {
                // SAFETY: kill has no memory-safety preconditions. A
                // negative pid names the process group `run_limited` made.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = child.kill();
        }
    }

    /// Windows: a Job Object. Processes the command starts join it too, and
    /// it can be terminated after the command itself has exited.
    #[cfg(windows)]
    pub struct Tree(Option<Job>);

    #[cfg(windows)]
    struct Job(*mut std::ffi::c_void);

    // SAFETY: a job handle is a kernel handle, usable from any thread.
    #[cfg(windows)]
    unsafe impl Send for Job {}
    #[cfg(windows)]
    unsafe impl Sync for Job {}

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut std::ffi::c_void, name: *const u16) -> *mut std::ffi::c_void;
        fn AssignProcessToJobObject(job: *mut std::ffi::c_void, process: *mut std::ffi::c_void) -> i32;
        fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    #[cfg(windows)]
    impl Tree {
        pub fn adopt(child: &Child, _own_group: bool) -> Tree {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: null attributes and name ask for an unnamed job with
            // default security; the handle is checked before use.
            let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if job.is_null() {
                return Tree(None);
            }
            let job = Job(job);
            // SAFETY: both handles are valid and open for this call. (A
            // process the command starts in the instant before this joins
            // no job; that window is microseconds wide.)
            let joined = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as _) } != 0;
            Tree(joined.then_some(job))
        }

        pub fn kill(&self, child: &mut Child) {
            if let Some(job) = &self.0 {
                // SAFETY: the job handle is open until `Job` is dropped.
                unsafe { TerminateJobObject(job.0, 1) };
            }
            let _ = child.kill();
        }
    }

    #[cfg(windows)]
    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closed once, here; nothing uses the handle afterwards.
            unsafe { CloseHandle(self.0) };
        }
    }
}

// --------------------------------------------------------------- Helpers ---

/// A request Copia is about to make: a URL and a few headers. Kept as plain
/// data so the HTTP library stays a detail of `send`.
struct Request {
    url: &'static str,
    headers: Vec<(&'static str, String)>,
}

struct Http;

fn agent() -> Http {
    Http
}

impl Http {
    fn get(self, url: &'static str) -> Request {
        Request { url, headers: Vec::new() }
    }
}

impl Request {
    fn set(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

fn send(request: Request) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(concat!("copia/", env!("CARGO_PKG_VERSION")))
        // 4xx / 5xx come back as responses, so their bodies can be reported.
        .http_status_as_error(false)
        .build()
        .into();

    let mut builder = agent.get(request.url);
    for (name, value) in &request.headers {
        builder = builder.header(*name, value.as_str());
    }
    let mut response = builder.call().map_err(|e| format!("network: {e}"))?;

    let code = response.status().as_u16();
    if code >= 400 {
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let detail: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let detail: String = detail.chars().take(100).collect();
        return if detail.is_empty() || detail.starts_with('<') {
            Err(format!("HTTP {code}"))
        } else {
            Err(format!("HTTP {code}: {detail}"))
        };
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| format!("unreadable reply: {e}"))
}

/// A 401 almost always means the CLI's token aged out; say how to fix it.
fn stale(error: String, cli: &str) -> String {
    if error.starts_with("HTTP 401") {
        format!("sign-in expired — run `{cli}` once to renew it")
    } else {
        error
    }
}

fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn some(value: &Value) -> Option<Value> {
    match value {
        Value::String(_) | Value::Number(_) => Some(value.clone()),
        _ => None,
    }
}

fn fraction(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn window_label(seconds: u64) -> String {
    match seconds {
        604_800 => "week".into(),
        s if s >= 86_400 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s => format!("{}m", (s / 60).max(1)),
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn custom_command_output_is_read() {
        let out = run_limited(&cmd(&["/bin/echo", "[1]"]), Duration::from_secs(5), true).unwrap();
        assert_eq!(out, b"[1]\n");
    }

    #[test]
    fn custom_command_failure_is_reported() {
        let err = run_limited(&cmd(&["/usr/bin/false"]), Duration::from_secs(5), true).unwrap_err();
        assert!(err.contains("exited with"), "{err}");
    }

    #[test]
    fn custom_command_that_hangs_is_killed() {
        let started = Instant::now();
        let err = run_limited(&cmd(&["/bin/sleep", "30"]), Duration::from_secs(1), true).unwrap_err();
        assert!(err.contains("took longer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn custom_command_that_closes_stdout_but_keeps_running_is_killed() {
        let started = Instant::now();
        let command = cmd(&["/bin/sh", "-c", "exec >&-; sleep 30"]);
        let err = run_limited(&command, Duration::from_secs(1), true).unwrap_err();
        assert!(err.contains("took longer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn custom_command_background_child_is_killed_too() {
        let dir = std::env::temp_dir().join(format!("copia-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        // The background sleep inherits stdout, as a careless script's would.
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let started = Instant::now();
        let err = run_limited(&cmd(&["/bin/sh", "-c", &script]), Duration::from_secs(1), true).unwrap_err();
        assert!(err.contains("took longer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));

        let pid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        // A killed process can linger as a zombie where nothing reaps
        // orphans (a container without an init); that counts as gone.
        let zombie = || {
            Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim_start().starts_with('Z'))
        };
        let gone = (0..50).any(|_| {
            // SAFETY: signal 0 only asks whether the process exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0 && !zombie();
            if alive {
                std::thread::sleep(Duration::from_millis(20));
            }
            !alive
        });
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(gone, "background child {pid} outlived the command");
    }

    #[test]
    fn custom_command_that_floods_stdout_is_stopped() {
        let err = run_limited(&cmd(&["/usr/bin/yes"]), Duration::from_secs(10), true).unwrap_err();
        assert!(err.contains("more than 1 MB"), "{err}");
    }

    #[test]
    fn config_writable_by_others_is_refused() {
        let dir = std::env::temp_dir().join(format!("copia-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, "{}").unwrap();
        let mode = |p: &std::path::Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();

        let check = |p: &std::path::Path| check_private(p, &std::fs::File::open(p).unwrap());

        mode(&dir, 0o700);
        mode(&file, 0o600);
        assert_eq!(check(&file), Ok(()));

        mode(&file, 0o666);
        assert!(check(&file).unwrap_err().starts_with("config.json"));

        mode(&file, 0o600);
        mode(&dir, 0o777);
        assert!(check(&file).unwrap_err().starts_with("its folder"));

        // A link in a writable folder is refused even if its target is safe.
        let safe = dir.join("safe");
        std::fs::create_dir_all(&safe).unwrap();
        mode(&safe, 0o700);
        let target = safe.join("config.json");
        std::fs::write(&target, "{}").unwrap();
        mode(&target, 0o600);
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(check(&link).unwrap_err().starts_with("its folder"));

        mode(&dir, 0o700);
        assert_eq!(check(&link), Ok(()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_config_is_reported_not_ignored() {
        let dir = std::env::temp_dir().join(format!("copia-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");

        assert!(Config::load_from(&file).problem.is_none(), "a missing file is fine");

        std::fs::write(&file, "  \n").unwrap();
        assert!(Config::load_from(&file).problem.is_none(), "an empty file is fine");

        std::fs::write(&file, r#"{ "refresh_minutes": 10, }"#).unwrap();
        let config = Config::load_from(&file);
        assert!(config.problem.unwrap().starts_with("config.json ignored"));

        std::fs::write(&file, r#"{ "custom": [{ "command": ["x"] }] }"#).unwrap();
        assert!(Config::load_from(&file).problem.unwrap().contains("name"));

        std::fs::write(&file, r#"{ "refresh_minutes": 10, "disabled": ["antigravity"] }"#).unwrap();
        let config = Config::load_from(&file);
        assert!(config.problem.is_none());
        assert_eq!(
            (config.refresh_minutes, config.disabled),
            (Some(10), vec!["antigravity".to_string()])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn custom_providers_with_the_same_name_get_distinct_ids() {
        let custom = |name: &str| Custom { name: name.into(), command: cmd(&["/bin/echo", "[]"]) };
        let config = Config {
            disabled: ["claude", "codex", "antigravity", "opencode-go"].map(String::from).to_vec(),
            custom: vec![custom("Team"), custom("Team"), custom("Other")],
            ..Config::default()
        };
        // No Claude sign-ins: whatever this machine has signed in (and its
        // Keychain) stays out of the test.
        let ids: Vec<String> = fetch_all_with(&config, Vec::new()).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["custom:Team", "custom:Team#2", "custom:Other"]);
    }
}

#[cfg(test)]
mod opencode_go_tests {
    use super::*;

    /// The example reply from OpenCode's own documentation.
    #[test]
    fn opencode_go_documented_reply() {
        let reply = json!({ "usage": {
            "rolling": { "status": "ok", "percent": 30, "resetsAt": "2026-11-21T15:00:00.000Z" },
            "weekly": { "status": "rate-limited", "percent": 100, "resetsAt": "2026-11-23T00:00:00.000Z" },
            "monthly": { "status": "ok", "percent": 50, "resetsAt": "2026-12-01T00:00:00.000Z" }
        }});
        let quotas = opencode_go_quotas(&reply);
        let seen: Vec<(&str, f64)> = quotas.iter().map(|q| (q.label.as_str(), q.used)).collect();
        assert_eq!(seen, [("5h", 0.3), ("week", 1.0), ("month", 0.5)]);
        assert_eq!(quotas[0].resets_at, Some(json!("2026-11-21T15:00:00.000Z")));
    }

    #[test]
    fn opencode_go_missing_window_is_skipped() {
        let reply = json!({ "usage": { "rolling": { "status": "ok", "percent": 5 } } });
        let quotas = opencode_go_quotas(&reply);
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].resets_at, None);
    }

    /// What `opencode api get /api/experimental/usage/limits` prints on
    /// OpenCode v2: meters under `data.go`, resets in epoch milliseconds.
    #[test]
    fn opencode_go_v2_cli_reply() {
        let reply = json!({ "location": { "directory": "/home/me" }, "data": { "go": {
            "rolling": { "percent": 0, "limited": false, "resetsAt": 1_795_527_276_960u64 },
            "weekly": { "percent": 1, "limited": false, "resetsAt": 1_795_996_800_000u64 },
            "monthly": { "percent": 38, "limited": false, "resetsAt": 1_797_493_654_000u64 }
        }}});
        let quotas = opencode_go_quotas(&reply);
        let seen: Vec<(&str, f64)> = quotas.iter().map(|q| (q.label.as_str(), q.used)).collect();
        assert_eq!(seen, [("5h", 0.0), ("week", 0.01), ("month", 0.38)]);
        let resets: Vec<Option<Value>> = quotas.iter().map(|q| q.resets_at.clone()).collect();
        assert_eq!(
            resets,
            [
                Some(json!(1_795_527_276u64)),
                Some(json!(1_795_996_800u64)),
                Some(json!(1_797_493_654u64))
            ]
        );
    }

    #[test]
    fn opencode_go_no_subscription_is_empty() {
        let reply = json!({ "location": { "directory": "/home/me" }, "data": { "go": null } });
        assert!(opencode_go_quotas(&reply).is_empty());
    }

    #[test]
    fn opencode_go_zero_reset_is_none() {
        let reply = json!({ "usage": { "rolling": { "percent": 5, "resetsAt": 0 } } });
        let quotas = opencode_go_quotas(&reply);
        assert_eq!(quotas[0].resets_at, None);
    }

    #[cfg(unix)]
    fn fake_opencode(dir: &ProbeDir, payload: &Value) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.0.join("opencode");
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{payload}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn opencode_go_cli_reads_v2_reply() {
        let dir = ProbeDir::create().unwrap();
        let script = fake_opencode(
            &dir,
            &json!({ "data": { "go": {
                "rolling": { "percent": 12, "resetsAt": 1_795_527_276_960u64 },
                "weekly": { "percent": 34, "resetsAt": 1_795_996_800_000u64 },
                "monthly": { "percent": 56, "resetsAt": 1_797_493_654_000u64 }
            }}}),
        );
        let quotas = opencode_go_cli(&script).unwrap().expect("data.go is a Go subscription");
        let seen: Vec<(&str, f64)> = quotas.iter().map(|q| (q.label.as_str(), q.used)).collect();
        assert_eq!(seen, [("5h", 0.12), ("week", 0.34), ("month", 0.56)]);
        assert_eq!(quotas[1].resets_at, Some(json!(1_795_996_800u64)));
    }

    #[cfg(unix)]
    #[test]
    fn opencode_go_cli_without_go_subscription_is_none() {
        let dir = ProbeDir::create().unwrap();
        let script = fake_opencode(&dir, &json!({ "data": { "go": null } }));
        assert!(opencode_go_cli(&script).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn find_opencode_skips_non_executable_and_uses_fallbacks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = ProbeDir::create().unwrap();
        let bad = dir.0.join("bad");
        let good = dir.0.join("good");
        let fallback_dir = dir.0.join("fallback");
        for folder in [&bad, &good, &fallback_dir] {
            std::fs::create_dir_all(folder).unwrap();
        }

        let non_exec = bad.join("opencode");
        std::fs::write(&non_exec, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();

        let exec = good.join("opencode");
        std::fs::write(&exec, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = std::env::join_paths([&bad, &good]).unwrap();
        assert_eq!(
            find_opencode_in(Some(path_env.as_os_str()), &[]),
            Some(std::fs::canonicalize(&exec).unwrap())
        );

        let fallback = fallback_dir.join("opencode");
        std::fs::write(&fallback, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bad_path = std::env::join_paths([&bad]).unwrap();
        assert_eq!(
            find_opencode_in(Some(bad_path.as_os_str()), std::slice::from_ref(&fallback)),
            Some(std::fs::canonicalize(&fallback).unwrap())
        );

        assert_eq!(
            find_opencode_in(Some(path_env.as_os_str()), &[fallback]),
            Some(std::fs::canonicalize(&exec).unwrap())
        );
        assert_eq!(find_opencode_in(None, &[]), None);
    }
}

#[cfg(test)]
mod claude_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("copia-claude-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keychain_service_matches_claude_code() {
        // Claude Code: "Claude Code-credentials-" + sha256(CLAUDE_CONFIG_DIR)[..8].
        let sep = std::path::MAIN_SEPARATOR;
        let services = claude_services(std::path::Path::new("/Users/someone/.claude-work"));
        assert_eq!(services[0], "Claude Code-credentials-18cc900a");
        if sep == '/' {
            assert_eq!(services[1], "Claude Code-credentials-fb460960");
        }
        // A trailing slash in the folder gives the same two names.
        assert_eq!(claude_services(std::path::Path::new("/Users/someone/.claude-work/")), services);
    }

    #[test]
    fn folder_labels() {
        let label = |p: &str| claude_dir_label(std::path::Path::new(p));
        assert_eq!(label("/h/.claude-work"), "work");
        assert_eq!(label("/h/.claude_personal"), "personal");
        assert_eq!(label("/srv/claude-team"), "claude-team");
        assert_eq!(label("/h/.claude-"), "other");
    }

    #[test]
    fn finds_extra_config_folders_once() {
        let home = scratch("home");
        for name in [".claude", ".claude-work", ".claude-empty", ".claude_personal", ".clauderc"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }
        // Only folders Claude Code has used (they hold .claude.json) count.
        for name in [".claude-work", ".claude_personal", ".clauderc"] {
            std::fs::write(home.join(name).join(".claude.json"), "{}").unwrap();
        }
        let elsewhere = scratch("elsewhere");
        let config = Config {
            // Named in config.json and also found by the scan: listed once.
            claude_config_dirs: vec!["~/.claude-work".into(), elsewhere.to_string_lossy().into_owned()],
            ..Config::default()
        };
        // $CLAUDE_CONFIG_DIR pointing at the default folder adds nothing.
        let logins = claude_logins_in(&config, &home, Some(home.join(".claude")));
        let ids: Vec<&str> = logins.iter().map(|l| l.id.as_str()).collect();
        let elsewhere_id = format!("claude:{}", claude_dir_label(&elsewhere));
        assert_eq!(ids, ["claude", "claude:work", elsewhere_id.as_str(), "claude:personal"]);
        assert_eq!(logins[0].services, [CLAUDE_SERVICE]);
        assert_eq!(logins[1].name, "Claude (work)");
        assert_eq!(logins[1].file, home.join(".claude-work/.credentials.json"));
        assert_eq!(logins[1].services, claude_services(&home.join(".claude-work")));
        assert_eq!(logins[1].cli, "CLAUDE_CONFIG_DIR=~/.claude-work claude");
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn same_account_twice_is_shown_once_and_empty_folders_are_dropped() {
        let dir = scratch("dedup");
        let login = |id: &str, account: Option<&str>| {
            let file = dir.join(format!("{}.json", id.replace(':', "_")));
            if let Some(account) = account {
                let creds = json!({ "claudeAiOauth": {
                    "accessToken": "t", "accountUuid": account, "organizationUuid": "org"
                }});
                std::fs::write(&file, creds.to_string()).unwrap();
            }
            ClaudeLogin {
                id: id.into(),
                name: id.into(),
                // No such Keychain item, so the file is read.
                services: vec![format!("copia-test-missing-{}", std::process::id())],
                file,
                cli: "claude".into(),
            }
        };
        let ready = claude_ready(vec![
            login("claude", Some("work")),
            login("claude:work", Some("work")),
            login("claude:personal", Some("personal")),
            login("claude:empty", None),
        ]);
        let ids: Vec<&str> = ready.iter().map(|(id, ..)| id.as_str()).collect();
        assert_eq!(ids, ["claude", "claude:personal"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn signed_out_default_still_reports_not_set_up() {
        let dir = scratch("signed-out");
        let ready = claude_ready(vec![ClaudeLogin {
            id: "claude".into(),
            name: "Claude".into(),
            services: vec![format!("copia-test-missing-{}", std::process::id())],
            file: dir.join(".credentials.json"),
            cli: "claude".into(),
        }]);
        assert_eq!(ready.len(), 1);
        let (.., cli, creds) = ready.into_iter().next().unwrap();
        assert!(matches!(claude(creds, &cli), Ok(None)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod antigravity_tests {
    use super::*;

    #[test]
    fn antigravity_parses_groups_orders_5h_first_and_drops_epoch_zero() {
        let reply = json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                {
                                    "id": "gemini-weekly",
                                    "window": "weekly",
                                    "remaining_fraction": 0.75,
                                    "reset_time": "2026-12-01T08:07:49Z"
                                },
                                {
                                    "id": "gemini-5h",
                                    "window": "5h",
                                    "remaining_fraction": 0.9,
                                    "reset_time": "2026-11-24T13:07:49Z"
                                }
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "buckets": [
                                {
                                    "id": "3p-weekly",
                                    "window": "weekly",
                                    "remaining_fraction": 1,
                                    "reset_time": "1970-01-01T00:00:00Z"
                                },
                                {
                                    "id": "3p-5h",
                                    "window": "5h",
                                    "remaining_fraction": 1,
                                    "reset_time": "1970-01-01T00:00:00.000Z"
                                }
                            ]
                        }
                    ]
                }
            }
        });
        let quotas = antigravity_quotas(&reply).unwrap();
        let seen: Vec<(&str, f64)> = quotas.iter().map(|q| (q.label.as_str(), q.used)).collect();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].0, "gemini 5h");
        assert!((seen[0].1 - 0.1).abs() < 1e-9);
        assert_eq!(seen[1].0, "gemini wk");
        assert!((seen[1].1 - 0.25).abs() < 1e-9);
        assert_eq!(seen[2], ("other 5h", 0.0));
        assert_eq!(seen[3], ("other wk", 0.0));

        assert_eq!(quotas[0].resets_at, Some(json!("2026-11-24T13:07:49Z")));
        assert_eq!(quotas[1].resets_at, Some(json!("2026-12-01T08:07:49Z")));
        assert_eq!(quotas[2].resets_at, None);
        assert_eq!(quotas[3].resets_at, None);
    }

    #[test]
    fn antigravity_weekly_only_starter_reply() {
        let reply = json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                {
                                    "id": "gemini-weekly",
                                    "window": "weekly",
                                    "remaining_fraction": 0.4,
                                    "reset_time": "2026-12-01T00:00:00Z"
                                }
                            ]
                        }
                    ]
                }
            }
        });
        let quotas = antigravity_quotas(&reply).unwrap();
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].label, "gemini wk");
        assert!((quotas[0].used - 0.6).abs() < 1e-9);
    }

    #[test]
    fn antigravity_error_classification_avoids_false_positives() {
        let expired = json!({
            "status": "ERROR",
            "error": "authentication failed or timed out"
        });
        assert_eq!(antigravity_quotas(&expired).unwrap_err(), ANTIGRAVITY_SIGN_IN_ERR);

        let token_limit = json!({
            "status": "ERROR",
            "error": "token limit exceeded"
        });
        let err = antigravity_quotas(&token_limit).unwrap_err();
        assert_eq!(err, "agy: token limit exceeded");
        assert!(!err.contains("sign-in expired"));

        let wrong_cmd = json!({
            "status": "SUCCESS",
            "command": { "name": "other" }
        });
        assert!(antigravity_quotas(&wrong_cmd).is_err());
    }

    #[test]
    fn antigravity_unknown_bucket_falls_back_cleanly() {
        let reply = json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Experimental Models",
                            "buckets": [
                                {
                                    "id": "exp-daily",
                                    "window": "day",
                                    "remaining_fraction": 0.5
                                }
                            ]
                        }
                    ]
                }
            }
        });
        let quotas = antigravity_quotas(&reply).unwrap();
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].label, "other day");
        assert_eq!(quotas[0].used, 0.5);
        assert_eq!(quotas[0].resets_at, None);
    }

    #[test]
    fn antigravity_timeout_and_spawn_errors_are_not_hidden() {
        let auth_timeout = antigravity_from_run(Err(RunError::TimedOut {
            message: "agy took longer than 15s".into(),
            stderr: b"Authentication required. Please visit the URL to log in:\n".to_vec(),
        }));
        assert_eq!(auth_timeout.unwrap_err(), ANTIGRAVITY_SIGN_IN_ERR);

        let hang_timeout = antigravity_from_run(Err(RunError::TimedOut {
            message: "agy took longer than 15s".into(),
            stderr: Vec::new(),
        }));
        assert_eq!(hang_timeout.unwrap_err(), "agy took longer than 15s");

        let spawn_err = antigravity_from_run(Err(RunError::Failed("could not run agy: denied".into())));
        assert_eq!(spawn_err.unwrap_err(), "could not run agy: denied");
    }

    #[test]
    fn antigravity_local_state_markers() {
        let home = ProbeDir::create().unwrap();
        assert!(!antigravity_has_local_state(&home.0));
        assert!(!antigravity_adc_enabled(&home.0));

        let cache = home.0.join(".gemini/antigravity-cli/cache");
        std::fs::create_dir_all(&cache).unwrap();
        // The Antigravity desktop app's state file is not an agy sign-in.
        let app = home.0.join(".gemini/antigravity");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("antigravity_state.pbtxt"), "").unwrap();
        assert!(!antigravity_has_local_state(&home.0));

        std::fs::write(cache.join("onboarding.json"), r#"{"onboardingComplete": false}"#).unwrap();
        assert!(!antigravity_has_local_state(&home.0));

        std::fs::write(cache.join("onboarding.json"), r#"{"onboardingComplete": true}"#).unwrap();
        assert!(antigravity_has_local_state(&home.0));

        let cfg = home.0.join(".gemini/config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.json"), r#"{"userSettings":{"enableAdc":true}}"#).unwrap();
        assert!(antigravity_adc_enabled(&home.0));
    }

    #[test]
    fn probe_dir_creates_unique_dirs_and_cleans_up() {
        let a = ProbeDir::create().unwrap();
        let b = ProbeDir::create().unwrap();
        assert_ne!(a.0, b.0);
        let path = a.0.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
            for stub in ["open", "xdg-open"] {
                let smode = std::fs::metadata(path.join(stub)).unwrap().permissions().mode() & 0o777;
                assert_eq!(smode, 0o700);
            }
        }
        drop(a);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_non_zero_exit_still_parses_json_error() {
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"printf '{"status":"ERROR","error":"authentication failed or timed out"}'; exit 1"#
                .to_string(),
        ];
        let run = run_limited_with(&cmd, Duration::from_secs(5), true, &[], None, true, None);
        let err = antigravity_from_run(run).unwrap_err();
        assert_eq!(err, ANTIGRAVITY_SIGN_IN_ERR);
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_stops_as_soon_as_it_asks_for_sign_in() {
        // Like agy with a lapsed sign-in: asks on stderr, then waits.
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo 'Authentication required. Please visit the URL to log in:' >&2; sleep 10".to_string(),
        ];
        let started = std::time::Instant::now();
        let run = run_limited_with(
            &cmd,
            Duration::from_secs(10),
            true,
            &[],
            None,
            true,
            Some(antigravity_asks_for_sign_in),
        );
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        assert!(matches!(run, Err(RunError::Interrupted(_))));
        assert_eq!(antigravity_from_run(run).unwrap_err(), ANTIGRAVITY_SIGN_IN_ERR);
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_stderr_does_not_stop_the_run() {
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo 'refreshing token' >&2; printf ok".to_string(),
        ];
        let out = run_limited_with(
            &cmd,
            Duration::from_secs(5),
            true,
            &[],
            None,
            true,
            Some(antigravity_asks_for_sign_in),
        )
        .unwrap_or_else(|e| panic!("{}", e.into_message()));
        assert_eq!(out.stdout, b"ok");
        assert!(out.status.success());

        assert!(antigravity_asks_for_sign_in("note\n  Authentication required. Visit:\n"));
        assert!(!antigravity_asks_for_sign_in("retrying: authentication required by server, ok"));
    }

    #[cfg(unix)]
    #[test]
    fn find_agy_validates_override_expands_tilde_and_skips_non_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = ProbeDir::create().unwrap();
        let home = dir.0.join("home");
        let bad_bin = dir.0.join("bad_bin");
        let good_bin = dir.0.join("good_bin");
        for d in [&home, &bad_bin, &good_bin, &home.join("bin")] {
            std::fs::create_dir_all(d).unwrap();
        }

        // Non-executable agy earlier on PATH is skipped for an executable one later.
        let non_exec = bad_bin.join("agy");
        std::fs::write(&non_exec, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();

        let exec = good_bin.join("agy");
        std::fs::write(&exec, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = std::env::join_paths([&bad_bin, &good_bin]).unwrap();
        let found = find_agy_in(None, Some(&path_env), Some(&home), &[]).unwrap();
        assert_eq!(found, Some(std::fs::canonicalize(&exec).unwrap()));

        // Missing or non-executable ANTIGRAVITY_CLI_PATH returns an explicit error.
        let missing = find_agy_in(
            Some(std::ffi::OsStr::new("~/bin/does-not-exist")),
            Some(&path_env),
            Some(&home),
            &[],
        );
        assert_eq!(missing.unwrap_err(), "ANTIGRAVITY_CLI_PATH does not point to agy");

        let non_exec_override = find_agy_in(Some(non_exec.as_os_str()), None, Some(&home), &[]);
        assert_eq!(non_exec_override.unwrap_err(), "ANTIGRAVITY_CLI_PATH does not point to agy");

        // Tilde override expands against home and canonicalizes.
        let home_agy = home.join("bin/agy");
        std::fs::write(&home_agy, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&home_agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let expanded =
            find_agy_in(Some(std::ffi::OsStr::new("~/bin/agy")), None, Some(&home), &[]).unwrap();
        assert_eq!(expanded, Some(std::fs::canonicalize(&home_agy).unwrap()));
    }
}
