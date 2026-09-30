//! StatusNotifierItem host (and, when nobody else provides one, watcher).
//!
//! Runs on its own thread with a single-threaded tokio runtime and talks to the
//! UI through channels.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Structure, Value};
use zbus::{Connection, MessageStream, Proxy, interface};

use crate::icon::{self, Icon};
use crate::menu::{self, MenuNode};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passive,
    Active,
    NeedsAttention,
}

#[derive(Clone)]
pub struct TrayItem {
    pub key: String,
    pub title: String,
    pub tooltip: String,
    pub status: Status,
    pub icon: Option<Icon>,
    pub item_is_menu: bool,
    pub menu: Option<MenuNode>,
}

pub enum UiEvent {
    Upsert(TrayItem),
    Remove(String),
    Status(String),
}

pub enum Command {
    Activate { key: String, x: i32, y: i32 },
    SecondaryActivate { key: String, x: i32, y: i32 },
    ContextMenu { key: String, x: i32, y: i32 },
    Scroll { key: String, delta: i32, horizontal: bool },
    MenuOpened { key: String },
    MenuClicked { key: String, id: i32 },
}

enum Internal {
    Registered(String),
    Unregistered(String),
}

/// Routing information for one registered item.
struct ItemRef {
    /// Bus name the item registered with (may be well-known).
    bus: String,
    /// Unique connection name that currently owns `bus`.
    dest: String,
    path: String,
    menu_path: Option<String>,
    /// Bumped on every refresh so stale results are dropped.
    generation: u64,
}

type Items = Arc<Mutex<HashMap<String, ItemRef>>>;

#[derive(Clone)]
struct Ui {
    tx: std::sync::mpsc::Sender<UiEvent>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Ui {
    fn send(&self, ev: UiEvent) {
        let _ = self.tx.send(ev);
        (self.repaint)();
    }
}

pub fn spawn(
    tx: std::sync::mpsc::Sender<UiEvent>,
    repaint: impl Fn() + Send + Sync + 'static,
) -> UnboundedSender<Command> {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let ui = Ui { tx, repaint: Arc::new(repaint) };
    std::thread::Builder::new()
        .name("sni".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            if let Err(e) = rt.block_on(run(ui.clone(), cmd_rx)) {
                eprintln!("frame-tray: D-Bus backend failed: {e:#}");
                ui.send(UiEvent::Status(format!("D-Bus error: {e:#}")));
            }
        })
        .expect("spawn sni thread");
    cmd_tx
}

