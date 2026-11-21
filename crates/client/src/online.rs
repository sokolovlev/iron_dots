//! The online menu: server address or LAN host, lobby list, lobby with
//! slots/teams/bots, ready and start. Hands a connected game to the app.

use egui::{Color32, RichText};
use net::lan::{Beacon, LanGame, Scanner};
use net::proto::{ClientMsg, LobbyInfo, LobbySettings, LobbyState, PROTOCOL_VERSION, ServerMsg, SlotKind};
use net::{NetClient, NetEvent};
use relay_server::LocalServer;
use sim_core::Balance;
use sim_core::scenario::ScenarioSpec;
use std::net::{SocketAddr, ToSocketAddrs};

/// A relay this client runs for a LAN game; lives as long as the game.
pub struct Hosting {
    pub relay: LocalServer,
    _beacon: Option<Beacon>,
}

/// Everything needed to begin a networked match.
pub struct Started {
    pub net: NetClient,
    pub addr: SocketAddr,
    pub name: String,
    pub game: u32,
    pub token: u64,
    pub you: u8,
    pub scenario: ScenarioSpec,
    pub bots: Vec<(u8, u8)>,
    pub tick_rate: u32,
    pub hosting: Option<Hosting>,
    /// Tick packets that arrived together with `GameStart`.
    pub early: Vec<net::TickPacket>,
}

pub struct OnlineMenu {
    pub open: bool,
    pub name: String,
    pub address: String,
    net: Option<(NetClient, SocketAddr)>,
    connected: bool,
    client_id: Option<u32>,
    lobbies: Vec<LobbyInfo>,
    lobby: Option<LobbyState>,
    error: Option<String>,
    hosting: Option<Hosting>,
    scanner: Option<Result<Scanner, String>>,
    lan: Vec<(LanGame, f32)>,
    new_lobby: LobbySettings,
    lobby_name: String,
    time: f32,
    last_list: f32,
}

impl Default for OnlineMenu {
    fn default() -> Self {
        OnlineMenu {
            open: false,
            name: std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "Player".into()),
            address: format!("127.0.0.1:{}", net::DEFAULT_PORT),
            net: None,
            connected: false,
            client_id: None,
            lobbies: Vec::new(),
            lobby: None,
            error: None,
            hosting: None,
            scanner: None,
            lan: Vec::new(),
            new_lobby: LobbySettings { width: 320, height: 192, seed: 1, slots: 2 },
            lobby_name: "Iron Dots".into(),
            time: 0.0,
            last_list: -10.0,
        }
    }
}

fn resolve(address: &str) -> Result<SocketAddr, String> {
    let with_port =
        if address.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !address.ends_with(']') {
            address.to_string()
        } else {
            format!("{address}:{}", net::DEFAULT_PORT)
        };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("{address}: {e}"))?
        .next()
        .ok_or_else(|| format!("{address}: no address"))
}

fn hello(name: &str) -> ClientMsg {
    ClientMsg::Hello {
        name: name.to_string(),
        protocol: PROTOCOL_VERSION,
        sim_version: sim_core::sim_version(&Balance::default_balance()),
    }
}

impl OnlineMenu {
    fn send(&self, m: ClientMsg) {
        if let Some((n, _)) = &self.net {
            n.send(m);
        }
    }

    fn connect(&mut self, addr: SocketAddr) {
        let n = NetClient::connect(addr);
        n.send(hello(&self.name));
        self.net = Some((n, addr));
        self.connected = false;
        self.error = None;
        self.lobbies.clear();
        self.lobby = None;
        self.last_list = -10.0;
    }

    fn disconnect(&mut self) {
        self.net = None;
        self.connected = false;
        self.client_id = None;
        self.lobby = None;
        self.hosting = None;
    }

