use anyhow::{Result};
use futures_util::{SinkExt, StreamExt};
use goxlr_ipc::{DaemonRequest, DaemonResponse, DaemonStatus, WebsocketRequest, WebsocketResponse};
use goxlr_ipc::ipc_socket::Socket;
use goxlr_types::ChannelName;
use interprocess::local_socket::NameTypeSupport;
use interprocess::local_socket::tokio::LocalSocketStream;

use obws::requests::inputs::Volume;
use obws::Client;
use tokio::sync::mpsc::{channel, Sender};
use tokio::{select, task};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// Change these..
static OBS_HOST: &str = "localhost";
static OBS_PORT: u16 = 4455;
static OBS_PASS: &str = "";
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
                if let Some(volume) = result {
                    // Convert the Percent into an OBS DB rating..
                    let volume = -100. + (((volume as f32 / 255.) * 100.));

                    // TODO: This isn't *QUITE* right
                    // From a volume perspective, OBS cuts out at around -50db, while we can still
                    // hear audio from the GoXLR so the 'lower end' of this may need tweaking to
                    // better represent what the user can hear.

                    // For now, this is close enough!

                    // Update OBS..
                    client.inputs().set_volume(OBS_AUDIO_SOURCE, Volume::Db(volume)).await?;
                }
            },
        };
    }
}

async fn sync_goxlr(sender: Sender<u8>) -> Result<()> {
    println!("Determining Websocket Location..");
    let address = get_websocket_address().await;

    println!("Connecting to GoXLR Websocket..");
    let url = Url::parse(&address).expect("Bad URL Provided");
    let (mut ws_stream, _) = connect_async(url).await?;

    println!("Connected to GoXLR..");
    let mut daemon_status = DaemonStatus::default();
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

async fn get_websocket_address() -> String {
    let connection = LocalSocketStream::connect(match NameTypeSupport::query() {
        NameTypeSupport::OnlyPaths | NameTypeSupport::Both => GOXLR_SOCKET_PATH,
        NameTypeSupport::OnlyNamespaced => GOXLR_NAMED_PIPE,
    })
        .await
        .expect("Unable to connect to the GoXLR daemon Process");

    let socket: Socket<DaemonResponse, DaemonRequest> = Socket::new(connection);
    let mut client = goxlr_ipc::client::Client::new(socket);
    client.poll_http_status().await.expect("Unable to fetch HTTP Status");

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
