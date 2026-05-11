//! Server configuration loaded from `server.yaml`.
//!
//! On first run (or with `--config <path>` pointing at a missing file) the
//! server writes a fully-commented default file at the given path so the
//! operator can edit it without first knowing the schema.
//!
//! All fields are optional in the YAML — missing keys fall back to
//! `Default::default`. Command-line flags (`--bind`, `--world`, etc.) still
//! take precedence over the file so a one-off override doesn't require
//! editing config.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub network: NetworkConfig,
    pub world: WorldConfig,
    pub dashboard: DashboardConfig,
}

/// Networking / chunk-streaming knobs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// `host:port` to bind. Default `0.0.0.0:25565`.
    pub bind: String,
    /// Maximum simultaneous players advertised in the status response.
    pub max_players: u32,
    /// Server-side view distance: the maximum number of chunks (Chebyshev)
    /// from the player at which the server will send chunk data. The
    /// client may *render* fewer chunks than this (its own video setting),
    /// but cannot render more — this is the hard upper bound.
    pub view_distance: i32,
    /// Simulation distance: how far ticking entities/redstone propagate.
    /// Currently informational (we don't tick yet) but sent in the Login
    /// packet for protocol compliance.
    pub simulation_distance: i32,
    /// On chunk-boundary crossings, chunks within Chebyshev distance
    /// `immediate_radius` are sent synchronously before the chunk-cache
    /// center update. Outer chunks are queued and drained at
    /// `chunks_per_iter` per main-loop iteration.
    ///
    /// If `null`, all new chunks are immediate (matches `view_distance`).
    pub immediate_radius: Option<i32>,
    /// Maximum deferred chunks sent per main-loop iteration.
    pub chunks_per_iter: usize,
}

/// World storage and pre-generation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorldConfig {
    /// Where saved (player-modified) chunks are persisted.
    pub dir: PathBuf,
    /// Autosave cadence in seconds.
    pub autosave_interval_secs: u64,
    /// Worldgen seed. CLI `--seed` overrides this.
    pub seed: u32,
    /// Number of chunks (radius, Chebyshev) around origin to pre-generate
    /// at startup so the spawn region is immediate. Beyond this, chunks
    /// generate lazily as players approach.
    pub pregenerate_radius: i32,
    /// Worldgen preset: a built-in name (`"noise"`, `"superflat"`) or
    /// a path to a JSON file describing a custom pipeline. See
    /// `crates/server/src/worldgen/presets/*.json` for examples and
    /// `worldgen::preset` for the schema.
    pub preset: String,
}

/// Dashboard (live graph + metrics over HTTP).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardConfig {
    /// HTTP port for the dashboard. Bound to localhost only.
    pub port: u16,
}

// ── Defaults ────────────────────────────────────────────────────────────────

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig::default(),
            world: WorldConfig::default(),
            dashboard: DashboardConfig::default(),
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:25565".to_string(),
            max_players: 20,
            view_distance: 8,
            simulation_distance: 8,
            immediate_radius: None,
            chunks_per_iter: 5,
        }
    }
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("world"),
            autosave_interval_secs: 300,
            seed: 0xC0FFEE,
            pregenerate_radius: 8,
            preset: "noise".to_string(),
        }
    }
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self { port: 8000 }
    }
}

// ── Loading ─────────────────────────────────────────────────────────────────

/// The default `server.yaml` written on first run, with comments explaining
/// each field. Kept as a string literal because `serde_yaml` doesn't
/// preserve comments through round-trip.
pub const DEFAULT_CONFIG_YAML: &str = r#"# Ultimate Minecraft -- server configuration.
#
# This file is auto-created on first run with the defaults below. Edit
# any field; commented-out lines fall back to the built-in default. CLI
# flags (--bind, --world, --seed, --dashboard-port) override matching
# fields in this file.

