use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    Contain,
    Cover,
    Stretch,
}

#[derive(Debug, Default, Clone, clap::Args, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Playback {
    /// mpv hardware decoder, e.g. auto, vaapi, or no
    #[arg(long)]
    pub hwdec: Option<String>,
    /// Mute audio (true or false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub mute: Option<bool>,
    /// Audio volume from 0 to 100
    #[arg(long)]
    pub volume: Option<f64>,
    /// Playback speed, greater than zero
    #[arg(long)]
    pub speed: Option<f64>,
    /// Fit the video to the output
    #[arg(long, value_enum)]
    pub fit: Option<Fit>,
    /// Shuffle the playlist (true or false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub shuffle: Option<bool>,
    /// Seconds between playlist entries; zero disables the timer
    #[arg(long)]
    pub interval: Option<u64>,
    /// Repeat the playlist (true or false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub repeat: Option<bool>,
    /// Pause when the compositor hides the wallpaper (true or false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub auto_pause: Option<bool>,
    /// Switch at video EOF even when an interval is configured (true or false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub change_on_end: Option<bool>,
    /// Let the interval run while playback is paused (default: false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub change_while_paused: Option<bool>,
    /// Keep the original first entry first even when shuffling (true or false)
    #[arg(long, action = clap::ArgAction::Set)]
    pub start_first: Option<bool>,
}

