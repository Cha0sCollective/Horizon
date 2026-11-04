//! Explicit headless native Game. It owns the original worker, transport and
//! native close sequence; ordinary dedicated/integrated startup is unchanged.
use super::paired_close::{CloseOwner, CloseProfile, CloseStatus, NativeReady};
use super::world_loading::{ColdPreparation, PreparationCloseStatus, RetainedEntryIdentity};
use super::*;
use horizon_client::{
    control::{RetainedControl, RetainedControlPhase},
    lan::{
        RetainedOfflineConfig, RetainedOutcome,
        chunks::ChunkEncoder,
        hub::{
            HostEvent, Hub, HubConfig, RetainedHubCloseView, RetainedHubIdentity, RetainedHubPhase,
            RetainedHubRefusal, RetainedPairedHubRefusal,
        },
    },
};
use horizon_protocol::{connection::Pipe, packets::play::Serverbound};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

#[cfg(test)]
#[path = "retained_server_tests.rs"]
mod tests;
#[cfg(all(test, feature = "mods", target_os = "linux"))]
pub(in crate::game) use tests::OrdinaryControl;

const CLOSE_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ServerProfile {
    Dedicated,
    Integrated { owner: u128, record: [i32; 4] },
}

pub(super) struct Launch {
    root: PathBuf,
    args: Vec<String>,
}
impl Launch {
    pub(super) fn parse(args: &[String]) -> Result<Option<Self>> {
        if !args.iter().any(|a| a.starts_with("--retained-server")) {
            return Ok(None);
        }
        if !cfg!(target_os = "linux") || cfg!(feature = "controllers") || cfg!(feature = "audio") {
            return Err("retained server requires Linux without audio/controllers".into());
        }
        for name in [
            "HORIZON_SESSION_FILE",
            "HORIZON_PLAYER_NAME",
            "HORIZON_BENCHMARK",
            "HORIZON_COMMAND_HARNESS",
            "HORIZON_NET_HARNESS",
        ] {
            if std::env::var_os(name).is_some_and(|v| !v.is_empty()) {
                return Err(format!("retained server refuses {name}").into());
            }
        }
        let mut root = None;
        let mut ordinary = Vec::new();
        let mut input = args.iter();
        while let Some(arg) = input.next() {
            match arg.as_str() {
                "--retained-server" if root.is_none() => {
                    root = Some(PathBuf::from(
                        input.next().ok_or("missing fresh retained server root")?,
                    ))
                }
                "--resources" | "--control-socket" | "--timeout-seconds" => {
                    ordinary.push(arg.clone());
                    ordinary.push(
                        input
                            .next()
                            .ok_or("missing retained server option value")?
                            .clone(),
                    );
                }
                _ => return Err(format!("unsupported retained server option {arg}").into()),
            }
        }
        let root = root.ok_or("missing retained server root")?;
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(
                "retained server requires an absolute root without parent components".into(),
            );
        }
        match std::fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("retained server root must not already exist".into()),
        }
        ordinary.extend(["--in-frame".into(), "--test-terrain".into()]);
        Ok(Some(Self {
            root,
            args: ordinary,
        }))
    }
    pub(super) fn ordinary_args(&self) -> Vec<String> {
        self.args.clone()
    }
    pub(super) fn configure(&self, options: &mut Options) {
        options.execution_role = execution_role::ExecutionRole::DedicatedServer;
        options.server_thread = false;
        options.test_terrain = true;
        options.world = Some("retained-server".into());
        options.distance = 2;
        options.distance_override = Some(2);
        options.player_name = Some("RetainedServer".into());
        options
            .control
            .get_or_insert_with(|| self.root.join("control.sock"));
    }
}

/// Issued only after actual native installation on this cold-constructed Game.
/// Native enter/leave_guest moves/reboxes the body without creating a new
/// logical installation. Unsupported replacement transitions are fenced first.
struct NativeInstallation {
    construction: Arc<()>,
    uuid: u128,
    initial_placement: Option<(i32, [f64; 3])>,
}
pub(super) struct ServerGameState {
    profile: ServerProfile,
    startup: Option<Arc<mod_packages::FrozenHostInputs>>,
    construction: Arc<()>,
    identity: Option<RetainedHubIdentity>,
    hub_refusal: Option<Box<RetainedHubRefusal>>,
    paired_hub_refusal: Option<Box<RetainedPairedHubRefusal>>,
    checkpoint_roster: Option<super::lan::ParticipantRoster>,
    continuation_archive: Option<PathBuf>,
    frozen: bool,
    failure: Option<String>,
    pub(super) pending: VecDeque<HostEvent>,
    pub(super) admissions: HashMap<u32, super::lan::NativeAdmission>,
    installations: HashMap<u32, NativeInstallation>,
    unsent_pipes: Vec<Pipe>,
    #[cfg(test)]
    pub(super) test: tests::Startup,
    #[cfg(test)]
    pub(super) consume_refusal_checked: bool,
}
impl ServerGameState {
    pub(super) fn from_cold(profile: ServerProfile) -> Self {
        Self {
            profile,
            startup: None,
            construction: Arc::new(()),
            identity: None,
            hub_refusal: None,
            paired_hub_refusal: None,
            checkpoint_roster: None,
            continuation_archive: None,
            frozen: false,
            failure: None,
            pending: VecDeque::new(),
            admissions: HashMap::new(),
            installations: HashMap::new(),
            unsent_pipes: Vec::new(),
            #[cfg(test)]
            test: tests::Startup::default(),
            #[cfg(test)]
            consume_refusal_checked: false,
        }
    }
    pub(super) fn can_bind_hub(&self) -> bool {
        self.identity.is_none()
            && self.hub_refusal.is_none()
            && self.paired_hub_refusal.is_none()
            && !self.frozen
    }
    pub(super) fn from_ordinary(
        profile: ServerProfile,
        startup: Arc<mod_packages::FrozenHostInputs>,
    ) -> Self {
        let mut state = Self::from_cold(profile);
        state.startup = Some(startup);
        state
    }
    pub(super) fn startup(&self) -> Option<&Arc<mod_packages::FrozenHostInputs>> {
        self.startup.as_ref()
    }
    pub(super) fn checkpoint_roster(&self) -> Option<&super::lan::ParticipantRoster> {
        self.checkpoint_roster.as_ref()
    }
    pub(in crate::game) fn set_continuation_archive(&mut self, archive: PathBuf) {
        self.continuation_archive = Some(archive);
    }
    pub(super) fn hold_paired_hub_refusal(&mut self, input: Box<RetainedPairedHubRefusal>) {
        self.paired_hub_refusal = Some(input);
    }
    pub(super) fn bind_hub(&mut self, identity: RetainedHubIdentity) {
        assert!(
            self.can_bind_hub(),
            "original cold server binds its Hub once"
        );
        self.identity = Some(identity);
    }
    pub(super) fn hold_hub_refusal(&mut self, input: Box<RetainedHubRefusal>) {
        self.hub_refusal = Some(input);
    }
    pub(super) fn hub_identity(&self) -> Option<&RetainedHubIdentity> {
        self.identity.as_ref()
    }
    pub(super) fn frozen(&self) -> bool {
        self.frozen
    }
    pub(super) fn fail(&mut self, detail: String) {
        self.failure.get_or_insert(detail);
        self.frozen = true;
    }
    pub(super) fn freeze(&mut self) {
        self.frozen = true;
    }
    pub(super) fn check_hub(&mut self, hub: &Hub) {
        if let ServerProfile::Integrated { owner, .. } = self.profile
            && hub.guests.iter().any(|guest| guest.uuid != owner)
        {
            self.fail("integrated Memory connection has a different owner profile".into());
            return;
        }
        if !self
            .identity
            .as_ref()
            .is_some_and(|identity| identity.matches(hub))
        {
            self.fail("original server Hub identity changed".into());
            return;
        }
        if let Some(view) = hub.retained_close_view() {
            if let Some(failure) = view.failure {
                self.fail(failure);
            }
            if view.phase != RetainedHubPhase::Open {
                self.freeze();
            }
        } else {
            self.fail("original retained Hub missing".into());
        }
    }
    pub(super) fn check_inboxes(&mut self, hub: &Hub) {
        if hub.guests.iter().any(|guest| {
            guest.inbox.iter().any(|packet| {
                !supported_packet(packet)
                    && !(self.startup.is_some() && ordinary_stone_candidate(packet))
            })
        }) {
            self.fail(
                "fixed retained server refuses an unsupported play transition before dispatch"
                    .into(),
            );
        }
    }
    pub(super) fn record_installation(&mut self, conn: u32, uuid: u128) {
        assert!(
            !self.installations.contains_key(&conn),
            "native installation is single-use"
        );
        self.installations.insert(
            conn,
            NativeInstallation {
                construction: self.construction.clone(),
                uuid,
                initial_placement: None,
            },
        );
    }
    pub(super) fn native_installed(&self, conn: u32, uuid: u128) -> bool {
        self.installations.get(&conn).is_some_and(|receipt| {
            receipt.uuid == uuid && Arc::ptr_eq(&receipt.construction, &self.construction)
        })
    }
    pub(super) fn record_initial_native_placement(
        &mut self,
        guest: &horizon_client::lan::guest::Guest,
    ) {
        // The original transport may terminate between admission and placement.
        // A refused native placement issues no receipt and retains that Guest.
        if !guest.placed || guest.awaiting_teleport.is_none() {
            return;
        }
        let original = self
            .installations
            .get_mut(&guest.conn)
            .expect("actual installed native player");
        assert!(
            original.uuid == guest.uuid && original.initial_placement.is_none(),
            "original initial placement is one-shot"
        );
        original.initial_placement = Some(
            guest
                .awaiting_teleport
                .expect("native initial placement teleport"),
        );
    }
    pub(super) fn retains_initial_native_custody(
        &self,
        guest: &horizon_client::lan::guest::Guest,
    ) -> bool {
        if self.startup.is_none() || !self.frozen || !self.native_installed(guest.conn, guest.uuid)
        {
            return false;
        }
        let original = &self.installations[&guest.conn];
        if !guest.placed {
            return guest.awaiting_teleport.is_none() && original.initial_placement.is_none();
        }
        original.initial_placement.is_some_and(|(id, at)| {
            guest.position == at
                && (guest.awaiting_teleport == Some((id, at))
                    || (!guest.loaded && guest.awaiting_teleport.is_none()))
        })
    }
    pub(super) fn is_untouched(&self, conn: u32) -> bool {
        !self.admissions.contains_key(&conn)
            && !self.installations.contains_key(&conn)
            && self
                .pending
                .iter()
                .any(|event| matches!(event, HostEvent::Joined(joined) if *joined == conn))
    }
    fn incomplete(&self) -> bool {
        self.admissions
            .values()
            .any(|admission| !admission.installed())
    }
    pub(super) fn integrated_owner(&self) -> Option<(u128, [i32; 4])> {
        match self.profile {
            ServerProfile::Integrated { owner, record } => Some((owner, record)),
            ServerProfile::Dedicated => None,
        }
    }
    pub(super) fn profile(&self) -> ServerProfile {
        self.profile
    }
    pub(super) fn authorizes_close(&self, game: &Game<'_>) -> bool {
        let profile_matches = match self.profile {
            ServerProfile::Dedicated => {
                game.options.execution_role == execution_role::ExecutionRole::DedicatedServer
                    && game
                        .lan
                        .as_ref()
                        .is_none_or(|host| host.owner.is_none() && !host.hub.config.integrated)
            }
            ServerProfile::Integrated { owner, record } => {
                game.options.execution_role == execution_role::ExecutionRole::IntegratedServer
                    && game.options.dedicated.as_ref().is_some_and(Vec::is_empty)
                    && game.level.player_uuid == Some(record)
                    && game.lan.as_ref().is_none_or(|host| {
                        host.owner.as_ref().is_some_and(|saved| {
                            saved.uuid == owner
                                && saved
                                    .saved
                                    .as_ref()
                                    .is_none_or(|level| level.player_uuid == Some(record))
                        }) && host.hub.singleplayer_owner == Some(owner)
                            && host.hub.config.integrated
                    })
            }
        };
        let startup_matches = match &self.startup {
            Some(original) => {
                game.ordinary_startup
                    .as_ref()
                    .is_some_and(|startup| Arc::ptr_eq(original, startup))
                    && original
                        .retains_startup(&game.options.mod_packages, game.pack.assets.registry)
            }
            None => true,
        };
        self.frozen
            && !self.incomplete()
            && profile_matches
            && startup_matches
            && (self.startup.is_some()
                || (game.settings.render_distance == 2 && game.settings.simulation_distance == 2))
            && game.settings.horizon.lod_distance == 0
            && game.player.is_none()
            && game.levels.parked_dims().is_empty()
            && game.lod.is_none()
            && game.renderer.is_none()
            && game.window.is_none()
            && match (&self.identity, &game.lan) {
                (Some(identity), Some(host)) => {
                    identity.matches(&host.hub)
                        && host.hub.retained_close_view().is_some_and(|view| {
                            view.phase == RetainedHubPhase::Ready && view.transport.quiescent
                        })
                }
                (None, None) => true,
                _ => false,
            }
    }
}