async fn run(ui: Ui, mut cmd_rx: UnboundedReceiver<Command>) -> Result<()> {
    let conn = Connection::session().await.context("connecting to session bus")?;
    let dbus = DBusProxy::new(&conn).await?;
    let mut name_changes = dbus.receive_name_owner_changed().await?;

    let (int_tx, mut int_rx) = unbounded_channel();
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    conn.request_name(host_name.as_str()).await?;

    let own_watcher = become_watcher(&conn, int_tx.clone()).await?;
    if own_watcher {
        ui.send(UiEvent::Status("Acting as StatusNotifierWatcher".into()));
    } else {
        attach_to_watcher(&conn, &host_name, int_tx.clone()).await?;
        ui.send(UiEvent::Status("Using existing StatusNotifierWatcher".into()));
    }

    let mut item_signals =
        MessageStream::for_match_rule(format!("type='signal',interface='{ITEM_IFACE}'").as_str(), &conn, None)
            .await?;
    let mut menu_signals =
        MessageStream::for_match_rule(format!("type='signal',interface='{MENU_IFACE}'").as_str(), &conn, None)
            .await?;

    let items: Items = Arc::default();

    loop {
        tokio::select! {
            Some(ev) = int_rx.recv() => match ev {
                Internal::Registered(service) => add_item(&conn, &dbus, &items, &ui, service).await,
                Internal::Unregistered(service) => {
                    if items.lock().unwrap().remove(&service).is_some() {
                        ui.send(UiEvent::Remove(service));
                    }
                }
            },
            Some(cmd) = cmd_rx.recv() => handle_command(&conn, &items, &ui, cmd),
            Some(Ok(msg)) = item_signals.next() => {
                let hdr = msg.header();
                let (Some(sender), Some(path)) = (hdr.sender(), hdr.path()) else { continue };
                let key = items.lock().unwrap().iter()
                    .find(|(_, it)| it.dest == sender.as_str() && it.path == path.as_str())
                    .map(|(k, _)| k.clone());
                if let Some(key) = key {
                    refresh(&conn, &items, &ui, &key);
                }
            },
            Some(Ok(msg)) = menu_signals.next() => {
                let hdr = msg.header();
                let (Some(sender), Some(path)) = (hdr.sender(), hdr.path()) else { continue };
                let key = items.lock().unwrap().iter()
                    .find(|(_, it)| it.dest == sender.as_str() && it.menu_path.as_deref() == Some(path.as_str()))
                    .map(|(k, _)| k.clone());
                if let Some(key) = key {
                    refresh(&conn, &items, &ui, &key);
                }
            },
            Some(sig) = name_changes.next() => {
                let Ok(args) = sig.args() else { continue };
                if args.new_owner().is_some() {
                    continue;
                }
                let gone = args.name().to_string();
                let removed: Vec<String> = {
                    let mut map = items.lock().unwrap();
                    let keys: Vec<String> = map.iter()
                        .filter(|(_, it)| it.bus == gone || it.dest == gone)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in &keys {
                        map.remove(k);
                    }
                    keys
                };
                if own_watcher {
                    watcher_remove(&conn, &gone).await;
                }
                for k in removed {
                    ui.send(UiEvent::Remove(k));
                }
            },
            else => break,
        }
    }
    Ok(())
}

/// Split an SNI service string into (bus name, object path).
/// Accepts `bus`, `bus/path` (KDE) and `bus@/path` (GNOME AppIndicator extension).
fn parse_service(service: &str) -> (String, String) {
    match service.find('/') {
        Some(i) => (service[..i].trim_end_matches('@').to_owned(), service[i..].to_owned()),
        None => (service.to_owned(), "/StatusNotifierItem".to_owned()),
    }
}

async fn add_item(conn: &Connection, dbus: &DBusProxy<'_>, items: &Items, ui: &Ui, service: String) {
    let (bus, path) = parse_service(&service);
    let dest = if bus.starts_with(':') {
        bus.clone()
    } else {
        let owner = match BusName::try_from(bus.as_str()) {
            Ok(name) => dbus.get_name_owner(name).await.map_err(anyhow::Error::from),
            Err(e) => Err(e.into()),
        };
        match owner {
            Ok(owner) => owner.to_string(),
            Err(e) => {
                eprintln!("frame-tray: cannot resolve owner of {bus}: {e}");
                return;
            }
        }
    };
    items.lock().unwrap().insert(
        service.clone(),
        ItemRef { bus, dest, path, menu_path: None, generation: 0 },
    );
    refresh(conn, items, ui, &service);
}

/// Re-read all properties (and the menu) of an item in the background.
fn refresh(conn: &Connection, items: &Items, ui: &Ui, key: &str) {
    let (dest, path, generation) = {
        let mut map = items.lock().unwrap();
        let Some(it) = map.get_mut(key) else { return };
        it.generation += 1;
        (it.dest.clone(), it.path.clone(), it.generation)
    };
    let (conn, items, ui, key) = (conn.clone(), items.clone(), ui.clone(), key.to_owned());
    tokio::spawn(async move {
        match fetch_item(&conn, &key, &dest, &path).await {
            Ok((item, menu_path)) => {
                let mut map = items.lock().unwrap();
                let Some(it) = map.get_mut(&key) else { return };
                if it.generation != generation {
                    return;
                }
                it.menu_path = menu_path;
                drop(map);
                ui.send(UiEvent::Upsert(item));
            }
            Err(e) => eprintln!("frame-tray: failed to read {key}: {e:#}"),
        }
    });
}

