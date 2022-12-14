use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use goxlr_ipc::ipc_socket::Socket;
use goxlr_ipc::{DaemonRequest, DaemonResponse, DaemonStatus, WebsocketRequest, WebsocketResponse};
use goxlr_types::{ChannelName, FaderName, MuteFunction, MuteState};
use interprocess::local_socket::tokio::LocalSocketStream;
use interprocess::local_socket::NameTypeSupport;
use strum::IntoEnumIterator;

use obws::requests::inputs::Volume;
use obws::Client;
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
static OBS_AUDIO_SOURCE: &str = "Music";

static GOXLR_CHANNEL: ChannelName = ChannelName::Music;

// Leave these alone :)
static GOXLR_SOCKET_PATH: &str = "/tmp/goxlr.socket";
static GOXLR_NAMED_PIPE: &str = "@goxlr.socket";

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
    let (goxlr_tx, mut goxlr_rx) = channel(10);
    task::spawn(sync_goxlr(goxlr_tx));

    loop {
        select! {
            result = goxlr_rx.recv() => {
                if let Some(result) = result {
                    match result {
                        SetVolume(volume) => {
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
                            client.inputs().set_volume(OBS_AUDIO_SOURCE, Volume::Db(volume)).await?;
                        }
                        SetMuted(muted) => {
                            client.inputs().set_muted(OBS_AUDIO_SOURCE, muted).await?;
                        }
                    }
                }
            },
        };
    }
}

#[derive(Debug)]
enum OBSMessages {
    SetVolume(u8),
    SetMuted(bool),
}

async fn sync_goxlr(sender: Sender<OBSMessages>) -> Result<()> {
    println!("Determining Websocket Location..");
    let address = get_websocket_address().await;

    println!("Connecting to GoXLR Websocket..");
    let url = Url::parse(&address).expect("Bad URL Provided");
    let (mut ws_stream, _) = connect_async(url).await?;

    println!("Connected to GoXLR..");
    let mut daemon_status = DaemonStatus::default();
    let initial_message = WebsocketRequest {
        id: 0,
        data: DaemonRequest::GetStatus,
    };

    let message = Message::Text(serde_json::to_string(&initial_message)?);

    let mut last_volume = 0;
    let mut is_muted = false;

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
                                DaemonResponse::HttpState(_) => {}
                                DaemonResponse::Ok => {}
                                DaemonResponse::Error(err) => {
                                    eprintln!("Error From GoXLR Utility: {:?}", err);
                                }
                                DaemonResponse::Status(status) => {
                                    // Force replace the status..
                                    daemon_status = status;

                                    // Force a Volume Update to OBS..
                                    if let Some(mixer) = daemon_status.mixers.values().next() {
                                        let volume = mixer.get_channel_volume(GOXLR_CHANNEL);
                                        last_volume = volume;
                                        sender.send(OBSMessages::SetVolume(volume)).await?;

                                        // Firstly, are we attached to a fader?
                                        for fader in FaderName::iter() {
                                            if mixer.fader_status[fader].channel == GOXLR_CHANNEL {
                                                let mute_type = mixer.fader_status[fader].mute_type;
                                                let state = mixer.fader_status[fader].mute_state;

                                                // Here we go again :D
                                                if ((mute_type == MuteFunction::ToStream || mute_type == MuteFunction::All) && state == MuteState::MutedToX)
                                                || (state == MuteState::MutedToAll) {
                                                    is_muted = true;
                                                    sender.send(OBSMessages::SetMuted(true)).await?;
                                                } else {
                                                   // Not muted anymore..
                                                    is_muted = false;
                                                    sender.send(OBSMessages::SetMuted(false)).await?;
                                                }
                                            }
                                        }
                                    }
                                }
                                DaemonResponse::Patch(patch) => {
                                    let mut old = serde_json::to_value(&daemon_status)?;
                                    json_patch::patch(&mut old, &patch)?;
                                    daemon_status = serde_json::from_value(old)?;

                                    // This *WILL* go weird if you have more than one GoXLR!
                                    if let Some(mixer) = daemon_status.mixers.values().next() {
                                        let volume = mixer.get_channel_volume(GOXLR_CHANNEL);
                                        if volume != last_volume {
                                            sender.send(OBSMessages::SetVolume(volume)).await?;
                                            last_volume = volume;
                                        }

                                        for fader in FaderName::iter() {
                                            if mixer.fader_status[fader].channel == GOXLR_CHANNEL {
                                                let mute_type = mixer.fader_status[fader].mute_type;
                                                let state = mixer.fader_status[fader].mute_state;

                                                if ((mute_type == MuteFunction::ToStream || mute_type == MuteFunction::All) && state == MuteState::MutedToX)
                                                || (state == MuteState::MutedToAll) {
                                                    if !is_muted {
                                                        is_muted = true;
                                                        sender.send(OBSMessages::SetMuted(true)).await?;
                                                    }
                                                } else if is_muted {
                                                    // Not muted anymore..
                                                    is_muted = false;
                                                    sender.send(OBSMessages::SetMuted(false)).await?;
                                                }
                                            }
                                        }
                                    }
                                }
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
    let mut client = goxlr_ipc::client::Client::new(socket);
    client
        .poll_http_status()
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
