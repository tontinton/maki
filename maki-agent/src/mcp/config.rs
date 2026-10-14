use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::error::McpError;
use crate::tools::is_builtin_tool;
use maki_config::{GatedFile, ProjectConfig, expand_env, is_valid_server_name};
use maki_storage::paths::expand_tilde;
use serde::Deserialize;
use toml_edit::DocumentMut;

const MCP_CONFIG_FILE: &str = "mcp.toml";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const SECRET_NEEDS_CLIENT_ID: &str = "oauth.client_secret needs client_id";
pub(super) const HTTPS_CALLBACK_NEEDS_PORT: &str = "oauth.callback_scheme \"https\" needs callback_port to match the redirect URI registered with the provider";

#[derive(Debug, Clone)]
pub enum McpConfigError {
    Read { path: PathBuf, error: String },
    Parse { path: PathBuf, error: String },
}

/// Generates a compacted but still human-meaningful version of a path.
fn compact_path(path: &Path, base_path: &Path) -> String {
    let mut path_string = path.to_string_lossy().into_owned();

    if let Ok(stripped) = path.strip_prefix(base_path) {
        path_string = format!(".{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    };
    if !path_string.starts_with('.')
        && let Some(home) = maki_storage::paths::home()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        path_string = format!("~{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    };

    path_string
}

/// Wraps a `Vec` of `McpConfigError`s for compact display.
#[derive(Clone, Debug)]
pub struct McpConfigErrors {
    errors: Vec<McpConfigError>,
    initial_wd: PathBuf,
}

impl McpConfigErrors {
    pub fn new(working_directory: PathBuf) -> Self {
        McpConfigErrors {
            errors: Vec::new(),
            initial_wd: working_directory,
        }
    }

    fn add_error(&mut self, e: McpConfigError) {
        self.errors.push(e);
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

impl std::fmt::Display for McpConfigErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut shown = self
            .errors
            .iter()
            .take(2)
            .map(|e: &McpConfigError| match e {
                McpConfigError::Read { path, .. } => {
                    format!("failed to read {}", compact_path(path, &self.initial_wd))
                }
                McpConfigError::Parse { path, .. } => {
                    format!("failed to parse {}", compact_path(path, &self.initial_wd))
                }
            });
        if let Some(first) = shown.next() {
            write!(f, "{}", first)?;
            for rest in shown {
                write!(f, "; {}", rest)?;
            }
        }
        let hidden = self.errors.len().saturating_sub(2);
        if hidden > 0 {
            write!(f, "; ... ({} more)", hidden)?;
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerStatus {
    Connecting,
    Running,
    Disabled,
    Failed(String),
    NeedsAuth { url: Option<String> },
}

impl McpServerStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Connecting)
    }
}

#[derive(Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub transport_kind: &'static str,
    pub tool_count: usize,
    pub prompt_count: usize,
    pub status: McpServerStatus,
    pub config_path: PathBuf,
    pub url: Option<String>,
    pub oauth: Option<OauthClientConfig>,
    pub ca_file: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
pub struct McpConfig {
    /// Defer tools behind `tool_search` only when more than this many
    /// non-`always_load` tools exist. `None` means the built-in default;
    /// 0 always defers, a large value disables deferral.
    #[serde(default)]
    pub defer_tools: Option<usize>,
    #[serde(default)]
    pub mcp: HashMap<String, RawServerConfig>,
    #[serde(skip)]
    pub origins: HashMap<String, PathBuf>,
}

#[derive(Deserialize, Clone)]
pub struct RawServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub always_load: bool,
    #[serde(flatten)]
    pub transport: RawTransport,
}

impl RawServerConfig {
    /// Server declared at runtime instead of in `mcp.toml`.
    pub(super) fn runtime(transport: RawTransport) -> Self {
        Self {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            always_load: false,
            transport,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum RawTransport {
    Stdio(RawStdioFields),
    Http(RawHttpFields),
}

#[derive(Deserialize, Clone)]
pub struct RawStdioFields {
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

#[derive(Deserialize, Clone)]
pub struct RawHttpFields {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub oauth: Option<OauthClientConfig>,
    /// A PEM bundle that replaces the default CAs, for OAuth too, like curl's `--cacert`.
    #[serde(default)]
    pub ca_file: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub name: String,
    pub timeout: Duration,
    /// Skip deferral: every tool from this server enters the context upfront
    /// instead of being discoverable through `tool_search`.
    pub always_load: bool,
    pub transport: Transport,
}

/// OAuth client settings. Without `client_id`, a client is registered
/// dynamically with the redirect URI built from the callback settings.
#[derive(Deserialize, Clone, Debug)]
pub struct OauthClientConfig {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Fixed loopback port so the redirect URI can be pre-registered.
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Loopback path of the redirect URI (defaults to `/mcp/oauth/callback`).
    #[serde(default)]
    pub callback_path: Option<String>,
    /// Loopback hostname of the redirect URI (defaults to `127.0.0.1`).
    #[serde(default)]
    pub callback_hostname: Option<String>,
    /// `https` is for providers that only accept https redirect URIs. Maki runs
    /// no TLS listener, so the user pastes the redirect URL from the browser.
    #[serde(default)]
    pub callback_scheme: CallbackScheme,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CallbackScheme {
    #[default]
    Http,
    Https,
}

#[derive(Clone, Debug)]
pub enum Transport {
    Stdio {
        program: String,
        args: Vec<String>,
        environment: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        oauth: Option<OauthClientConfig>,
        ca_file: Option<PathBuf>,
    },
}

pub(super) use maki_config::is_valid_wire_name as is_valid_tool_name;

impl McpConfig {
    pub fn is_empty(&self) -> bool {
        self.mcp.is_empty()
    }

    pub fn preliminary_infos(&self, disabled: &[String]) -> Vec<McpServerInfo> {
        self.mcp
            .iter()
            .map(|(name, raw)| {
                let status = if !raw.enabled || disabled.contains(name) {
                    McpServerStatus::Disabled
                } else {
                    McpServerStatus::Connecting
                };
                McpServerInfo {
                    name: name.clone(),
                    transport_kind: transport_kind(&raw.transport),
                    tool_count: 0,
                    prompt_count: 0,
                    status,
                    config_path: self.origins.get(name).cloned().unwrap_or_default(),
                    url: match &raw.transport {
                        RawTransport::Http(h) => Some(h.url.clone()),
                        _ => None,
                    },
                    oauth: match &raw.transport {
                        RawTransport::Http(h) => h.oauth.clone(),
                        _ => None,
                    },
                    // The raw path is not resolved yet, and OAuth only starts from published infos.
                    ca_file: None,
                }
            })
            .collect()
    }
}

fn unset_var(server: &str, field: &str, var: &str) -> McpError {
    McpError::Config(format!(
        "server '{server}' {field}: environment variable '{var}' is unset or empty"
    ))
}

/// Erroring (not dropping) surfaces the variable name in the server's status
/// instead of an untraceable 401 later.
fn expand_map(
    server: &str,
    kind: &str,
    map: HashMap<String, String>,
) -> Result<HashMap<String, String>, McpError> {
    map.into_iter()
        .map(|(key, value)| {
            let expanded = expand_env(&value)
                .map_err(|var| unset_var(server, &format!("{kind} '{key}'"), &var))?;
            Ok((key, expanded))
        })
        .collect()
}

/// `${VAR}` goes first, so a variable can hold `~` or an absolute path. A relative
/// path starts from the folder of the `mcp.toml` that sets it, so a global config
/// works from any cwd.
fn resolve_ca_file(server: &str, ca_file: &str, config_path: &Path) -> Result<PathBuf, McpError> {
    let expanded = expand_env(ca_file).map_err(|var| unset_var(server, "ca_file", &var))?;
    let base = config_path.parent().unwrap_or(Path::new(""));
    Ok(base.join(expand_tilde(Path::new(&expanded))))
}

pub fn parse_server(
    name: String,
    server: RawServerConfig,
    config_path: &Path,
) -> Result<ServerConfig, McpError> {
    if !is_valid_server_name(&name) {
        return Err(McpError::Config(format!(
            "server name '{name}' must be ASCII alphanumeric + hyphens"
        )));
    }
    if is_builtin_tool(&name) {
        return Err(McpError::Config(format!(
            "server name '{name}' conflicts with built-in tool"
        )));
    }
    if server.timeout == 0 || server.timeout > MAX_TIMEOUT_MS {
        return Err(McpError::Config(format!(
            "server '{name}' timeout must be 1..={MAX_TIMEOUT_MS}"
        )));
    }
    let transport = match server.transport {
        RawTransport::Stdio(cfg) => {
            let mut cmd = cfg.command.into_iter();
            let program = cmd
                .next()
                .ok_or_else(|| McpError::Config(format!("server '{name}' has empty command")))?;
            Transport::Stdio {
                program,
                args: cmd.collect(),
                environment: expand_map(&name, "environment", cfg.environment)?,
            }
        }
        RawTransport::Http(cfg) => {
            if !cfg.url.starts_with("http://") && !cfg.url.starts_with("https://") {
                return Err(McpError::Config(format!(
                    "server '{name}' url must start with http:// or https://"
                )));
            }
            if let Some(path) = &cfg.oauth.as_ref().and_then(|o| o.callback_path.as_ref())
                && (path.is_empty() || !path.starts_with('/'))
            {
                return Err(McpError::Config(format!(
                    "server '{name}' oauth.callback_path must start with '/'"
                )));
            }
            if let Some(oauth) = &cfg.oauth
                && oauth.client_secret.is_some()
                && oauth.client_id.is_none()
            {
                return Err(McpError::Config(format!(
                    "server '{name}' {SECRET_NEEDS_CLIENT_ID}"
                )));
            }
            if let Some(oauth) = &cfg.oauth
                && oauth.callback_scheme == CallbackScheme::Https
                && oauth.callback_port.is_none()
            {
                return Err(McpError::Config(format!(
                    "server '{name}' {HTTPS_CALLBACK_NEEDS_PORT}"
                )));
            }
            Transport::Http {
                url: cfg.url,
                headers: expand_map(&name, "header", cfg.headers)?,
                oauth: cfg.oauth,
                ca_file: cfg
                    .ca_file
                    .map(|path| resolve_ca_file(&name, &path, config_path))
                    .transpose()?,
            }
        }
    };
    Ok(ServerConfig {
        name,
        timeout: Duration::from_millis(server.timeout),
        always_load: server.always_load,
        transport,
    })
}

pub fn transport_kind(raw: &RawTransport) -> &'static str {
    match raw {
        RawTransport::Stdio(_) => "stdio",
        RawTransport::Http(_) => "http",
    }
}

/// Call order is precedence: the caller merges global first, project last,
/// so a project's servers and `defer_tools` beat the global ones.
fn merge_config(merged: &mut McpConfig, errors: &mut McpConfigErrors, path: &Path) {
    match read_config(path) {
        Ok(None) => {}
        Ok(Some(cfg)) => {
            tracing::info!(
                path = %path.display(),
                servers = cfg.mcp.len(),
                "loaded mcp config"
            );
            for name in cfg.mcp.keys() {
                merged.origins.insert(name.clone(), path.to_path_buf());
            }
            merged.defer_tools = cfg.defer_tools.or(merged.defer_tools);
            merged.mcp.extend(cfg.mcp);
        }
        Err(e) => errors.add_error(e),
    }
}

pub fn load_config(cwd: &Path, project_config: ProjectConfig) -> (McpConfig, McpConfigErrors) {
    load_config_inner(
        cwd,
        maki_storage::paths::find_config_path(MCP_CONFIG_FILE).as_deref(),
        project_config,
    )
}

fn load_config_inner(
    cwd: &Path,
    global_path: Option<&Path>,
    project_config: ProjectConfig,
) -> (McpConfig, McpConfigErrors) {
    let mut merged = McpConfig::default();
    let mut errors = McpConfigErrors::new(cwd.to_path_buf());

    if let Some(global_path) = global_path {
        merge_config(&mut merged, &mut errors, global_path);
    }
    if let Some(project_path) = project_config.gated_path(GatedFile::Mcp) {
        merge_config(&mut merged, &mut errors, &project_path);
    }
    (merged, errors)
}

pub fn persist_enabled(
    config_path: &Path,
    server_name: &str,
    enabled: bool,
) -> Result<(), McpError> {
    let content = fs::read_to_string(config_path).unwrap_or_default();
    let mut doc: DocumentMut = content
        .parse()
        .map_err(|e| McpError::Config(format!("failed to parse {}: {e}", config_path.display())))?;

    let mcp = doc
        .entry("mcp")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let server = mcp
        .as_table_like_mut()
        .ok_or_else(|| McpError::Config("[mcp] is not a table".into()))?
        .entry(server_name)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    server
        .as_table_like_mut()
        .ok_or_else(|| McpError::Config(format!("[mcp.{server_name}] is not a table")))?;
    server["enabled"] = toml_edit::value(enabled);

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| McpError::Config(format!("cannot create dir: {e}")))?;
    }
    fs::write(config_path, doc.to_string())
        .map_err(|e| McpError::Config(format!("cannot write {}: {e}", config_path.display())))?;
    Ok(())
}

fn read_config(path: &Path) -> Result<Option<McpConfig>, McpConfigError> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                path = %path.display(),
                "no mcp config to read"
            );
            return Ok(None);
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "failed to read mcp config"
            );
            return Err(McpConfigError::Read {
                path: path.into(),
                error: e.to_string(),
            });
        }
    };
    toml::from_str(&content)
        .inspect_err(|e| {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to parse mcp config")
        })
        .map_err(|e| McpConfigError::Parse {
            path: path.into(),
            error: e.to_string(),
        })
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_storage::paths::home;
    use test_case::test_case;

    const CONFIG_PATH: &str = "/etc/maki/mcp.toml";

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            always_load: false,
            transport: RawTransport::Stdio(RawStdioFields {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                environment: HashMap::new(),
            }),
        }
    }

    fn parse(name: &str, raw: RawServerConfig) -> Result<ServerConfig, McpError> {
        parse_server(name.into(), raw, Path::new(CONFIG_PATH))
    }

    fn http_raw(url: &str) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            always_load: false,
            transport: RawTransport::Http(RawHttpFields {
                url: url.to_string(),
                headers: HashMap::new(),
                oauth: None,
                ca_file: None,
            }),
        }
    }

    fn http_raw_with_ca(ca_file: &str) -> RawServerConfig {
        let mut cfg = http_raw("https://mcp.example.com/mcp");
        if let RawTransport::Http(http) = &mut cfg.transport {
            http.ca_file = Some(ca_file.into());
        }
        cfg
    }

    #[test_case("srv",       stdio_raw(&[]),            "empty command"        ; "empty_command")]
    #[test_case("bash",      stdio_raw(&["echo"]),      "conflicts with built-in" ; "builtin_name_collision")]
    #[test_case("bad name!", stdio_raw(&["echo"]),      "ASCII alphanumeric"   ; "invalid_server_name")]
    #[test_case("srv",       http_raw("ftp://bad.com"), "http://"              ; "invalid_http_url")]
    #[test_case("srv",       http_raw_with_ca("${MAKI_TEST_MCP_UNSET_84421}/ca.pem"), "MAKI_TEST_MCP_UNSET_84421" ; "ca_file_unset_var")]
    fn parse_server_rejects(name: &str, cfg: RawServerConfig, expected_msg: &str) {
        let err = parse(name, cfg).unwrap_err();
        assert!(err.to_string().contains(expected_msg), "got: {err}");
    }

    #[test]
    fn header_with_unset_var_fails_the_server_with_the_var_name() {
        let config: McpConfig = toml::from_str(
            r#"
[mcp.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${MAKI_TEST_MCP_UNSET_84421}" }
"#,
        )
        .unwrap();
        let err = parse("remote", config.mcp["remote"].clone()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MAKI_TEST_MCP_UNSET_84421"), "got: {msg}");
        assert!(msg.contains("Authorization"), "got: {msg}");
    }

    #[test]
    fn header_with_empty_var_fails_the_server_like_unset() {
        unsafe { std::env::set_var("MAKI_TEST_MCP_EMPTY_84421", "") };
        let config: McpConfig = toml::from_str(
            r#"
[mcp.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${MAKI_TEST_MCP_EMPTY_84421}" }
"#,
        )
        .unwrap();
        let err = parse("remote", config.mcp["remote"].clone()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MAKI_TEST_MCP_EMPTY_84421"), "got: {msg}");
    }

    #[test]
    fn stdio_environment_expands_from_the_process_env() {
        unsafe { std::env::set_var("MAKI_TEST_MCP_ENV_84421", "tok") };
        let config: McpConfig = toml::from_str(
            r#"
[mcp.local]
command = ["server"]
environment = { GITHUB_TOKEN = "${MAKI_TEST_MCP_ENV_84421}" }
"#,
        )
        .unwrap();
        let parsed = parse("local", config.mcp["local"].clone()).unwrap();
        match parsed.transport {
            Transport::Stdio { environment, .. } => {
                assert_eq!(environment["GITHUB_TOKEN"], "tok");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test_case(0               ; "zero")]
    #[test_case(MAX_TIMEOUT_MS + 1 ; "over_max")]
    fn invalid_timeout_rejected(timeout: u64) {
        let mut cfg = stdio_raw(&["echo"]);
        cfg.timeout = timeout;
        let err = parse("srv", cfg).unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn toml_deferral_fields_deserialize_and_default() {
        let config: McpConfig = toml::from_str(
            r#"
defer_tools = 30

[mcp.github]
command = ["gh", "mcp-server"]
always_load = true

[mcp.other]
command = ["other"]
"#,
        )
        .unwrap();
        assert_eq!(config.defer_tools, Some(30));
        assert!(config.mcp["github"].always_load);
        assert!(!config.mcp["other"].always_load);
        let parsed = parse("github", config.mcp["github"].clone()).unwrap();
        assert!(parsed.always_load);

        let bare: McpConfig = toml::from_str("[mcp.srv]\ncommand = [\"x\"]").unwrap();
        assert_eq!(bare.defer_tools, None);
    }

    #[test]
    fn parse_splits_command_into_program_and_args() {
        let result = parse("srv", stdio_raw(&["npx", "-y", "server"])).unwrap();
        match &result.transport {
            Transport::Stdio { program, args, .. } => {
                assert_eq!(program, "npx");
                assert_eq!(args, &["-y", "server"]);
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn toml_deserialization() {
        let toml_str = r#"
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_TOKEN = "tok" }
timeout = 10000
enabled = false

[mcp.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok123" }
"#;
        let config: McpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mcp.len(), 3);

        assert!(matches!(
            config.mcp["filesystem"].transport,
            RawTransport::Stdio(_)
        ));

        let gh_cfg = &config.mcp["github"];
        assert!(!gh_cfg.enabled);
        assert_eq!(gh_cfg.timeout, 10000);
        match &gh_cfg.transport {
            RawTransport::Stdio(s) => assert_eq!(s.environment["GITHUB_TOKEN"], "tok"),
            _ => panic!("expected Stdio"),
        }

        match &config.mcp["remote"].transport {
            RawTransport::Http(h) => {
                assert_eq!(h.url, "https://mcp.example.com/mcp");
                assert_eq!(h.headers["Authorization"], "Bearer tok123");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_client_config_deserializes() {
        let toml_str = r#"
[mcp.acme]
url = "https://mcp.acme.example.com/mcp"
oauth = { client_id = "acme-client", client_secret = "s3cret", callback_port = 3118, callback_path = "/callback" }
"#;
        let config: McpConfig = toml::from_str(toml_str).unwrap();
        let parsed = parse("acme", config.mcp["acme"].clone()).unwrap();
        match parsed.transport {
            Transport::Http { url, oauth, .. } => {
                assert_eq!(url, "https://mcp.acme.example.com/mcp");
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_id.as_deref(), Some("acme-client"));
                assert_eq!(oauth.client_secret.as_deref(), Some("s3cret"));
                assert_eq!(oauth.callback_port, Some(3118));
                assert_eq!(oauth.callback_path.as_deref(), Some("/callback"));
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_client_secret_and_port_optional() {
        let config: McpConfig = toml::from_str(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = { client_id = \"acme-client\" }\n",
        )
        .unwrap();
        let parsed = parse("acme", config.mcp["acme"].clone()).unwrap();
        match parsed.transport {
            Transport::Http { oauth, .. } => {
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_secret, None);
                assert_eq!(oauth.callback_port, None);
                assert_eq!(oauth.callback_path, None);
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_callback_path_must_start_with_slash() {
        let config: McpConfig = toml::from_str(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = { client_id = \"acme-client\", callback_path = \"callback\" }\n",
        )
        .unwrap();
        let err = parse("acme", config.mcp["acme"].clone()).unwrap_err();
        assert!(err.to_string().contains("callback_path"));
    }

    #[test_case("callback_port = 3119, callback_scheme = \"https\"", CallbackScheme::Https ; "https")]
    #[test_case("callback_port = 3119", CallbackScheme::Http ; "defaults_to_http")]
    fn oauth_callback_scheme_parses(fields: &str, expected: CallbackScheme) {
        let config: McpConfig = toml::from_str(&format!(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = {{ client_id = \"acme-client\", {fields} }}\n"
        ))
        .unwrap();
        match parse("acme", config.mcp["acme"].clone()).unwrap().transport {
            Transport::Http { oauth, .. } => assert_eq!(oauth.unwrap().callback_scheme, expected),
            _ => panic!("expected Http"),
        }
    }

    #[test_case("client_id = \"acme-client\", callback_scheme = \"https\"", HTTPS_CALLBACK_NEEDS_PORT ; "https_without_port")]
    #[test_case("client_secret = \"s3cret\"",                                  SECRET_NEEDS_CLIENT_ID    ; "secret_without_client_id")]
    fn oauth_invalid_rejected(fields: &str, expected: &str) {
        let config: McpConfig = toml::from_str(&format!(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = {{ {fields} }}\n"
        ))
        .unwrap();
        let err = parse("acme", config.mcp["acme"].clone()).unwrap_err();
        assert!(err.to_string().contains(expected));
    }

    #[test]
    fn oauth_without_client_id_parses() {
        let config: McpConfig = toml::from_str(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = { callback_scheme = \"https\", callback_port = 3119, callback_path = \"/callback\", callback_hostname = \"localhost\" }\n",
        )
        .unwrap();
        match parse("acme", config.mcp["acme"].clone()).unwrap().transport {
            Transport::Http { oauth, .. } => assert_eq!(oauth.unwrap().client_id, None),
            _ => panic!("expected Http"),
        }
    }

    #[test_case("certs/ca.pem",                         "/etc/maki/certs/ca.pem" ; "relative_to_config_dir")]
    #[test_case("/certs/ca.pem",                        "/certs/ca.pem"          ; "absolute")]
    #[test_case("~/ca.pem",                             "~/ca.pem"               ; "tilde")]
    #[test_case("${MAKI_TEST_MCP_CA_DIR_84421}/ca.pem", "/opt/certs/ca.pem"      ; "env_var")]
    fn ca_file_resolves_to_an_absolute_path(ca_file: &str, expected: &str) {
        if expected.starts_with('~') && home().is_none() {
            return;
        }
        unsafe { std::env::set_var("MAKI_TEST_MCP_CA_DIR_84421", "/opt/certs") };
        let parsed = parse("srv", http_raw_with_ca(ca_file)).unwrap();
        match parsed.transport {
            Transport::Http { ca_file: got, .. } => {
                assert_eq!(got, Some(expand_tilde(Path::new(expected))))
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn project_config_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let global_dir = dir.path().join("global");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("mcp.toml"),
            r#"[mcp.srv]
command = ["global"]
timeout = 5000
"#,
        )
        .unwrap();

        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        let project_maki_dir = project_dir.join(".maki");
        fs::create_dir_all(&project_maki_dir).unwrap();
        fs::write(
            project_maki_dir.join("mcp.toml"),
            r#"[mcp.srv]
command = ["project"]
"#,
        )
        .unwrap();

        let (global_only, errors) = load_config_inner(
            &project_dir,
            Some(&global_dir.join(MCP_CONFIG_FILE)),
            ProjectConfig::discover(&project_dir),
        );
        assert!(errors.is_empty());
        let global = parse("srv", global_only.mcp["srv"].clone()).unwrap();
        match global.transport {
            Transport::Stdio { program, .. } => assert_eq!(program, "global"),
            _ => panic!("expected Stdio"),
        }

        let (merged, errors) = load_config_inner(
            &project_dir,
            Some(&global_dir.join(MCP_CONFIG_FILE)),
            ProjectConfig::for_project(&project_dir),
        );
        assert!(errors.is_empty());
        let all: Vec<_> = merged
            .mcp
            .into_iter()
            .filter(|(_, v)| v.enabled)
            .map(|(name, cfg)| parse(&name, cfg))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(all.len(), 1);
        match &all[0].transport {
            Transport::Stdio { program, .. } => assert_eq!(program, "project"),
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn persist_enabled_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");

        persist_enabled(&path, "srv", false).unwrap();
        let doc: toml_edit::DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["mcp"]["srv"]["enabled"].as_bool(), Some(false));

        fs::write(
            &path,
            r#"[mcp.srv]
command = ["echo"]
timeout = 5000
enabled = true
"#,
        )
        .unwrap();
        persist_enabled(&path, "srv", false).unwrap();
        let doc: toml_edit::DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["mcp"]["srv"]["enabled"].as_bool(), Some(false));
        assert!(doc["mcp"]["srv"]["command"].is_array());
        assert_eq!(doc["mcp"]["srv"]["timeout"].as_integer(), Some(5000));
    }

    #[test]
    fn preliminary_infos_statuses() {
        let mut off = stdio_raw(&["echo"]);
        off.enabled = false;
        let config = McpConfig {
            mcp: [
                ("enabled".into(), stdio_raw(&["echo"])),
                ("disabled-config".into(), off),
                ("disabled-runtime".into(), stdio_raw(&["echo"])),
            ]
            .into(),
            origins: [("enabled".into(), PathBuf::from("/test.toml"))].into(),
            ..Default::default()
        };
        let mut infos = config.preliminary_infos(&["disabled-runtime".into()]);
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].status, McpServerStatus::Disabled);
        assert_eq!(infos[1].status, McpServerStatus::Disabled);
        assert_eq!(infos[2].status, McpServerStatus::Connecting);
        assert_eq!(infos[2].config_path, PathBuf::from("/test.toml"));
    }

    #[test_case("defer_tools = 7\n", Some(7) ; "project_overrides_global")]
    #[test_case("", Some(5) ; "global_survives_unset_project")]
    fn merge_config_defer_tools_precedence(project_toml: &str, expected: Option<usize>) {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        fs::write(&global, "defer_tools = 5\n[mcp.srv]\ncommand = [\"a\"]").unwrap();
        fs::write(
            &project,
            format!("{project_toml}[mcp.srv]\ncommand = [\"b\"]"),
        )
        .unwrap();

        let mut merged = McpConfig::default();
        let mut errors = McpConfigErrors::new(dir.path().to_path_buf());
        merge_config(&mut merged, &mut errors, &global);
        merge_config(&mut merged, &mut errors, &project);

        assert!(errors.is_empty());
        assert_eq!(merged.defer_tools, expected);
        assert_eq!(merged.origins["srv"], project, "later config must win");
    }

    #[test]
    fn read_config_directory_path_returns_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            read_config(&path),
            Err(McpConfigError::Read { .. })
        ));
    }

    #[test]
    fn read_config_invalid_toml_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        fs::write(&path, "this is not valid toml {{").unwrap();
        assert!(matches!(
            read_config(&path),
            Err(McpConfigError::Parse { .. })
        ));
    }

    #[test]
    fn read_config_valid_toml_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        fs::write(
            &path,
            r#"[mcp.valid]
command = ["echo", "hello"]
"#,
        )
        .unwrap();
        let cfg = read_config(&path).unwrap().unwrap();
        assert!(cfg.mcp.contains_key("valid"));
    }
}
