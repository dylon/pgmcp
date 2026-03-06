use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{PgmcpError, Result};

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub indexer: IndexerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub work_pool: WorkPoolConfig,
    #[serde(default)]
    pub cron: CronConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default = "default_workspace_paths")]
    pub paths: Vec<String>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            paths: default_workspace_paths(),
        }
    }
}

fn default_workspace_paths() -> Vec<String> {
    vec![]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTypeMapping {
    pub extension: String,
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerConfig {
    #[serde(default = "default_file_types")]
    pub file_types: Vec<FileTypeMapping>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: u64,
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            file_types: default_file_types(),
            debounce_ms: default_debounce_ms(),
            max_file_size_bytes: default_max_file_size(),
            exclude_patterns: default_exclude_patterns(),
        }
    }
}

impl IndexerConfig {
    /// Build extension → language lookup map.
    #[allow(dead_code)]
    pub fn extension_map(&self) -> HashMap<String, String> {
        self.file_types
            .iter()
            .map(|ft| (ft.extension.clone(), ft.language.clone()))
            .collect()
    }

    /// Check if an extension is configured for indexing.
    pub fn is_configured_extension(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| self.file_types.iter().any(|ft| ft.extension == ext))
            .unwrap_or(false)
    }

    /// Get the language for a file path, if configured.
    pub fn language_for_path(&self, path: &Path) -> Option<String> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| {
                self.file_types
                    .iter()
                    .find(|ft| ft.extension == ext)
                    .map(|ft| ft.language.clone())
            })
    }
}

fn default_file_types() -> Vec<FileTypeMapping> {
    vec![
        FileTypeMapping { extension: "rs".into(), language: "rust".into() },
        FileTypeMapping { extension: "md".into(), language: "markdown".into() },
        FileTypeMapping { extension: "metta".into(), language: "metta".into() },
        FileTypeMapping { extension: "rho".into(), language: "rholang".into() },
        FileTypeMapping { extension: "js".into(), language: "javascript".into() },
        FileTypeMapping { extension: "jsx".into(), language: "javascript".into() },
        FileTypeMapping { extension: "py".into(), language: "python".into() },
        FileTypeMapping { extension: "pl".into(), language: "prolog".into() },
        FileTypeMapping { extension: "pro".into(), language: "prolog".into() },
        FileTypeMapping { extension: "ts".into(), language: "typescript".into() },
        FileTypeMapping { extension: "tsx".into(), language: "typescript".into() },
        FileTypeMapping { extension: "toml".into(), language: "toml".into() },
        FileTypeMapping { extension: "json".into(), language: "json".into() },
        FileTypeMapping { extension: "yaml".into(), language: "yaml".into() },
        FileTypeMapping { extension: "yml".into(), language: "yaml".into() },
    ]
}

fn default_debounce_ms() -> u64 {
    300
}

fn default_max_file_size() -> u64 {
    1_048_576 // 1 MB
}

fn default_exclude_patterns() -> Vec<String> {
    vec![
        "node_modules".into(),
        "target".into(),
        ".git".into(),
        "__pycache__".into(),
        "*.lock".into(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_host")]
    pub host: String,
    #[serde(default = "default_db_port")]
    pub port: u16,
    #[serde(default = "default_db_name")]
    pub name: String,
    #[serde(default = "default_db_user")]
    pub user: String,
    pub password: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: default_db_host(),
            port: default_db_port(),
            name: default_db_name(),
            user: default_db_user(),
            password: None,
            max_connections: default_max_connections(),
        }
    }
}

impl DatabaseConfig {
    /// Build the database connection URL.
    pub fn connection_url(&self) -> String {
        let password = self
            .password
            .clone()
            .or_else(|| std::env::var("PGMCP_DB_PASSWORD").ok())
            .unwrap_or_default();

        if password.is_empty() {
            format!(
                "postgres://{}@{}:{}/{}",
                self.user, self.host, self.port, self.name
            )
        } else {
            format!(
                "postgres://{}:{}@{}:{}/{}",
                self.user, password, self.host, self.port, self.name
            )
        }
    }
}

