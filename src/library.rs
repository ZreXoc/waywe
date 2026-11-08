use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::Playback;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug)]
pub struct Playlist {
    pub name: String,
    pub items: Vec<PathBuf>,
    pub playback: Playback,
    pub skipped: usize,
}

#[derive(Debug, Deserialize)]
struct Settings {
    delay: f64,
    mode: String,
    order: String,
    #[serde(default)]
    videosequence: Option<bool>,
    #[serde(default)]
    updateonpause: Option<bool>,
    #[serde(default)]
    beginfirst: Option<bool>,
}

impl Settings {
    fn into_playback(self) -> Result<Playback> {
        let interval = if self.mode == "timer" {
            let seconds = self.delay * 60.0;
            anyhow::ensure!(
                seconds.is_finite() && seconds >= 1.0 && seconds <= i32::MAX as f64,
                "invalid Wallpaper Engine playlist timer"
            );
            seconds.round() as u64
        } else {
            0
        };
        Ok(Playback {
            shuffle: Some(self.order == "random"),
            interval: Some(interval),
            change_on_end: self.videosequence,
            change_while_paused: self.updateonpause,
            start_first: self.beginfirst,
            ..Playback::default()
        })
    }
}

#[derive(Debug)]
pub struct Video {
    pub title: String,
    pub path: PathBuf,
}

#[derive(Deserialize)]
struct Config {
    steamuser: SteamUser,
}

#[derive(Deserialize)]
struct SteamUser {
    general: General,
}

#[derive(Deserialize)]
struct General {
    playlists: Vec<RawPlaylist>,
}

#[derive(Deserialize)]
struct RawPlaylist {
    name: String,
    items: Vec<String>,
    settings: Settings,
}

#[derive(Deserialize)]
struct Project {
    title: String,
    file: Option<String>,
}

fn engine_dir(steam_root: &Path) -> PathBuf {
    steam_root.join("steamapps/common/wallpaper_engine")
}

fn resolve_path(steam_root: &Path, base: &Path, value: &str) -> Option<PathBuf> {
    let normalized;
    let value = if value.contains('\\') {
        normalized = value.replace('\\', "/");
        normalized.as_str()
    } else {
        value
    };
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if bytes.get(2) != Some(&b'/') {
            return None;
        }
        if bytes[0].eq_ignore_ascii_case(&b'z') {
            return Some(PathBuf::from(&value[2..]));
        }
        let mut components = Path::new(&value[3..]).components();
        if components.any(|component| component.as_os_str().eq_ignore_ascii_case("steamapps")) {
            return Some(steam_root.join("steamapps").join(components.as_path()));
        }
        // Only recognizable Steam paths can be mapped without Wine's dosdevices.
        return None;
    }
    Some(base.join(value))
}

pub(crate) fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["mp4", "webm", "mkv", "mov", "m4v", "avi"]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

