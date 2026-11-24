//! Settings (TOML in ~/.config/loki) and the local-network policy.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    /// Ask before every tool call.
    Ask,
    /// Run read-only tools without asking.
    #[default]
    AutoRead,
    /// Never ask.
    AutoAll,
}

impl Approval {
    pub const ALL: [Approval; 3] = [Approval::Ask, Approval::AutoRead, Approval::AutoAll];

    pub fn label(self) -> &'static str {
        match self {
            Approval::Ask => "Ask before every tool",
            Approval::AutoRead => "Auto-approve reading",
            Approval::AutoAll => "Auto-approve everything",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Approval::Ask => "Ask always",
            Approval::AutoRead => "Auto-read",
            Approval::AutoAll => "Auto-all",
        }
    }
}

/// What the system tray shows.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrayIcon {
    /// Lucide "bot-message-square" outline, recolored by Plasma to match the panel.
    #[default]
    Monochrome,
    /// The colored app logo.
    Color,
    /// `tray_icon_custom`: an icon theme name or the path of an SVG/PNG file.
    Custom,
}

impl TrayIcon {
    pub const ALL: [TrayIcon; 3] = [TrayIcon::Monochrome, TrayIcon::Color, TrayIcon::Custom];

    pub fn label(self) -> &'static str {
        match self {
            TrayIcon::Monochrome => "Monochrome",
            TrayIcon::Color => "Color",
            TrayIcon::Custom => "Custom…",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    /// OpenAI-compatible base URL. `/v1` is appended when the URL has no path.
    pub server_url: String,
    pub api_key: String,
    pub model: String,
    pub restrict_to_lan: bool,
    pub temperature: f32,
    /// 0 = server default.
    pub max_tokens: u32,
    /// Abort when the server sends nothing for this long.
    pub idle_timeout_secs: u64,
    /// Appended to the built-in system prompt.
    pub system_prompt: String,

    pub tools_enabled: bool,
    pub approval: Approval,
    /// Start directory for tools (empty = the scratch directory).
    pub working_dir: String,
    /// Where temporary and generated files go (empty = ~/.local/share/loki/scratch).
    pub scratch_dir: String,
    pub command_timeout_secs: u64,
    pub max_output_chars: usize,
    pub max_tool_rounds: u32,

    pub width: f32,
    pub height: f32,
    pub hide_on_focus_loss: bool,
    pub keep_history: bool,
    pub tray_icon: TrayIcon,
    pub tray_icon_custom: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "http://192.168.0.171:8080".into(),
            api_key: String::new(),
            model: String::new(),
            restrict_to_lan: true,
            temperature: 0.6,
            max_tokens: 0,
            idle_timeout_secs: 300,
            system_prompt: String::new(),
            tools_enabled: true,
            approval: Approval::AutoRead,
            working_dir: String::new(),
            scratch_dir: String::new(),
            command_timeout_secs: 120,
            max_output_chars: 30_000,
            max_tool_rounds: 40,
            width: 780.0,
            height: 600.0,
            hide_on_focus_loss: true,
            keep_history: true,
            tray_icon: TrayIcon::Monochrome,
            tray_icon_custom: String::new(),
        }
    }
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home_dir().join(fallback))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("loki")
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("loki")
}

/// Default scratch directory for temporary and generated files: `~/.local/share/loki/scratch`.
pub fn default_scratch_dir() -> PathBuf {
    data_dir().join("scratch")
}

