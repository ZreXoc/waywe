use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::process::{Signal, getpid, getppid, set_parent_process_death_signal};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::Playback,
    scheduler::{Action, Scheduler},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub screen: String,
    pub items: Vec<PathBuf>,
    pub playback: Playback,
    pub mpv_options: Option<String>,
}

struct Player(Child);
impl Drop for Player {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct RuntimeFiles {
    root: PathBuf,
    screen: String,
}
impl Drop for RuntimeFiles {
    fn drop(&mut self) {
        let _ = crate::clean(&self.root, &self.screen);
    }
}

struct Client {
    command: Value,
    request_id: Value,
    stream: UnixStream,
}
impl Client {
    fn reply(mut self, result: Result<Value>) {
        let response = match result {
            Ok(data) => json!({"request_id": self.request_id, "error": "success", "data": data}),
            Err(error) => json!({"request_id": self.request_id, "error": format!("{error:#}")}),
        };
        let _ = self.stream.set_write_timeout(Some(Duration::from_secs(2)));
        if serde_json::to_writer(&mut self.stream, &response).is_ok() {
            let _ = self.stream.write_all(b"\n");
        }
    }
}

enum Event {
    Client(Client),
    Mpv(Value),
    Disconnected(String),
}
enum Target {
    Client(Client),
    Load,
    Replay,
    PauseState(u64),
}
struct Pending {
    target: Target,
    since: Instant,
}

struct Session {
    items: Vec<PathBuf>,
    scheduler: Scheduler,
    writer: UnixStream,
    receiver: Receiver<Event>,
    pending: HashMap<u64, Pending>,
    navigation: VecDeque<Client>,
    active_navigation: Option<Client>,
    next_id: u64,
    user_paused: bool,
    playback_paused: bool,
    change_while_paused: bool,
    pause_epoch: u64,
    at_eof: bool,
    replaying: bool,
    loading_since: Option<Instant>,
    finished: bool,
}

pub fn run(spec_path: &Path) -> Result<()> {
    let mut spec: Spec = serde_json::from_reader(File::open(spec_path)?)
        .with_context(|| format!("read playback session {}", spec_path.display()))?;
    crate::validate_monitor(&spec.screen)?;
    spec.playback.validate()?;
    ensure!(!spec.items.is_empty(), "empty playback session");
    for path in &spec.items {
        ensure!(path.is_file(), "video missing: {}", path.display());
    }
    let root = spec_path
        .parent()
        .context("session file has no directory")?;
    let _files = RuntimeFiles {
        root: root.to_owned(),
        screen: spec.screen.clone(),
    };
    if spec.playback.shuffle.unwrap_or(false) {
        let first = spec.playback.start_first.unwrap_or(false);
        shuffle(&mut spec.items[usize::from(first)..])?;
    }
    let private = root.join(format!("{}.mpv.sock", spec.screen));
    let (mut options, interval, auto_pause) = spec.playback.options(&private)?;
    if let Some(extra) = &spec.mpv_options {
        options.push(' ');
        options.push_str(extra);
    }
    options.push_str(&format!(
        " input-ipc-server={} keep-open=always keep-open-pause=no idle=yes pause=no",
        private.display()
    ));
    let mut command = Command::new("mpvpaper");
    if auto_pause {
        command.arg("--auto-pause");
    }
    command
        .arg("-o")
        .arg(options)
        .arg(&spec.screen)
        .arg(&spec.items[0])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let parent = getpid();
    // Only async-signal-safe syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            set_parent_process_death_signal(Some(Signal::KILL)).map_err(io::Error::from)?;
            if getppid() != Some(parent) {
                return Err(io::Error::from_raw_os_error(3));
            }
            Ok(())
        });
    }
    let mut player = Player(
        command
            .spawn()
            .context("start mpvpaper; is it installed?")?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = player.0.try_wait()? {
            bail!("mpvpaper exited during startup: {status}");
        }
        if crate::ipc(&private, json!(["get_property", "video-params"])).is_ok() {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "mpvpaper video did not become ready"
        );
        thread::sleep(Duration::from_millis(30));
    }
    // mpvpaper overwrites idle during its initialization. Disable inherited or raw
    // native scheduling here; Rust is the only owner of playlist transitions.
    for (name, value) in [
        ("idle", "yes"),
        ("loop-file", "no"),
        ("loop-playlist", "no"),
        ("shuffle", "no"),
        ("keep-open", "always"),
        ("keep-open-pause", "no"),
    ] {
        crate::ipc(&private, json!(["set_property", name, value]))?;
    }
    let writer = UnixStream::connect(&private)?;
    let reader = writer.try_clone()?;
    let (sender, receiver) = mpsc::channel();
    spawn_mpv_reader(reader, sender.clone());
    let listener = UnixListener::bind(crate::socket(root, &spec.screen))?;
    spawn_clients(listener, sender);
    let mut session = Session {
        scheduler: Scheduler::new(
            spec.items.len(),
            interval.map(Duration::from_secs),
            spec.playback.repeat.unwrap_or(true),
            spec.playback.change_on_end.unwrap_or(interval.is_none()),
        ),
        items: spec.items,
        writer,
        receiver,
        pending: HashMap::new(),
        navigation: VecDeque::new(),
        active_navigation: None,
        next_id: 10,
        user_paused: false,
        playback_paused: false,
        change_while_paused: spec.playback.change_while_paused.unwrap_or(false),
        pause_epoch: 0,
        at_eof: false,
        replaying: false,
        loading_since: None,
        finished: false,
    };
    session.scheduler.file_loaded(Instant::now());
    // The initial property notifications also cover a short clip that reached EOF
    // before the supervisor finished connecting.
    session.send(&json!(["observe_property", 1, "eof-reached"]), None)?;
    session.send(&json!(["observe_property", 2, "pause"]), None)?;
    session.send(&json!(["observe_property", 3, "loop-file"]), None)?;
    session.send(&json!(["observe_property", 4, "loop-playlist"]), None)?;
    session.send(&json!(["observe_property", 5, "keep-open-pause"]), None)?;
    let result = session.event_loop(&mut player);
    let _ = session.send(&json!(["quit"]), None);
    let deadline = Instant::now() + Duration::from_millis(700);
    while player.0.try_wait()?.is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    result
}

