#! /usr/bin/env node
import path, { dirname } from "path";
import os from "os";
import fs from "fs";
import childProcess from "child_process";
import { program } from "commander";
import shell from "shelljs";
import { select } from "@inquirer/prompts";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const validExt = [".mp4"];
const waywePlaylistPath = "/tmp/wayme-playlist";
const wayweSocket = "/tmp/waywe-socket";

program.name("waywe").version("0.1.0");

program
    .command("start")
    .option("-p, --playlist <name>", "name of playlist")
    .option(
        "--steamroot <path>",
        "path to steam",
        path.resolve(os.homedir(), ".local/share/Steam")
    )
    .option("-o, --mpv-options <args>", "forward mpv options")
    .action(startWallpaper);

program
    .command("send")
    .argument("<command>", "send mpv Input Command to socket")
    .action((command) => sendMpvCommand(command));

program
    .command("attach")
    .description("attach to mpv interactive control")
    .action(() => {
        //shell.exec("tmux attach -t waywe")
        childProcess.spawn("tmux", ["attach", "-t", "waywe"], {
            detached: true,
            stdio: "inherit",
        });
    });

// mpv commands
[
    ["stop", "quit 0", "stop playing wallpaper"],
    ["unpause", "set pause no"],
    ["pause", "set pause yes"],
    ["next", "playlist-next", "next wallpaper"],
    ["preview", "playlist-prev", "previous wallpaper"],
].forEach(([command, mpvCommand]) => {
    program.command(command).action(() => sendMpvCommand(mpvCommand));
});

//TODO change Playlist

program.parseAsync();

async function startWallpaper(options, command) {
    console.log(options.playlist);
    const { steamroot: steamRoot } = options;
    const weRoot = path.resolve(steamRoot, "steamapps/common/wallpaper_engine");
    const weConfig = JSON.parse(
        fs.readFileSync(path.resolve(weRoot, "config.json"), "utf8")
    );

    const playlists = modifyPlaylist(weConfig.steamuser.general.playlists);
    const playlist =
        playlists.find((pl) => pl.name === options.playlist) ??
        (await select({
            message: "Select a playlist",
            choices: playlists.map(({ items, name }, index) => ({
                name: `${name}\n\t(${previewPlaylist(items)})`,
                value: playlists[index],
                description: "choose playlist from wallpaper engine",
            })),
        }));
    fs.writeFileSync(waywePlaylistPath, playlist.items.join("\n"));
    runMpvpaper(playlist, options);
}

function modifyPlaylist(playlists) {
    for (let i = 0; i < playlists.length; i++) {
        const pl = playlists[i];
        pl.items = pl.items
            .filter((item) => validExt.includes(path.extname(item))) // filter extention
            .map((item) => item.replace(/^Z:/, "")); // remove drive name 'Z:'
    }

    return playlists;
}
function previewPlaylist(items) {
    if (items.length > 3) {
        items.splice(3);
        return [...items.map((item) => path.basename(item)), "..."];
    } else {
        return items.map((item) => path.basename(item));
    }
}
async function runMpvpaper(playlist, options) {
    const { settings } = playlist;
    const mpvOptions = [
        options.mpvOptions,
        `--scripts=${path.resolve(__dirname, "./mpv_scripts")}`,
        `--playlist=${waywePlaylistPath}`,
        `input-ipc-server=${wayweSocket}`,
    ];

    if (!settings.beginfirst) mpvOptions.push("save-position-on-quit");
    if (settings.order == "random") mpvOptions.push("shuffle");

    // work with https://github.com/ZreXoc/mpv-loop-until
    if (settings.mode == "timer")
        mpvOptions.push(`--script-opts=delay-delay=${settings.delay * 60}`);

    console.info(`[Running] mpvpaper "*" -o "${mpvOptions.join(" ")}"`);
    //const mpvpaper = child_process.exec(command, () => {});
    /*
     *    const mpvpaper =new (forever.Monitor) (forever.start(
     *        [
     *'mpvpaper','"*"','-o',mpvOptions.join(' ')
     *        ],
     *        {
     *            max: 1,
     *            silent: false,
     *        }
     *    ));

    /*
     *const mpvpaper = child_process.spawn(
     *    "mpvpaper",
     *    ["*", "-o", `${mpvOptions.join(" ")}`],
     *    { stdio: "inherit" }
     *);
     *console.log(mpvpaper.pid);
     */
    /*
     *mpvpaper.stdout.on("data", (chunk) => {
     *    console.log(chunk.toString());
     *});
     *mpvpaper.stderr.on("data", (chunk) => {
     *    //console.log(chunk.toString());
     *});
     */
    /*
     *const child = child_process.spawn("tmux", ["new", "-s", "waywe"], {
     *    detached: true,
     *    stdio: ['inherit', null,null]
     *}).unref();
     */
    /*
     *child_process.spawnSync("tmux", [
     *    "send-keys",
     *    "-t",
     *    "waywe",
     *    `mpvpaper "*" -o "${mpvOptions.join(" ")}"`,
     *    "C-m",
     *]);
     */
    /*
     *child.on("spawn", () => {
     *    shell.exec(
     *        `tmux send-keys -t waywe 'mpvpaper "*" -o "${mpvOptions.join(
     *            " "
     *        )}"' Enter`
     *    );
     *}).disconnect().unref();
     */
    shell.exec("tmux new -s waywe -d");
    shell.exec(
        `tmux send-keys -t waywe 'mpvpaper "*" -o "${mpvOptions.join(
            " "
        )}"' Enter`
    );
    //await onExit(mpvpaper);
}
function onExit(childProcess) {
    return new Promise((resolve, reject) => {
        childProcess.once("exit", (code, signal) => {
            if (code === 0) {
                resolve(undefined);
            } else {
                reject(new Error("Exit with error code: " + code));
            }
        });
        childProcess.once("error", (err) => {
            reject(err);
        });
    });
}

function sendMpvCommand(commands) {
    console.log(commands, `echo "${commands}" | socat - ${wayweSocket}`);
    shell.exec(`echo "${commands}" | socat - ${wayweSocket}`);
}
