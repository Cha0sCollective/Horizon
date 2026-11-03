//! Hosting the open world on the local network ("Open to LAN"): starts the
//! server threads and the play-state hub, feeds it the world's entities each
//! tick and turns what the guests do into gameplay. A world that is not open
//! costs one `Option` check per tick.
use super::{Game, shield::OFFHAND_SLOT};
#[cfg(feature = "paired-packages")]
pub(in crate::game) mod connected_recovery;
use horizon_client::{
    Result,
    items::ItemStack,
    lan::{
        self, Lan, MAX_PLAYERS, NetConfig,
        chunks::ChunkEncoder,
        data::SyncData,
        entity::{EntityState, States},
        hub::{BodyFlags, HostEvent, HostState, Hub, HubConfig, TickCtx},
        playerdata,
    },
    mobs::{Mob, Species, Who, leash::Holder},
    text::Text,
};
use horizon_core::geometry::Vec3;
use horizon_protocol::{
    crypto::offline_uuid,
    entity_data::{Value, serializer},
    packets::play::{self, Clientbound as C},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// The registries and tags are the same for every world: load them once.
static SYNC: Mutex<Option<std::result::Result<Arc<SyncData>, String>>> = Mutex::new(None);

#[cfg(feature = "paired-packages")]
pub(super) struct NativeAdmission {
    pub(super) uuid: u128,
    tag: Option<Option<horizon_world::nbt::Tag>>,
    stats: Option<Option<serde_json::Value>>,
    advancements: Option<Option<serde_json::Value>>,
    /// Includes the failed path, original JSON input where available, and the
    /// original diagnostic. Successful stages are never reread on Retry.
    failures: Vec<(std::path::PathBuf, Option<Vec<u8>>, String)>,
    installed: bool,
    #[allow(clippy::vec_box)]
    // Keep each original failed candidate allocation, not a replacement body.
    partial_bodies: Vec<Box<horizon_client::motion::PlayerMotion>>,
}
#[cfg(feature = "paired-packages")]
impl NativeAdmission {
    pub(super) fn new(uuid: u128) -> Self {
        Self {
            uuid,
            tag: None,
            stats: None,
            advancements: None,
            failures: Vec::new(),
            installed: false,
            partial_bodies: Vec::new(),
        }
    }
    pub(super) fn installed(&self) -> bool {
        self.installed
    }
    fn read_json(&mut self, path: &std::path::Path) -> Result<Option<serde_json::Value>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                let detail = format!("{}: {error}", path.display());
                self.failures.push((path.to_owned(), None, detail.clone()));
                return Err(detail.into());
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(json) => Ok(Some(json)),
            Err(error) => {
                let detail = format!("{}: {error}", path.display());
                self.failures
                    .push((path.to_owned(), Some(bytes), detail.clone()));
                Err(detail.into())
            }
        }
    }
    fn read_inputs(&mut self, root: &std::path::Path) -> Result<()> {
        let path = playerdata::path(root, self.uuid);
        if self.tag.is_none() {
            match horizon_client::save::read_dat_checked(&path) {
                Ok(tag) => self.tag = Some(tag),
                Err(error) => {
                    let detail = format!("{}: {error}", path.display());
                    self.failures.push((path, None, detail.clone()));
                    return Err(detail.into());
                }
            }
        }
        let uuid = playerdata::uuid_ints(self.uuid);
        if self.stats.is_none() {
            self.stats = Some(self.read_json(&super::stats::player_json(root, "stats", uuid))?);
        }
        if self.advancements.is_none() {
            self.advancements =
                Some(self.read_json(&super::stats::player_json(root, "advancements", uuid))?);
        }
        Ok(())
    }
}

#[cfg(feature = "paired-packages")]
fn retained_guest_untouched(host: &LanHost, g: &lan::guest::Guest) -> bool {
    !g.placed
        && !g.loaded
        && g.motion.is_none()
        && !host.play.contains_key(&g.conn)
        && g.inventory.len() == 36
        && g.inventory.iter().all(Option::is_none)
        && g.ender_items.len() == 27
        && g.ender_items.iter().all(Option::is_none)
        && g.armor.iter().all(Option::is_none)
        && g.offhand.is_none()
        && g.loaded_body.is_none()
        && g.saved_absorption.is_none()
        && g.saved.is_none()
        && g.logs_kept.is_none()
        && g.carried.is_none()
        && g.craft_grid.iter().all(Option::is_none)
        && g.craft_result.is_none()
        && g.menu.is_none()
        && g.drag.is_none()
        && g.vehicle.is_none()
        && !g.dead
        && g.death_time == 0
        && !g.sleeping
        && g.bed.is_none()
        && g.awaiting_teleport.is_none()
        && !g.advancements_loaded
        && g.advancements.criteria.is_empty()
        && g.effects.saved().is_empty()
        && g.health == 20.0
        && g.food == 20
        && g.saturation == 5.0
        && g.xp_level == 0
        && g.xp_total == 0
        && g.xp_progress == 0.0
        && g.xp_seed == 0
        && g.spawn.is_none()
        && g.respawn_elsewhere.is_none()
        && g.last_death.is_none()
        && g.left_reason.is_empty()
        && !g.seen_credits
}

#[cfg(feature = "paired-packages")]
impl Game<'_> {
    #[cfg(test)]
    pub(super) fn test_refuse_original_retained_host(&mut self) {
        let identity = self
            .retained_server
            .as_ref()
            .unwrap()
            .hub_identity()
            .unwrap()
            .clone();
        let host = self.lan.take().unwrap();
        let bodies = host
            .hub
            .guests
            .iter()
            .map(|g| (g.conn, g.motion.as_deref().map(std::ptr::from_ref)))
            .collect::<Vec<_>>();
        let play = host.play.len();
        let host = match host.try_retained_closed() {
            Err(host) => host,
            Ok(_) => {
                eprintln!("fatal: open original server host consumed without closure");
                std::process::abort();
            }
        };
        assert!(identity.matches(&host.hub));
        assert_eq!(play, host.play.len());
        assert_eq!(
            bodies,
            host.hub
                .guests
                .iter()
                .map(|g| (g.conn, g.motion.as_deref().map(std::ptr::from_ref)))
                .collect::<Vec<_>>()
        );
        self.lan = Some(host);
        self.retained_server
            .as_mut()
            .unwrap()
            .consume_refusal_checked = true;
    }

    pub(super) fn install_original_retained_host(
        &mut self,
        sync: Arc<SyncData>,
        encoder: ChunkEncoder,
        config: HubConfig,
        offline: lan::RetainedOfflineConfig,
    ) -> std::result::Result<(), String> {
        if self.lan.is_some()
            || !self
                .retained_server
                .as_ref()
                .is_some_and(|s| s.can_bind_hub())
        {
            // This fixed call is made once by the cold factory, before offers.
            return Err("original retained host is already bound".into());
        }
        let mut hub = match Hub::new_retained(sync, encoder, config, offline) {
            Ok(hub) => hub,
            Err(refusal) => {
                let detail = refusal.reason.clone();
                self.retained_server
                    .as_mut()
                    .unwrap()
                    .hold_hub_refusal(refusal);
                return Err(detail);
            }
        };
        hub.flat_world = self.world_flat;
        self.retained_server
            .as_mut()
            .unwrap()
            .bind_hub(hub.retained_identity().unwrap());
        self.world.block_log = Some(Vec::new());
        self.world.light_log = Some(Default::default());
        self.lan = Some(LanHost {
            hub,
            settings: LanSettings {
                port: 0,
                guest_commands: false,
                force_game_mode: false,
            },
            host_entity: 0,
            host_swings: self.swings_started,
            host_crack: None,
            host_uuid: 0,
            host_name: "Retained server".into(),
            models: HashMap::new(),
            host_mode: self.mode_id(),
            play: HashMap::new(),
            broken: Vec::new(),
            mob_state: HashMap::new(),
            mob_players: HashMap::new(),
            owner: None,
            waypoints: Default::default(),
            waypoint_dead: Default::default(),
            border_sent: None,
            commands: Vec::new(),
            quiet: Default::default(),
            states: Default::default(),
        });
        self.audio.capture = Some(Vec::new());
        Ok(())
    }

    fn install_retained_guest(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        spawn: [f64; 3],
    ) -> Result<()> {
        let server = self
            .retained_server
            .as_ref()
            .ok_or("not an original retained server")?;
        let g = host
            .hub
            .guest(conn)
            .ok_or("original admitted Guest missing")?;
        if !server
            .hub_identity()
            .is_some_and(|id| id.matches_guest(&host.hub, conn, g.uuid))
        {
            return Err("original admitted connection identity differs".into());
        }
        if server.native_installed(conn, g.uuid) {
            return Ok(());
        }
        if !retained_guest_untouched(host, g) {
            return Err("native installation requires the original untouched Guest".into());
        }
        let uuid = g.uuid;
        let mut admission = self
            .retained_server
            .as_mut()
            .unwrap()
            .admissions
            .remove(&conn)
            .unwrap_or_else(|| NativeAdmission::new(uuid));
        let outcome = self.prepare_retained_guest(host, conn, spawn, &mut admission);
        self.retained_server
            .as_mut()
            .unwrap()
            .admissions
            .insert(conn, admission);
        outcome
    }

    fn prepare_retained_guest(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        spawn: [f64; 3],
        admission: &mut NativeAdmission,
    ) -> Result<()> {
        if host.hub.guest(conn).map(|g| g.uuid) != Some(admission.uuid) {
            return Err("native admission profile changed".into());
        }
        admission.read_inputs(&self.world_root)?;
        let tag = admission.tag.as_ref().unwrap();
        let mut level = horizon_client::save::Level {
            health: 20.0,
            food: 20,
            saturation: 5.0,
            player_uuid: Some(playerdata::uuid_ints(admission.uuid)),
            ..Default::default()
        };
        if let Some(tag) = tag {
            validate_session_player_tag(tag)?;
            level.read_player(tag);
            level.player_extra = Some(tag.clone());
        } else {
            level.dimension = super::Dim::Overworld.id().into();
            level.position = Some(self.server_player_spawn(spawn));
            level.game_mode = host.hub.config.default_game_mode;
            level.creative = level.game_mode == 1;
        }
        if level.player_uuid != Some(playerdata::uuid_ints(admission.uuid))
            || level.dimension != "minecraft:overworld"
            || !level.active_effects.is_empty()
            || level.selected_slot < 0
            || level.selected_slot >= 9
            || level.inventory.iter().any(|(slot, _, _, _)| *slot > 35)
        {
            return Err("unsupported native retained player profile".into());
        }
        let p = level.position.ok_or("native player has no position")?;
        if !p.iter().all(|n| n.is_finite()) {
            return Err("native player position is nonfinite".into());
        }
        // All fallible body work precedes consumption of the original Guest's
        // saved body, absorption and logs. The legacy .ok() branch never runs.
        let mut body = Box::new(horizon_client::motion::PlayerMotion::new(
            self.motion,
            Vec3::new(p[0], p[1], p[2]),
        )?);
        let mut pending = true;
        if let Err(error) = body
            .load_saved_state_once(&mut pending, &level)
            .and_then(|()| body.internal_set_absorption(level.absorption))
        {
            admission.partial_bodies.push(body);
            let detail = error.to_string();
            admission.failures.push((
                self.world_root.clone(),
                None,
                format!("native first body: {detail}"),
            ));
            return Err(error);
        }
        body.velocity = Vec3::new(level.motion[0], level.motion[1], level.motion[2]);
        body.health.food.exhaustion = level.food_exhaustion;
        body.health.food.timer = level.food_tick_timer;
        body.yaw = level.rotation[0];
        body.head_yaw = body.yaw;
        body.pitch = level.rotation[1];
        body.ground.on_ground = level.on_ground;
        body.server_copy = true;
        body.may_fly = matches!(level.mode(), 1 | 3);
        body.invulnerable = body.may_fly;
        body.spectator = level.mode() == 3;
        body.flying = level.flying;
        let tree = self.adv.tree.clone();
        if tree.list.is_empty() || self.adv.pending_tree.is_some() {
            admission.partial_bodies.push(body);
            return Err("complete native advancement resources missing".into());
        }
        let mut progress = admission
            .advancements
            .as_ref()
            .unwrap()
            .as_ref()
            .map(horizon_client::advancements::Progress::from_json)
            .unwrap_or_default();
        progress
            .criteria
            .retain(|id, _| tree.by_id.contains_key(id));
        let mut stats =
            super::stats::Stats::from_saved_json(admission.stats.as_ref().unwrap().as_ref());
        stats.mark_all_dirty();
        let g = host.hub.guest_mut(conn).unwrap();
        playerdata::apply(g, &level);
        g.loaded_body
            .take()
            .expect("native apply installed body")
            .apply(&mut body);
        g.saved_absorption
            .take()
            .expect("native apply installed absorption");
        if let Some((hurts, attacks)) = g.logs_kept.take() {
            body.hurt_log = hurts;
            body.attack_log = attacks;
        }
        g.motion = Some(body);
        g.advancements = progress;
        g.advancements_loaded = true;
        host.play.entry(conn).or_default().stats = stats;
        admission.installed = true;
        self.retained_server
            .as_mut()
            .unwrap()
            .record_installation(conn, admission.uuid);
        Ok(())
    }

    fn place_retained_guest(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        state: Option<&HostState>,
        entities: &[EntityState],
        spawn: [f64; 3],
    ) {
        let clock_rate = self.network_clock_rate();
        let mut ctx = TickCtx {
            world: &mut self.world,
            host: state,
            entities,
            next_entity: &mut self.next_entity,
            spawn,
            difficulty: i32::from(self.ui.difficulty),
            difficulty_locked: self.level.difficulty_locked,
            clock_rate,
            tick_rate: self.clock.rate(),
            tick_frozen: self.clock.frozen(),
            tick_steps: self.clock.step_state(),
            collision: Some(self.collision),
            border: &self.border,
        };
        host.hub.place_new_player(conn, &mut ctx);
    }

    /// Only explicit close Retry invokes this. It finishes already-started
    /// native reads/body installation, never placement, input or simulation.
    pub(super) fn retry_retained_admissions(&mut self) -> Result<()> {
        let conns = self
            .retained_server
            .as_ref()
            .ok_or("server missing")?
            .admissions
            .iter()
            .filter_map(|(conn, admission)| (!admission.installed()).then_some(*conn))
            .collect::<Vec<_>>();
        let Some(mut host) = self.lan.take() else {
            return if conns.is_empty() {
                Ok(())
            } else {
                Err("original admission host missing".into())
            };
        };
        let spawn = self.spawn.map_or([0.5, 80.0, 0.5], |s| [s.x, s.y, s.z]);
        let mut result = Ok(());
        for conn in conns {
            if let Err(error) = self.install_retained_guest(&mut host, conn, spawn) {
                result = Err(error);
                break;
            }
        }
        self.lan = Some(host);
        result
    }

    /// Hub movement owns these fields before the next as_guest handoff. A
    /// mid-turn stop completes only that fixed native movement handoff, without
    /// physics, wire replay, acknowledgments, or replacing the installed body.
    pub(super) fn fence_retained_native_motion(&mut self) -> Result<()> {
        let server = self
            .retained_server
            .as_ref()
            .ok_or("original server missing")?;
        let Some(host) = &mut self.lan else {
            return Ok(());
        };
        for g in &mut host.hub.guests {
            if !server.native_installed(g.conn, g.uuid) {
                continue;
            }
            let body = g
                .motion
                .as_mut()
                .ok_or("original installed body missing at freeze")?;
            if !g.position.iter().all(|n| n.is_finite())
                || !g.yaw.is_finite()
                || !g.pitch.is_finite()
            {
                return Err("nonfinite original movement at freeze".into());
            }
            body.position = Vec3::new(g.position[0], g.position[1], g.position[2]);
            body.yaw = g.yaw;
            body.pitch = g.pitch;
            body.ground.on_ground = g.on_ground;
            body.sprinting = g.sprinting;
            body.flying = g.flying;
        }
        Ok(())
    }
}

/// The registries and tags a server sends, loaded from `root` the first time.
pub(super) fn sync_data(root: &std::path::Path) -> std::result::Result<Arc<SyncData>, String> {
    let mut sync = SYNC.lock().unwrap_or_else(|e| e.into_inner());
    sync.get_or_insert_with(|| SyncData::load(root).map(Arc::new))
        .clone()
}

