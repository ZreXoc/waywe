mod cli;
mod config;
mod import;
mod library;
mod manager;
mod scheduler;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use cli::{Action, Cli, ListingSource, PlaylistAction, ScreenAction, SourceKind, WallpaperAction};
use config::{Assignment, NativePlaylist, Playback, Source};
use library::Playlist;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde::ser::{SerializeMap, Serializer as _};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = match cli.command {
        Action::Session { spec } => return manager::run(&spec),
        command => command,
    };
    let config_path = config_path(cli.config)?;
    let steam_override = cli.steam_dir;
    let steam = steam_override.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/Steam")
    });
    match command {
        Action::Defaults { command } => {
            if let Some(cli::DefaultsAction::Set { playback }) = command {
                playback.validate()?;
                config::update(&config_path, |settings| {
                    settings.defaults = settings.defaults.merged(&playback);
                    Ok(())
                })?;
                println!("Saved global defaults; use apply to restart saved bindings");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&playback_json(
                        &config::load(&config_path)?.defaults
                    )?)?
                );
            }
        }
        Action::Playlist {
            command,
            options,
            videos,
        } => manage_playlists(
            command.unwrap_or(PlaylistAction::List { options, videos }),
            &config_path,
            &steam,
        )?,
        Action::Wallpaper { command, options } => {
            let WallpaperAction::List { options } =
                command.unwrap_or(WallpaperAction::List { options });
            for source in listing_sources(options.source(), &steam)? {
                match source {
                    SourceKind::We => {
                        for video in library::discover_videos(&steam)? {
                            println!("WE\t{}\t{}", video.title, video.path.display());
                        }
                    }
                    SourceKind::Local => {
                        let playlists = source_playlists(*source, &config_path, &steam)?;
                        let paths: std::collections::BTreeSet<_> =
                            playlists.into_iter().flat_map(|p| p.items).collect();
                        for path in paths {
                            println!("native\t{}", path.display());
                        }
                    }
                }
            }
        }
        Action::Screen { command } => manage_screens(command, &config_path, &steam)?,
        Action::Play {
            playlist,
            file,
            screen,
            mpv_options,
            playback,
        } => {
            playback.validate()?;
            let pl = if let Some(file) = file {
                let file = file.canonicalize().context("resolve video path")?;
                ensure!(
                    file.is_file() && library::is_video(&file),
                    "not a supported video file: {}",
                    file.display()
                );
                Playlist {
                    name: file.display().to_string(),
                    items: vec![file],
                    playback: Playback::default(),
                    skipped: 0,
                }
            } else if let Some(reference) = playlist {
                named_playlist(reference.source, &config_path, &steam, &reference.name)?
            } else {
                let mut playlists = source_playlists(SourceKind::Local, &config_path, &steam)?;
                let index = choose(&playlists)?;
                playlists.remove(index)
            };
            let screens = if screen.is_empty() {
                vec!["ALL".into()]
            } else {
                screen
            };
            let plans = screens
                .into_iter()
                .map(|s| (s, 0, playback.clone()))
                .collect();
            launch(&config_path, &[pl], plans, mpv_options.as_deref())?;
        }
        Action::Apply {
            screen,
            mpv_options,
            playback,
        } => {
            apply_bindings(
                &config_path,
                steam_override.as_deref(),
                &steam,
                screen,
                playback,
                mpv_options.as_deref(),
            )?;
        }
        Action::Status { screen } => show_status(screen)?,
        Action::Pause { screen } => control(screen, json!(["set_property", "pause", true]), false)?,
        Action::Resume { screen } => {
            control(screen, json!(["set_property", "pause", false]), false)?
        }
        Action::Next { screen } => control(screen, json!(["playlist-next", "force"]), false)?,
        Action::Prev { screen } => control(screen, json!(["playlist-prev", "force"]), false)?,
        Action::Stop { screen } => control(screen, json!(["quit"]), true)?,
        Action::Mpv {
            command,
            json: raw,
            args,
            screen,
        } => {
            let command = if let Some(raw) = raw {
                let command: Value =
                    serde_json::from_str(&raw).context("--json must be an mpv command array")?;
                ensure!(
                    command
                        .as_array()
                        .is_some_and(|a| a.first().is_some_and(Value::is_string)),
                    "mpv command must be a nonempty array starting with a command name"
                );
                command
            } else {
                Value::Array(
                    std::iter::once(command.context("missing mpv command")?)
                        .chain(args)
                        .map(Value::String)
                        .collect(),
                )
            };
            control(screen, command, false)?;
        }
        Action::Session { .. } => unreachable!("session handled before config resolution"),
    }
    Ok(())
}

