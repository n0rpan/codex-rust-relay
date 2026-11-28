//! Local agent-browser transport. Receives viewport frames without sending browser input.
use base64::{Engine, engine::general_purpose::STANDARD};
use gpui::RenderImage;
use serde::Deserialize;
use std::{
    io::{Cursor, ErrorKind},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
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
    let port = crate::platform::browser::preview_port(project, label)?;
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