fn supported_packet(packet: &Serverbound) -> bool {
    use Serverbound as S;
    match packet {
        S::AcceptTeleportation(_)
        | S::PlayerLoaded(_)
        | S::ChunkBatchReceived(_)
        | S::ClientInformation(_)
        | S::MovePlayerPos(_)
        | S::MovePlayerPosRot(_)
        | S::MovePlayerRot(_)
        | S::MovePlayerStatus(_)
        | S::PlayerInput(_)
        | S::PlayerAbilities(_)
        | S::ClientTickEnd(_)
        | S::KeepAlive(_)
        | S::RecipeBookChangeSettings(_) => true,
        S::PlayerCommand(command) => matches!(command.action, 1 | 2),
        S::ClientCommand(command) => command.action == 1,
        _ => false,
    }
}

fn ordinary_stone_candidate(packet: &Serverbound) -> bool {
    match packet {
        Serverbound::SetCarriedItem(packet) => (0..9).contains(&packet.slot),
        Serverbound::SetCreativeModeSlot(packet) => {
            (9..=44).contains(&packet.slot)
                && packet.item.as_ref().is_none_or(|stack| {
                    (1..=64).contains(&stack.count)
                        && stack.added.is_empty()
                        && stack.removed.is_empty()
                })
        }
        Serverbound::UseItemOn(packet) => {
            packet.hand == 0
                && (0..6).contains(&packet.face)
                && packet.sequence >= 0
                && !packet.inside
                && !packet.world_border
                && packet
                    .cursor
                    .iter()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        }
        _ => false,
    }
}

impl Game<'_> {
    /// Actual published native population, never a UI observation or close right.
    fn ordinary_owned_population_ready(&self) -> bool {
        self.session_lock
            .as_ref()
            .and_then(|session| session.owned())
            .is_some()
            && match self.retained_server.as_ref() {
                Some(server) => !server.frozen() && server.checkpoint_roster.is_some(),
                None => {
                    self.local_checkpoint_owner()
                        && self
                            .native_close_authority
                            .as_ref()
                            .is_some_and(|original| original.retains_ordinary_source(self))
                        && self.player.as_ref().is_some_and(|body| !body.server_copy)
                        && !self.restore_player_state
                        && !self.retained_entry_closing
                }
            }
    }
    pub(super) fn arm_ordinary_owned_counter(&mut self) -> Result<bool> {
        if !self.ordinary_owned_population_ready() {
            return Ok(false);
        }
        let binding = world_loading::owned_guest_binding(
            &self.options.mod_packages,
            self.pack.assets.registry,
        )?;
        self.arm_registered_guest_placement(binding.type_id())?;
        Ok(true)
    }
    /// Frontend metadata only. Authoritative admission remains a separate check.
    pub(super) fn ordinary_counter_stack(&self, stack: &ItemStack) -> bool {
        let Some(startup) = self.ordinary_startup.as_deref().or_else(|| {
            self.retained_server
                .as_ref()
                .and_then(|server| server.startup.as_deref())
        }) else {
            return false;
        };
        ordinary_counter_item(
            &self.res,
            &self.options.mod_packages,
            self.pack.assets.registry,
            startup,
        )
        .is_ok_and(|(binding, _)| {
            stack.item.as_ref() == binding.physical_descriptor().name
                && stack.damage == 0
                && stack.extra.is_none()
                && self
                    .res
                    .items()
                    .get(&stack.item)
                    .is_some_and(|info| stack.count > 0 && stack.count <= info.max_stack.min(64))
        })
    }
    pub(super) fn ordinary_counter_use_ready(&self) -> bool {
        self.ordinary_owned_population_ready() && self.world.has_registered_guest_placement_policy()
    }
    /// Publish the fixed actual native population once. The retained opaque
    /// roster is monotonic: losing a player can block close, never make an OWN
    /// world eligible for original-new native cancellation again.
    fn publish_ordinary_owned_roster(&mut self) -> std::result::Result<bool, String> {
        let Some(owned) = self
            .session_lock
            .as_ref()
            .and_then(|session| session.owned())
        else {
            return Ok(true);
        };
        let server = self
            .retained_server
            .as_ref()
            .ok_or("OWN requires its actual native server")?;
        if server.startup.is_none()
            || server.continuation_archive.as_deref() != Some(owned.archive())
        {
            return Err("OWN server lost its actual startup/archive route".into());
        }
        if let Some(roster) = &server.checkpoint_roster {
            self.preflight_checkpoint_participants(roster)
                .map_err(|error| format!("published OWN native population changed: {error:?}"))?;
            return self
                .arm_ordinary_owned_counter()
                .map_err(|error| error.to_string());
        }
        let expected = if server.integrated_owner().is_some() {
            1
        } else {
            2
        };
        let host = self.lan.as_ref().ok_or("OWN original Hub absent")?;
        if host.hub.guests.len() > expected {
            return Err("OWN has extra actual native participants".into());
        }
        let identity = server
            .hub_identity()
            .ok_or("OWN original Hub identity absent")?;
        if host.hub.guests.len() < expected
            || host.hub.guests.iter().any(|guest| {
                !identity.matches_configured_guest(&host.hub, guest.conn, guest.uuid)
                    || !server.native_installed(guest.conn, guest.uuid)
                    || !guest.placed
                    || !guest.loaded
                    || guest.motion.is_none()
                    || guest.awaiting_teleport.is_some()
                    || !guest.left_reason.is_empty()
            })
        {
            return Ok(false);
        }
        let roster = match self.observe_checkpoint_participants() {
            Ok(roster) => roster,
            Err(_) => return Ok(false),
        };
        if self.preflight_checkpoint_participants(&roster).is_err() {
            return Ok(false);
        }
        let binding = world_loading::owned_guest_binding(
            &self.options.mod_packages,
            self.pack.assets.registry,
        )
        .map_err(|error| error.to_string())?;
        owned
            .check_binding(&binding)
            .map_err(|error| error.to_string())?;
        if self.world.counter_domain_snapshot().is_none() {
            self.session_lock
                .as_ref()
                .unwrap()
                .validate_owned_unplaced(&self.world, &binding)
                .map_err(|error| error.to_string())?;
        }
        self.retained_server.as_mut().unwrap().checkpoint_roster = Some(roster);
        self.arm_ordinary_owned_counter()
            .map_err(|error| error.to_string())
    }
    /// Only the actual never-editable NEW constructor may use the existing
    /// native-only cancellation close. This neither publishes a complete image
    /// nor reconstructs an unused admission for a later launch.
    fn preflight_new_owned_cancellation(&self) -> std::result::Result<(), String> {
        let session = self
            .session_lock
            .as_ref()
            .ok_or("new OWN native session absent")?;
        let owned = session
            .original_new_owned(&self.world_root)
            .ok_or("OWN cancellation is not the actual original NEW constructor")?;
        let server = self
            .retained_server
            .as_ref()
            .ok_or("OWN native server absent")?;
        if server.startup.is_none()
            || server.checkpoint_roster.is_some()
            || server.continuation_archive.as_deref() != Some(owned.archive())
            || !self
                .native_close_authority
                .as_ref()
                .is_some_and(|authority| {
                    authority.is_server() && authority.retains_ordinary_source(self)
                })
        {
            return Err(
                "OWN native cancellation cannot replace published participant custody".into(),
            );
        }
        let binding = world_loading::owned_guest_binding(
            &self.options.mod_packages,
            self.pack.assets.registry,
        )
        .map_err(|error| error.to_string())?;
        session
            .validate_owned_unplaced(&self.world, &binding)
            .map_err(|error| error.to_string())?;
        self.inspect_session_checkpoint_owners()
            .map_err(|error| format!("OWN cancellation item owners: {error:?}"))?;
        Ok(())
    }
    pub(super) fn freeze_ordinary_checkpoint_participants(
        &mut self,
    ) -> std::result::Result<(), String> {
        let owned = self
            .session_lock
            .as_ref()
            .and_then(|session| session.owned())
            .is_some();
        if self.world.counter_domain_snapshot().is_none() && !owned {
            return Ok(());
        }
        if owned
            && self
                .retained_server
                .as_ref()
                .is_some_and(|server| server.checkpoint_roster.is_none())
            && self
                .session_lock
                .as_ref()
                .is_some_and(|session| session.original_new_owned(&self.world_root).is_some())
        {
            self.preflight_new_owned_cancellation()?;
            return Ok(());
        }
        let server = self
            .retained_server
            .as_ref()
            .ok_or("ordinary domain requires actual authoritative retained server")?;
        if server.startup.is_none() || server.continuation_archive.is_none() {
            return Err("ordinary persistent domain lacks its captured continuation route".into());
        }
        if owned {
            // Published recovery already owns the complete checked population,
            // even before its normal initial placement acknowledgments. Frozen
            // original-native preflight can close those exact installed bodies;
            // it cannot invent an absent or partially installed participant.
            let expected = if server.integrated_owner().is_some() {
                1
            } else {
                2
            };
            let host = self.lan.as_ref().ok_or("OWN original population absent")?;
            if host.hub.guests.len() != expected
                || host.hub.guests.iter().any(|guest| {
                    !server.native_installed(guest.conn, guest.uuid) || guest.motion.is_none()
                })
            {
                return Err(
                    "OWN coherent close requires its complete original native population".into(),
                );
            }
        }
        if let Some(original) = server.checkpoint_roster.as_ref() {
            // Retry may finish the original admission; it cannot replace a
            // census already captured from different owners.
            return self
                .preflight_checkpoint_participants(original)
                .map_err(|error| {
                    format!("original retained participant census changed: {error:?}")
                });
        }
        let roster = self
            .observe_checkpoint_participants()
            .map_err(|error| format!("original retained participant census: {error:?}"))?;
        self.preflight_checkpoint_participants(&roster)
            .map_err(|error| format!("original retained participant idle custody: {error:?}"))?;
        self.retained_server.as_mut().unwrap().checkpoint_roster = Some(roster);
        Ok(())
    }
    pub(super) fn preflight_ordinary_coherent_owner(&self) -> std::result::Result<(), String> {
        let server = self
            .retained_server
            .as_ref()
            .ok_or("original ordinary server missing")?;
        let roster = server
            .checkpoint_roster
            .as_ref()
            .ok_or("original frozen participant census absent")?;
        if server.startup.is_none()
            || !server.frozen()
            || server.continuation_archive.is_none()
            || (self.world.counter_domain_snapshot().is_none()
                && self
                    .session_lock
                    .as_ref()
                    .and_then(|session| session.owned())
                    .is_none())
            || !self
                .native_close_authority
                .as_ref()
                .is_some_and(|authority| authority.is_server() && authority.authorizes(self))
        {
            return Err("ordinary coherent close lacks original native constructor/session/participant custody".into());
        }
        self.preflight_checkpoint_participants(roster)
            .map_err(|error| format!("original coherent participant custody changed: {error:?}"))
    }
    pub(super) fn ordinary_server_close_profile(
        &self,
    ) -> std::result::Result<CloseProfile, String> {
        if let Some(owned) = self
            .session_lock
            .as_ref()
            .and_then(|session| session.owned())
        {
            let server = self
                .retained_server
                .as_ref()
                .ok_or("OWN native server absent")?;
            if server.checkpoint_roster.is_none() {
                self.preflight_new_owned_cancellation()?;
                return Ok(CloseProfile::NativeWorld);
            }
            if server.continuation_archive.as_deref() != Some(owned.archive()) {
                return Err("OWN cannot change or lose its bound coherent archive".into());
            }
            return Ok(CloseProfile::CoherentSession {
                archive: owned.archive().to_owned(),
            });
        }
        if self.world.counter_domain_snapshot().is_some() {
            let archive = self
                .retained_server
                .as_ref()
                .and_then(|server| server.continuation_archive.clone())
                .ok_or("ordinary domain cannot fall back to native-only close")?;
            return Ok(CloseProfile::CoherentSession { archive });
        }
        if self.world.counter_trial_snapshot().is_some() || self.world.has_world_slot_trial() {
            return Err(
                "ordinary close refuses unsupported transient item/domain ownership".into(),
            );
        }
        Ok(CloseProfile::NativeWorld)
    }
    pub(super) fn ordinary_plain_stone(&self, stack: &ItemStack) -> bool {
        &*stack.item == "minecraft:stone"
            && stack.damage == 0
            && stack.extra.is_none()
            && self.res.items().get("minecraft:stone").is_some_and(|info| {
                stack.count > 0
                    && stack.count <= info.max_stack.min(64)
                    && info.block.is_some()
                    && info.block == self.pack.assets.registry.default_state("minecraft:stone")
            })
    }
    fn ordinary_wire_stack(
        &self,
        host: &super::lan::LanHost,
        wire: &horizon_protocol::item::Stack,
    ) -> std::result::Result<ItemStack, String> {
        if !(1..=64).contains(&wire.count) || !wire.added.is_empty() || !wire.removed.is_empty() {
            return Err("ordinary item requires plain native components/count".into());
        }
        let decoded = host.hub.stack_from_network(wire);
        if (!self.ordinary_plain_stone(&decoded) && !self.ordinary_counter_stack(&decoded))
            || decoded.count != wire.count as u32
            || host.hub.registry_id("minecraft:item", &decoded.item) != Some(wire.item)
            || host.hub.registry_name("minecraft:item", wire.item) != Some(&*decoded.item)
        {
            return Err("ordinary item native identity/count differs".into());
        }
        let encoded = horizon_client::lan::guest::stack_to_network(
            |name| host.hub.registry_id("minecraft:item", name).unwrap_or(-1),
            |kind, name| host.hub.registry_id(kind, name),
            &Some(decoded.clone()),
        );
        if encoded.as_ref() != Some(wire) {
            return Err("ordinary native item roundtrip differs".into());
        }
        Ok(decoded)
    }
    pub(super) fn ordinary_stone_use_admitted(&self) -> bool {
        if self
            .session_lock
            .as_ref()
            .and_then(|session| session.owned())
            .is_some()
            && !self.ordinary_owned_population_ready()
        {
            return false;
        }
        if !self.creative
            || self.hand != Hand::Main
            || !self.held().is_some_and(|stack| {
                self.ordinary_plain_stone(stack)
                    || (self.ordinary_counter_stack(stack)
                        && (self.remote.is_some() || self.ordinary_counter_use_ready()))
            })
        {
            return false;
        }
        let Some(hit) = self.pick() else {
            return false;
        };
        let face = match hit.face {
            [0, -1, 0] => 0,
            [0, 1, 0] => 1,
            [0, 0, -1] => 2,
            [0, 0, 1] => 3,
            [-1, 0, 0] => 4,
            [1, 0, 0] => 5,
            _ => return false,
        };
        self.eye_position().is_some_and(|eye| {
            horizon_client::lan::guest::within_block_range(eye, self.reach(), hit.pos)
        }) && self.validate_ordinary_stone_target(hit.pos, face).is_ok()
    }
    /// Called on the ORIGINAL event immediately before its native handler. The
    /// Hub has already applied preceding movement, and earlier native events
    /// have applied hotbar/slot edits, so validation uses current player state.
    pub(super) fn validate_ordinary_stone_packet(
        &self,
        host: &super::lan::LanHost,
        conn: u32,
        packet: &Serverbound,
    ) -> std::result::Result<(), String> {
        let Some(server) = self.retained_server.as_ref() else {
            return Ok(());
        };
        if server.startup.is_none() || supported_packet(packet) {
            return Ok(());
        }
        let guest = host.hub.guest(conn).ok_or("original guest absent")?;
        let identity = server
            .hub_identity()
            .ok_or("original paired Hub association absent")?;
        if !ordinary_stone_candidate(packet)
            || server.frozen()
            || !server.native_installed(conn, guest.uuid)
            || !identity.matches_configured_guest(&host.hub, conn, guest.uuid)
            || !guest.placed
            || !guest.loaded
            || guest.motion.is_none()
            || !guest.creative()
            || guest.dead
            || guest.awaiting_teleport.is_some()
            || guest.dimension != Dim::Overworld.id()
            || self.dim != Dim::Overworld
            || guest.selected >= 9
            || guest.inventory.len() != 36
            || guest.menu.is_some()
            || guest.carried.is_some()
            || guest.vehicle.is_some()
        {
            return Err(
                "ordinary stone operation lacks its live configured native creative owner".into(),
            );
        }
        match packet {
            Serverbound::SetCarriedItem(_) => Ok(()),
            Serverbound::SetCreativeModeSlot(packet) => {
                if self
                    .session_lock
                    .as_ref()
                    .and_then(|session| session.owned())
                    .is_some()
                    && server.checkpoint_roster.is_none()
                {
                    return Err(
                        "OWN item creation waits for its complete configured native population"
                            .into(),
                    );
                }
                let decoded = packet
                    .item
                    .as_ref()
                    .map(|wire| self.ordinary_wire_stack(host, wire))
                    .transpose()?;
                let current = guest.menu_slot(packet.slot as usize);
                if current
                    .as_ref()
                    .is_some_and(|stack| self.ordinary_counter_stack(stack))
                {
                    return if (36..=44).contains(&packet.slot)
                        && self.ordinary_counter_use_ready()
                        && decoded.as_ref() == current.as_ref()
                    {
                        Ok(())
                    } else {
                        Err("ordinary counter stack cannot be cleared or replaced".into())
                    };
                }
                if current
                    .as_ref()
                    .is_some_and(|stack| !self.ordinary_plain_stone(stack))
                {
                    return Err(
                        "ordinary creative edit would replace an unsupported native item owner"
                            .into(),
                    );
                }
                if let Some(stack) = &decoded
                    && self.ordinary_counter_stack(stack)
                    && (!(36..=44).contains(&packet.slot) || !self.ordinary_counter_use_ready())
                {
                    return Err("ordinary counter creation requires an armed OWN hotbar".into());
                }
                Ok(())
            }
            Serverbound::UseItemOn(packet) => {
                if self
                    .session_lock
                    .as_ref()
                    .and_then(|session| session.owned())
                    .is_some()
                    && server.checkpoint_roster.is_none()
                {
                    return Err(
                        "OWN placement waits for its complete configured native population".into(),
                    );
                }
                if !guest.inventory[guest.selected]
                    .as_ref()
                    .is_some_and(|stack| {
                        self.ordinary_plain_stone(stack)
                            || (self.ordinary_counter_stack(stack)
                                && self.ordinary_counter_use_ready())
                    })
                    || !guest.within_block_range(packet.pos, 0.0)
                {
                    return Err("ordinary placement requires native stone in the actual main hand within native reach".into());
                }
                self.validate_ordinary_stone_target(packet.pos, packet.face)
            }
            _ => Err(
                "unsupported original ordinary play packet retained before native dispatch".into(),
            ),
        }
    }
    pub(super) fn validate_ordinary_stone_target(
        &self,
        pos: [i32; 3],
        face: i32,
    ) -> std::result::Result<(), String> {
        let direction = match face {
            0 => [0, -1, 0],
            1 => [0, 1, 0],
            2 => [0, 0, -1],
            3 => [0, 0, 1],
            4 => [-1, 0, 0],
            5 => [1, 0, 0],
            _ => return Err("unsupported stone face".into()),
        };
        let mut destination = pos;
        for axis in 0..3 {
            destination[axis] = pos[axis]
                .checked_add(direction[axis])
                .ok_or("stone destination overflow")?;
        }
        if [pos, destination].iter().any(|at| {
            !(-64..320).contains(&at[1])
                || !self
                    .world
                    .chunks
                    .contains_key(&(at[0].div_euclid(16), at[2].div_euclid(16)))
        }) {
            return Err(
                "ordinary stone target and destination must be actual loaded Overworld columns"
                    .into(),
            );
        }
        let target = self
            .res
            .facts()
            .get(self.world.block(pos[0], pos[1], pos[2]));
        if !matches!(
            &*target.name,
            "minecraft:stone" | "minecraft:dirt" | "minecraft:grass_block" | "minecraft:bedrock"
        ) || self
            .world
            .block(destination[0], destination[1], destination[2])
            != self.res.facts().air
        {
            return Err("ordinary stone placement refuses unsupported native interaction or occupied destination".into());
        }
        Ok(())
    }
    pub(super) fn retained_server_check_running(&self) -> Result<()> {
        if let Some(server) = &self.retained_server
            && server.frozen
        {
            return Err(server
                .failure
                .as_deref()
                .unwrap_or("original retained server frozen")
                .into());
        }
        Ok(())
    }
    fn retained_server_settle_step(&mut self) -> Result<bool> {
        self.retained_server_check_running()?;
        self.update_world();
        self.poll_ordinary_native_reads()?;
        self.retained_server_check_running()?;
        self.postprocess_chunks()?;
        if self.spawn.is_none() {
            let Some([x, y, z]) = self.find_world_spawn()? else {
                return Ok(false);
            };
            self.spawn = Some(Vec3::new(
                f64::from(x) + 0.5,
                f64::from(y),
                f64::from(z) + 0.5,
            ));
            self.level.spawn = Some([f64::from(x), f64::from(y), f64::from(z)]);
            self.level.world_spawn = Some([x, y, z]);
        }
        let spawn = self.spawn.unwrap();
        let (cx, cz) = ((spawn.x.floor() as i32) >> 4, (spawn.z.floor() as i32) >> 4);
        self.streaming.expect_existing_mut().center = (cx, cz);
        Ok((-1..=1)
            .all(|dz| (-1..=1).all(|dx| self.streaming.expect_existing().is_lit(cx + dx, cz + dz))))
    }
}

