use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, ensure};

use crate::{config, library};

#[derive(Debug, clap::Args)]
#[command(group(
    clap::ArgGroup::new("import-selection")
        .required(true)
        .multiple(false)
        .args(["playlist", "all"])
))]
pub struct Options {
    /// Wallpaper Engine reference, e.g. WE:playlist1
    #[arg(value_parser = parse_we_reference)]
    pub playlist: Option<String>,
    /// Import every playable Wallpaper Engine playlist
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Name of the imported native playlist (only with a single reference)
    #[arg(long, requires = "playlist", conflicts_with = "all")]
    pub name: Option<String>,
    /// Copy videos into waywe's own media library
    #[arg(long = "copy")]
    pub copy_videos: bool,
    /// Parent directory for copied media
    #[arg(long, requires = "copy_videos")]
    pub media_dir: Option<PathBuf>,
    /// Replace selected native playlists that already exist
    #[arg(long)]
    pub replace: bool,
}

fn parse_we_reference(value: &str) -> std::result::Result<String, String> {
    let reference: crate::cli::PlaylistRef = value.parse()?;
    if reference.source != crate::cli::SourceKind::We {
        return Err("import requires a Wallpaper Engine reference: WE:NAME".into());
    }
    Ok(reference.name)
}

#[derive(Debug)]
pub struct Report {
    pub playlists: Vec<String>,
    pub copied_files: usize,
    pub skipped_items: usize,
    pub skipped_playlists: usize,
    /// The newly created import directory, not the media library parent.
    pub media_dir: Option<PathBuf>,
}

pub fn run(steam: &Path, config_path: &Path, options: &Options) -> Result<Report> {
    ensure!(
        options.playlist.is_some() != options.all,
        "select exactly one WE:NAME reference or --all"
    );
    ensure!(
        options.name.is_none() || options.playlist.is_some(),
        "--name requires a single WE:NAME reference"
    );
    ensure!(
        options.media_dir.is_none() || options.copy_videos,
        "--media-dir requires --copy"
    );

    let settings = config::load(config_path)?;
    let playlists = library::load_playlists(steam, &settings.we_overrides)?;
    let mut selected = if let Some(name) = &options.playlist {
        let matches: Vec<_> = playlists
            .into_iter()
            .filter(|playlist| playlist.name == *name)
            .collect();
        ensure!(
            !matches.is_empty(),
            "Wallpaper Engine playlist {name:?} not found"
        );
        ensure!(
            matches.len() == 1,
            "Wallpaper Engine playlist {name:?} is ambiguous"
        );
        ensure!(
            !matches[0].items.is_empty(),
            "Wallpaper Engine playlist {name:?} has no playable videos"
        );
        matches
    } else {
        playlists
    };
    if let Some(name) = &options.name {
        selected[0].name.clone_from(name);
    }
    let skipped_items = selected.iter().map(|playlist| playlist.skipped).sum();
    let skipped_playlists = selected
        .iter()
        .filter(|playlist| playlist.items.is_empty())
        .count();
    selected.retain(|playlist| !playlist.items.is_empty());
    ensure!(
        !selected.is_empty(),
        "no playable Wallpaper Engine playlists to import"
    );
    let mut names = BTreeSet::new();
    for playlist in &selected {
        ensure!(
            !playlist.name.trim().is_empty(),
            "imported playlist names must not be empty"
        );
        ensure!(
            names.insert(playlist.name.as_str()),
            "duplicate output playlist name {:?}",
            playlist.name
        );
        playlist.playback.validate()?;
    }
    drop(names);
    let mut report = Report {
        playlists: selected
            .iter()
            .map(|playlist| playlist.name.clone())
            .collect(),
        copied_files: 0,
        skipped_items,
        skipped_playlists,
        media_dir: None,
    };
    let mut created_directory = None;
    let result = config::update(config_path, |config| {
        for playlist in &selected {
            ensure!(
                options.replace || !config.playlists.contains_key(&playlist.name),
                "native playlist {:?} already exists; use --replace to replace it",
                playlist.name
            );
        }
        for playlist in &mut selected {
            for item in &mut playlist.items {
                *item = fs::canonicalize(&*item)
                    .with_context(|| format!("failed to resolve video {}", item.display()))?;
            }
        }
        if options.copy_videos {
            let media_root = match &options.media_dir {
                Some(path) => path.clone(),
                None => default_media_dir()?,
            };
            let directory = create_import_directory(&media_root)?;
            created_directory = Some(directory.clone());
            report.media_dir = Some(directory.clone());
            let mut copied = BTreeMap::<PathBuf, PathBuf>::new();
            for playlist in &mut selected {
                for item in &mut playlist.items {
                    if let Some(destination) = copied.get(item) {
                        item.clone_from(destination);
                        continue;
                    }
                    let mut name = OsString::from(format!("{:06}-", report.copied_files + 1));
                    name.push(item.file_name().context("video path must name a file")?);
                    let destination = directory.join(name);
                    fs::copy(&*item, &destination).with_context(|| {
                        format!(
                            "failed to copy {} to {}",
                            item.display(),
                            destination.display()
                        )
                    })?;
                    let source = std::mem::replace(item, destination.clone());
                    copied.insert(source, destination);
                    report.copied_files += 1;
                }
            }
        }
        for playlist in selected {
            config.playlists.insert(
                playlist.name,
                config::NativePlaylist {
                    items: playlist.items,
                    playback: playlist.playback,
                },
            );
        }
        Ok(())
    });
    if let Err(error) = result {
        if let Some(directory) = created_directory {
            fs::remove_dir_all(&directory).with_context(|| {
                format!(
                    "{error:#}; also failed to roll back import directory {}",
                    directory.display()
                )
            })?;
        }
        return Err(error);
    }
    Ok(report)
}