async fn fetch_item(conn: &Connection, key: &str, dest: &str, path: &str) -> Result<(TrayItem, Option<String>)> {
    let props = get_all_props(conn, dest, path).await?;

    let s = |k: &str| props.get(k).and_then(|v| menu::as_str(v)).unwrap_or_default();
    let status = match s("Status").as_str() {
        "Passive" => Status::Passive,
        "NeedsAttention" => Status::NeedsAttention,
        _ => Status::Active,
    };
    let (tip_title, tip_body) = props.get("ToolTip").map(|v| parse_tooltip(v)).unwrap_or_default();
    let title = [s("Title"), tip_title.clone(), s("Id")]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| key.to_owned());
    let tooltip = [tip_title, tip_body].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");

    let theme_path = s("IconThemePath");
    let mut sources = Vec::new();
    if status == Status::NeedsAttention {
        sources.push((s("AttentionIconName"), props.get("AttentionIconPixmap").map(|v| best_pixmap(v))));
    }
    sources.push((s("IconName"), props.get("IconPixmap").map(|v| best_pixmap(v))));
    let icon = tokio::task::spawn_blocking(move || {
        sources.into_iter().find_map(|(name, pixmap)| {
            icon::load_named(&name, Some(&theme_path)).or(pixmap.flatten())
        })
    })
    .await?;

    let menu_path = props
        .get("Menu")
        .and_then(|v| menu::as_str(v))
        .filter(|p| !p.is_empty() && p != "/");
    let menu = match &menu_path {
        Some(mp) => match fetch_menu(conn, dest, mp).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("frame-tray: failed to read menu of {key}: {e:#}");
                None
            }
        },
        None => None,
    };

    let item = TrayItem {
        key: key.to_owned(),
        title,
        tooltip,
        status,
        icon,
        item_is_menu: props.get("ItemIsMenu").and_then(|v| menu::as_bool(v)).unwrap_or(false),
        menu,
    };
    Ok((item, menu_path))
}

const ITEM_PROPS: &[&str] = &[
    "Id", "Title", "Status", "IconName", "IconPixmap", "AttentionIconName", "AttentionIconPixmap",
    "IconThemePath", "ToolTip", "Menu", "ItemIsMenu",
];

async fn get_all_props(conn: &Connection, dest: &str, path: &str) -> Result<HashMap<String, OwnedValue>> {
    let proxy = Proxy::new(conn, dest, path, "org.freedesktop.DBus.Properties").await?;
    match proxy.call_method("GetAll", &(ITEM_IFACE,)).await {
        Ok(reply) => Ok(reply.body().deserialize()?),
        // Some implementations fail GetAll if any single getter errors; read what we can one by one.
        Err(get_all_err) => {
            let mut props = HashMap::new();
            for name in ITEM_PROPS {
                if let Ok(reply) = proxy.call_method("Get", &(ITEM_IFACE, *name)).await
                    && let Ok(v) = reply.body().deserialize::<OwnedValue>()
                {
                    props.insert((*name).to_owned(), v);
                }
            }
            if props.is_empty() { Err(get_all_err.into()) } else { Ok(props) }
        }
    }
}

async fn fetch_menu(conn: &Connection, dest: &str, path: &str) -> Result<Option<MenuNode>> {
    let proxy = Proxy::new(conn, dest, path, MENU_IFACE).await?;
    let reply = proxy.call_method("GetLayout", &(0i32, -1i32, Vec::<&str>::new())).await?;
    let body = reply.body();
    let s: Structure = body.deserialize()?;
    Ok(s.fields().get(1).and_then(menu::parse_node))
}