#[derive(Clone, Debug)]
pub(super) struct View {
    #[cfg(all(test, feature = "mods", target_os = "linux"))]
    ordinary_counter: serde_json::Value,
    ordinary_closed_world: Arc<Mutex<Option<super::paired_close::OrdinaryClosedWorld>>>,
    phase: &'static str,
    pub(super) attempt: u64,
    pub(super) ready: bool,
    /// Observation only. `ready` remains transport-offer eligibility.
    owned_editable: bool,
    port: u16,
    ticks: u64,
    world_time: i64,
    guests: usize,
    installed: usize,
    pending_events: usize,
    pub(super) original_error: Option<String>,
    pub(super) blocked: Option<String>,
    worker_joined: bool,
    players: Vec<serde_json::Value>,
    transport: Option<RetainedHubCloseView>,
    owner_record: Option<[i32; 4]>,
    owner_saved: bool,
    paused: bool,
    #[cfg(test)]
    consume_refusal_checked: bool,
}
impl Default for View {
    fn default() -> Self {
        Self {
            #[cfg(all(test, feature = "mods", target_os = "linux"))]
            ordinary_counter: serde_json::Value::Null,
            ordinary_closed_world: Arc::new(Mutex::new(None)),
            phase: "Preparing",
            attempt: 0,
            ready: false,
            owned_editable: false,
            port: 0,
            ticks: 0,
            world_time: 0,
            guests: 0,
            installed: 0,
            pending_events: 0,
            original_error: None,
            blocked: None,
            worker_joined: false,
            players: Vec::new(),
            transport: None,
            owner_record: None,
            owner_saved: false,
            paused: false,
            #[cfg(test)]
            consume_refusal_checked: false,
        }
    }
}
impl View {
    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::json!({"phase":self.phase,"attempt":self.attempt,"ready":self.ready,"owned_editable":self.owned_editable,"port":self.port,
        "ticks":self.ticks,"world_time":self.world_time,"guests":self.guests,"installed":self.installed,
        "pending_events":self.pending_events,"original_error":self.original_error,"blocked":self.blocked,
        "owner_record":self.owner_record,"owner_saved":self.owner_saved,"paused":self.paused,
        "worker_joined":self.worker_joined,"players":self.players,
        "transport":self.transport.as_ref().map(|view|serde_json::json!({
            "phase":format!("{:?}",view.phase),"attempt":view.attempt,"failure":view.failure,
            "quiescent":view.transport.quiescent,"pending_events":view.pending_events,"inbox_packets":view.inbox_packets,
            "queued_events":view.transport.queued_events,"applying_packet":view.applying_packet,
            "terminals":view.transport.terminals.iter().map(|terminal|serde_json::json!({"conn":terminal.conn,
                "actor":format!("{:?}",terminal.actor),"outcome":format!("{:?}",terminal.outcome)})).collect::<Vec<_>>(),
            "outbound":view.transport.attempts.iter().filter_map(|attempt|attempt.outbound.as_ref().map(|outbound|
                serde_json::json!({"conn":attempt.conn,"observation":format!("{outbound:?}"),
                    "queued_bodies":outbound.queued_bodies,"active_bodies":outbound.active_bodies,
                    "failure":outbound.failure.as_ref().map(|failure|format!("{failure:?}"))}))).collect::<Vec<_>>()
        }))})
    }
}

