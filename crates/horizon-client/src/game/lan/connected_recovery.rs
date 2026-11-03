//! Dormant, synchronous two-profile recovery. No ordinary lifecycle caller.
#![allow(dead_code)]
use super::*;
use crate::game::levels::session_checkpoint::{
    CheckpointReceipt, ManagedRecoveryRequest, NativeRecoveryParts, Publication, RecoveryRequest,
    next_session_checkpoint_epoch,
};
use crate::game::world_loading::host_identity::session_authority::AuthorityNamespace;
use horizon_protocol::connection::Pipe;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub(in crate::game) enum State {
    Joining,
    Admitted,
    Loading,
    ReadyFrozen,
    RetainedFailure {
        attempt: u64,
        publication: Publication,
        detail: String,
    },
    Refusing(String),
    CleanupBlocked(String),
    Closing(CheckpointReceipt),
}
#[derive(Clone, Copy)]
pub(in crate::game) enum Command {
    Wait,
    Close { attempt: u64 },
    RetryClose { attempt: u64, failed_attempt: u64 },
}
#[derive(Clone, Debug)]
pub(in crate::game) struct Observation {
    pub(in crate::game) state: State,
    pub(in crate::game) world_time: i64,
    pub(in crate::game) day_time: i64,
    pub(in crate::game) hub_ticks: u64,
    pub(in crate::game) players: std::result::Result<Vec<SessionPlayerRow>, String>,
    pub(in crate::game) counter:
        std::result::Result<horizon_client::world::counter_restore::CounterFrameV2, String>,
    pub(in crate::game) slot_bytes: std::result::Result<Vec<u8>, String>,
    pub(in crate::game) counter_key: horizon_client::world::counter_restore::RuntimeCounterKey,
    pub(in crate::game) slot_key: horizon_client::world_slot_trial::SlotKey,
    pub(in crate::game) live_keys: std::result::Result<(), String>,
    pub(in crate::game) guests: usize,
    pub(in crate::game) play_entries: usize,
    pub(in crate::game) pending_transport_bytes: usize,
    pub(in crate::game) attempts: usize,
    pub(in crate::game) last_refusal: Option<&'static str>,
}
pub(in crate::game) enum End {
    Closed(CheckpointReceipt),
    Refused(String),
}
#[cfg(test)]
thread_local! {
    static CLOSE_FAULT: std::cell::Cell<Option<crate::game::levels::session_checkpoint::CheckpointTestFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(in crate::game) fn set_close_fault(
    fault: Option<crate::game::levels::session_checkpoint::CheckpointTestFault>,
) {
    CLOSE_FAULT.with(|slot| slot.set(fault));
}
/// This frame is never returned to callers. An unexpected unwind cannot drop a
/// live native owner or detach connection threads. There is no joining in Drop.
struct OwnedRecovery<'a> {
    parts: NativeRecoveryParts<'a>,
    detached: Option<LanHost>,
    state: State,
    armed: bool,
    installed: bool,
    last_attempt: u64,
    last_refusal: Option<&'static str>,
}
impl Drop for OwnedRecovery<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::panic::catch_unwind(|| {
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "CONNECTED_RECOVERY_UNRESOLVED_FATAL");
            });
            std::process::abort();
        }
    }
}
impl OwnedRecovery<'_> {
    fn host(&mut self) -> &mut LanHost {
        match self.detached.as_mut() {
            Some(host) => host,
            None => self
                .parts
                .game_mut()
                .lan
                .as_mut()
                .expect("owned attached recovery host"),
        }
    }
    fn observation(&mut self) -> Observation {
        let state = self.state.clone();
        let last_refusal = self.last_refusal;
        let (counter_key, slot_key) = self.parts.runtime_keys();
        let live_keys = self
            .parts
            .validate_runtime_keys()
            .map_err(|error| error.to_string());
        let (world_time, day_time, counter, slot_bytes) = {
            let game = self.parts.game_mut();
            (
                game.world.game_time,
                game.world.day_time,
                game.world
                    .counter_checkpoint_frame(game.world.game_time)
                    .map_err(|e| e.to_string()),
                game.world
                    .slot_checkpoint_bytes(game.res.items(), game.world.game_time)
                    .map_err(|e| e.to_string()),
            )
        };
        let (hub_ticks, pending_transport_bytes, attempts, guests, play_entries) = {
            let host = self.host();
            let status = host
                .hub
                .lan
                .legacy()
                .expect("owned staged legacy transport")
                .staged_status()
                .expect("owned staged transport");
            (
                host.hub.ticks,
                status.pending_outbound,
                status.attempts,
                host.hub.guests.len(),
                host.play.len(),
            )
        };
        let players = if self.installed {
            let game = self.parts.game_mut();
            game.observe_checkpoint_participants()
                .map_err(|error| format!("{error:?}"))
                .and_then(|roster| {
                    game.project_session_participant_rows(&roster)
                        .map_err(|error| error.to_string())
                })
        } else {
            Ok(Vec::new())
        };
        Observation {
            state,
            world_time,
            day_time,
            hub_ticks,
            players,
            counter,
            slot_bytes,
            counter_key,
            slot_key,
            live_keys,
            guests,
            play_entries,
            pending_transport_bytes,
            attempts,
            last_refusal,
        }
    }
    fn pump(&mut self) -> Result<()> {
        if !self.installed {
            let (game, checked) = self.parts.game_and_players();
            let host = self
                .detached
                .as_mut()
                .ok_or("missing detached recovery host")?;
            poll_frozen(game, host, None)?;
            if host.hub.guests.len() != 2 {
                return Ok(());
            }
            if matches!(self.state, State::Joining) {
                self.state = State::Admitted;
                return Ok(());
            }
            let roster = host.hub.staged_fresh_roster()?;
            game.install_staged_session_participants(host, checked, &roster)?;
            poll_frozen(game, host, Some(&roster))?;
            game.lan = self.detached.take();
            self.installed = true;
            self.state = State::Loading;
        } else {
            let game = self.parts.game_mut();
            let mut host = game.lan.take().ok_or("missing attached recovery host")?;
            let result = poll_frozen(game, &mut host, None);
            game.lan = Some(host);
            result?;
        }
        let ready = {
            let game = self.parts.game_mut();
            let host = game.lan.as_mut().ok_or("missing installed staged host")?;
            host.hub.pump_staged_recovery(&game.world)?
        };
        if ready && matches!(self.state, State::Loading) {
            let (game, checked) = self.parts.game_and_players();
            game.check_installed_session_participants(checked)?;
            self.parts.verify_final().map_err(|e| e.to_string())?;
            self.state = State::ReadyFrozen;
        }
        Ok(())
    }
    fn begin_refusal(&mut self, detail: String) {
        self.state = State::Refusing(detail);
        let _ = self
            .host()
            .hub
            .lan
            .legacy()
            .expect("legacy host factory")
            .close_staged();
    }
    fn close_attempt(
        &mut self,
        attempt: u64,
        failed: Option<u64>,
        archive: &Path,
        timeout: Duration,
    ) {
        let admissible = attempt != 0
            && attempt > self.last_attempt
            && match (&self.state, failed) {
                (State::ReadyFrozen, None) => true,
                (
                    State::RetainedFailure {
                        attempt: previous, ..
                    },
                    Some(failed),
                ) => *previous == failed,
                _ => false,
            };
        if !admissible {
            self.last_refusal = Some("invalid state or stale close attempt");
            return;
        }
        if let Err(error) = self.pump() {
            self.state =
                State::CleanupBlocked(format!("ready owner retained before close: {error}"));
            return;
        }
        self.last_attempt = attempt;
        self.last_refusal = None;
        let result = next_session_checkpoint_epoch(archive).and_then(|epoch| {
            #[cfg(test)]
            if let Some(fault) = CLOSE_FAULT.with(|fault| fault.take()) {
                return self.parts.game_mut().checkpoint_session_with_test_fault(
                    archive,
                    epoch,
                    Instant::now() + timeout,
                    fault,
                );
            }
            self.parts
                .game_mut()
                .checkpoint_session(archive, epoch, Instant::now() + timeout)
        });
        match result {
            Ok(receipt) => match self
                .host()
                .hub
                .lan
                .legacy()
                .expect("legacy host factory")
                .close_staged_if_live()
            {
                Ok(()) => self.state = State::Closing(receipt),
                Err(error) => {
                    self.state = State::CleanupBlocked(format!(
                        "published epoch {} retained after departure before close: {error}",
                        receipt.epoch
                    ))
                }
            },
            Err(error) => {
                self.state = State::RetainedFailure {
                    attempt,
                    publication: error.publication,
                    detail: error.detail,
                }
            }
        }
    }
}