impl Config {
    fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("loki: ignoring invalid {}: {e}", Self::path().display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(), text)
    }

    /// Base URL with `/v1` added when no path was given.
    pub fn api_base(&self) -> String {
        let base = self.server_url.trim().trim_end_matches('/');
        match base.split_once("://") {
            Some((_, rest)) if !rest.contains('/') => format!("{base}/v1"),
            _ => base.to_string(),
        }
    }

    pub fn host(&self) -> String {
        let url = self.server_url.trim();
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        rest.split('/').next().unwrap_or(rest).to_string()
    }

    /// Why a request to the configured server must not be made, if it must not.
    pub fn policy_error(&self) -> Option<String> {
        let url = self.server_url.trim();
        if url.starts_with("https://") {
            return Some("LoKI talks plain http:// to local servers; https is not supported.".into());
        }
        if !url.starts_with("http://") {
            return Some("The server URL must start with http://".into());
        }
        if self.restrict_to_lan && !is_local_url(url) {
            return Some(format!(
                "{} is not a local network address. Disable “Local network only” in the settings to allow it.",
                self.host()
            ));
        }
        None
    }

    pub fn start_dir(&self) -> PathBuf {
        let wd = self.working_dir.trim();
        if wd.is_empty() { self.ensure_scratch_dir() } else { expand_home(wd) }
    }

    pub fn scratch_dir(&self) -> PathBuf {
        let dir = self.scratch_dir.trim();
        if dir.is_empty() { default_scratch_dir() } else { expand_home(dir) }
    }

    /// The scratch directory, created (private to the user) if it doesn't exist yet.
    pub fn ensure_scratch_dir(&self) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let dir = self.scratch_dir();
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
        dir
    }
}

pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

pub fn collapse_home(path: &std::path::Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// True for loopback, private/link-local/CGNAT IPs and LAN-style host names.
pub fn is_local_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        let v6 = v6.split(']').next().unwrap_or("").to_ascii_lowercase();
        return v6 == "::1"
            || v6.starts_with("fc")
            || v6.starts_with("fd")
            || ["fe8", "fe9", "fea", "feb"].iter().any(|p| v6.starts_with(p));
    } else {
        authority.split(':').next().unwrap_or("").to_ascii_lowercase()
    };
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let [a, b, ..] = ip.octets();
        return ip.is_loopback() || ip.is_private() || ip.is_link_local() || (a == 100 && (64..=127).contains(&b));
    }
    if host.is_empty() {
        return false;
    }
    host == "localhost"
        || !host.contains('.')
        || [".local", ".lan", ".home", ".internal", ".localdomain", ".home.arpa"].iter().any(|s| host.ends_with(s))
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_start_dir_is_private_scratch() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Config::default().start_dir();
        assert_eq!(dir, default_scratch_dir());
        assert!(dir.is_dir() && dir.ends_with("loki/scratch"));
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077, 0);
    }

    #[test]
    fn custom_scratch_dir() {
        let cfg = Config { scratch_dir: "~/loki-test-scratch".into(), ..Default::default() };
        assert_eq!(cfg.scratch_dir(), home_dir().join("loki-test-scratch"));
        let tmp = std::env::temp_dir().join(format!("loki-custom-scratch-{}", std::process::id()));
        let cfg = Config { scratch_dir: tmp.display().to_string(), ..Default::default() };
        assert_eq!(cfg.start_dir(), tmp);
        assert!(tmp.is_dir());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    use super::*;

    #[test]
    fn local_urls() {
        for u in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://192.168.0.171:8080",
            "http://10.0.0.5",
            "http://172.20.1.1",
            "http://gpubox:11434",
            "http://gpubox.local:11434",
            "http://[::1]:8080",
            "http://[fd12::1]/v1",
            "http://100.100.1.2",
        ] {
            assert!(is_local_url(u), "{u}");
        }
        for u in [
            "https://api.openai.com/v1",
            "http://8.8.8.8",
            "http://172.32.0.1",
            "http://example.com",
            "ftp://localhost",
            "http://[2001:db8::1]",
            "http://evil.com@8.8.8.8/",
        ] {
            assert!(!is_local_url(u), "{u}");
        }
    }

    #[test]
    fn api_base() {
        let mut c = Config { server_url: "http://192.168.0.171:8080".into(), ..Default::default() };
        assert_eq!(c.api_base(), "http://192.168.0.171:8080/v1");
        c.server_url = "http://h:1/v1/".into();
        assert_eq!(c.api_base(), "http://h:1/v1");
        c.server_url = "http://h/api/v2".into();
        assert_eq!(c.api_base(), "http://h/api/v2");
    }
}
