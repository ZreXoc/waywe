use std::{fmt, path::PathBuf, str::FromStr};

use crate::{config::Playback, import};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    version,
    about = "Manage video wallpapers with mpvpaper",
    after_help = "PLAYLIST REFERENCES:\n  favorites       Native playlist\n  WE:playlist1    Wallpaper Engine playlist\n\nQUICK START:\n  waywe pl ls\n  waywe sc set DP-2 favorites\n  waywe apply\n  waywe play WE:playlist1 -s DP-2"
)]
pub struct Cli {
    /// Override waywe's configuration file
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Steam library used by Wallpaper Engine references and imports
    #[arg(long, global = true)]
    pub steam_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Local,
    We,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ListingSource {
    Local,
    #[value(name = "WE", alias = "we")]
    We,
    All,
}

#[derive(Clone, Debug, Default, clap::Args)]
#[group(multiple = false)]
pub struct ListingOptions {
    /// List all sources (the default)
    #[arg(short = 'a', long)]
    all: bool,
    /// List only native playlists or their local video files
    #[arg(long)]
    local: bool,
    /// List only installed Wallpaper Engine content
    #[arg(long = "WE", visible_alias = "we")]
    we: bool,
    #[arg(long, value_enum, ignore_case = true, hide = true)]
    source: Option<ListingSource>,
}

impl ListingOptions {
    pub fn source(&self) -> ListingSource {
        if self.local {
            ListingSource::Local
        } else if self.we {
            ListingSource::We
        } else {
            self.source.unwrap_or(ListingSource::All)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistRef {
    pub source: SourceKind,
    pub name: String,
}

impl FromStr for PlaylistRef {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (source, name) = if let Some(name) = we_reference_name(value) {
            (SourceKind::We, name)
        } else {
            (
                SourceKind::Local,
                value.strip_prefix("local:").unwrap_or(value),
            )
        };
        if name.trim().is_empty() {
            return Err("playlist name cannot be empty".into());
        }
        Ok(Self {
            source,
            name: name.into(),
        })
    }
}

fn we_reference_name(value: &str) -> Option<&str> {
    value
        .get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("WE:"))
        .map(|_| &value[3..])
}

pub fn reference_prefix(source: SourceKind, name: &str) -> &'static str {
    match source {
        SourceKind::We => "WE:",
        SourceKind::Local if we_reference_name(name).is_some() || name.starts_with("local:") => {
            "local:"
        }
        SourceKind::Local => "",
    }
}

impl fmt::Display for PlaylistRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            reference_prefix(self.source, &self.name),
            self.name
        )
    }
}