    /// Runs a relay in this process and announces it on the LAN.
    fn host_lan(&mut self) {
        self.disconnect();
        let config = relay_server::Config::default();
        let relay = relay_server::spawn_local(SocketAddr::from(([0, 0, 0, 0], net::DEFAULT_PORT)), config.clone())
            .or_else(|_| relay_server::spawn_local(SocketAddr::from(([0, 0, 0, 0], 0)), config));
        match relay {
            Ok(relay) => {
                let port = relay.addr.port();
                let beacon = Beacon::start(&format!("{}'s game", self.name), port).ok();
                self.hosting = Some(Hosting { relay, _beacon: beacon });
                self.connect(SocketAddr::from(([127, 0, 0, 1], port)));
                self.lobby_name = format!("{}'s game", self.name);
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Polls the network; returns a match that just started.
    pub fn update(&mut self, dt: f32) -> Option<Started> {
        if !self.open {
            return None;
        }
        self.time += dt;
        let scanner = self.scanner.get_or_insert_with(Scanner::new);
        if let Ok(s) = scanner {
            for g in s.poll() {
                match self.lan.iter_mut().find(|(l, _)| *l == g) {
                    Some(e) => e.1 = self.time,
                    None => self.lan.push((g, self.time)),
                }
            }
        }
        let now = self.time;
        self.lan.retain(|(_, seen)| now - *seen < 4.0);
        let Some((n, addr)) = &self.net else { return None };
        let addr = *addr;
        let mut started = None;
        let mut early = Vec::new();
        for ev in n.poll() {
            if started.is_some() {
                if let NetEvent::Msg(ServerMsg::Tick(p)) = ev {
                    early.push(p);
                }
                continue;
            }
            match ev {
                NetEvent::Connected => self.connected = true,
                NetEvent::Disconnected(e) => {
                    self.error = Some(e);
                    self.net = None;
                    self.connected = false;
                    self.lobby = None;
                    return None;
                }
                NetEvent::Msg(m) => match m {
                    ServerMsg::Welcome { client } => {
                        self.client_id = Some(client);
                        if self.hosting.is_some() && self.lobby.is_none() {
                            self.send(ClientMsg::CreateLobby {
                                name: self.lobby_name.clone(),
                                settings: self.new_lobby.clone(),
                            });
                        }
                    }
                    ServerMsg::Error(e) => self.error = Some(e),
                    ServerMsg::Lobbies(l) => self.lobbies = l,
                    ServerMsg::Lobby(l) => self.lobby = Some(l),
                    ServerMsg::GameStart { game, token, you, scenario, bots, tick_rate } => {
                        started = Some((game, token, you, scenario, bots, tick_rate));
                    }
                    _ => {}
                },
            }
        }
        if let Some((game, token, you, scenario, bots, tick_rate)) = started {
            let (net, _) = self.net.take().expect("connected");
            self.lobby = None;
            self.connected = false;
            self.open = false;
            return Some(Started {
                net,
                addr,
                name: self.name.clone(),
                game,
                token,
                you,
                scenario: *scenario,
                bots,
                tick_rate,
                hosting: self.hosting.take(),
                early,
            });
        }
        if self.client_id.is_some() && self.lobby.is_none() && self.time - self.last_list > 2.0 {
            self.last_list = self.time;
            self.send(ClientMsg::ListLobbies);
        }
        None
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("ONLINE").size(32.0).strong());
        ui.add_space(12.0);
        if let Some(l) = self.lobby.clone() {
            self.lobby_ui(ui, &l);
        } else if self.client_id.is_some() {
            self.browser_ui(ui);
        } else {
            self.connect_ui(ui);
        }
        if let Some(e) = &self.error {
            ui.colored_label(Color32::LIGHT_RED, e);
        }
        ui.add_space(12.0);
        if ui.button("Back").clicked() {
            if self.lobby.is_some() {
                self.send(ClientMsg::LeaveLobby);
                self.lobby = None;
                if self.hosting.is_some() {
                    self.disconnect();
                }
            } else {
                self.disconnect();
                self.open = false;
            }
        }
    }

    fn connect_ui(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("connect").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Your name");
            ui.text_edit_singleline(&mut self.name);
            ui.end_row();
            ui.label("Server");
            ui.text_edit_singleline(&mut self.address);
            ui.end_row();
        });
        ui.add_space(8.0);
        if self.net.is_some() {
            ui.label("Connecting…");
        } else {
            ui.horizontal(|ui| {
                if ui.button("Connect").clicked() {
                    match resolve(self.address.trim()) {
                        Ok(a) => self.connect(a),
                        Err(e) => self.error = Some(e),
                    }
                }
                if ui.button("Host LAN game").clicked() {
                    self.host_lan();
                }
                ui.add(egui::Slider::new(&mut self.new_lobby.slots, 2..=8).text("slots"));
            });
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Games on your network").strong());
        match &self.scanner {
            Some(Err(e)) => {
                ui.weak(e);
            }
            _ if self.lan.is_empty() => {
                ui.weak("none found yet");
            }
            _ => {
                let mut join = None;
                for (g, _) in &self.lan {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}  ({})", g.name, g.addr));
                        if ui.button("Join").clicked() {
                            join = Some(g.addr);
                        }
                    });
                }
                if let Some(a) = join {
                    self.connect(a);
                }
            }
        }
    }

    fn browser_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Lobbies").strong());
        if self.lobbies.is_empty() {
            ui.weak("no open lobbies");
        }
        let mut join = None;
        for l in &self.lobbies {
            ui.horizontal(|ui| {
                ui.label(format!("{}  ·  {} players, {} open  ·  {}×{}", l.name, l.humans, l.open, l.width, l.height));
                if l.open > 0 && ui.button("Join").clicked() {
                    join = Some(l.id);
                }
            });
        }
        if let Some(id) = join {
            self.send(ClientMsg::JoinLobby { lobby: id });
        }
        ui.add_space(12.0);
        ui.label(RichText::new("New lobby").strong());
        egui::Grid::new("new").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.lobby_name);
            ui.end_row();
            ui.label("Map size");
            let mut map = [self.new_lobby.width, self.new_lobby.height];
            // Up to 2048×1152: the relay's limit.
            crate::ui::map_picker(ui, "omap", &mut map, 5);
            [self.new_lobby.width, self.new_lobby.height] = map;
            ui.end_row();
            ui.label("Slots");
            ui.add(egui::Slider::new(&mut self.new_lobby.slots, 2..=8));
            ui.end_row();
            ui.label("Map seed");
            ui.add(egui::DragValue::new(&mut self.new_lobby.seed));
            ui.end_row();
        });
        if ui.button("Create lobby").clicked() {
            self.send(ClientMsg::CreateLobby { name: self.lobby_name.clone(), settings: self.new_lobby.clone() });
        }
    }

    fn lobby_ui(&mut self, ui: &mut egui::Ui, l: &LobbyState) {
        let me = self.client_id.unwrap_or(0);
        let host = l.host == me;
        ui.label(RichText::new(&l.name).size(20.0).strong());
        let lan = self.hosting.as_ref().map(|h| format!("  ·  LAN, port {}", h.relay.addr.port())).unwrap_or_default();
        ui.weak(format!("map {}×{}, seed {}{lan}", l.settings.width, l.settings.height, l.settings.seed));
        ui.add_space(8.0);
        let mut msgs = Vec::new();
        egui::Grid::new("slots").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
            for (k, s) in l.slots.iter().enumerate() {
                let k = k as u8;
                ui.label(format!("{}.", k + 1));
                match &s.kind {
                    SlotKind::Human { client, name, ready } => {
                        let tag = if *client == l.host { " (host)" } else { "" };
                        let col = if *ready || *client == l.host { Color32::LIGHT_GREEN } else { Color32::GRAY };
                        ui.colored_label(col, format!("{name}{tag}"));
                    }
                    other if host => {
                        let label = |k: &SlotKind| match k {
                            SlotKind::Open => "Open",
                            SlotKind::Closed => "Closed",
                            SlotKind::Bot { difficulty: 0 } => "Bot (easy)",
                            SlotKind::Bot { difficulty: 1 } => "Bot (normal)",
                            SlotKind::Bot { .. } => "Bot (hard)",
                            SlotKind::Human { .. } => "",
                        };
                        egui::ComboBox::from_id_salt(("slot", k)).selected_text(label(other)).show_ui(ui, |ui| {
                            for kind in [
                                SlotKind::Open,
                                SlotKind::Closed,
                                SlotKind::Bot { difficulty: 0 },
                                SlotKind::Bot { difficulty: 1 },
                                SlotKind::Bot { difficulty: 2 },
                            ] {
                                if ui.selectable_label(*other == kind, label(&kind)).clicked() && *other != kind {
                                    msgs.push(ClientMsg::SetSlot { slot: k, kind });
                                }
                            }
                        });
                    }
                    SlotKind::Open => {
                        ui.weak("open");
                    }
                    SlotKind::Closed => {
                        ui.weak("closed");
                    }
                    SlotKind::Bot { difficulty } => {
                        ui.label(["Bot (easy)", "Bot (normal)", "Bot (hard)"][(*difficulty).min(2) as usize]);
                    }
                }
                if host {
                    let mut team = s.team;
                    if ui.add(egui::DragValue::new(&mut team).range(1..=8).prefix("team ")).changed() {
                        msgs.push(ClientMsg::SetTeam { slot: k, team });
                    }
                } else {
                    ui.label(format!("team {}", s.team));
                }
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        if host {
            if ui.button(RichText::new("  Start  ").size(18.0)).clicked() {
                msgs.push(ClientMsg::Start);
            }
        } else {
            let ready =
                l.slots.iter().any(|s| matches!(s.kind, SlotKind::Human { client, ready: true, .. } if client == me));
            let mut r = ready;
            if ui.checkbox(&mut r, "Ready").changed() {
                msgs.push(ClientMsg::SetReady(r));
            }
        }
        for m in msgs {
            self.send(m);
        }
    }
}