fn default_db_host() -> String { "localhost".into() }
fn default_db_port() -> u16 { 5432 }
fn default_db_name() -> String { "pgmcp".into() }
fn default_db_user() -> String { "pgmcp".into() }
fn default_max_connections() -> u32 { 20 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: usize,
    #[serde(default = "default_chunk_size")]
    pub chunk_size_lines: usize,
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap_lines: usize,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_embed_pool_size")]
    pub pool_size: usize,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            dimensions: default_dimensions(),
            chunk_size_lines: default_chunk_size(),
            chunk_overlap_lines: default_chunk_overlap(),
            batch_size: default_batch_size(),
            pool_size: default_embed_pool_size(),
        }
    }
}

fn default_model() -> String { "all-MiniLM-L6-v2".into() }
fn default_dimensions() -> usize { 384 }
fn default_chunk_size() -> usize { 50 }
fn default_chunk_overlap() -> usize { 10 }
fn default_batch_size() -> usize { 32 }
fn default_embed_pool_size() -> usize { 2 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Bind address for the Streamable HTTP transport (daemon mode).
    #[serde(default = "default_mcp_host")]
    pub host: String,
    /// Port for the Streamable HTTP transport (daemon mode).
    #[serde(default = "default_mcp_port")]
    pub port: u16,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            host: default_mcp_host(),
            port: default_mcp_port(),
        }
    }
}

fn default_transport() -> String { "stdio".into() }
fn default_mcp_host() -> String { "127.0.0.1".into() }
fn default_mcp_port() -> u16 { 3100 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_http_enabled")]
    pub http_enabled: bool,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            http_enabled: default_http_enabled(),
            http_port: default_http_port(),
            http_bind: default_http_bind(),
        }
    }
}

fn default_http_enabled() -> bool { true }
fn default_http_port() -> u16 { 9464 }
fn default_http_bind() -> String { "127.0.0.1".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_file")]
    pub file: String,
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_rotation")]
    pub rotation: String,
    #[serde(default = "default_max_log_files")]
    pub max_log_files: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            file: default_log_file(),
            level: default_log_level(),
            rotation: default_rotation(),
            max_log_files: default_max_log_files(),
        }
    }
}

fn default_log_file() -> String { "~/.local/share/pgmcp/pgmcp.log".into() }
fn default_log_level() -> String { "info".into() }
fn default_rotation() -> String { "daily".into() }
fn default_max_log_files() -> u32 { 7 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkPoolConfig {
    #[serde(default = "default_min_threads")]
    pub min_threads: usize,
    #[serde(default)]
    pub max_threads: usize,
    #[serde(default)]
    pub initial_threads: usize,
}

impl Default for WorkPoolConfig {
    fn default() -> Self {
        Self {
            min_threads: default_min_threads(),
            max_threads: 0,
            initial_threads: 0,
        }
    }
}

impl WorkPoolConfig {
    /// Resolve 0 values to actual thread counts.
    pub fn resolved_max_threads(&self) -> usize {
        if self.max_threads == 0 {
            num_cpus::get()
        } else {
            self.max_threads
        }
    }

    pub fn resolved_initial_threads(&self) -> usize {
        if self.initial_threads == 0 {
            self.min_threads
        } else {
            self.initial_threads
        }
    }
}

fn default_min_threads() -> usize { 2 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    #[serde(default = "default_stale_cleanup")]
    pub stale_cleanup_interval_secs: u64,
    #[serde(default = "default_integrity_check")]
    pub integrity_check_interval_secs: u64,
    #[serde(default = "default_stats_aggregation")]
    pub stats_aggregation_interval_secs: u64,
    #[serde(default = "default_db_maintenance")]
    pub db_maintenance_interval_secs: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            stale_cleanup_interval_secs: default_stale_cleanup(),
            integrity_check_interval_secs: default_integrity_check(),
            stats_aggregation_interval_secs: default_stats_aggregation(),
            db_maintenance_interval_secs: default_db_maintenance(),
        }
    }
}