impl Playback {
    pub fn merged(&self, overrides: &Self) -> Self {
        Self {
            hwdec: overrides.hwdec.clone().or_else(|| self.hwdec.clone()),
            mute: overrides.mute.or(self.mute),
            volume: overrides.volume.or(self.volume),
            speed: overrides.speed.or(self.speed),
            fit: overrides.fit.or(self.fit),
            shuffle: overrides.shuffle.or(self.shuffle),
            interval: overrides.interval.or(self.interval),
            repeat: overrides.repeat.or(self.repeat),
            auto_pause: overrides.auto_pause.or(self.auto_pause),
            change_on_end: overrides.change_on_end.or(self.change_on_end),
            change_while_paused: overrides.change_while_paused.or(self.change_while_paused),
            start_first: overrides.start_first.or(self.start_first),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(hwdec) = &self.hwdec {
            ensure!(
                !hwdec.is_empty()
                    && hwdec
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_,.".contains(&c)),
                "hwdec must be a decoder name or comma-separated names without whitespace or quoting"
            );
        }
        if let Some(volume) = self.volume {
            ensure!(
                volume.is_finite() && (0.0..=100.0).contains(&volume),
                "volume must be between 0 and 100"
            );
        }
        if let Some(speed) = self.speed {
            ensure!(
                speed.is_finite() && speed > 0.0,
                "speed must be finite and greater than zero"
            );
        }
        if let Some(interval) = self.interval {
            ensure!(
                interval <= i32::MAX as u64,
                "interval must not exceed {} seconds",
                i32::MAX
            );
        }
        Ok(())
    }

    pub fn options(&self, socket: &Path) -> Result<(String, Option<u64>, bool)> {
        self.validate()?;
        let interval = self.interval.filter(|seconds| *seconds != 0);
        let socket = socket
            .to_str()
            .context("mpv IPC socket requires a UTF-8 path")?;
        ensure!(
            !socket.is_empty()
                && !socket.contains(|c: char| c.is_whitespace()
                    || c.is_control()
                    || matches!(c, '\'' | '"' | '\\')),
            "mpv IPC socket path cannot contain whitespace, control characters, or quoting"
        );
        let (keepaspect, panscan) = match self.fit.unwrap_or(Fit::Contain) {
            Fit::Contain => ("yes", 0),
            Fit::Cover => ("yes", 1),
            Fit::Stretch => ("no", 0),
        };
        let yes_no = |value| if value { "yes" } else { "no" };
        let options = format!(
            "resume-playback=no save-position-on-quit=no audio={} mute={} volume={} speed={} hwdec={} keepaspect={} panscan={} input-ipc-server={}",
            if self.mute.unwrap_or(true) {
                "no"
            } else {
                "auto"
            },
            yes_no(self.mute.unwrap_or(true)),
            self.volume.unwrap_or(100.0),
            self.speed.unwrap_or(1.0),
            self.hwdec.as_deref().unwrap_or("auto"),
            keepaspect,
            panscan,
            socket,
        );
        Ok((options, interval, self.auto_pause.unwrap_or(false)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlaylist {
    pub items: Vec<PathBuf>,
    #[serde(default)]
    pub playback: Playback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Source {
    Local {
        playlist: String,
    },
    WallpaperEngine {
        playlist: String,
        steamroot: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub source: Source,
    #[serde(default)]
    pub playback: Playback,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub defaults: Playback,
    pub playlists: BTreeMap<String, NativePlaylist>,
    pub screens: BTreeMap<String, Assignment>,
    /// Per-field playback overrides layered over Wallpaper Engine settings.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub we_overrides: BTreeMap<String, Playback>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        self.defaults
            .validate()
            .context("invalid global playback defaults")?;
        for (name, playlist) in &self.playlists {
            ensure!(!name.trim().is_empty(), "playlist name cannot be empty");
            playlist
                .playback
                .validate()
                .with_context(|| format!("invalid playback for playlist {name}"))?;
        }
        for (name, playback) in &self.we_overrides {
            ensure!(!name.trim().is_empty(), "WE override name cannot be empty");
            playback
                .validate()
                .with_context(|| format!("invalid WE override for {name}"))?;
        }
        for (screen, assignment) in &self.screens {
            ensure!(!screen.trim().is_empty(), "output name cannot be empty");
            let playlist = match &assignment.source {
                Source::Local { playlist } => {
                    ensure!(
                        self.playlists.contains_key(playlist),
                        "output {screen} references unknown local playlist {playlist:?}"
                    );
                    playlist
                }
                Source::WallpaperEngine { playlist, .. } => playlist,
            };
            ensure!(
                !playlist.trim().is_empty(),
                "playlist name for output {screen} cannot be empty"
            );
            assignment
                .playback
                .validate()
                .with_context(|| format!("invalid playback for output {screen}"))?;
        }
        Ok(())
    }
}

pub fn default_path() -> Result<PathBuf> {
    let root = if let Some(root) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        PathBuf::from(root)
    } else {
        PathBuf::from(
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .context("HOME or XDG_CONFIG_HOME must be set")?,
        )
        .join(".config")
    };
    Ok(root.join("waywe/config.json"))
}

pub fn load(path: &Path) -> Result<Config> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let config: Config = serde_json::from_slice(&contents)
        .with_context(|| format!("invalid waywe config {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("invalid waywe config {}", path.display()))?;
    Ok(config)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn lock(path: &Path) -> Result<File> {
    fs::create_dir_all(parent(path)).with_context(|| {
        format!(
            "failed to create config directory {}",
            parent(path).display()
        )
    })?;
    let mut name = path
        .file_name()
        .context("config path must name a file")?
        .to_os_string();
    name.push(".lock");
    let lock_path = path.with_file_name(name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open config lock {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(file)
}

fn save_locked(path: &Path, config: &Config) -> Result<()> {
    config.validate()?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path
        .file_name()
        .context("config path must name a file")?
        .to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = path.with_file_name(name);
    let mut file = File::create_new(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut file, config)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to save {}", path.display()))
}

pub fn update(path: &Path, edit: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
    let _lock = lock(path)?;
    let mut config = load(path)?;
    edit(&mut config)?;
    save_locked(path, &config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let root = env::temp_dir().join(format!(
                "waywe-config-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assignment(playback: Playback) -> Assignment {
        Assignment {
            source: Source::Local {
                playlist: "Favorites".into(),
            },
            playback,
        }
    }

    #[test]
    fn merge_preserves_unspecified_values_and_honors_false_and_zero() {
        let base = Playback {
            hwdec: Some("vaapi".into()),
            mute: Some(true),
            volume: Some(60.0),
            speed: Some(2.0),
            fit: Some(Fit::Cover),
            shuffle: Some(true),
            interval: Some(30),
            repeat: Some(true),
            auto_pause: Some(true),
            change_on_end: Some(true),
            change_while_paused: Some(true),
            start_first: Some(true),
        };
        let merged = base.merged(&Playback {
            mute: Some(false),
            volume: Some(0.0),
            fit: Some(Fit::Stretch),
            shuffle: Some(false),
            interval: Some(0),
            repeat: Some(false),
            auto_pause: Some(false),
            ..Playback::default()
        });
        assert_eq!(merged.hwdec.as_deref(), Some("vaapi"));
        assert_eq!(merged.speed, Some(2.0));
        assert_eq!(merged.mute, Some(false));
        assert_eq!(merged.volume, Some(0.0));
        assert!(matches!(merged.fit, Some(Fit::Stretch)));
        assert_eq!(merged.shuffle, Some(false));
        assert_eq!(merged.interval, Some(0));
        assert_eq!(merged.repeat, Some(false));
        assert_eq!(merged.auto_pause, Some(false));
        assert_eq!(base.mute, Some(true));
        assert_eq!(base.interval, Some(30));
        let (options, interval, auto_pause) =
            merged.options(Path::new("/tmp/waywe/DP-1.sock")).unwrap();
        let values: BTreeMap<_, _> = options
            .split_whitespace()
            .map(|option| option.split_once('=').unwrap())
            .collect();
        assert_eq!(values["volume"], "0");
        assert_eq!(values["mute"], "no");
        assert_eq!(interval, None);
        assert!(!auto_pause);
        let cli = merged.merged(&Playback {
            mute: Some(true),
            interval: Some(5),
            ..Playback::default()
        });
        assert_eq!(cli.mute, Some(true));
        assert_eq!(cli.interval, Some(5));
        assert_eq!(cli.volume, Some(0.0));
        assert_eq!(cli.shuffle, Some(false));
    }

    #[test]
    fn validates_numeric_limits_and_decoder_tokens() {
        for volume in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
            assert!(
                Playback {
                    volume: Some(volume),
                    ..Playback::default()
                }
                .validate()
                .is_err()
            );
        }
        for speed in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert!(
                Playback {
                    speed: Some(speed),
                    ..Playback::default()
                }
                .validate()
                .is_err()
            );
        }
        for hwdec in ["", "auto mute=no", "auto\n", "auto;echo", "\"auto\""] {
            assert!(
                Playback {
                    hwdec: Some(hwdec.into()),
                    ..Playback::default()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            Playback {
                interval: Some(i32::MAX as u64 + 1),
                ..Playback::default()
            }
            .validate()
            .is_err()
        );
        for volume in [0.0, 100.0] {
            Playback {
                hwdec: Some("vaapi,vaapi-copy".into()),
                volume: Some(volume),
                speed: Some(0.5),
                interval: Some(i32::MAX as u64),
                ..Playback::default()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn persistence_roundtrips_and_refuses_invalid_replacements() {
        let root = TempRoot::new();
        let path = root.0.join("nested/config.json");
        assert!(load(&path).unwrap().screens.is_empty());
        let mut config = Config::default();
        config.playlists.insert(
            "Favorites".into(),
            NativePlaylist {
                items: vec![
                    PathBuf::from("videos/relative.mp4"),
                    PathBuf::from("/unavailable/video.webm"),
                ],
                playback: Playback {
                    shuffle: Some(true),
                    interval: Some(30),
                    ..Playback::default()
                },
            },
        );
        config.screens.insert(
            "DP-1".into(),
            assignment(Playback {
                mute: Some(false),
                fit: Some(Fit::Cover),
                interval: Some(0),
                ..Playback::default()
            }),
        );
        update(&path, |stored| {
            stored.playlists = config.playlists.clone();
            stored.screens = config.screens.clone();
            Ok(())
        })
        .unwrap();
        let loaded = load(&path).unwrap();
        let screen = &loaded.screens["DP-1"];
        assert!(matches!(
            &screen.source,
            Source::Local { playlist } if playlist == "Favorites"
        ));
        assert_eq!(
            loaded.playlists["Favorites"].items,
            [
                PathBuf::from("videos/relative.mp4"),
                PathBuf::from("/unavailable/video.webm"),
            ]
        );
        assert_eq!(loaded.playlists["Favorites"].playback.shuffle, Some(true));
        assert_eq!(loaded.playlists["Favorites"].playback.interval, Some(30));
        assert_eq!(screen.playback.mute, Some(false));
        assert!(matches!(screen.playback.fit, Some(Fit::Cover)));
        assert_eq!(screen.playback.interval, Some(0));
        update(&path, |config| {
            config.screens.insert(
                "HDMI-A-1".into(),
                Assignment {
                    source: Source::WallpaperEngine {
                        playlist: "External".into(),
                        steamroot: PathBuf::from("/steam"),
                    },
                    playback: Playback::default(),
                },
            );
            Ok(())
        })
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.screens.len(), 2);
        assert!(matches!(
            &loaded.screens["HDMI-A-1"].source,
            Source::WallpaperEngine { playlist, steamroot }
                if playlist == "External" && steamroot == Path::new("/steam")
        ));
        let contents = fs::read(&path).unwrap();
        config.screens.get_mut("DP-1").unwrap().playback.speed = Some(0.0);
        assert!(
            update(&path, |stored| {
                stored.screens = config.screens.clone();
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), contents);
        assert!(
            update(&path, |config| {
                config.screens.clear();
                anyhow::bail!("cancel edit")
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), contents);
        fs::write(&path, b"{broken").unwrap();
        assert!(load(&path).is_err());
        assert!(update(&path, |_| Ok(())).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
    }

    #[test]
    fn local_references_and_playlist_playback_are_validated_before_saving() {
        let root = TempRoot::new();
        let path = root.0.join("config.json");
        update(&path, |config| {
            config.playlists.insert(
                "Favorites".into(),
                NativePlaylist {
                    items: vec![PathBuf::from("missing-but-portable.mp4")],
                    playback: Playback::default(),
                },
            );
            config
                .screens
                .insert("DP-1".into(), assignment(Playback::default()));
            Ok(())
        })
        .unwrap();
        let contents = fs::read(&path).unwrap();
        assert!(
            update(&path, |config| {
                config.playlists.remove("Favorites");
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), contents);
        assert!(
            update(&path, |config| {
                config
                    .playlists
                    .get_mut("Favorites")
                    .unwrap()
                    .playback
                    .speed = Some(0.0);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), contents);
        fs::write(
            &path,
            br#"{"screens":{"DP-1":{"source":{"type":"local","playlist":"Missing"}}}}"#,
        )
        .unwrap();
        let invalid = fs::read(&path).unwrap();
        assert!(load(&path).is_err());
        assert!(update(&path, |_| Ok(())).is_err());
        assert_eq!(fs::read(&path).unwrap(), invalid);
    }

    #[test]
    fn canonical_schema_defaults_maps_and_rejects_unknown_fields_and_empty_names() {
        for json in [
            r#"{}"#,
            r#"{"playlists":{}}"#,
            r#"{"screens":{}}"#,
            r#"{"playlists":{"Favorites":{"items":[]}}}"#,
        ] {
            let config: Config = serde_json::from_str(json).unwrap();
            config.validate().unwrap();
        }
        for json in [
            r#"{"unknown":{}}"#,
            r#"{"playlists":{"Favorites":{"items":[],"unknown":true}}}"#,
            r#"{"screens":{"DP-1":{"source":{"type":"local","playlist":"Favorites","unknown":true}}}}"#,
            r#"{"screens":{"DP-1":{"playlist":"Favorites","steamroot":"/steam"}}}"#,
            r#"{"screens":{"DP-1":{"source":{"type":"wallpaper-engine","playlist":"Favorites","steamroot":"/steam"},"unknown":true}}}"#,
        ] {
            assert!(serde_json::from_str::<Config>(json).is_err(), "{json}");
        }
        for json in [
            r#"{"playlists":{" ":{"items":[]}}}"#,
            r#"{"screens":{"":{"source":{"type":"wallpaper-engine","playlist":"Favorites","steamroot":"/steam"}}}}"#,
            r#"{"screens":{"DP-1":{"source":{"type":"wallpaper-engine","playlist":" ","steamroot":"/steam"}}}}"#,
        ] {
            let config: Config = serde_json::from_str(json).unwrap();
            assert!(config.validate().is_err(), "{json}");
        }
    }
}