network:
  # Address and port to listen on. Use 0.0.0.0 for all interfaces.
  bind: "0.0.0.0:25565"
  # Maximum simultaneous players (advertised in the status response).
  max_players: 20
  # Server-side view distance, in chunks (Chebyshev radius). The client
  # may render fewer than this, but cannot render more.
  view_distance: 8
  # Simulation distance: how far ticking entities/redstone propagate.
  # Currently informational; sent in the Login packet.
  simulation_distance: 8
  # When the player crosses a chunk boundary, chunks within this radius
  # are sent SYNCHRONOUSLY before the cache-center update; outer chunks
  # queue and drain at `chunks_per_iter` per main-loop iteration.
  # null = every new chunk is immediate (matches view_distance).
  immediate_radius: null
  # Deferred-chunk drain rate, per main-loop iteration.
  chunks_per_iter: 5

world:
  # Directory for saved (player-modified) chunks.
  dir: "world"
  # Autosave cadence, seconds.
  autosave_interval_secs: 300
  # Worldgen seed. Override on the CLI with --seed <u32>.
  seed: 12648430   # 0xC0FFEE
  # Chunks (radius) to pre-generate at startup so spawn is immediate.
  pregenerate_radius: 8
  # Worldgen preset. Built-in: "noise" (default, vanilla-ish noise terrain)
  # or "superflat" (flat layered world). Anything else is treated as a
  # path to a JSON file -- see crates/server/src/worldgen/presets/ for
  # examples and the worldgen::preset module for the schema.
  preset: "noise"

dashboard:
  # HTTP port for the live dashboard. Bound to localhost only.
  port: 8000
"#;

/// Load `path` if it exists, otherwise write the default file there and
/// return the defaults. Errors propagate from disk and YAML parsing.
pub fn load_or_create(path: &Path) -> anyhow::Result<ServerConfig> {
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
        let cfg: ServerConfig = serde_yaml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))?;
        Ok(cfg)
    } else {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        std::fs::write(path, DEFAULT_CONFIG_YAML)
            .map_err(|e| anyhow::anyhow!("writing default config to {}: {}", path.display(), e))?;
        tracing::info!("Wrote default config to {}", path.display());
        Ok(ServerConfig::default())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_yaml() {
        let cfg = ServerConfig::default();
        let text = serde_yaml::to_string(&cfg).unwrap();
        let parsed: ServerConfig = serde_yaml::from_str(&text).unwrap();
        assert_eq!(cfg.network.bind, parsed.network.bind);
        assert_eq!(cfg.network.view_distance, parsed.network.view_distance);
        assert_eq!(cfg.world.seed, parsed.world.seed);
    }

    #[test]
    fn embedded_default_yaml_parses() {
        let cfg: ServerConfig = serde_yaml::from_str(DEFAULT_CONFIG_YAML).unwrap();
        // Sanity-check a few values match the Rust defaults so the YAML
        // doesn't drift from the code.
        let defaults = ServerConfig::default();
        assert_eq!(cfg.network.bind, defaults.network.bind);
        assert_eq!(cfg.network.view_distance, defaults.network.view_distance);
        assert_eq!(cfg.world.dir, defaults.world.dir);
        assert_eq!(cfg.world.seed, defaults.world.seed);
        assert_eq!(cfg.dashboard.port, defaults.dashboard.port);
    }

    #[test]
    fn unknown_field_rejected() {
        // `deny_unknown_fields` should surface typos rather than silently ignore.
        let bad = "network:\n  bind: \"0.0.0.0:25565\"\n  bogus_field: 42\n";
        assert!(serde_yaml::from_str::<ServerConfig>(bad).is_err());
    }

    #[test]
    fn partial_yaml_uses_defaults() {
        // Only override a couple of fields; everything else should default.
        let yaml = "network:\n  view_distance: 12\nworld:\n  seed: 7\n";
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.network.view_distance, 12);
        assert_eq!(cfg.world.seed, 7);
        // Unset fields default.
        assert_eq!(cfg.network.bind, NetworkConfig::default().bind);
        assert_eq!(cfg.dashboard.port, DashboardConfig::default().port);
    }
}