/// Let go of what `sync_data` loaded: for a preload no world used
/// (game/world_loading.rs).
pub(super) fn forget_sync_data() {
    *SYNC.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The settings of the open world (WorldOptionsScreen's multiplayer section).
#[derive(Clone, Copy, Debug)]
pub struct LanSettings {
    pub port: u16,
    pub guest_commands: bool,
    pub force_game_mode: bool,
}

pub(super) struct LanHost<H = Hub> {
    pub hub: H,
    pub settings: LanSettings,
    pub host_entity: i32,
    /// The host's swings the guests were told of (`Game::swings_started`).
    pub host_swings: u32,
    /// The host's crack the guests were told of: the block and its stage.
    pub host_crack: Option<([i32; 3], i32)>,
    pub host_uuid: u128,
    /// The host's profile name (the account's, or the offline name).
    pub host_name: String,
    /// The guests' models in the host's world.
    pub models: HashMap<u32, Mob>,
    host_mode: i32,
    pub play: HashMap<u32, super::lan_actions::GuestPlay>,
    /// Blocks broken this tick (their sound is a level event for the guests).
    pub broken: Vec<[f64; 3]>,
    /// Hurt time, death and swing time (while swinging) of the mobs guests see.
    mob_state: HashMap<super::Dim, HashMap<u32, (i32, bool, Option<i32>)>>,
    /// The guest each mob sees as its player, when not the host (game/lan_mobs.rs).
    pub mob_players: HashMap<u32, u32>,
    /// An integrated server's owner (game/integrated.rs).
    pub owner: Option<Owner>,
    /// The players' locator bars, one waypoint manager per level by
    /// dimension id (game/lan_waypoints.rs), and the guests who are dead,
    /// whose respawn replaces their waypoint.
    pub waypoints: std::collections::HashMap<String, lan::waypoints::Waypoints>,
    pub waypoint_dead: std::collections::HashSet<u128>,
    /// The world border as the guests were last told of it (lan/border.rs).
    pub border_sent: Option<lan::border::Sent>,
    /// Guests' commands waiting for the hub to be back in `self.lan`, where
    /// a command can reach every player (game/lan_actions.rs).
    pub commands: Vec<super::lan_actions::GuestCommand>,
    /// Mods' quiet commands waiting to run a few a tick (game/lan_quiet.rs).
    pub quiet: std::collections::VecDeque<super::lan_quiet::QuietBatch>,
    /// The entities the guests track, described again every tick
    /// (`lan_entities`) over last tick's states.
    states: lan::entity::States,
}

impl LanHost {
    #[cfg(feature = "paired-packages")]
    #[allow(clippy::result_large_err)] // Refusal returns the entire original host.
    pub(super) fn try_retained_closed(
        self,
    ) -> std::result::Result<LanHost<lan::hub::ClosedHub>, Self> {
        let Self {
            hub,
            settings,
            host_entity,
            host_swings,
            host_crack,
            host_uuid,
            host_name,
            models,
            host_mode,
            play,
            broken,
            mob_state,
            mob_players,
            owner,
            waypoints,
            waypoint_dead,
            border_sent,
            commands,
            quiet,
            states,
        } = self;
        macro_rules! restore {
            ($hub:expr) => {
                LanHost {
                    hub: $hub,
                    settings,
                    host_entity,
                    host_swings,
                    host_crack,
                    host_uuid,
                    host_name,
                    models,
                    host_mode,
                    play,
                    broken,
                    mob_state,
                    mob_players,
                    owner,
                    waypoints,
                    waypoint_dead,
                    border_sent,
                    commands,
                    quiet,
                    states,
                }
            };
        }
        match hub.try_retained_closed() {
            Ok(hub) => Ok(restore!(hub)),
            Err(hub) => Err(restore!(hub)),
        }
    }
    /// `PlayerList.remove`: `player.awardStat(Stats.LEAVE_GAME)` before the
    /// player is saved. Once per guest, however often they are saved.
    pub(super) fn award_leave_game(&mut self, conn: u32) {
        if let Some(play) = self.play.get_mut(&conn)
            && !play.left_game
        {
            play.left_game = true;
            play.stats.custom("leave_game", 1);
        }
    }

    /// `PlayerList.removeAll`: every player who joined leaves with the world.
    pub(super) fn award_leave_game_to_all(&mut self) {
        let conns: Vec<u32> = self
            .hub
            .guests
            .iter()
            .filter(|g| g.placed)
            .map(|g| g.conn)
            .collect();
        for conn in conns {
            self.award_leave_game(conn);
        }
    }
}

/// The player who owns an integrated server's world: a guest whose state is
/// the world's own player (`players/data/<singleplayer_uuid>.dat`), as in
/// single player.
pub(super) struct Owner {
    pub uuid: u128,
    /// The world's player as loaded, until the owner joins.
    pub saved: Option<Box<horizon_client::save::Level>>,
}

fn record_uuid(
    owner_uuid: Option<u128>,
    level_uuid: Option<[i32; 4]>,
    guest_uuid: u128,
) -> Option<[i32; 4]> {
    if owner_uuid == Some(guest_uuid) {
        level_uuid
    } else {
        Some(playerdata::uuid_ints(guest_uuid))
    }
}

// Dormant PLAYER-only observations. These do not authorize checkpoint publication.
#[cfg(feature = "paired-packages")]
use std::sync::atomic::AtomicUsize;

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
type ParticipantCheck<T> = std::result::Result<T, ParticipantPreflightError>;

#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // No complete capture operation consumes this observation yet.
pub(in crate::game) struct ParticipantRoster {
    host_identity: ParticipantHostIdentity,
    owner_uuid: Option<u128>,
    level_player_uuid: Option<[i32; 4]>,
    identities: Vec<ParticipantIdentity>,
}
#[cfg(feature = "paired-packages")]
enum ParticipantHostIdentity {
    Legacy(Arc<lan::Shared>),
    Retained(lan::hub::RetainedHubIdentity),
}
#[cfg(feature = "paired-packages")]
impl ParticipantHostIdentity {
    fn matches(&self, host: &LanHost) -> bool {
        match self {
            Self::Legacy(shared) => host
                .hub
                .lan
                .legacy()
                .is_some_and(|lan| Arc::ptr_eq(shared, &lan.shared)),
            Self::Retained(identity) => identity.matches(&host.hub),
        }
    }
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
struct ParticipantIdentity {
    conn: u32,
    uuid: u128,
    dimension: String,
    sender_identity: Arc<AtomicUsize>,
    storage: ParticipantStorage,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParticipantStorage {
    IntegratedOwner { record_uuid: [i32; 4] },
    External { record_uuid: [i32; 4] },
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum ParticipantPreflightError {
    NotAuthoritative,
    ActingGuest,
    MissingHost,
    MissingOwner,
    OwnerMismatch,
    MissingMapping,
    InvalidIdentity { conn: u32 },
    DuplicateConnection { conn: u32 },
    DuplicateUuid { conn: u32 },
    DuplicateRecord { conn: u32 },
    RosterMismatch,
    UnsupportedPlayer { conn: u32 },
    OrphanPlay { conn: u32 },
    UnsupportedServerState,
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
impl ParticipantRoster {
    fn validate(&self) -> ParticipantCheck<()> {
        use ParticipantPreflightError as E;
        for (index, p) in self.identities.iter().enumerate() {
            if !matches!(
                p.dimension.as_str(),
                "minecraft:overworld" | "minecraft:the_nether" | "minecraft:the_end"
            ) {
                return Err(E::InvalidIdentity { conn: p.conn });
            }
            let record = record_uuid(self.owner_uuid, self.level_player_uuid, p.uuid)
                .ok_or(E::MissingMapping)?;
            let expected = if Some(p.uuid) == self.owner_uuid {
                ParticipantStorage::IntegratedOwner {
                    record_uuid: record,
                }
            } else {
                ParticipantStorage::External {
                    record_uuid: record,
                }
            };
            if p.storage != expected {
                return Err(E::InvalidIdentity { conn: p.conn });
            }
            for other in &self.identities[..index] {
                if p.conn == other.conn {
                    return Err(E::DuplicateConnection { conn: p.conn });
                }
                if p.uuid == other.uuid {
                    return Err(E::DuplicateUuid { conn: p.conn });
                }
                let other_record = match other.storage {
                    ParticipantStorage::IntegratedOwner { record_uuid }
                    | ParticipantStorage::External { record_uuid } => record_uuid,
                };
                if record == other_record {
                    return Err(E::DuplicateRecord { conn: p.conn });
                }
            }
        }
        if self.owner_uuid.is_some()
            && self
                .identities
                .iter()
                .filter(|p| Some(p.uuid) == self.owner_uuid)
                .count()
                != 1
        {
            return Err(E::MissingOwner);
        }
        Ok(())
    }
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // Dormant; this does not qualify global capture readiness.
impl Game<'_> {
    /// Distinct NativeWorld population. Network acknowledgments and inboxes
    /// remain original owned inputs; they are not coherent-session readiness.
    fn observe_retained_native_participants(&self) -> ParticipantCheck<ParticipantRoster> {
        use ParticipantPreflightError as E;
        let host = self.checkpoint_participant_host()?;
        let server = self.retained_server.as_ref().ok_or(E::NotAuthoritative)?;
        let identity = server.hub_identity().ok_or(E::NotAuthoritative)?;
        if !identity.matches(&host.hub)
            || host.owner.is_some()
            || self.player.is_some()
            || self.lan_acting.is_some()
            || !self.server_trips.is_empty()
            || !self.level_teleports.is_empty()
            || !host.commands.is_empty()
            || !host.quiet.is_empty()
            || self.inventory.iter().any(Option::is_some)
            || self.armor.iter().any(Option::is_some)
            || self.offhand.is_some()
            || self.carried.is_some()
            || self.craft_grid.iter().any(Option::is_some)
            || self.container.is_some()
            || self.station.is_some()
            || self.ender_open.is_some()
            || self.craft_table.is_some()
            || self.inv_drag.is_some()
            || self.last_quick_moved.is_some()
        {
            return Err(E::UnsupportedServerState);
        }
        let mut identities = Vec::new();
        for guest in &host.hub.guests {
            if !identity.matches_guest(&host.hub, guest.conn, guest.uuid) {
                return Err(E::InvalidIdentity { conn: guest.conn });
            }
            if server.is_untouched(guest.conn) {
                if !retained_guest_untouched(host, guest) {
                    return Err(E::UnsupportedPlayer { conn: guest.conn });
                }
                continue;
            }
            if !server.native_installed(guest.conn, guest.uuid)
                || guest.motion.is_none()
                || guest.loaded_body.is_some()
                || guest.saved_absorption.is_some()
                || guest.logs_kept.is_some()
                || guest.dimension != "minecraft:overworld"
                || guest.inventory.len() != 36
                || guest.ender_items.len() != 27
                || guest.selected >= 9
                || guest.carried.is_some()
                || guest.craft_grid.iter().any(Option::is_some)
                || guest.craft_result.is_some()
                || guest.menu.is_some()
                || guest.drag.is_some()
                || guest.vehicle.is_some()
                || guest.sleeping
                || guest.bed.is_some()
                || guest.dead
                || guest.death_time != 0
                || !host.play.contains_key(&guest.conn)
            {
                return Err(E::UnsupportedPlayer { conn: guest.conn });
            }
            identities.push(ParticipantIdentity {
                conn: guest.conn,
                uuid: guest.uuid,
                dimension: guest.dimension.clone(),
                sender_identity: guest.sender.pending.clone(),
                storage: ParticipantStorage::External {
                    record_uuid: playerdata::uuid_ints(guest.uuid),
                },
            });
        }
        if host
            .play
            .keys()
            .any(|conn| !identities.iter().any(|p| p.conn == *conn))
        {
            return Err(E::UnsupportedServerState);
        }
        let roster = ParticipantRoster {
            host_identity: ParticipantHostIdentity::Retained(identity.clone()),
            owner_uuid: None,
            level_player_uuid: self.level.player_uuid,
            identities,
        };
        roster.validate()?;
        Ok(roster)
    }

    fn project_native_participant_rows(
        &self,
        expected: &ParticipantRoster,
    ) -> Result<Vec<SessionPlayerRow>> {
        if self.retained_server.is_none() {
            return self.project_session_participant_rows(expected);
        }
        let current = self
            .observe_retained_native_participants()
            .map_err(|e| format!("retained native population: {e:?}"))?;
        let host = self.lan.as_ref().ok_or("retained native host missing")?;
        if !expected.host_identity.matches(host)
            || current.identities.len() != expected.identities.len()
            || current
                .identities
                .iter()
                .zip(&expected.identities)
                .any(|(a, b)| {
                    a.conn != b.conn
                        || a.uuid != b.uuid
                        || a.dimension != b.dimension
                        || a.storage != b.storage
                })
        {
            return Err("retained native participant owner changed".into());
        }
        self.pack_checked_participant_rows(expected)
    }

    fn checkpoint_participant_host(&self) -> ParticipantCheck<&LanHost> {
        use ParticipantPreflightError as E;
        if self.remote.is_some()
            || self.integrated.is_some()
            || self.options.execution_role.is_client()
        {
            return Err(E::NotAuthoritative);
        }
        if self.lan_acting.is_some() {
            return Err(E::ActingGuest);
        }
        self.lan.as_ref().ok_or(E::MissingHost)
    }

    /// Native expected population only; no item snapshot or lasting lease.
    pub(in crate::game) fn observe_checkpoint_participants(
        &self,
    ) -> ParticipantCheck<ParticipantRoster> {
        use ParticipantPreflightError as E;
        let host = self.checkpoint_participant_host()?;
        let owner_uuid = host.owner.as_ref().map(|owner| owner.uuid);
        if let Some(serving) = &self.serving
            && owner_uuid != Some(serving.owner)
        {
            return Err(E::OwnerMismatch);
        }
        if let Some(owner) = &host.owner {
            if owner.saved.is_some() {
                return Err(E::OwnerMismatch);
            }
            if self.level.player_uuid.is_none() {
                return Err(E::MissingMapping);
            }
        }
        let mut identities = Vec::new();
        for guest in &host.hub.guests {
            if !guest.placed
                || guest.inventory.len() != 36
                || guest.ender_items.len() != 27
                || guest.selected >= 9
            {
                return Err(E::UnsupportedPlayer { conn: guest.conn });
            }
            let record = record_uuid(owner_uuid, self.level.player_uuid, guest.uuid)
                .ok_or(E::MissingMapping)?;
            identities.push(ParticipantIdentity {
                conn: guest.conn,
                uuid: guest.uuid,
                dimension: guest.dimension.clone(),
                sender_identity: guest.sender.pending.clone(),
                storage: if owner_uuid == Some(guest.uuid) {
                    ParticipantStorage::IntegratedOwner {
                        record_uuid: record,
                    }
                } else {
                    ParticipantStorage::External {
                        record_uuid: record,
                    }
                },
            });
        }
        let roster = ParticipantRoster {
            host_identity: ParticipantHostIdentity::Legacy(
                host.hub
                    .lan
                    .legacy()
                    .ok_or(E::NotAuthoritative)?
                    .shared
                    .clone(),
            ),
            owner_uuid,
            level_player_uuid: self.level.player_uuid,
            identities,
        };
        roster.validate()?;
        Ok(roster)
    }

    /// Checks current PLAYER owners only. Global owners and byte coverage are separate.
    pub(in crate::game) fn preflight_checkpoint_participants(
        &self,
        expected: &ParticipantRoster,
    ) -> ParticipantCheck<()> {
        use ParticipantPreflightError as E;
        self.checkpoint_participant_host()?;
        expected.validate()?;
        let current = self.observe_checkpoint_participants()?;
        if !expected
            .host_identity
            .matches(self.checkpoint_participant_host()?)
            || current.owner_uuid != expected.owner_uuid
            || current.level_player_uuid != expected.level_player_uuid
            || current.identities.len() != expected.identities.len()
            || current.identities.iter().any(|p| {
                !expected.identities.iter().any(|e| {
                    p.conn == e.conn
                        && p.uuid == e.uuid
                        && p.dimension == e.dimension
                        && p.storage == e.storage
                        && Arc::ptr_eq(&p.sender_identity, &e.sender_identity)
                })
            })
        {
            return Err(E::RosterMismatch);
        }
        if !self.server_trips.is_empty()
            || !self.level_teleports.is_empty()
            || self.pending_save.is_some()
            || self.pending_entry.is_some()
            || self.world_loader.is_some()
            || self.rest_loader.is_some()
            || self.saved_for_exit
            || self.player.is_some()
            || self.inventory.iter().any(Option::is_some)
            || self.armor.iter().any(Option::is_some)
            || self.offhand.is_some()
            || self.carried.is_some()
            || self.craft_grid.iter().any(Option::is_some)
            || self.scratch_slot.is_some()
            || self.ender_items.iter().any(Option::is_some)
            || self.container.is_some()
            || self.station.is_some()
            || self.ender_open.is_some()
            || self.craft_table.is_some()
            || self.inv_drag.is_some()
            || self.last_quick_moved.is_some()
        {
            return Err(E::UnsupportedServerState);
        }
        let host = self.checkpoint_participant_host()?;
        for &conn in host.play.keys() {
            if !host.hub.guests.iter().any(|g| g.conn == conn) {
                return Err(E::OrphanPlay { conn });
            }
        }
        for g in &host.hub.guests {
            if !g.loaded
                || g.motion.is_none()
                || !g.left_reason.is_empty()
                || g.dead
                || g.death_time != 0
                || g.awaiting_teleport.is_some()
                || g.carried.is_some()
                || g.craft_grid.iter().any(Option::is_some)
                || g.craft_result.is_some()
                || g.menu.is_some()
                || g.drag.is_some()
                || g.vehicle.is_some()
                || g.sleeping
                || g.bed.is_some()
                || g.sleep_timer != 0
                || g.sent_bed.is_some()
                || g.loaded_body.is_some()
                || !g.inbox.is_empty()
                || !host
                    .play
                    .get(&g.conn)
                    .is_some_and(|p| p.checkpoint_participant_idle())
            {
                return Err(E::UnsupportedPlayer { conn: g.conn });
            }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod participant_preflight_tests;

#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // Structured observations; no complete capture operation consumes them yet.
pub(in crate::game) struct ParticipantProjections {
    players: Vec<ProjectedParticipant>,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
struct ProjectedParticipant {
    conn: u32,
    uuid: u128,
    dimension: String,
    storage: ParticipantStorage,
    player: horizon_world::nbt::Tag,
    advancements: serde_json::Value,
    stats: serde_json::Value,
}
#[cfg(feature = "paired-packages")]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::game) enum ParticipantProjectionError {
    Preflight(ParticipantPreflightError),
    UnavailableAdvancementTree,
    UnsupportedPlayerData { conn: u32 },
    UnsupportedStack { conn: u32 },
    Reservation { conn: u32 },
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // No writer, publication permit or gameplay caller is introduced here.
impl Game<'_> {
    pub(in crate::game) fn project_checkpoint_participants(
        &self,
        expected: &ParticipantRoster,
    ) -> std::result::Result<ParticipantProjections, ParticipantProjectionError> {
        use super::slot_retirement::{DeliveryError, check_counts};
        use ParticipantProjectionError as E;
        use horizon_client::save::Level;
        use horizon_world::nbt::Tag;

        self.preflight_checkpoint_participants(expected)
            .map_err(E::Preflight)?;
        if self.adv.tree.list.is_empty() || self.adv.pending_tree.is_some() {
            return Err(E::UnavailableAdvancementTree);
        }
        let host = self.checkpoint_participant_host().map_err(E::Preflight)?;
        // Validate every live owner before any player_tag/item encoder is called.
        for guest in &host.hub.guests {
            if guest
                .saved
                .as_ref()
                .is_some_and(|saved| !matches!(saved, Tag::Compound(_)))
            {
                return Err(E::UnsupportedPlayerData { conn: guest.conn });
            }
            for stack in guest
                .inventory
                .iter()
                .chain(guest.armor.iter())
                .chain(std::iter::once(&guest.offhand))
                .chain(guest.ender_items.iter())
                .flatten()
            {
                if &*stack.item == "minecraft:air" || self.res.items().get(&stack.item).is_none() {
                    return Err(E::UnsupportedStack { conn: guest.conn });
                }
                check_counts(stack).map_err(|error| match error {
                    DeliveryError::Reservation => E::Reservation { conn: guest.conn },
                    _ => E::UnsupportedStack { conn: guest.conn },
                })?;
            }
        }
        let mut players = Vec::new();
        for identity in &expected.identities {
            players.try_reserve(1).map_err(|_| E::Reservation {
                conn: identity.conn,
            })?;
            // The same-call preflight and continuous borrow preserve this match.
            let guest = host
                .hub
                .guests
                .iter()
                .find(|g| g.conn == identity.conn)
                .expect("preflight checked every expected participant");
            let play = host
                .play
                .get(&identity.conn)
                .expect("preflight checked play ownership");
            let mut local = match identity.storage {
                ParticipantStorage::IntegratedOwner { .. } => self.level.clone(),
                ParticipantStorage::External { record_uuid } => Level {
                    player_uuid: Some(record_uuid),
                    ..Level::default()
                },
            };
            playerdata::store(guest, &mut local);
            players.push(ProjectedParticipant {
                conn: identity.conn,
                uuid: identity.uuid,
                dimension: identity.dimension.clone(),
                storage: identity.storage,
                player: local.player_tag(),
                advancements: guest
                    .advancements
                    .to_json(&self.adv.tree, horizon_world::DATA_VERSION),
                stats: play.stats.to_json(),
            });
        }
        Ok(ParticipantProjections { players })
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod player_projection_tests;

/// Detached observations of actual native participants. A row grants no admission.
#[cfg(feature = "paired-packages")]
#[derive(Clone, Debug)]
pub(in crate::game) struct SessionPlayerRow {
    pub(in crate::game) conn: u32,
    pub(in crate::game) profile_uuid: u128,
    pub(in crate::game) record_uuid: [i32; 4],
    pub(in crate::game) integrated_owner: bool,
    pub(in crate::game) dimension: String,
    pub(in crate::game) player: horizon_world::nbt::Tag,
    pub(in crate::game) advancements: serde_json::Value,
    pub(in crate::game) stats: serde_json::Value,
}

/// Native NBT reading normalizes signed zero. All other finite values and types
/// must agree; in particular NaN cannot make two observations agree.
#[cfg(feature = "paired-packages")]
pub(in crate::game) fn session_player_tags_equal(
    a: &horizon_world::nbt::Tag,
    b: &horizon_world::nbt::Tag,
) -> bool {
    use horizon_world::nbt::Tag;
    fn equal(a: &Tag, b: &Tag, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match (a, b) {
            (Tag::Float(a), Tag::Float(b)) => {
                let (a, b) = (f32::from_bits(*a), f32::from_bits(*b));
                a.is_finite() && b.is_finite() && a == b
            }
            (Tag::Double(a), Tag::Double(b)) => {
                let (a, b) = (f64::from_bits(*a), f64::from_bits(*b));
                a.is_finite() && b.is_finite() && a == b
            }
            (Tag::List(a), Tag::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b, depth + 1))
            }
            (Tag::Compound(a), Tag::Compound(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(key, a)| b.get(key).is_some_and(|b| equal(a, b, depth + 1)))
            }
            _ => a == b,
        }
    }
    equal(a, b, 0)
}

#[cfg(feature = "paired-packages")]
pub(in crate::game) fn session_player_payload_supported(root: &horizon_world::nbt::Tag) -> bool {
    use horizon_world::nbt::Tag;
    let mut work = vec![(root, 0usize)];
    let mut nodes = 0usize;
    let string = |s: &horizon_world::nbt::JavaString| {
        s.0.len() <= 4096 && char::decode_utf16(s.0.iter().copied()).all(|c| c.is_ok())
    };
    while let Some((tag, depth)) = work.pop() {
        nodes += 1;
        if nodes > 16_384 || depth > 32 {
            return false;
        }
        match tag {
            Tag::End => return false,
            Tag::Float(v) if !f32::from_bits(*v).is_finite() => return false,
            Tag::Double(v) if !f64::from_bits(*v).is_finite() => return false,
            Tag::String(s) if !string(s) => return false,
            Tag::List(values) => {
                if values.len() > 16_384usize.saturating_sub(nodes + work.len())
                    || work.try_reserve(values.len()).is_err()
                {
                    return false;
                }
                work.extend(values.iter().map(|tag| (tag, depth + 1)));
            }
            Tag::Compound(values) => {
                if values.len() > 16_384usize.saturating_sub(nodes + work.len())
                    || work.try_reserve(values.len()).is_err()
                {
                    return false;
                }
                for (key, value) in values {
                    if !string(key)
                        || matches!(
                            key.to_string().as_deref(),
                            Ok("minecraft:attribute_modifiers"
                                | "!minecraft:attribute_modifiers"
                                | "attribute_modifiers")
                        )
                    {
                        return false;
                    }
                    work.push((value, depth + 1));
                }
            }
            Tag::ByteArray(values) if values.len() > 1 << 20 => return false,
            Tag::IntArray(values) if values.len() > (1 << 20) / 4 => return false,
            Tag::LongArray(values) if values.len() > (1 << 20) / 8 => return false,
            _ => {}
        }
    }
    true
}

/// Level::write emits a native UUID record even when a dedicated session has no
/// local player. This exact inert record is metadata, never a participant row.
#[cfg(feature = "paired-packages")]
pub(in crate::game) fn session_empty_native_player(tag: &horizon_world::nbt::Tag) -> bool {
    use horizon_client::save::Level;
    use horizon_world::nbt::Tag;
    if !session_player_payload_supported(tag) {
        return false;
    }
    let Some(Tag::IntArray(uuid)) = tag.get("UUID") else {
        return false;
    };
    let Ok(uuid) = <[i32; 4]>::try_from(uuid.as_slice()) else {
        return false;
    };
    let mut parsed = Level::default();
    parsed.read_player(tag);
    if !(0..=3).contains(&parsed.game_mode) {
        return false;
    }
    let mut expected = Level {
        player_uuid: Some(uuid),
        game_mode: parsed.game_mode,
        creative: parsed.game_mode == 1,
        rotation: parsed.rotation,
        health: 20.0,
        food: 20,
        saturation: 5.0,
        ..Level::default()
    };
    let mut extra = horizon_world::nbt::Compound::new();
    for (key, default) in [("Fire", -20i16), ("Air", 300i16)] {
        if let Some(value) = tag.get(key) {
            if value != &Tag::Short(default) {
                return false;
            }
            extra.insert(key.into(), value.clone());
        }
    }
    expected.player_extra = Some(Tag::Compound(extra));
    session_player_tags_equal(tag, &expected.player_tag())
}

/// This initial body profile supports only fields installed below. Unknown saved
/// state cannot survive merely because player_extra would copy it back unchanged.
#[cfg(feature = "paired-packages")]
pub(in crate::game) fn validate_session_player_tag(tag: &horizon_world::nbt::Tag) -> Result<()> {
    use horizon_client::save::Level;
    use horizon_world::nbt::Tag;
    if !session_player_payload_supported(tag) {
        return Err("unsupported session player payload or item attributes".into());
    }
    let Tag::Compound(fields) = tag else {
        return Err("session player is not a compound".into());
    };
    for key in fields.keys() {
        if !matches!(
            key.to_string().as_deref(),
            Ok("DataVersion"
                | "UUID"
                | "Pos"
                | "Rotation"
                | "Motion"
                | "OnGround"
                | "fall_distance"
                | "Health"
                | "AbsorptionAmount"
                | "foodLevel"
                | "foodSaturationLevel"
                | "foodExhaustionLevel"
                | "foodTickTimer"
                | "XpLevel"
                | "XpP"
                | "XpTotal"
                | "SelectedItemSlot"
                | "XpSeed"
                | "playerGameType"
                | "Inventory"
                | "EnderItems"
                | "equipment"
                | "recipeBook"
                | "abilities"
                | "Dimension"
                | "seenCredits"
                | "LastDeathLocation"
                | "respawn"
                | "Fire"
                | "Air"
                | "Invulnerable"
                | "PortalCooldown"
                | "Score"
                | "SleepTimer"
                | "HurtTime"
                | "DeathTime"
                | "FallFlying")
        ) {
            return Err("unsupported saved session player field".into());
        }
    }
    // The native writer always supplies these fields, but this body profile has
    // no restoration path for their nondefault state. Require their exact types.
    for (key, expected) in [
        ("Invulnerable", Tag::Byte(0)),
        ("PortalCooldown", Tag::Int(0)),
        ("Score", Tag::Int(0)),
        ("SleepTimer", Tag::Short(0)),
        ("HurtTime", Tag::Short(0)),
        ("DeathTime", Tag::Short(0)),
        ("FallFlying", Tag::Byte(0)),
    ] {
        if tag.get(key) != Some(&expected) {
            return Err("unsupported nondefault or malformed native session player field".into());
        }
    }
    let Some(Tag::IntArray(uuid)) = tag.get("UUID") else {
        return Err("session player UUID is not an int array".into());
    };
    let uuid: [i32; 4] = uuid
        .as_slice()
        .try_into()
        .map_err(|_| "session player UUID has wrong length")?;
    if !matches!(tag.get("Fire"), Some(Tag::Short(_)))
        || !matches!(tag.get("Air"), Some(Tag::Short(_)))
        || !matches!(tag.get("DataVersion"), Some(Tag::Int(v)) if *v == horizon_world::DATA_VERSION)
    {
        return Err("unsupported session player body encoding".into());
    }
    let Some(Tag::Compound(abilities)) = tag.get("abilities") else {
        return Err("session player abilities are missing".into());
    };
    if abilities.len() != 7
        || abilities.keys().any(|key| {
            !matches!(
                key.to_string().as_deref(),
                Ok("invulnerable"
                    | "mayfly"
                    | "instabuild"
                    | "mayBuild"
                    | "flying"
                    | "flySpeed"
                    | "walkSpeed")
            )
        })
        || !matches!(tag.get("abilities").and_then(|a| a.get("flySpeed")),
        Some(Tag::Float(v)) if f32::from_bits(*v) == 0.05)
        || !matches!(tag.get("abilities").and_then(|a| a.get("walkSpeed")),
            Some(Tag::Float(v)) if f32::from_bits(*v) == 0.1)
    {
        return Err("unsupported session player abilities".into());
    }
    let mut level = Level {
        player_uuid: Some(uuid),
        ..Level::default()
    };
    level.read_player(tag);
    level.player_extra = Some(tag.clone());
    if level.position.is_none()
        || !matches!(
            level.dimension.as_str(),
            "minecraft:overworld" | "minecraft:the_nether" | "minecraft:the_end"
        )
        || !(0..=3).contains(&level.game_mode)
        || !(0..9).contains(&level.selected_slot)
        || !(level.health > 0.0 && level.health <= 20.0)
        || level.absorption < 0.0
        || !(0..=20).contains(&level.food)
        || level.saturation < 0.0
        || level.food_exhaustion < 0.0
        || level.food_tick_timer < 0
        || level.fall_distance < 0.0
        || level.xp_level < 0
        || level.xp_total < 0
        || !(0.0..=1.0).contains(&level.xp_progress)
        || !session_player_tags_equal(tag, &level.player_tag())
    {
        return Err("session player data is not a supported native saved projection".into());
    }
    Ok(())
}

#[cfg(feature = "paired-packages")]
fn session_player_level(
    guest: &horizon_client::lan::guest::Guest,
    record_uuid: [i32; 4],
) -> Result<horizon_client::save::Level> {
    pack_player_level(guest, record_uuid, PlayerBodyProjection::Coherent)
}

#[cfg(feature = "paired-packages")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlayerBodyProjection {
    Coherent,
    RetainedNative,
}

#[cfg(feature = "paired-packages")]
fn pack_player_level(
    guest: &horizon_client::lan::guest::Guest,
    record_uuid: [i32; 4],
    profile: PlayerBodyProjection,
) -> Result<horizon_client::save::Level> {
    use horizon_client::save::Level;
    let body = guest
        .motion
        .as_deref()
        .ok_or("session player has no live body")?;
    if body.attributes.instances().any(|(_, key, value)| {
        body.attributes
            .catalog()
            .default_base("minecraft:player", key.attribute())
            != Some(value.base_value())
            || (profile == PlayerBodyProjection::RetainedNative
                && !value.permanent_modifiers().is_empty())
            || value.modifiers().iter().any(|modifier| {
                // Original ServerPlayer.updatePlayerAttributes derives these
                // transient reach modifiers from creative mode each tick. The
                // native codec persists that mode; keep the actual instances
                // intact while checking their exact native derivation here.
                let expected = match key.name() {
                    "minecraft:block_interaction_range" => {
                        Some(("minecraft:creative_mode_block_range", 0.5))
                    }
                    "minecraft:entity_interaction_range" => {
                        Some(("minecraft:creative_mode_entity_range", 2.0))
                    }
                    _ => None,
                };
                profile != PlayerBodyProjection::RetainedNative
                    || guest.game_mode != 1
                    || value.modifiers().len() != 1
                    || !expected.is_some_and(|(id, amount)| {
                        modifier.id() == id
                            && modifier.amount() == amount
                            && modifier.operation()
                                == horizon_world::attributes::Operation::AddValue
                    })
            })
    }) {
        return Err("nondefault session player body attributes are unsupported".into());
    }
    // These mirror fields are native owners too. A stale guest cache cannot hide
    // a changed body, nor can a stale body silently replace a newer guest value.
    if !(0..=3).contains(&guest.game_mode)
        || [body.position.x, body.position.y, body.position.z] != guest.position
        || [body.yaw, body.pitch] != [guest.yaw, guest.pitch]
        || body.ground.on_ground != guest.on_ground
        || body.health.health() != guest.health
        || body.health.food.food != guest.food
        || body.health.food.saturation != guest.saturation
        || body.may_fly != matches!(guest.game_mode, 1 | 3)
        || body.invulnerable != matches!(guest.game_mode, 1 | 3)
        || body.spectator != (guest.game_mode == 3)
        || body.flying != guest.flying
        || body.flying_speed != 0.05
        || body.fall_flying
        || guest.fall_flying
        || body.ticks_frozen != 0
        || !guest.effects.saved().is_empty()
        || (matches!(guest.game_mode, 0 | 2) && body.flying)
        || (guest.game_mode == 3 && !body.flying)
        || !(i16::MIN as i32..=i16::MAX as i32).contains(&body.fire_ticks)
        || !(i16::MIN as i32..=i16::MAX as i32).contains(&body.air)
    {
        return Err("unsupported or inconsistent actual session player body".into());
    }
    // Every non-body value comes from the actual guest via the native writer.
    // In particular the integrated owner's cached Level supplies no body values.
    let mut level = Level {
        player_uuid: Some(record_uuid),
        ..Level::default()
    };
    playerdata::store(guest, &mut level);
    level.position = Some([body.position.x, body.position.y, body.position.z]);
    level.rotation = [body.yaw, body.pitch];
    level.on_ground = body.ground.on_ground;
    level.health = body.health.health();
    level.absorption = body.health.absorption();
    level.fall_distance = body.fall_distance;
    let Some(horizon_world::nbt::Tag::Compound(extra)) = &mut level.player_extra else {
        return Err("session native body extras were not projected".into());
    };
    extra.insert(
        "Fire".into(),
        horizon_world::nbt::Tag::Short(body.fire_ticks as i16),
    );
    extra.insert(
        "Air".into(),
        horizon_world::nbt::Tag::Short(body.air as i16),
    );
    level.food = body.health.food.food;
    level.saturation = body.health.food.saturation;
    level.flying = body.flying;
    level.motion = [body.velocity.x, body.velocity.y, body.velocity.z];
    level.food_exhaustion = body.health.food.exhaustion;
    level.food_tick_timer = body.health.food.timer;
    validate_session_player_tag(&level.player_tag())?;
    Ok(level)
}

#[cfg(feature = "paired-packages")]
fn session_body_matches_level(
    body: &horizon_client::motion::PlayerMotion,
    level: &horizon_client::save::Level,
) -> bool {
    use horizon_world::nbt::Tag;
    let extra = level.player_extra.as_ref();
    let fire = match extra.and_then(|tag| tag.get("Fire")) {
        Some(Tag::Short(value)) => i32::from(*value),
        _ => return false,
    };
    let air = match extra.and_then(|tag| tag.get("Air")) {
        Some(Tag::Short(value)) => i32::from(*value),
        _ => return false,
    };
    Some([body.position.x, body.position.y, body.position.z]) == level.position
        && [body.yaw, body.pitch] == level.rotation
        && [body.velocity.x, body.velocity.y, body.velocity.z] == level.motion
        && body.ground.on_ground == level.on_ground
        && body.health.health() == level.health
        && body.health.absorption() == level.absorption
        && body.fire_ticks == fire
        && body.air == air
        && body.fall_distance == level.fall_distance
        && body.health.food.food == level.food
        && body.health.food.saturation == level.saturation
        && body.health.food.exhaustion == level.food_exhaustion
        && body.health.food.timer == level.food_tick_timer
        && body.may_fly == matches!(level.mode(), 1 | 3)
        && body.invulnerable == matches!(level.mode(), 1 | 3)
        && body.spectator == (level.mode() == 3)
        && body.flying == level.flying
}

#[cfg(feature = "paired-packages")]
#[allow(dead_code)] // Only the internal session operation may consume these rows.
impl Game<'_> {
    pub(in crate::game) fn project_session_participant_rows(
        &self,
        expected: &ParticipantRoster,
    ) -> Result<Vec<SessionPlayerRow>> {
        // Reuse the existing population/owner checks, without constructing the
        // research projection (which deliberately retains its cached Level rules).
        self.preflight_checkpoint_participants(expected)
            .map_err(|error| format!("session participant preflight: {error:?}"))?;
        self.pack_checked_participant_rows(expected)
    }

    fn pack_checked_participant_rows(
        &self,
        expected: &ParticipantRoster,
    ) -> Result<Vec<SessionPlayerRow>> {
        if self.adv.tree.list.is_empty() || self.adv.pending_tree.is_some() {
            return Err("session advancement tree is unavailable".into());
        }
        let host = self
            .checkpoint_participant_host()
            .map_err(|error| format!("session participant host: {error:?}"))?;
        let mut rows = Vec::new();
        for identity in &expected.identities {
            let guest = host
                .hub
                .guests
                .iter()
                .find(|g| g.conn == identity.conn)
                .ok_or("session participant disappeared")?;
            // Inspect the live graph before invoking a native item/player encoder.
            let mut stacks = guest
                .inventory
                .iter()
                .chain(guest.armor.iter())
                .chain(std::iter::once(&guest.offhand))
                .chain(guest.ender_items.iter())
                .flatten()
                .map(|stack| (stack, 0usize))
                .collect::<Vec<_>>();
            let mut nodes = 0usize;
            while let Some((stack, depth)) = stacks.pop() {
                nodes += 1;
                if nodes > 16_384
                    || depth > 32
                    || stack.count == 0
                    || stack.count > i32::MAX as u32
                    || &*stack.item == "minecraft:air"
                    || self.res.items().get(&stack.item).is_none()
                {
                    return Err("unsupported live session item graph".into());
                }
                if let Some(data) = stack.data() {
                    if data.bundle_selected.0 != -1 {
                        return Err("unsupported live session runtime bundle selection".into());
                    }
                    if data.container.iter().enumerate().any(|(index, (slot, _))| {
                        data.container[..index]
                            .iter()
                            .any(|(other, _)| other == slot)
                    }) {
                        return Err("duplicate live session container cell".into());
                    }
                    for (key, value) in &data.other {
                        if matches!(
                            key.as_str(),
                            "minecraft:attribute_modifiers"
                                | "!minecraft:attribute_modifiers"
                                | "attribute_modifiers"
                        ) || !session_player_payload_supported(value)
                        {
                            return Err("unsupported live session item component".into());
                        }
                    }
                    let children = data
                        .container
                        .len()
                        .checked_add(data.charged.len())
                        .and_then(|n| n.checked_add(data.bundle_contents.len()))
                        .and_then(|n| {
                            n.checked_add(
                                data.pot_decorations
                                    .as_ref()
                                    .map_or(0, |p| p.iter().flatten().count()),
                            )
                        })
                        .ok_or("session item graph size overflow")?;
                    if children > 16_384usize.saturating_sub(nodes + stacks.len()) {
                        return Err("session item graph exceeds bound".into());
                    }
                    stacks
                        .try_reserve(children)
                        .map_err(|_| "session item graph allocation")?;
                    stacks.extend(data.container.iter().map(|(_, stack)| (stack, depth + 1)));
                    stacks.extend(data.charged.iter().map(|stack| (stack, depth + 1)));
                    stacks.extend(data.bundle_contents.iter().map(|stack| (stack, depth + 1)));
                    if let Some(pots) = &data.pot_decorations {
                        stacks.extend(pots.iter().flatten().map(|stack| (stack, depth + 1)));
                    }
                }
            }
            // The whole graph is bounded and finite before the structured native
            // encoder. A modelled field which the codec would coerce must refuse.
            for stack in guest
                .inventory
                .iter()
                .chain(guest.armor.iter())
                .chain(std::iter::once(&guest.offhand))
                .chain(guest.ender_items.iter())
                .flatten()
            {
                let tag = horizon_client::save::item_to_nbt(stack);
                if !session_player_payload_supported(&tag)
                    || horizon_client::save::item_from_nbt(&tag).as_ref() != Some(stack)
                {
                    return Err("native session item encoding changes a live stack".into());
                }
            }
            let play = host
                .play
                .get(&identity.conn)
                .ok_or("session play owner missing")?;
            let (record_uuid, integrated_owner) = match identity.storage {
                ParticipantStorage::IntegratedOwner { record_uuid } => (record_uuid, true),
                ParticipantStorage::External { record_uuid } => (record_uuid, false),
            };
            let level = match &expected.host_identity {
                ParticipantHostIdentity::Legacy(_) => session_player_level(guest, record_uuid)?,
                ParticipantHostIdentity::Retained(_) => {
                    pack_player_level(guest, record_uuid, PlayerBodyProjection::RetainedNative)?
                }
            };
            rows.try_reserve(1)
                .map_err(|_| "session player row allocation")?;
            rows.push(SessionPlayerRow {
                conn: identity.conn,
                profile_uuid: identity.uuid,
                record_uuid,
                integrated_owner,
                dimension: identity.dimension.clone(),
                player: level.player_tag(),
                advancements: guest
                    .advancements
                    .to_json(&self.adv.tree, horizon_world::DATA_VERSION),
                stats: play.stats.to_json(),
            });
        }
        Ok(rows)
    }

    /// The caller owns the whole fresh Game until every installation check passes.
    /// This is not a join/reconnect entry point and never runs guest gameplay.
    pub(in crate::game) fn install_session_checkpoint_participants(
        &mut self,
        mut host: LanHost,
        checked: &super::levels::session_checkpoint::CheckedSessionPlayers,
    ) -> Result<()> {
        if host
            .hub
            .guests
            .iter()
            .any(|guest| !guest.placed || !guest.loaded)
        {
            return Err("session recovery requires placed and loaded participants".into());
        }
        self.install_session_participant_rows(&mut host, checked)?;
        self.lan = Some(host);
        self.check_installed_session_participants(checked)
    }

    /// The staged route has a distinct sealed transport/roster eligibility gate.
    /// This shared implementation never manufactures readiness or placement.
    fn install_session_participant_rows(
        &mut self,
        host: &mut LanHost,
        checked: &super::levels::session_checkpoint::CheckedSessionPlayers,
    ) -> Result<()> {
        use horizon_client::{motion::PlayerMotion, save::Level};
        use std::sync::atomic::Ordering;
        let rows = checked.rows();
        if self.lan.is_some()
            || self.remote.is_some()
            || self.integrated.is_some()
            || self.options.execution_role.is_client()
            || self.lan_acting.is_some()
            || self.player.is_some()
            || !self.server_trips.is_empty()
            || !self.level_teleports.is_empty()
            || self.pending_save.is_some()
            || self.pending_entry.is_some()
            || self.world_loader.is_some()
            || self.rest_loader.is_some()
            || self.saved_for_exit
            || self.carried.is_some()
            || self.craft_grid.iter().any(Option::is_some)
            || self.scratch_slot.is_some()
            || self.container.is_some()
            || self.station.is_some()
            || self.ender_open.is_some()
            || self.craft_table.is_some()
            || self.inv_drag.is_some()
            || self.last_quick_moved.is_some()
            || !self.guest_packets.is_empty()
            || !self.guest_entity_events.is_empty()
            || !host.commands.is_empty()
            || !host.quiet.is_empty()
            || !host.models.is_empty()
            || !host.broken.is_empty()
            || !host.mob_state.is_empty()
            || !host.mob_players.is_empty()
            || !host.states.as_slice().is_empty()
            || !host.waypoints.is_empty()
            || !host.waypoint_dead.is_empty()
            || host.border_sent.is_some()
            || host.host_crack.is_some()
            || !host.hub.riding.passengers.is_empty()
            || !host.hub.riding.vehicle.is_empty()
            || host.hub.guests.len() != rows.len()
            || host.hub.config.integrated != host.owner.is_some()
            || host
                .owner
                .as_ref()
                .is_some_and(|owner| owner.saved.is_some())
            || self
                .serving
                .as_ref()
                .is_some_and(|s| Some(s.owner) != host.owner.as_ref().map(|o| o.uuid))
        {
            return Err("session recovery requires a fresh headless participant owner".into());
        }
        // Opening the native file populated either the integrated owner's
        // redundant cache or the dedicated server's inert native metadata record.
        let native_uuid = self
            .level
            .player_uuid
            .ok_or("native local-player UUID is missing")?;
        let local_tag = self.level.player_tag();
        let local = rows.iter().find(|row| row.record_uuid == native_uuid);
        let local_matches = match &host.owner {
            Some(owner) => local.is_some_and(|row| {
                row.integrated_owner
                    && row.profile_uuid == owner.uuid
                    && session_player_tags_equal(&local_tag, &row.player)
            }),
            None => local.is_none() && session_empty_native_player(&local_tag),
        };
        if !local_matches {
            return Err("native local-player cache differs from its verified participant".into());
        }
        let mut inventory = vec![None; 36];
        let mut armor = [None, None, None, None];
        let mut offhand = None;
        for (slot, item, count, damage) in &self.level.inventory {
            let stack = Some(ItemStack {
                item: Arc::from(item.as_str()),
                count: *count,
                damage: *damage,
                extra: self.level.inventory_components.get(slot).cloned(),
            });
            match *slot {
                0..=35 => inventory[usize::from(*slot)] = stack,
                100..=103 => armor[usize::from(103 - *slot)] = stack,
                150 => offhand = stack,
                _ => return Err("unsupported local-player inventory cell".into()),
            }
        }
        if self.inventory != inventory
            || self.armor != armor
            || self.offhand != offhand
            || self.ender_items != self.level.ender_items
        {
            return Err("native local-player item cache has changed".into());
        }
        for &conn in host.play.keys() {
            if !host.hub.guests.iter().any(|g| g.conn == conn) {
                return Err("fresh participant host has orphan play state".into());
            }
        }
        for (index, guest) in host.hub.guests.iter().enumerate() {
            if host.hub.guests[..index]
                .iter()
                .any(|g| g.uuid == guest.uuid || g.conn == guest.conn)
                || guest.inventory.len() != 36
                || guest.ender_items.len() != 27
                || guest.inventory.iter().any(Option::is_some)
                || guest.armor.iter().any(Option::is_some)
                || guest.offhand.is_some()
                || guest.ender_items.iter().any(Option::is_some)
                || guest.motion.is_some()
                || guest.loaded_body.is_some()
                || guest.saved_absorption.is_some()
                || guest.saved.is_some()
                || guest.logs_kept.is_some()
                || !guest.left_reason.is_empty()
                || guest.dead
                || guest.death_time != 0
                || guest.awaiting_teleport.is_some()
                || guest.carried.is_some()
                || guest.craft_grid.iter().any(Option::is_some)
                || guest.craft_result.is_some()
                || guest.menu.is_some()
                || guest.drag.is_some()
                || guest.vehicle.is_some()
                || guest.sleeping
                || guest.bed.is_some()
                || guest.sleep_timer != 0
                || guest.sent_bed.is_some()
                || !guest.inbox.is_empty()
                || !guest.moves.is_empty()
                || guest.sender.pending.load(Ordering::Acquire) != 0
                || !guest.effects.saved().is_empty()
                || !guest.sent_effects.is_empty()
                || guest.advancements_loaded
                || !guest.advancements.criteria.is_empty()
                || !guest.advancements_changed.is_empty()
                || !guest.recipes_unlocked.is_empty()
                || guest.health != 20.0
                || guest.food != 20
                || guest.saturation != 5.0
                || guest.xp_level != 0
                || guest.xp_total != 0
                || guest.xp_progress != 0.0
                || guest.xp_seed != 0
                || guest.seen_credits
                || guest.last_death.is_some()
                || guest.spawn.is_some()
                || guest.respawn_elsewhere.is_some()
                || guest.recipe_book.write()
                    != horizon_client::recipe_book::Settings::default().write()
                || host.play.get(&guest.conn).is_some_and(|play| {
                    !play.checkpoint_participant_idle()
                        || !play.stats.values.is_empty()
                        || !play.stats.events.is_empty()
                })
            {
                return Err("session recovery participant is not fresh and empty".into());
            }
            let row = rows
                .iter()
                .find(|row| row.profile_uuid == guest.uuid)
                .ok_or("session recovery actual participant UUID mismatch")?;
            let owner = host.owner.as_ref().map(|o| o.uuid);
            if row.integrated_owner != (owner == Some(guest.uuid))
                || record_uuid(owner, self.level.player_uuid, guest.uuid) != Some(row.record_uuid)
                || guest.dimension != row.dimension
            {
                return Err("session recovery participant storage mapping mismatch".into());
            }
        }
        if rows.iter().filter(|row| row.integrated_owner).count()
            != usize::from(host.owner.is_some())
        {
            return Err("session recovery integrated owner mismatch".into());
        }
        for guest in &mut host.hub.guests {
            let row = rows
                .iter()
                .find(|row| row.profile_uuid == guest.uuid)
                .ok_or("session recovery participant row missing")?;
            validate_session_player_tag(&row.player)?;
            let mut level = Level {
                player_uuid: Some(row.record_uuid),
                ..Level::default()
            };
            level.read_player(&row.player);
            level.player_extra = Some(row.player.clone());
            playerdata::apply(guest, &level);
            let position = level.position.ok_or("session player position missing")?;
            let mut body = PlayerMotion::new(
                self.motion,
                Vec3::new(position[0], position[1], position[2]),
            )?;
            let mut pending = true;
            body.load_saved_state_once(&mut pending, &level)?;
            // Consume native staging exactly once; no enter/leave_guest side effects.
            guest
                .loaded_body
                .take()
                .ok_or("native player body was not staged")?
                .apply(&mut body);
            let absorption = guest
                .saved_absorption
                .take()
                .ok_or("native absorption was not staged")?;
            body.internal_set_absorption(absorption)?;
            body.velocity = Vec3::new(level.motion[0], level.motion[1], level.motion[2]);
            body.health.food.exhaustion = level.food_exhaustion;
            body.health.food.timer = level.food_tick_timer;
            body.yaw = level.rotation[0];
            body.head_yaw = body.yaw;
            body.pitch = level.rotation[1];
            body.ground.on_ground = level.on_ground;
            body.server_copy = true;
            body.may_fly = matches!(level.mode(), 1 | 3);
            body.invulnerable = body.may_fly;
            body.spectator = level.mode() == 3;
            body.flying = level.flying;
            if pending || !session_body_matches_level(&body, &level) {
                return Err("actual restored session body differs from saved body".into());
            }
            guest.motion = Some(Box::new(body));
            guest.advancements =
                horizon_client::advancements::Progress::from_json(&row.advancements);
            guest.advancements_loaded = true;
            let play = host.play.entry(guest.conn).or_default();
            play.stats.loaded = true;
            let values = row
                .stats
                .get("stats")
                .and_then(serde_json::Value::as_object)
                .ok_or("session statistics object missing")?;
            for (kind, entries) in values {
                let entries = entries
                    .as_object()
                    .ok_or("session statistics entries malformed")?;
                let values = play.stats.values.entry(kind.clone()).or_default();
                for (key, value) in entries {
                    let value = value
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .ok_or("session statistic is not an i32")?;
                    values.insert(key.clone(), value);
                }
            }
            // This projection reads every admitted saved body value directly from
            // the installed motion, before inspecting its native encoding.
            if !session_player_tags_equal(
                &session_player_level(guest, row.record_uuid)?.player_tag(),
                &row.player,
            ) || guest
                .advancements
                .to_json(&self.adv.tree, horizon_world::DATA_VERSION)
                != row.advancements
                || play.stats.to_json() != row.stats
            {
                return Err("session participant restoration changed a saved value".into());
            }
        }
        let dedicated_cache = host.owner.is_none();
        self.inventory.fill(None);
        self.armor.fill(None);
        self.offhand = None;
        self.ender_items.fill(None);
        self.restore_player_state = false;
        self.level.inventory.clear();
        self.level.inventory_components.clear();
        self.level.ender_items = vec![None; 27];
        self.level.player_extra = dedicated_cache.then_some(local_tag);
        // Keep the verified native singleplayer UUID. In dedicated mode it is
        // the inert metadata record, never one of the external guest records;
        // the next native save must not overwrite a guest with this empty cache.
        self.level.active_effects.clear();
        self.level.position = None;
        if !dedicated_cache {
            self.level.rotation = [0.0; 2];
        }
        self.level.motion = [0.0; 3];
        self.level.health = 20.0;
        self.level.absorption = 0.0;
        self.level.food = 20;
        self.level.saturation = 5.0;
        self.level.food_exhaustion = 0.0;
        self.level.food_tick_timer = 0;
        self.level.fall_distance = 0.0;
        self.level.on_ground = false;
        self.level.flying = false;
        self.level.xp_level = 0;
        self.level.xp_progress = 0.0;
        self.level.xp_total = 0;
        self.level.xp_seed = 0;
        self.level.selected_slot = 0;
        self.level.spawn = None;
        self.level.respawn_elsewhere = None;
        self.level.seen_credits = false;
        self.level.last_death = None;
        self.level.recipe_book = Default::default();
        self.potions = Default::default();
        self.experience = Default::default();
        self.adv.progress = Default::default();
        self.stats = Default::default();
        self.spawn = None;
        self.respawn_config = None;
        Ok(())
    }

    fn install_staged_session_participants(
        &mut self,
        host: &mut LanHost,
        checked: &super::levels::session_checkpoint::CheckedSessionPlayers,
        roster: &horizon_client::lan::hub::StagedRoster,
    ) -> Result<()> {
        host.hub.validate_staged_roster(roster)?;
        if host
            .hub
            .guests
            .iter()
            .any(|guest| guest.placed || guest.loaded)
        {
            return Err("staged recovery participants must be genuinely unplaced".into());
        }
        self.install_session_participant_rows(host, checked)
    }

    fn check_installed_session_participants(
        &self,
        checked: &super::levels::session_checkpoint::CheckedSessionPlayers,
    ) -> Result<()> {
        let rows = checked.rows();
        let roster = self
            .observe_checkpoint_participants()
            .map_err(|error| format!("restored session roster: {error:?}"))?;
        let actual = self.project_session_participant_rows(&roster)?;
        if actual.len() != rows.len()
            || actual.iter().any(|actual| {
                !rows.iter().any(|row| {
                    actual.profile_uuid == row.profile_uuid
                        && actual.record_uuid == row.record_uuid
                        && actual.integrated_owner == row.integrated_owner
                        && actual.dimension == row.dimension
                        && session_player_tags_equal(&actual.player, &row.player)
                        && actual.advancements == row.advancements
                        && actual.stats == row.stats
                })
            })
        {
            return Err("installed session participants differ from verified rows".into());
        }
        Ok(())
    }
}

fn save_guest_records_to(
    world: &std::path::Path,
    tree: &horizon_client::advancements::Tree,
    owner_uuid: Option<u128>,
    level_uuid: Option<[i32; 4]>,
    guest: &horizon_client::lan::guest::Guest,
    play: Option<&super::lan_actions::GuestPlay>,
) -> Result<()> {
    if !guest.placed || tree.list.is_empty() {
        return Ok(());
    }
    let Some(uuid) = record_uuid(owner_uuid, level_uuid, guest.uuid) else {
        return Ok(());
    };
    save_guest_record_values_to(
        world,
        tree,
        uuid,
        &guest.advancements,
        play.map(|play| &play.stats),
    )
}

fn save_guest_record_values_to(
    world: &std::path::Path,
    tree: &horizon_client::advancements::Tree,
    uuid: [i32; 4],
    advancements: &horizon_client::advancements::Progress,
    stats: Option<&super::stats::Stats>,
) -> Result<()> {
    let advancement_result = super::advancements::write_progress(
        &super::stats::player_json(world, "advancements", uuid),
        advancements,
        tree,
    );
    // Both independent records are attempted, in the original advancement-then-stats order.
    let stats_result = match stats {
        Some(stats) => stats.write(&super::stats::player_json(world, "stats", uuid)),
        None => Ok(()),
    };
    advancement_result.and(stats_result)
}

fn store_owner_records_to(
    world: &std::path::Path,
    level: &mut horizon_client::save::Level,
    owner_uuid: Option<u128>,
    guests: &[horizon_client::lan::guest::Guest],
    play: &HashMap<u32, super::lan_actions::GuestPlay>,
    tree: &horizon_client::advancements::Tree,
) -> Result<()> {
    let Some(guest) = project_owner_to(level, owner_uuid, guests) else {
        return Ok(());
    };
    save_guest_records_to(
        world,
        tree,
        owner_uuid,
        level.player_uuid,
        guest,
        play.get(&guest.conn),
    )
}

/// Native owner packing is pure; ordinary saving and retained close share it.
fn project_owner_to<'a>(
    level: &mut horizon_client::save::Level,
    owner_uuid: Option<u128>,
    guests: &'a [horizon_client::lan::guest::Guest],
) -> Option<&'a horizon_client::lan::guest::Guest> {
    let uuid = owner_uuid?;
    let guest = guests
        .iter()
        .find(|guest| guest.uuid == uuid && guest.placed)?;
    playerdata::store(guest, level);
    Some(guest)
}

#[cfg(feature = "paired-packages")]
pub(in crate::game) struct NativeParticipantsExpected {
    roster: Option<ParticipantRoster>,
    rows: Vec<SessionPlayerRow>,
    tree: Arc<horizon_client::advancements::Tree>,
}

#[cfg(feature = "paired-packages")]
#[derive(Clone)]
struct NativeParticipantRecord {
    row: SessionPlayerRow,
    advancements: horizon_client::advancements::Progress,
    stats: Arc<super::stats::Stats>,
}

#[cfg(feature = "paired-packages")]
#[derive(Clone)]
pub(in crate::game) struct NativeParticipantsInput {
    world: std::path::PathBuf,
    tree: Arc<horizon_client::advancements::Tree>,
    records: Vec<NativeParticipantRecord>,
}

#[cfg(feature = "paired-packages")]
impl NativeParticipantsInput {
    pub(in crate::game) fn write_native(&self) -> std::result::Result<(), String> {
        let mut first_error = None;
        for record in &self.records {
            // The integrated owner is written by Level::write. External records
            // use exactly its native player NBT encoder captured on the CPU.
            let player_result = if record.row.integrated_owner {
                Ok(())
            } else {
                horizon_client::save::write_dat(
                    &playerdata::path(&self.world, record.row.profile_uuid),
                    record.row.player.clone(),
                )
            };
            let records_result = save_guest_record_values_to(
                &self.world,
                &self.tree,
                record.row.record_uuid,
                &record.advancements,
                Some(&record.stats),
            );
            for result in [player_result, records_result] {
                if let Err(error) = result {
                    first_error.get_or_insert_with(|| error.to_string());
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(feature = "paired-packages")]
impl Game<'_> {
    pub(in crate::game) fn project_native_owner_identity(
        &self,
        level: &mut horizon_client::save::Level,
    ) {
        if let Some(host) = &self.lan {
            project_owner_to(
                level,
                host.owner.as_ref().map(|owner| owner.uuid),
                &host.hub.guests,
            );
        }
    }

    pub(in crate::game) fn project_native_owner(
        &self,
        level: &mut horizon_client::save::Level,
    ) -> std::result::Result<(), String> {
        if let Some(host) = &self.lan
            && let Some(guest) = project_owner_to(
                level,
                host.owner.as_ref().map(|owner| owner.uuid),
                &host.hub.guests,
            )
        {
            let uuid = level
                .player_uuid
                .ok_or("native owner record UUID is missing")?;
            let actual = session_player_level(guest, uuid).map_err(|error| error.to_string())?;
            // The retained session projection includes these actual body
            // fields; ordinary owner saves retain their existing behavior.
            level.motion = actual.motion;
            level.food_exhaustion = actual.food_exhaustion;
            level.food_tick_timer = actual.food_tick_timer;
            // The participant owns an explicit forced spawn even when it
            // coincides with world spawn, which Level's fallback would omit.
            if actual.respawn_elsewhere.is_none()
                && let Some(block) = actual
                    .spawn
                    .map(|spawn| spawn.map(|value| value.floor() as i32))
                && level.world_spawn == Some(block)
            {
                level.respawn_elsewhere = Some(("minecraft:overworld".into(), block, 0.0, true));
            }
            if level.player_tag() != actual.player_tag() {
                return Err("native owner projection differs from its actual session row".into());
            }
        }
        Ok(())
    }

    pub(in crate::game) fn prepare_native_participants(
        &self,
    ) -> std::result::Result<(NativeParticipantsExpected, NativeParticipantsInput), String> {
        let roster = self
            .lan
            .as_ref()
            .map(|_| {
                if self.retained_server.is_some() {
                    self.observe_retained_native_participants()
                } else {
                    self.observe_checkpoint_participants()
                }
            })
            .transpose()
            .map_err(|error| format!("native close roster: {error:?}"))?;
        let rows = match &roster {
            Some(roster) => self
                .project_native_participant_rows(roster)
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        let mut records = Vec::new();
        if let Some(host) = &self.lan {
            for row in &rows {
                let guest = host
                    .hub
                    .guests
                    .iter()
                    .find(|guest| guest.conn == row.conn)
                    .ok_or("native close participant disappeared")?;
                let play = host
                    .play
                    .get(&row.conn)
                    .ok_or("native close play owner disappeared")?;
                let mut stats = super::stats::Stats::default();
                stats.values = play.stats.values.clone();
                records.push(NativeParticipantRecord {
                    row: row.clone(),
                    advancements: guest.advancements.clone(),
                    stats: Arc::new(stats),
                });
            }
        }
        Ok((
            NativeParticipantsExpected {
                roster,
                rows,
                tree: self.adv.tree.clone(),
            },
            NativeParticipantsInput {
                world: self.world_root.clone(),
                tree: self.adv.tree.clone(),
                records,
            },
        ))
    }

    pub(in crate::game) fn preflight_native_participants(
        &self,
        expected: &NativeParticipantsExpected,
    ) -> std::result::Result<(), String> {
        if !Arc::ptr_eq(&expected.tree, &self.adv.tree) {
            return Err("native close advancement tree owner changed".into());
        }
        let rows = match &expected.roster {
            Some(roster) => self
                .project_native_participant_rows(roster)
                .map_err(|error| error.to_string())?,
            None if self.lan.is_none() => Vec::new(),
            None => return Err("native close acquired a participant host".into()),
        };
        if rows.len() != expected.rows.len()
            || rows.iter().zip(&expected.rows).any(|(a, b)| {
                a.conn != b.conn
                    || a.profile_uuid != b.profile_uuid
                    || a.record_uuid != b.record_uuid
                    || a.integrated_owner != b.integrated_owner
                    || a.dimension != b.dimension
                    || a.player != b.player
                    || a.advancements != b.advancements
                    || a.stats != b.stats
            })
        {
            return Err("native close participant payload changed".into());
        }
        Ok(())
    }
}

/// A stable UUID for an entity that has none.
pub(super) fn entity_uuid(id: u32) -> u128 {
    0x4856_5a4e_0000_0000_0000_0000_0000_0000u128 | u128::from(id)
}

/// The SynchedEntityData index of the baby flag: AgeableMob.DATA_BABY_ID and
/// Zombie.DATA_BABY_ID are both the first entry after Mob's (16); Piglin's
/// comes after AbstractPiglin's zombification flag (17).
pub(super) fn baby_index(kind: &str) -> Option<u8> {
    match kind {
        "minecraft:piglin" => Some(17),
        "minecraft:cow"
        | "minecraft:mooshroom"
        | "minecraft:pig"
        | "minecraft:sheep"
        | "minecraft:chicken"
        | "minecraft:horse"
        | "minecraft:donkey"
        | "minecraft:mule"
        | "minecraft:skeleton_horse"
        | "minecraft:zombie_horse"
        | "minecraft:llama"
        | "minecraft:trader_llama"
        | "minecraft:cat"
        | "minecraft:wolf"
        | "minecraft:fox"
        | "minecraft:rabbit"
        | "minecraft:panda"
        | "minecraft:polar_bear"
        | "minecraft:bee"
        | "minecraft:frog"
        | "minecraft:sniffer"
        | "minecraft:armadillo"
        | "minecraft:villager"
        | "minecraft:zombie"
        | "minecraft:husk"
        | "minecraft:drowned"
        | "minecraft:zombie_villager"
        | "minecraft:zombified_piglin"
        | "minecraft:hoglin"
        | "minecraft:zoglin"
        | "minecraft:goat"
        | "minecraft:ocelot"
        | "minecraft:turtle"
        | "minecraft:strider"
        | "minecraft:camel"
        | "minecraft:axolotl"
        | "minecraft:dolphin"
        | "minecraft:squid"
        | "minecraft:glow_squid" => Some(16),
        _ => None,
    }
}

/// The variant and look entries of `mob_meta`'s mobs (registry holders, the
/// fish's, the bee's, the copper golem's weathering...), in a match of their
/// own: `mob_meta`'s arms hold these mobs' other entries.
fn mob_variant_meta(
    m: &Mob,
    hub: &Hub,
    meta: &mut Vec<(u8, i32, Value)>,
    defaults: &mut Vec<(u8, i32, Value)>,
) {
    let (kind, e) = (m.kind.id.as_str(), &m.extra);
    let id = |registry: &str, name: &str| hub.registry_id(registry, &format!("minecraft:{name}"));
    match kind {
        // Axolotl.DATA_VARIANT (after AgeableMob's two).
        "minecraft:axolotl" => {
            if e.variant != 0 {
                meta.push((18, serializer::INT, Value::Int(i32::from(e.variant))));
            }
        }
        // Frog.DATA_VARIANT_ID, Pig's, Cow's, Chicken's and ZombieNautilus's
        // (after the entries of their parents): registry holders.
        "minecraft:frog"
        | "minecraft:pig"
        | "minecraft:cow"
        | "minecraft:chicken"
        | "minecraft:zombie_nautilus" => {
            let (index, serializer, registry) = match kind {
                "minecraft:frog" => (18, serializer::FROG_VARIANT, "minecraft:frog_variant"),
                "minecraft:pig" => (19, serializer::PIG_VARIANT, "minecraft:pig_variant"),
                "minecraft:cow" => (18, serializer::COW_VARIANT, "minecraft:cow_variant"),
                "minecraft:chicken" => {
                    (18, serializer::CHICKEN_VARIANT, "minecraft:chicken_variant")
                }
                _ => (
                    21,
                    serializer::ZOMBIE_NAUTILUS_VARIANT,
                    "minecraft:zombie_nautilus_variant",
                ),
            };
            // Each registry's default (`define(DATA_VARIANT_ID, ... TEMPERATE)`)
            // stays out of the pairing data, as other default values do.
            let name = horizon_client::mobs::finalize::variant_name(m.species(), e.variant);
            let name = name.trim_start_matches("minecraft:");
            if let Some(v) = id(registry, name)
                && name != "temperate"
            {
                meta.push((index, serializer, Value::Id(v)));
            }
            if let Some(v) = id(registry, "temperate") {
                defaults.push((index, serializer, Value::Id(v)));
            }
        }
        // Rabbit.DATA_TYPE_ID (after AgeableMob's two) and
        // Parrot.DATA_VARIANT_ID (after TamableAnimal's two and its flags and
        // owner). The fox's type goes with its flags (#2538).
        "minecraft:rabbit" | "minecraft:parrot" => {
            let index = if kind == "minecraft:parrot" { 20 } else { 18 };
            if e.variant != 0 {
                meta.push((index, serializer::INT, Value::Int(i32::from(e.variant))));
            }
        }
        // Bee.DATA_FLAGS_ID (rolling 2, stung 4, nectar 8) and
        // DATA_ANGER_END_TIME: the texture shows the nectar and the anger.
        "minecraft:bee" => {
            let bee = horizon_client::mobs::bee::get(m);
            let flags = bee.map_or(0, |b| b.flags);
            if flags != 0 {
                meta.push((18, serializer::BYTE, Value::Byte(flags as i8)));
            }
            defaults.push((18, serializer::BYTE, Value::Byte(0)));
            let anger_end = bee.map_or(-1, |b| b.anger_end);
            if anger_end != -1 {
                meta.push((19, serializer::LONG, Value::Long(anger_end)));
            }
            defaults.push((19, serializer::LONG, Value::Long(-1)));
        }
        // AbstractFish.FROM_BUCKET takes 16: Salmon.DATA_TYPE (small 0,
        // medium 1, large 2; medium is the data's default, so always sent),
        // Pufferfish.PUFF_STATE and TropicalFish.DATA_ID_TYPE_VARIANT: the
        // pattern's packed id (size, then index << 8), base colour << 16 and
        // pattern colour << 24.
        "minecraft:salmon" => {
            meta.push((17, serializer::INT, Value::Int(i32::from(e.variant.min(2)))));
        }
        "minecraft:pufferfish" => {
            if e.size > 0 {
                meta.push((17, serializer::INT, Value::Int(e.size)));
            }
            defaults.push((17, serializer::INT, Value::Int(0)));
        }
        "minecraft:tropical_fish" => {
            let pattern = e.variant.min(11);
            let packed = i32::from(pattern >= 6)
                | i32::from(pattern % 6) << 8
                | i32::from(e.variant2 & 15) << 16
                | i32::from(e.variant3 & 15) << 24;
            if packed != 0 {
                meta.push((17, serializer::INT, Value::Int(packed)));
            }
        }
        // CopperGolem.DATA_WEATHER_STATE: unaffected 0 to oxidized 3, and it
        // goes back down when the golem is scraped.
        "minecraft:copper_golem" => {
            if e.variant != 0 {
                meta.push((
                    16,
                    serializer::WEATHERING_COPPER_STATE,
                    Value::Id(i32::from(e.variant.min(3))),
                ));
            }
            defaults.push((16, serializer::WEATHERING_COPPER_STATE, Value::Id(0)));
        }
        // Ghast.DATA_IS_CHARGING: the shooting texture.
        "minecraft:ghast" => {
            if e.flag {
                meta.push((16, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((16, serializer::BOOLEAN, Value::Bool(false)));
        }
        // WitherBoss.DATA_ID_INV (after the three head targets): the
        // invulnerable ticks, the blinking texture and the growing scale.
        "minecraft:wither" => {
            if e.counter > 0 {
                meta.push((19, serializer::INT, Value::Int(e.counter)));
            }
            defaults.push((19, serializer::INT, Value::Int(0)));
        }
        _ => {}
    }
}

/// A mob's synched data as ServerEntity sends it (only the values that differ
/// from the defaults the client constructs), with vanilla's entry indices:
/// Entity 0..=7, LivingEntity 8..=14, Mob 15, then the subclasses.
/// `defaults` gets the defaults of the entries that can go back to theirs.
fn mob_meta(
    m: &Mob,
    hub: &Hub,
    player_entity: Option<i32>,
    game_time: i64,
    meta: &mut Vec<(u8, i32, Value)>,
    defaults: &mut Vec<(u8, i32, Value)>,
) {
    let kind = m.kind.id.as_str();
    // Entity.DATA_SHARED_FLAGS_ID: on fire 0x01, swimming 0x10 (a drowned
    // under water, `Drowned.setSwimming`), invisible 0x20 (the effect's flag)
    // and glowing 0x40 (LivingEntity.updateGlowingStatus).
    let shared = i8::from(m.fire_ticks > 0)
        | i8::from(m.species() == Species::Drowned && m.extra.flag) << 4
        | i8::from(m.invisible || horizon_client::mobs::trader::is_invisible(m)) << 5
        | i8::from(m.is_currently_glowing()) << 6;
    if shared != 0 {
        meta.push((0, serializer::BYTE, Value::Byte(shared)));
    }
    // Entity.DATA_POSE: the pose the species keeps in its own fields (a
    // pose back at standing goes out by the entity's own default).
    let pose = horizon_client::mobs::sync::pose_id(m);
    if pose != 0 {
        meta.push((6, serializer::POSE, Value::Id(i32::from(pose))));
    }
    // LivingEntity.DATA_SLEEPING_POS_ID: the bed a sleeping villager lies in.
    if horizon_client::mobs::villager::sleeping(m)
        && let Some(bed) = m.extra.bed
    {
        meta.push((14, serializer::OPTIONAL_BLOCK_POS, Value::OptPos(Some(bed))));
    }
    if let Some(name) = &m.custom_name {
        meta.push((
            2,
            serializer::OPTIONAL_COMPONENT,
            Value::OptText(Some(horizon_protocol::text::literal(name))),
        ));
        if m.name_visible {
            meta.push((3, serializer::BOOLEAN, Value::Bool(true)));
        }
    }
    // LivingEntity.DATA_LIVING_ENTITY_FLAGS: using an item 1, in the off
    // hand 2 (a skeleton draws its bow, a witch raises her potion).
    let using = i8::from(m.extra.using_item > 0) | i8::from(m.extra.using_off_hand) << 1;
    if using != 0 {
        meta.push((8, serializer::BYTE, Value::Byte(using)));
    }
    defaults.push((8, serializer::BYTE, Value::Byte(0)));
    // LivingEntity.DATA_HEALTH_ID: defined as 1, then set to the max health.
    let health = m.health.max(0.0);
    if health != 1.0 {
        meta.push((9, serializer::FLOAT, Value::Float(health)));
    }
    defaults.push((9, serializer::FLOAT, Value::Float(1.0)));
    // LivingEntity.DATA_EFFECT_PARTICLES and DATA_EFFECT_AMBIENCE_ID: the
    // swirls of its effects (the ones a mob has are visible, not ambient).
    meta.extend(
        hub.effect_meta(
            (m.extra.effects.iter())
                .filter(|e| !m.extra.hidden_effects.contains(&e.0))
                .map(|e| (e.0, false)),
        ),
    );
    defaults.push((10, serializer::PARTICLES, Value::Particles(Vec::new())));
    defaults.push((11, serializer::BOOLEAN, Value::Bool(false)));
    // Mob.DATA_MOB_FLAGS_ID: no AI 1, left-handed 2, aggressive 4 (what
    // draws a vindicator's axe, a skeleton's drawn bow, a piglin's raised
    // weapon).
    let flags =
        i8::from(m.no_ai) | i8::from(m.extra.left_handed) << 1 | i8::from(m.ai.aggressive) << 2;
    if flags != 0 {
        meta.push((15, serializer::BYTE, Value::Byte(flags)));
    }
    defaults.push((15, serializer::BYTE, Value::Byte(0)));
    // AgeableMob.DATA_BABY_ID (and Zombie's, Piglin's).
    if let Some(index) = baby_index(kind) {
        if m.is_baby() {
            meta.push((index, serializer::BOOLEAN, Value::Bool(true)));
        }
        defaults.push((index, serializer::BOOLEAN, Value::Bool(false)));
    }
    let e = &m.extra;
    let id = |registry: &str, name: &str| hub.registry_id(registry, &format!("minecraft:{name}"));
    mob_variant_meta(m, hub, meta, defaults);
    match kind {
        "minecraft:guardian" | "minecraft:elder_guardian" => {
            // Guardian.DATA_ID_MOVING and DATA_ID_ATTACK_TARGET.
            if e.flag2 {
                meta.push((16, serializer::BOOLEAN, Value::Bool(true)));
            }
            let target = if e.flag {
                m.ai.target.and_then(|who| match who {
                    Who::Player => player_entity,
                    Who::Mob(id) => Some(id as i32),
                })
            } else {
                None
            };
            if let Some(target) = target {
                meta.push((17, serializer::INT, Value::Int(target)));
            }
            defaults.push((16, serializer::BOOLEAN, Value::Bool(false)));
            defaults.push((17, serializer::INT, Value::Int(0)));
        }
        // Bat.DATA_ID_FLAGS: resting in bit 0, flying by default.
        "minecraft:bat" => {
            if e.flag {
                meta.push((16, serializer::BYTE, Value::Byte(1)));
            }
            defaults.push((16, serializer::BYTE, Value::Byte(0)));
        }
        // SnowGolem.DATA_PUMPKIN_ID: 16 while the pumpkin is on (the default).
        "minecraft:snow_golem" => {
            if e.sheared {
                meta.push((16, serializer::BYTE, Value::Byte(0)));
            }
            defaults.push((16, serializer::BYTE, Value::Byte(16)));
        }
        // MushroomCow.DATA_TYPE: 0 red, 1 brown.
        "minecraft:mooshroom" => {
            if e.variant == 1 {
                meta.push((17, serializer::INT, Value::Int(1)));
            }
            defaults.push((17, serializer::INT, Value::Int(0)));
        }
        // AbstractPiglin.DATA_IMMUNE_TO_ZOMBIFICATION (16: with the dimension it
        // makes it shake) and Piglin.DATA_IS_DANCING (after the baby flag and
        // the crossbow charge).
        "minecraft:piglin" => {
            if e.flag2 {
                meta.push((16, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((16, serializer::BOOLEAN, Value::Bool(false)));
            if e.scratch[0] > 0 {
                meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
        }
        // Sheep.DATA_WOOL_ID: the colour in the low nibble, 16 when sheared.
        "minecraft:sheep" => {
            let value = (e.color & 15) | if e.sheared { 16 } else { 0 };
            if value != 0 {
                meta.push((18, serializer::BYTE, Value::Byte(value as i8)));
            }
            defaults.push((18, serializer::BYTE, Value::Byte(0)));
        }
        // Fox.DATA_TYPE_ID and DATA_FLAGS_ID (after AgeableMob's two; sleeping 32, ...).
        "minecraft:fox" => {
            if e.variant != 0 {
                meta.push((18, serializer::INT, Value::Int(i32::from(e.variant))));
            }
            defaults.push((18, serializer::INT, Value::Int(0)));
            let flags = (e.ints[0] & 0xff) as u8 as i8;
            if flags != 0 {
                meta.push((19, serializer::BYTE, Value::Byte(flags)));
            }
            defaults.push((19, serializer::BYTE, Value::Byte(0)));
        }
        "minecraft:parrot" => {
            // TamableAnimal.DATA_FLAGS_ID: sitting 1, tame 4.
            let flags = i8::from(e.sitting) | if e.tame { 4 } else { 0 };
            if flags != 0 {
                meta.push((18, serializer::BYTE, Value::Byte(flags)));
            }
        }
        "minecraft:cat" | "minecraft:wolf" => {
            // TamableAnimal.DATA_FLAGS_ID: sitting 1, tame 4.
            let flags = i8::from(e.sitting) | if e.tame { 4 } else { 0 };
            if flags != 0 {
                meta.push((18, serializer::BYTE, Value::Byte(flags)));
            }
            let (index, registry, name) = if kind == "minecraft:cat" {
                let v = horizon_client::mobs::tame::CAT_VARIANTS[(e.variant as usize).min(10)];
                (20, "minecraft:cat_variant", v)
            } else {
                let v = horizon_client::mobs::tame::WOLF_VARIANTS[(e.variant as usize).min(8)].0;
                (23, "minecraft:wolf_variant", v)
            };
            let serializer = if index == 20 {
                serializer::CAT_VARIANT
            } else {
                serializer::WOLF_VARIANT
            };
            if let Some(v) = id(registry, name) {
                meta.push((index, serializer, Value::Id(v)));
            }
            // Cat.IS_LYING and RELAX_STATE_ONE.
            if kind == "minecraft:cat" {
                for (index, on) in [(21, e.flag), (22, e.relax_one)] {
                    if on {
                        meta.push((index, serializer::BOOLEAN, Value::Bool(true)));
                    }
                    defaults.push((index, serializer::BOOLEAN, Value::Bool(false)));
                }
            }
            // Wolf.DATA_COLLAR_COLOR and Cat's, red by default.
            let collar = if kind == "minecraft:cat" { 23 } else { 21 };
            let red = i32::from(horizon_client::mobs::tame::RED);
            if i32::from(e.color & 15) != red {
                meta.push((collar, serializer::INT, Value::Int(i32::from(e.color & 15))));
            }
            defaults.push((collar, serializer::INT, Value::Int(red)));
            // Wolf.DATA_ANGER_END_TIME: the game time the anger ends, -1 when calm.
            if kind == "minecraft:wolf" {
                if e.anger > 0 {
                    let end = game_time + i64::from(e.anger);
                    meta.push((22, serializer::LONG, Value::Long(end)));
                }
                defaults.push((22, serializer::LONG, Value::Long(-1)));
            }
        }
        "minecraft:horse"
        | "minecraft:donkey"
        | "minecraft:mule"
        | "minecraft:llama"
        | "minecraft:trader_llama"
        | "minecraft:skeleton_horse"
        | "minecraft:zombie_horse"
        | "minecraft:camel" => {
            // AbstractHorse.DATA_ID_FLAGS: tame 2, eating 16 (grazing), standing
            // 32 (rearing).
            let grazes = horizon_client::mobs::equine::grazes(m.species());
            let flags = if e.tame { 2 } else { 0 }
                | if grazes && e.flag { 16 } else { 0 }
                | if grazes && e.counter > 0 { 32 } else { 0 };
            if flags != 0 {
                meta.push((18, serializer::BYTE, Value::Byte(flags)));
            }
            defaults.push((18, serializer::BYTE, Value::Byte(0)));
            if kind == "minecraft:horse" {
                let v = i32::from(e.variant) | i32::from(e.variant2) << 8;
                if v != 0 {
                    meta.push((19, serializer::INT, Value::Int(v)));
                }
            } else if kind.ends_with("llama") {
                if e.chested {
                    meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
                }
                meta.push((
                    20,
                    serializer::INT,
                    Value::Int(i32::from(e.variant2.max(1))),
                ));
                if e.variant != 0 {
                    meta.push((21, serializer::INT, Value::Int(i32::from(e.variant))));
                }
            } else if (kind == "minecraft:donkey" || kind == "minecraft:mule") && e.chested {
                meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
            }
        }
        // HappyGhast.STAYS_STILL (after AgeableMob's two and IS_LEASH_HOLDER),
        // set while `serverStillTimeout > 0` (`syncStayStillFlag`).
        "minecraft:happy_ghast" => {
            if horizon_client::mobs::happy_ghast::on_still_timeout(m) {
                meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
        }
        // Villager.DATA_VILLAGER_DATA and ZombieVillager's (after Zombie's three).
        "minecraft:villager" | "minecraft:zombie_villager" => {
            use horizon_client::mobs::render::{PROFESSIONS, VILLAGER_TYPES};
            if kind == "minecraft:villager" {
                unhappy_counter(m, meta, defaults);
            } else {
                // ZombieVillager.DATA_CONVERTING_ID: the cure's shaking.
                if horizon_client::mobs::zombie_villager::is_converting(m) {
                    meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
                }
                defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
            }
            let villager_type = VILLAGER_TYPES[e.variant as usize % VILLAGER_TYPES.len()];
            let profession = PROFESSIONS[e.variant2 as usize % PROFESSIONS.len()];
            if let (Some(t), Some(p)) = (
                id("minecraft:villager_type", villager_type),
                id("minecraft:villager_profession", profession),
            ) {
                let index = if kind == "minecraft:villager" { 19 } else { 20 };
                let level = i32::from(e.variant3.max(1));
                meta.push((
                    index,
                    serializer::VILLAGER_DATA,
                    Value::Ids(vec![t, p, level]),
                ));
            }
        }
        // AbstractCubeMob.ID_SIZE.
        "minecraft:slime" | "minecraft:magma_cube" => {
            if e.size > 1 {
                meta.push((18, serializer::INT, Value::Int(e.size)));
            }
        }
        // Creeper.DATA_SWELL_DIR, DATA_IS_POWERED and DATA_IS_IGNITED: the
        // guest swells the creeper from them. The direction always goes in,
        // as only changed entries are sent and it drops back to -1.
        "minecraft:creeper" => {
            meta.push((16, serializer::INT, Value::Int(e.counter)));
            if e.charged {
                meta.push((17, serializer::BOOLEAN, Value::Bool(true)));
            }
            if e.flag {
                meta.push((18, serializer::BOOLEAN, Value::Bool(true)));
            }
        }
        // Pillager.IS_CHARGING_CROSSBOW: the charge pose and the held
        // item's loading animation.
        "minecraft:pillager" => {
            if e.flag {
                meta.push((17, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((17, serializer::BOOLEAN, Value::Bool(false)));
        }
        // Panda: UNHAPPY_COUNTER, SNEEZE_COUNTER and EAT_COUNTER (after
        // AgeableMob's two), MAIN_GENE_ID, HIDDEN_GENE_ID and DATA_ID_FLAGS;
        // the client animates the states from them.
        "minecraft:panda" => {
            let (flags, counters) = horizon_client::mobs::panda::get(m)
                .map_or((0, [0; 3]), |s| (s.flags, [s.unhappy, s.sneeze, s.eat]));
            for (index, value) in (18..).zip(counters) {
                if value != 0 {
                    meta.push((index, serializer::INT, Value::Int(value)));
                }
                defaults.push((index, serializer::INT, Value::Int(0)));
            }
            for (index, gene) in [(21, e.variant), (22, e.variant2)] {
                if gene != 0 {
                    meta.push((index, serializer::BYTE, Value::Byte(gene as i8)));
                }
                defaults.push((index, serializer::BYTE, Value::Byte(0)));
            }
            if flags != 0 {
                meta.push((23, serializer::BYTE, Value::Byte(flags as i8)));
            }
            defaults.push((23, serializer::BYTE, Value::Byte(0)));
        }
        // PolarBear.DATA_STANDING_ID (after AgeableMob's two): the bear
        // rears up; the client animates it.
        "minecraft:polar_bear" => {
            if e.flag {
                meta.push((18, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((18, serializer::BOOLEAN, Value::Bool(false)));
        }
        // SpellcasterIllager.DATA_SPELL_CASTING_ID: the casting pose.
        "minecraft:evoker" | "minecraft:illusioner" => {
            if e.variant2 != 0 {
                meta.push((17, serializer::BYTE, Value::Byte(e.variant2 as i8)));
            }
            defaults.push((17, serializer::BYTE, Value::Byte(0)));
        }
        // Warden.CLIENT_ANGER_LEVEL: the heartbeat's pace.
        "minecraft:warden" => {
            let anger = e.warden.as_ref().map_or(0, |w| w.client_anger);
            if anger != 0 {
                meta.push((16, serializer::INT, Value::Int(anger)));
            }
            defaults.push((16, serializer::INT, Value::Int(0)));
        }
        // Sniffer.DATA_STATE (after AgeableMob's two).
        "minecraft:sniffer" => {
            if e.variant != 0 {
                meta.push((
                    18,
                    serializer::SNIFFER_STATE,
                    Value::Id(i32::from(e.variant)),
                ));
            }
            defaults.push((18, serializer::SNIFFER_STATE, Value::Id(0)));
        }
        // Shulker.DATA_ATTACH_FACE_ID, DATA_PEEK_ID and DATA_COLOR_ID (16
        // for none).
        "minecraft:shulker" => {
            if e.variant3 != 0 {
                meta.push((16, serializer::DIRECTION, Value::Id(i32::from(e.variant3))));
            }
            defaults.push((16, serializer::DIRECTION, Value::Id(0)));
            if e.counter != 0 {
                meta.push((17, serializer::BYTE, Value::Byte(e.counter as i8)));
            }
            defaults.push((17, serializer::BYTE, Value::Byte(0)));
            if e.color != 16 {
                meta.push((18, serializer::BYTE, Value::Byte(e.color as i8)));
            }
            defaults.push((18, serializer::BYTE, Value::Byte(16)));
        }
        // Vex.DATA_FLAGS_ID: charging in bit 0 (the arms and the glowing
        // texture).
        "minecraft:vex" => {
            if e.flag {
                meta.push((16, serializer::BYTE, Value::Byte(1)));
            }
            defaults.push((16, serializer::BYTE, Value::Byte(0)));
        }
        // Creaking.CAN_MOVE, IS_ACTIVE and IS_TEARING_DOWN.
        "minecraft:creaking" => {
            if e.flag2 {
                meta.push((16, serializer::BOOLEAN, Value::Bool(false)));
            }
            defaults.push((16, serializer::BOOLEAN, Value::Bool(true)));
            if e.flag {
                meta.push((17, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((17, serializer::BOOLEAN, Value::Bool(false)));
            if horizon_client::mobs::creaking::tearing_down(m) {
                meta.push((18, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((18, serializer::BOOLEAN, Value::Bool(false)));
        }
        // Allay.DATA_DANCING and DATA_CAN_DUPLICATE (the held item is its
        // main hand equipment).
        "minecraft:allay" => {
            if e.flag {
                meta.push((16, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((16, serializer::BOOLEAN, Value::Bool(false)));
            if e.egg_time > 0 {
                meta.push((17, serializer::BOOLEAN, Value::Bool(false)));
            }
            defaults.push((17, serializer::BOOLEAN, Value::Bool(true)));
        }
        // Axolotl.DATA_PLAYING_DEAD (after DATA_VARIANT).
        "minecraft:axolotl" => {
            if horizon_client::mobs::axolotl::is_playing_dead(m) {
                meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
        }
        // CopperGolem.COPPER_GOLEM_STATE (after DATA_WEATHER_STATE).
        "minecraft:copper_golem" => {
            if e.variant3 != 0 {
                meta.push((
                    17,
                    serializer::COPPER_GOLEM_STATE,
                    Value::Id(i32::from(e.variant3)),
                ));
            }
            defaults.push((17, serializer::COPPER_GOLEM_STATE, Value::Id(0)));
        }
        // Armadillo.ARMADILLO_STATE (after AgeableMob's two).
        "minecraft:armadillo" => {
            let state = horizon_client::mobs::armadillo::state(m);
            if state != 0 {
                meta.push((18, serializer::ARMADILLO_STATE, Value::Id(i32::from(state))));
            }
            defaults.push((18, serializer::ARMADILLO_STATE, Value::Id(0)));
        }
        // Frog.DATA_TONGUE_TARGET_ID (after the variant): the entity id plus
        // one, 0 for none.
        "minecraft:frog" => {
            if let Some(target) = e.frog.tongue_target {
                let id = i32::try_from(target).map_or(0, |t| t + 1);
                meta.push((19, serializer::OPTIONAL_UNSIGNED_INT, Value::Id(id)));
            }
            defaults.push((19, serializer::OPTIONAL_UNSIGNED_INT, Value::Id(0)));
        }
        // Goat.DATA_HAS_LEFT_HORN and DATA_HAS_RIGHT_HORN (after
        // DATA_IS_SCREAMING_GOAT): both true by default.
        "minecraft:goat" => {
            for (index, lost) in [(19, e.lost_horns[0]), (20, e.lost_horns[1])] {
                if lost {
                    meta.push((index, serializer::BOOLEAN, Value::Bool(false)));
                }
                defaults.push((index, serializer::BOOLEAN, Value::Bool(true)));
            }
        }
        // Enderman.DATA_CARRY_STATE and DATA_CREEPY (angry: the open mouth).
        "minecraft:enderman" => {
            if let Some(state) = e.carried {
                meta.push((
                    16,
                    serializer::OPTIONAL_BLOCK_STATE,
                    Value::Id(state.0 as i32),
                ));
            }
            if e.flag {
                meta.push((17, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((17, serializer::BOOLEAN, Value::Bool(false)));
            defaults.push((16, serializer::OPTIONAL_BLOCK_STATE, Value::Id(0)));
        }
        // Zombie.DATA_DROWNED_CONVERSION_ID (after DATA_BABY_ID and
        // DATA_SPECIAL_TYPE_ID): the shaking of the conversion under water.
        "minecraft:zombie" | "minecraft:husk" => {
            if e.counter2 > 0 {
                meta.push((18, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((18, serializer::BOOLEAN, Value::Bool(false)));
        }
        // AbstractVillager.DATA_UNHAPPY_COUNTER (after AgeableMob's two).
        "minecraft:wandering_trader" => unhappy_counter(m, meta, defaults),
        // Hoglin.DATA_IMMUNE_TO_ZOMBIFICATION (after AgeableMob's two) and
        // AbstractPiglin's, the first entry of its subclasses (the piglin's is
        // in its own arm): with the dimension it makes them shake.
        "minecraft:hoglin" | "minecraft:piglin_brute" => {
            let index = if kind == "minecraft:hoglin" { 18 } else { 16 };
            if e.flag2 {
                meta.push((index, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((index, serializer::BOOLEAN, Value::Bool(false)));
        }
        // Blaze.DATA_FLAGS_ID: charged 1, which makes it burn.
        "minecraft:blaze" => {
            if e.flag {
                meta.push((16, serializer::BYTE, Value::Byte(1)));
            }
            defaults.push((16, serializer::BYTE, Value::Byte(0)));
        }
        // Turtle.HAS_EGG and LAYING_EGG (after AgeableMob's two).
        "minecraft:turtle" => {
            if e.flag {
                meta.push((18, serializer::BOOLEAN, Value::Bool(true)));
            }
            if e.flag2 {
                meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
            }
            defaults.push((18, serializer::BOOLEAN, Value::Bool(false)));
            defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
        }
        // WitherBoss.DATA_TARGET_B and DATA_TARGET_C (after DATA_TARGET_A, the
        // main target): the entities the side heads look at, by id.
        "minecraft:wither" => {
            for (k, index) in [(0, 17), (1, 18)] {
                let target = match e.ints[k] {
                    0 => None,
                    -1 => player_entity,
                    id => Some(id),
                };
                if let Some(target) = target {
                    meta.push((index, serializer::INT, Value::Int(target)));
                }
                defaults.push((index, serializer::INT, Value::Int(0)));
            }
        }
        _ => {}
    }
    // Camel.DASH and LAST_POSE_CHANGE_TICK (after AbstractHorse's flags).
    if horizon_client::mobs::camel::is_camel(m.species()) {
        if e.flag2 {
            meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
        }
        defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
        let tick = horizon_client::mobs::camel::synced_pose_tick(m, game_time);
        if tick != 0 {
            meta.push((20, serializer::LONG, Value::Long(tick)));
        }
        defaults.push((20, serializer::LONG, Value::Long(0)));
    }
}

/// AbstractVillager.DATA_UNHAPPY_COUNTER (after AgeableMob's two): the head
/// shake of a villager saying no counts it down from 40.
fn unhappy_counter(
    m: &Mob,
    meta: &mut Vec<(u8, i32, Value)>,
    defaults: &mut Vec<(u8, i32, Value)>,
) {
    if m.extra.ints[0] > 0 {
        meta.push((18, serializer::INT, Value::Int(m.extra.ints[0])));
    }
    defaults.push((18, serializer::INT, Value::Int(0)));
}

/// What a client shakes a mob by: Entity.DATA_TICKS_FROZEN (7), whose full
/// count is `isFullyFrozen`, and Strider.DATA_SUFFOCATING (19, after
/// AgeableMob's two and DATA_BOOST_TIME).
fn mob_freeze_meta(
    m: &Mob,
    meta: &mut Vec<(u8, i32, Value)>,
    defaults: &mut Vec<(u8, i32, Value)>,
) {
    if m.ticks_frozen > 0 {
        meta.push((7, serializer::INT, Value::Int(m.ticks_frozen)));
    }
    defaults.push((7, serializer::INT, Value::Int(0)));
    if m.kind.id == "minecraft:strider" {
        if m.extra.flag {
            meta.push((19, serializer::BOOLEAN, Value::Bool(true)));
        }
        defaults.push((19, serializer::BOOLEAN, Value::Bool(false)));
    }
}

/// The equipment a mob shows (ClientboundSetEquipmentPacket slots: main hand,
/// off hand, feet, legs, chest, head, body, saddle).
fn mob_equipment(
    m: &Mob,
    hub: &mut Hub,
    out: &mut Vec<(u8, Option<horizon_protocol::item::Stack>)>,
) {
    let e = &m.extra;
    // A horse's armour, a llama's carpet, a harness: Horizon keeps the body
    // slot's item (6) in `head`.
    let body = horizon_client::mobs::drops::head_is_body(m.species());
    let slots = [
        (0u8, &e.main_hand),
        (1, &e.off_hand),
        (2, &e.feet),
        (3, &e.legs),
        (4, &e.chest),
        (if body { 6 } else { 5 }, &e.head),
    ];
    for (slot, item) in slots {
        if let Some(item) = item {
            let stack = ItemStack {
                item: item.clone(),
                count: 1,
                // A wolf's armor shows its cracks.
                damage: if slot == 6 && m.species() == horizon_client::mobs::Species::Wolf {
                    e.body_damage.max(0) as u32
                } else {
                    0
                },
                extra: if slot == 6 && m.species() == horizon_client::mobs::Species::Wolf {
                    e.body_extra.clone()
                } else {
                    None
                },
            };
            out.push((slot, hub.stack(&Some(stack))));
        }
    }
    if e.saddled {
        let stack = e.saddle_stack.clone().unwrap_or(ItemStack {
            item: Arc::from("minecraft:saddle"),
            count: 1,
            damage: 0,
            extra: None,
        });
        out.push((7, hub.stack(&Some(stack))));
    }
}

pub const HELP: &[&str] = &[
    "lan open [PORT] [commands] [force]  open the world to the network (PORT 0 picks one)",
    "lan close                      stop hosting",
    "lan status                     the hosting state and the guests",
];

impl Game<'_> {
    /// `horizon-ctl lan ...`.
    pub(super) fn lan_control(&mut self, words: &[&str]) -> Result<Option<serde_json::Value>> {
        use serde_json::json;
        if words[0] != "lan" {
            return Ok(None);
        }
        if let Some(server) = &self.integrated
            && words.get(1) == Some(&"status")
        {
            // The hub is the server thread's.
            let status = server
                .call(|g| g.lan_status())
                .ok_or("the integrated server stopped")?;
            return Ok(Some(status));
        }
        match words.get(1).copied() {
            Some("open") => {
                if self.title.is_some() {
                    return Err("open a world first".into());
                }
                let port = words.get(2).map_or(Ok(0), |p| p.parse::<u16>())?;
                self.lan_open(LanSettings {
                    port: if port == 0 {
                        lan::available_port()
                    } else {
                        port
                    },
                    guest_commands: words.contains(&"commands"),
                    force_game_mode: words.contains(&"force"),
                })?;
                Ok(Some(json!({ "ok": true, "port": self.lan_port() })))
            }
            Some("close") => {
                self.lan_close(true);
                Ok(Some(json!({ "ok": true })))
            }
            Some("status") => Ok(Some(self.lan_status())),
            _ => Err("lan open|close|status".into()),
        }
    }

    /// `horizon-ctl lan status`.
    fn lan_status(&self) -> serde_json::Value {
        use serde_json::json;
        match &self.lan {
            Some(h) if !self.serving() || self.published_port().is_some() => json!({
                "open": true,
                "host": h.host_name,
                "port": h.settings.port,
                // The server's own world: its finished chunks and how many it
                // has generated or loaded so far.
                "world": {
                    "chunks": self.world.chunks.len(),
                    "generated": self.streaming.expect_existing().generated_total,
                },
                "guests": h.hub.guests.iter().map(|g| json!({
                    "name": g.name,
                    "position": g.position,
                    "placed": g.placed,
                    "loaded": g.loaded,
                    "game_mode": g.game_mode,
                    // ItemCooldowns: the ticks the shield an axe disabled stays down.
                    "shield_cooldown": h.play.get(&g.conn).map_or(0, |p| p.shield_cooldown),
                    // The item the server has this guest using: its slot and ticks.
                    "using": h.play.get(&g.conn).and_then(|p| p.using())
                        .map(|(slot, ticks)| json!({ "slot": slot, "ticks": ticks })),
                    "sneaking": g.sneaking,
                    // The server's Elytra glide (shared flag 7).
                    "fall_flying": g.fall_flying,
                    "sleeping": g.bed,
                    "health": g.health,
                    // The server's body: its air supply and golden hearts.
                    "air": g.motion.as_ref().map(|m| m.air),
                    "absorption": g.motion.as_ref().map(|m| m.health.absorption()),
                    // FoodData: food level, saturation and exhaustion.
                    "food": [
                        f64::from(g.food),
                        f64::from(g.saturation),
                        g.motion.as_ref().map_or(0.0, |m| f64::from(m.health.food.exhaustion)),
                    ],
                    "chunks": g.sent_chunks.len(),
                    "tracked": g.tracker.len(),
                    // Whether the host draws this guest lying in a bed.
                    "model_lying": h.models.get(&g.conn).map(|m| m.extra.ints[1] != 0),
                    // The armour the host draws on this guest, head to feet.
                    "model_armor": h.models.get(&g.conn).map(|m| {
                        [&m.extra.head, &m.extra.chest, &m.extra.legs, &m.extra.feet]
                            .map(|s| s.as_deref())
                    }),
                    // The host draws this guest's arm swinging.
                    "model_swinging": h.models.get(&g.conn).map(|m| m.ai.swinging),
                    // For how many ticks the host draws this guest using an item.
                    "model_using": h.models.get(&g.conn).map(|m| m.extra.using_item),
                })).collect::<Vec<_>>(),
            }),
            _ => json!({ "open": false }),
        }
    }

    pub(super) fn lan_open(&mut self, settings: LanSettings) -> Result<()> {
        if self.is_retained_remote() {
            return Err("fixed retained client refuses LAN publication".into());
        }
        let port = if let Some(server) = &self.integrated {
            // ShareToLanScreen: IntegratedServer.publishServer.
            server
                .call(move |g| g.publish(settings))
                .ok_or("the integrated server stopped")??
        } else if self.serving() {
            return self.publish(settings).map(drop).map_err(Into::into);
        } else {
            self.lan_start(settings)?;
            self.lan.as_ref().map_or(0, |l| l.settings.port)
        };
        self.system_message(Text::translate(
            "menu.multiplayerOptions.publish.started.lan",
            &[Text::literal(port.to_string())],
        ));
        Ok(())
    }
    /// IntegratedServer.publishServer without the screen's announcement.
    pub(super) fn lan_start(&mut self, settings: LanSettings) -> Result<()> {
        self.options.mod_packages.external_network()?;
        if self.lan.is_some() || self.remote.is_some() || self.title.is_some() {
            return Err("already open".into());
        }
        // IntegratedServer.publishServer: the MOTD names the host's profile.
        let name = self.options.player_name();
        let motd = format!("{name} - {}", self.level.name);
        let host_uuid = self.level.player_uuid.map_or_else(
            || offline_uuid(&name),
            |u| {
                u128::from(u[0] as u32) << 96
                    | u128::from(u[1] as u32) << 64
                    | u128::from(u[2] as u32) << 32
                    | u128::from(u[3] as u32)
            },
        );
        let config = HubConfig {
            default_game_mode: self.level.mode(),
            guest_commands: settings.guest_commands,
            force_game_mode: settings.force_game_mode,
            hardcore: self.level.hardcore,
            view_distance: self.settings.render_distance.clamp(2, 32),
            simulation_distance: self.settings.render_distance.clamp(2, 32),
            dimension: self.dim.id().to_owned(),
            seed: self.level.seed,
            max_players: MAX_PLAYERS,
            dedicated: false,
            integrated: false,
        };
        let net = NetConfig::lan(settings.port, motd, MAX_PLAYERS);
        self.lan_host(settings, net, config, Some(host_uuid))
    }
    /// Start the network side and the play-state hub: for the LAN (with the
    /// host's own profile reserved) or for a dedicated server (no host).
    pub(super) fn lan_host(
        &mut self,
        settings: LanSettings,
        net: NetConfig,
        config: HubConfig,
        host: Option<u128>,
    ) -> Result<()> {
        let host = self.build_detached_lan_host(settings, net, config, host, Lan::start_with)?;
        self.lan = Some(host);
        Ok(())
    }

    fn build_detached_lan_host(
        &mut self,
        settings: LanSettings,
        net: NetConfig,
        config: HubConfig,
        host: Option<u128>,
        start: impl FnOnce(Arc<SyncData>, NetConfig) -> std::io::Result<Lan>,
    ) -> Result<LanHost> {
        if self.is_retained_remote() {
            return Err("fixed retained client refuses native host acquisition".into());
        }
        if net.listen {
            self.options.mod_packages.external_network()?;
        }
        let sync = sync_data(&self.options.resources)?;
        let biome_names: Vec<String> = self
            .res
            .biomes()
            .list
            .iter()
            .map(|b| b.name.clone())
            .collect();
        let registry: Vec<String> = sync
            .dynamic
            .registries
            .get("minecraft:worldgen/biome")
            .map(|v| v.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default();
        let mut encoder = ChunkEncoder::new(
            self.res
                .facts()
                .states
                .iter()
                .map(|s| (s.name.clone(), s.fluid_level > 0 || s.waterlogged)),
            &sync.builtin,
            &biome_names,
            &registry,
        );
        encoder.tables = Some(
            self.save
                .as_ref()
                .expect("current Game factory owns storage")
                .tables
                .get_or_init(|| Arc::new(horizon_client::save::SaveTables::new(&self.pack.assets)))
                .clone(),
        );
        let lan = start(sync.clone(), net)?;
        if let Some(uuid) = host {
            lan.reserve(uuid);
        }
        let host_uuid = host.unwrap_or(0);
        self.next_entity += 1;
        let host_entity = self.next_entity as i32;
        let mut hub = Hub::new(lan, sync, encoder, config);
        hub.flat_world = self.world_flat;
        let port = hub.lan.port();
        self.world.block_log = Some(Vec::new());
        self.world.light_log = Some(Default::default());
        let host = LanHost {
            hub,
            settings: LanSettings { port, ..settings },
            host_entity,
            host_swings: self.swings_started,
            host_crack: None,
            host_uuid,
            host_name: self.options.player_name(),
            models: HashMap::new(),
            host_mode: self.mode_id(),
            play: HashMap::new(),
            broken: Vec::new(),
            mob_state: HashMap::new(),
            mob_players: HashMap::new(),
            owner: None,
            waypoints: Default::default(),
            waypoint_dead: Default::default(),
            border_sent: None,
            commands: Vec::new(),
            quiet: std::collections::VecDeque::new(),
            states: Default::default(),
        };
        self.audio.capture = Some(Vec::new());
        Ok(host)
    }

    /// IntegratedServer.unpublishServer: the guests are sent away.
    pub(super) fn lan_close(&mut self, announce: bool) {
        if self.integrated.is_some() || self.serving() {
            let closed = match &self.integrated {
                Some(server) => server.call(|g| g.unpublish()).unwrap_or(false),
                None => self.unpublish(),
            };
            if closed {
                self.friends_lan_closed();
            }
            if closed && announce && self.integrated.is_some() {
                self.system_message(Text::translate(
                    "menu.multiplayerOptions.publish.stopped",
                    &[],
                ));
            }
            return;
        }
        self.friends_lan_closed();
        let Some(mut host) = self.lan.take() else {
            return;
        };
        host.award_leave_game_to_all();
        for g in &host.hub.guests {
            self.save_guest(&host, g);
        }
        host.hub.disconnect_all();
        // Give the writer threads a moment to flush the disconnect packets.
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.world.block_log = None;
        self.world.light_log = None;
        self.audio.capture = None;
        self.lan_events.clear();
        self.lan_songs.clear();
        self.lan_critical_hits.clear();
        self.lan_cushion_breaks.clear();
        self.streaming.expect_existing_mut().extra.clear();
        if announce {
            self.system_message(Text::translate(
                "menu.multiplayerOptions.publish.stopped",
                &[],
            ));
        }
    }

    /// Save a guest when they leave or the world closes: an integrated
    /// server's owner with the world (`save_game`), anyone else in their file.
    pub(super) fn save_guest(&mut self, host: &LanHost, guest: &horizon_client::lan::guest::Guest) {
        if host.owner.as_ref().is_some_and(|o| o.uuid == guest.uuid) {
            if guest.placed {
                playerdata::store(guest, &mut self.level);
            }
        } else if let Err(error) = playerdata::save(guest, &self.world_root) {
            log::warn!("player NBT record was not written: {error}");
        }
        if let Err(error) = self.save_guest_records(host, guest) {
            log::warn!("player JSON records were not written: {error}");
        }
    }
    /// The file name of a player's advancements and statistics: an
    /// integrated server's owner keeps single player's.
    fn records_uuid(&self, host: &LanHost, uuid: u128) -> Option<[i32; 4]> {
        record_uuid(
            host.owner.as_ref().map(|owner| owner.uuid),
            self.level.player_uuid,
            uuid,
        )
    }
    /// `PlayerAdvancements.save` and `ServerStatsCounter.save` of a network player.
    pub(super) fn save_guest_records(
        &self,
        host: &LanHost,
        guest: &horizon_client::lan::guest::Guest,
    ) -> Result<()> {
        save_guest_records_to(
            &self.world_root,
            &self.adv.tree,
            host.owner.as_ref().map(|owner| owner.uuid),
            self.level.player_uuid,
            guest,
            host.play.get(&guest.conn),
        )
    }
    /// `PlayerAdvancements.load` and `ServerStatsCounter.parseLocal` of a
    /// network player who joined; every statistic goes out on the first
    /// request (`PlayerList.placeNewPlayer`'s `markAllDirty`).
    fn load_guest_records(&mut self, host: &mut LanHost, conn: u32) {
        let Some(uuid) = host
            .hub
            .guest(conn)
            .and_then(|g| self.records_uuid(host, g.uuid))
        else {
            return;
        };
        let tree = self.advancement_tree();
        let path = super::stats::player_json(&self.world_root, "advancements", uuid);
        let saved = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        if let Some(g) = host.hub.guest_mut(conn)
            && let Some(json) = saved
        {
            let mut progress = horizon_client::advancements::Progress::from_json(&json);
            // PlayerAdvancements.applyFrom: advancements this server lacks are dropped.
            progress
                .criteria
                .retain(|id, _| tree.by_id.contains_key(id));
            g.advancements = progress;
            g.advancements_loaded = true;
        }
        let mut stats =
            super::stats::Stats::read(&super::stats::player_json(&self.world_root, "stats", uuid));
        stats.mark_all_dirty();
        host.play.entry(conn).or_default().stats = stats;
    }
    /// The integrated server's owner into the world's player, for `Level::write`.
    pub(super) fn store_owner(&mut self) -> Result<()> {
        let Some(host) = &self.lan else {
            return Ok(());
        };
        store_owner_records_to(
            &self.world_root,
            &mut self.level,
            host.owner.as_ref().map(|owner| owner.uuid),
            &host.hub.guests,
            &host.play,
            &self.adv.tree,
        )
    }

    /// The world's default game mode (`defaultgamemode`).
    pub(super) fn set_default_game_mode(&mut self, mode: i32) {
        self.level.game_mode = mode;
        self.level.creative = mode == 1;
        if let Some(host) = self.lan.as_mut() {
            host.hub.config.default_game_mode = mode;
        }
    }

    /// New command access and game mode forcing settings of the open world.
    pub(super) fn lan_apply_settings(&mut self, guest_commands: bool, force: bool) {
        if let Some(server) = &self.integrated {
            server.call(move |g| g.lan_apply_settings(guest_commands, force));
            return;
        }
        // An integrated server gives guests commands only where the world
        // allows them (IntegratedServer.getCustomPermissionLevel).
        let effective = guest_commands && (!self.serving() || self.allow_commands());
        if let Some(host) = self.lan.as_mut() {
            host.settings.guest_commands = guest_commands;
            host.settings.force_game_mode = force;
            host.hub.config.guest_commands = effective;
            host.hub.config.force_game_mode = force;
            host.hub.resend_commands();
        }
    }

    pub(super) fn lan_port(&self) -> Option<u16> {
        if let Some(server) = &self.integrated {
            return server.published();
        }
        if self.serving() {
            return self.published_port();
        }
        self.lan.as_ref().map(|h| h.settings.port)
    }

    /// The host as the guests see them.
    fn lan_host_state(&self, host: &LanHost) -> Option<HostState> {
        let p = self.player.as_ref()?;
        let mut equipment: [Option<ItemStack>; 6] = Default::default();
        equipment[0] = self.inventory.get(self.selected).cloned().flatten();
        equipment[1] = self.offhand.clone();
        // Feet, legs, chest, head are slots 2..=5; armor is stored head first.
        equipment[2] = self.armor[3].clone();
        equipment[3] = self.armor[2].clone();
        equipment[4] = self.armor[1].clone();
        equipment[5] = self.armor[0].clone();
        Some(HostState {
            uuid: host.host_uuid,
            name: host.host_name.clone(),
            entity_id: host.host_entity,
            position: [p.position.x, p.position.y, p.position.z],
            yaw: self.yaw,
            pitch: self.pitch,
            head_yaw: self.yaw,
            on_ground: p.ground.on_ground,
            game_mode: self.mode_id(),
            sneaking: p.pose == horizon_client::pose::Pose::Crouching,
            sprinting: p.sprinting,
            equipment,
            health: p.health.health(),
            attributes: horizon_client::lan::attributes::syncable(&p.attributes),
            // Bit 4: Riptide's spin (LivingEntity.isAutoSpinAttack).
            living_flags: living_flags(self.using)
                | if p.auto_spin_attack_ticks > 0 { 4 } else { 0 },
            bed: self.sleeping,
            body: BodyFlags {
                gliding: p.fall_flying,
                swimming: p.swimming,
                crawling: p.pose == horizon_client::pose::Pose::Swimming && !p.swimming,
                crouching: p.pose == horizon_client::pose::Pose::Crouching,
                spin_attack: p.auto_spin_attack_ticks > 0,
                on_fire: p.fire_ticks > 0,
                invisible: self.potions.effects.get("invisibility").is_some(),
                glowing: self.potions.effects.get("glowing").is_some(),
            },
        })
    }

    /// The entities guests track: mobs, items and the rest (game/lan_entities.rs).
    /// `mob_players` is the guest each mob sees as its player (game/lan_mobs.rs).
    fn lan_entities(
        &self,
        host_entity: Option<i32>,
        mob_players: &HashMap<u32, u32>,
        hub: &mut Hub,
        out: &mut States,
    ) {
        out.clear();
        for m in self.mobs.iter().filter(|m| !m.removed) {
            // The lead's holder: the player the mob sees (a guest, else the
            // host), another mob or a fence knot (game/lan_entities.rs).
            let player_entity = mob_players
                .get(&m.id)
                .and_then(|conn| hub.guest(*conn))
                .map(|g| g.entity_id)
                .or(host_entity);
            let leash = m.extra.leash.and_then(|h| match h {
                Holder::Player => player_entity,
                Holder::Mob(id) => Some(id as i32),
                Holder::Knot(p) => self.knot_ids.get(&p).map(|&id| id as i32),
            });
            let e = out.add(m.id as i32, entity_uuid(m.id), m.kind.id.as_str());
            e.position = [m.position.x, m.position.y, m.position.z];
            (e.yaw, e.pitch, e.head_yaw) = (m.yaw, m.head_pitch, m.head_yaw);
            e.on_ground = m.on_ground;
            e.velocity = [m.velocity.x, m.velocity.y, m.velocity.z];
            e.needs_sync = m.needs_sync;
            mob_meta(
                m,
                hub,
                player_entity,
                self.world.game_time,
                &mut e.meta,
                &mut e.defaults,
            );
            // HappyGhast.aiStep: requiresPrecisePosition while on a still timeout.
            e.precise = m.species() == horizon_client::mobs::Species::HappyGhast
                && horizon_client::mobs::happy_ghast::on_still_timeout(m);
            mob_freeze_meta(m, &mut e.meta, &mut e.defaults);
            mob_equipment(m, hub, &mut e.equipment);
            self.mob_attributes(m, &mut e.attributes);
            e.leash = leash;
        }
        for d in &self.dropped {
            let stack = hub.stack(&Some(d.stack.clone()));
            let e = out.add(d.id as i32, entity_uuid(d.id), "minecraft:item");
            e.position = [d.position.x, d.position.y, d.position.z];
            e.on_ground = d.on_ground;
            e.velocity = [d.velocity.x, d.velocity.y, d.velocity.z];
            // Entity.DATA_SHARED_FLAGS_ID: glowing 0x40 (the Glowing tag).
            if self.entity_glowing(d.id) {
                e.meta.push((0, serializer::BYTE, Value::Byte(0x40)));
            }
            e.meta.push((8, serializer::ITEM_STACK, Value::Item(stack)));
        }
        for o in &self.orbs {
            let e = out.add(o.id as i32, entity_uuid(o.id), "minecraft:experience_orb");
            e.position = [o.position.x, o.position.y, o.position.z];
            e.on_ground = o.on_ground;
            e.velocity = [o.velocity.x, o.velocity.y, o.velocity.z];
            // ExperienceOrb.DATA_VALUE.
            e.meta
                .push((8, serializer::INT, Value::Int(o.value as i32)));
        }
        self.lan_object_states(host_entity, hub, out);
    }

    /// Run the hub for this tick and apply what the guests did. While the
    /// single player game is `paused` only the connections run
    /// (`IntegratedServer.tickPaused`): `ServerGamePacketListenerImpl.tick`
    /// skips `tickPlayer`, so no player's air, hunger or damage moves on.
    pub(super) fn tick_lan(&mut self, paused: bool) {
        #[cfg(all(test, feature = "paired-packages"))]
        if self
            .retained_server
            .as_ref()
            .is_some_and(|s| s.test.hold_first_poll())
        {
            return;
        }
        #[cfg(feature = "paired-packages")]
        if self.retained_server_check_running().is_err() {
            return;
        }
        let Some(mut host) = self.lan.take() else {
            return;
        };
        // What the tick sends a guest goes to its connection at the end.
        let holding = lan::Holding::start();
        // The host's own commands' (and command blocks') players moving
        // to another level, and their broadcastToAdmins lines.
        if !self.level_teleports.is_empty() {
            self.run_level_teleports(&mut host);
        }
        if !self.admin_feedback.is_empty() {
            self.send_admin_feedback(&mut host, None);
        }
        let state = self.lan_host_state(&host);
        // An animal whose rider left the game (or the level) stops taking
        // its input.
        for m in &mut self.mobs {
            if m.rider.is_some()
                && self.riding != Some(m.id)
                && !host.hub.guests.iter().any(|g| g.vehicle == Some(m.id))
            {
                m.rider = None;
            }
        }
        // A dedicated server runs the hub without a host player.
        if state.is_some() || host.hub.config.dedicated {
            let has_guests = !host.hub.guests.is_empty();
            let mut states = std::mem::take(&mut host.states);
            if has_guests {
                let host_entity = state.as_ref().map(|s| s.entity_id);
                self.lan_assign_ids();
                let guests: Vec<(u32, i32)> = host
                    .hub
                    .guests
                    .iter()
                    .filter_map(|g| Some((g.vehicle?, g.entity_id)))
                    .collect();
                self.lan_riding(host_entity, &guests, &mut host.hub.riding);
                self.lan_entities(host_entity, &host.mob_players, &mut host.hub, &mut states);
                self.lan_hook_states(host_entity, &host.play, &host.hub, &mut states);
            } else {
                states.clear();
            }
            // ServerEntity.sendChanges clears needsSync once it has looked.
            for m in &mut self.mobs {
                m.needs_sync = false;
            }
            for p in &mut self.creatures.projectiles {
                p.needs_sync = false;
            }
            // `runBlockEvents`' broadcast precedes the tick's block changes
            // (`ChunkHolder.broadcastChanges` in `tick_level` below): the
            // guest re-runs a piston's `triggerEvent` while its level still
            // has the blocks as they were before the move.
            for (pos, b0, b1, state) in std::mem::take(&mut self.lan_block_events) {
                host.hub.block_event(pos, b0, b1, state);
            }
            let entities = states.as_slice();
            // An entity that left the level since the last tick (a snowball
            // that hit) is dropped by the tracker below: its event goes out
            // first, as `Level.broadcastEntityEvent` precedes the discard.
            let (gone, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.lan_entity_events)
                .into_iter()
                .partition(|(id, ..)| !entities.iter().any(|e| e.id == *id));
            self.lan_entity_events = later;
            for (id, event, at) in gone {
                host.hub.entity_event(id, event, at);
            }
            let spawn = self.spawn.map_or([0.5, 80.0, 0.5], |s| [s.x, s.y, s.z]);
            let events = {
                let clock_rate = self.network_clock_rate();
                let mut ctx = TickCtx {
                    world: &mut self.world,
                    host: state.as_ref(),
                    entities,
                    next_entity: &mut self.next_entity,
                    spawn,
                    difficulty: i32::from(self.ui.difficulty),
                    difficulty_locked: self.level.difficulty_locked,
                    clock_rate,
                    tick_rate: self.clock.rate(),
                    tick_frozen: self.clock.frozen(),
                    tick_steps: self.clock.step_state(),
                    collision: Some(self.collision),
                    border: &self.border,
                };
                host.hub.movement_checks = (
                    self.ui.rules.bool("player_movement_check"),
                    self.ui.rules.bool("elytra_movement_check"),
                );
                host.hub
                    .set_immediate_respawn(self.ui.rules.bool("immediate_respawn"));
                // A server with several levels runs a pass for each
                // (`tick_server`): the network is read once, before the first.
                let pass = self.level_pass;
                let mut events = if pass.first {
                    #[cfg(feature = "paired-packages")]
                    if self.retained_server.is_some() {
                        host.hub.poll_retained(&mut ctx, 16)
                    } else {
                        host.hub.poll(&mut ctx)
                    }
                    #[cfg(not(feature = "paired-packages"))]
                    host.hub.poll(&mut ctx)
                } else {
                    Vec::new()
                };
                #[cfg(feature = "paired-packages")]
                let retained_allowed = if let Some(server) = &mut self.retained_server {
                    server.check_hub(&host.hub);
                    server.check_inboxes(&host.hub);
                    !server.frozen()
                } else {
                    true
                };
                #[cfg(not(feature = "paired-packages"))]
                let retained_allowed = true;
                if retained_allowed {
                    events.extend(host.hub.tick_level(&mut ctx));
                }
                if pass.last {
                    host.hub.end_tick();
                }
                events
            };
            if let Some(state) = &state
                && host.host_mode != state.game_mode
            {
                host.host_mode = state.game_mode;
                host.hub.set_host_game_mode(state.uuid, state.game_mode);
            }
            #[cfg(feature = "paired-packages")]
            let events = if let Some(server) = &mut self.retained_server {
                server.pending.extend(events);
                #[cfg(test)]
                if server.test.stop_on_untouched_join
                    && server
                        .pending
                        .iter()
                        .any(|e| matches!(e, HostEvent::Joined(_)))
                {
                    server.fail("controlled untouched original Joined stop".into());
                }
                if !server.frozen() {
                    server.check_hub(&host.hub);
                }
                if server.frozen() {
                    host.states = states;
                    self.lan = Some(host);
                    return;
                }
                std::mem::take(&mut server.pending)
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                events
            };
            let mut events = events.into_iter();
            while let Some(event) = events.next() {
                #[cfg(feature = "paired-packages")]
                if let Some(server) = &mut self.retained_server {
                    server.check_hub(&host.hub);
                }
                #[cfg(feature = "paired-packages")]
                if self.retained_server.as_ref().is_some_and(|s| s.frozen()) {
                    let server = self.retained_server.as_mut().unwrap();
                    server.pending.push_back(event);
                    server.pending.extend(events);
                    break;
                }
                match event {
                    event @ (HostEvent::Chat { .. } | HostEvent::Warn(_) | HostEvent::Error(_)) => {
                        self.lan_log_event(event);
                    }
                    HostEvent::PlayerChat { sender, text } => {
                        self.mod_player_chat(super::server_mods::Who::Uuid(sender), &text);
                    }
                    HostEvent::Joined(conn) => {
                        #[cfg(feature = "paired-packages")]
                        if self.retained_server.is_some() {
                            if let Err(error) = self.install_retained_guest(&mut host, conn, spawn)
                            {
                                let server = self.retained_server.as_mut().unwrap();
                                server.fail(error.to_string());
                                server.pending.push_back(HostEvent::Joined(conn));
                                server.pending.extend(events);
                                break;
                            }
                            self.place_retained_guest(
                                &mut host,
                                conn,
                                state.as_ref(),
                                entities,
                                spawn,
                            );
                        } else {
                            self.lan_guest_joined(&mut host, conn, state.as_ref(), entities, spawn);
                        }
                        #[cfg(not(feature = "paired-packages"))]
                        self.lan_guest_joined(&mut host, conn, state.as_ref(), entities, spawn);
                        if self.options.dedicated.is_some()
                            && let Some(g) = host.hub.guest(conn)
                        {
                            // PlayerList.placeNewPlayer, then the broadcast join message.
                            super::dedicated::player_joined(g.uuid, &g.name);
                            let [x, y, z] = g.position;
                            super::dedicated::log(
                                "INFO",
                                &format!(
                                    "{}[/{}] logged in with entity id {} at ({}, {}, {})",
                                    g.name,
                                    g.address,
                                    g.entity_id,
                                    super::dedicated::java_double(x),
                                    super::dedicated::java_double(y),
                                    super::dedicated::java_double(z)
                                ),
                            );
                            super::dedicated::log(
                                "INFO",
                                &format!("System chat: {} joined the game", g.name),
                            );
                        } else if self.player.is_some()
                            && let Some(g) = host.hub.guest(conn)
                            && g.placed
                        {
                            // The host's chat hears of the newcomer as the guests do.
                            let who = self.player_text(&g.name, g.uuid);
                            self.system_message(
                                Text::translate("multiplayer.player.joined", &[who])
                                    .with_color(horizon_client::text::YELLOW),
                            );
                        }
                    }
                    HostEvent::Left(mut guest) => {
                        // The container they had open loses an opener.
                        if let Some(pos) = guest.menu.as_ref().and_then(|m| m.pos)
                            && !guest.spectator()
                        {
                            // In the level they were in.
                            let back = self.dim;
                            let theirs = super::Dim::from_id(&guest.dimension);
                            if self.enter_level(Some(&mut host), theirs).is_ok() {
                                self.container_openers_changed(pos, false);
                            }
                            if let Err(e) = self.enter_level(Some(&mut host), back) {
                                log::error!("could not reopen {}: {e}", back.id());
                            }
                        }
                        self.lan_crafting_left(&mut host, &mut guest);
                        if guest.placed {
                            host.award_leave_game(guest.conn);
                        }
                        self.save_guest(&host, &guest);
                        host.models.remove(&guest.conn);
                        host.play.remove(&guest.conn);
                        self.guest_combat.remove(&guest.conn);
                        if self.options.dedicated.is_some() && guest.placed {
                            super::dedicated::log(
                                "INFO",
                                &format!("{} lost connection: {}", guest.name, guest.left_reason),
                            );
                            super::dedicated::log(
                                "INFO",
                                &format!("System chat: {} left the game", guest.name),
                            );
                        } else if self.player.is_some() && guest.placed {
                            // ServerGamePacketListenerImpl.removePlayerFromWorld.
                            let who = self.player_text(&guest.name, guest.uuid);
                            self.system_message(
                                Text::translate("multiplayer.player.left", &[who])
                                    .with_color(horizon_client::text::YELLOW),
                            );
                        }
                    }
                    HostEvent::Packet(conn, packet) => {
                        self.lan_guest_packet(&mut host, conn, *packet);
                        if !host.commands.is_empty() {
                            self.lan = Some(host);
                            self.run_guest_commands();
                            host = self.lan.take().expect("the hub is back after the commands");
                        }
                    }
                }
            }
            host.states = states;
        }
        #[cfg(feature = "paired-packages")]
        if let Some(server) = &mut self.retained_server {
            server.check_hub(&host.hub);
            if server.frozen() {
                self.lan = Some(host);
                return;
            }
        }
        if !host.quiet.is_empty() {
            self.lan = Some(host);
            self.run_quiet_commands();
            host = self
                .lan
                .take()
                .expect("the hub is back after the quiet commands");
        }
        self.lan_flush_effects(&mut host);
        if !paused {
            self.tick_guests(&mut host);
        }
        self.lan_living_flags(&mut host);
        self.tick_server_travel(&mut host);
        self.lan_swings(&mut host);
        self.lan_update_streaming(&host);
        self.lan_update_models(&mut host);
        self.lan_update_waypoints(&mut host, state.as_ref());
        self.lan_sync_scoreboard(&mut host);
        self.lan_sync_border(&mut host);
        #[cfg(feature = "paired-packages")]
        if let Some(server) = &mut self.retained_server {
            server.check_hub(&host.hub);
            if server.frozen() {
                self.lan = Some(host);
                return;
            }
        }
        drop(holding);
        lan::flush_all(host.hub.guests.iter().map(|g| &g.sender));
        self.lan = Some(host);
        // A blast from a guest's attack, and a mace's wind burst.
        self.flush_lan_explosions();
        let _ = self.flush_wind_bursts();
        // A sweep attack's reach to the other players from a guest's attack.
        self.flush_sweeps();
        // A reeled-in hook pulling a player.
        self.flush_hook_pulls();
        // Particles a guest's turn sent (sweep attack, damage indicator, a hook's splashes).
        self.flush_lan_particles();
    }

    fn lan_guest_joined(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        state: Option<&HostState>,
        entities: &[EntityState],
        spawn: [f64; 3],
    ) {
        let default_mode = host.hub.config.default_game_mode;
        let force = host.hub.config.force_game_mode;
        let root = self.world_root.clone();
        let in_place = host.hub.config.dimension.clone();
        let owner = host.owner.as_mut();
        let Some(g) = host.hub.guest_mut(conn) else {
            return;
        };
        let saved = owner
            .filter(|o| o.uuid == g.uuid)
            .and_then(|o| o.saved.take());
        let known = match saved {
            Some(level) => {
                playerdata::apply(g, &level);
                level.position.is_some()
            }
            None => playerdata::load(g, &root),
        };
        if !known || force {
            g.game_mode = default_mode;
        }
        if !known {
            // ServerPlayer.adjustSpawnLocation: a player new to the world
            // stands where PlayerSpawnFinder puts them, not all on one spot
            // (it reads the loaded level, which holds the spawn when it is
            // the overworld).
            let feet = if self.dim == super::Dim::Overworld {
                self.server_player_spawn(spawn)
            } else {
                spawn
            };
            g.position = feet;
            g.previous_position = feet;
            g.health = 20.0;
            g.food = 20;
            g.dimension = super::Dim::Overworld.id().to_owned();
        }
        // A server places each player in their own level
        // (`PlayerList.placeNewPlayer`); the in-frame game has one.
        let back = self.dim;
        let level = super::Dim::from_id(&g.dimension);
        let elsewhere = self.options.dedicated.is_some() && level != back;
        if !elsewhere {
            g.dimension = in_place;
        }
        self.load_guest_records(host, conn);
        let mut theirs = States::default();
        let entities = if elsewhere {
            if let Err(e) = self.enter_level(Some(host), level) {
                log::error!("could not open {}: {e}", level.id());
                return;
            }
            // A dedicated server has no host player.
            self.lan_entities(None, &host.mob_players, &mut host.hub, &mut theirs);
            self.lan_hook_states(None, &host.play, &host.hub, &mut theirs);
            theirs.as_slice()
        } else {
            entities
        };
        let clock_rate = self.network_clock_rate();
        let mut ctx = TickCtx {
            world: &mut self.world,
            host: state,
            entities,
            next_entity: &mut self.next_entity,
            spawn,
            difficulty: i32::from(self.ui.difficulty),
            difficulty_locked: self.level.difficulty_locked,
            clock_rate,
            tick_rate: self.clock.rate(),
            tick_frozen: self.clock.frozen(),
            tick_steps: self.clock.step_state(),
            collision: Some(self.collision),
            border: &self.border,
        };
        host.hub.place_new_player(conn, &mut ctx);
        if elsewhere && let Err(e) = self.enter_level(Some(host), back) {
            log::error!("could not reopen {}: {e}", back.id());
        }
    }

    /// Level events the game queued, for the guests (`process_level_events`).
    pub(super) fn lan_record_events(&mut self, events: &[horizon_world::level::LevelEvent]) {
        if self.audio.capture.is_none() {
            return;
        }
        for e in events {
            match e {
                horizon_world::level::LevelEvent::LevelEvent { id, pos, data } => {
                    // The acting guest does not predict the egg's teleport
                    // particles, bone meal's growth, anvil result sounds, or a
                    // finished brushing (these levelEvent calls name no
                    // excluded player), nor the digger's own crack and hit
                    // sound (2019, 2020).
                    let except = if matches!(*id, 2015 | 1505 | 1029 | 1030 | 2019 | 2020 | 3008) {
                        None
                    } else {
                        self.lan_acting
                    };
                    self.lan_events
                        .push((*id, [pos.x, pos.y, pos.z], *data, except));
                }
                // `ServerLevel.runBlockEvents` broadcasts a handled block
                // event (`ClientboundBlockEventPacket`): the guest re-runs
                // the block's `triggerEvent` — a piston's `moveBlocks` is
                // what animates the move there. `state` is the block the
                // event named, read before it ran (the packet's `block`).
                horizon_world::level::LevelEvent::BlockEntityEvent { pos, state, a, b } => {
                    self.lan_block_events
                        .push(([pos.x, pos.y, pos.z], *a, *b, state.0));
                }
                _ => {}
            }
        }
    }

    /// `Level.blockEvent` for the guests (`ClientboundBlockEventPacket`).
    pub(super) fn lan_block_event(&mut self, pos: [i32; 3], b0: i32, b1: i32) {
        if self.audio.capture.is_some() {
            let state = self.world.block(pos[0], pos[1], pos[2]).0;
            self.lan_block_events.push((pos, b0, b1, state));
        }
    }
    /// `Level.broadcastEntityEvent` for the guests.
    pub(super) fn lan_entity_event(&mut self, id: i32, event: i8, at: [f64; 3]) {
        if self.audio.capture.is_some() {
            self.lan_entity_events.push((id, event, at));
        }
        // Event 21: a window that simulates the world itself is its own
        // client, and hears the guardian's hum too.
        if event == 21 && self.options.execution_role.is_client() {
            self.start_guardian_sound(id as u32);
        }
    }
    /// `ServerPlayer.crit` (or `magicCrit` when `magic`): this window's
    /// emitter, and an animation for the attacker's client and observers once
    /// the hub is back in place.
    pub(super) fn critical_hit(
        &mut self,
        attacker: Option<u32>,
        id: i32,
        feet: [f64; 3],
        width: f64,
        height: f64,
        magic: bool,
    ) {
        use horizon_client::particles::{ParticleOptions, ParticleType};
        let particle = if magic {
            ParticleType::EnchantedHit
        } else {
            ParticleType::Crit
        };
        self.particles
            .tracking_emitter(ParticleOptions::simple(particle), feet, width, height);
        if self.audio.capture.is_some() {
            self.lan_critical_hits.push((attacker, id, magic));
        }
    }
    /// Send the tick's level events, sounds and mob animations.
    fn lan_flush_effects(&mut self, host: &mut LanHost) {
        let critical_hits = std::mem::take(&mut self.lan_critical_hits);
        let kinetic_hits = std::mem::take(&mut self.lan_kinetic_hits);
        for (state, at) in std::mem::take(&mut self.lan_cushion_breaks) {
            if let Some(id) = host
                .hub
                .registry_id("minecraft:particle_type", "minecraft:block")
            {
                host.hub.broadcast_near(
                    at,
                    32.0,
                    &C::LevelParticles(play::LevelParticles {
                        particle: horizon_protocol::entity_data::Particle {
                            id,
                            data: Some(horizon_protocol::entity_data::ParticleData::State(
                                state.0 as i32,
                            )),
                        },
                        override_limiter: false,
                        always_show: false,
                        pos: at,
                        dist: [0.25, 0.0625, 0.25],
                        max_speed: [0.05; 3],
                        count: 10,
                        randomization: 0,
                    }),
                    None,
                );
            }
        }
        for (kind, pos, data, acting) in std::mem::take(&mut self.lan_events) {
            if kind == 2001 {
                host.broken
                    .push([f64::from(pos[0]), f64::from(pos[1]), f64::from(pos[2])]);
            }
            host.hub.level_event(kind, pos, data, acting);
        }
        // Level events 1010 and 1011: the guests' parrots dance or stop.
        for (pos, song) in std::mem::take(&mut self.lan_songs) {
            match song {
                Some(name) => {
                    let id = host.hub.registry_id("minecraft:jukebox_song", &name);
                    host.hub.level_event(1010, pos, id.unwrap_or(0), None);
                }
                None => host.hub.level_event(1011, pos, 0, None),
            }
        }
        for (pos, b0, b1, state) in std::mem::take(&mut self.lan_block_events) {
            host.hub.block_event(pos, b0, b1, state);
        }
        for (id, event, at) in std::mem::take(&mut self.lan_entity_events) {
            host.hub.entity_event(id, event, at);
        }
        for (kind, pos, data) in std::mem::take(&mut self.lan_global_events) {
            host.hub.global_level_event(kind, pos, data);
        }
        self.lan_broadcast_effects(host);
        if host.hub.guests.is_empty() {
            host.mob_state.clear();
            return;
        }
        // Hurt and death animations of mobs.
        let mut seen = HashMap::new();
        for m in &self.mobs {
            let dead = !m.alive();
            let old = host
                .mob_state
                .get(&self.dim)
                .and_then(|s| s.get(&m.id))
                .copied();
            let at = [m.position.x, m.position.y, m.position.z];
            // LivingEntity.hurtServer: a hurt that restarted the hurt time.
            if m.hurt_time > old.map_or(0, |o| o.0)
                && !dead
                && let Some((source, _)) = m.last_damage
            {
                use horizon_client::mobs::{DamageKind as K, Who};
                let player = source.attacker == Some(Who::Player);
                let kind = super::creatures::damage_type(source.kind, player);
                // The player a mob sees is the guest acting, else the host.
                let cause = source.attacker.map(|who| match who {
                    Who::Player => host
                        .mob_players
                        .get(&m.id)
                        .and_then(|conn| host.hub.guest(*conn))
                        .map_or(host.host_entity, |g| g.entity_id),
                    Who::Mob(id) => id as i32,
                });
                // A projectile is the direct entity of its hit: not tracked here.
                let melee = matches!(
                    source.kind,
                    K::PlayerAttack
                        | K::MaceSmash
                        | K::Spear
                        | K::MobAttack
                        | K::Sting
                        | K::SonicBoom
                );
                let direct = cause.filter(|_| melee);
                host.hub.damage_event(m.id as i32, kind, cause, direct, at);
            }
            if dead && !old.is_some_and(|o| o.1) {
                host.hub.entity_event(m.id as i32, 3, at);
            }
            // LivingEntity.swing on a server (a melee hit, a thrown potion):
            // ClientboundSwingAnimationPacket for the players around. A swing
            // started since the last flush, or restarted (its time went back).
            let swing = m.ai.swinging.then_some(m.ai.swing_time);
            if let Some(time) = swing
                && !dead
                && old.and_then(|o| o.2).is_none_or(|before| time < before)
            {
                host.hub.swing(m.id as i32, 0, at, None);
            }
            seen.insert(m.id, (m.hurt_time, dead, swing));
        }
        host.mob_state.insert(self.dim, seen);
        // Level.broadcastEntityEvent(player, 2) of a spear's stab.
        for attacker in kinetic_hits {
            let hit = match attacker {
                None => self
                    .player
                    .as_ref()
                    .map(|p| (host.host_entity, [p.position.x, p.position.y, p.position.z])),
                Some(conn) => host.hub.guest(conn).map(|g| (g.entity_id, g.position)),
            };
            if let Some((id, at)) = hit {
                host.hub.entity_event(id, 2, at);
            }
        }
        for (attacker, target, magic) in critical_hits {
            let Some(attacker) = attacker.map_or(Some(host.host_entity), |conn| {
                host.hub.guest(conn).map(|g| g.entity_id)
            }) else {
                continue;
            };
            host.hub.critical_hit(attacker, target, magic);
        }
    }

    /// Keep the chunks around every guest loaded.
    fn lan_update_streaming(&mut self, host: &LanHost) {
        // DistanceManager.PlayerTicketTracker: every chunk within the server's
        // view distance (Chebyshev) of a player holds a PLAYER_LOADING ticket
        // at the entity-ticking level (31), whatever the player asked for;
        // levels rise by one a chunk, so FULL (33) reaches two chunks past
        // it. An extra area finishes its chunks out to its radius + 1. That
        // covers what the player tracks (`ChunkTrackingView`, within its view
        // distance + 1) and those chunks' lit neighbours.
        let radius = host.hub.config.view_distance + 1;
        self.streaming.expect_existing_mut().extra = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g))
            .map(|g| (g.chunk(), radius))
            .collect();
    }

    /// The guests' models, for the host to see them.
    fn lan_update_models(&mut self, host: &mut LanHost) {
        let Some(kind) = self.res.player_type().clone() else {
            return;
        };
        host.models
            .retain(|conn, _| host.hub.guest(*conn).is_some_and(|g| g.placed));
        for g in host.hub.guests.iter().filter(|g| g.placed) {
            let at = Vec3::new(
                g.position[0],
                g.position[1] - if g.sneaking { 0.125 } else { 0.0 },
                g.position[2],
            );
            let m = host
                .models
                .entry(g.conn)
                .or_insert_with(|| Mob::new(g.entity_id as u32, kind.clone(), at, g.yaw));
            let (dx, dy, dz) = (
                at.x - m.position.x,
                at.y - m.position.y,
                at.z - m.position.z,
            );
            m.previous = m.position;
            m.position = at;
            if dx * dx + dy * dy + dz * dz > 100.0 {
                m.previous = at;
            }
            m.previous_yaw = m.yaw;
            m.previous_body_yaw = m.body_yaw;
            m.previous_head_yaw = m.head_yaw;
            m.previous_head_pitch = m.head_pitch;
            m.yaw = g.yaw;
            m.head_yaw = g.head_yaw;
            m.head_pitch = g.pitch;
            let mut target = m.body_yaw;
            let wrap = |a: f32| (a + 180.0).rem_euclid(360.0) - 180.0;
            if dx * dx + dz * dz > 0.002_500_000_2 {
                // LivingEntity.tick: walking backwards the body faces away
                // from the walk direction, not along it.
                let walk = (dz.atan2(dx) as f32).to_degrees() - 90.0;
                let facing = (wrap(g.yaw) - walk).abs();
                target = if 95.0 < facing && facing < 265.0 {
                    walk - 180.0
                } else {
                    walk
                };
            }
            m.body_yaw += wrap(target - m.body_yaw) * 0.3;
            let diff = wrap(g.yaw - m.body_yaw);
            if diff.abs() > 50.0 {
                m.body_yaw += diff - diff.signum() * 50.0;
            }
            let dist = (dx * dx + dz * dz).sqrt() as f32;
            m.previous_limb_speed = m.limb_speed;
            m.limb_speed += ((dist * 4.0).min(1.0) - m.limb_speed) * 0.4;
            m.limb_position += m.limb_speed;
            m.age = m.age.wrapping_add(1);
            m.extra.main_hand = g
                .inventory
                .get(g.selected)
                .and_then(|s| s.as_ref())
                .map(|s| s.item.clone());
            m.extra.off_hand = g.offhand.as_ref().map(|s| s.item.clone());
            [m.extra.head, m.extra.chest, m.extra.legs, m.extra.feet] = g
                .armor
                .each_ref()
                .map(|s| s.as_ref().map(|s| s.item.clone()));
            m.extra.crouching = g.sneaking;
            m.extra.avatar_pose = g.pose();
            m.extra.sprinting = g.sprinting;
            // Entity.DATA_POSE and the shared and living flags the others
            // get (hub::guest_body): gliding, swimming, Riptide's spin.
            let body = lan::hub::guest_body(g);
            m.extra.fall_flying = body.gliding;
            m.extra.synced_pose = if body.swimming || body.crawling { 3 } else { 0 };
            m.extra.auto_spin_attack = body.spin_attack;
            m.velocity = g
                .motion
                .as_ref()
                .map_or(Vec3::new(dx, dy, dz), |motion| motion.velocity);
            m.tick_avatar_pose(
                g.motion
                    .as_ref()
                    .is_some_and(|motion| motion.fluids.was_touching_water),
            );
            // CrossbowItem.isCharged: the held crossbow's projectiles.
            m.extra.charged = g
                .inventory
                .get(g.selected)
                .and_then(|s| s.as_ref())
                .is_some_and(|s| s.data().is_some_and(|d| !d.charged.is_empty()));
            // ClientInformation.mainHand (HumanoidArm: 0 left, 1 right).
            m.extra.main_hand_left = g.info.main_hand == 0;
            // LivingEntity.DATA_EFFECT_PARTICLES: the swirls of its visible effects.
            let visible = || g.effects.list.iter().filter(|e| e.visible);
            m.extra.effect_particles = visible()
                .filter_map(|e| super::client_ambient::effect_particle(e.id, e.ambient))
                .collect();
            m.extra.effect_ambient = visible().all(|e| e.ambient);
            // Entity.isInvisible: the invisibility effect hides the body (the
            // model pass, the name tag rules and `trader::is_invisible` read it).
            m.extra.effects.retain(|e| e.0 != "minecraft:invisibility");
            if let Some(e) = g.effects.get("invisibility") {
                m.extra
                    .effects
                    .push(("minecraft:invisibility", e.duration, e.amplifier));
            }
            // Lying in its bed (LivingEntity.getBedOrientation).
            m.extra.ints[1] = g.bed.map_or(0, |b| self.bed_facing(b));
            // LivingEntity.updateSwingTime: the arm swing lan_swings started.
            m.anim.attack_anim_o = m.anim.attack_anim;
            horizon_client::mobs::ai::tick_swing(m);
            // LivingEntity.onSyncedDataUpdated(DATA_LIVING_ENTITY_FLAGS): the
            // item in use and its hand (the arm poses of a bow, a shield...).
            let using = host.play.get(&g.conn).and_then(|p| p.using());
            m.extra.using_item = using.map_or(0, |(_, ticks)| ticks as i32 + 1);
            m.extra.using_off_hand = using.is_some_and(|(slot, _)| slot == OFFHAND_SLOT);
            // AvatarRenderer.getArmPose: the used item's BLOCK animation (an
            // item with `blocks_attacks`) raises the arm.
            m.extra.blocking = using
                .and_then(|(slot, _)| {
                    if slot == OFFHAND_SLOT {
                        m.extra.off_hand.as_ref()
                    } else {
                        m.extra.main_hand.as_ref()
                    }
                })
                .and_then(|i| self.res.items().get(i))
                .is_some_and(|i| i.blocks_attacks.is_some());
        }
    }

    /// The bed at `head` as a sleeping model lies in it (`ints[1]`,
    /// `villager::sleep_pose`): its `facing`.
    pub(super) fn bed_facing(&self, head: [i32; 3]) -> i32 {
        let state = self.world.block(head[0], head[1], head[2]);
        let facing = self
            .pack
            .assets
            .registry
            .state(state)
            .and_then(|s| s.properties.get("facing"))
            .map(String::as_str);
        horizon_client::mobs::villager::bed_facing_code(facing)
    }

    /// The guests' models for the entity pass.
    pub(super) fn guest_model_geometry(
        &self,
        camera: glam::DVec3,
        alpha: f64,
        out: &mut Vec<horizon_client::mesher::Vertex>,
    ) {
        let Some(host) = &self.lan else {
            return;
        };
        let atlas = &self.gui.atlas;
        let lookup = |name: &str| atlas.sprites.get(name).map(|r| [r.u0, r.v0, r.u1, r.v1]);
        for (conn, m) in &host.models {
            let entity_start = out.len();
            let light = self.world.light(
                m.position.x.floor() as i32,
                (m.position.y + 1.62).floor() as i32,
                m.position.z.floor() as i32,
            );
            let packed = (u32::from(light >> 4) * 16) | ((u32::from(light & 15) * 16) << 8);
            horizon_client::mobs::render::render_with(
                m,
                alpha as f32,
                camera,
                &lookup,
                packed,
                self.res.mob_data(),
                &host.hub.guest(*conn).map_or_else(
                    || self.worn_armor_layers(m),
                    |g| self.stack_armor_layers(&g.armor),
                ),
                out,
            );
            self.player_elytra_geometry(
                m,
                m.extra.chest.as_deref(),
                camera,
                alpha as f32,
                packed,
                out,
            );
            self.spin_attack_geometry(m, camera, alpha as f32, packed, out);
            self.held_item_geometry(m, camera, alpha, packed, true, out);
            // LivingEntity.isCurrentlyGlowing: the guest's Glowing effect.
            let guest = host.hub.guest(*conn);
            if m.glowing || guest.is_some_and(|g| g.effects.get("glowing").is_some()) {
                let team = guest.and_then(|g| self.score.board.players_team(&g.name));
                self.mark_outline(&out[entity_start..], super::glowing::team_color(team));
            } else {
                self.cull_entity_vertices(out, entity_start);
            }
        }
    }

    /// A chat line the host typed, for the guests.
    pub(super) fn lan_host_chat(&mut self, message: &str) {
        if let Some(host) = &self.lan {
            host.hub.chat(&host.host_name, message);
        }
    }
}

impl Game<'_> {
    /// A hub's chat and log lines: a dedicated server logs them
    /// (MinecraftServer.logChatMessage, `[Not Secure]` for unsigned or
    /// expired messages); a LAN host shows the chat.
    pub(super) fn lan_log_event(&mut self, event: HostEvent) {
        let dedicated = self.options.dedicated.is_some();
        match event {
            HostEvent::Chat { line, secure } if dedicated => {
                let tag = if secure { "" } else { "[Not Secure] " };
                super::dedicated::log("INFO", &format!("{tag}{line}"));
            }
            // Player chat (and `/say`'s or `/me`'s, which are chat messages
            // too): the host's Chat option decides, as `ChatListener` does
            // for the guests' packets. Command feedback is a system message
            // and takes the system-message permission instead.
            HostEvent::Chat { line, .. } => self.player_message(Text::literal(line)),
            HostEvent::Warn(line) if dedicated => super::dedicated::log("WARN", &line),
            HostEvent::Warn(line) => log::warn!("{line}"),
            HostEvent::Error(line) if dedicated => super::dedicated::log("ERROR", &line),
            HostEvent::Error(line) => log::error!("{line}"),
            _ => {}
        }
    }
}

/// `LivingEntity.DATA_LIVING_ENTITY_FLAGS` for an item in use: 1, and 2 when
/// it is the off hand's.
pub(super) fn living_flags(using: Option<(usize, u32)>) -> i8 {
    using.map_or(0, |(slot, _)| if slot == OFFHAND_SLOT { 3 } else { 1 })
}

#[cfg(test)]
mod player_json_tests {
    use super::*;
    use horizon_client::lan::{Joined, PlayerSender, guest::Guest};
    use horizon_protocol::packets::configuration::ClientInformation;
    use std::sync::{atomic::AtomicI32, mpsc};

    fn scratch(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-guest-json-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn guest(uuid: u128, conn: u32) -> Guest {
        let (send, _receive) = mpsc::channel();
        let mut guest = Guest::new(
            Joined {
                conn,
                uuid,
                name: format!("Guest{conn}"),
                properties: Vec::new(),
                info: ClientInformation::new(8),
                brand: "test".into(),
                sender: PlayerSender::new(send, Arc::default()),
                latency: Arc::new(AtomicI32::new(0)),
                address: "127.0.0.1:25565".parse().unwrap(),
            },
            conn as i32,
            [0.0, 64.0, 0.0],
            8,
        );
        guest.placed = true;
        guest
            .advancements
            .grant("test:goal", "criterion", "2026-10-28 12:00:00 +0000");
        guest
    }
    fn play() -> super::super::lan_actions::GuestPlay {
        let mut play = super::super::lan_actions::GuestPlay::default();
        play.stats.add("mined", "minecraft:stone", 7);
        play
    }
    fn read(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
    fn paths(world: &std::path::Path, uuid: [i32; 4]) -> [std::path::PathBuf; 2] {
        [
            super::super::stats::player_json(world, "advancements", uuid),
            super::super::stats::player_json(world, "stats", uuid),
        ]
    }

    #[test]
    fn guest_json_attempts_both_records_returns_first_error_and_retries_shared_projection() {
        let tree = super::super::advancements::json_test_tree();
        for failed in [0, 1, 2] {
            let world = scratch(&format!("attempts-{failed}"));
            let guest = guest(42, 7);
            let play = play();
            let before_progress = guest.advancements.criteria.clone();
            let before_stats = play.stats.values.clone();
            let before_events = play.stats.events.clone();
            let [adv_path, stats_path] = paths(&world, playerdata::uuid_ints(guest.uuid));
            if failed != 1 {
                std::fs::create_dir_all(&adv_path).unwrap();
            }
            if failed != 0 {
                std::fs::create_dir_all(&stats_path).unwrap();
            }
            let error =
                save_guest_records_to(&world, &tree, None, None, &guest, Some(&play)).unwrap_err();
            assert!(error.to_string().contains(if failed == 1 {
                stats_path.to_str().unwrap()
            } else {
                adv_path.to_str().unwrap()
            }));
            if failed == 0 {
                assert_eq!(
                    read(&stats_path),
                    serde_json::json!({"stats":{"minecraft:mined":{"minecraft:stone":7}},"DataVersion":horizon_world::DATA_VERSION})
                );
            } else if failed == 1 {
                assert_eq!(
                    read(&adv_path),
                    guest
                        .advancements
                        .to_json(&tree, horizon_world::DATA_VERSION)
                );
            }
            assert_eq!(guest.advancements.criteria, before_progress);
            assert_eq!(play.stats.values, before_stats);
            assert_eq!(play.stats.events, before_events);
            assert!(!play.stats.loaded); // Guest record writing does not adopt the local loaded guard.
            if failed != 1 {
                std::fs::remove_dir(&adv_path).unwrap();
            }
            if failed != 0 {
                std::fs::remove_dir(&stats_path).unwrap();
            }
            save_guest_records_to(&world, &tree, None, None, &guest, Some(&play)).unwrap();
            assert_eq!(
                read(&adv_path),
                guest
                    .advancements
                    .to_json(&tree, horizon_world::DATA_VERSION)
            );
            assert_eq!(
                read(&stats_path)["stats"]["minecraft:mined"]["minecraft:stone"],
                7
            );
            assert_eq!(guest.advancements.criteria, before_progress);
            assert_eq!(play.stats.values, before_stats);
            assert_eq!(play.stats.events, before_events);
            std::fs::remove_dir_all(world).unwrap();
        }
    }

    #[test]
    fn guest_record_identity_gates_and_missing_play_keep_existing_behavior() {
        let profile = 0x8000_0000_ffff_ffff_0123_4567_89ab_cdef_u128;
        let local = [-1, 0, i32::MIN, i32::MAX];
        assert_eq!(
            record_uuid(Some(profile), Some(local), profile),
            Some(local)
        );
        assert_eq!(record_uuid(Some(profile), None, profile), None);
        let ordinary = record_uuid(None, None, profile).unwrap();
        let world = scratch("identity");
        assert_eq!(
            paths(&world, ordinary)[0],
            world.join("players/advancements/80000000-ffff-ffff-0123-456789abcdef.json")
        );
        std::fs::remove_dir_all(world).unwrap();
        let tree = super::super::advancements::json_test_tree();
        for gate in [0, 1, 2] {
            let world = scratch("gates");
            let mut guest = guest(profile, 7);
            let play = play();
            if gate == 0 {
                guest.placed = false;
            }
            let empty_tree = horizon_client::advancements::Tree::default();
            save_guest_records_to(
                &world,
                if gate == 1 { &empty_tree } else { &tree },
                Some(profile),
                if gate == 2 { None } else { Some(local) },
                &guest,
                Some(&play),
            )
            .unwrap();
            assert!(std::fs::read_dir(&world).unwrap().next().is_none());
            std::fs::remove_dir_all(world).unwrap();
        }
        let world = scratch("missing-play");
        let guest = guest(profile, 7);
        save_guest_records_to(&world, &tree, None, None, &guest, None).unwrap();
        let [adv_path, stats_path] = paths(&world, ordinary);
        assert_eq!(
            read(&adv_path),
            guest
                .advancements
                .to_json(&tree, horizon_world::DATA_VERSION)
        );
        assert!(!stats_path.exists());
        std::fs::remove_dir_all(world).unwrap();
    }

    #[test]
    fn integrated_owner_selection_stores_level_state_and_propagates_checked_owner_json() {
        let world = scratch("owner");
        let local = [-1, 0, i32::MIN, i32::MAX];
        let tree = super::super::advancements::json_test_tree();
        let mut owner = guest(42, 7);
        owner.position = [10.5, 80.0, -3.25];
        owner.inventory[0] = Some(ItemStack::new("minecraft:stone", 3));
        let mut other = guest(43, 8);
        other.position = [99.0, 99.0, 99.0];
        let guests = vec![other, owner];
        let play = HashMap::from([(7, play())]);
        let mut level = horizon_client::save::Level {
            player_uuid: Some(local),
            ..Default::default()
        };
        let [adv_path, stats_path] = paths(&world, local);
        std::fs::create_dir_all(&adv_path).unwrap();
        let error = store_owner_records_to(&world, &mut level, Some(42), &guests, &play, &tree)
            .unwrap_err();
        assert!(error.to_string().contains(adv_path.to_str().unwrap()));
        assert_eq!(level.position, Some([10.5, 80.0, -3.25]));
        assert_eq!(level.player_uuid, Some(local));
        assert!(
            level
                .inventory
                .iter()
                .any(|(slot, id, count, _)| *slot == 0 && id == "minecraft:stone" && *count == 3)
        );
        assert_eq!(
            read(&stats_path)["stats"]["minecraft:mined"]["minecraft:stone"],
            7
        );
        for guest in &guests {
            assert!(!paths(&world, playerdata::uuid_ints(guest.uuid))[0].exists());
        }
        std::fs::remove_dir(adv_path).unwrap();
        store_owner_records_to(&world, &mut level, Some(42), &guests, &play, &tree).unwrap();
        assert_eq!(
            read(&paths(&world, local)[0]),
            guests[1]
                .advancements
                .to_json(&tree, horizon_world::DATA_VERSION)
        );
        std::fs::remove_dir_all(world).unwrap();
    }

    #[test]
    fn owner_missing_unplaced_and_empty_tree_paths_preserve_existing_selection_rules() {
        let tree = super::super::advancements::json_test_tree();
        for (owner, placed) in [(None, true), (Some(999), true), (Some(42), false)] {
            let world = scratch("owner-guard");
            let mut guest = guest(42, 7);
            guest.placed = placed;
            let mut level = horizon_client::save::Level::default();
            store_owner_records_to(
                &world,
                &mut level,
                owner,
                std::slice::from_ref(&guest),
                &HashMap::new(),
                &tree,
            )
            .unwrap();
            assert!(level.position.is_none());
            assert!(std::fs::read_dir(&world).unwrap().next().is_none());
            std::fs::remove_dir_all(world).unwrap();
        }
        let world = scratch("owner-empty-tree");
        let mut guest = guest(42, 7);
        guest.position = [7.0, 8.0, 9.0];
        let mut level = horizon_client::save::Level {
            player_uuid: Some([1, 2, 3, 4]),
            ..Default::default()
        };
        store_owner_records_to(
            &world,
            &mut level,
            Some(42),
            std::slice::from_ref(&guest),
            &HashMap::new(),
            &horizon_client::advancements::Tree::default(),
        )
        .unwrap();
        assert_eq!(level.position, Some([7.0, 8.0, 9.0]));
        assert!(std::fs::read_dir(&world).unwrap().next().is_none());
        std::fs::remove_dir_all(world).unwrap();
    }
}

/// Detached native PLAYER output for the fixed capture fixture only.
#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
#[derive(Debug)]
pub(in crate::game) struct CapturePlayerRow {
    pub(in crate::game) conn: u32,
    pub(in crate::game) profile_uuid: u128,
    pub(in crate::game) record_uuid: [i32; 4],
    pub(in crate::game) integrated_owner: bool,
    pub(in crate::game) dimension: String,
    pub(in crate::game) player: horizon_world::nbt::Tag,
    pub(in crate::game) advancements: serde_json::Value,
    pub(in crate::game) stats: serde_json::Value,
}

#[cfg(all(test, feature = "paired-packages", target_os = "linux"))]
impl ParticipantProjections {
    /// Moves the already projected values and their validated storage identity.
    pub(in crate::game) fn into_capture_rows(self) -> Vec<CapturePlayerRow> {
        self.players
            .into_iter()
            .map(|row| {
                let (record_uuid, integrated_owner) = match row.storage {
                    ParticipantStorage::IntegratedOwner { record_uuid } => (record_uuid, true),
                    ParticipantStorage::External { record_uuid } => (record_uuid, false),
                };
                CapturePlayerRow {
                    conn: row.conn,
                    profile_uuid: row.uuid,
                    record_uuid,
                    integrated_owner,
                    dimension: row.dimension,
                    player: row.player,
                    advancements: row.advancements,
                    stats: row.stats,
                }
            })
            .collect()
    }
}
