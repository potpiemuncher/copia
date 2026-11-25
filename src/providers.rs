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
use std::path::PathBuf;
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
    /// Provider ids to skip: "claude", "claude:<folder>", "codex", "gemini",
    /// "opencode-go".
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
const EXPERIMENTAL: [&str; 1] = ["opencode-go"];

/// Ok(None) = not set up on this machine; Err = set up but the fetch failed.
type Fetched = Result<Option<Vec<Quota>>, String>;

pub fn fetch_all(config: &Config) -> Vec<Provider> {
    let builtin: [(&str, &str, fn() -> Fetched); 3] = [
        ("codex", "Codex", codex),
        ("gemini", "Gemini", gemini),
        ("opencode-go", "OpenCode Go", opencode_go),
    ];
    let disabled = |id: &str| config.disabled.iter().any(|d| d == id);
    let claude_logins: Vec<ClaudeLogin> =
        claude_logins(config).into_iter().filter(|l| !disabled(&l.id)).collect();

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
            "gemini",
            "Gemini",
            vec![quota("pro", 0.95, 9 * 3600), quota("flash", 0.20, 9 * 3600)],
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
        None,
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
    let reply = send(request, None).map_err(|e| stale(e, "codex"))?;

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

// ---------------------------------------------------------------- Gemini ---

fn gemini() -> Fetched {
    let Some(creds) = dirs::home_dir().and_then(|h| read_json(h.join(".gemini/oauth_creds.json"))) else {
        return Ok(None);
    };
    let Some(token) = creds["access_token"].as_str() else {
        return Ok(None);
    };
    // Copia does not refresh tokens (that needs the Gemini CLI's own OAuth
    // client secret), so an expired one is reported instead of used.
    if let Some(expiry_ms) = creds["expiry_date"].as_f64() {
        if expiry_ms < now_secs() as f64 * 1000.0 {
            return Err("sign-in expired — run `gemini` once to renew it".into());
        }
    }

    let reply = send(
        agent()
            .post("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota")
            .set("Authorization", &format!("Bearer {token}")),
        Some(json!({})),
    )
    .map_err(|e| stale(e, "gemini"))?;

    // One bucket per model; show the tightest bucket of each model tier.
    let mut quotas: Vec<Quota> = Vec::new();
    for bucket in reply["buckets"].as_array().into_iter().flatten() {
        let Some(remaining) = bucket["remainingFraction"].as_f64() else {
            continue;
        };
        let model = bucket["modelId"].as_str().unwrap_or("quota");
        let label = if model.contains("flash-lite") {
            "lite"
        } else if model.contains("flash") {
            "flash"
        } else if model.contains("pro") {
            "pro"
        } else {
            model
        };
        let quota = Quota {
            label: label.into(),
            used: fraction(1.0 - remaining),
            resets_at: some(&bucket["resetTime"]),
        };
        match quotas.iter_mut().find(|q| q.label == label) {
            Some(existing) if existing.used < quota.used => *existing = quota,
            Some(_) => {}
            None => quotas.push(quota),
        }
    }
    Ok(Some(quotas))
}

// ----------------------------------------------------------- OpenCode Go ---

/// Endpoint and reply shape are OpenCode's documented ones
/// (https://opencode.ai/docs, "Go" page: `GET /inference/go/v1/usage`), but
/// nobody has run this against a live Go subscription yet.
fn opencode_go() -> Fetched {
    // The key is the Go API key. OpenCode v1 kept it in auth.json; v2 keeps
    // credentials in a SQLite database Copia does not read, so v2-only users
    // need OPENCODE_API_KEY set.
    let key = std::env::var("OPENCODE_API_KEY").ok().filter(|k| !k.is_empty()).or_else(|| {
        let data = match std::env::var_os("XDG_DATA_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()?.join(".local/share"),
        };
        let auth = read_json(data.join("opencode/auth.json"))?;
        let entry = &auth["opencode-go"];
        Some(entry["key"].as_str().or_else(|| entry["access"].as_str())?.to_string())
    });
    let Some(key) = key else {
        return Ok(None);
    };

    let reply = send(
        agent()
            .get("https://opencode.ai/inference/go/v1/usage")
            .set("Authorization", &format!("Bearer {key}")),
        None,
    )
    .map_err(|e| {
        if e.starts_with("HTTP 401") {
            "OpenCode did not accept this API key".to_string()
        } else if e.starts_with("HTTP 403") {
            "this key has no OpenCode Go subscription".to_string()
        } else {
            e
        }
    })?;
    Ok(Some(opencode_go_quotas(&reply)))
}

/// `{"usage": {"rolling": {"status", "percent", "resetsAt"}, "weekly", "monthly"}}`
/// where `percent` is 0–100 *used* and `resetsAt` is ISO-8601.
fn opencode_go_quotas(reply: &Value) -> Vec<Quota> {
    let windows = [("rolling", "5h"), ("weekly", "week"), ("monthly", "month")];
    let mut quotas = Vec::new();
    for (key, label) in windows {
        let meter = &reply["usage"][key];
        if let Some(percent) = meter["percent"].as_f64() {
            quotas.push(Quota {
                label: label.into(),
                used: fraction(percent / 100.0),
                resets_at: some(&meter["resetsAt"]),
            });
        }
    }
    quotas
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

/// Run `command` and return its stdout, killing it if it runs past `timeout`
/// or prints more than `CUSTOM_MAX_OUTPUT` bytes.
///
/// `own_group`: on Unix, run it in a process group of its own so that
/// stopping it also stops anything it started (otherwise a background child
/// keeps the stdout pipe, and the reader thread below, alive). Pass false when
/// Copia runs in a terminal, so Ctrl-C still reaches the command and it can
/// still prompt on the terminal (ssh, gpg…). Windows always uses a Job Object.
fn run_limited(command: &[String], timeout: Duration, own_group: bool) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;

    let (program, args) = command.split_first().ok_or("empty command")?;
    let deadline = std::time::Instant::now() + timeout;
    let mut builder = Command::new(program);
    builder.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(unix)]
    if own_group {
        std::os::unix::process::CommandExt::process_group(&mut builder, 0);
    }
    #[cfg(not(unix))]
    let _ = own_group;
    let mut child = builder.spawn().map_err(|e| format!("could not run {program}: {e}"))?;
    let tree = tree::Tree::adopt(&child, own_group);

    // Read stdout on its own thread so the deadline holds even while the
    // command is still printing (or keeps the pipe open and prints nothing).
    let mut pipe = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = (&mut pipe).take(CUSTOM_MAX_OUTPUT + 1).read_to_end(&mut buf);
        let _ = tx.send(read.map(|_| buf));
    });

    let stop = |child: &mut std::process::Child, why: String| {
        tree.kill(child);
        let _ = child.wait();
        Err(why)
    };
    let stdout = match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
        Ok(Ok(buf)) if buf.len() as u64 > CUSTOM_MAX_OUTPUT => {
            return stop(&mut child, format!("{program} printed more than 1 MB"));
        }
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => return stop(&mut child, format!("could not read {program}: {e}")),
        Err(_) => return stop(&mut child, format!("{program} took longer than {}s", timeout.as_secs())),
    };

    // Stdout is closed; give the command the rest of its time to exit.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                return stop(&mut child, format!("{program} took longer than {}s", timeout.as_secs()))
            }
            Err(e) => return stop(&mut child, format!("could not wait for {program}: {e}")),
        }
    };
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(stdout)
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
    post: bool,
    url: &'static str,
    headers: Vec<(&'static str, String)>,
}