/// ToolTip is `(sa(iiay)ss)`: icon name, icon pixmaps, title, description.
fn parse_tooltip(v: &Value) -> (String, String) {
    match menu::unwrap(v) {
        Value::Structure(s) => {
            let f = s.fields();
            let get = |i: usize| f.get(i).and_then(menu::as_str).unwrap_or_default();
            (get(2), strip_markup(&get(3)))
        }
        _ => Default::default(),
    }
}

/// Tooltip descriptions may contain a small HTML subset; show plain text.
fn strip_markup(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
}

/// Pick the pixmap closest to (but preferably not below) our display size.
fn best_pixmap(v: &Value) -> Option<Icon> {
    let Value::Array(a) = menu::unwrap(v) else { return None };
    a.inner()
        .iter()
        .filter_map(|e| match e {
            Value::Structure(s) => match s.fields() {
                [w, h, data] => Some((menu::as_i32(w)?, menu::as_i32(h)?, data)),
                _ => None,
            },
            _ => None,
        })
        .min_by_key(|(w, _, _)| {
            let target = icon::ICON_SIZE as i32;
            if *w >= target { w - target } else { (target - w) * 4 }
        })
        .and_then(|(w, h, data)| icon::from_argb32(w, h, &menu::as_bytes(data)?))
}

fn handle_command(conn: &Connection, items: &Items, ui: &Ui, cmd: Command) {
    let key = match &cmd {
        Command::Activate { key, .. }
        | Command::SecondaryActivate { key, .. }
        | Command::ContextMenu { key, .. }
        | Command::Scroll { key, .. }
        | Command::MenuOpened { key }
        | Command::MenuClicked { key, .. } => key.clone(),
    };
    let Some((dest, path, menu_path)) = items
        .lock()
        .unwrap()
        .get(&key)
        .map(|it| (it.dest.clone(), it.path.clone(), it.menu_path.clone()))
    else {
        return;
    };
    let (conn, items, ui) = (conn.clone(), items.clone(), ui.clone());
    tokio::spawn(async move {
        let result: Result<()> = async {
            match cmd {
                Command::Activate { x, y, .. } => call_item(&conn, &dest, &path, "Activate", &(x, y)).await,
                Command::SecondaryActivate { x, y, .. } => {
                    call_item(&conn, &dest, &path, "SecondaryActivate", &(x, y)).await
                }
                Command::ContextMenu { x, y, .. } => call_item(&conn, &dest, &path, "ContextMenu", &(x, y)).await,
                Command::Scroll { delta, horizontal, .. } => {
                    let orientation = if horizontal { "horizontal" } else { "vertical" };
                    call_item(&conn, &dest, &path, "Scroll", &(delta, orientation)).await
                }
                Command::MenuOpened { .. } => {
                    let Some(mp) = menu_path else { return Ok(()) };
                    let proxy = Proxy::new(&conn, dest.as_str(), mp.as_str(), MENU_IFACE).await?;
                    let reply = proxy.call_method("AboutToShow", &(0i32,)).await?;
                    let needs_update: bool = reply.body().deserialize()?;
                    if needs_update {
                        refresh(&conn, &items, &ui, &key);
                    }
                    Ok(())
                }
                Command::MenuClicked { id, .. } => {
                    let Some(mp) = menu_path else { return Ok(()) };
                    let proxy = Proxy::new(&conn, dest.as_str(), mp.as_str(), MENU_IFACE).await?;
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as u32)
                        .unwrap_or(0);
                    proxy
                        .call_method("Event", &(id, "clicked", Value::from(0i32), timestamp))
                        .await?;
                    Ok(())
                }
            }
        }
        .await;
        if let Err(e) = result {
            eprintln!("frame-tray: command on {key} failed: {e:#}");
        }
    });
}