fn default_stale_cleanup() -> u64 { 3600 }
fn default_integrity_check() -> u64 { 86400 }
fn default_stats_aggregation() -> u64 { 60 }
fn default_db_maintenance() -> u64 { 604_800 }

impl Config {
    /// Load configuration from the default path or the specified path.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        };

        if !config_path.exists() {
            return Ok(Config::default());
        }

        let content =
            std::fs::read_to_string(&config_path).map_err(|e| PgmcpError::file_io(&config_path, e))?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Default config file path: ~/.config/pgmcp/config.toml
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("pgmcp")
            .join("config.toml")
    }

    /// Generate default config content as TOML string.
    pub fn default_toml() -> String {
        let config = Config::default();
        toml::to_string_pretty(&config).expect("Failed to serialize default config")
    }

    /// Write the default config to the default path.
    pub fn write_default() -> Result<PathBuf> {
        let path = Self::default_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PgmcpError::file_io(parent, e))?;
        }
        std::fs::write(&path, Self::default_toml())
            .map_err(|e| PgmcpError::file_io(&path, e))?;
        Ok(path)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: WorkspaceConfig::default(),
            indexer: IndexerConfig::default(),
            database: DatabaseConfig::default(),
            embeddings: EmbeddingsConfig::default(),
            mcp: McpConfig::default(),
            metrics: MetricsConfig::default(),
            logging: LoggingConfig::default(),
            work_pool: WorkPoolConfig::default(),
            cron: CronConfig::default(),
        }
    }
}

/// Per-project override config (.pgmcp.toml in project root).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct ProjectOverride {
    #[serde(default)]
    pub indexer: Option<ProjectIndexerOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIndexerOverride {
    pub exclude_patterns: Option<Vec<String>>,
    pub file_types: Option<Vec<FileTypeMapping>>,
    pub max_file_size_bytes: Option<u64>,
}

impl ProjectOverride {
    #[allow(dead_code)]
    pub fn load(project_root: &Path) -> Option<Self> {
        let path = project_root.join(".pgmcp.toml");
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        toml::from_str(&content).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_parses() {
        let toml_str = Config::default_toml();
        let _config: Config = toml::from_str(&toml_str).expect("Default config should parse");
    }

    #[test]
    fn test_extension_map() {
        let config = IndexerConfig::default();
        let map = config.extension_map();
        assert_eq!(map.get("rs"), Some(&"rust".to_string()));
        assert_eq!(map.get("py"), Some(&"python".to_string()));
    }

    #[test]
    fn test_is_configured_extension() {
        let config = IndexerConfig::default();
        assert!(config.is_configured_extension(Path::new("foo.rs")));
        assert!(config.is_configured_extension(Path::new("bar.py")));
        assert!(!config.is_configured_extension(Path::new("baz.exe")));
    }

    #[test]
    fn test_language_for_path() {
        let config = IndexerConfig::default();
        assert_eq!(config.language_for_path(Path::new("foo.rs")), Some("rust".into()));
        assert_eq!(config.language_for_path(Path::new("foo.xyz")), None);
    }

    #[test]
    fn test_database_url() {
        let db = DatabaseConfig::default();
        let url = db.connection_url();
        assert!(url.starts_with("postgres://pgmcp@localhost:5432/pgmcp"));
    }

    #[test]
    fn test_work_pool_config_defaults() {
        let wpc = WorkPoolConfig::default();
        assert_eq!(wpc.min_threads, 2);
        assert!(wpc.resolved_max_threads() >= 1);
        assert_eq!(wpc.resolved_initial_threads(), 2);
    }
}
