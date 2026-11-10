//! Interactive terminal management over the same private API as the desktop.
pub(crate) mod api;
use api::Api;
use crossterm::{
    clipboard::CopyToClipboard,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PAGES: [&str; 8] = [
    "Devices",
    "Local services",
    "Pair remote host",
    "Pair this device",
    "Requests",
    "Settings",
    "Diagnostics",
    "Owner operations",
];
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Machines,
    Services,
    Pair,
    Qr,
    Pending,
    Home,
    Diagnostics,
    Owners,
}
impl Page {
    fn index(self) -> usize {
        self as usize
    }
    fn at(n: usize) -> Self {
        [
            Self::Machines,
            Self::Services,
            Self::Pair,
            Self::Qr,
            Self::Pending,
            Self::Home,
            Self::Diagnostics,
            Self::Owners,
        ][n % 8]
    }
}
#[derive(Clone)]
enum Task {
    Status,
    Request(Value),
    Online(bool),
    Remove {
        peer: String,
        revision: u64,
    },
    Pair {
        preview: Value,
        document: String,
    },
    Open {
        host: String,
        service: Value,
        name: String,
        enable: bool,
    },
    Settings,
    ResolveLink(String),
    SaveSettings {
        name: String,
        port: u16,
    },
    Links {
        host: String,
        service: Value,
    },
    OwnerList,
    OwnerStart(Value),
    OwnerRetry(String),
}
#[derive(Clone)]
enum Kind {
    Status,
    Saved,
    Qr,
    Catalog(String),
    Preview(String),
    Paired,
    Open,
    Owners,
    OwnerSaved,
    Settings,
    Links,
}
#[derive(Clone)]
struct Work {
    kind: Kind,
    task: Task,
}
struct Reply {
    kind: Kind,
    result: Result<Value, String>,
}
fn perform(api: &Api, task: Task) -> Result<Value, String> {
    match task {
        Task::Status => api.request(json!({"op":"status","refresh_clients":true})),
        Task::Request(v) => api.request(v),
        Task::Online(v) => api.online(v),
        Task::Remove { peer, revision } => {
            api.request(json!({"op":"unpair","host":peer,"expected_revision":revision}))
        }
        Task::Pair { preview, document } => {
            let host = text(&preview, "host");
            let value = api.request(json!({"op":"pair","host":host,"invitation":document}))?;
            api.save_pair(host, text(&preview, "host_name"))?;
            Ok(value)
        }
        Task::Open {
            host,
            service,
            name,
            enable,
        } => {
            if service["kind"] != "http" && service["kind"] != "tcp" {
                return Err("This service has no browser or TCP connection".into());
            }
            if enable {
                api.request(json!({"op":"enable"}))?;
            }
            let result=api.request(json!({"op":"open","host":host,"service":service["id"],"incarnation":service["incarnation"],"host_name":name}))?;
            if !text(&result, "url").starts_with("tcp://") {
                launch_browser(&result)?;
            }
            Ok(result)
        }
        Task::ResolveLink(link) => {
            api.request(json!({"op":"enable"}))?;
            let result = api.request(json!({"op":"resolve_link","link":link}))?;
            if !text(&result, "url").starts_with("tcp://") {
                launch_browser(&result)?;
            }
            Ok(result)
        }
        Task::Settings => api.request(json!({"op":"gateway_settings"})),
        Task::SaveSettings { name, port } => {
            api.check_rename(&name)?;
            let mut settings = api.request(json!({"op":"gateway_settings"}))?["settings"].clone();
            settings["registry_port"] = json!(port);
            settings["service_ports"] = json!([]);
            api.request(json!({"op":"configure_access","settings":settings}))?;
            api.rename_device(&name)?;
            Ok(json!({"saved":true}))
        }
        Task::Links { host, service } => service_links(api, &host, &service),
        Task::OwnerList => api.owner(&["--operations".into()]),
        Task::OwnerStart(plan) => api.owner_start(&plan),
        Task::OwnerRetry(id) => api.owner(&["--retry".into(), id]),
    }
}
fn launch_browser(result: &Value) -> Result<(), String> {
    // Stable local URLs contain no capability; never invoke a shell.
    let url = text(result, "url");
    let parsed = url::Url::parse(url).map_err(|_| "Invalid browser handoff")?;
    if parsed.scheme() != "http"
        || !parsed.host_str().is_some_and(|h| h.ends_with(".localhost"))
        || parsed.port().is_none()
        || parsed.path() != "/"
        || parsed.fragment().is_some()
        || parsed.query().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("Invalid local browser handoff".into());
    }
    if cfg!(target_os = "macos")
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        api::command(std::process::Command::new(opener).arg(url), None, 10)?;
    }
    Ok(())
}
pub(crate) fn service_links(api: &Api, host: &str, service: &Value) -> Result<Value, String> {
    api.request(json!({"op":"enable"}))?;
    let route = api.request(json!({"op":"open","host":host,"service":service["id"],"incarnation":service["incarnation"]}))?;
    let status = api.request(json!({"op":"status"}))?;
    let publications = if status["host"]["host"] == host {
        status["public_access"]["services"].clone()
    } else {
        // Public metadata is optional; the local route remains usable if unavailable.
        api.request(json!({"op":"host_public_links","host":host}))
            .unwrap_or(Value::Null)["services"]
            .clone()
    };
    let public = array(&publications).into_iter().find(|p| {
        p["service"] == service["id"]
            && p["incarnation"] == service["incarnation"]
            && p["enabled"] != false
    });
    Ok(
        json!({"local":route["url"],"wispnet":format!("wispnet://{}/{}",host,text(service,"id")),"public":public.and_then(|p| p["url"].as_str().map(str::to_owned))}),
    )
}
fn worker(api: Api) -> (mpsc::Sender<Work>, mpsc::Receiver<Reply>) {
    let (tx, jobs) = mpsc::channel::<Work>();
    let (done, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for work in jobs {
            let result = perform(&api, work.task);
            if done
                .send(Reply {
                    kind: work.kind,
                    result,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (tx, rx)
}
#[derive(Clone)]
enum Action {
    Section,
    Nav(Page),
    Code,
    Copy(String),
    Qr,
    Pending(Value),
    Machine(String),
    Service(Value),
    Remote(String, Value),
    Shell(String, Value),
    PairInput,
    Form(Form),
    Confirm {
        title: String,
        detail: Vec<String>,
        work: Work,
    },
    Run(Work),
    OwnerNew(bool),
    OwnerDetail(Value),
    Info(String, Vec<String>),
}
#[derive(Clone)]
struct Entry {
    id: String,
    label: String,
    detail: Vec<String>,
    action: Action,
}
#[derive(Clone)]
struct Field {
    label: String,
    value: String,
    kind: FieldKind,
}
#[derive(Clone)]
enum FieldKind {
    ReadOnly,
    Granted,
    Text(usize),
    Check(bool),
    Choice(Vec<String>, usize),
}
impl Field {
    fn text(label: &str, value: &str, limit: usize) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            kind: FieldKind::Text(limit),
        }
    }
    fn check(label: &str, value: Value, checked: bool) -> Self {
        Self {
            label: label.into(),
            value: value.to_string(),
            kind: FieldKind::Check(checked),
        }
    }
    fn choice(label: &str, choices: &[&str], index: usize) -> Self {
        Self {
            label: label.into(),
            value: String::new(),
            kind: FieldKind::Choice(choices.iter().map(|s| s.to_string()).collect(), index),
        }
    }
}
#[derive(Clone)]
enum FormKind {
    Files {
        revision: u64,
        exports: Value,
    },
    Shell {
        revision: u64,
    },
    Approval {
        invitation: String,
        peer: String,
        name: String,
        revision: u64,
    },
    Service {
        existing: Option<String>,
        revision: u64,
    },
    Pair,
    Settings,
    Link,
    Owner {
        approve: bool,
    },
}
#[derive(Clone)]
struct Form {
    title: String,
    kind: FormKind,
    fields: Vec<Field>,
    selected: usize,
}
enum Modal {
    Choice {
        title: String,
        detail: Vec<String>,
        entries: Vec<Entry>,
        selected: usize,
        scroll: u16,
    },
    Form(Form),
    Info {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
}
#[derive(Clone)]
enum Hit {
    Nav(usize),
    Row(usize),
    ModalRow(usize),
    Field(usize),
}
struct CatalogReply {
    peer: String,
    generation: u64,
    result: Result<Value, String>,
}
struct App {
    shell: Option<Value>,
    api: Api,
    page: Page,
    menu_focus: bool,
    menu: usize,
    selected: usize,
    list_state: ListState,
    snapshot: Option<Value>,
    online: bool,
    names: Value,
    catalogs: HashMap<String, Value>,
    catalog_errors: HashMap<String, String>,
    catalog_tx: mpsc::Sender<CatalogReply>,
    catalog_rx: mpsc::Receiver<CatalogReply>,
    catalog_inflight: HashMap<String, u64>,
    catalog_checked: HashMap<String, Instant>,
    catalog_generation: u64,
    catalog_active: usize,
    qr: Option<Value>,
    show_code: bool,
    code_scroll: u16,
    operations: Vec<Value>,
    modal: Option<Modal>,
    message: String,
    tx: mpsc::Sender<Work>,
    rx: mpsc::Receiver<Reply>,
    busy: bool,
    pending_work: Option<Work>,
    last_poll: Instant,
    hits: Vec<(Rect, Hit)>,
    last_click: Option<(Instant, usize)>,
}
fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn array(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
        })
        .collect()
}
fn short(s: &str) -> String {
    s.chars().take(12).collect()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn entry(id: &str, label: impl Into<String>, action: Action) -> Entry {
    Entry {
        id: id.into(),
        label: label.into(),
        detail: vec![],
        action,
    }
}
impl App {
    fn new(api: Api) -> Self {
        let names = api.names();
        let (tx, rx) = worker(api.clone());
        let (catalog_tx, catalog_rx) = mpsc::channel();
        Self {
            shell: None,
            api,
            page: Page::Machines,
            menu_focus: false,
            menu: 0,
            selected: 0,
            list_state: ListState::default(),
            snapshot: None,
            online: false,
            names,
            catalogs: HashMap::new(),
            catalog_errors: HashMap::new(),
            catalog_tx,
            catalog_rx,
            catalog_inflight: HashMap::new(),
            catalog_checked: HashMap::new(),
            catalog_generation: 0,
            catalog_active: 0,
            qr: None,
            show_code: false,
            code_scroll: 0,
            operations: vec![],
            modal: None,
            message: "Connecting to local node…".into(),
            tx,
            rx,
            busy: false,
            pending_work: None,
            last_poll: Instant::now() - Duration::from_secs(5),
            hits: vec![],
            last_click: None,
        }
    }
    fn status(&self) -> Value {
        self.snapshot.clone().unwrap_or_else(|| json!({}))
    }
    fn host(&self) -> Value {
        self.status()["host"].clone()
    }
    fn revision(&self) -> u64 {
        self.host()["revision"].as_u64().unwrap_or(0)
    }
    fn clients(&self) -> Vec<Value> {
        array(&self.host()["clients"])
    }
    fn services(&self) -> Vec<Value> {
        array(&self.host()["services"])
    }
    fn pending(&self) -> Vec<Value> {
        array(&self.status()["invitations"])
            .into_iter()
            .filter(|v| v["state"] == "pending")
            .collect()
    }
    fn name(&self, peer: &str) -> String {
        self.names[peer]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(clean)
            .or_else(|| {
                self.clients()
                    .iter()
                    .find(|v| v["peer"] == peer)
                    .map(|v| clean(text(v, "name")))
            })
            .unwrap_or_else(|| format!("Host {}", short(peer)))
    }
    fn mutate(&self, operation: Value) -> Work {
        Work {
            kind: Kind::Saved,
            task: Task::Request(json!({"op":"mutate","operation":operation})),
        }
    }
    fn send(&mut self, work: Work) {
        if self.busy {
            if !matches!(work.kind, Kind::Status) && self.pending_work.is_none() {
                self.pending_work = Some(work);
                self.message = "Queued after the current operation…".into();
            } else {
                self.message = "An operation is already queued; please wait".into();
            }
            return;
        }
        let allow_offline = matches!(
            work.task,
            Task::Status
                | Task::Online(_)
                | Task::OwnerList
                | Task::OwnerStart(_)
                | Task::OwnerRetry(_)
        );
        if !self.online && !allow_offline {
            self.message = "Node offline. Go online or refresh before making changes.".into();
            return;
        }
        let is_status = matches!(work.kind, Kind::Status);
        if self.tx.send(work).is_ok() {
            self.busy = true;
            if !is_status {
                self.message = "Working… (UI remains responsive)".into();
            }
        }
    }
    fn navigate(&mut self, page: Page) {
        if page != Page::Qr {
            self.qr = None;
        }
        self.page = page;
        self.menu = page.index();
        self.selected = 0;
        self.list_state = ListState::default();
        self.menu_focus = false;
        if page == Page::Qr {
            self.action(Action::Code);
        }
        if page == Page::Owners {
            self.send(Work {
                kind: Kind::Owners,
                task: Task::OwnerList,
            });
        }
    }
    fn entries(&self) -> Vec<Entry> {
        match self.page {
            Page::Home => vec![
                entry(
                    "settings",
                    "Device ID and shared HTTP port",
                    Action::Run(Work {
                        kind: Kind::Settings,
                        task: Task::Settings,
                    }),
                ),
                entry(
                    "link",
                    "Open a Wispnet service link",
                    Action::Form(Form {
                        title: "Open service link".into(),
                        kind: FormKind::Link,
                        fields: vec![Field::text("Wispnet URL", "", 256)],
                        selected: 0,
                    }),
                ),
                entry(
                    "online",
                    if self.online {
                        "Go offline"
                    } else {
                        "Go online"
                    },
                    Action::Confirm {
                        title: if self.online {
                            "Go offline"
                        } else {
                            "Go online"
                        }
                        .into(),
                        detail: vec![
                            "Start/stop this computer's node. The relay remains independent."
                                .into(),
                        ],
                        work: Work {
                            kind: Kind::Saved,
                            task: Task::Online(!self.online),
                        },
                    },
                ),
                entry(
                    "access",
                    if self.status()["access_enabled"] == true {
                        "Stop browser access"
                    } else {
                        "Enable browser access"
                    },
                    Action::Run(Work {
                        kind: Kind::Saved,
                        task: Task::Request(
                            json!({"op":if self.status()["access_enabled"]==true{"stop"}else{"enable"}}),
                        ),
                    }),
                ),
            ],
            Page::Pending => self
                .pending()
                .into_iter()
                .map(|v| {
                    let mut row = entry(
                        text(&v, "invitation"),
                        format!("{} · {}", clean(text(&v, "name")), short(text(&v, "peer"))),
                        Action::Pending(v.clone()),
                    );
                    row.detail = vec![
                        format!("Host ID: {}", text(&v, "peer")),
                        format!(
                            "Expires in {} seconds",
                            v["expires_at"].as_u64().unwrap_or(0).saturating_sub(now())
                        ),
                        "Choose service access while approving this request.".into(),
                    ];
                    row
                })
                .collect(),
            Page::Machines => {
                let mut peers = BTreeMap::new();
                for pin in array(&self.status()["peer_pins"]) {
                    if let Some(id) = pin.as_str() {
                        peers.insert(id.to_string(), false);
                    }
                }
                for client in self.clients() {
                    peers.insert(text(&client, "peer").to_owned(), true);
                }
                let remote_rows: Vec<Entry> = peers
                    .into_iter()
                    .flat_map(|(peer, client)| {
                        let status = self.status();
                        let telemetry = &status["peer_telemetry"][&peer];
                        let activity = if self
                            .catalog_errors
                            .get(&peer)
                            .is_some_and(|e| e == "awaiting approval")
                        {
                            "Awaiting approval".to_string()
                        } else if !self.online {
                            "Node offline".to_string()
                        } else if status["connected_peers"][&peer].as_u64().unwrap_or(0) > 0 {
                            "Connected".into()
                        } else if let Some(age) = status["recent_peers"][&peer].as_u64() {
                            format!("Seen {age}s ago")
                        } else {
                            "Idle".into()
                        };
                        let path = if telemetry.is_object() {
                            format!(
                                " · {} · {}ms",
                                text(telemetry, "path").to_uppercase(),
                                telemetry["rtt_ms"]
                            )
                        } else {
                            String::new()
                        };
                        let mut row = entry(
                            &peer,
                            format!("{}  ·  {activity}{path}", self.name(&peer)),
                            Action::Machine(peer.clone()),
                        );
                        row.detail = vec![
                            self.name(&peer),
                            activity.clone(),
                            String::new(),
                            format!("Device ID: {peer}"),
                            format!("Paired client here: {}", if client { "yes" } else { "no" }),
                            String::new(),
                            format!("Shared services: {}", {
                                let shared = self
                                    .services()
                                    .into_iter()
                                    .filter(|service| {
                                        array(&service["allow"]).contains(&json!(peer))
                                    })
                                    .map(|service| clean(text(&service, "id")))
                                    .collect::<Vec<_>>();
                                if shared.is_empty() {
                                    "None".into()
                                } else {
                                    shared.join(", ")
                                }
                            }),
                            String::new(),
                            if array(&status["peer_pins"]).contains(&json!(peer)) {
                                "Enter  Browse services or manage pairing".into()
                            } else {
                                "Enter  Manage pairing".into()
                            },
                        ];
                        if let Some(error) = self.catalog_errors.get(&peer) {
                            row.detail
                                .push(format!("Catalog unavailable: {}", clean(error)));
                        }
                        let mut rows = vec![row];
                        rows.extend(self.remote_service_entries(&peer));
                        rows
                    })
                    .collect();
                let mut local = entry(
                    "this-device",
                    format!(
                        "{}  ·  This device",
                        clean(text(&self.status(), "host_name"))
                    ),
                    Action::Nav(Page::Services),
                );
                local.detail = vec![
                    clean(text(&self.status(), "host_name")),
                    "This device".into(),
                    String::new(),
                    format!(
                        "{} local services · {} approved devices",
                        self.services().len(),
                        self.clients().len()
                    ),
                    String::new(),
                    "Enter  Manage local services and permissions".into(),
                ];
                let mut rows = vec![local];
                if self.services().is_empty() {
                    rows.push(self.service_state_entry(
                        "local",
                        "No services",
                        "This device has no local services configured.",
                    ));
                }
                for mut service in self.local_service_entries().into_iter().skip(1) {
                    service.label = format!("  {}", service.label);
                    service.id = format!("local-service:{}", service.id);
                    rows.push(service);
                }
                rows.push(entry(
                    "remote-sector",
                    "── Remote Sector ──",
                    Action::Section,
                ));
                if remote_rows.is_empty() {
                    rows.push(self.service_state_entry(
                        "remote-sector",
                        "No remote machines",
                        "Pair a remote machine to see its services here.",
                    ));
                } else {
                    rows.extend(remote_rows);
                }
                rows
            }
            Page::Services => self.local_service_entries(),
            Page::Pair => vec![entry("paste", "Paste a pairing code", Action::PairInput)],
            Page::Owners => {
                let mut rows = vec![
                    entry(
                        "approve",
                        "Add a client across selected owner hosts",
                        Action::OwnerNew(true),
                    ),
                    entry(
                        "revoke",
                        "Revoke a client across selected owner hosts",
                        Action::OwnerNew(false),
                    ),
                ];
                rows.extend(self.operations.iter().map(|v| {
                    entry(
                        text(v, "id"),
                        format!(
                            "{} · {} · {}",
                            short(text(v, "id")),
                            text(&v["action"], "op"),
                            if v["complete"] == true {
                                "All hosts acknowledged"
                            } else {
                                "Pending or rejected hosts"
                            }
                        ),
                        Action::OwnerDetail(v.clone()),
                    )
                }));
                rows
            }
            Page::Diagnostics => {
                let s = self.status();
                vec![entry(
                    "info",
                    "Show node details",
                    Action::Info(
                        "Node diagnostics".into(),
                        vec![
                            format!("Host: {}", text(&s["host"], "host")),
                            format!("Device ID: {}", text(&s, "host_name")),
                            format!("Revision: {}", s["host"]["revision"]),
                            format!("Relay: {}", text(&s, "relay")),
                            format!("Transport streams: {}", s["transport_streams"]),
                            format!("Config: {}", self.api.config.display()),
                            format!("State: {}", self.api.state.display()),
                            "A recent request or a connected transport never grants authorization."
                                .into(),
                        ],
                    ),
                )]
            }
            Page::Qr => vec![],
        }
    }
    fn service_state_entry(&self, peer: &str, label: &str, detail: &str) -> Entry {
        let mut row = entry(
            &format!("services-state:{peer}"),
            format!("  {label}"),
            Action::Info(label.into(), vec![detail.into()]),
        );
        row.detail = vec![label.into(), String::new(), detail.into()];
        row
    }
    fn remote_service_entries(&self, peer: &str) -> Vec<Entry> {
        let state = |label, detail| vec![self.service_state_entry(peer, label, detail)];
        if !array(&self.status()["peer_pins"]).contains(&json!(peer)) {
            return state(
                "No services",
                "This device is paired as a client; no remote service host is saved here.",
            );
        }
        if !self.online {
            return state(
                "Services unavailable · node offline",
                "Go online to refresh this device’s services.",
            );
        }
        if let Some(error) = self.catalog_errors.get(peer) {
            return state(
                if error == "awaiting approval" {
                    "Awaiting approval"
                } else {
                    "Services unavailable"
                },
                &clean(error),
            );
        }
        let Some(catalog) = self.catalogs.get(peer) else {
            return state(
                "Loading services…",
                "Fetching the services this device permits you to access.",
            );
        };
        let services = array(&catalog["services"]);
        if services.is_empty() {
            return state(
                "No services",
                "This device currently shares no services with you.",
            );
        }
        services
            .into_iter()
            .map(|service| {
                let mut row = entry(
                    &format!("remote-service:{peer}:{}", text(&service, "id")),
                    format!(
                        "  {} · {}",
                        clean(text(&service, "id")),
                        text(&service, "kind")
                    ),
                    Action::Remote(peer.into(), service.clone()),
                );
                row.detail = vec![
                    clean(text(&service, "id")),
                    format!("On {}", self.name(peer)),
                    format!("{} service", text(&service, "kind").to_uppercase()),
                    String::new(),
                    "Enter  Open or show service links".into(),
                ];
                row
            })
            .collect()
    }
    // Catalog reads have their own bounded workers: an offline peer must not
    // hold up local status, confirmations, or the other machines’ services.
    fn refresh_catalogs(&mut self) {
        let pins = array(&self.status()["peer_pins"]);
        self.catalogs
            .retain(|peer, _| self.online && pins.contains(&json!(peer)));
        self.catalog_errors
            .retain(|peer, _| self.online && pins.contains(&json!(peer)));
        self.catalog_inflight
            .retain(|peer, _| self.online && pins.contains(&json!(peer)));
        self.catalog_checked
            .retain(|peer, _| self.online && pins.contains(&json!(peer)));
        while let Ok(reply) = self.catalog_rx.try_recv() {
            self.catalog_active = self.catalog_active.saturating_sub(1);
            if self.catalog_inflight.get(&reply.peer) != Some(&reply.generation) {
                continue;
            }
            self.catalog_inflight.remove(&reply.peer);
            self.catalog_checked
                .insert(reply.peer.clone(), Instant::now());
            let selected = self.entries().get(self.selected).map(|row| row.id.clone());
            match reply.result.and_then(|value| {
                if value["host"] != reply.peer || !value["services"].is_array() {
                    Err("Invalid service catalog".into())
                } else {
                    Ok(value)
                }
            }) {
                Ok(value) => {
                    self.catalog_errors.remove(&reply.peer);
                    self.catalogs.insert(reply.peer, value);
                }
                Err(error) => {
                    self.catalogs.remove(&reply.peer);
                    self.catalog_errors.insert(reply.peer, error);
                }
            }
            if let Some(index) =
                selected.and_then(|id| self.entries().iter().position(|row| row.id == id))
            {
                self.selected = index;
            }
        }
        if !self.online {
            return;
        }
        for peer in pins.iter().filter_map(Value::as_str) {
            if self.catalog_active >= 4 {
                break;
            }
            if self.catalog_inflight.contains_key(peer)
                || self
                    .catalog_checked
                    .get(peer)
                    .is_some_and(|checked| checked.elapsed() < Duration::from_secs(15))
            {
                continue;
            }
            self.catalog_active += 1;
            self.catalog_generation += 1;
            let generation = self.catalog_generation;
            self.catalog_inflight.insert(peer.into(), generation);
            let api = self.api.clone();
            let done = self.catalog_tx.clone();
            let peer = peer.to_owned();
            std::thread::spawn(move || {
                let result = api.request(json!({"op":"catalog", "host":peer}));
                let _ = done.send(CatalogReply {
                    peer,
                    generation,
                    result,
                });
            });
        }
    }
    fn local_service_entries(&self) -> Vec<Entry> {
        let mut rows = vec![entry(
            "+",
            "+ Add local service",
            Action::Form(self.service_form(None)),
        )];
        rows[0].detail = vec!["Share a local service".into(), String::new(), "Connect an HTTP or TCP service running on this computer, then choose which paired devices may access it.".into(), String::new(), "Enter  Add service".into()];
        rows.extend(self.services().into_iter().map(|service| {
            let mut row = entry(
                text(&service, "id"),
                format!(
                    "{} · {} · {} permitted devices",
                    clean(text(&service, "id")),
                    text(&service, "kind"),
                    array(&service["allow"]).len()
                ),
                Action::Service(service.clone()),
            );
            row.detail = vec![
                clean(text(&service, "id")),
                format!("{} service", text(&service, "kind").to_uppercase()),
                String::new(),
                format!("Backend: {}", text(&service, "backend")),
                format!(
                    "Permitted devices: {}",
                    array(&service["allow"])
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|p| self.name(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                String::new(),
                "Enter  Open, edit or show service links".into(),
            ];
            row
        }));
        rows
    }
    fn service_form(&self, existing: Option<Value>) -> Form {
        let old = existing.clone().unwrap_or_else(|| json!({}));
        let mut fields = vec![
            Field::text("Service ID", text(&old, "id"), 64),
            Field::text("Local port / loopback address", text(&old, "backend"), 128),
            Field::choice(
                "Protocol",
                &["http", "tcp"],
                usize::from(old["kind"] == "tcp"),
            ),
        ];
        for client in self.clients() {
            let checked = existing.is_none() || array(&old["allow"]).contains(&client["peer"]);
            fields.push(Field::check(
                &format!(
                    "Allow {} · {}",
                    clean(text(&client, "name")),
                    short(text(&client, "peer"))
                ),
                client["peer"].clone(),
                checked,
            ));
        }
        Form {
            title: if existing.is_some() {
                "Edit service and device grants"
            } else {
                "Add service"
            }
            .into(),
            kind: FormKind::Service {
                existing: existing.map(|v| text(&v, "id").to_owned()),
                revision: self.revision(),
            },
            fields,
            selected: 0,
        }
    }
    fn shell_form(&self, service: &Value) -> Form {
        let fields = self
            .clients()
            .iter()
            .map(|client| {
                Field::check(
                    &format!(
                        "Allow {} · {}",
                        clean(text(client, "name")),
                        short(text(client, "peer"))
                    ),
                    client["peer"].clone(),
                    array(&service["allow"]).contains(&client["peer"]),
                )
            })
            .collect();
        Form {
            title: "Shell · permitted devices".into(),
            kind: FormKind::Shell {
                revision: self.revision(),
            },
            fields,
            selected: 0,
        }
    }
    fn files_configured(&self) -> bool {
        self.host()["file_exports"]
            .as_object()
            .is_some_and(|roots| !roots.is_empty())
    }
    fn files_form(&self, service: &Value) -> Form {
        let mut form = self.shell_form(service);
        form.title = "Files · permitted devices".into();
        if !self.files_configured() {
            for field in &mut form.fields {
                field.kind = FieldKind::ReadOnly;
                field.value = "Set a shared folder before granting Files access".into();
            }
        }
        form.kind = FormKind::Files {
            revision: self.revision(),
            exports: self.host()["file_exports"]
                .as_object()
                .cloned()
                .map(Value::Object)
                .unwrap_or(json!({})),
        };
        form
    }
    fn choice(&mut self, title: impl Into<String>, detail: Vec<String>, entries: Vec<Entry>) {
        self.modal = Some(Modal::Choice {
            title: title.into(),
            detail,
            entries,
            selected: 0,
            scroll: 0,
        });
    }
    fn confirmation(&mut self, title: String, detail: Vec<String>, work: Work) {
        self.choice(
            title,
            detail,
            vec![
                entry("confirm", "Confirm", Action::Run(work)),
                entry(
                    "cancel",
                    "Cancel",
                    Action::Info("Cancelled".into(), vec!["No changes made.".into()]),
                ),
            ],
        );
    }
    fn action(&mut self, action: Action) {
        match action {
            Action::Shell(host, service) => {
                self.modal = None;
                self.shell = Some(
                    json!({"host":host,"service":service["id"],"incarnation":service["incarnation"],"session":"main"}),
                );
            }
            Action::Section => {}
            Action::Nav(page) => self.navigate(page),
            Action::Code | Action::Qr => {
                self.show_code = matches!(action, Action::Code);
                self.code_scroll = 0;
                self.qr = None;
                self.page = Page::Qr;
                self.menu = Page::Qr.index();
                self.send(Work {
                    kind: Kind::Qr,
                    task: Task::Request(json!({"op":"invite_qr"})),
                });
            }
            Action::Run(work) => {
                self.modal = None;
                self.send(work);
            }
            Action::Confirm {
                title,
                detail,
                work,
            } => self.confirmation(title, detail, work),
            Action::Info(title, lines) => {
                self.modal = Some(Modal::Info {
                    title,
                    lines,
                    scroll: 0,
                })
            }
            Action::Form(form) => self.modal = Some(Modal::Form(form)),
            Action::PairInput => {
                self.code_scroll = 0;
                self.modal = Some(Modal::Form(Form {
                    title: "Paste pairing code".into(),
                    kind: FormKind::Pair,
                    fields: vec![Field::text("Pairing code", "", 8192)],
                    selected: 0,
                }))
            }
            Action::Pending(v) => {
                let peer = text(&v, "peer").to_owned();
                let name = clean(text(&v, "name"));
                let detail = vec![
                    format!("Device: {name}"),
                    format!("Full client ID: {peer}"),
                    format!("Invitation: {}", text(&v, "invitation")),
                    if v["mutual"].as_bool() == Some(true) {
                        "Both machines will be paired. The requester grants you no new service access. Choose what it may access here.".into()
                    } else {
                        "Choose services during approval. Existing grants are preserved.".into()
                    },
                ];
                let mut fields = vec![
                    Field {
                        label: "Device".into(),
                        value: name.clone(),
                        kind: FieldKind::ReadOnly,
                    },
                    Field {
                        label: "Full client ID".into(),
                        value: peer.clone(),
                        kind: FieldKind::ReadOnly,
                    },
                ];
                for service in self.services() {
                    let mut field = Field::check(
                        &format!(
                            "Grant {} · {} · {}",
                            clean(text(&service, "id")),
                            text(&service, "id"),
                            text(&service, "kind")
                        ),
                        service["id"].clone(),
                        false,
                    );
                    if service["kind"] == "files" && !self.files_configured() {
                        field.kind = FieldKind::ReadOnly;
                        field.value = "Set a shared folder before granting Files access".into();
                    } else if array(&service["allow"]).contains(&json!(peer)) {
                        field.kind = FieldKind::Granted;
                    }
                    fields.push(field);
                }
                let form = Form {
                    title: "Approve device and select services".into(),
                    kind: FormKind::Approval {
                        invitation: text(&v, "invitation").to_owned(),
                        peer,
                        name,
                        revision: self.revision(),
                    },
                    fields,
                    selected: 2,
                };
                self.choice("Review pairing request", detail.clone(), vec![
                    entry("approve", "Approve this device and choose services", Action::Form(form)),
                    entry("reject", "Reject this request", Action::Confirm { title: "Reject pairing".into(), detail, work: self.mutate(json!({"op":"reject_invitation","invitation":v["invitation"],"expected_revision":self.revision()})) }),
                ]);
            }
            Action::Machine(peer) => {
                let client = self.clients().iter().any(|v| v["peer"] == peer);
                let pinned = array(&self.status()["peer_pins"]).contains(&json!(peer));
                let detail = vec![
                    self.name(&peer),
                    format!("Full machine ID: {peer}"),
                    format!("Approved client here: {client}; saved host pin: {pinned}"),
                ];
                let mut choices = vec![];
                if pinned {
                    choices.push(entry(
                        "catalog",
                        "Refresh and browse remote services",
                        Action::Run(Work {
                            kind: Kind::Catalog(peer.clone()),
                            task: Task::Request(json!({"op":"catalog","host":peer})),
                        }),
                    ));
                }
                choices.push(entry("remove","Remove machine / revoke local access",Action::Confirm{title:"Remove this machine".into(),detail:vec![self.name(&peer),format!("Full ID: {peer}"),"Revokes its service grants here, ends active access, and forgets its saved pin and browser routes. The other machine will remove this pairing when it next contacts this machine. Offline or unavailable peers keep their record until then.".into()],work:Work{kind:Kind::Saved,task:Task::Remove{peer,revision:self.revision()}}}));
                self.choice("Manage machine", detail, choices);
            }
            Action::Service(service) => {
                if service["kind"] == "files" {
                    self.choice("Built-in Files", vec![
                        "Send files with wispnet files put DEVICE WORKSPACE LOCAL REMOTE.".into(),
                        "Configure exported directories with wispnet files-access --export NAME=/absolute/path.".into(),
                        "Permitted devices can send files into your configured folders without confirming each transfer.".into(),
                        "Choose a shared folder before granting Files access.".into(),
                    ], if self.files_configured() {
                        vec![entry("grants", "Edit permitted devices", Action::Form(self.files_form(&service)))]
                    } else { vec![] });
                    return;
                }
                if service["kind"] == "shell" {
                    self.choice("Built-in Shell",vec!["Shell is included with every desktop host. Choose which paired devices may open a terminal.".into()],vec![entry("open","Open shell",Action::Shell(text(&self.host(),"host").into(),service.clone())),entry("grants","Edit permitted devices",Action::Form(self.shell_form(&service)))]);
                    return;
                }
                let mut details = vec![
                    format!("Service: {}", clean(text(&service, "id"))),
                    format!("ID: {}", text(&service, "id")),
                    format!("Backend: {}", text(&service, "backend")),
                ];
                if let Some(publication) = array(&self.status()["public_access"]["services"])
                    .iter()
                    .find(|p| p["service"] == service["id"])
                {
                    details.push(format!("Public URL: {}", text(publication, "url")));
                    details.push(format!("Public status: {}", text(publication, "state")));
                }
                let mut choices = vec![entry(
                    "edit",
                    "Edit service and permitted devices",
                    Action::Form(self.service_form(Some(service.clone()))),
                )];
                if service["kind"] == "shell" {
                    choices.push(entry(
                        "shell",
                        "Open shell",
                        Action::Shell(text(&self.host(), "host").into(), service.clone()),
                    ));
                }
                if service["kind"] == "http" {
                    choices.push(entry(
                        "open",
                        "Open service / show browser URL",
                        Action::Run(Work {
                            kind: Kind::Open,
                            task: Task::Open {
                                host: text(&self.host(), "host").into(),
                                service: service.clone(),
                                name: text(&self.status(), "host_name").into(),
                                enable: self.status()["access_enabled"] != true,
                            },
                        }),
                    ));
                }
                choices.push(entry("remove","Remove service",Action::Confirm{title:"Remove local service".into(),detail:details.clone(),work:self.mutate(json!({"op":"remove","id":service["id"],"expected_revision":self.revision()}))}));
                if service["kind"] == "http" {
                    let enabled = array(&self.status()["public_access"]["services"])
                        .iter()
                        .any(|p| p["service"] == service["id"] && p["enabled"] != false);
                    choices.push(entry("public", if enabled {"Disable public access"}else{"Enable public access (anyone can connect)"},Action::Run(Work{kind:Kind::Saved,task:Task::Request(json!({"op":"configure_public","service":service["id"],"incarnation":service["incarnation"],"enabled":!enabled,"name":self.status()["host_name"]}))})));
                }
                self.choice("Manage service", details, choices);
            }
            Action::Copy(value) => {
                self.message =
                    match execute!(io::stdout(), CopyToClipboard::to_clipboard_from(value)) {
                        Ok(()) => {
                            "Copy requested. Allow clipboard access if your terminal asks.".into()
                        }
                        Err(_) => "Cannot copy link. Check terminal clipboard access.".into(),
                    };
            }
            Action::Remote(host, service) => {
                if service["kind"] == "files" {
                    self.choice(
                        "Files access",
                        vec![
                            format!("Host: {host}"),
                            format!("Send: wispnet files put {host} WORKSPACE LOCAL REMOTE"),
                            "Transfers resume automatically after interruption.".into(),
                        ],
                        vec![],
                    );
                    return;
                }
                if service["kind"] == "shell" {
                    self.action(Action::Shell(host, service));
                    return;
                }
                self.choice(
                    "Service access",
                    vec![format!("Service: {}", text(&service, "id"))],
                    vec![
                        entry(
                            "open",
                            "Open in browser",
                            Action::Run(Work {
                                kind: Kind::Open,
                                task: Task::Open {
                                    host: host.clone(),
                                    service: service.clone(),
                                    name: self.name(&host),
                                    enable: self.status()["access_enabled"] != true,
                                },
                            }),
                        ),
                        entry(
                            "links",
                            "Show Local, Wispnet and Public links",
                            Action::Run(Work {
                                kind: Kind::Links,
                                task: Task::Links { host, service },
                            }),
                        ),
                    ],
                );
            }
            Action::OwnerNew(approve) => {
                let path = self
                    .api
                    .config
                    .parent()
                    .unwrap_or(&self.api.config)
                    .join("admin-targets.json");
                let targets = if path.exists() {
                    match api::read_private(&path, 65536).and_then(|s| {
                        serde_json::from_str::<Value>(&s)
                            .map_err(|_| "Invalid owner targets file".into())
                    }) {
                        Ok(v) if v.is_array() && !array(&v).is_empty() => array(&v),
                        Ok(_) => {
                            self.message = "Owner targets must be a nonempty array".into();
                            return;
                        }
                        Err(error) => {
                            self.message = error;
                            return;
                        }
                    }
                } else {
                    vec![json!({"host":self.host()["host"]})]
                };
                let mut seen = std::collections::HashSet::new();
                let valid = targets.len() <= 32
                    && targets.iter().all(|target| {
                        target.as_object().is_some_and(|object| {
                            object.keys().all(|k| matches!(k.as_str(), "host" | "ssh"))
                        }) && text(target, "host")
                            .parse::<wispnet_core::identity::EndpointId>()
                            .is_ok()
                            && seen.insert(text(target, "host").to_owned())
                            && target.get("ssh").is_none_or(|v| {
                                v.as_str().is_some_and(|s| {
                                    !s.is_empty()
                                        && s.len() <= 254
                                        && s.as_bytes()[0].is_ascii_alphanumeric()
                                        && s.bytes().all(|c| {
                                            c.is_ascii_alphanumeric() || b"_.@-".contains(&c)
                                        })
                                })
                            })
                    });
                if !valid {
                    self.message = "Invalid owner targets. Use unique complete host IDs and explicit trusted SSH routes; go online to discover this host's ID.".into();
                    return;
                }
                let mut fields = vec![Field::text("Client public ID", "", 64)];
                if approve {
                    fields.push(Field::text("Device ID", "", 64));
                }
                for target in targets {
                    fields.push(Field::check(
                        &format!(
                            "Target {} · {}",
                            if text(&target, "ssh").is_empty() {
                                "This host"
                            } else {
                                text(&target, "ssh")
                            },
                            short(text(&target, "host"))
                        ),
                        target,
                        false,
                    ));
                }
                self.modal = Some(Modal::Form(Form {
                    title: if approve {
                        "Add client across hosts"
                    } else {
                        "Revoke client across hosts"
                    }
                    .into(),
                    kind: FormKind::Owner { approve },
                    fields,
                    selected: 0,
                }));
            }
            Action::OwnerDetail(value) => {
                let mut detail = vec![
                    format!("Operation ID: {}", text(&value, "id")),
                    format!("Action: {}", value["action"]),
                ];
                for target in array(&value["targets"]) {
                    detail.push(format!(
                        "{} · {} · revision {} · {}",
                        text(&target, "host"),
                        text(&target, "state"),
                        target["revision"],
                        clean(text(&target, "reason"))
                    ));
                }
                self.choice(
                    "Owner acknowledgements",
                    detail,
                    vec![entry(
                        "retry",
                        "Retry only pending owner targets",
                        Action::Run(Work {
                            kind: Kind::OwnerSaved,
                            task: Task::OwnerRetry(text(&value, "id").into()),
                        }),
                    )],
                );
            }
        }
    }
    fn save_form(&mut self, form: Form) -> Result<(), String> {
        match form.kind {
            FormKind::Settings => {
                let name = form.fields[0].value.trim().to_owned();
                if !wispnet_core::protocol::valid_service_id(&name) {
                    return Err("Invalid device ID".into());
                }
                let port: u16 = form.fields[1]
                    .value
                    .trim()
                    .parse()
                    .map_err(|_| "Port must be between 1024 and 65535")?;
                if port < 1024 {
                    return Err("Port must be between 1024 and 65535".into());
                }
                self.confirmation(
                    "Save device settings".into(),
                    vec![
                        format!("Device ID: {name}"),
                        format!("Shared HTTP port: {port}"),
                        "Changing the device ID restarts the installed node.".into(),
                    ],
                    Work {
                        kind: Kind::Saved,
                        task: Task::SaveSettings { name, port },
                    },
                );
            }
            FormKind::Link => {
                let link = form.fields[0].value.trim();
                wispnet_core::service_link::ServiceLink::parse(link)?;
                self.send(Work {
                    kind: Kind::Open,
                    task: Task::ResolveLink(link.to_owned()),
                });
            }
            FormKind::Approval {
                invitation,
                peer,
                name,
                revision,
            } => {
                let chosen: Vec<&Field> = form
                    .fields
                    .iter()
                    .filter(|f| matches!(f.kind, FieldKind::Check(true) | FieldKind::Granted))
                    .collect();
                let services: Vec<Value> = chosen
                    .iter()
                    .map(|f| {
                        serde_json::from_str(&f.value)
                            .map_err(|_| "Invalid selected service".to_string())
                    })
                    .collect::<Result<_, _>>()?;
                if services.iter().any(|s| s == "files") && !self.files_configured() {
                    return Err("Set a shared folder before granting Files access".into());
                }
                let mut detail = vec![format!("Device: {name}"), format!("Full client ID: {peer}"), "Pairing and the selected grants will be saved together. Existing grants are preserved.".into()];
                if services.iter().any(|s| s == "files") {
                    detail.push("Files permission lets this device send files into your configured folders without confirming each transfer.".into());
                }
                if chosen.is_empty() {
                    detail.push("No services selected: pair without new service access.".into());
                } else {
                    detail.extend(chosen.iter().map(|f| f.label.clone()));
                }
                self.confirmation("Approve pairing and service access".into(), detail, self.mutate(json!({"op":"approve_invitation","invitation":invitation,"expected_revision":revision,"services":services})));
            }
            FormKind::Files { revision, exports } => {
                let allow = form
                    .fields
                    .iter()
                    .filter(|f| matches!(f.kind, FieldKind::Check(true)))
                    .map(|f| {
                        serde_json::from_str::<Value>(&f.value)
                            .map_err(|_| "Invalid selected device")
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !allow.is_empty() && exports.as_object().is_none_or(|roots| roots.is_empty()) {
                    return Err("Set a shared folder before granting Files access".into());
                }
                self.confirmation("Save Files access".into(),
                    vec![format!("{} permitted devices", allow.len()), "These devices may send files into your configured folders without confirming each transfer.".into()],
                    self.mutate(json!({"op":"files","allow":allow,"exports":exports,"expected_revision":revision})));
            }
            FormKind::Shell { revision } => {
                let allow = form
                    .fields
                    .iter()
                    .filter(|f| matches!(f.kind, FieldKind::Check(true)))
                    .map(|f| {
                        serde_json::from_str::<Value>(&f.value)
                            .map_err(|_| "Invalid selected device")
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.confirmation("Save Shell access".into(),vec![format!("{} permitted devices",allow.len()),"Only these devices will be allowed to open a shell. Existing remote sessions lose access when their grant is removed.".into()],self.mutate(json!({"op":"shell","allow":allow,"expected_revision":revision})));
            }
            FormKind::Service { existing, revision } => {
                let name = form.fields[0].value.trim();
                if matches!(name, "shell" | "files") {
                    return Err("shell and files are reserved for built-in services".into());
                }
                if !wispnet_core::protocol::valid_service_id(name) {
                    return Err("Enter a valid service ID".into());
                }
                let FieldKind::Choice(ref choices, index) = form.fields[2].kind else {
                    return Err("Missing protocol".into());
                };
                let shell = choices[index] == "shell";
                let port = form.fields[1].value.trim();
                let backend = if shell {
                    "127.0.0.1:0".to_owned()
                } else if port.chars().all(|c| c.is_ascii_digit()) {
                    format!("127.0.0.1:{port}")
                } else if port.starts_with(':') {
                    format!("127.0.0.1{port}")
                } else {
                    port.into()
                };
                let address: std::net::SocketAddr = backend
                    .parse()
                    .map_err(|_| "Enter a loopback address or local port (for example 8201)")?;
                if !address.ip().is_loopback() || (!shell && address.port() == 0) {
                    return Err(
                        "Service backend must be a loopback address with a nonzero port".into(),
                    );
                }
                let id = name.to_owned();
                if existing.as_ref().is_some_and(|old| old != &id) {
                    return Err(
                        "Existing service IDs are used by URLs and grants and cannot be changed"
                            .into(),
                    );
                }
                let allow: Vec<Value> = form
                    .fields
                    .iter()
                    .skip(3)
                    .filter(|f| matches!(f.kind, FieldKind::Check(true)))
                    .map(|f| {
                        serde_json::from_str(&f.value)
                            .map_err(|_| "Invalid selected client".to_string())
                    })
                    .collect::<Result<_, _>>()?;
                let replace = existing.is_none() && self.services().iter().any(|s| s["id"] == id);
                let detail = vec![
                    format!("Service ID: {id}"),
                    format!("Backend: {backend} · {}", choices[index]),
                    format!(
                        "Permitted devices: {}",
                        allow
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|p| format!("{} ({p})", self.name(p)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    if replace {
                        "An existing service has this ID. Confirming replaces its backend and entire access list.".into()
                    } else {
                        "Only the selected devices will be allowed.".into()
                    },
                ];
                self.confirmation(if replace{"Replace existing service"}else{"Save service and grants"}.into(),detail,self.mutate(json!({"op":"service","id":id,"backend":backend,"kind":choices[index],"allow":allow,"expected_revision":revision})));
            }
            FormKind::Pair => {
                let document: String = form.fields[0].value.split_whitespace().collect();
                if !document.starts_with("wispnet1:") || document.len() > 8192 {
                    return Err("Enter a Wispnet pairing code of at most 8192 bytes".into());
                }
                self.modal = None;
                self.send(Work {
                    kind: Kind::Preview(document.clone()),
                    task: Task::Request(json!({"op":"preview_invitation","invitation":document})),
                });
            }
            FormKind::Owner { approve } => {
                let peer = form.fields[0].value.trim();
                peer.parse::<wispnet_core::identity::EndpointId>()
                    .map_err(|_| "Enter the complete client public ID")?;
                let mut action = json!({"op":if approve{"approve"}else{"revoke"},"peer":peer});
                if approve {
                    let name = form.fields[1].value.trim();
                    if !wispnet_core::enrollment::valid_name(name) {
                        return Err("Enter a valid client name".into());
                    }
                    action["name"] = json!(name);
                }
                let targets: Vec<Value> = form
                    .fields
                    .iter()
                    .skip(if approve { 2 } else { 1 })
                    .filter(|f| matches!(f.kind, FieldKind::Check(true)))
                    .map(|f| {
                        serde_json::from_str(&f.value)
                            .map_err(|_| "Invalid owner target".to_string())
                    })
                    .collect::<Result<_, _>>()?;
                if targets.is_empty() {
                    return Err("Select at least one owner target".into());
                }
                let mut detail=vec![format!("Client: {peer}"),"Each selected host acts independently. Offline hosts remain pending. Approval grants no service access automatically.".into()];
                detail.extend(targets.iter().map(|t| {
                    format!(
                        "{} · {}",
                        if text(t, "ssh").is_empty() {
                            "This host"
                        } else {
                            text(t, "ssh")
                        },
                        text(t, "host")
                    )
                }));
                self.confirmation(
                    "Apply across selected owner hosts".into(),
                    detail,
                    Work {
                        kind: Kind::OwnerSaved,
                        task: Task::OwnerStart(json!({"action":action,"targets":targets})),
                    },
                );
            }
        }
        Ok(())
    }
    fn receive(&mut self) {
        while let Ok(reply) = self.rx.try_recv() {
            self.busy = false;
            let old_id = self.entries().get(self.selected).map(|e| e.id.clone());
            match reply.result {
                Err(error) => {
                    if matches!(reply.kind, Kind::Status) {
                        self.online = false;
                    }
                    if let Kind::Catalog(ref peer) = reply.kind {
                        self.catalogs.remove(peer);
                        self.catalog_errors.insert(peer.clone(), error.clone());
                    }
                    self.message = clean(&error);
                    // Do not replay a mutation or silently substitute a newer revision.
                    if !matches!(reply.kind, Kind::Status) {
                        self.modal = Some(Modal::Info {
                            title: if error == "awaiting approval" {
                                "Awaiting approval"
                            } else {
                                "Operation not completed"
                            }
                            .into(),
                            lines: vec![
                                clean(&error),
                                "Refresh and review the current state before trying again.".into(),
                            ],
                            scroll: 0,
                        });
                        self.last_poll = Instant::now() - Duration::from_secs(5);
                    }
                }
                Ok(value) => match reply.kind {
                    Kind::Status => {
                        self.snapshot = Some(value);
                        self.online = true;
                        self.names = self.api.names();
                        if self.message.starts_with("Working")
                            || self.message.starts_with("Connecting")
                        {
                            self.message = "Ready".into();
                        }
                    }
                    Kind::Saved => {
                        self.message = "Saved. Refreshing node state…".into();
                        self.last_poll = Instant::now() - Duration::from_secs(5);
                    }
                    Kind::Qr => {
                        self.qr = Some(value);
                        self.page = Page::Qr;
                        self.menu = Page::Qr.index();
                        self.menu_focus = false;
                        self.message="Copy the code to another computer or scan its QR on your phone. Requests appear automatically.".into();
                    }
                    Kind::Catalog(peer) => {
                        self.catalog_errors.remove(&peer);
                        self.catalogs.insert(peer.clone(), value.clone());
                        let services = array(&value["services"]);
                        let entries = services
                            .iter()
                            .map(|s| {
                                entry(
                                    text(s, "id"),
                                    format!("{} · {}", clean(text(s, "id")), text(s, "kind")),
                                    Action::Remote(peer.clone(), s.clone()),
                                )
                            })
                            .collect();
                        self.choice("Remote services",vec![self.name(&peer),format!("Full host ID: {peer}"),if services.is_empty(){"No services granted here, or request still awaits approval on the target host.".into()}else{"These are the services this host currently permits you to access.".into()}],entries);
                        self.message = "Authenticated catalog refreshed".into();
                    }
                    Kind::Preview(document) => {
                        let detail=vec![format!("Host: {}",clean(text(&value,"host_name"))),format!("Compare full host ID: {}",text(&value,"host")),format!("Relay: {}",clean(text(&value,"relay_url"))),"Compare this ID with the target computer before confirming. Both machines will be paired after approval. The target owner chooses services to grant; this computer grants no new service access. You can allow services later in service settings.".into()];
                        self.confirmation(
                            "Request pairing with this host".into(),
                            detail,
                            Work {
                                kind: Kind::Paired,
                                task: Task::Pair {
                                    preview: value,
                                    document,
                                },
                            },
                        );
                    }
                    Kind::Paired => {
                        self.catalog_errors
                            .insert(text(&value, "host").into(), "awaiting approval".into());
                        self.names = self.api.names();
                        self.catalogs.remove(text(&value, "host"));
                        self.message="Request sent. Choose allowed services on the target host when approving. This computer grants no new service access.".into();
                        self.last_poll = Instant::now() - Duration::from_secs(5);
                    }
                    Kind::Open => {
                        let tcp = text(&value, "url").starts_with("tcp://");
                        let (title, detail) = if tcp {
                            (
                                "TCP connection",
                                "Connect a native app on this computer to this loopback address while Wispnet is online.",
                            )
                        } else {
                            (
                                "Browser service",
                                "This URL belongs to this computer. The browser opens automatically on graphical sessions; WebSocket clients can use the equivalent ws:// URL.",
                            )
                        };
                        self.modal = Some(Modal::Info {
                            title: title.into(),
                            lines: vec![text(&value, "url").into(), detail.into()],
                            scroll: 0,
                        });
                        self.message = if tcp {
                            "TCP forward ready"
                        } else {
                            "Browser route ready"
                        }
                        .into();
                        self.last_poll = Instant::now() - Duration::from_secs(5);
                    }
                    Kind::Settings => {
                        self.modal = Some(Modal::Form(Form {
                            title: "Device settings".into(),
                            kind: FormKind::Settings,
                            fields: vec![
                                Field::text("Device ID", text(&self.status(), "host_name"), 64),
                                Field::text(
                                    "Shared HTTP port",
                                    &value["settings"]["registry_port"].to_string(),
                                    5,
                                ),
                            ],
                            selected: 0,
                        }));
                    }
                    Kind::Links => {
                        let mut lines = vec![
                            format!("Local: {}", text(&value, "local")),
                            format!("Wispnet (Android): {}", text(&value, "wispnet")),
                        ];
                        if let Some(url) = value["public"].as_str() {
                            lines.push(format!("Public: {url}"));
                        }
                        lines.push("Local links use this computer's shared HTTP port. Wispnet links resolve on the receiving device and require pairing and permission.".into());
                        let mut choices = vec![
                            entry(
                                "local",
                                "Copy Local URL",
                                Action::Copy(text(&value, "local").into()),
                            ),
                            entry(
                                "wispnet",
                                "Copy Wispnet URL",
                                Action::Copy(text(&value, "wispnet").into()),
                            ),
                        ];
                        if let Some(url) = value["public"].as_str() {
                            choices.push(entry(
                                "public",
                                "Copy Public URL",
                                Action::Copy(url.into()),
                            ));
                        }
                        self.choice("Service links", lines, choices);
                    }
                    Kind::Owners => {
                        self.operations = array(&value);
                        self.message = "Owner journals loaded".into();
                    }
                    Kind::OwnerSaved => {
                        self.operations.retain(|v| v["id"] != value["id"]);
                        self.operations.push(value.clone());
                        self.action(Action::OwnerDetail(value));
                        self.message = "Review each host's acknowledgement".into();
                        self.last_poll = Instant::now() - Duration::from_secs(5);
                    }
                },
            }
            if let Some(id) = old_id {
                if let Some(index) = self.entries().iter().position(|e| e.id == id) {
                    self.selected = index;
                } else {
                    self.selected = self.selected.min(self.entries().len().saturating_sub(1));
                }
            }
        }
        if !self.busy
            && let Some(work) = self.pending_work.take()
        {
            self.send(work);
        }
        if let Some(qr) = &self.qr {
            let pending = self
                .pending()
                .into_iter()
                .find(|v| v["invitation"] == qr["invitation"]);
            if let Some(pending) = pending {
                self.qr = None;
                self.navigate(Page::Pending);
                self.selected = self
                    .pending()
                    .iter()
                    .position(|v| v["invitation"] == pending["invitation"])
                    .unwrap_or(0);
                self.message =
                    "Pairing request received. Select the device and press Enter to review.".into();
            } else if array(&self.status()["invitations"]).iter().any(|v| {
                v["invitation"] == qr["invitation"]
                    && !matches!(text(v, "state"), "issued" | "challenged")
            }) {
                self.qr = None;
                self.message = "Invitation is no longer active. Press n for a new code.".into();
            } else if qr["expires_at"].as_u64().unwrap_or(0) <= now() {
                self.qr = None;
                self.message = "Pairing code expired. Press n to generate another.".into();
            }
        }
        self.refresh_catalogs();
        if !self.busy && self.last_poll.elapsed() >= Duration::from_secs(3) {
            self.last_poll = Instant::now();
            self.send(Work {
                kind: Kind::Status,
                task: Task::Status,
            });
        }
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if let Some(mut modal) = self.modal.take() {
            if key.code == KeyCode::Esc {
                return false;
            }
            match &mut modal {
                Modal::Info { scroll, .. } => match key.code {
                    KeyCode::Down | KeyCode::PageDown => *scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::PageUp => *scroll = scroll.saturating_sub(1),
                    KeyCode::Enter | KeyCode::Char('q') => return false,
                    _ => {}
                },
                Modal::Choice {
                    entries,
                    selected,
                    scroll,
                    ..
                } => {
                    let len = entries.len();
                    match key.code {
                        KeyCode::PageDown => *scroll = scroll.saturating_add(3),
                        KeyCode::PageUp => *scroll = scroll.saturating_sub(3),
                        KeyCode::Down | KeyCode::Tab => *selected = (*selected + 1) % len.max(1),
                        KeyCode::Up | KeyCode::BackTab => {
                            *selected = (*selected + len.saturating_sub(1)) % len.max(1)
                        }
                        KeyCode::Enter => {
                            if let Some(row) = entries.get(*selected) {
                                self.action(row.action.clone());
                            }
                            return false;
                        }
                        _ => {}
                    }
                }
                Modal::Form(form) => {
                    let count = form.fields.len() + 1;
                    match key.code {
                        KeyCode::PageUp if matches!(form.kind, FormKind::Pair) => {
                            self.code_scroll = self.code_scroll.saturating_add(5)
                        }
                        KeyCode::PageDown if matches!(form.kind, FormKind::Pair) => {
                            self.code_scroll = self.code_scroll.saturating_sub(5)
                        }
                        KeyCode::Down | KeyCode::Tab => form.selected = (form.selected + 1) % count,
                        KeyCode::Up | KeyCode::BackTab => {
                            form.selected = (form.selected + count - 1) % count
                        }
                        KeyCode::Enter if form.selected == form.fields.len() => {
                            let saved = form.clone();
                            if let Err(error) = self.save_form(saved) {
                                self.message = error;
                                self.modal = Some(modal);
                            }
                            return false;
                        }
                        _ => {
                            if let Some(field) = form.fields.get_mut(form.selected) {
                                match &mut field.kind {
                                    FieldKind::ReadOnly | FieldKind::Granted => {
                                        if key.code == KeyCode::Enter {
                                            form.selected = (form.selected + 1) % count;
                                        }
                                    }
                                    FieldKind::Check(v) => {
                                        if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                                            *v = !*v;
                                        }
                                    }
                                    FieldKind::Choice(choices, index) => match key.code {
                                        KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                                            *index = (*index + 1) % choices.len()
                                        }
                                        KeyCode::Left => {
                                            *index = (*index + choices.len() - 1) % choices.len()
                                        }
                                        _ => {}
                                    },
                                    FieldKind::Text(limit) => match key.code {
                                        KeyCode::Char('u')
                                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                        {
                                            field.value.clear()
                                        }
                                        KeyCode::Char(c)
                                            if !key.modifiers.intersects(
                                                KeyModifiers::CONTROL | KeyModifiers::ALT,
                                            ) && field.value.len() + c.len_utf8() <= *limit =>
                                        {
                                            field.value.push(c)
                                        }
                                        KeyCode::Backspace => {
                                            field.value.pop();
                                        }
                                        KeyCode::Enter => {
                                            form.selected = (form.selected + 1) % count
                                        }
                                        _ => {}
                                    },
                                }
                            }
                        }
                    }
                }
            }
            self.modal = Some(modal);
            return false;
        }
        match key.code {
            KeyCode::Char('q') => {
                if self.busy {
                    self.message="Operation in progress. Wait for its result, or Ctrl+C to exit with an unconfirmed result.".into();
                } else {
                    return true;
                }
            }
            KeyCode::Esc => {
                self.qr = None;
                self.navigate(Page::Machines);
            }
            KeyCode::Tab | KeyCode::BackTab => self.menu_focus = !self.menu_focus,
            KeyCode::Char('r') if !self.busy => {
                self.catalog_checked.clear();
                self.last_poll = Instant::now() - Duration::from_secs(5);
            }
            KeyCode::Char('g') => self.action(Action::Qr),
            KeyCode::Char('n') => self.action(Action::Code),
            KeyCode::Char('c') if self.page == Page::Qr => {
                if let Some(code) = self.active_pairing_code() {
                    self.message =
                        match execute!(io::stdout(), CopyToClipboard::to_clipboard_from(code)) {
                            Ok(()) => {
                                "Copy requested. Allow clipboard access if your terminal asks."
                                    .into()
                            }
                            Err(_) => {
                                "Cannot copy pairing code. Check terminal clipboard access.".into()
                            }
                        };
                } else {
                    self.message =
                        "Pairing code expired or unavailable. Press n for a new code.".into();
                }
            }
            KeyCode::Down | KeyCode::PageDown if self.page == Page::Qr && self.show_code => {
                self.code_scroll = self
                    .code_scroll
                    .saturating_add(if key.code == KeyCode::PageDown { 5 } else { 1 });
            }
            KeyCode::Up | KeyCode::PageUp if self.page == Page::Qr && self.show_code => {
                self.code_scroll = self
                    .code_scroll
                    .saturating_sub(if key.code == KeyCode::PageUp { 5 } else { 1 });
            }
            KeyCode::Char('p') => self.navigate(Page::Pending),
            KeyCode::Char(c @ '1'..='8') => self.navigate(Page::at(c as usize - '1' as usize)),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.menu_focus {
                    self.menu = (self.menu + 1) % 8;
                } else {
                    self.select_row(true);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.menu_focus {
                    self.menu = (self.menu + 7) % 8;
                } else {
                    self.select_row(false);
                }
            }
            KeyCode::Enter => {
                if self.menu_focus {
                    self.navigate(Page::at(self.menu));
                } else if let Some(row) = self.entries().get(self.selected) {
                    self.action(row.action.clone());
                }
            }
            _ => {}
        }
        false
    }
    fn select_row(&mut self, forward: bool) {
        let entries = self.entries();
        let count = entries.len().max(1);
        for _ in 0..count {
            self.selected = (self.selected + if forward { 1 } else { count - 1 }) % count;
            if !entries
                .get(self.selected)
                .is_some_and(|row| matches!(row.action, Action::Section))
            {
                break;
            }
        }
    }
    fn paste(&mut self, value: &str) {
        if let Some(Modal::Form(form)) = &mut self.modal
            && let Some(field) = form.fields.get_mut(form.selected)
            && let FieldKind::Text(limit) = field.kind
        {
            if field.value.len() + value.len() <= limit {
                field.value.push_str(value);
            } else {
                self.message = format!("Input exceeds {limit} bytes");
            }
        }
    }
    fn mouse(&mut self, event: event::MouseEvent) {
        let found = self
            .hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains((event.column, event.row).into()))
            .map(|(_, h)| h.clone());
        if event.kind == MouseEventKind::ScrollDown {
            self.key(KeyCode::Down.into());
            return;
        }
        if event.kind == MouseEventKind::ScrollUp {
            self.key(KeyCode::Up.into());
            return;
        }
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        match found {
            Some(Hit::Nav(n)) if self.modal.is_none() => {
                self.navigate(Page::at(n));
            }
            Some(Hit::Row(n)) if self.modal.is_none() => {
                self.menu_focus = false;
                self.selected = n;
                let double = self.last_click.is_some_and(|(when, row)| {
                    row == n && when.elapsed() < Duration::from_millis(400)
                });
                self.last_click = Some((Instant::now(), n));
                if double {
                    self.key(KeyCode::Enter.into());
                }
            }
            Some(Hit::ModalRow(n)) => {
                if let Some(Modal::Choice { selected, .. }) = &mut self.modal {
                    *selected = n;
                }
                self.key(KeyCode::Enter.into());
            }
            Some(Hit::Field(n)) => {
                let toggle = if let Some(Modal::Form(form)) = &mut self.modal {
                    form.selected = n;
                    n == form.fields.len()
                        || form
                            .fields
                            .get(n)
                            .is_some_and(|f| !matches!(f.kind, FieldKind::Text(_)))
                } else {
                    false
                };
                if toggle {
                    self.key(KeyCode::Enter.into());
                }
            }
            _ => {}
        }
    }
    fn draw(&mut self, frame: &mut Frame) {
        self.hits.clear();
        let full = frame.area();
        let area = Rect {
            height: full.height.saturating_sub(1),
            ..full
        };
        frame.render_widget(
            Paragraph::new(concat!("v", env!("WISPNET_VERSION"))).alignment(Alignment::Right),
            Rect {
                y: full.y + full.height.saturating_sub(1),
                height: full.height.min(1),
                ..full
            },
        );
        let base = Style::default().fg(Color::Gray).bg(Color::Rgb(10, 16, 29));
        frame.render_widget(Block::default().style(base), area);
        if full.width < 65 || full.height < 16 {
            frame.render_widget(Paragraph::new("Wispnet — enlarge this terminal to at least 65 columns × 16 rows.\nq: exit · Ctrl+C: exit immediately").wrap(Wrap{trim:false}),area);
            return;
        }
        let parts = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);
        let status = self.status();
        let heading = Line::from(vec![
            Span::styled(
                " WISPNET  ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                clean(text(&status, "host_name")),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("   {}", if self.online { "ONLINE" } else { "OFFLINE" }),
                Style::default().fg(if self.online {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
            Span::raw(format!("   ·   {} requests", self.pending().len())),
        ]);
        let summary = Line::styled(
            format!(
                " relay {}   ·   {} devices   ·   {} services{}",
                text(&status, "relay"),
                self.clients().len(),
                self.services().len(),
                if self.busy {
                    "   ·   refreshing…"
                } else {
                    ""
                }
            ),
            Style::default().fg(Color::DarkGray),
        );
        frame.render_widget(
            Paragraph::new(vec![heading, summary]).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            parts[0],
        );
        if self.page == Page::Qr {
            self.draw_qr(frame, parts[1]);
        } else {
            let body =
                Layout::horizontal([Constraint::Length(23), Constraint::Min(20)]).split(parts[1]);
            let menu_items: Vec<ListItem> = PAGES
                .iter()
                .enumerate()
                .map(|(n, s)| {
                    ListItem::new(format!(
                        "{} {s}{}",
                        n + 1,
                        if n == Page::Pending.index() && !self.pending().is_empty() {
                            format!(" ({})", self.pending().len())
                        } else {
                            String::new()
                        }
                    ))
                })
                .collect();
            let mut menu_state = ListState::default().with_selected(Some(self.menu));
            let block = Block::bordered()
                .title(if self.menu_focus {
                    " Navigate · Enter "
                } else {
                    " Navigate · Tab "
                })
                .border_style(Style::default().fg(if self.menu_focus {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }));
            frame.render_stateful_widget(
                List::new(menu_items)
                    .block(block)
                    .highlight_style(Style::default().bg(Color::Rgb(31, 55, 84)).fg(Color::White))
                    .highlight_symbol("› "),
                body[0],
                &mut menu_state,
            );
            for n in 0..8 {
                self.hits.push((
                    Rect::new(
                        body[0].x + 1,
                        body[0].y + 1 + n as u16,
                        body[0].width - 2,
                        1,
                    ),
                    Hit::Nav(n),
                ));
            }
            let content = if full.width >= 110 {
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .split(body[1])
            } else {
                Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .split(body[1])
            };
            let entries = self.entries();
            self.selected = self.selected.min(entries.len().saturating_sub(1));
            self.list_state.select(if entries.is_empty() {
                None
            } else {
                Some(self.selected)
            });
            let items: Vec<ListItem> = entries
                .iter()
                .map(|e| {
                    let item = ListItem::new(clean(&e.label));
                    if matches!(e.action, Action::Section) {
                        item.style(
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        item
                    }
                })
                .collect();
            let mut title = PAGES[self.page.index()].to_owned();
            if self.page == Page::Pending {
                title.push_str(" · Enter to review");
            }
            let block =
                Block::bordered()
                    .title(format!(" {title} "))
                    .border_style(Style::default().fg(if self.menu_focus {
                        Color::DarkGray
                    } else {
                        Color::Cyan
                    }));
            if entries.is_empty() {
                frame.render_widget(
                    Paragraph::new(if self.page == Page::Pending {
                        "No pending pairing requests. Press g to generate a QR invitation."
                    } else {
                        "No machines paired. Generate a QR or pair a remote host."
                    })
                    .wrap(Wrap { trim: false })
                    .block(block),
                    content[0],
                );
            } else {
                frame.render_stateful_widget(
                    List::new(items)
                        .block(block)
                        .highlight_symbol("› ")
                        .highlight_style(
                            Style::default().fg(Color::White).bg(Color::Rgb(31, 55, 84)),
                        ),
                    content[0],
                    &mut self.list_state,
                );
            }
            for (n, row) in entries
                .iter()
                .enumerate()
                .skip(self.list_state.offset())
                .take(usize::from(content[0].height.saturating_sub(2)))
            {
                if matches!(row.action, Action::Section) {
                    continue;
                }
                self.hits.push((
                    Rect::new(
                        content[0].x + 1,
                        content[0].y + 1 + (n - self.list_state.offset()) as u16,
                        content[0].width - 2,
                        1,
                    ),
                    Hit::Row(n),
                ));
            }
            let details = entries
                .get(self.selected)
                .map(|v| {
                    if v.detail.is_empty() {
                        format!("{}\n\nEnter  Open this action", v.label)
                    } else {
                        v.detail.join("\n")
                    }
                })
                .unwrap_or_else(|| {
                    "No requests to review.\n\nPair a device to share your services.".into()
                });
            frame.render_widget(
                Paragraph::new(details).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .title(" Selected ")
                        .border_style(Style::default().fg(Color::DarkGray)),
                ),
                content[1],
            );
        }
        let shortcuts = if self.page == Page::Qr {
            "c copy · n new code · g QR · Esc devices · q quit"
        } else if self.menu_focus {
            "↑↓ navigate · Enter open · Tab content · q quit"
        } else if full.width < 100 {
            "↑↓ select · Enter open · Tab menu · n pair · r refresh · q quit"
        } else {
            "↑↓ select · Enter open · Tab navigate · n pair · p requests · r refresh · q quit"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(clean(&self.message)),
                Line::styled(shortcuts, Style::default().fg(Color::DarkGray)),
            ]),
            parts[2],
        );
        if self.modal.is_some() {
            self.draw_modal(frame, area);
        }
    }
    fn active_pairing_code(&self) -> Option<&str> {
        self.qr
            .as_ref()
            .filter(|value| value["expires_at"].as_u64().unwrap_or(0) > now())
            .and_then(|value| value["code"].as_str())
    }
    fn draw_pairing_code(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" Pairing code · c: copy · n: new code · g: QR ");
        let inside = block.inner(area);
        frame.render_widget(block, area);
        let Some(code) = self.active_pairing_code() else {
            frame.render_widget(
                Paragraph::new(if self.busy {
                    "Generating pairing code…"
                } else {
                    "Press n to generate a five-minute pairing code."
                }),
                inside,
            );
            return;
        };
        let parts = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inside);
        frame.render_widget(Paragraph::new(format!("Paste this code into Pair remote host on the other computer.\nExpires in {} seconds · keep this code private",self.qr.as_ref().unwrap()["expires_at"].as_u64().unwrap_or(0).saturating_sub(now()))).wrap(Wrap { trim: false }),parts[0]);
        let lines = code.len().div_ceil(usize::from(parts[1].width.max(1)));
        let scroll = self
            .code_scroll
            .min(lines.saturating_sub(usize::from(parts[1].height)) as u16);
        frame.render_widget(
            Paragraph::new(code)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            parts[1],
        );
        frame.render_widget(Paragraph::new("c: copy complete code · ↑↓/PgUp/PgDn: scroll\nApprove the incoming request and choose allowed services here.").wrap(Wrap { trim: false }),parts[2]);
    }
    fn draw_qr(&self, frame: &mut Frame, area: Rect) {
        if self.show_code {
            self.draw_pairing_code(frame, area);
            return;
        }
        let block = Block::bordered().title(" Pair with this computer · g: new QR · Esc: back ");
        let inside = block.inner(area);
        frame.render_widget(block, area);
        let Some(qr) = &self.qr else {
            frame.render_widget(
                Paragraph::new(if self.busy {
                    "Generating invitation…"
                } else {
                    "Press g to generate a five-minute pairing QR."
                }),
                inside,
            );
            return;
        };
        let matrix = array(&qr["matrix"]);
        let width = matrix.len();
        let height = width.div_ceil(2);
        if usize::from(inside.width) < width || usize::from(inside.height) < height + 5 {
            frame.render_widget(Paragraph::new(format!("Enlarge the terminal or reduce its font size.\nThis QR needs at least {} columns × {} rows.\nThe code will not be drawn while it would wrap or clip.\nPress Esc to go back.",width+2,height+14)).wrap(Wrap{trim:false}),inside);
            return;
        }
        let mut lines = vec![
            Line::raw("In Wispnet on your phone, choose Scan pairing QR."),
            Line::raw(format!("Host: {}", text(qr, "host"))),
            Line::raw(format!(
                "Expires in {} seconds · keep this QR private",
                qr["expires_at"].as_u64().unwrap_or(0).saturating_sub(now())
            )),
        ];
        for pair in matrix.chunks(2) {
            let top = array(&pair[0]);
            let bottom = pair.get(1).map(array).unwrap_or_default();
            let text: String = top
                .iter()
                .enumerate()
                .map(
                    |(x, p)| match (p == true, bottom.get(x) == Some(&Value::Bool(true))) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    },
                )
                .collect();
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(Color::Black).bg(Color::White),
            )));
        }
        lines.push(Line::raw(
            "After scanning, select the incoming request and approve it.",
        ));
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inside);
    }
    fn draw_modal(&mut self, frame: &mut Frame, area: Rect) {
        let rect = Rect::new(
            area.x + 3,
            area.y + 2,
            area.width.saturating_sub(6),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let modal = self.modal.as_ref().unwrap();
        let title = match modal {
            Modal::Choice { title, .. } | Modal::Info { title, .. } => title.clone(),
            Modal::Form(form) => form.title.clone(),
        };
        let block = Block::bordered()
            .title(format!(" {title} · Esc: cancel/back "))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Rgb(12, 21, 39)).fg(Color::White));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        match modal {
            Modal::Info { lines, scroll, .. } => {
                let text = lines
                    .iter()
                    .map(|s| clean(s))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0)),
                    inner,
                );
            }
            Modal::Choice {
                detail,
                entries,
                selected,
                scroll,
                ..
            } => {
                let choice_height = (entries.len() as u16 + 2).min(inner.height / 2).max(2);
                let areas =
                    Layout::vertical([Constraint::Min(2), Constraint::Length(choice_height)])
                        .split(inner);
                frame.render_widget(
                    Paragraph::new(
                        detail
                            .iter()
                            .map(|s| clean(s))
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                    )
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0)),
                    areas[0],
                );
                let mut state = ListState::default().with_selected(if entries.is_empty() {
                    None
                } else {
                    Some(*selected)
                });
                frame.render_stateful_widget(
                    List::new(
                        entries
                            .iter()
                            .map(|e| ListItem::new(clean(&e.label)))
                            .collect::<Vec<_>>(),
                    )
                    .block(
                        Block::default()
                            .borders(Borders::TOP)
                            .title(" Choose · Enter/click · PgUp/PgDn: details "),
                    )
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().bg(Color::Rgb(31, 55, 84)).fg(Color::Cyan)),
                    areas[1],
                    &mut state,
                );
                for n in state.offset()
                    ..entries
                        .len()
                        .min(state.offset() + usize::from(areas[1].height.saturating_sub(1)))
                {
                    self.hits.push((
                        Rect::new(
                            areas[1].x,
                            areas[1].y + 1 + (n - state.offset()) as u16,
                            areas[1].width,
                            1,
                        ),
                        Hit::ModalRow(n),
                    ));
                }
            }
            Modal::Form(form) => {
                if matches!(form.kind, FormKind::Pair) {
                    let parts =
                        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(inner);
                    let code_box = Block::bordered().title(" Pairing code ").border_style(
                        Style::default().fg(if form.selected == 0 {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        }),
                    );
                    let content = code_box.inner(parts[0]);
                    let value = clean(&form.fields[0].value);
                    let lines = value.len().div_ceil(usize::from(content.width.max(1)));
                    let bottom = lines.saturating_sub(usize::from(content.height));
                    let scroll = bottom.saturating_sub(usize::from(self.code_scroll));
                    frame.render_widget(
                        Paragraph::new(value)
                            .wrap(Wrap { trim: false })
                            .scroll((scroll as u16, 0))
                            .block(code_box),
                        parts[0],
                    );
                    frame.render_widget(
                        Paragraph::new(vec![
                            Line::styled(
                                "[ Review and save ]",
                                Style::default().fg(if form.selected == 1 {
                                    Color::Cyan
                                } else {
                                    Color::Green
                                }),
                            ),
                            Line::raw("Tab: review button · Enter: next/review · Ctrl+U: clear"),
                            Line::raw("PgUp/PgDn: scroll · Paste the complete pairing code"),
                        ]),
                        parts[1],
                    );
                    self.hits.push((parts[0], Hit::Field(0)));
                    self.hits.push((
                        Rect::new(parts[1].x, parts[1].y, parts[1].width, 1),
                        Hit::Field(1),
                    ));
                    return;
                }
                let mut items: Vec<ListItem> = form
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(n, f)| {
                        let value = match &f.kind {
                            FieldKind::ReadOnly => clean(&f.value),
                            FieldKind::Granted => "[x] Already granted (preserved)".into(),
                            FieldKind::Text(_) => clean(&f.value),
                            FieldKind::Check(v) => if *v { "[x]" } else { "[ ]" }.into(),
                            FieldKind::Choice(choices, index) => {
                                format!("{}  (←/→ to change)", choices[*index])
                            }
                        };
                        let marker = if form.selected == n { " > " } else { "   " };
                        ListItem::new(vec![
                            Line::styled(
                                format!("{marker}{}", f.label),
                                Style::default().fg(Color::Cyan),
                            ),
                            Line::raw(format!("   {value}")),
                        ])
                    })
                    .collect();
                items.push(ListItem::new(vec![
                    Line::raw(""),
                    Line::styled("   [ Review and save ]", Style::default().fg(Color::Green)),
                ]));
                let sections =
                    Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).split(inner);
                let mut state = ListState::default().with_selected(Some(form.selected));
                frame.render_stateful_widget(
                    List::new(items).highlight_style(Style::default().bg(Color::Rgb(31, 55, 84))),
                    sections[0],
                    &mut state,
                );
                for n in state.offset()
                    ..(form.fields.len() + 1)
                        .min(state.offset() + usize::from(sections[0].height) / 2)
                {
                    self.hits.push((
                        Rect::new(
                            sections[0].x,
                            sections[0].y + ((n - state.offset()) * 2) as u16,
                            sections[0].width,
                            2,
                        ),
                        Hit::Field(n),
                    ));
                }
                frame.render_widget(Paragraph::new("Tab/↑↓: next field · Space: checkbox · Enter: next/review · Ctrl+U: clear\nPaste supported. No capability is written to a history file."),sections[1]);
            }
        }
    }
}
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
    }
}
pub fn run(config: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("Interactive management requires a terminal. Use ssh -t desktop.home wispnet, or --help for scripting commands.".into());
    }
    let api = Api::new(config)?;
    let mut app = App::new(api);
    let stop = Arc::new(AtomicBool::new(false));
    let sigterm = signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    let sigint = signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    let mut terminal = ratatui::init();
    let _restore = Restore;
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    while !stop.load(Ordering::Relaxed) {
        if let Some(request) = app.shell.take() {
            execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste)?;
            ratatui::restore();
            let result = crate::shell::run_selected(&app.api, request);
            terminal = ratatui::init();
            execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
            app.message = match result {
                Ok(code) => format!("Shell detached or exited ({code})"),
                Err(error) => error,
            };
        }
        app.receive();
        // A QR needs explicit black-on-white contrast even with NO_COLOR set.
        crossterm::style::force_color_output(
            (app.page == Page::Qr && app.qr.is_some()) || std::env::var_os("NO_COLOR").is_none(),
        );
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if app.key(key) => {
                    break;
                }
                Event::Mouse(mouse) => app.mouse(mouse),
                Event::Paste(text) => app.paste(&text),
                _ => {}
            }
        }
    }
    signal_hook::low_level::unregister(sigterm);
    signal_hook::low_level::unregister(sigint);
    Ok(())
}

#[cfg(test)]
mod tests;