fn transport_failure(view: &RetainedHubCloseView) -> Option<String> {
    view.failure
        .clone()
        .or_else(|| {
            view.transport.attempts.iter().find_map(|attempt| {
                attempt
                    .outbound
                    .as_ref()
                    .and_then(|outbound| outbound.failure.as_ref())
                    .map(|failure| format!("original outbound {}: {failure:?}", attempt.conn))
            })
        })
        .or_else(|| {
            view.transport.terminals.iter().find_map(|terminal| {
                // These are concrete library stop/normal-return outcomes. EOF,
                // Receive, IO, panic and every other raw result remain failures,
                // even if first harvested after the deliberate close request.
                (!matches!(
                    terminal.outcome,
                    RetainedOutcome::Returned | RetainedOutcome::Closed
                ))
                .then(|| format!("original retained transport completion: {terminal:?}"))
            })
        })
}
#[cfg_attr(not(test), allow(dead_code))] // Memory offers are exercised by the fixed native fixture; default attachment remains separate.
enum Command {
    Close,
    Retry(u64),
    Offer(mpsc::Sender<std::result::Result<Pipe, String>>),
}
struct Input {
    options: Options,
    settings: Settings,
    launch: Launch,
    owner: Option<u128>,
    #[cfg(test)]
    test: tests::Startup,
}
impl Input {
    #[cfg_attr(not(test), allow(dead_code))] // Joint window attachment follows this native owner adapter.
    #[allow(clippy::result_large_err)] // Refusal preserves the complete inert launch input.
    fn with_integrated_owner(mut self, name: &str) -> std::result::Result<Self, (Self, String)> {
        if name.is_empty()
            || name.len() > 16
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err((self, "invalid integrated offline player name".into()));
        }
        self.owner = Some(horizon_protocol::crypto::offline_uuid(name));
        self.options.execution_role = execution_role::ExecutionRole::IntegratedServer;
        // Match the ordinary integrated factory's native no-local-player marker.
        self.options.dedicated = Some(Vec::new());
        self.options.player_name = Some(name.into());
        Ok(self)
    }
}
enum WorkerInput {
    Cold(Input),
    #[cfg(feature = "mods")]
    Ordinary(Box<OrdinaryServerInput>),
}
/// The ordinary caller moves its actual final generation into the original
/// server worker. No renderer, window, replacement package scan or fixed Launch.
#[cfg(feature = "mods")]
pub(super) struct OrdinaryServerInput {
    pub(super) options: Options,
    pub(super) settings: Settings,
    pub(super) request: OrdinaryRequest,
    pub(super) entry: RetainedEntryIdentity,
    pub(super) resources: Arc<Resources>,
    pub(super) pack: PackResources,
    pub(super) gui: UiResources,
    pub(super) cpu: GameCpuResources,
    pub(super) tree: Arc<horizon_client::advancements::Tree>,
    pub(super) startup: Arc<mod_packages::FrozenHostInputs>,
    pub(super) offline: RetainedOfflineConfig,
    pub(super) dedicated_setup: Option<dedicated::RetainedSetup>,
    pub(super) continuation_archive: Option<PathBuf>,
    #[cfg(test)]
    pub(super) test: tests::Startup,
}
#[cfg(feature = "mods")]
pub(super) enum OrdinaryRequest {
    Native(world_loading::WorldRequest),
    #[allow(dead_code)] // Typed actual fixture route; ordinary machine ingress stays closed.
    Recover(Box<super::levels::session_checkpoint::OrdinaryRecoveryRequest>),
    RecoverOwned(Box<super::levels::session_checkpoint::OwnedRecoveryRequest>),
}
#[cfg(feature = "mods")]
pub(super) struct OrdinaryStartRefusal {
    _input: Box<OrdinaryServerInput>,
    _activation: Option<Box<mod_packages::ActivationRefusal>>,
    pub(super) reason: String,
}
pub(super) struct Supervisor {
    thread: Option<JoinHandle<std::result::Result<(), String>>>,
    commands: mpsc::Sender<Command>,
    view: Arc<Mutex<View>>,
    _inert: Arc<Mutex<Option<WorkerInput>>>,
    #[cfg(feature = "mods")]
    _ordinary_refusal: Arc<Mutex<Option<Box<OrdinaryStartRefusal>>>>,
    paused: Arc<AtomicBool>,
}
pub(super) struct IntegratedStartRefusal {
    _input: Box<Input>,
    pub(super) reason: String,
}
impl Supervisor {
    #[cfg(feature = "mods")]
    pub(super) fn prepare_ordinary(
        input: OrdinaryServerInput,
    ) -> std::result::Result<Self, Box<OrdinaryStartRefusal>> {
        Self::prepare_original(WorkerInput::Ordinary(Box::new(input))).map_err(|(input, reason)| {
            let WorkerInput::Ordinary(input) = input else {
                unreachable!("same original ordinary input");
            };
            Box::new(OrdinaryStartRefusal {
                _input: input,
                _activation: None,
                reason,
            })
        })
    }
    pub(super) fn prepare_integrated(
        mut options: Options,
        mut settings: Settings,
        root: PathBuf,
        name: &str,
    ) -> std::result::Result<Self, Box<IntegratedStartRefusal>> {
        let launch = Launch {
            root,
            args: Vec::new(),
        };
        launch.configure(&mut options);
        options.control = None;
        // The remote frontend keeps its native settings; only this fixed
        // server profile uses the existing bounded backend distances.
        settings.render_distance = 2;
        settings.simulation_distance = 2;
        let input = Input {
            options,
            settings,
            launch,
            owner: None,
            #[cfg(test)]
            test: tests::Startup::default(),
        };
        let refusal = |input, reason| {
            Box::new(IntegratedStartRefusal {
                _input: Box::new(input),
                reason,
            })
        };
        if !input.launch.root.is_absolute()
            || input
                .launch
                .root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(refusal(
                input,
                "integrated server requires a fresh absolute application child".into(),
            ));
        }
        if let Err(error) = std::fs::create_dir(&input.launch.root) {
            return Err(refusal(input, error.to_string()));
        }
        let input = input
            .with_integrated_owner(name)
            .map_err(|(input, reason)| refusal(input, reason))?;
        Self::prepare(input).map_err(|(input, reason)| refusal(input, reason))
    }
    pub(super) fn offer(
        &self,
    ) -> std::result::Result<mpsc::Receiver<std::result::Result<Pipe, String>>, String> {
        let (send, receive) = mpsc::channel();
        self.commands
            .send(Command::Offer(send))
            .map_err(|_| "original server no longer accepts Memory offers".to_string())?;
        Ok(receive)
    }
    pub(super) fn request_close(&self) {
        let _ = self.commands.send(Command::Close);
    }
    pub(super) fn retry(&self, attempt: u64) -> bool {
        self.commands.send(Command::Retry(attempt)).is_ok()
    }
    pub(super) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }
    /// The handle is registered before the worker can acquire native owners.
    #[allow(clippy::result_large_err)] // Refusal returns the exact original inert inputs.
    fn prepare(input: Input) -> std::result::Result<Self, (Input, String)> {
        Self::prepare_original(WorkerInput::Cold(input)).map_err(|(input, reason)| match input {
            WorkerInput::Cold(input) => (input, reason),
            #[cfg(feature = "mods")]
            WorkerInput::Ordinary(_) => unreachable!("same original cold input"),
        })
    }
    #[allow(clippy::result_large_err)]
    fn prepare_original(input: WorkerInput) -> std::result::Result<Self, (WorkerInput, String)> {
        let inert = Arc::new(Mutex::new(Some(input)));
        let worker_input = inert.clone();
        let view = Arc::new(Mutex::new(View::default()));
        let worker_view = view.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let worker_paused = paused.clone();
        #[cfg(feature = "mods")]
        let ordinary_refusal = Arc::new(Mutex::new(None));
        #[cfg(feature = "mods")]
        let worker_refusal = ordinary_refusal.clone();
        let (commands, receive) = mpsc::channel();
        let (release, acquire) = mpsc::sync_channel(0);
        let handle = std::thread::Builder::new()
            .name("Retained original server".into())
            .spawn(move || {
                if acquire.recv().is_err() {
                    return Err("server acquisition latch refused".into());
                }
                let input = worker_input
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("original inert input");
                match input {
                    WorkerInput::Cold(input) => worker(input, receive, worker_view, worker_paused),
                    #[cfg(feature = "mods")]
                    WorkerInput::Ordinary(input) => worker_ordinary(
                        *input,
                        receive,
                        worker_view,
                        worker_paused,
                        &worker_refusal,
                    ),
                }
            });
        match handle {
            Err(error) => Err((
                inert
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .unwrap(),
                error.to_string(),
            )),
            Ok(thread) => {
                let owner = Self {
                    thread: Some(thread),
                    commands,
                    view,
                    _inert: inert,
                    #[cfg(feature = "mods")]
                    _ordinary_refusal: ordinary_refusal,
                    paused,
                };
                // A refused latch is an original finished worker result, joined
                // by the same supervisor; no acquisition has taken place.
                let _ = release.send(());
                Ok(owner)
            }
        }
    }
    pub(super) fn snapshot(&self) -> View {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }
    pub(super) fn join(&mut self) -> Option<std::result::Result<(), String>> {
        if !self.finished() {
            return None;
        }
        let result = self
            .thread
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| Err("original server worker panicked".into()));
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_joined = true;
        Some(result)
    }
    pub(super) fn take_ordinary_closed_world(
        &mut self,
    ) -> Option<super::paired_close::OrdinaryClosedWorld> {
        if self.thread.is_some() {
            return None;
        }
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ordinary_closed_world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.thread.is_some() {
            eprintln!("fatal: unresolved original server worker");
            std::process::abort();
        }
    }
}

struct HubInputs {
    sync: Arc<horizon_client::lan::data::SyncData>,
    encoder: ChunkEncoder,
    config: HubConfig,
    offline: RetainedOfflineConfig,
}
fn hub_inputs(
    resources: &Resources,
    pack: &PackResources,
    root: &std::path::Path,
) -> Result<HubInputs> {
    let sync = super::lan::sync_data(root)?;
    hub_inputs_with_sync(resources, pack, sync)
}
fn hub_inputs_with_sync(
    resources: &Resources,
    pack: &PackResources,
    sync: Arc<horizon_client::lan::data::SyncData>,
) -> Result<HubInputs> {
    let biomes = resources
        .biomes()
        .list
        .iter()
        .map(|b| b.name.clone())
        .collect::<Vec<_>>();
    let registry = sync
        .dynamic
        .registries
        .get("minecraft:worldgen/biome")
        .ok_or("biome registry missing")?
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let mut encoder = ChunkEncoder::new(
        resources.facts().states.iter().map(|state| {
            (
                state.name.clone(),
                state.fluid_level > 0 || state.waterlogged,
            )
        }),
        &sync.builtin,
        &biomes,
        &registry,
    );
    encoder.tables = Some(Arc::new(horizon_client::save::SaveTables::new(
        &pack.assets,
    )));
    Ok(HubInputs {
        sync,
        encoder,
        config: HubConfig {
            default_game_mode: 1,
            guest_commands: false,
            force_game_mode: false,
            hardcore: false,
            view_distance: 2,
            simulation_distance: 2,
            dimension: "minecraft:overworld".into(),
            seed: 17,
            max_players: 2,
            dedicated: true,
            integrated: false,
        },
        offline: RetainedOfflineConfig {
            bind: Some(std::net::Ipv4Addr::LOCALHOST.into()),
            port: 0,
            motd: "Retained original server".into(),
            max_players: 2,
            compression_threshold: -1,
            enable_status: true,
            hide_online_players: false,
            announce: false,
        },
    })
}