fn poll_frozen(
    game: &mut Game<'_>,
    host: &mut LanHost,
    place: Option<&horizon_client::lan::hub::StagedRoster>,
) -> Result<()> {
    let clock_rate = game.network_clock_rate();
    let mut ctx = TickCtx {
        world: &mut game.world,
        host: None,
        entities: &[],
        next_entity: &mut game.next_entity,
        spawn: [0.0, 120.0, 0.0],
        difficulty: i32::from(game.ui.difficulty),
        difficulty_locked: game.level.difficulty_locked,
        clock_rate,
        tick_rate: game.clock.rate(),
        tick_frozen: true,
        tick_steps: (0, 0),
        collision: Some(game.collision),
        border: &game.border,
    };
    match place {
        Some(roster) => host.hub.place_staged_recovery(roster, &mut ctx)?,
        None => {
            host.hub.poll_staged_recovery(&mut ctx)?;
        }
    }
    Ok(())
}

fn real_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err("absolute nontraversing directory required".into());
    }
    for at in path.ancestors() {
        let meta = fs::symlink_metadata(at)?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = false;
        if !meta.is_dir() || meta.file_type().is_symlink() || reparse {
            return Err("real unlinked directory required".into());
        }
    }
    Ok(fs::canonicalize(path)?)
}