fn default_media_dir() -> Result<PathBuf> {
    let root = if let Some(root) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(root)
    } else {
        PathBuf::from(
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .context("neither XDG_DATA_HOME nor HOME is set; use --media-dir")?,
        )
        .join(".local/share")
    };
    Ok(root.join("waywe/media"))
}

fn create_import_directory(root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create media directory {}", root.display()))?;
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve media directory {}", root.display()))?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let directory = root.join(format!(
            "import-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", directory.display()));
            }
        }
    }
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
                "waywe-import-{}-{}-{}",
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

        fn engine(&self) -> PathBuf {
            self.0.join("steamapps/common/wallpaper_engine")
        }

        fn video(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.engine().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }

        fn playlists(&self, playlists: &[(&str, &[&str])]) -> Vec<u8> {
            fs::create_dir_all(self.engine()).unwrap();
            let playlists: Vec<_> = playlists
                .iter()
                .map(|(name, items)| {
                    serde_json::json!({
                        "name": name,
                        "items": items,
                        "settings": {"delay": 30, "mode": "timer", "order": "random"}
                    })
                })
                .collect();
            let bytes = serde_json::to_vec(&serde_json::json!({
                "steamuser": {"general": {"playlists": playlists}}
            }))
            .unwrap();
            fs::write(self.engine().join("config.json"), &bytes).unwrap();
            bytes
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options(name: &str) -> Options {
        Options {
            playlist: Some(name.into()),
            all: false,
            name: None,
            copy_videos: false,
            media_dir: None,
            replace: false,
        }
    }

    #[test]
    fn reference_import_preserves_order_duplicates_settings_and_engine_bytes() {
        let root = TempRoot::new();
        let first = root.video("one.mp4", b"first video");
        let second = root.video("two.webm", b"second video");
        let engine_bytes = root.playlists(&[(
            "Favorites",
            &["two.webm", "missing.mp4", "one.mp4", "two.webm"],
        )]);
        let config_path = root.0.join("waywe.json");
        let mut options = options("Favorites");
        options.name = Some("Native".into());
        let report = run(&root.0, &config_path, &options).unwrap();
        assert_eq!(report.playlists, ["Native"]);
        assert_eq!(report.copied_files, 0);
        assert_eq!(report.skipped_items, 1);
        assert_eq!(report.skipped_playlists, 0);
        assert!(report.media_dir.is_none());
        let config = config::load(&config_path).unwrap();
        let playlist = &config.playlists["Native"];
        assert_eq!(playlist.items, [second.clone(), first.clone(), second]);
        assert!(playlist.items.iter().all(|path| path.is_absolute()));
        assert_eq!(playlist.playback.shuffle, Some(true));
        assert_eq!(playlist.playback.interval, Some(30 * 60));
        assert_eq!(fs::read(first).unwrap(), b"first video");
        assert_eq!(
            fs::read(root.engine().join("config.json")).unwrap(),
            engine_bytes
        );
        assert!(!root.0.join("media").exists());
    }

    #[test]
    fn copied_import_deduplicates_shared_sources_and_survives_original_removal() {
        let root = TempRoot::new();
        let first = root.video("a/clip.mp4", b"first video");
        let second = root.video("b/clip.mp4", b"different video");
        let engine_bytes = root.playlists(&[
            ("First", &["a/clip.mp4", "b/clip.mp4", "a/clip.mp4"]),
            ("Second", &["a/../a/clip.mp4"]),
        ]);
        let config_path = root.0.join("waywe.json");
        let mut options = options("First");
        options.playlist = None;
        options.all = true;
        options.copy_videos = true;
        options.media_dir = Some(root.0.join("media"));
        let report = run(&root.0, &config_path, &options).unwrap();
        assert_eq!(report.playlists, ["First", "Second"]);
        assert_eq!(report.copied_files, 2);
        let directory = report.media_dir.unwrap();
        assert!(directory.is_absolute());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        let config = config::load(&config_path).unwrap();
        let items = &config.playlists["First"].items;
        assert_eq!(items[0], items[2]);
        assert_eq!(items[0], config.playlists["Second"].items[0]);
        assert_ne!(items[0], items[1]);
        assert!(items.iter().all(|path| path.starts_with(&directory)));
        assert_eq!(items[0].file_name().unwrap(), "000001-clip.mp4");
        assert_eq!(items[1].file_name().unwrap(), "000002-clip.mp4");
        assert_eq!(fs::read(&first).unwrap(), b"first video");
        assert_eq!(fs::read(&second).unwrap(), b"different video");
        assert_eq!(
            fs::read(root.engine().join("config.json")).unwrap(),
            engine_bytes
        );
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
        assert_eq!(fs::read(&items[0]).unwrap(), b"first video");
        assert_eq!(fs::read(&items[1]).unwrap(), b"different video");
    }

    fn existing_config(path: &Path) -> Vec<u8> {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "playlists": {
                "Favorites": {"items": ["/old.mp4"]},
                "Keep": {"items": ["/keep.mp4"]}
            },
            "screens": {
                "DP-1": {
                    "source": {"type": "local", "playlist": "Favorites"},
                    "playback": {"mute": false}
                }
            }
        }))
        .unwrap();
        fs::write(path, &bytes).unwrap();
        bytes
    }

    #[test]
    fn conflict_does_not_copy_and_replace_preserves_other_playlists_and_screens() {
        let root = TempRoot::new();
        let source = root.video("clip.mp4", b"video");
        root.playlists(&[("Favorites", &["clip.mp4"])]);
        let path = root.0.join("waywe.json");
        let before = existing_config(&path);
        let before_config = serde_json::to_value(config::load(&path).unwrap()).unwrap();
        let mut options = options("Favorites");
        options.copy_videos = true;
        options.media_dir = Some(root.0.join("media"));
        assert!(
            run(&root.0, &path, &options)
                .unwrap_err()
                .to_string()
                .contains("--replace")
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!root.0.join("media").exists());
        options.replace = true;
        let report = run(&root.0, &path, &options).unwrap();
        assert_eq!(report.copied_files, 1);
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["screens"], before_config["screens"]);
        assert_eq!(
            after["playlists"]["Keep"]["items"],
            before_config["playlists"]["Keep"]["items"]
        );
        assert_eq!(fs::read(source).unwrap(), b"video");
    }

    #[test]
    fn failed_copy_removes_partial_output_and_preserves_existing_config_and_media() {
        let root = TempRoot::new();
        root.video("first.mp4", b"first video");
        // A valid source basename that exceeds Linux NAME_MAX once prefixed for the copy.
        let long_name = format!("{}.mp4", "v".repeat(246));
        root.video(&long_name, b"second video");
        root.playlists(&[("Favorites", &["first.mp4", long_name.as_str()])]);
        let path = root.0.join("waywe.json");
        let before = existing_config(&path);
        let media = root.0.join("media");
        fs::create_dir(&media).unwrap();
        fs::write(media.join("keep.mp4"), b"keep").unwrap();
        let mut options = options("Favorites");
        options.replace = true;
        options.copy_videos = true;
        options.media_dir = Some(media.clone());
        let error = run(&root.0, &path, &options).unwrap_err();
        assert!(error.to_string().contains("failed to copy"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(&media).unwrap().count(), 1);
        assert_eq!(fs::read(media.join("keep.mp4")).unwrap(), b"keep");
        assert_eq!(
            fs::read(root.engine().join("first.mp4")).unwrap(),
            b"first video"
        );
        assert_eq!(
            fs::read(root.engine().join(long_name)).unwrap(),
            b"second video"
        );
    }

    #[test]
    fn failed_atomic_save_rolls_back_only_new_import_directory() {
        let root = TempRoot::new();
        root.video("clip.mp4", b"video");
        root.playlists(&[("Favorites", &["clip.mp4"])]);
        // On Linux the config and .lock fit NAME_MAX, but the atomic-save suffix does not.
        let path = root.0.join("c".repeat(250));
        let before = existing_config(&path);
        let media = root.0.join("media");
        let preexisting = media.join("import-existing");
        fs::create_dir_all(&preexisting).unwrap();
        fs::write(preexisting.join("keep.mp4"), b"keep").unwrap();
        let mut options = options("Favorites");
        options.replace = true;
        options.copy_videos = true;
        options.media_dir = Some(media.clone());
        let error = run(&root.0, &path, &options).unwrap_err();
        assert!(error.to_string().contains("failed to create"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(&media).unwrap().count(), 1);
        assert_eq!(fs::read(preexisting.join("keep.mp4")).unwrap(), b"keep");
        assert_eq!(fs::read(root.engine().join("clip.mp4")).unwrap(), b"video");
    }

    #[test]
    fn invalid_existing_config_is_not_overwritten_or_copied() {
        let root = TempRoot::new();
        root.video("clip.mp4", b"video");
        root.playlists(&[("Favorites", &["clip.mp4"])]);
        let path = root.0.join("waywe.json");
        fs::write(&path, b"{ invalid").unwrap();
        let mut options = options("Favorites");
        options.copy_videos = true;
        options.media_dir = Some(root.0.join("media"));
        assert!(run(&root.0, &path, &options).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{ invalid");
        assert!(!root.0.join("media").exists());
    }

    #[test]
    fn all_skips_empty_playlists_but_explicit_empty_and_duplicate_names_fail() {
        let root = TempRoot::new();
        root.video("clip.mp4", b"video");
        root.playlists(&[("Empty", &["missing.mp4"]), ("Good", &["clip.mp4"])]);
        let path = root.0.join("waywe.json");
        assert!(run(&root.0, &path, &options("Empty")).is_err());
        let mut all = options("Good");
        all.playlist = None;
        all.all = true;
        let report = run(&root.0, &path, &all).unwrap();
        assert_eq!(report.playlists, ["Good"]);
        assert_eq!(report.skipped_playlists, 1);
        assert_eq!(report.skipped_items, 1);
        root.playlists(&[("Same", &["clip.mp4"]), ("Same", &["clip.mp4"])]);
        let before = fs::read(&path).unwrap();
        assert!(
            run(&root.0, &path, &all)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        assert!(
            run(&root.0, &path, &options("Same"))
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        root.playlists(&[("Empty", &["missing.mp4"])]);
        assert!(run(&root.0, &path, &all).is_err());
    }

    #[test]
    fn clap_and_runtime_require_unambiguous_selection_and_dependent_options() {
        use clap::Parser;

        #[derive(Debug, Parser)]
        struct Cli {
            #[command(flatten)]
            options: Options,
        }
        for arguments in [
            vec!["import"],
            vec!["import", "--all", "we:Favorites"],
            vec!["import", "--all", "--name", "Native"],
            vec!["import", "--all", "--media-dir", "/tmp/media"],
            vec!["import", "Favorites"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert!(Cli::try_parse_from(["import", "we:Favorites", "--name", "Native"]).is_ok());
        assert!(
            Cli::try_parse_from(["import", "--all", "--copy", "--media-dir", "/tmp/media"]).is_ok()
        );
        let root = TempRoot::new();
        let path = root.0.join("waywe.json");
        let mut invalid = options("Favorites");
        invalid.all = true;
        assert!(run(&root.0, &path, &invalid).is_err());
        invalid.playlist = None;
        invalid.all = false;
        assert!(run(&root.0, &path, &invalid).is_err());
        invalid.all = true;
        invalid.name = Some("Native".into());
        assert!(run(&root.0, &path, &invalid).is_err());
        invalid.name = None;
        invalid.media_dir = Some(root.0.join("media"));
        assert!(run(&root.0, &path, &invalid).is_err());
        assert!(!path.exists());
    }
}