#[cfg(feature = "mods")]
fn worker_ordinary(
    input: OrdinaryServerInput,
    commands: mpsc::Receiver<Command>,
    view: Arc<Mutex<View>>,
    paused: Arc<AtomicBool>,
    refusal: &Mutex<Option<Box<OrdinaryStartRefusal>>>,
) -> std::result::Result<(), String> {
    let validate: Result<HubInputs> = (|| {
        if let OrdinaryRequest::Native(request) = &input.request {
            request.check_ordinary_native_entry()?;
        }
        if input.options.server_thread
            || input.options.server.is_some()
            || input.options.dedicated.is_none()
            || !input.options.test_terrain
            || input.options.dimension.is_some()
            || input.options.execution_role.is_client()
            || !input
                .startup
                .retains_startup(&input.options.mod_packages, input.pack.assets.registry)
            || input.settings.render_distance != 2
            || input.settings.simulation_distance != 2
            || input.settings.horizon.lod_distance != 0
            || input.tree.list.is_empty()
        {
            return Err("ordinary server requires its actual bounded final startup inputs".into());
        }
        if input.options.execution_role == execution_role::ExecutionRole::IntegratedServer {
            let name = input
                .options
                .player_name
                .as_deref()
                .ok_or("integrated owner name missing")?;
            if !input.options.dedicated.as_ref().is_some_and(Vec::is_empty)
                || name.is_empty()
                || name.len() > 16
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || input.offline.bind.is_some()
                || input.offline.max_players != 1
            {
                return Err(
                    "ordinary integrated server requires its actual single Memory owner".into(),
                );
            }
        } else if input.offline.max_players != 2 {
            return Err(
                "ordinary dedicated server requires the explicit two-player profile".into(),
            );
        }
        hub_inputs_with_sync(&input.resources, &input.pack, input.startup.retained_sync())
    })();
    let mut inputs = match validate {
        Ok(inputs) => inputs,
        Err(error) => {
            let reason = error.to_string();
            *refusal.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Box::new(OrdinaryStartRefusal {
                    _input: Box::new(input),
                    _activation: None,
                    reason: reason.clone(),
                }));
            return Err(reason);
        }
    };
    let activated = match input.startup.activate(
        input.options.execution_role,
        input.options.server_thread,
        false,
        &input.settings,
    ) {
        Ok(activated) => activated,
        Err(activation) => {
            let reason = activation.reason.clone();
            *refusal.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Box::new(OrdinaryStartRefusal {
                    _input: Box::new(input),
                    _activation: Some(activation),
                    reason: reason.clone(),
                }));
            return Err(reason);
        }
    };
    let mut cancelled = horizon_threads::stop_requests() > 0;
    for command in commands.try_iter().take(16) {
        match command {
            Command::Close => cancelled = true,
            Command::Offer(reply) => {
                let _ = reply.send(Err("ordinary world not yet acquired".into()));
            }
            Command::Retry(_) => {}
        }
    }
    if cancelled {
        view.lock().unwrap_or_else(|e| e.into_inner()).phase = "UnacquiredClosed";
        return Ok(());
    }
    if matches!(
        &input.request,
        OrdinaryRequest::Recover(_) | OrdinaryRequest::RecoverOwned(_)
    ) {
        return worker_ordinary_recovery(input, activated, inputs, commands, &view, &paused);
    }
    let OrdinaryServerInput {
        options,
        settings,
        request,
        entry,
        resources,
        pack,
        gui,
        cpu,
        tree,
        startup,
        offline,
        dedicated_setup,
        continuation_archive,
        #[cfg(test)]
        test,
    } = input;
    let OrdinaryRequest::Native(request) = request else {
        unreachable!()
    };
    let prepared = world_loading::OrdinaryPreparation::prepare(
        request, &resources, &pack, &options, &settings, &entry, startup,
    );
    let prepared = match prepared.into_prepared(&entry) {
        Ok(prepared) => prepared,
        Err(prepared) => {
            return close_preparation(
                ServerPreparation::Ordinary(prepared),
                commands,
                &view,
                (resources, pack, gui, cpu, tree, inputs, activated),
            );
        }
    };
    let root = options.resources.clone();
    let audio = Audio::silent(&root);
    let mut game = Box::new(
        Game::new_with_world(
            &resources,
            pack,
            gui,
            options,
            settings,
            audio,
            Some(cpu),
            InitialGameWorld::Ordinary(Box::new(prepared)),
        )
        .unwrap_or_else(|error| {
            eprintln!("fatal: preloaded ordinary native transfer failed: {error}");
            std::process::abort();
        }),
    );
    if let Err(original) = game.adopt_activated_startup(activated) {
        eprintln!(
            "fatal: original ordinary runtime association refused: {}",
            original.1
        );
        std::process::abort();
    }
    game.title = None;
    game.retained_server.as_mut().unwrap().continuation_archive = game
        .session_lock
        .as_ref()
        .and_then(|session| session.owned())
        .map(|owned| owned.archive().to_owned())
        .or(continuation_archive);
    #[cfg(test)]
    {
        game.retained_server.as_mut().unwrap().test = test;
    }
    #[cfg(all(test, feature = "mods", target_os = "linux"))]
    if game
        .retained_server
        .as_ref()
        .unwrap()
        .test
        .ordinary
        .is_some()
    {
        paused.store(true, Ordering::SeqCst);
    }
    game.screen = Screen::Playing;
    game.adv.tree = tree;
    game.skin_profiles = RefCell::new(horizon_client::skins::Resolver::without_fetch());
    game.skin_downloads = RefCell::new(horizon_client::skins::Downloader::without_fetch(
        root.join("assets/skins"),
    ));
    game.streaming.expect_existing_mut().radius = 2;
    game.streaming.expect_existing_mut().center = game.level.position.map_or((0, 0), |position| {
        (
            (position[0].floor() as i32).div_euclid(16),
            (position[2].floor() as i32).div_euclid(16),
        )
    });
    game.load_world_state();
    if let Some(setup) = &dedicated_setup {
        game.level.creative = true;
        game.level.game_mode = 1;
        game.level.hardcore = false;
        game.ui.difficulty = setup.props.difficulty as u8;
        inputs.config.force_game_mode = setup.props.force_game_mode;
    }
    inputs.config.default_game_mode = if game.level.creative { 1 } else { 0 };
    inputs.config.hardcore = game.level.hardcore;
    inputs.config.seed = game.level.seed;
    inputs.config.integrated =
        game.options.execution_role == execution_role::ExecutionRole::IntegratedServer;
    inputs.config.max_players = offline.max_players;
    inputs.config.view_distance = game.settings.render_distance;
    inputs.config.simulation_distance = game.settings.simulation_distance;
    let startup_error = game
        .install_original_retained_host(inputs.sync, inputs.encoder, inputs.config, offline)
        .err();
    if let Some(setup) = &dedicated_setup
        && let Some(host) = &mut game.lan
    {
        host.hub.spawn_protection = setup.props.spawn_protection;
    }
    run_game(game, commands, &view, startup_error, &paused)
}

#[cfg(feature = "mods")]
fn worker_ordinary_recovery(
    input: OrdinaryServerInput,
    activated: mod_packages::ActivatedStartup,
    mut inputs: HubInputs,
    commands: mpsc::Receiver<Command>,
    view: &Mutex<View>,
    paused: &AtomicBool,
) -> std::result::Result<(), String> {
    use super::levels::session_checkpoint::OrdinaryRecoveryWork;
    let OrdinaryServerInput {
        options,
        settings,
        request,
        entry,
        resources,
        pack,
        gui,
        cpu,
        tree,
        startup,
        offline,
        dedicated_setup,
        continuation_archive: _,
        #[cfg(test)]
        test,
    } = input;
    let until = match &request {
        OrdinaryRequest::Recover(request) => request.deadline(),
        OrdinaryRequest::RecoverOwned(request) => request.deadline,
        OrdinaryRequest::Native(_) => unreachable!(),
    };
    let root = options.resources.clone();
    let mut game = Box::new(
        Game::new_with_world(
            &resources,
            pack,
            gui,
            options,
            settings,
            Audio::silent(&root),
            Some(cpu),
            InitialGameWorld::OrdinaryRecoverySeed(startup),
        )
        .unwrap_or_else(|error| {
            eprintln!("fatal: preloaded no-world recovery candidate refused: {error}");
            std::process::abort();
        }),
    );
    game.adv.tree = tree;
    game.skin_profiles = RefCell::new(horizon_client::skins::Resolver::without_fetch());
    game.skin_downloads = RefCell::new(horizon_client::skins::Downloader::without_fetch(
        root.join("assets/skins"),
    ));
    if let Err(original) = game.adopt_activated_startup(activated) {
        eprintln!(
            "fatal: original recovery runtime association refused: {}",
            original.1
        );
        std::process::abort();
    }
    let mut recovery = match request {
        OrdinaryRequest::Recover(request) => OrdinaryRecoveryWork::prepare(game, entry, *request),
        OrdinaryRequest::RecoverOwned(request) => {
            OrdinaryRecoveryWork::prepare_owned(game, entry, *request)
        }
        OrdinaryRequest::Native(_) => unreachable!(),
    };
    #[cfg(test)]
    if let Some(server) = recovery.game_mut().retained_server.as_mut() {
        server.test = test;
    }
    #[cfg(all(
        test,
        target_os = "linux",
        feature = "native-writer-test-control",
        debug_assertions
    ))]
    if recovery.original_error().is_none() {
        let fixture = recovery
            .game_mut()
            .retained_server
            .as_ref()
            .and_then(|server| server.test.ordinary_recovery_writer.clone());
        if let Some(fixture) = fixture
            && let Err(error) = fixture.arm(recovery.game_mut())
        {
            recovery.fail(error);
        }
    }
    #[cfg(all(test, feature = "mods", target_os = "linux"))]
    if recovery
        .game_mut()
        .retained_server
        .as_ref()
        .is_some_and(|server| server.test.ordinary.is_some())
    {
        paused.store(true, Ordering::SeqCst);
    }
    if recovery.original_error().is_none() {
        let game = recovery.game_mut();
        inputs.config.default_game_mode = game.level.mode();
        inputs.config.hardcore = game.level.hardcore;
        inputs.config.seed = game.level.seed;
        inputs.config.integrated =
            game.options.execution_role == execution_role::ExecutionRole::IntegratedServer;
        inputs.config.max_players = offline.max_players;
        inputs.config.view_distance = game.settings.render_distance;
        inputs.config.simulation_distance = game.settings.simulation_distance;
        if let Some(setup) = &dedicated_setup {
            inputs.config.force_game_mode = setup.props.force_game_mode;
        }
        let result = game.install_original_retained_host(
            inputs.sync,
            inputs.encoder,
            inputs.config,
            offline,
        );
        if let Err(reason) = result {
            recovery.fail(reason);
        }
        if let Some(setup) = &dedicated_setup
            && let Some(host) = recovery.game_mut().lan.as_mut()
        {
            host.hub.spawn_protection = setup.props.spawn_protection;
        }
    }
    let mut state = View::default();
    let mut listener_started = false;
    loop {
        if horizon_threads::stop_requests() > 0 {
            recovery.fail("ordinary recovery cancelled by stop request".into());
        }
        if Instant::now() >= until {
            recovery.fail("ordinary recovery admission deadline expired".into());
        }
        for command in commands.try_iter().take(16) {
            match command {
                Command::Close => {
                    recovery.fail("ordinary recovery cancelled before verified activation".into())
                }
                Command::Offer(reply) => {
                    let offered = if listener_started && recovery.original_error().is_none() {
                        recovery
                            .game_mut()
                            .lan
                            .as_mut()
                            .unwrap()
                            .hub
                            .retained_connect_local()
                            .map_err(|error| error.to_string())
                    } else {
                        Err("original recovery is not admitting Memory offers".into())
                    };
                    if let Err(mpsc::SendError(Ok(pipe))) = reply.send(offered) {
                        pipe.shutdown();
                        recovery
                            .game_mut()
                            .retained_server
                            .as_mut()
                            .unwrap()
                            .unsent_pipes
                            .push(pipe);
                        recovery.fail("original recovery Memory offer reply was lost".into());
                    }
                }
                Command::Retry(_) => {}
            }
        }
        if recovery.original_error().is_some() {
            return close_ordinary_recovery(recovery, commands, view);
        }
        if !listener_started {
            let game = recovery.game_mut();
            let listening = if game
                .retained_server
                .as_ref()
                .unwrap()
                .integrated_owner()
                .is_some()
            {
                Ok(0)
            } else {
                game.lan
                    .as_mut()
                    .unwrap()
                    .hub
                    .retained_listen()
                    .map_err(|error| error.to_string())
            };
            match listening {
                Ok(port) => {
                    state.port = port;
                    listener_started = true;
                }
                Err(reason) => {
                    recovery.fail(reason);
                    continue;
                }
            }
        }
        if let Err(reason) = recovery.game_mut().poll_ordinary_recovery_admission() {
            recovery.fail(reason);
            continue;
        }
        match recovery.try_activate() {
            Ok(Some(mut game)) => {
                // The exact listener/Memory Hub is already acquired. First
                // gameplay occurs only after consuming verified recovery.
                let error = match game.publish_ordinary_owned_roster() {
                    Ok(true) => None,
                    // Installation and original publication precede normal
                    // native placement/loaded acknowledgment. No NEW native-only
                    // cancellation right is acquired by waiting for that step.
                    Ok(false) => None,
                    Err(error) => Some(error),
                };
                return run_game_from(game, commands, view, error, paused, true, true);
            }
            Ok(None) => {}
            Err(error) => {
                recovery.fail(error.to_string());
                continue;
            }
        }
        let game = recovery.game_mut();
        state.phase = "RecoveryJoining";
        state.ready = true; // Offer eligibility only; no native/player activation.
        state.world_time = game.world.game_time;
        state.guests = game.lan.as_ref().unwrap().hub.guests.len();
        state.pending_events = game.retained_server.as_ref().unwrap().pending.len();
        *view.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(feature = "mods")]