pub fn load_playlists(
    steam_root: &Path,
    overrides: &BTreeMap<String, Playback>,
) -> Result<Vec<Playlist>> {
    let engine = engine_dir(steam_root);
    let config_path = engine.join("config.json");
    let contents = fs::read(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let config: Config = serde_json::from_slice(&contents)
        .with_context(|| format!("Invalid Wallpaper Engine config {}", config_path.display()))?;

    config
        .steamuser
        .general
        .playlists
        .into_iter()
        .map(|playlist| {
            let mut playback = playlist.settings.into_playback().with_context(|| {
                format!(
                    "Invalid settings for Wallpaper Engine playlist {}",
                    playlist.name
                )
            })?;
            if let Some(overrides) = overrides.get(&playlist.name) {
                playback = playback.merged(overrides);
            }
            let original_count = playlist.items.len();
            let items: Vec<PathBuf> = playlist
                .items
                .into_iter()
                .filter_map(|item| resolve_path(steam_root, &engine, &item))
                .filter(|path| is_video(path) && path.is_file())
                .collect();
            Ok(Playlist {
                name: playlist.name,
                skipped: original_count - items.len(),
                items,
                playback,
            })
        })
        .collect()
}

pub fn discover_videos(steam_root: &Path) -> Result<Vec<Video>> {
    let mut videos = Vec::new();
    for directory in [
        steam_root.join("steamapps/workshop/content/431960"),
        engine_dir(steam_root).join("projects/myprojects"),
    ] {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to list {}", directory.display()));
            }
        };
        for entry in entries {
            let entry = entry
                .with_context(|| format!("Failed to read entry in {}", directory.display()))?;
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }
            let metadata_path = project_dir.join("project.json");
            let contents = match fs::read(&metadata_path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to read {}", metadata_path.display()));
                }
            };
            let project: Project = serde_json::from_slice(&contents)
                .with_context(|| format!("Invalid project metadata {}", metadata_path.display()))?;
            if let Some(file) = project.file {
                if let Some(path) = resolve_path(steam_root, &project_dir, &file) {
                    if is_video(&path) && path.is_file() {
                        videos.push(Video {
                            title: project.title,
                            path,
                        });
                    }
                }
            }
        }
    }
    videos.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.path.cmp(&b.path)));
    Ok(videos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "waywe-library-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_steam_drives_wine_absolute_and_engine_relative_paths() {
        let steam = Path::new("/selected/Steam");
        let base = engine_dir(steam);
        for value in [
            "S:/steamapps/workshop/content/431960/3054635239/2560x1440.mp4",
            r"s:\steamapps\workshop\content\431960\3054635239\2560x1440.mp4",
            r"C:\Program Files (x86)\Steam\SteamApps\workshop\content\431960\3054635239\2560x1440.mp4",
        ] {
            assert_eq!(
                resolve_path(steam, &base, value),
                Some(steam.join("steamapps/workshop/content/431960/3054635239/2560x1440.mp4"))
            );
        }
        for value in ["Z:/home/a/video.mp4", r"z:\home\a\video.mp4"] {
            assert_eq!(
                resolve_path(steam, &base, value),
                Some(PathBuf::from("/home/a/video.mp4"))
            );
        }
        for value in ["projects/a.mp4", r"projects\a.mp4"] {
            assert_eq!(
                resolve_path(steam, &base, value),
                Some(base.join("projects/a.mp4"))
            );
        }
        assert_eq!(
            resolve_path(steam, &base, "/video.mp4"),
            Some(PathBuf::from("/video.mp4"))
        );
        assert_eq!(resolve_path(steam, &base, r"C:\video.mp4"), None);
        assert_eq!(resolve_path(steam, &base, "Z:video.mp4"), None);
        assert_eq!(
            resolve_path(steam, &base, "S:/notsteamapps/video.mp4"),
            None
        );
    }

    #[test]
    fn filters_invalid_items_without_truncating_or_reordering() {
        let root = TempRoot::new();
        let engine = engine_dir(&root.0);
        fs::create_dir_all(&engine).unwrap();
        for name in [
            "a.mp4",
            "b.WEBM",
            "c.mkv",
            "d.mov",
            "e.m4v",
            "f.avi",
            "scene.pkg",
        ] {
            fs::write(engine.join(name), b"").unwrap();
        }
        let config = serde_json::json!({"steamuser": {"general": {"playlists": [{
            "name": "All videos",
            "items": ["a.mp4", "scene.pkg", "b.WEBM", "missing.mp4", "c.mkv", "d.mov", "e.m4v", "f.avi", "a.mp4"],
            "settings": {"delay": 30, "mode": "timer", "order": "random"}
        }]}}});
        let contents = serde_json::to_vec(&config).unwrap();
        fs::write(engine.join("config.json"), &contents).unwrap();
        let playlists = load_playlists(&root.0, &BTreeMap::new()).unwrap();
        assert_eq!(playlists.len(), 1);
        let playlist = &playlists[0];
        assert_eq!(playlist.name, "All videos");
        assert_eq!(
            playlist.items,
            [
                "a.mp4", "b.WEBM", "c.mkv", "d.mov", "e.m4v", "f.avi", "a.mp4"
            ]
            .map(|name| engine.join(name))
        );
        assert_eq!(playlist.skipped, 2);
        assert_eq!(playlist.playback.interval, Some(1800));
        assert_eq!(playlist.playback.shuffle, Some(true));
        assert_eq!(fs::read(engine.join("config.json")).unwrap(), contents);
    }

    #[test]
    fn maps_arcaea_steam_paths_and_skips_missing_files_in_both_consumers() {
        let root = TempRoot::new();
        let engine = engine_dir(&root.0);
        let workshop = root.0.join("steamapps/workshop/content/431960");
        let project = workshop.join("3054635239");
        let missing = workshop.join("missing");
        fs::create_dir_all(&engine).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&missing).unwrap();
        let clip = project.join("2560x1440.mp4");
        fs::write(&clip, b"").unwrap();
        let existing_path = "S:/steamapps/workshop/content/431960/3054635239/2560x1440.mp4";
        let missing_path = r"s:\steamapps\workshop\content\431960\missing\missing.mp4";
        for (directory, title, file) in [
            (&project, "Arcaea", existing_path),
            (&missing, "Missing", missing_path),
        ] {
            fs::write(
                directory.join("project.json"),
                serde_json::to_vec(&serde_json::json!({"title": title, "file": file})).unwrap(),
            )
            .unwrap();
        }
        fs::write(
            engine.join("config.json"),
            serde_json::to_vec(
                &serde_json::json!({"steamuser": {"general": {"playlists": [{
                    "name": "arcaea0",
                    "items": [existing_path, missing_path, existing_path],
                    "settings": {"delay": 30, "mode": "timer", "order": "random"}
                }]}}}),
            )
            .unwrap(),
        )
        .unwrap();
        let playlists = load_playlists(&root.0, &BTreeMap::new()).unwrap();
        assert_eq!(playlists[0].items, vec![clip.clone(), clip.clone()]);
        assert_eq!(playlists[0].skipped, 1);
        let videos = discover_videos(&root.0).unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].title, "Arcaea");
        assert_eq!(videos[0].path, clip);
    }

    #[test]
    fn malformed_config_reports_path_instead_of_empty_playlists() {
        let root = TempRoot::new();
        let engine = engine_dir(&root.0);
        fs::create_dir_all(&engine).unwrap();
        let path = engine.join("config.json");
        for contents in [
            "{",
            "{}",
            r#"{"steamuser":{"general":{"playlists":"invalid"}}}"#,
        ] {
            fs::write(&path, contents).unwrap();
            let message = load_playlists(&root.0, &BTreeMap::new())
                .unwrap_err()
                .to_string();
            assert!(message.contains("Invalid Wallpaper Engine config"));
            assert!(message.contains(path.to_str().unwrap()));
        }
    }

    #[test]
    fn discovers_only_direct_projects_with_existing_video_files() {
        let root = TempRoot::new();
        assert!(discover_videos(&root.0).unwrap().is_empty());
        let workshop = root.0.join("steamapps/workshop/content/431960");
        let local = engine_dir(&root.0).join("projects/myprojects");
        for (directory, title, file, exists) in [
            (workshop.join("1"), "Workshop", "clip.mp4", true),
            (local.join("local"), "Local", "movie.webm", true),
            (workshop.join("scene"), "Scene", "scene.pkg", true),
            (workshop.join("missing"), "Missing", "missing.mp4", false),
            (
                workshop.join("container/nested"),
                "Nested",
                "nested.mp4",
                true,
            ),
        ] {
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("project.json"),
                serde_json::to_vec(&serde_json::json!({"title": title, "file": file})).unwrap(),
            )
            .unwrap();
            if exists {
                fs::write(directory.join(file), b"").unwrap();
            }
        }
        let preset = workshop.join("preset");
        fs::create_dir_all(&preset).unwrap();
        fs::write(
            preset.join("project.json"),
            br#"{"title":"Preset","preset":{}}"#,
        )
        .unwrap();
        fs::write(workshop.join("loose.mp4"), b"").unwrap();
        let videos = discover_videos(&root.0).unwrap();
        assert_eq!(videos.len(), 2);
        assert_eq!(videos[0].title, "Local");
        assert_eq!(videos[0].path, local.join("local/movie.webm"));
        assert_eq!(videos[1].title, "Workshop");
        assert_eq!(videos[1].path, workshop.join("1/clip.mp4"));
    }

    #[test]
    fn we_overrides_replace_only_specified_fields_and_follow_source_changes() {
        let root = TempRoot::new();
        let engine = engine_dir(&root.0);
        fs::create_dir_all(&engine).unwrap();
        fs::write(engine.join("a.mp4"), b"").unwrap();
        fs::write(engine.join("b.mp4"), b"").unwrap();
        let mut source = serde_json::json!({"steamuser":{"general":{"playlists":[{
            "name":"Linked", "items":["a.mp4"],
            "settings":{"delay":30,"mode":"timer","order":"random","videosequence":true}
        }]}}});
        let path = engine.join("config.json");
        fs::write(&path, serde_json::to_vec(&source).unwrap()).unwrap();
        let overrides = BTreeMap::from([(
            "Linked".into(),
            Playback {
                interval: Some(0),
                shuffle: Some(false),
                ..Playback::default()
            },
        )]);
        let before = load_playlists(&root.0, &overrides).unwrap().remove(0);
        assert_eq!(before.items, vec![engine.join("a.mp4")]);
        assert_eq!(before.playback.interval, Some(0));
        assert_eq!(before.playback.change_on_end, Some(true));
        assert_eq!(before.playback.shuffle, Some(false));
        source["steamuser"]["general"]["playlists"][0]["items"] =
            serde_json::json!(["b.mp4", "a.mp4", "b.mp4"]);
        source["steamuser"]["general"]["playlists"][0]["settings"] = serde_json::json!({"delay":10,"mode":"timer","order":"random","videosequence":false,"beginfirst":true});
        fs::write(&path, serde_json::to_vec(&source).unwrap()).unwrap();
        let after = load_playlists(&root.0, &overrides).unwrap().remove(0);
        assert_eq!(
            after.items,
            vec![
                engine.join("b.mp4"),
                engine.join("a.mp4"),
                engine.join("b.mp4")
            ]
        );
        assert_eq!(after.playback.interval, Some(0));
        assert_eq!(after.playback.shuffle, Some(false));
        assert_eq!(after.playback.change_on_end, Some(false));
        assert_eq!(after.playback.start_first, Some(true));
        let empty = BTreeMap::from([("Linked".into(), Playback::default())]);
        let inherited = load_playlists(&root.0, &empty).unwrap().remove(0);
        assert_eq!(inherited.playback.interval, Some(600));
        assert_eq!(inherited.playback.shuffle, Some(true));
        assert_eq!(inherited.playback.change_on_end, Some(false));
    }
}