/// The client driver sees immutable observations and the two owned initial pipes,
/// never a Game, host, transport handle, tick entry or arbitrary game callback.
pub(in crate::game) fn recover_connected<'a>(
    candidate: Box<Game<'a>>,
    namespace: &AuthorityNamespace,
    request: ManagedRecoveryRequest,
    continuation: &Path,
    timeout: Duration,
    mut driver: impl FnMut(Observation, Option<[Pipe; 2]>) -> Command,
) -> Result<End> {
    let archive = real_directory(&request.archive_root)?;
    let continuation = real_directory(continuation)?;
    namespace.check_new_world(&continuation)?;
    let owner = real_directory(&request.owner_root)?;
    let destination_parent =
        real_directory(request.destination.parent().ok_or("destination parent")?)?;
    let destination =
        destination_parent.join(request.destination.file_name().ok_or("destination name")?);
    if timeout.is_zero()
        || timeout > Duration::from_secs(120)
        || fs::read_dir(&continuation)?.next().is_some()
        || [&archive, &owner, &destination]
            .iter()
            .any(|p| continuation.starts_with(p) || p.starts_with(&continuation))
    {
        return Err("fresh separate continuation archive and bounded timeout required".into());
    }
    let mut lease = Some(namespace.acquire_recovery(&owner, &archive, &destination)?);
    let expected_world = lease.as_ref().ok_or("missing recovery authority")?.world();
    let startup_deadline = request.deadline;
    let mut parts = candidate.prepare_session_checkpoint(RecoveryRequest {
        expected_world,
        expected_target: request.expected_target,
        expected_type: request.expected_type,
        archive_root: archive,
        destination,
        deadline: startup_deadline,
    })?;
    parts.install_authority(&mut lease)?;
    let profiles = lan::StagedProfiles::memory_pair();
    let owner_uuid = profiles.profiles()[0].0;
    let rows = parts.players().rows();
    if rows.len() != 2
        || rows.iter().filter(|r| r.integrated_owner).count() != 1
        || !profiles
            .profiles()
            .iter()
            .enumerate()
            .all(|(i, (uuid, _))| {
                rows.iter().any(|r| {
                    r.profile_uuid == *uuid
                        && r.integrated_owner == (i == 0)
                        && r.dimension == super::super::Dim::Overworld.id()
                })
            })
    {
        return Err("verified roster must match the fixed owner and guest profiles".into());
    }
    let game = parts.game_mut();
    let mut net = NetConfig::lan(0, game.level.name.clone(), 2);
    net.listen = false;
    net.announce = false;
    let config = HubConfig {
        default_game_mode: 1,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: super::super::Dim::Overworld.id().into(),
        seed: game.level.seed,
        max_players: 2,
        dedicated: true,
        integrated: true,
    };
    let mut host = game.build_detached_lan_host(
        LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false,
        },
        net,
        config,
        None,
        |sync, config| Lan::start_staged(sync, config, profiles),
    )?;
    host.owner = Some(Owner {
        uuid: owner_uuid,
        saved: None,
    });
    host.hub.singleplayer_owner = Some(owner_uuid);
    host.hub.begin_staged_recovery()?;
    let mut owned = OwnedRecovery {
        parts,
        detached: Some(host),
        state: State::Joining,
        armed: true,
        installed: false,
        last_attempt: 0,
        last_refusal: None,
    };
    let pipes = (|| -> Result<[Pipe; 2]> {
        let first = owned
            .host()
            .hub
            .lan
            .legacy()
            .expect("legacy host factory")
            .connect_local_staged()?;
        let second = owned
            .host()
            .hub
            .lan
            .legacy()
            .expect("legacy host factory")
            .connect_local_staged()?;
        Ok([first, second])
    })();
    let mut offers = match pipes {
        Ok(pipes) => Some(pipes),
        Err(error) => {
            owned.begin_refusal(error.to_string());
            None
        }
    };
    // Bound the actual admission window too; native preparation remains bounded
    // by the original request deadline and cannot borrow extra setup time.
    let startup_deadline = startup_deadline.min(Instant::now() + timeout);
    let mut cleanup_deadline = None;
    loop {
        if matches!(
            owned.state,
            State::Joining
                | State::Admitted
                | State::Loading
                | State::ReadyFrozen
                | State::RetainedFailure { .. }
        ) {
            if let Err(error) = owned.pump() {
                if matches!(
                    owned.state,
                    State::Joining | State::Admitted | State::Loading
                ) {
                    owned.begin_refusal(error.to_string());
                } else {
                    // After Ready no incomplete roster can authorize release.
                    owned.state = State::CleanupBlocked(format!(
                        "ready owner retained after transport refusal: {error}"
                    ));
                }
            }
            if Instant::now() >= startup_deadline
                && matches!(
                    owned.state,
                    State::Joining | State::Admitted | State::Loading
                )
            {
                owned.begin_refusal("startup deadline".into());
            }
        }
        let command = driver(owned.observation(), offers.take());
        match command {
            Command::Wait => {}
            Command::Close { attempt } => {
                owned.close_attempt(attempt, None, &continuation, timeout)
            }
            Command::RetryClose {
                attempt,
                failed_attempt,
            } => owned.close_attempt(attempt, Some(failed_attempt), &continuation, timeout),
        }
        if matches!(owned.state, State::Refusing(_) | State::Closing(_)) {
            let until = *cleanup_deadline.get_or_insert_with(|| Instant::now() + timeout);
            if owned
                .host()
                .hub
                .lan
                .legacy()
                .expect("legacy host factory")
                .staged_finished()
            {
                match owned
                    .host()
                    .hub
                    .lan
                    .legacy()
                    .expect("legacy host factory")
                    .join_staged_finished()
                {
                    Ok(()) => {
                        let result = match owned.state.clone() {
                            State::Closing(receipt) => {
                                owned
                                    .parts
                                    .game_mut()
                                    .retire_all_counter_trials("connected-confirmed-stop");
                                End::Closed(receipt)
                            }
                            State::Refusing(reason) => End::Refused(reason),
                            _ => unreachable!(),
                        };
                        owned.armed = false;
                        return Ok(result);
                    }
                    Err(error) => {
                        owned.state =
                            State::CleanupBlocked(format!("connection join refused: {error:?}"))
                    }
                }
            } else if Instant::now() >= until {
                owned.state = State::CleanupBlocked(
                    "native connection cleanup deadline; owner retained".into(),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "connected_recovery_tests.rs"]
pub(in crate::game) mod tests;