fn close_ordinary_recovery(
    mut recovery: super::levels::session_checkpoint::OrdinaryRecoveryWork<'_>,
    commands: mpsc::Receiver<Command>,
    view: &Mutex<View>,
) -> std::result::Result<(), String> {
    let original = recovery
        .original_error()
        .unwrap_or("ordinary recovery cancelled")
        .to_owned();
    recovery.begin_close(Instant::now() + CLOSE_WAIT);
    let mut attempt = 1;
    loop {
        let status = recovery.poll_close();
        let blocked = match &status {
            PreparationCloseStatus::Blocked(reason) => Some(reason.clone()),
            _ => None,
        };
        *view.lock().unwrap_or_else(|e| e.into_inner()) = View {
            phase: "RecoveryClosing",
            attempt,
            original_error: Some(original.clone()),
            blocked: blocked.clone(),
            ..Default::default()
        };
        for command in commands.try_iter().take(16) {
            match command {
                Command::Retry(expected) if expected == attempt && blocked.is_some() => {
                    if recovery.retry_close(Instant::now() + CLOSE_WAIT).is_ok() {
                        attempt += 1;
                    }
                }
                Command::Offer(reply) => {
                    let _ = reply.send(Err("original recovery is closing".into()));
                }
                _ => {}
            }
        }
        if status == PreparationCloseStatus::Complete {
            return Err(original);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(feature = "mods")]
impl Game<'_> {
    fn poll_ordinary_recovery_admission(&mut self) -> std::result::Result<(), String> {
        let clocks = (
            self.world.game_time,
            self.world.day_time,
            self.world.random.state(),
        );
        let spawn = self
            .level
            .world_spawn
            .ok_or("checked recovery world spawn absent")?;
        self.spawn = Some(Vec3::new(
            f64::from(spawn[0]) + 0.5,
            f64::from(spawn[1]),
            f64::from(spawn[2]) + 0.5,
        ));
        let clock_rate = self.network_clock_rate();
        let host = self.lan.as_mut().ok_or("original recovery Hub absent")?;
        let mut ctx = horizon_client::lan::hub::TickCtx {
            world: &mut self.world,
            host: None,
            entities: &[],
            next_entity: &mut self.next_entity,
            spawn: [
                f64::from(spawn[0]) + 0.5,
                f64::from(spawn[1]),
                f64::from(spawn[2]) + 0.5,
            ],
            difficulty: i32::from(self.ui.difficulty),
            difficulty_locked: self.level.difficulty_locked,
            clock_rate,
            tick_rate: self.clock.rate(),
            tick_frozen: self.clock.frozen(),
            tick_steps: self.clock.step_state(),
            collision: Some(self.collision),
            border: &self.border,
        };
        let events = host.hub.poll_retained(&mut ctx, 16);
        let server = self
            .retained_server
            .as_mut()
            .ok_or("original recovery construction absent")?;
        server.pending.extend(events);
        server.check_hub(&host.hub);
        server.check_inboxes(&host.hub);
        if let Some(reason) = &server.failure {
            return Err(reason.clone());
        }
        if (
            self.world.game_time,
            self.world.day_time,
            self.world.random.state(),
        ) != clocks
        {
            return Err("recovery admission changed native clocks/random".into());
        }
        Ok(())
    }
}

fn worker(
    input: Input,
    commands: mpsc::Receiver<Command>,
    view: Arc<Mutex<View>>,
    paused: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let Input {
        options,
        settings,
        launch,
        owner,
        #[cfg(test)]
        test,
    } = input;
    // These operations precede native acquisition. Every complete resource
    // allocation stays lexical until Game and all dependent native actors close.
    let root = std::fs::canonicalize(&options.resources).map_err(|e| e.to_string())?;
    let loaded = load_resources(
        root.clone(),
        &root,
        &settings,
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .map_err(|e| e.to_string())?;
    if loaded.rest_loader.is_some() {
        eprintln!("fatal: synchronous retained server resources unexpectedly started a helper");
        std::process::abort();
    }
    if loaded.pack_error.is_some() || loaded.root != root {
        return Err("retained server requires complete synchronous resources".into());
    }
    let gui = loaded
        .gui
        .ok_or("retained server GUI/font inputs missing")?;
    let cpu = GameCpuResources::load(&root).map_err(|e| e.to_string())?;
    let tree = Arc::new(horizon_client::advancements::Tree::load(&root.join("data")));
    if tree.list.is_empty() {
        return Err("retained server advancement resource input missing".into());
    }
    let resources = loaded.resources;
    let mut inputs = hub_inputs(&resources, &loaded.pack, &root).map_err(|e| e.to_string())?;
    if owner.is_some() {
        inputs.config.integrated = true;
        inputs.config.max_players = 1;
        inputs.offline.bind = None;
        inputs.offline.max_players = 1;
    }
    let mut stop_before_acquisition = horizon_threads::stop_requests() > 0;
    for command in commands.try_iter().take(16) {
        match command {
            Command::Close => stop_before_acquisition = true,
            Command::Offer(reply) => {
                let _ = reply.send(Err("original native server has not been acquired".into()));
            }
            Command::Retry(_) => {}
        }
    }
    if stop_before_acquisition {
        view.lock().unwrap_or_else(|e| e.into_inner()).phase = "UnacquiredClosed";
        return Ok(());
    }
    let mut request = world_loading::WorldRequest::new(
        "retained-server",
        Some("Retained original server"),
        Some(17),
        true,
        None,
    )
    .map_err(|e| e.to_string())?
    .with_identity_intent(world_loading::host_identity::IdentityIntent::CreateNewIdentity);
    request.save_root = launch.root.join("world");
    let entry = RetainedEntryIdentity {
        token: 1,
        owner: Arc::new(()),
    };
    #[cfg(not(test))]
    let cold = ColdPreparation::prepare_server(
        request,
        &resources,
        &loaded.pack,
        &options,
        &settings,
        &entry,
        owner,
    );
    #[cfg(test)]
    let cold = if test.fail_after_streaming {
        ColdPreparation::prepare_server_failure_for_test(
            request,
            &resources,
            &loaded.pack,
            &options,
            &settings,
            &entry,
        )
    } else {
        ColdPreparation::prepare_server(
            request,
            &resources,
            &loaded.pack,
            &options,
            &settings,
            &entry,
            owner,
        )
    };
    let prepared = match cold.into_prepared(&entry) {
        Ok(prepared) => prepared,
        Err(cold) => {
            return close_preparation(
                ServerPreparation::Cold(cold),
                commands,
                &view,
                (loaded.pack, gui, inputs, resources, cpu),
            );
        }
    };
    let audio = Audio::silent(&root);
    let mut game = match Game::new_with_world(
        &resources,
        loaded.pack,
        gui,
        options,
        settings,
        audio,
        Some(cpu),
        InitialGameWorld::Retained(Box::new(prepared)),
    ) {
        Ok(game) => Box::new(game),
        // Preloaded CPU plus the typed retained branch has no fallible step
        // after selection of original native inputs. A violated invariant is
        // fatal, never an ordinary owner-discarding startup error return.
        Err(error) => {
            eprintln!("fatal: cold retained server transfer: {error}");
            std::process::abort();
        }
    };
    game.title = None;
    game.screen = Screen::Playing;
    game.adv.tree = tree;
    game.skin_profiles = RefCell::new(horizon_client::skins::Resolver::without_fetch());
    game.skin_downloads = RefCell::new(horizon_client::skins::Downloader::without_fetch(
        root.join("assets/skins"),
    ));
    game.streaming.expect_existing_mut().radius = 2;
    game.streaming.expect_existing_mut().center = (0, 0);
    game.load_world_state();
    #[cfg(test)]
    let fixture_error = {
        game.retained_server.as_mut().unwrap().test = test.clone();
        test.install_staged_inputs(&game.world_root)
            .and_then(|_| test.install_owner_inputs(&mut game))
            .err()
    };
    #[cfg(not(test))]
    let fixture_error: Option<String> = None;
    let startup = match fixture_error {
        None => game.install_original_retained_host(
            inputs.sync,
            inputs.encoder,
            inputs.config,
            inputs.offline,
        ),
        Some(error) => {
            game.retained_server
                .as_mut()
                .unwrap()
                .hold_hub_refusal(Box::new(RetainedHubRefusal {
                    sync: inputs.sync,
                    encoder: inputs.encoder,
                    config: inputs.config,
                    offline: inputs.offline,
                    reason: error.clone(),
                }));
            Err(error)
        }
    };
    run_game(game, commands, &view, startup.err(), &paused)
}

enum ServerPreparation {
    Cold(ColdPreparation),
    #[cfg(feature = "mods")]
    Ordinary(world_loading::OrdinaryPreparation),
}
impl ServerPreparation {
    fn original_error(&self) -> Option<&str> {
        match self {
            Self::Cold(owner) => owner.original_error(),
            #[cfg(feature = "mods")]
            Self::Ordinary(owner) => owner.original_error(),
        }
    }
    fn begin_close(&mut self, deadline: Instant) {
        match self {
            Self::Cold(owner) => owner.begin_close(deadline),
            #[cfg(feature = "mods")]
            Self::Ordinary(owner) => owner.begin_close(deadline),
        }
    }
    fn poll_close(&mut self) -> PreparationCloseStatus {
        match self {
            Self::Cold(owner) => owner.poll_close(),
            #[cfg(feature = "mods")]
            Self::Ordinary(owner) => owner.poll_close(),
        }
    }
    fn retry_close(&mut self, deadline: Instant) -> Result<()> {
        match self {
            Self::Cold(owner) => owner.retry_close(deadline),
            #[cfg(feature = "mods")]
            Self::Ordinary(owner) => owner.retry_close(deadline),
        }
    }
    fn finish_close(self) -> std::result::Result<world_loading::PreparationClosed, Self> {
        match self {
            Self::Cold(owner) => owner.finish_close().map_err(Self::Cold),
            #[cfg(feature = "mods")]
            Self::Ordinary(owner) => owner.finish_close().map_err(Self::Ordinary),
        }
    }
}

fn close_preparation<T>(
    mut cold: ServerPreparation,
    commands: mpsc::Receiver<Command>,
    view: &Mutex<View>,
    _resources: T,
) -> std::result::Result<(), String> {
    let original = cold
        .original_error()
        .unwrap_or("original cold transfer refused")
        .to_owned();
    let mut attempt = 1;
    cold.begin_close(Instant::now() + CLOSE_WAIT);
    loop {
        let status = cold.poll_close();
        let blocked = match &status {
            PreparationCloseStatus::Blocked(detail) => Some(detail.clone()),
            _ => None,
        };
        *view.lock().unwrap_or_else(|e| e.into_inner()) = View {
            phase: "ColdClosing",
            attempt,
            original_error: Some(original.clone()),
            blocked: blocked.clone(),
            ..Default::default()
        };
        for command in commands.try_iter().take(16) {
            match command {
                Command::Retry(expected) if expected == attempt && blocked.is_some() => {
                    if cold.retry_close(Instant::now() + CLOSE_WAIT).is_ok() {
                        attempt += 1;
                    }
                }
                Command::Offer(reply) => {
                    let _ = reply.send(Err("original cold server is closing".into()));
                }
                _ => {}
            }
        }
        if status == PreparationCloseStatus::Complete {
            match cold.finish_close() {
                Ok(closed) => {
                    drop(closed);
                    return Err(original);
                }
                Err(owner) => cold = owner,
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

enum Phase<'a> {
    Running(Box<Game<'a>>),
    Transport(Box<Game<'a>>),
    Native(Box<CloseOwner<'a>>),
    Ready(NativeReady<'a>),
}

fn player_observations(game: &Game<'_>) -> Vec<serde_json::Value> {
    game.lan.as_ref().map(|h|h.hub.guests.iter().map(|g|serde_json::json!({
        "uuid":format!("{:032x}",g.uuid),"placed":g.placed,"loaded":g.loaded,"body":g.motion.is_some(),
        "position":g.position,"yaw":g.yaw,"awaiting_teleport":g.awaiting_teleport.is_some(),"inbox":g.inbox.len(),
        "items":g.inventory.iter().flatten().map(|s|u64::from(s.count)).sum::<u64>(),
        "creative_reach_modifiers":g.motion.as_ref().map_or(0,|body|body.attributes.instances().flat_map(|(_,_,value)|value.modifiers())
            .filter(|modifier|matches!(modifier.id(),"minecraft:creative_mode_block_range"|"minecraft:creative_mode_entity_range")).count()),
        "book_open":g.recipe_book.open[0],"book_filtering":g.recipe_book.filtering[0],"position":g.position})).collect()).unwrap_or_default()
}

/// Test observation only: no bootstrap, terrain edit, grant or evaluator call.
#[cfg(all(test, feature = "mods", target_os = "linux"))]
fn ordinary_counter_approach(
    game: &Game<'_>,
    guest: &horizon_client::lan::guest::Guest,
) -> Option<[f64; 3]> {
    let [x, y, z] = guest.position.map(|axis| axis.floor() as i32);
    if y + 6 >= 320 {
        return None;
    }
    let clear = |cx: i32, cz: i32, low: i32| {
        game.world
            .chunks
            .contains_key(&(cx.div_euclid(16), cz.div_euclid(16)))
            && (low..=y + 6).all(|cy| game.world.block(cx, cy, cz) == game.world.air)
    };
    if !clear(x, z, y) {
        return None;
    }
    let mut candidates = Vec::new();
    for dx in -8..=8_i32 {
        for dz in -8..=8_i32 {
            for dy in 1..=16 {
                let at = [x + dx, y - dy, z + dz];
                if game.validate_ordinary_stone_target(at, 1).is_ok()
                    && clear(at[0], at[2], at[1] + 1)
                    && (x.min(at[0])..=x.max(at[0])).all(|cx| clear(cx, z, y + 4))
                    && (z.min(at[2])..=z.max(at[2])).all(|cz| clear(at[0], cz, y + 4))
                {
                    candidates.push((dx * dx + dy * dy + dz * dz, at));
                }
            }
        }
    }
    candidates.sort_by_key(|(distance, at)| (*distance, *at));
    candidates.first().map(|(_, at)| {
        [
            f64::from(at[0]) + 0.5,
            f64::from(at[1]) + 3.0,
            f64::from(at[2]) + 0.5,
        ]
    })
}

#[cfg(all(test, feature = "mods", target_os = "linux"))]
fn ordinary_counter_observation(game: &Game<'_>) -> serde_json::Value {
    use horizon_client::world::counter_restore::GuestRuntimeState;
    if game
        .session_lock
        .as_ref()
        .and_then(|session| session.owned())
        .is_none()
        || !game
            .retained_server
            .as_ref()
            .is_some_and(|server| server.startup.is_some() && server.test.ordinary.is_none())
    {
        return serde_json::Value::Null;
    }
    let active = match game.world.guest_runtime_state() {
        Ok(GuestRuntimeState::ReadyActive { counter, slot }) => {
            let placement = counter.placement();
            serde_json::json!({"world":placement.world.to_string(),"dimension":placement.dimension,
                "position":placement.position,"generation":placement.generation,
                "value":game.world.read_persistent_counter(&counter).unwrap().counter(),
                "slot_incarnation":slot.incarnation(),
                "slot_empty":game.world.read_world_slot(&slot).unwrap().slot.stack().is_none(),
                "native":game.world.block(placement.position[0],placement.position[1],placement.position[2]).0})
        }
        _ => serde_json::Value::Null,
    };
    let host = game.lan.as_ref().unwrap();
    let mut players = host.hub.guests.iter().map(|guest| {
        let mut supports = Vec::new();
        let [x,y,z] = guest.position.map(|axis| axis.floor() as i32);
        for dy in 1..=4 {
            for dz in -2..=2 {
                for dx in -2..=2 {
                    let at = [x+dx,y-dy,z+dz];
                    if game.validate_ordinary_stone_target(at,1).is_ok() && guest.within_block_range(at,0.0) {
                        supports.push(at);
                    }
                }
            }
        }
        let approach=if supports.len()<2 {ordinary_counter_approach(game,guest)} else {None};
        serde_json::json!({"uuid":format!("{:032x}",guest.uuid),"supports":supports,"selected":guest.selected,
            "position":guest.position,"flying":guest.flying,"approach":approach,
            "inventory":guest.inventory.iter().map(|stack|stack.as_ref().map(|s|serde_json::json!({
                "name":s.item.as_ref(),"count":s.count,"damage":s.damage,"extra":format!("{:?}",s.extra)}))).collect::<Vec<_>>(),
            "carried_empty":guest.carried.is_none()})
    }).collect::<Vec<_>>();
    players.sort_by(|a, b| a["uuid"].as_str().unwrap().cmp(b["uuid"].as_str().unwrap()));
    serde_json::json!({"policy":game.world.has_registered_guest_placement_policy(),
        "domain":game.world.counter_domain_snapshot().map(|domain|domain.high_water),
        "mutating_control_absent":game.retained_server.as_ref().unwrap().test.ordinary.is_none(),
        "admission_controls":tests::ordinary_counter_admission_controls(game),
        "active":active,"players":players})
}

fn run_game<'a>(
    game: Box<Game<'a>>,
    commands: mpsc::Receiver<Command>,
    shared: &Mutex<View>,
    startup_error: Option<String>,
    paused: &AtomicBool,
) -> std::result::Result<(), String> {
    run_game_from(game, commands, shared, startup_error, paused, false, false)
}
fn run_game_from<'a>(
    game: Box<Game<'a>>,
    commands: mpsc::Receiver<Command>,
    shared: &Mutex<View>,
    startup_error: Option<String>,
    paused: &AtomicBool,
    mut settled: bool,
    mut listen_started: bool,
) -> std::result::Result<(), String> {
    #[cfg(test)]
    let controlled_published_close = game
        .retained_server
        .as_ref()
        .is_some_and(|server| server.test.close_owned_before_placement);
    #[cfg(not(test))]
    let controlled_published_close = false;
    #[cfg(test)]
    if controlled_published_close {
        assert!(
            game.session_lock
                .as_ref()
                .is_some_and(|session| session.owned().is_some()
                    && session.original_new_owned(&game.world_root).is_none())
        );
        assert!(game.native_close_authority.is_some());
        assert!(
            game.lan
                .as_ref()
                .unwrap()
                .hub
                .guests
                .iter()
                .all(|guest| guest.motion.is_some()
                    && !guest.placed
                    && !guest.loaded
                    && guest.awaiting_teleport.is_none())
        );
        eprintln!("ORDINARY_OWN_PUBLISHED_RECOVERY_CLOSE_BEFORE_PLACEMENT");
    }
    let mut phase = Some(Phase::Running(game));
    let mut view = View::default();
    let mut original_error = startup_error;
    let mut attempt = 0u64;
    let mut close = original_error.is_some() || controlled_published_close;
    let mut retry = false;
    let mut admission_error = None;
    let mut close_deadline = None;
    let mut transport_started = false;
    let mut next_tick = Instant::now();
    let settle_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for command in commands.try_iter().take(16) {
            match command {
                Command::Close => close = true,
                Command::Retry(expected) if expected == attempt && view.blocked.is_some() => {
                    retry = true
                }
                Command::Offer(reply) => {
                    let offered = match phase.as_mut().unwrap() {
                        Phase::Running(game) if settled && !close => game
                            .lan
                            .as_mut()
                            .unwrap()
                            .hub
                            .retained_connect_local()
                            .map_err(|e| e.to_string()),
                        _ => Err("original server is not admitting pipes".into()),
                    };
                    if let Err(mpsc::SendError(Ok(pipe))) = reply.send(offered) {
                        pipe.shutdown();
                        if let Phase::Running(game) = phase.as_mut().unwrap() {
                            let server = game.retained_server.as_mut().unwrap();
                            server.unsent_pipes.push(pipe);
                            server.fail("original offered Pipe reply was lost".into());
                        }
                        close = true;
                        original_error.get_or_insert("original offered Pipe reply was lost".into());
                    }
                }
                _ => {}
            }
        }
        if horizon_threads::stop_requests() > 0 {
            close = true;
        }
        let mut next = None;
        match phase.as_mut().unwrap() {
            Phase::Running(game) => {
                let pause = paused.load(Ordering::SeqCst);
                view.phase = if settled { "Running" } else { "Settling" };
                if !close {
                    let result = if !settled {
                        match game.retained_server_settle_step() {
                            Ok(true) => {
                                #[cfg(all(test, feature = "mods", target_os = "linux"))]
                                let bootstrap = game
                                    .retained_server
                                    .as_ref()
                                    .unwrap()
                                    .test
                                    .ordinary
                                    .clone()
                                    .map_or(Ok(()), |control| control.bootstrap(game));
                                #[cfg(not(all(test, feature = "mods", target_os = "linux")))]
                                let bootstrap: std::result::Result<
                                    (),
                                    String,
                                > = Ok(());
                                match bootstrap {
                                    Ok(()) => {
                                        settled = true;
                                        Ok(())
                                    }
                                    Err(error) => Err(error.into()),
                                }
                            }
                            Ok(false) if Instant::now() < settle_deadline => Ok(()),
                            Ok(false) => Err("original native server spawn did not settle".into()),
                            Err(error) => Err(error),
                        }
                    } else if !listen_started
                        && game
                            .retained_server
                            .as_ref()
                            .unwrap()
                            .integrated_owner()
                            .is_some()
                    {
                        // Memory-only integrated profile never calls listen:
                        // an absent bind address would otherwise mean wildcard.
                        listen_started = true;
                        Ok(())
                    } else if !listen_started {
                        match game.lan.as_mut().unwrap().hub.retained_listen() {
                            Ok(port) => {
                                view.port = port;
                                listen_started = true;
                                Ok(())
                            }
                            Err(error) => Err(error.into()),
                        }
                    } else if Instant::now() >= next_tick {
                        next_tick = Instant::now() + Duration::from_millis(50);
                        game.tick_server_timed(pause)
                            .and_then(|()| game.poll_ordinary_native_reads())
                    } else {
                        Ok(())
                    };
                    if let Err(error) = result {
                        let detail = error.to_string();
                        original_error.get_or_insert(detail.clone());
                        game.retained_server.as_mut().unwrap().fail(detail);
                        close = true;
                    }
                }
                if settled
                    && game
                        .session_lock
                        .as_ref()
                        .and_then(|session| session.owned())
                        .is_some()
                    && let Err(error) = game.publish_ordinary_owned_roster()
                {
                    original_error.get_or_insert(error.clone());
                    game.retained_server.as_mut().unwrap().fail(error);
                    close = true;
                }
                view.ready = settled && listen_started && !close;
                view.owned_editable = game
                    .session_lock
                    .as_ref()
                    .and_then(|session| session.owned())
                    .is_some()
                    && game
                        .retained_server
                        .as_ref()
                        .is_some_and(|server| server.checkpoint_roster.is_some());
                #[cfg(all(test, feature = "mods", target_os = "linux"))]
                if !close
                    && settled
                    && pause
                    && let Some(control) =
                        game.retained_server.as_ref().unwrap().test.ordinary.clone()
                    && let Err(error) = control.capture(game)
                {
                    original_error.get_or_insert(error.clone());
                    game.retained_server.as_mut().unwrap().fail(error);
                    close = true;
                }
                view.paused = pause;
                view.ticks = game.ticks;
                view.world_time = game.world.game_time;
                view.guests = game.lan.as_ref().map_or(0, |h| h.hub.guests.len());
                view.installed = game.retained_server.as_ref().unwrap().installations.len();
                view.owner_record = game
                    .retained_server
                    .as_ref()
                    .unwrap()
                    .integrated_owner()
                    .map(|(_, record)| record);
                view.owner_saved = game
                    .lan
                    .as_ref()
                    .and_then(|host| host.owner.as_ref())
                    .is_some_and(|owner| owner.saved.is_some());
                view.pending_events = game.retained_server.as_ref().unwrap().pending.len();
                view.transport = game
                    .lan
                    .as_ref()
                    .and_then(|host| host.hub.retained_close_view());
                view.players = player_observations(game);
                #[cfg(all(test, feature = "mods", target_os = "linux"))]
                {
                    view.ordinary_counter = ordinary_counter_observation(game);
                }
                #[cfg(test)]
                {
                    if game
                        .retained_server
                        .as_ref()
                        .unwrap()
                        .test
                        .refuse_live_consume
                        && !game
                            .retained_server
                            .as_ref()
                            .unwrap()
                            .consume_refusal_checked
                        && view.players.len() == 2
                        && view.players.iter().all(|g| g["loaded"] == true)
                    {
                        game.test_refuse_original_retained_host();
                    }
                    view.consume_refusal_checked = game
                        .retained_server
                        .as_ref()
                        .unwrap()
                        .consume_refusal_checked;
                }
                if close {
                    attempt = 1;
                    close_deadline = Some(Instant::now() + CLOSE_WAIT);
                    game.retained_server.as_mut().unwrap().freeze();
                    if let Err(error) = game.fence_retained_native_motion() {
                        original_error.get_or_insert(error.to_string());
                    }
                    if let Err(error) = game.freeze_ordinary_checkpoint_participants() {
                        original_error.get_or_insert(error.clone());
                        admission_error = Some(error);
                    }
                    next = Some(1);
                }
            }
            Phase::Transport(game) => {
                view.phase = "TransportClosing";
                view.ready = false;
                #[cfg(test)]
                let hold_frozen = game.retained_server.as_ref().unwrap().test.hold_frozen();
                #[cfg(not(test))]
                let hold_frozen = false;
                if hold_frozen {
                    view.phase = "Frozen";
                    view.transport = game
                        .lan
                        .as_ref()
                        .and_then(|host| host.hub.retained_close_view());
                } else {
                    if !transport_started {
                        if let Some(host) = &mut game.lan {
                            host.hub.begin_retained_close(
                                close_deadline
                                    .unwrap()
                                    .saturating_duration_since(Instant::now()),
                            );
                        }
                        transport_started = true;
                    }
                    if retry {
                        attempt += 1;
                        close_deadline = Some(Instant::now() + CLOSE_WAIT);
                        if let Some(host) = &mut game.lan
                            && let Some(hub) = host.hub.retained_close_view()
                            && hub.phase == RetainedHubPhase::Blocked
                        {
                            host.hub.retry_retained_close(hub.attempt, CLOSE_WAIT);
                        }
                        admission_error = game
                            .retry_retained_admissions()
                            .err()
                            .map(|e| e.to_string());
                        if admission_error.is_none() {
                            admission_error = game.freeze_ordinary_checkpoint_participants().err();
                        }
                        retry = false;
                    }
                    view.guests = game.lan.as_ref().map_or(0, |h| h.hub.guests.len());
                    view.installed = game.retained_server.as_ref().unwrap().installations.len();
                    view.pending_events = game.retained_server.as_ref().unwrap().pending.len();
                    view.players = player_observations(game);
                    let transport = game
                        .lan
                        .as_mut()
                        .and_then(|host| host.hub.poll_retained_close());
                    if let Some(transport) = &transport {
                        if let Some(error) = transport_failure(transport) {
                            original_error.get_or_insert(error);
                        }
                        view.transport = Some(transport.clone());
                    }
                    let transport_ready = transport
                        .as_ref()
                        .is_none_or(|status| status.phase == RetainedHubPhase::Ready);
                    let incomplete = game.retained_server.as_ref().unwrap().incomplete();
                    view.blocked = admission_error
                        .clone()
                        .or_else(|| {
                            incomplete.then(|| {
                                "original partial native admission requires exact Retry".into()
                            })
                        })
                        .or_else(|| {
                            transport
                                .as_ref()
                                .filter(|s| s.phase == RetainedHubPhase::Blocked)
                                .map(|_| "original transport close blocked".into())
                        });
                    if transport_ready && !incomplete && admission_error.is_none() {
                        #[cfg(test)]
                        if let Err(error) = game
                            .retained_server
                            .as_mut()
                            .unwrap()
                            .test
                            .inject_native_write_failure(&game.world_root)
                        {
                            admission_error = Some(error);
                            continue;
                        }
                        next = Some(2);
                    }
                }
            }
            Phase::Native(owner) => {
                view.phase = "NativeClosing";
                if retry {
                    let deadline = Instant::now() + CLOSE_WAIT;
                    if owner.retry(deadline).is_ok() {
                        attempt += 1;
                        close_deadline = Some(deadline);
                        view.blocked = None;
                    }
                    retry = false;
                }
                match owner.poll() {
                    CloseStatus::Pending(_) => view.blocked = None,
                    CloseStatus::Blocked(error) => {
                        original_error.get_or_insert(error.clone());
                        view.blocked = Some(error);
                    }
                    CloseStatus::NativeReady => next = Some(3),
                }
            }
            Phase::Ready(_) => {
                if retry {
                    attempt += 1;
                    view.blocked = None;
                    retry = false;
                }
                if view.blocked.is_none() {
                    next = Some(4);
                }
            }
        }
        if let Some(transition) = next {
            phase = Some(match (transition, phase.take().unwrap()) {
                (1, Phase::Running(game)) => Phase::Transport(game),
                (2, Phase::Transport(game)) => {
                    let profile = if game.ordinary_startup.is_some() {
                        game.ordinary_server_close_profile()
                    } else {
                        Ok(CloseProfile::NativeWorld)
                    };
                    match profile {
                        Ok(profile) => {
                            Phase::Native(CloseOwner::begin(game, profile, close_deadline.unwrap()))
                        }
                        Err(reason) => {
                            original_error.get_or_insert(reason.clone());
                            view.blocked = Some(reason);
                            Phase::Transport(game)
                        }
                    }
                }
                (3, Phase::Native(owner)) => match owner.try_native_ready() {
                    Ok(ready) => Phase::Ready(ready),
                    Err(owner) => Phase::Native(owner),
                },
                (4, Phase::Ready(ready)) => match ready.try_dispose_server() {
                    Ok(mut disposed) => {
                        if let Some(transport) = disposed.server_transport() {
                            if let Some(error) = transport_failure(transport) {
                                original_error.get_or_insert(error);
                            }
                            view.transport = Some(transport.clone());
                        }
                        *view
                            .ordinary_closed_world
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = disposed.take_ordinary_world();
                        drop(disposed);
                        view.phase = "NativeDisposed";
                        view.blocked = None;
                        view.attempt = attempt;
                        view.original_error = original_error.clone();
                        *shared.lock().unwrap_or_else(|e| e.into_inner()) = view;
                        return original_error.map_or(Ok(()), Err);
                    }
                    Err((ready, error)) => {
                        view.blocked = Some(error);
                        Phase::Ready(ready)
                    }
                },
                _ => unreachable!("fixed original server transition"),
            });
        }
        view.attempt = attempt;
        view.original_error = original_error.clone();
        *shared.lock().unwrap_or_else(|e| e.into_inner()) = view.clone();
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(super) fn run(options: Options, launch: Launch) -> Result<()> {
    run_configured(
        options,
        launch,
        #[cfg(test)]
        tests::Startup::default(),
    )
}

#[cfg(feature = "mods")]
pub(super) fn run_ordinary_dedicated(
    mut options: Options,
    mut settings: Settings,
    vanilla_root: PathBuf,
    started: Instant,
) -> Result<()> {
    let setup = dedicated::take_retained_setup()?;
    let started = started.min(setup.started);
    let (request, offline) = setup.retained_world_inputs(&options)?;
    let root = packs::startup_root(&vanilla_root, &settings.resource_packs, &[])?;
    let root = mods::content_root(
        &root,
        &settings.mods.enabled,
        &options.dev_mods,
        &options.mod_packages,
    )?;
    let loaded = load_resources(
        root,
        &vanilla_root,
        &settings,
        started,
        false,
        &Progress::new(),
        &mut |_, _| {},
    )?;
    if loaded.rest_loader.is_some() {
        eprintln!("fatal: synchronous dedicated generation acquired an unexpected helper");
        std::process::abort();
    }
    if loaded.pack_error.is_some() {
        return Err("paired dedicated startup refuses resource fallback".into());
    }
    options.resources = loaded.root.clone();
    options.server_thread = false;
    let cpu = GameCpuResources::load(&loaded.root)?;
    let tree = Arc::new(horizon_client::advancements::Tree::load(
        &loaded.root.join("data"),
    ));
    let startup = Arc::new(
        options
            .mod_packages
            .freeze_peer_inputs(
                Arc::new(horizon_client::lan::data::SyncData::load(&loaded.root)?),
                loaded.pack.assets.registry,
            )?
            .ok_or("dedicated startup lost selected pair")?,
    );
    settings.render_distance = setup.props.view_distance;
    settings.simulation_distance = setup.props.simulation_distance;
    let endpoint = options.control.clone();
    let timeout = if options.frames.is_some() {
        options.timeout
    } else {
        Duration::MAX
    };
    let selection = request.into_ordinary_network(
        &std::env::current_dir()?,
        None,
        Instant::now() + Duration::from_secs(120),
    )?;
    let input = OrdinaryServerInput {
        options,
        settings,
        request: selection.request,
        entry: RetainedEntryIdentity {
            token: 1,
            owner: Arc::new(()),
        },
        resources: loaded.resources,
        pack: loaded.pack,
        gui: loaded.gui.ok_or("dedicated GUI/font bundle missing")?,
        cpu,
        tree,
        startup,
        offline,
        dedicated_setup: Some(setup),
        continuation_archive: None,
        #[cfg(test)]
        test: tests::Startup::default(),
    };
    let mut supervisor = match Supervisor::prepare_ordinary(input) {
        Ok(supervisor) => supervisor,
        Err(original) => return Err(original.reason.clone().into()),
    };
    if let Some(endpoint) = endpoint {
        return run_parent(
            supervisor,
            RetainedControl::prepare(&endpoint),
            timeout,
            #[cfg(test)]
            tests::Startup::default(),
        );
    }
    let mut stop_sent = false;
    let mut last_blocked = None;
    loop {
        if !stop_sent && horizon_threads::stop_requests() > 0 {
            supervisor.request_close();
            stop_sent = true;
        }
        let state = supervisor.snapshot();
        if state.blocked != last_blocked {
            if let Some(reason) = &state.blocked {
                log::error!(
                    "ordinary dedicated closure retains attempt {}: {reason}",
                    state.attempt
                );
            }
            last_blocked = state.blocked;
        }
        if let Some(result) = supervisor.join() {
            return result.map_err(Into::into);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_configured(
    options: Options,
    launch: Launch,
    #[cfg(test)] test: tests::Startup,
) -> Result<()> {
    horizon_threads::catch_retained_stop_signals();
    let mut settings = Settings::load(&std::env::current_dir()?.join("options.txt"));
    if options.server.is_some()
        || options.dedicated.is_some()
        || options.dimension.is_some()
        || options.server_thread
        || !options.test_terrain
        || !options.dev_mods.is_empty()
        || options.mod_packages.catalog().is_some()
        || !settings.mods.enabled.is_empty()
        || !settings.resource_packs.is_empty()
        || options.timeout > Duration::from_secs(120)
        || settings.extras.no_ai
        || settings.extras.fullbright
        || settings.extras.dynamic_lights
        || settings.extras.zoom
        || settings.extras.fps
        || settings.extras.keystrokes
        || settings.extras.minimap
        || settings.extras.auto_reconnect
        || settings.extras.mouse_tweaks
        || settings.extras.sort
    {
        return Err(
            "fixed retained server refuses unsupported options, selected content or extras".into(),
        );
    }
    settings.render_distance = 2;
    settings.simulation_distance = 2;
    settings.horizon.lod_distance = 0;
    let timeout = options.timeout;
    let control_path = options
        .control
        .clone()
        .ok_or("retained server control path missing")?;
    if !control_path.is_absolute() || control_path.parent() != Some(launch.root.as_path()) {
        return Err(
            "retained server endpoint must be directly inside its fresh absolute application root"
                .into(),
        );
    }
    std::fs::create_dir(&launch.root)?;
    let mut control = Some(RetainedControl::prepare(&control_path));
    #[cfg(test)]
    let final_test = test.clone();
    let supervisor = match Supervisor::prepare(Input {
        options,
        settings,
        launch,
        owner: None,
        #[cfg(test)]
        test,
    }) {
        Ok(supervisor) => supervisor,
        Err((_input, error)) => {
            let mut owner = control.take().unwrap();
            owner.begin_close(Instant::now() + CLOSE_WAIT);
            loop {
                if owner.poll_close().observation.native_quiescent {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let _closed = owner
                .try_closed()
                .unwrap_or_else(|_| unreachable!("unstarted retained endpoint"));
            return Err(error.into());
        }
    };
    run_parent(
        supervisor,
        control.take().unwrap(),
        timeout,
        #[cfg(test)]
        final_test,
    )
}

fn run_parent(
    mut supervisor: Supervisor,
    controller: RetainedControl,
    timeout: Duration,
    #[cfg(test)] final_test: tests::Startup,
) -> Result<()> {
    let mut control = Some(controller);
    let mut original_error = control.as_mut().unwrap().start().err();
    if original_error.is_some() {
        let _ = supervisor.commands.send(Command::Close);
    }
    let started = Instant::now();
    let mut timeout_sent = false;
    let mut control_stop_sent = original_error.is_some();
    let result = loop {
        let control_observation = control.as_ref().unwrap().observation();
        if let Some(failure) = control_observation.failures.first() {
            original_error
                .get_or_insert_with(|| format!("original retained control failure: {failure:?}"));
            if !control_stop_sent {
                control_stop_sent = true;
                let _ = supervisor.commands.send(Command::Close);
            }
        }
        if !timeout_sent && (started.elapsed() >= timeout || horizon_threads::stop_requests() > 0) {
            timeout_sent = true;
            let _ = supervisor.commands.send(Command::Close);
        }
        for _ in 0..16 {
            let Some(request) = control.as_mut().unwrap().try_recv() else {
                break;
            };
            let words = request.line.split_whitespace().collect::<Vec<_>>();
            let state = supervisor.snapshot();
            let reply = match words.as_slice() {
                ["status"] => {
                    serde_json::json!({"state":state.json(),"parent_error":original_error,
                    "control_phase":format!("{:?}",control_observation.phase),"control_endpoint":format!("{:?}",control_observation.endpoint),
                    "control_failures":control_observation.failures.iter().map(|failure|format!("{failure:?}")).collect::<Vec<_>>()})
                }
                ["close"] | ["quit"] => {
                    serde_json::json!({"accepted":supervisor.commands.send(Command::Close).is_ok(),"state":state.json()})
                }
                ["retry", attempt] => match attempt.parse::<u64>() {
                    Ok(expected) if state.blocked.is_some() && state.attempt == expected => {
                        serde_json::json!({"accepted":supervisor.commands.send(Command::Retry(expected)).is_ok(),"attempt":expected})
                    }
                    _ => {
                        serde_json::json!({"error":"Retry requires the exact current blocked attempt","state":state.json()})
                    }
                },
                _ => {
                    serde_json::json!({"error":"fixed retained server commands: status, close, retry ATTEMPT, quit"})
                }
            };
            let _ = request.reply.send(reply.to_string());
        }
        if let Some(result) = supervisor.join() {
            break result;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    if let Err(error) = result {
        original_error.get_or_insert(error);
    }
    // Finish every response handed to this parent using actual native delivery
    // counters, not response-channel enqueue. New requests still get completed
    // status while the original endpoint and joined worker result remain owned.
    let response_deadline = Instant::now() + CLOSE_WAIT;
    let mut refused_responses = Vec::new();
    loop {
        let before = control.as_ref().unwrap().observation();
        #[cfg(test)]
        if final_test.hold_parent_final(before.queued_requests) {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        for _ in 0..16 {
            let Some(request) = control.as_mut().unwrap().try_recv() else {
                break;
            };
            let reply = serde_json::json!({"state":supervisor.snapshot().json(),"parent_error":original_error,"worker_completed":true,
                "response_delivery":{"required":before.handed_off_requests,"delivered":before.delivered_replies,
                    "blocked":Instant::now()>=response_deadline}});
            if let Err(mpsc::SendError(original)) = request.reply.send(reply.to_string()) {
                refused_responses.push(original);
                original_error
                    .get_or_insert("original final control response channel refused".into());
            }
        }
        let after = control.as_ref().unwrap().observation();
        if let Some(failure) = after.failures.first() {
            original_error
                .get_or_insert_with(|| format!("original retained control failure: {failure:?}"));
            break;
        }
        if !refused_responses.is_empty()
            || (after.queued_requests == 0 && after.delivered_replies >= after.handed_off_requests)
        {
            break;
        }
        if Instant::now() >= response_deadline {
            // No forced discard or deadline reset. Keep serving completed
            // status while a native writer is still pending; its real delivery
            // or concrete failure must resolve before endpoint sealing.
            original_error.get_or_insert(
                "original final control response delivery exceeded its attempt".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // The original worker and every native world/transport actor have completed
    // before sealing the still-owned actual control endpoint.
    let mut owner = control.take().unwrap();
    owner.begin_close(Instant::now() + CLOSE_WAIT);
    loop {
        let status = owner.poll_close();
        if status.observation.native_quiescent {
            break;
        }
        if status.observation.phase == RetainedControlPhase::Blocked {
            // Keep the same absolute attempt and actual endpoint. Finished
            // native joins may still establish quiescence after the deadline.
            original_error
                .get_or_insert("original retained control cleanup exceeded its attempt".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let closed = match owner.try_closed() {
        Ok(closed) => closed,
        Err(_) => unreachable!("checked native endpoint completion"),
    };
    if let Some(failure) = closed.observation().failures.first() {
        original_error
            .get_or_insert_with(|| format!("original retained control failure: {failure:?}"));
    }
    drop(supervisor);
    drop(closed);
    drop(refused_responses);
    original_error.map_or(Ok(()), |error| Err(error.into()))
}
