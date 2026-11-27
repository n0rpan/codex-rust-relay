//! One session-bus connection for all Linux alerts and activation signals.
use flume::{Receiver, Sender};
use futures_lite::{StreamExt, future};
use std::collections::HashMap;
use zbus::{Connection, Message, Proxy, fdo::DBusProxy, zvariant::Value};

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
pub(super) enum Command {
    Show {
        tag: String,
        title: String,
        body: String,
    },
    Close(String),
}

pub(super) fn start(clicks: Sender<String>) -> Sender<Command> {
    let (tx, rx) = flume::unbounded();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            // If a desktop has no notification service, retry on the next event.
            let _ = future::block_on(serve(first, &rx, &clicks));
        }
    });
    tx
}

async fn serve(
    first: Command,
    commands: &Receiver<Command>,
    clicks: &Sender<String>,
) -> zbus::Result<()> {
    let bus = Connection::session().await?;
    let service = Proxy::new(&bus, SERVICE, PATH, SERVICE).await?;
    let dbus = DBusProxy::new(&bus).await?;
    let mut signals = service.receive_all_signals().await?;
    // zbus applies backpressure when a signal subscription fills up. Keep
    // polling it even while the command loop is waiting for a method reply.
    let (tx, rx) = flume::unbounded();
    let drain = async move {
        while let Some(message) = signals.next().await {
            if tx.send(message).is_err() {
                break;
            }
        }
        drop(tx);
    };
    future::race(
        run(&bus, &service, &dbus, first, commands, clicks, rx),
        async {
            drain.await;
            std::future::pending().await
        },
    )
    .await
}

async fn run(
    bus: &Connection,
    service: &Proxy<'_>,
    dbus: &DBusProxy<'_>,
    first: Command,
    commands: &Receiver<Command>,
    clicks: &Sender<String>,
    signals: Receiver<Message>,
) -> zbus::Result<()> {
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut owner = String::new();
    enum Event {
        Command(Command),
        Signal(Message),
        Closed,
    }
    let mut event = Event::Command(first);
    loop {
        match event {
            Event::Command(Command::Close(ref tag)) if !ids.contains_key(tag) => (),
            Event::Command(cmd) => {
                // Activate on demand and address the unique owner. Numeric IDs may
                // be reused by a restarted notification daemon.
                let capabilities: Vec<String> = service.call("GetCapabilities", &()).await?;
                let current = dbus.get_name_owner(SERVICE.try_into()?).await?;
                if owner != current.as_str() {
                    ids.clear();
                    owner = current.to_string();
                }
                let peer = Proxy::new(bus, current, PATH, SERVICE).await?;
                match cmd {
                    Command::Show { tag, title, body } => {
                        let body = if capabilities.iter().any(|c| c == "body-markup") {
                            body.replace('&', "&amp;")
                                .replace('<', "&lt;")
                                .replace('>', "&gt;")
                        } else {
                            body
                        };
                        let hints =
                            HashMap::from([("desktop-entry", Value::from("dev.ox.desktop"))]);
                        let id: u32 = peer
                            .call(
                                "Notify",
                                &(
                                    "OX Desktop",
                                    ids.get(&tag).copied().unwrap_or(0),
                                    "dev.ox.desktop",
                                    title,
                                    body,
                                    ["default", "Open"],
                                    hints,
                                    10_000i32,
                                ),
                            )
                            .await?;
                        ids.insert(tag, id);
                    }
                    Command::Close(tag) => {
                        if let Some(id) = ids.remove(&tag) {
                            let _: zbus::Result<()> = peer.call("CloseNotification", &(id,)).await;
                        }
                    }
                }
            }
            Event::Signal(msg) if msg.header().sender().is_some_and(|s| s.as_str() == owner) => {
                let header = msg.header();
                match header.member().map(|m| m.as_str()) {
                    Some("ActionInvoked") => {
                        if let Ok((id, action)) = msg.body().deserialize::<(u32, String)>()
                            && action == "default"
                            && let Some((tag, _)) = ids.iter().find(|(_, n)| **n == id)
                        {
                            let _ = clicks.send(tag.clone());
                        }
                    }
                    Some("NotificationClosed") => {
                        if let Ok((id, _)) = msg.body().deserialize::<(u32, u32)>() {
                            ids.retain(|_, v| *v != id);
                        }
                    }
                    _ => (),
                }
            }
            Event::Signal(_) => (),
            Event::Closed => return Ok(()),
        }
        event = future::or(
            async {
                signals
                    .recv_async()
                    .await
                    .map_or(Event::Closed, Event::Signal)
            },
            async {
                commands
                    .recv_async()
                    .await
                    .map_or(Event::Closed, Event::Command)
            },
        )
        .await;
    }
}
