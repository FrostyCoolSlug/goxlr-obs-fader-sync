# GoXLR -> OBS Fader Sync

This project is primarily an API example for the [GoXLR Utility](https://github.com/GoXLR-on-Linux/goxlr-utility), and
it's API.

The goal is to automatically have OBS adjust the volume of an Audio Output source based on the volume of a GoXLR
Channel. This is achieved by utilising the GoXLR Utility to get reports on volume changes, then using the
obs-websocket plugin to update the stream volume.

## Minimum Requirements
* OBS 28.0
* obs-websocket 5.0.0
* goxlr-utility 0.8.0
* Rust and Cargo

Note that OBS may already have the websocket plugin included.

## Setup
Firstly, Set up an Audio Output source pointed to the Channel of the GoXLR you would like to sync (I have a source 
called `Music`).

Then, Ensure you have the OBS websocket configured via the Tools -> obs-websocket menu. Make sure `Enable WebSocket 
server` is checked, then hitting `Show Connect Info` should provide you with the details you'll need to set this up,
so make a note.

At the top of `main.rs`, there are several constants which will need updating (sorry, no UI or fanciness yet):
```rust
static OBS_HOST: &str = "localhost";
static OBS_PORT: u16 = 4455;
static OBS_PASS: &str = "";
```

Below this, is a mapping list, which will map a Channel on the GoXLR to an Audio Source in OBS, you can define as few
or as many as you want, but there's no sanity checking, so if you overlap them there may be problems!
```rust
static CHANNEL_MAPPING: Lazy<HashMap<Channels, &str>> = Lazy::new(||
    HashMap::from([
        // Change or add Channel Mappings below.
        // Format: (GoXLRChannel, OBSSource),
        (Channels::Music, "Music"),
        (Channels::System, "System")
    ])
);
```

Update the values prefixed `OBS_` with the details which you aquired earlier.

Update the values prefixed `GOXLR_` with values related to the GoXLR utility. Unless you're doing something special,
you should only need to update `GOXLR_CHANNEL`.

The `Music` channel is the default on this app, but any of the following `ChannelName`s are acceptable (case-sensitive):

    Mic
    LineIn
    Console
    System
    Game
    Chat
    Sample
    Music
    Headphones
    MicMonitor
    LineOut

Once that's all done, you should be able to run `cargo install --path .` to compile and install the app, then run 
`goxlr-obs-fader-sync` from the command line to start. 

You should get some output which looks like the following:
```
Attempting to Connect to OBS..
OBS Connection Established, Attempting to connect to GoXLR Utility..
Connecting to GoXLR Socket..
Connected to GoXLR..
```

And that's it! Your GoXLR fader should adjust your OBS audio source.

## Notes
This is primarily a proof of concept, and may not receive any additional changes or updates outside of fixing any, API
changes which may occur in the future. I will accept pull requests if they make sense, and don't overly complicate the
goal of this being an example.