async fn call_item<B>(conn: &Connection, dest: &str, path: &str, method: &str, body: &B) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let proxy = Proxy::new(conn, dest, path, ITEM_IFACE).await?;
    proxy.call_method(method, body).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Watcher

struct Watcher {
    items: Vec<String>,
    tx: UnboundedSender<Internal>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        // Items may register with just an object path; the bus name is then the sender.
        let full = if service.starts_with('/') { format!("{sender}{service}") } else { service.to_owned() };
        if self.items.contains(&full) {
            return;
        }
        self.items.push(full.clone());
        let _ = Self::status_notifier_item_registered(&emitter, &full).await;
        let _ = self.registered_status_notifier_items_changed(&emitter).await;
        let _ = self.tx.send(Internal::Registered(full));
    }

    async fn register_status_notifier_host(
        &mut self,
        _service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let _ = Self::status_notifier_host_registered(&emitter).await;
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Try to own `org.kde.StatusNotifierWatcher`. Returns false if another process already does.
async fn become_watcher(conn: &Connection, tx: UnboundedSender<Internal>) -> Result<bool> {
    conn.object_server().at(WATCHER_PATH, Watcher { items: Vec::new(), tx }).await?;
    let reply = conn
        .request_name_with_flags(WATCHER_NAME, RequestNameFlags::DoNotQueue.into())
        .await;
    match reply {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(true),
        Ok(_) | Err(zbus::Error::NameTaken) => {
            conn.object_server().remove::<Watcher, _>(WATCHER_PATH).await?;
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

async fn watcher_remove(conn: &Connection, gone: &str) {
    let Ok(iface) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH).await else { return };
    let emitter = iface.signal_emitter().clone();
    let mut w = iface.get_mut().await;
    let removed: Vec<String> = w.items.iter().filter(|s| parse_service(s).0 == gone).cloned().collect();
    if removed.is_empty() {
        return;
    }
    w.items.retain(|s| !removed.contains(s));
    for s in &removed {
        let _ = Watcher::status_notifier_item_unregistered(&emitter, s).await;
    }
    let _ = w.registered_status_notifier_items_changed(&emitter).await;
}

/// Register as a host with someone else's watcher and mirror its item list.
async fn attach_to_watcher(conn: &Connection, host_name: &str, tx: UnboundedSender<Internal>) -> Result<()> {
    let proxy = Proxy::new(conn, WATCHER_NAME, WATCHER_PATH, WATCHER_NAME).await?;
    let mut registered = proxy.receive_signal("StatusNotifierItemRegistered").await?;
    let mut unregistered = proxy.receive_signal("StatusNotifierItemUnregistered").await?;

    // GNOME's AppIndicator extension refuses extra hosts; we can still mirror its items.
    let _ = proxy.call_method("RegisterStatusNotifierHost", &(host_name,)).await;
    let existing: Vec<String> = proxy.get_property("RegisteredStatusNotifierItems").await?;
    for s in existing {
        let _ = tx.send(Internal::Registered(s));
    }

    tokio::spawn(async move {
        loop {
            let (msg, registered_ev) = tokio::select! {
                Some(m) = registered.next() => (m, true),
                Some(m) = unregistered.next() => (m, false),
                else => break,
            };
            let Ok(service) = msg.body().deserialize::<String>() else { continue };
            let ev = if registered_ev { Internal::Registered(service) } else { Internal::Unregistered(service) };
            if tx.send(ev).is_err() {
                break;
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_parsing() {
        assert_eq!(parse_service(":1.5/org/ayatana/NotificationItem/x"), (":1.5".into(), "/org/ayatana/NotificationItem/x".into()));
        assert_eq!(
            parse_service("org.kde.StatusNotifierItem-1-1"),
            ("org.kde.StatusNotifierItem-1-1".into(), "/StatusNotifierItem".into())
        );
        assert_eq!(parse_service(":1.9@/StatusNotifierItem"), (":1.9".into(), "/StatusNotifierItem".into()));
    }
}
