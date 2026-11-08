# waywe

English | [简体中文](README.zh-CN.md)

A Rust video wallpaper manager for Wayland, powered by mpvpaper. Supports native playlists and Wallpaper Engine video playlists.

## Why

waywe plays Wallpaper Engine video wallpapers on Wayland through [mpvpaper](https://github.com/GhostNaN/mpvpaper). Keep managing playlists, settings, and video files in Wallpaper Engine, and let waywe handle playback. Resource usage depends on the videos, decoding, and rendering settings.

## Installation

Requirements: Linux 5.3+, a Wayland compositor with layer-shell support, and mpvpaper. Building requires Rust 1.89+.

```sh
cargo install --path . --locked
waywe --help
```

GitHub Actions builds native Linux binaries for x86_64 and ARM64, available as `waywe-linux-x86_64` and `waywe-linux-aarch64` workflow artifacts.

## Playlists

```sh
waywe pl                             # All playlists
waywe pl -av                         # All playlists with their videos
waywe pl --WE                        # WE playlists
waywe pl --local                     # Native playlists
waywe pl info WE:playlist1            # Playlist details and settings
waywe wp                             # Available video wallpapers
waywe pl create favorites /path/to/a.mp4 /path/to/b.webm
```

Refer to native playlists by name, such as `favorites`, and WE playlists as `WE:NAME`. Command aliases: `playlist/pl`, `wallpaper/wp`, `screen/sc`, `list/ls`, and `show/info`.

## Defaults and playlist settings

```sh
waywe defaults set --hwdec auto --mute true --fit cover --interval 1800 --shuffle true
waywe defaults                       # Show global defaults
waywe pl set favorites --interval 600
waywe pl set WE:playlist1 --interval 1800 --change-on-end false
waywe pl set WE:playlist1 --volume 30
```

`defaults set` and `pl set` update the specified fields. Run `apply` or `play` again to use the saved settings.

WE playlists load their settings from WE, then apply the per-field overrides saved by `pl set WE:NAME`. Other fields retain their WE values. Videos and WE settings are refreshed whenever a playlist is inspected or started.

Settings precedence: **current command > screen settings > playlist settings > global defaults > built-in defaults**. Screen settings apply to `sc set` and `apply`.

## Importing WE playlists

Imported native playlists are independent of subsequent playlist edits in WE.

```sh
waywe pl import WE:playlist1 --name favorites           # Snapshot the playlist, referencing original videos
waywe pl import WE:playlist1 --name standalone --copy   # Also copy the videos
waywe pl import -a --copy --media-dir /mnt/wallpapers   # Import all playable playlists
```

Imports save the video list and its current playlist settings. Use `--replace` to replace an existing native playlist. Copied videos default to `~/.local/share/waywe/media/`, respecting `XDG_DATA_HOME`.

The default Steam library is `~/.local/share/Steam`. Use the global option to select another library:

```sh
waywe --steam-dir /mnt/SteamLibrary pl --WE
```

## Screen bindings

```sh
waywe sc ls                          # Connected screens
waywe sc info                        # Saved bindings
waywe sc set DP-2 WE:playlist1        # Save and start playback
waywe sc set eDP-2 favorites --fit cover
waywe sc set DP-2 favorites --save-only
waywe sc unset DP-2                  # Remove the binding
waywe apply                          # Start playback from all saved bindings
waywe apply -s eDP-2                 # Start playback on a specific screen
```

`sc set` replaces the screen's overrides. Omitted fields inherit playlist settings and global defaults.

## Playback and controls

```sh
waywe play favorites -s DP-2          # Temporary playback
waywe play --file /path/to/video.mp4 -s DP-2
waywe status
waywe pause -s DP-2
waywe resume -s DP-2
waywe next -s DP-2
waywe prev -s DP-2
waywe stop
waywe mpv -s DP-2 get_property path
waywe mpv -s DP-2 set volume 30
waywe mpv -s DP-2 --json '["set_property","pause",true]'
```

Running `play` without a playlist opens an interactive native playlist selector; omitting `-s` targets `ALL`. For `apply` and control commands, omitting `-s` targets all saved bindings or managed sessions. Repeat `-s` with `play` or `apply` to select multiple screens. Run `waywe stop` before switching between `ALL` and individual screens.

## Playback options

Available on `defaults set`, `pl create/set`, `sc set`, `play`, and `apply`. Boolean values are `true` / `false`.

| Option | Meaning | Built-in default |
| --- | --- | --- |
| `--hwdec` | mpv decoder, such as `auto`, `vaapi`, `nvdec`, or `no` | `auto` |
| `--mute` | Mute audio | `true` |
| `--volume` | Volume, 0–100 | `100` |
| `--speed` | Playback speed, greater than 0 | `1` |
| `--fit` | `contain` fits the entire video, `cover` crops to fill, `stretch` stretches | `contain` |
| `--shuffle` | Shuffle playlist order | `false` |
| `--interval` | Seconds between entries; 0 disables the timer | `0` |
| `--repeat` | Repeat the playlist | `true` |
| `--auto-pause` | Pause when the compositor reports the wallpaper as hidden | `false` |
| `--change-on-end` | Switch immediately when a video ends | `true` when interval is 0, otherwise `false` |
| `--change-while-paused` | Keep the timer running and allow switches while paused | `false` |
| `--start-first` | Start with the original first entry when shuffling | `false` |

With a timer, short videos repeat until the interval expires; long videos switch at the deadline. `change-on-end=true` switches earlier when a video ends. Pausing freezes the remaining interval by default; manual navigation resets it.

Use `--mpv-options 'profile=fast'` with `play` or `apply` for additional mpv rendering and audio options.

## Configuration and logs

- Configuration: `~/.config/waywe/config.json`, respecting `XDG_CONFIG_HOME`; select another file with `--config PATH`.
- Fields: `defaults` for global defaults, `playlists` for native playlists, `we_overrides` for WE field overrides, and `screens` for screen bindings.
- Set a field to `null` or remove it to inherit the next settings layer. Remove a playlist's `we_overrides` entry to use its WE settings directly.
- Relative video paths in native playlists resolve from the configuration file's directory.
- Runtime files and logs: `$XDG_RUNTIME_DIR/waywe/`, with one `SCREEN.log` per screen.

## License

[MIT](LICENSE)
