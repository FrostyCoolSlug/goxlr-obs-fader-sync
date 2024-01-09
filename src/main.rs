use std::collections::HashMap;
use std::hash::{Hash};
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use goxlr_ipc::clients::ipc::ipc_socket::Socket;
use goxlr_ipc::{DaemonRequest, DaemonResponse, DaemonStatus, WebsocketRequest, WebsocketResponse};
use goxlr_ipc::client::Client as GoXLRClient;
use goxlr_ipc::clients::ipc::ipc_client::IPCClient;
use goxlr_types::{ChannelName, FaderName, MuteFunction, MuteState};
use interprocess::local_socket::tokio::LocalSocketStream;
use interprocess::local_socket::NameTypeSupport;
use strum::IntoEnumIterator;

use obws::requests::inputs::Volume;
use obws::Client;
use once_cell::sync::Lazy;
use tokio::sync::mpsc::{channel, Sender};
use tokio::{select, task};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::OBSMessages::{SetMuted, SetVolume};

// Change these..
static OBS_HOST: &str = "localhost";
static OBS_PORT: u16 = 4455;
static OBS_PASS: &str = "wVhgI4fvB8OfQ2wz";

static CHANNEL_MAPPING: Lazy<HashMap<Channels, &str>> = Lazy::new(||
    HashMap::from([
        // Change or add Channel Mappings below.
        (Channels::Music, "Music"),
        (Channels::System, "System")
    ])
);

// Leave these alone :)
static GOXLR_SOCKET_PATH: &str = "/tmp/goxlr.socket";
static GOXLR_NAMED_PIPE: &str = "@goxlr.socket";

/*
    This is slightly obnoxious, I don't derive Hash for the ChannelName enum in the utility, so
    they can't be directly used, I'll fix this for 1.0.5, but for now, we'll re-declare and map.
 */
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Channels {
    Mic,
    LineIn,
    Console,
    System,
    Game,
    Chat,
    Sample,
    Music,
    Headphones,
    MicMonitor,
    LineOut,
}