fn listing_sources(source: ListingSource, steam: &Path) -> Result<&'static [SourceKind]> {
    if matches!(source, ListingSource::Local) {
        return Ok(&[SourceKind::Local]);
    }
    let path = steam.join("steamapps/common/wallpaper_engine/config.json");
    let installed = path
        .try_exists()
        .with_context(|| format!("check WE configuration {}", path.display()))?;
    match source {
        ListingSource::Local => unreachable!(),
        ListingSource::We => {
            ensure!(installed, "WE configuration not found: {}", path.display());
            Ok(&[SourceKind::We])
        }
        ListingSource::All if installed => Ok(&[SourceKind::Local, SourceKind::We]),
        ListingSource::All => Ok(&[SourceKind::Local]),
    }
}

fn print_playlist(source: SourceKind, pl: &Playlist, videos: bool) -> Result<()> {
    println!(
        "{}{}\t{} videos\t{} skipped",
        cli::reference_prefix(source, &pl.name),
        pl.name,
        pl.items.len(),
        pl.skipped
    );
    if videos {
        println!("  playback: {}", playback_json(&pl.playback)?);
        for (index, path) in pl.items.iter().enumerate() {
            println!("  {}. {}", index + 1, path.display());
        }
    }
    Ok(())
}

fn manage_playlists(command: PlaylistAction, config_path: &Path, steam: &Path) -> Result<()> {
    match command {
        PlaylistAction::List { options, videos } => {
            let mut count = 0;
            for source in listing_sources(options.source(), steam)? {
                for pl in source_playlists(*source, config_path, steam)? {
                    print_playlist(*source, &pl, videos)?;
                    count += 1;
                }
            }
            if count == 0 {
                println!("No playlists in selected source");
            }
        }
        PlaylistAction::Show { playlist } => {
            let pl = source_playlists(playlist.source, config_path, steam)?
                .into_iter()
                .find(|p| p.name == playlist.name)
                .with_context(|| format!("playlist not found: {playlist}"))?;
            print_playlist(playlist.source, &pl, true)?;
        }
        PlaylistAction::Set { playlist, playback } => {
            playback.validate()?;
            config::update(config_path, |settings| {
                match playlist.source {
                    SourceKind::Local => {
                        let native =
                            settings
                                .playlists
                                .get_mut(&playlist.name)
                                .with_context(|| {
                                    format!("native playlist not found: {}", playlist.name)
                                })?;
                        native.playback = native.playback.merged(&playback);
                    }
                    SourceKind::We => {
                        let own = settings
                            .we_overrides
                            .entry(playlist.name.clone())
                            .or_default();
                        *own = own.merged(&playback);
                        ensure!(
                            library::load_playlists(steam, &settings.we_overrides)?
                                .iter()
                                .any(|p| p.name == playlist.name),
                            "WE playlist not found: {}",
                            playlist.name
                        );
                    }
                }
                Ok(())
            })?;
            println!("{playlist}: saved playback settings; use apply to restart saved bindings");
        }
        PlaylistAction::Create {
            name,
            files,
            replace,
            playback,
        } => {
            playback.validate()?;
            ensure!(!name.trim().is_empty(), "playlist name cannot be empty");
            let files: Vec<PathBuf> = files
                .into_iter()
                .map(|file| {
                    let file = file.canonicalize().context("resolve video path")?;
                    ensure!(
                        library::is_video(&file) && file.is_file(),
                        "not a supported video file: {}",
                        file.display()
                    );
                    Ok(file)
                })
                .collect::<Result<_>>()?;
            let count = files.len();
            config::update(config_path, |settings| {
                ensure!(
                    replace || !settings.playlists.contains_key(&name),
                    "native playlist already exists: {name}; use --replace"
                );
                settings.playlists.insert(
                    name.clone(),
                    NativePlaylist {
                        items: files,
                        playback,
                    },
                );
                Ok(())
            })?;
            println!(
                "{}{name}: saved native playlist ({count} videos)",
                cli::reference_prefix(SourceKind::Local, &name)
            );
        }
        PlaylistAction::Import { options } => {
            let report = import::run(steam, config_path, &options)?;
            println!(
                "Imported {} native playlists; {} videos copied; {} entries and {} empty playlists skipped",
                report.playlists.len(),
                report.copied_files,
                report.skipped_items,
                report.skipped_playlists
            );
            for name in report.playlists {
                println!(
                    "  {}{name}",
                    cli::reference_prefix(SourceKind::Local, &name)
                );
            }
            if let Some(directory) = report.media_dir {
                println!("Media: {}", directory.display());
            }
        }
    }
    Ok(())
}