#[derive(Subcommand)]
pub enum Action {
    #[command(hide = true)]
    Session {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Show or change global playback defaults
    Defaults {
        #[command(subcommand)]
        command: Option<DefaultsAction>,
    },
    /// List, inspect, create, and import playlists
    #[command(visible_alias = "pl", args_conflicts_with_subcommands = true)]
    Playlist {
        #[command(subcommand)]
        command: Option<PlaylistAction>,
        #[command(flatten)]
        options: ListingOptions,
        /// Expand every video's path
        #[arg(short = 'v', long)]
        videos: bool,
    },
    /// List available video wallpapers
    #[command(visible_alias = "wp", args_conflicts_with_subcommands = true)]
    Wallpaper {
        #[command(subcommand)]
        command: Option<WallpaperAction>,
        #[command(flatten)]
        options: ListingOptions,
    },
    /// Discover screens and manage their saved wallpaper bindings
    #[command(visible_alias = "sc")]
    Screen {
        #[command(subcommand)]
        command: ScreenAction,
    },
    /// Play temporarily without changing saved bindings; omit playlist to choose
    Play {
        #[arg(conflicts_with = "file")]
        playlist: Option<PlaylistRef>,
        #[arg(long)]
        file: Option<PathBuf>,
        /// Target screen; repeat for multiple screens (default: ALL)
        #[arg(short = 's', long)]
        screen: Vec<String>,
        /// Additional mpv options
        #[arg(long)]
        mpv_options: Option<String>,
        #[command(flatten)]
        playback: Playback,
    },
    /// Restore saved bindings (default: all saved screens)
    Apply {
        #[arg(short = 's', long)]
        screen: Vec<String>,
        #[arg(long)]
        mpv_options: Option<String>,
        #[command(flatten)]
        playback: Playback,
    },
    /// Show player state (default: all managed screens)
    Status {
        #[arg(short = 's', long)]
        screen: Option<String>,
    },
    /// Pause playback (default: all managed screens)
    Pause {
        #[arg(short = 's', long)]
        screen: Option<String>,
    },
    /// Resume playback (default: all managed screens)
    Resume {
        #[arg(short = 's', long)]
        screen: Option<String>,
    },
    /// Play the next video
    Next {
        #[arg(short = 's', long)]
        screen: Option<String>,
    },
    /// Play the previous video
    Prev {
        #[arg(short = 's', long)]
        screen: Option<String>,
    },
    /// Stop players, leaving saved bindings unchanged
    Stop {
        #[arg(short = 's', long)]
        screen: Option<String>,
    },
    /// Send an mpv command; place screen options before the command
    Mpv {
        #[arg(short = 's', long)]
        screen: Option<String>,
        #[arg(required_unless_present = "json", conflicts_with = "json")]
        command: Option<String>,
        /// Raw mpv command array for structured arguments
        #[arg(long)]
        json: Option<String>,
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            requires = "command"
        )]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum DefaultsAction {
    /// Show configured global defaults
    Show,
    /// Update specified defaults without changing other fields or restarting players
    Set {
        #[command(flatten)]
        playback: Playback,
    },
}

#[derive(Subcommand)]
pub enum PlaylistAction {
    /// List playlists; --videos also expands every video's path
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        options: ListingOptions,
        #[arg(short = 'v', long)]
        videos: bool,
    },
    /// Show a playlist's videos and playback settings
    #[command(visible_alias = "info")]
    Show { playlist: PlaylistRef },
    /// Save per-field playback overrides; WE references inherit unspecified WE settings
    Set {
        playlist: PlaylistRef,
        #[command(flatten)]
        playback: Playback,
    },
    /// Create a native playlist from video files
    Create {
        name: String,
        #[arg(required = true, num_args = 1..)]
        files: Vec<PathBuf>,
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        playback: Playback,
    },
    /// Import WE:NAME (references only by default); --copy also copies videos
    Import {
        #[command(flatten)]
        options: import::Options,
    },
}

#[derive(Subcommand)]
pub enum WallpaperAction {
    /// List videos in native playlists or installed Wallpaper Engine projects
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        options: ListingOptions,
    },
}

#[derive(Subcommand)]
pub enum ScreenAction {
    /// Discover connected outputs through mpvpaper
    #[command(visible_alias = "ls")]
    List,
    /// Show saved bindings and overrides; omit screen to show all
    #[command(visible_alias = "info")]
    Show { screen: Option<String> },
    /// Save and apply a binding; --save-only keeps current playback unchanged
    Set {
        screen: String,
        playlist: PlaylistRef,
        #[arg(long)]
        save_only: bool,
        #[command(flatten)]
        playback: Playback,
    },
    /// Remove a saved binding without stopping playback
    Unset { screen: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_roundtrip_ambiguous_native_names() {
        for value in [
            "summer",
            "we:playlist1",
            "WE:playlist1",
            "wE:playlist1",
            "local:WE:summer",
            "local:We:summer",
            "local:we:summer",
            "local:local:summer",
        ] {
            let reference: PlaylistRef = value.parse().unwrap();
            assert_eq!(
                reference.to_string().parse::<PlaylistRef>().unwrap(),
                reference
            );
        }
        let native: PlaylistRef = "local:we:summer".parse().unwrap();
        assert_eq!(native.source, SourceKind::Local);
        assert_eq!(native.name, "we:summer");
        assert_eq!(
            "we:summer".parse::<PlaylistRef>().unwrap().source,
            SourceKind::We
        );
        assert_eq!(
            "we:summer".parse::<PlaylistRef>().unwrap().to_string(),
            "WE:summer"
        );
        for invalid in ["", "we:", "WE:", "local:", "we:   "] {
            assert!(invalid.parse::<PlaylistRef>().is_err());
        }
    }

    #[test]
    fn contradictory_source_filters_are_rejected() {
        assert!(Cli::try_parse_from(["waywe", "pl", "ls", "--local", "--WE"]).is_err());
        assert!(Cli::try_parse_from(["waywe", "pl", "ls", "--all", "--source", "we"]).is_err());
    }

    #[test]
    fn play_file_and_playlist_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["waywe", "play", "summer", "--file", "clip.mp4"]).is_err());
    }
}
