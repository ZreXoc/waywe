var options = {
    delay : 1800
};
mp.options.read_options(options,"waywe");

var delay  = options.delay;

// loop video to the given delay.
// e.g. if delay=50 and the length of the video is 15s, it will be looped for 3 times to reach 50s, that's 15*(1+3)=60s in total.
// add script-opts=main-delay=<time> to your mpv.conf or add --script-opts=main-delay=<time> in command line.

print('[delay] '+ delay +' seconds');

// runs every time a new file is loaded.
if (delay)
    mp.register_event("file-loaded", function () {
        duration = mp.get_property_native("duration");
        if (!duration) return;
        mp.set_property("loop-file", Math.floor(delay / duration));
    });