fn manage_screens(command: ScreenAction, config_path: &Path, steam: &Path) -> Result<()> {
    match command {
        ScreenAction::List => {
            ensure!(
                Command::new("mpvpaper")
                    .arg("-d")
                    .status()
                    .context("run mpvpaper")?
                    .success(),
                "mpvpaper could not list outputs"
            );
        }
        ScreenAction::Show { screen } => {
            let settings = config::load(config_path)?;
            if let Some(screen) = &screen {
                validate_monitor(screen)?;
                ensure!(
                    settings.screens.contains_key(screen),
                    "no saved binding for {screen}"
                );
            }
            if settings.screens.is_empty() {
                println!("No saved screen bindings");
            }
            for (name, assignment) in settings.screens {
                if screen.as_ref().is_some_and(|screen| screen != &name) {
                    continue;
                }
                match &assignment.source {
                    Source::Local { playlist } => println!(
                        "{name}\t{}{playlist}",
                        cli::reference_prefix(SourceKind::Local, playlist)
                    ),
                    Source::WallpaperEngine {
                        playlist,
                        steamroot,
                    } => println!("{name}\tWE:{playlist}\t{}", steamroot.display()),
                }
                println!("  overrides: {}", playback_json(&assignment.playback)?);
            }
        }
        ScreenAction::Set {
            screen,
            playlist,
            save_only,
            playback,
        } => {
            validate_monitor(&screen)?;
            playback.validate()?;
            let pl = named_playlist(playlist.source, config_path, steam, &playlist.name)?;
            let source = match playlist.source {
                SourceKind::Local => Source::Local {
                    playlist: playlist.name.clone(),
                },
                SourceKind::We => Source::WallpaperEngine {
                    playlist: playlist.name.clone(),
                    steamroot: steam.canonicalize().context("resolve Steam library")?,
                },
            };
            let assignment = Assignment { source, playback };
            config::update(config_path, |settings| {
                ensure!(
                    !settings
                        .screens
                        .keys()
                        .any(|old| old != &screen && (old == "ALL" || screen == "ALL")),
                    "ALL conflicts with saved individual outputs; remove those bindings first"
                );
                settings.screens.insert(screen.clone(), assignment.clone());
                Ok(())
            })?;
            println!("{screen}: saved {playlist}");
            if !save_only {
                launch(
                    config_path,
                    &[pl],
                    vec![(screen, 0, assignment.playback)],
                    None,
                )?;
            }
        }
        ScreenAction::Unset { screen } => {
            validate_monitor(&screen)?;
            config::update(config_path, |settings| {
                ensure!(
                    settings.screens.remove(&screen).is_some(),
                    "no saved binding for {screen}"
                );
                Ok(())
            })?;
            println!("{screen}: removed saved binding (playback unchanged)");
        }
    }
    Ok(())
}

