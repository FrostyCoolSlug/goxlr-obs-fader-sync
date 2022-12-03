use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use goxlr_ipc::{DaemonRequest, DaemonResponse, DaemonStatus, WebsocketRequest, WebsocketResponse};
use goxlr_types::ChannelName;

use obws::requests::inputs::Volume;
use obws::Client;
use tokio::sync::mpsc::{channel, Sender};
use tokio::{select, task};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

static OBS_HOST: &str = "localhost";
static OBS_PORT: u16 = 4455;
static OBS_PASS: &str = "";
static OBS_AUDIO_SOURCE: &str = "Music";

static GOXLR_HOST: &str = "localhost";
static GOXLR_PORT: u16 = 14564;
static GOXLR_CHANNEL: ChannelName = ChannelName::Music;

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
                if let Some(volume) = result {
                    let mul = volume as f32 / 255.;

                    // Update OBS..
                    client.inputs().set_volume(OBS_AUDIO_SOURCE, Volume::Mul(mul)).await?;
                }
            },
        };
    }
}

async fn sync_goxlr(sender: Sender<u8>) -> Result<()> {
    println!("Connecting to GoXLR Socket..");
    let mut daemon_status = DaemonStatus::default();

    let url = format!("ws://{}:{}/api/websocket", GOXLR_HOST, GOXLR_PORT);
    let url = Url::parse(&url).expect("Bad URL Provided");
    let (mut ws_stream, _) = connect_async(url).await?;

    println!("Connected to GoXLR..");
    let initial_message = WebsocketRequest {
        id: 0,
        data: DaemonRequest::GetStatus
    };

    let message = Message::Text(serde_json::to_string(&initial_message)?);

    let mut last_volume = 0;
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
                                        sender.send(volume).await?;
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
                                            sender.send(volume).await?;
                                            last_volume = volume;
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
