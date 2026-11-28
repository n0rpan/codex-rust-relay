//! Local agent-browser transport. Receives viewport frames without sending browser input.
use base64::{Engine, engine::general_purpose::STANDARD};
use gpui::RenderImage;
use serde::Deserialize;
use std::{
    io::{Cursor, ErrorKind},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tungstenite::{Message, client::client_with_config, protocol::WebSocketConfig};

const SOCKET_POLL: Duration = Duration::from_millis(250);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_MESSAGE_BYTES: usize = 6 * 1024 * 1024;
const MAX_FRAME_EDGE: u32 = 4096;

pub(super) enum Update {
    Frame(Arc<RenderImage>),
    Ended(String),
}

pub(super) type Latest = Arc<Mutex<Option<Update>>>;

fn parse_port(endpoint: &str) -> Option<u16> {
    let line = endpoint
        .strip_suffix("\r\n")
        .or_else(|| endpoint.strip_suffix('\n'))
        .unwrap_or(endpoint);
    let port = line.strip_prefix("http://127.0.0.1:")?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

fn locate(project: &Path, label: &str) -> Result<u16, String> {
    let output = Command::new(crate::platform::ox())
        .args(["browser", "--session", label, "preview"])
        .current_dir(project)
        .output()
        .map_err(|error| format!("Could not find OX’s browser component: {error}"))?;
    match output.status.code() {
        Some(0) => std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(parse_port)
            .ok_or_else(|| "OX returned an invalid local browser address.".to_owned()),
        Some(2) => Err("Install the browser component with ox browser install.".into()),
        Some(3) => Err("Open this session’s browser and try again.".into()),
        _ => Err("Could not find the local browser session.".into()),
    }
}

#[derive(Deserialize)]
struct StreamMessage<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    seq: Option<u64>,
    data: Option<&'a str>,
}

fn decode(data: &str) -> Result<Arc<RenderImage>, String> {
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| "Browser returned an invalid frame.")?;
    let mut decoder = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_FRAME_EDGE);
    limits.max_image_height = Some(MAX_FRAME_EDGE);
    limits.max_alloc = Some(80 * 1024 * 1024);
    decoder.limits(limits);
    let mut image = decoder
        .decode()
        .map_err(|_| "Browser frame could not be decoded.")?
        .into_rgba8();
    for pixel in image.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(image)])))
}

fn publish(wake: &flume::Sender<()>, latest: &Latest, update: Update) {
    *latest.lock().unwrap_or_else(|error| error.into_inner()) = Some(update);
    let _ = wake.try_send(());
}

pub(super) fn run(project: &Path, label: &str, wake: &flume::Sender<()>, latest: &Latest) {
    if let Err(error) = receive(project, label, wake, latest)
        && !wake.is_disconnected()
    {
        publish(wake, latest, Update::Ended(error));
    }
}

fn receive(
    project: &Path,
    label: &str,
    wake: &flume::Sender<()>,
    latest: &Latest,
) -> Result<(), String> {
    let port = locate(project, label)?;
    if wake.is_disconnected() {
        return Ok(());
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|_| "The local browser is no longer available.")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let endpoint = format!("ws://{address}/?pacing=ack&maxFps=15");
    let (mut socket, _) = client_with_config(endpoint, stream, Some(config))
        .map_err(|_| "Could not connect to the local browser viewport.")?;
    socket
        .get_mut()
        .set_read_timeout(Some(SOCKET_POLL))
        .map_err(|e| e.to_string())?;
    let mut previous = None;
    let first_frame_deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    while !wake.is_disconnected() {
        if previous.is_none() && Instant::now() >= first_frame_deadline {
            return Err("The local browser did not send a viewport frame.".into());
        }
        let message = match socket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(_) => return Err("The browser viewport disconnected.".into()),
        };
        let Message::Text(message) = message else {
            if matches!(message, Message::Close(_)) {
                return Err("The browser viewport closed.".into());
            }
            continue;
        };
        let Ok(event) = serde_json::from_str::<StreamMessage>(&message) else {
            continue;
        };
        if event.kind == "frame"
            && let (Some(seq), Some(data)) = (event.seq, event.data)
            && previous.is_none_or(|previous| seq > previous)
        {
            publish(wake, latest, Update::Frame(decode(data)?));
            previous = Some(seq);
            socket
                .send(Message::Text(
                    format!(r#"{{"type":"ack","seq":{seq}}}"#).into(),
                ))
                .map_err(|_| "The browser viewport disconnected.")?;
        }
    }
    let _ = socket.close(None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_companion_loopback_origin() {
        assert_eq!(parse_port("http://127.0.0.1:5678\n"), Some(5678));
        for value in [
            "http://127.0.0.1:0",
            "http://127.0.0.1:+5678",
            "http://127.0.0.1:5678/",
            "http://127.0.0.1:5678\n\n",
            "http://127.0.0.1:5678\nextra",
            "http://127.0.0.1:5678@external.test",
            "http://localhost:5678",
            "https://127.0.0.1:5678",
        ] {
            assert!(parse_port(value).is_none(), "{value}");
        }
    }

    #[test]
    fn a_delayed_reader_receives_only_the_latest_update() {
        let (wake, incoming) = flume::bounded(1);
        let latest = Arc::new(Mutex::new(None));
        publish(&wake, &latest, Update::Ended("older".into()));
        publish(&wake, &latest, Update::Ended("current".into()));

        assert_eq!(incoming.try_recv(), Ok(()));
        assert!(incoming.try_recv().is_err());
        assert!(matches!(
            latest.lock().unwrap().take(),
            Some(Update::Ended(error)) if error == "current"
        ));
    }
}