fn apply_bindings(
    config_path: &Path,
    steam_override: Option<&Path>,
    steam: &Path,
    screens: Vec<String>,
    playback: Playback,
    extra: Option<&str>,
) -> Result<()> {
    playback.validate()?;
    let settings = config::load(config_path)?;
    let screens = if screens.is_empty() {
        settings.screens.keys().cloned().collect()
    } else {
        screens
    };
    ensure!(
        !screens.is_empty(),
        "no saved bindings; use screen set SCREEN PLAYLIST first"
    );
    let mut plans = Vec::new();
    let mut playlists = Vec::new();
    for screen in screens {
        let assignment = settings
            .screens
            .get(&screen)
            .with_context(|| format!("no saved binding for {screen}"))?;
        let pl = match &assignment.source {
            Source::Local { playlist } => {
                named_playlist(SourceKind::Local, config_path, steam, playlist)?
            }
            Source::WallpaperEngine {
                playlist,
                steamroot,
            } => named_playlist(
                SourceKind::We,
                config_path,
                steam_override.unwrap_or(steamroot),
                playlist,
            )?,
        };
        plans.push((
            screen,
            playlists.len(),
            assignment.playback.merged(&playback),
        ));
        playlists.push(pl);
    }
    launch(config_path, &playlists, plans, extra)
}

fn show_status(screen: Option<String>) -> Result<()> {
    let root = runtime()?;
    let mut screens = sessions(&root)?;
    if let Some(screen) = screen {
        validate_monitor(&screen)?;
        ensure!(screens.contains(&screen), "no managed player for {screen}");
        screens.retain(|name| name == &screen);
    }
    if screens.is_empty() {
        println!("No managed players");
    }
    for screen in screens {
        match ipc(&socket(&root, &screen), json!(["get_property", "path"])) {
            Ok(file) => {
                let paused = ipc(&socket(&root, &screen), json!(["get_property", "pause"]))?;
                println!(
                    "{screen}\t{}\t{}",
                    if paused == true { "paused" } else { "playing" },
                    file.as_str().unwrap_or("(idle)")
                );
            }
            Err(_)
                if ipc(
                    &socket(&root, &screen),
                    json!(["get_property", "idle-active"]),
                )
                .is_ok_and(|value| value == true) =>
            {
                println!("{screen}\tidle\t(end of playlist)")
            }
            Err(_)
                if root.join(format!("{screen}.pid")).is_file()
                    && owned_process(&root, &screen)?.is_none() =>
            {
                println!("{screen}\tstopped\t(end of playback or player exited; see log)")
            }
            Err(error) => println!("{screen}\tunreachable\t{error:#}"),
        }
    }
    Ok(())
}

fn config_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = path.map(Ok).unwrap_or_else(config::default_path)?;
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn playback_json(playback: &Playback) -> Result<Value> {
    let mut values = serde_json::to_value(playback)?;
    if let Some(values) = values.as_object_mut() {
        values.retain(|_, value| !value.is_null());
    }
    Ok(values)
}

fn source_playlists(source: SourceKind, config_path: &Path, steam: &Path) -> Result<Vec<Playlist>> {
    match source {
        SourceKind::We => {
            let settings = config::load(config_path)?;
            library::load_playlists(steam, &settings.we_overrides)
        }
        SourceKind::Local => {
            let settings = config::load(config_path)?;
            let base = config_path.parent().unwrap_or(Path::new("."));
            Ok(settings
                .playlists
                .into_iter()
                .map(|(name, playlist)| {
                    let count = playlist.items.len();
                    let items: Vec<_> = playlist
                        .items
                        .into_iter()
                        .map(|p| base.join(p))
                        .filter(|p| library::is_video(p) && p.is_file())
                        .collect();
                    Playlist {
                        name,
                        skipped: count - items.len(),
                        items,
                        playback: playlist.playback,
                    }
                })
                .collect())
        }
    }
}