struct Http;

fn agent() -> Http {
    Http
}

impl Http {
    fn get(self, url: &'static str) -> Request {
        Request { post: false, url, headers: Vec::new() }
    }

    fn post(self, url: &'static str) -> Request {
        Request { post: true, url, headers: Vec::new() }
    }
}

impl Request {
    fn set(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

fn send(request: Request, body: Option<Value>) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(concat!("copia/", env!("CARGO_PKG_VERSION")))
        // 4xx / 5xx come back as responses, so their bodies can be reported.
        .http_status_as_error(false)
        .build()
        .into();

    let result = if request.post {
        let mut builder = agent.post(request.url);
        for (name, value) in &request.headers {
            builder = builder.header(*name, value.as_str());
        }
        builder.send_json(body.unwrap_or_else(|| json!({})))
    } else {
        let mut builder = agent.get(request.url);
        for (name, value) in &request.headers {
            builder = builder.header(*name, value.as_str());
        }
        builder.call()
    };
    let mut response = result.map_err(|e| format!("network: {e}"))?;

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

        std::fs::write(&file, r#"{ "refresh_minutes": 10, "disabled": ["gemini"] }"#).unwrap();
        let config = Config::load_from(&file);
        assert!(config.problem.is_none());
        assert_eq!((config.refresh_minutes, config.disabled), (Some(10), vec!["gemini".to_string()]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn custom_providers_with_the_same_name_get_distinct_ids() {
        let custom = |name: &str| Custom { name: name.into(), command: cmd(&["/bin/echo", "[]"]) };
        let config = Config {
            disabled: ["claude", "codex", "gemini", "opencode-go"].map(String::from).to_vec(),
            custom: vec![custom("Team"), custom("Team"), custom("Other")],
            ..Config::default()
        };
        let ids: Vec<String> = fetch_all(&config).into_iter().map(|p| p.id).collect();
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