fn shuffle(items: &mut [PathBuf]) -> Result<()> {
    let mut random = File::open("/dev/urandom")?;
    for i in (1..items.len()).rev() {
        let n = (i + 1) as u64;
        let ceiling = u64::MAX - u64::MAX % n;
        let choice = loop {
            let mut bytes = [0; 8];
            random.read_exact(&mut bytes)?;
            let value = u64::from_ne_bytes(bytes);
            if value < ceiling {
                break (value % n) as usize;
            }
        };
        items.swap(i, choice);
    }
    Ok(())
}

fn spawn_mpv_reader(stream: UnixStream, sender: Sender<Event>) {
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            for line in BufReader::new(stream).lines() {
                let message: Value = serde_json::from_str(&line?)?;
                if sender.send(Event::Mpv(message)).is_err() {
                    return Ok(());
                }
            }
            bail!("mpv closed its IPC connection")
        })();
        if let Err(error) = result {
            let _ = sender.send(Event::Disconnected(format!("{error:#}")));
        }
    });
}

fn spawn_clients(listener: UnixListener, sender: Sender<Event>) {
    thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(stream) = connection else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let parsed = (|| -> Result<Value> {
                let mut line = String::new();
                BufReader::new(stream.try_clone()?).read_line(&mut line)?;
                Ok(serde_json::from_str(&line)?)
            })();
            match parsed {
                Ok(request) => {
                    let client = Client {
                        command: request.get("command").cloned().unwrap_or(Value::Null),
                        request_id: request.get("request_id").cloned().unwrap_or(json!(0)),
                        stream,
                    };
                    if sender.send(Event::Client(client)).is_err() {
                        break;
                    }
                }
                Err(error) => Client {
                    command: Value::Null,
                    request_id: json!(0),
                    stream,
                }
                .reply(Err(error)),
            }
        }
    });
}