fn named_playlist(
    source: SourceKind,
    config_path: &Path,
    steam: &Path,
    name: &str,
) -> Result<Playlist> {
    let pl = source_playlists(source, config_path, steam)?
        .into_iter()
        .find(|pl| pl.name == name)
        .with_context(|| format!("playlist not found in selected source: {name}"))?;
    ensure!(
        !pl.items.is_empty(),
        "playlist {name} contains no installed video wallpapers"
    );
    Ok(pl)
}

fn launch(
    config_path: &Path,
    playlists: &[Playlist],
    mut plans: Vec<(String, usize, Playback)>,
    extra: Option<&str>,
) -> Result<()> {
    let defaults = config::load(config_path)?.defaults;
    let root = runtime()?;
    let _lock = lock(&root)?;
    let existing = sessions(&root)?;
    ensure!(
        !(plans.iter().any(|(m, _, _)| m == "ALL") && plans.len() != 1),
        "ALL cannot be combined with other outputs"
    );
    for (monitor, index, playback) in &mut plans {
        let pl = &playlists[*index];
        validate_monitor(monitor)?;
        ensure!(
            !pl.items.is_empty(),
            "playlist {} contains no installed video wallpapers",
            pl.name
        );
        *playback = defaults.merged(&pl.playback).merged(playback);
        playback.options(&socket(&root, monitor))?;
        ensure!(
            !existing.iter().any(|old| old.as_str() != monitor.as_str()
                && (old == "ALL" || monitor.as_str() == "ALL")
                && alive(&root, old)),
            "ALL overlaps an active output; stop existing players first"
        );
    }
    for (monitor, index, playback) in plans {
        let pl = &playlists[index];
        if pl.skipped > 0 {
            eprintln!(
                "{}: skipping {} non-video or missing entries",
                pl.name, pl.skipped
            );
        }
        start(&root, &monitor, pl, &playback, extra)?;
    }
    Ok(())
}

fn choose(playlists: &[Playlist]) -> Result<usize> {
    ensure!(
        !playlists.is_empty(),
        "no native playlists; use playlist create or playlist import WE:NAME"
    );
    for (i, pl) in playlists.iter().enumerate() {
        println!(
            "{}: {} ({} videos, {} skipped)",
            i + 1,
            pl.name,
            pl.items.len(),
            pl.skipped
        );
    }
    print!("Playlist number: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let number: usize = input
        .trim()
        .parse()
        .context("enter a playlist number or use --playlist NAME")?;
    ensure!(
        (1..=playlists.len()).contains(&number),
        "playlist number out of range"
    );
    Ok(number - 1)
}

fn runtime() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; run within your desktop session")?;
    let root = PathBuf::from(base).join("waywe");
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    ensure!(
        !root.to_string_lossy().contains(char::is_whitespace),
        "runtime directory cannot contain whitespace (mpvpaper option syntax)"
    );
    Ok(root)
}

fn lock(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("lock"))?;
    file.try_lock()
        .context("another waywe operation is running")?;
    Ok(file)
}

fn validate_monitor(m: &str) -> Result<()> {
    ensure!(
        !m.is_empty()
            && m.len() <= 64
            && m.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && m != "."
            && m != "..",
        "invalid output name: {m}"
    );
    Ok(())
}

fn socket(root: &Path, monitor: &str) -> PathBuf {
    root.join(format!("{monitor}.sock"))
}

fn sessions(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if let Some(name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".session.json"))
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn ipc(socket: &Path, command: Value) -> Result<Value> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(&mut stream, &json!({"command": command, "request_id": 1}))?;
    stream.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        ensure!(Instant::now() < deadline, "mpv IPC response timed out");
        let mut line = String::new();
        ensure!(
            reader.read_line(&mut line)? != 0,
            "mpv closed IPC before responding"
        );
        let response: Value = serde_json::from_str(&line)?;
        if response["request_id"] == 1 {
            ensure!(response["error"] == "success", "mpv: {}", response["error"]);
            return Ok(response["data"].clone());
        }
    }
}

