# waywe

[English](README.md) | 简体中文

Rust 编写的 Wayland 视频壁纸管理器，使用 mpvpaper 播放，支持原生播放列表和 Wallpaper Engine（WE）视频列表。

## Why

waywe 通过 [mpvpaper](https://github.com/GhostNaN/mpvpaper) 在 Wayland 下播放 Wallpaper Engine 的视频壁纸。你可以继续在 Wallpaper Engine 中管理播放列表、设置和视频文件，由 waywe 负责读取和播放。播放开销取决于视频、解码和渲染设置。

## 安装

依赖：Linux 5.3+、支持 layer-shell 的 Wayland 合成器、mpvpaper。编译需要 Rust 1.89+。

```sh
cargo install --path . --locked
waywe --help
```

GitHub Actions 原生构建 Linux x86_64 和 ARM64 二进制，工作流产物分别为 `waywe-linux-x86_64` 和 `waywe-linux-aarch64`。

## 播放列表

```sh
waywe pl                             # 全部列表
waywe pl -av                         # 全部列表及视频
waywe pl --WE                        # WE 列表
waywe pl --local                     # 原生列表
waywe pl info WE:playlist1            # 列表详情与设置
waywe wp                             # 可用视频壁纸
waywe pl create favorites /path/to/a.mp4 /path/to/b.webm
```

原生列表用名称引用，如 `favorites`；WE 列表用 `WE:名称` 引用。命令简写：`playlist/pl`、`wallpaper/wp`、`screen/sc`、`list/ls`、`show/info`。

## 默认值与列表设置

```sh
waywe defaults set --hwdec auto --mute true --fit cover --interval 1800 --shuffle true
waywe defaults                       # 查看全局默认值
waywe pl set favorites --interval 600
waywe pl set WE:playlist1 --interval 1800 --change-on-end false
waywe pl set WE:playlist1 --volume 30
```

`defaults set` 和 `pl set` 修改指定字段，保存后重新运行 `apply` 或 `play` 生效。

WE 列表先读取 WE 设置，再逐字段应用 `pl set WE:名称` 保存的覆盖值；其余字段继续使用 WE 设置。视频清单和 WE 设置在每次查看、启动时重新读取。

设置优先级：**本次命令 > 屏幕设置 > 列表设置 > 全局默认 > 内置默认**。屏幕设置用于 `sc set` 和 `apply`。

## 导入壁纸引擎播放列表到本地

导入后，原生列表独立于 WE 的后续列表编辑。

```sh
waywe pl import WE:playlist1 --name favorites           # 保存列表快照，引用原视频
waywe pl import WE:playlist1 --name standalone --copy   # 同时复制视频到本地
waywe pl import -a --copy --media-dir /mnt/wallpapers   # 批量导入
```

导入保存视频清单和当前列表设置。`--replace` 替换同名原生列表。复制视频默认保存到 `~/.local/share/waywe/media/`，遵循 `XDG_DATA_HOME`。

默认 Steam 库为 `~/.local/share/Steam`，自定义路径使用全局参数：

```sh
waywe --steam-dir /mnt/SteamLibrary pl --WE
```

## 屏幕绑定

```sh
waywe sc ls                          # 已连接屏幕
waywe sc info                        # 已保存绑定
waywe sc set DP-2 WE:playlist1        # 保存并立即播放
waywe sc set eDP-2 favorites --fit cover
waywe sc set DP-2 favorites --save-only
waywe sc unset DP-2                  # 删除绑定
waywe apply                          # 根据绑定信息开始播放
waywe apply -s eDP-2                 # 指定屏幕开始播放
```

`sc set` 整体替换该屏幕的覆盖设置，省略的字段继承列表和全局默认。

## 播放与控制

```sh
waywe play favorites -s DP-2          # 临时播放
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

`play` 省略列表时交互选择原生列表，省略 `-s` 时播放到 `ALL`。`apply` 和控制命令省略 `-s` 时作用于全部绑定或会话。`play`、`apply` 支持重复指定 `-s`。切换 `ALL` 与单屏播放前执行 `waywe stop`。

## 播放参数

适用于 `defaults set`、`pl create/set`、`sc set`、`play` 和 `apply`。布尔值写为 `true` / `false`。

| 参数 | 含义 | 内置默认 |
| --- | --- | --- |
| `--hwdec` | mpv 解码方式，如 `auto`、`vaapi`、`nvdec`、`no` | `auto` |
| `--mute` | 静音 | `true` |
| `--volume` | 音量，0–100 | `100` |
| `--speed` | 播放速度，大于 0 | `1` |
| `--fit` | `contain` 完整显示、`cover` 裁切铺满、`stretch` 拉伸 | `contain` |
| `--shuffle` | 随机顺序 | `false` |
| `--interval` | 定时切换间隔，秒；0 关闭定时器 | `0` |
| `--repeat` | 循环列表 | `true` |
| `--auto-pause` | 合成器判定壁纸隐藏时暂停 | `false` |
| `--change-on-end` | 视频结束时立即切换 | interval 为 0 时 `true`，否则 `false` |
| `--change-while-paused` | 暂停时继续计时、切换 | `false` |
| `--start-first` | 随机播放时从原列表第一项开始 | `false` |

定时播放时，短视频循环到间隔结束，长视频到时间切换。`change-on-end=true` 会在视频结束时提前切换。暂停默认冻结剩余时间，手动切换重新计时。

`play`、`apply` 可通过 `--mpv-options 'profile=fast'` 设置额外的 mpv 渲染、音频选项。

## 配置与日志

- 配置：`~/.config/waywe/config.json`，遵循 `XDG_CONFIG_HOME`；通过 `--config PATH` 指定其他文件。
- 配置字段：`defaults` 全局默认、`playlists` 原生列表、`we_overrides` WE 字段覆盖、`screens` 屏幕绑定。
- 字段设为 `null` 或删除后继承下一级设置；删除 `we_overrides` 中对应条目后恢复 WE 设置。
- 原生列表的相对视频路径以配置文件所在目录为基准。
- 运行文件与日志：`$XDG_RUNTIME_DIR/waywe/`，每屏日志为 `屏幕名.log`。

## 许可证

[MIT](LICENSE)