impl Channels {
    fn get_channel_name(&self) -> ChannelName {
        match self {
            Channels::Mic => ChannelName::Mic,
            Channels::LineIn => ChannelName::LineIn,
            Channels::Console => ChannelName::Console,
            Channels::System => ChannelName::System,
            Channels::Game => ChannelName::Game,
            Channels::Chat => ChannelName::Chat,
            Channels::Sample => ChannelName::Sample,
            Channels::Music => ChannelName::Music,
            Channels::Headphones => ChannelName::Headphones,
            Channels::MicMonitor => ChannelName::MicMonitor,
            Channels::LineOut => ChannelName::LineOut,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Attempting to Connect to OBS..");
    let password = if !OBS_PASS.is_empty() {
        Some(OBS_PASS)
    } else {
        None
    };

    let client = Client::connect(OBS_HOST, OBS_PORT, password).await?;

    println!("OBS Connection Established, Attempting to connect to GoXLR Utility..");
    //let (goxlr_err_tx, goxlr_err_rx) = oneshot::channel();
    let (goxlr_tx, mut goxlr_rx) = channel(10);
    task::spawn(sync_goxlr(goxlr_tx));

    loop {
        select! {
            result = goxlr_rx.recv() => {
                if let Some(result) = result {
                    match result {
                        SetVolume(channel, volume) => {
                            // Convert the Percent into an OBS DB rating..
                            let volume = if volume == 0 {
                                -100.
                            } else {
                                // This *MOSTLY* matches audio through the range, at the extreme quiet end
                                // OBS is marginally louder than the GoXLR, but otherwise the volumes
                                // pretty much match... at least to my ears!

                                // This was tested by enabling the Monitor in OBS for the Music channel,
                                // configuring the GoXLR to mute to Phones, then switching between the two
                                // while changing the volumes until they sounded similar.. It's not
                                // scientific, YMMV.

                                // Either way, GoXLR floor seems to be around -60db, so convert our volume
                                // into that range, and send it to OBS.
                                -100. + ((volume as f32 / 255.) * 60.) + 40.
                            };

                            // Update OBS..
                            client.inputs().set_volume(&channel, Volume::Db(volume)).await?;
                        }
                        SetMuted(channel, muted) => {
                            client.inputs().set_muted(&channel, muted).await?;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
enum OBSMessages {
    SetVolume(String, u8),
    SetMuted(String, bool),
}

async fn sync_goxlr(sender: Sender<OBSMessages>) -> Result<()> {
    println!("Determining Websocket Location..");
    let address = get_websocket_address().await;

    println!("Connecting to GoXLR Websocket..");
    let url = Url::parse(&address).expect("Bad URL Provided");

    // Handle the Connection..
    let connection = connect_async(url).await;
    if let Err(error) = connection {
        //err_sender.send(format!("Error Connection to GoXLR: {:?}", error)).await;
        bail!(error);
    }
    let (mut ws_stream, _) = connection.unwrap();

    println!("Connected to GoXLR..");
    let mut daemon_status = DaemonStatus::default();
    let initial_message = WebsocketRequest {
        id: 0,
        data: DaemonRequest::GetStatus,
    };

    let message = Message::Text(serde_json::to_string(&initial_message)?);

    let mut last_volume_map: HashMap<Channels, u8> = Default::default();
    let mut is_muted_map: HashMap<Channels, bool> = Default::default();

    ws_stream.send(message).await?;
    loop {
        select! {
            msg = ws_stream.next() => {
                if let Some(msg) = msg {
                    let msg = msg?;
                    if msg.is_text() {
                        let result = serde_json::from_str::<WebsocketResponse>(msg.to_text()?);

                        if let Ok(result) = result {
                            match result.data {
                                DaemonResponse::Error(err) => {
                                    eprintln!("Error From GoXLR Utility: {:?}", err);
                                }
                                DaemonResponse::Status(status) => {
                                    // Force replace the status..
                                    daemon_status = status;

                                    // Force a Volume Update to OBS..
                                    if let Some(mixer) = daemon_status.mixers.values().next() {
                                        for key in CHANNEL_MAPPING.keys() {
                                            let volume = mixer.get_channel_volume(key.get_channel_name());
                                            last_volume_map.insert(*key, volume);

                                            let obs_channel = String::from(CHANNEL_MAPPING[key]);
                                            println!("Sending initial Volume Level for {}..", obs_channel);
                                            sender.send(OBSMessages::SetVolume(obs_channel.clone(), volume)).await?;
                                            println!("Initial Volume Sent..");

                                            // Firstly, are we attached to a fader?
                                            for fader in FaderName::iter() {
                                                if mixer.fader_status[fader].channel == key.get_channel_name() {
                                                    let mute_type = mixer.fader_status[fader].mute_type;
                                                    let state = mixer.fader_status[fader].mute_state;

                                                    // Here we go again :D
                                                    if ((mute_type == MuteFunction::ToStream || mute_type == MuteFunction::All) && state == MuteState::MutedToX)
                                                    || (state == MuteState::MutedToAll) {
                                                        is_muted_map.insert(*key, true);
                                                        println!("Sending Initial Mute state to Muted");
                                                        sender.send(OBSMessages::SetMuted(obs_channel.clone(), true)).await?;
                                                        println!("Initial Mute state sent..");
                                                    } else {
                                                       // Not muted anymore..
                                                        is_muted_map.insert(*key, false);
                                                        println!("Sending Initial Mute state to Unmuted");
                                                        sender.send(OBSMessages::SetMuted(obs_channel.clone(), false)).await?;
                                                        println!("Initial Mute state sent..");
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                DaemonResponse::Patch(patch) => {
                                    let mut old = serde_json::to_value(&daemon_status)?;
                                    json_patch::patch(&mut old, &patch)?;

                                    let result = serde_json::from_value(old);
                                    match result {
                                        Ok(status) => daemon_status = status,
                                        Err(error) => println!("Failed to Serialise: {:?}", error),
                                    }

                                    // This *WILL* go weird if you have more than one GoXLR!
                                    if let Some(mixer) = daemon_status.mixers.values().next() {
                                        for key in CHANNEL_MAPPING.keys() {
                                            let volume = mixer.get_channel_volume(key.get_channel_name());

                                            let obs_channel = String::from(CHANNEL_MAPPING[key]);
                                            if volume != last_volume_map[key] {
                                                sender.send(OBSMessages::SetVolume(obs_channel.clone(), volume)).await?;
                                                last_volume_map.insert(*key, volume);
                                            }

                                            for fader in FaderName::iter() {
                                                if mixer.fader_status[fader].channel == key.get_channel_name() {
                                                    let mute_type = mixer.fader_status[fader].mute_type;
                                                    let state = mixer.fader_status[fader].mute_state;

                                                    if ((mute_type == MuteFunction::ToStream || mute_type == MuteFunction::All) && state == MuteState::MutedToX) || (state == MuteState::MutedToAll) {
                                                        if !is_muted_map[key] {
                                                            is_muted_map.insert(*key, true);
                                                            println!("Setting Channel Muted");
                                                            sender.send(OBSMessages::SetMuted(obs_channel.clone(),true)).await?;
                                                            println!("Mute Update Sent..");
                                                        }
                                                    } else if is_muted_map[key] {
                                                        is_muted_map.insert(*key, false);
                                                        // Not muted anymore..
                                                        println!("Setting Channel Unmuted..");
                                                        sender.send(OBSMessages::SetMuted(obs_channel.clone(), false)).await?;
                                                        println!("Mute Update Sent..");
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn get_websocket_address() -> String {
    let connection = LocalSocketStream::connect(match NameTypeSupport::query() {
        NameTypeSupport::OnlyPaths | NameTypeSupport::Both => GOXLR_SOCKET_PATH,
        NameTypeSupport::OnlyNamespaced => GOXLR_NAMED_PIPE,
    })
        .await
        .expect("Unable to connect to the GoXLR daemon Process");

    let socket: Socket<DaemonResponse, DaemonRequest> = Socket::new(connection);
    let mut client = IPCClient::new(socket);
    client
        .poll_status()
        .await
        .expect("Unable to fetch HTTP Status");

    let status = client.http_status();
    if !status.enabled {
        panic!("Websocket is disabled, unable to proceed");
    }

    let mut address = String::from("localhost");
    if status.bind_address != "0.0.0.0" && status.bind_address != "localhost" {
        address = status.bind_address.clone();
    }

    format!("ws://{}:{}/api/websocket", address, status.port)
}