fn alive(root: &Path, monitor: &str) -> bool {
    ipc(
        &socket(root, monitor),
        json!(["get_property", "idle-active"]),
    )
    .is_ok()
}

fn clean(root: &Path, monitor: &str) -> Result<()> {
    for suffix in ["sock", "mpv.sock", "session.json", "pid"] {
        match fs::remove_file(root.join(format!("{monitor}.{suffix}"))) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

// Linux start time distinguishes the recorded process from a reused PID.
fn process_token(pid: u32) -> Result<Option<String>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (_, tail) = stat.rsplit_once(')').context("invalid process stat")?;
    let mut fields = tail.split_whitespace();
    if matches!(fields.next(), Some("Z" | "X")) {
        return Ok(None);
    }
    Ok(Some(
        fields
            .nth(18)
            .context("missing process start time")?
            .to_owned(),
    ))
}

fn owned_process(
    root: &Path,
    monitor: &str,
) -> Result<Option<(std::os::fd::OwnedFd, u32, String)>> {
    let record = match fs::read_to_string(root.join(format!("{monitor}.pid"))) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (pid, token) = record
        .trim()
        .split_once(' ')
        .context("invalid managed process record")?;
    let pid: u32 = pid.parse()?;
    let process = Pid::from_raw(i32::try_from(pid)?).context("invalid managed PID")?;
    let fd = match pidfd_open(process, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::SRCH) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Verify after opening: the fd stays bound to this exact process, never a reused PID.
    if process_token(pid)?.as_deref() != Some(token) {
        return Ok(None);
    }
    Ok(Some((fd, pid, token.to_owned())))
}

fn stop(root: &Path, monitor: &str) -> Result<()> {
    let owned = owned_process(root, monitor)?;
    let path = socket(root, monitor);
    match UnixStream::connect(&path) {
        Ok(mut stream) => {
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            if let Err(error) = stream.write_all(b"{\"command\":[\"quit\"]}\n") {
                ensure!(
                    owned.is_some()
                        && matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                        ),
                    "could not request player shutdown: {error}"
                );
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            ()
        }
        Err(error) => return Err(error.into()),
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let running = if let Some((_, pid, token)) = &owned {
            process_token(*pid)?.as_deref() == Some(token.as_str())
        } else {
            UnixStream::connect(&path).is_ok()
        };
        if !running {
            break;
        }
        if Instant::now() >= deadline {
            let (fd, pid, token) = owned
                .as_ref()
                .context("untracked player did not stop; refusing to signal an unknown process")?;
            eprintln!("{monitor}: mpvpaper stalled during quit; terminating the tracked process");
            match pidfd_send_signal(fd, Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => (),
                Err(error) => return Err(error.into()),
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while process_token(*pid)?.as_deref() == Some(token.as_str()) {
                ensure!(
                    Instant::now() < deadline,
                    "player for {monitor} did not terminate"
                );
                thread::sleep(Duration::from_millis(50));
            }
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    clean(root, monitor)
}

fn control(monitor: Option<String>, command: Value, stopping: bool) -> Result<()> {
    let root = runtime()?;
    let _lock = if stopping { Some(lock(&root)?) } else { None };
    let monitors = if let Some(m) = monitor {
        validate_monitor(&m)?;
        vec![m]
    } else {
        sessions(&root)?
    };
    let mut errors = Vec::new();
    for m in monitors {
        let result = if stopping {
            stop(&root, &m).map(|_| Value::Null)
        } else {
            ipc(&socket(&root, &m), command.clone())
        };
        match result {
            Ok(value) if !value.is_null() => println!("{m}\t{value}"),
            Ok(_) => println!("{m}: {}", if stopping { "stopped" } else { "ok" }),
            Err(error) => errors.push(format!("{m}: {error:#}")),
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

fn start(
    root: &Path,
    monitor: &str,
    pl: &Playlist,
    playback: &Playback,
    extra: Option<&str>,
) -> Result<()> {
    playback.options(&socket(root, monitor))?;
    stop(root, monitor)?;
    let spec_path = root.join(format!("{monitor}.session.json"));
    let mut spec_file = File::create(&spec_path)?;
    let mut serializer = serde_json::Serializer::new(&mut spec_file);
    let mut spec = (&mut serializer).serialize_map(Some(4))?;
    spec.serialize_entry("screen", monitor)?;
    spec.serialize_entry("items", &pl.items)?;
    spec.serialize_entry("playback", playback)?;
    spec.serialize_entry("mpv_options", &extra)?;
    spec.end()?;
    drop(spec_file);
    let log_path = root.join(format!("{monitor}.log"));
    let log = File::create(&log_path)?;
    let child = Command::new(std::env::current_exe()?)
        .arg("session")
        .arg("--spec")
        .arg(&spec_path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("start waywe playback manager");
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            clean(root, monitor)?;
            return Err(error);
        }
    };
    let tracking = (|| -> Result<()> {
        let token =
            process_token(child.id())?.context("waywe manager exited before process tracking")?;
        fs::write(
            root.join(format!("{monitor}.pid")),
            format!("{} {token}\n", child.id()),
        )?;
        Ok(())
    })();
    if let Err(error) = tracking {
        let _ = child.kill();
        let _ = child.wait();
        clean(root, monitor)?;
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait()? {
            clean(root, monitor)?;
            bail!(
                "waywe manager exited ({status}); see {}",
                log_path.display()
            );
        }
        if ipc(&socket(root, monitor), json!(["get_property", "path"])).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            clean(root, monitor)?;
            bail!(
                "waywe playback manager did not become ready; see {}",
                log_path.display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    println!(
        "{monitor}: started {} (manager pid {}, log {})",
        pl.name,
        child.id(),
        log_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tracking_rejects_a_reused_pid_identity() {
        let pid = std::process::id();
        let token = process_token(pid).unwrap().unwrap();
        let root = std::env::temp_dir().join(format!("waywe-pid-test-{pid}"));
        fs::create_dir(&root).unwrap();
        let record = root.join("screen.pid");
        fs::write(&record, format!("{pid} {token}\n")).unwrap();
        let matched = owned_process(&root, "screen").unwrap().unwrap();
        assert_eq!(matched.1, pid);
        drop(matched);
        fs::write(&record, format!("{pid} stale-start-time\n")).unwrap();
        let stale = owned_process(&root, "screen").unwrap();
        fs::remove_dir_all(root).unwrap();
        assert!(
            stale.is_none(),
            "a reused PID must not be eligible for signaling"
        );
    }

    #[test]
    fn native_playlists_resolve_config_relative_paths_without_steam() {
        let root = std::env::temp_dir().join(format!(
            "waywe-native-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("media")).unwrap();
        fs::write(root.join("media/clip.mp4"), b"fixture").unwrap();
        let path = root.join("config.json");
        config::update(&path, |settings| {
            settings.playlists.insert(
                "native".into(),
                NativePlaylist {
                    items: vec![
                        "media/clip.mp4".into(),
                        "missing.mp4".into(),
                        "media/clip.mp4".into(),
                    ],
                    playback: Playback {
                        speed: Some(0.5),
                        interval: Some(0),
                        ..Playback::default()
                    },
                },
            );
            Ok(())
        })
        .unwrap();
        let playlists = source_playlists(SourceKind::Local, &path, &root.join("no-steam")).unwrap();
        let playlist = &playlists[0];
        assert_eq!(
            playlist.items,
            vec![root.join("media/clip.mp4"), root.join("media/clip.mp4")]
        );
        assert_eq!(playlist.skipped, 1);
        assert_eq!(playlist.playback.speed, Some(0.5));
        assert_eq!(
            listing_sources(ListingSource::All, &root.join("no-steam")).unwrap(),
            &[SourceKind::Local]
        );
        assert!(listing_sources(ListingSource::We, &root.join("no-steam")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