impl Session {
    fn send(&mut self, command: &Value, target: Option<Target>) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        serde_json::to_writer(
            &mut self.writer,
            &json!({"command":command,"request_id":id}),
        )?;
        self.writer.write_all(b"\n")?;
        if let Some(target) = target {
            self.pending.insert(
                id,
                Pending {
                    target,
                    since: Instant::now(),
                },
            );
        }
        Ok(())
    }

    fn event_loop(&mut self, player: &mut Player) -> Result<()> {
        while !self.finished {
            let now = Instant::now();
            if !self.replaying {
                if let Some(action) = self.scheduler.tick(now) {
                    self.execute(action)?;
                }
            }
            if self.finished {
                break;
            }
            self.navigate()?;
            if self.finished {
                break;
            }
            let timeout = if self.replaying {
                Duration::from_secs(1)
            } else {
                self.scheduler
                    .wait(Instant::now())
                    .unwrap_or(Duration::from_secs(1))
                    .min(Duration::from_secs(1))
            };
            match self.receiver.recv_timeout(timeout) {
                Ok(Event::Client(client)) => self.client(client)?,
                Ok(Event::Mpv(message)) => self.mpv(message)?,
                Ok(Event::Disconnected(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(error) => return Err(error.into()),
            }
            if !self.finished {
                if let Some(status) = player.0.try_wait()? {
                    bail!("mpvpaper exited: {status}");
                }
            }
            if self
                .loading_since
                .is_some_and(|time| time.elapsed() > Duration::from_secs(8))
            {
                bail!("video load timed out");
            }
            let expired: Vec<u64> = self
                .pending
                .iter()
                .filter(|(_, p)| p.since.elapsed() > Duration::from_secs(4))
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                match self.pending.remove(&id).unwrap().target {
                    Target::Client(client) => {
                        client.reply(Err(anyhow::anyhow!("mpv command timed out")))
                    }
                    Target::Load | Target::Replay => bail!("mpv playback transition timed out"),
                    Target::PauseState(_) => bail!("mpv pause query timed out"),
                }
            }
        }
        Ok(())
    }

    fn execute(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Load(index) => {
                self.at_eof = false;
                self.loading_since = Some(Instant::now());
                self.send(
                    &json!(["loadfile", self.items[index], "replace"]),
                    Some(Target::Load),
                )?;
            }
            Action::Replay => {
                self.replaying = true;
                self.send(&json!(["seek", 0, "absolute+exact"]), Some(Target::Replay))?;
            }
            Action::Finish => {
                self.finished = true;
                if let Some(client) = self.active_navigation.take() {
                    client.reply(Ok(Value::Null));
                }
            }
        }
        Ok(())
    }

    fn mpv(&mut self, message: Value) -> Result<()> {
        if let Some(id) = message["request_id"].as_u64() {
            if let Some(pending) = self.pending.remove(&id) {
                let success = message["error"] == "success";
                match pending.target {
                    Target::Client(client) => {
                        if success {
                            client.reply(Ok(message.get("data").cloned().unwrap_or(Value::Null)));
                        } else {
                            client.reply(Err(anyhow::anyhow!("mpv: {}", message["error"])));
                        }
                    }
                    Target::Load => ensure!(success, "load video: {}", message["error"]),
                    Target::Replay => {
                        ensure!(success, "restart video: {}", message["error"]);
                        self.replaying = false;
                        self.at_eof = false;
                        self.pause_epoch += 1;
                        self.send(
                            &json!([
                                "set_property",
                                "pause",
                                self.user_paused || self.playback_paused
                            ]),
                            None,
                        )?;
                        if let Some(client) = self.active_navigation.take() {
                            client.reply(Ok(Value::Null));
                        }
                    }
                    Target::PauseState(epoch) => {
                        ensure!(success, "query playback pause: {}", message["error"]);
                        if epoch == self.pause_epoch
                            && !self.at_eof
                            && !self.replaying
                            && !self.scheduler.loading()
                        {
                            if let Some(paused) = message["data"].as_bool() {
                                self.playback_paused = paused;
                                self.scheduler.set_paused(
                                    paused && !self.change_while_paused,
                                    Instant::now(),
                                );
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        match message["event"].as_str() {
            Some("file-loaded") if self.scheduler.loading() => {
                self.scheduler.file_loaded(Instant::now());
                self.loading_since = None;
                self.at_eof = false;
                self.pause_epoch += 1;
                self.send(
                    &json!([
                        "set_property",
                        "pause",
                        self.user_paused || self.playback_paused
                    ]),
                    None,
                )?;
                if let Some(client) = self.active_navigation.take() {
                    client.reply(Ok(Value::Null));
                }
            }
            Some("property-change") => match message["name"].as_str() {
                Some("eof-reached") => {
                    if let Some(eof) = message["data"].as_bool() {
                        let changed = eof && !self.at_eof;
                        self.at_eof = eof;
                        if changed && !self.replaying {
                            if let Some(action) = self.scheduler.eof(Instant::now()) {
                                self.execute(action)?;
                            }
                        }
                    }
                }
                Some("pause") => {
                    // Read current state in command order; stale notifications are not user intent.
                    self.send(
                        &json!(["get_property", "pause"]),
                        Some(Target::PauseState(self.pause_epoch)),
                    )?;
                }
                Some(name @ ("loop-file" | "loop-playlist" | "keep-open-pause")) => {
                    let value = &message["data"];
                    if !value.is_null() && value != false && value != "no" {
                        self.send(&json!(["set_property", name, "no"]), None)?;
                    }
                }
                _ => (),
            },
            Some("end-file") if message["reason"] == "error" => {
                bail!("video playback failed: {message}")
            }
            Some("shutdown") => {
                self.finished = true;
            }
            _ => (),
        }
        Ok(())
    }

    fn client(&mut self, mut client: Client) -> Result<()> {
        let Some(command) = client.command.as_array() else {
            client.reply(Err(anyhow::anyhow!("command must be an array")));
            return Ok(());
        };
        let Some(name) = command.first().and_then(Value::as_str) else {
            client.reply(Err(anyhow::anyhow!("command name is missing")));
            return Ok(());
        };
        let property = command.get(1).and_then(Value::as_str);
        if matches!(name, "quit" | "quit-watch-later") {
            client.reply(Ok(Value::Null));
            self.finished = true;
            return Ok(());
        }
        if matches!(
            name,
            "playlist-next" | "playlist-prev" | "playlist-play-index"
        ) || (matches!(name, "set" | "set_property")
            && matches!(property, Some("playlist-pos" | "playlist-pos-1")))
        {
            self.navigation.push_back(client);
            return self.navigate();
        }
        if name == "get_property" {
            let value = match property {
                Some("playlist-pos") => Some(json!(self.scheduler.position())),
                Some("playlist-pos-1") => Some(json!(self.scheduler.position() + 1)),
                Some("playlist-count") => Some(json!(self.items.len())),
                Some("playlist") => Some(Value::Array(self.items.iter().enumerate().map(|(i,path)| json!({
                    "filename":path,"id":i+1,"current":i==self.scheduler.position(),"playing":i==self.scheduler.position()
                })).collect())),
                _ => None,
            };
            if let Some(value) = value {
                client.reply(Ok(value));
                return Ok(());
            }
        }
        if let Some(error) = managed_command_error(command) {
            client.reply(Err(anyhow::anyhow!(error)));
            return Ok(());
        }
        if matches!(name, "set" | "set_property" | "cycle") && property == Some("pause") {
            let paused = if name == "cycle" {
                Some(!self.user_paused)
            } else {
                command.get(2).and_then(parse_bool)
            };
            let Some(paused) = paused else {
                client.reply(Err(anyhow::anyhow!("pause requires true/false or yes/no")));
                return Ok(());
            };
            self.user_paused = paused;
            self.pause_epoch += 1;
            self.playback_paused = paused;
            self.scheduler
                .set_paused(paused && !self.change_while_paused, Instant::now());
            client.command = json!(["set_property", "pause", paused]);
        }
        // The request ID belongs to the private connection; clients keep their own IDs.
        let id = self.next_id;
        self.send(&client.command, None)?;
        self.pending.insert(
            id,
            Pending {
                target: Target::Client(client),
                since: Instant::now(),
            },
        );
        Ok(())
    }

    fn navigate(&mut self) -> Result<()> {
        if self.finished
            || self.scheduler.loading()
            || self.replaying
            || self.active_navigation.is_some()
        {
            return Ok(());
        }
        while let Some(client) = self.navigation.pop_front() {
            let command = client.command.as_array().unwrap();
            let name = command[0].as_str().unwrap();
            let action = match name {
                "playlist-next" => Ok(self.scheduler.advance(Instant::now())),
                "playlist-prev" => Ok(self.scheduler.previous(Instant::now())),
                _ => {
                    let value = command.get(if name == "playlist-play-index" { 1 } else { 2 });
                    let position = value
                        .and_then(|v| {
                            v.as_u64()
                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        })
                        .and_then(|n| usize::try_from(n).ok());
                    let position =
                        if command.get(1).and_then(Value::as_str) == Some("playlist-pos-1") {
                            position.and_then(|p| p.checked_sub(1))
                        } else {
                            position
                        };
                    match position {
                        Some(p) => self.scheduler.select(p, Instant::now()),
                        None => Err(anyhow::anyhow!("invalid playlist position")),
                    }
                }
            };
            match action {
                Ok(action) => {
                    self.active_navigation = Some(client);
                    self.execute(action)?;
                    break;
                }
                Err(error) => client.reply(Err(error)),
            }
        }
        Ok(())
    }
}

fn parse_bool(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| match value.as_str() {
        Some("yes" | "true" | "1") => Some(true),
        Some("no" | "false" | "0") => Some(false),
        _ => None,
    })
}

fn managed_command_error(command: &[Value]) -> Option<&'static str> {
    let name = command.first()?.as_str()?;
    let property = command
        .get(1)
        .and_then(Value::as_str)
        .map(|p| p.strip_prefix("options/").unwrap_or(p));
    if matches!(
        name,
        "loadfile"
            | "loadlist"
            | "playlist-clear"
            | "playlist-remove"
            | "playlist-move"
            | "playlist-shuffle"
            | "playlist-unshuffle"
            | "stop"
    ) {
        return Some("playlist is owned by waywe; use play, next, prev, or stop");
    }
    if matches!(
        name,
        "set" | "set_property" | "cycle" | "add" | "multiply" | "cycle-values"
    ) && matches!(
        property,
        Some(
            "loop-file"
                | "loop-playlist"
                | "shuffle"
                | "idle"
                | "keep-open"
                | "keep-open-pause"
                | "input-ipc-server"
                | "playlist-pos"
                | "playlist-pos-1"
        )
    ) {
        return Some("playback lifecycle and looping are owned by waywe");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_commands_cannot_bypass_rust_playlist_ownership() {
        for command in [
            json!(["loadfile", "other.mp4"]),
            json!(["set", "loop-file", "inf"]),
            json!(["cycle", "shuffle"]),
            json!(["set_property", "keep-open", false]),
            json!(["set_property", "options/loop-file", "inf"]),
            json!(["add", "playlist-pos", 1]),
        ] {
            assert!(managed_command_error(command.as_array().unwrap()).is_some());
        }
        for command in [
            json!(["get_property", "loop-file"]),
            json!(["set", "volume", "30"]),
            json!(["apply-profile", "fast"]),
        ] {
            assert!(managed_command_error(command.as_array().unwrap()).is_none());
        }
    }

    #[test]
    fn eof_pause_reply_cannot_pause_restarted_video() {
        let (writer, _player) = UnixStream::pair().unwrap();
        let (_sender, receiver) = mpsc::channel();
        let mut session = Session {
            items: vec!["short.mp4".into()],
            scheduler: Scheduler::new(1, None, true, true),
            writer,
            receiver,
            pending: HashMap::new(),
            navigation: VecDeque::new(),
            active_navigation: None,
            next_id: 10,
            user_paused: false,
            playback_paused: false,
            change_while_paused: false,
            pause_epoch: 0,
            at_eof: false,
            replaying: false,
            loading_since: None,
            finished: false,
        };
        session.scheduler.file_loaded(Instant::now());
        session
            .mpv(json!({"event":"property-change","name":"eof-reached","data":true}))
            .unwrap();
        session
            .mpv(json!({"event":"property-change","name":"pause","data":true}))
            .unwrap();
        session
            .mpv(json!({"request_id":10,"error":"success"}))
            .unwrap();
        session
            .mpv(json!({"request_id":11,"error":"success","data":true}))
            .unwrap();
        assert!(!session.user_paused);
        assert!(!session.playback_paused);
        assert_eq!(session.scheduler.eof(Instant::now()), Some(Action::Replay));
    }
}
