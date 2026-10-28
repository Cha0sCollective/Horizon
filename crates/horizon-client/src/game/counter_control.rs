//! Explicit transient native/isolated-guest counter controls. No inventory or persistence.
use super::Game;
#[cfg(feature = "mods")]
use horizon_client::counter_trial::PhaseToken;
use horizon_client::{
    Result,
    counter_trial::{
        Backend, Completion, Configuration, GuestFailure, PhaseError, Proposal, Record, Snapshot,
    },
    mutation_observer::AuthorityScope,
    world::World,
};
use horizon_mods::content::machines::MachineType;
#[cfg(feature = "mods")]
use horizon_mods::{CounterProposal, CounterRequest};
use horizon_world::{chunk::BlockState, registry::BlockStateId};
use serde_json::{Value, json};
use std::{io::Write, sync::Arc};

fn package_hex(identity: &[u8; 32]) -> String {
    identity.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn clipped_detail(detail: &str) -> String {
    detail.chars().take(512).collect()
}

#[cfg(feature = "mods")]
fn counter_request(token: &PhaseToken) -> CounterRequest {
    let config = token.configuration();
    let record = token.record();
    let [x, y, z] = config.position();
    CounterRequest {
        world_instance: config.scope().world_instance(),
        dimension: config.scope().dimension().to_owned(),
        type_id: config.machine_type().id().to_owned(),
        schema: config.machine_type().save_schema(),
        x,
        y,
        z,
        instance: record.instance(),
        counter: record.counter(),
        game_time: token.game_time(),
    }
}

#[cfg(feature = "mods")]
fn host_proposal(proposal: CounterProposal) -> Proposal {
    Proposal {
        world_instance: proposal.world_instance,
        instance: proposal.instance,
        game_time: proposal.game_time,
        expected_counter: proposal.expected_counter,
        next_counter: proposal.next_counter,
    }
}

#[cfg(feature = "mods")]
fn guest_receipts_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("HORIZON_COUNTER_GUEST_RECEIPTS").is_some_and(|value| value == "1")
    })
}

#[cfg(feature = "mods")]
fn guest_receipt(
    identity: &[u8; 32],
    request: &CounterRequest,
    proposal: Option<Proposal>,
    outcome: &str,
    error: Option<&str>,
) -> Value {
    json!({
        "package_identity": package_hex(identity),
        "scope": { "world_instance": request.world_instance, "dimension": request.dimension },
        "type": request.type_id, "instance": request.instance, "game_time": request.game_time,
        "request": { "world_instance": request.world_instance, "dimension": request.dimension,
            "type_id": request.type_id, "schema": request.schema,
            "x": request.x, "y": request.y, "z": request.z,
            "instance": request.instance, "counter": request.counter, "game_time": request.game_time },
        "proposal": proposal.map(|p| json!({ "world_instance": p.world_instance, "instance": p.instance,
            "game_time": p.game_time, "expected_counter": p.expected_counter, "next_counter": p.next_counter })),
        "outcome": outcome, "error": error.map(clipped_detail),
    })
}

#[cfg(feature = "mods")]
fn emit_guest_receipt(receipt: Value) {
    const PREFIX: &str = "HORIZON_COUNTER_GUEST";
    let encoded = receipt.to_string();
    // Validated identifier bounds and clipped details fit; never write an oversized line.
    if encoded.len() + PREFIX.len() + 2 <= 8 * 1024 {
        let _ = writeln!(std::io::stderr().lock(), "{PREFIX} {encoded}");
    }
}

fn log_new_guest_fault(
    was_faulted: bool,
    result: &std::result::Result<Completion, PhaseError>,
    detail: Option<&str>,
) {
    if !was_faulted && let Err(PhaseError::Fault(fault)) = result {
        if let Some(detail) = detail {
            log::error!("Horizon: {fault}: {}", clipped_detail(detail));
        } else {
            log::error!("Horizon: {fault}");
        }
    }
}

fn record_json(record: Record) -> Value {
    json!({ "instance": record.instance(), "counter": record.counter(),
        "last_advanced": record.last_advanced(), "resident": record.resident() })
}

fn snapshot_json(snapshot: &Snapshot) -> Value {
    let config = snapshot.configuration();
    let mut value = json!({
        "scope": { "world_instance": config.scope().world_instance(),
            "dimension": config.scope().dimension() },
        "type": config.machine_type().id(), "schema": config.machine_type().save_schema(),
        "position": config.position(),
        "state": { "name": config.state().name, "properties": config.state().properties },
        "record": snapshot.record().map(record_json),
        "last_retirement": snapshot.last_retirement().map(|r| json!({
            "record": record_json(r.record()), "reason": format!("{:?}", r.reason()),
            "game_time": r.game_time(),
        })),
        "fault": snapshot.fault().map(|f| f.to_string()),
        "persistence": "disabled-transient-trial",
    });
    if let Backend::Guest {
        primary_owner,
        package_identity,
    } = config.backend()
    {
        value["backend"] = json!({ "kind": "guest", "owner": primary_owner,
            "package_identity": package_hex(package_identity) });
    }
    value
}

/// The diagnostic cannot mask a save failure if its output sink is unavailable.
pub(super) fn retire_world_counter(world: &mut World, boundary: &str) {
    if let Some(snapshot) = world.take_counter_trial() {
        let receipt = json!({ "boundary": boundary, "trial": snapshot_json(&snapshot) });
        let _ = writeln!(std::io::stderr().lock(), "HORIZON_COUNTER_RETIRE {receipt}");
    }
}

impl Game<'_> {
    fn counter_position_eligibility(&self, position: [i32; 3]) -> (bool, bool) {
        let (x, z) = (position[0] >> 4, position[2] >> 4);
        (
            self.tickets.levels().block_ticking(x, z),
            super::ticking_chunk(&self.world, x, z),
        )
    }

    pub(super) fn tick_counter_trial(&mut self, running: bool) {
        if self.remote.is_some() || self.integrated.is_some() {
            return;
        }
        let Some(before) = self.world.counter_trial_snapshot() else {
            return;
        };
        let (ticket, storage) =
            self.counter_position_eligibility(before.configuration().position());
        if matches!(before.configuration().backend(), Backend::Guest { .. }) {
            self.tick_guest_counter_trial(running, ticket && storage, before.fault().is_some());
            return;
        }
        let result = self.world.advance_counter_trial(running, ticket && storage);
        if before.fault().is_none()
            && let Some(Err(fault)) = result
        {
            log::error!("Horizon: {fault}");
        }
    }

    fn tick_guest_counter_trial(&mut self, running: bool, eligible: bool, was_faulted: bool) {
        let token = match self.world.prepare_guest_counter_phase(running, eligible) {
            Ok(Some(token)) => token,
            Ok(None) => return,
            Err(error) => {
                log_new_guest_fault(was_faulted, &Err(error), None);
                return;
            }
        };
        let position = token.configuration().position();
        #[cfg(feature = "mods")]
        let (result, detail, receipt) = {
            let Backend::Guest {
                primary_owner,
                package_identity,
            } = token.configuration().backend()
            else {
                return;
            };
            match self
                .mods
                .mods
                .counter_evaluator(primary_owner, package_identity)
            {
                Err(error) => (
                    Err(GuestFailure::Unavailable),
                    Some(clipped_detail(&error)),
                    None,
                ),
                Ok(evaluator) => {
                    let request = counter_request(&token);
                    let evaluated = evaluator.evaluate(&request);
                    let proposal = evaluated.as_ref().ok().copied().map(host_proposal);
                    let detail = evaluated.as_ref().err().map(|error| clipped_detail(error));
                    let receipt =
                        guest_receipts_enabled().then_some((*package_identity, request, proposal));
                    (
                        evaluated
                            .map(host_proposal)
                            .map_err(|_| GuestFailure::Evaluation),
                        detail,
                        receipt,
                    )
                }
            }
        };
        #[cfg(not(feature = "mods"))]
        let (result, detail): (
            std::result::Result<Proposal, GuestFailure>,
            Option<String>,
        ) = (
            Err(GuestFailure::Unavailable),
            Some("counter guest needs the mods runtime".into()),
        );
        let (ticket, storage) = self.counter_position_eligibility(position);
        let completed = match result {
            Ok(proposal) => {
                self.world
                    .commit_guest_counter_phase(token, proposal, running, ticket && storage)
            }
            Err(failure) => {
                self.world
                    .fail_guest_counter_phase(token, failure, running, ticket && storage)
            }
        };
        log_new_guest_fault(was_faulted, &completed, detail.as_deref());
        #[cfg(feature = "mods")]
        if let Some((identity, request, proposal)) = receipt {
            let outcome = match completed {
                Ok(Completion::Committed) => "committed",
                Ok(Completion::Skipped) => "skipped",
                Ok(Completion::Stale) => "stale",
                Err(_) => "fault",
            };
            let error = detail.or_else(|| {
                completed
                    .err()
                    .map(|error| clipped_detail(&error.to_string()))
            });
            emit_guest_receipt(guest_receipt(
                &identity,
                &request,
                proposal,
                outcome,
                error.as_deref(),
            ));
        }
    }

    pub(super) fn retire_counter_trial(&mut self, boundary: &str) {
        retire_world_counter(&mut self.world, boundary);
    }

    pub(super) fn counter_trial_status(&self) -> Value {
        let snapshot = self.world.counter_trial_snapshot();
        let eligibility = snapshot
            .as_ref()
            .map(|s| self.counter_position_eligibility(s.configuration().position()));
        json!({ "enabled": snapshot.is_some(),
            "execution_role": format!("{:?}", self.options.execution_role),
            "remote": self.remote.is_some(), "level": self.dim.id(),
            "game_time": self.world.game_time,
            "position_block_tick_eligible": eligibility.map(|(ticket, storage)| ticket && storage),
            "position_ticket_block_ticking": eligibility.map(|(ticket, _)| ticket),
            "position_storage_ticking": eligibility.map(|(_, storage)| storage),
            "trial": snapshot.as_ref().map(snapshot_json),
        })
    }

    fn counter_binding(
        &self,
        type_id: &str,
    ) -> Result<(
        MachineType,
        BlockState,
        BlockStateId,
        Arc<horizon_mods::paired::PreparedBundle>,
    )> {
        let catalog = self
            .options
            .mod_packages
            .catalog()
            .ok_or("no captured package catalog")?;
        let mut found = None;
        for prepared in catalog.prepared().values() {
            let content = prepared.content();
            let Some(machine) = content.machine_types().get(type_id) else {
                continue;
            };
            for block in &content.blocks {
                if block.machine_type() != Some(type_id) {
                    continue;
                }
                if found.is_some()
                    || block.template != "minecraft:iron_block"
                    || block.state_plan.is_some()
                {
                    return Err("counter trial needs one captured single-state iron binding".into());
                }
                let state_id = self
                    .pack
                    .assets
                    .registry
                    .default_state(&block.id)
                    .ok_or("counter binding is absent from the admitted registry")?;
                let state = self
                    .pack
                    .assets
                    .registry
                    .state(state_id)
                    .ok_or("counter binding has an invalid registry state")?
                    .clone();
                if state.name != block.id
                    || !state.properties.is_empty()
                    || machine.save_schema() != 1
                {
                    return Err("counter binding does not match its canonical schema/state".into());
                }
                found = Some((machine.clone(), state, state_id, prepared.clone()));
            }
        }
        found.ok_or_else(|| "counter type needs one selected captured block binding".into())
    }

    pub(super) fn counter_trial_control(&mut self, words: &[&str]) -> Result<Value> {
        match words.get(1).copied() {
            Some("status") if words.len() == 2 => Ok(self.counter_trial_status()),
            Some("on") if words.len() == 6 => {
                let position = [words[3].parse()?, words[4].parse()?, words[5].parse()?];
                let (machine, state, state_id, _) = self.counter_binding(words[2])?;
                let config = Configuration::new(
                    self.observation_scope()?,
                    &machine,
                    state,
                    state_id,
                    position,
                )?;
                let snapshot = self.world.enable_counter_trial(config)?;
                Ok(
                    json!({ "trial": snapshot_json(&snapshot), "status": self.counter_trial_status() }),
                )
            }
            Some("guest-on") if words.len() == 6 => {
                #[cfg(feature = "mods")]
                {
                    let position = [words[3].parse()?, words[4].parse()?, words[5].parse()?];
                    let (machine, state, state_id, prepared) = self.counter_binding(words[2])?;
                    let config = Configuration::new(self.observation_scope()?, &machine, state, state_id, position)?
                        .with_guest_package(&prepared)?;
                    if config.machine_type().id().len() > 256 || config.scope().dimension().len() > 256 {
                        return Err("counter guest identifiers exceed the diagnostic evaluator limit".into());
                    }
                    let Backend::Guest { primary_owner, package_identity } = config.backend() else {
                        return Err("counter guest configuration did not select captured code".into());
                    };
                    self.mods.mods.prepare_counter_evaluator(primary_owner, package_identity)?;
                    let snapshot = self.world.enable_counter_trial(config)?;
                    Ok(json!({ "trial": snapshot_json(&snapshot), "status": self.counter_trial_status() }))
                }
                #[cfg(not(feature = "mods"))]
                {
                    Err("counter-trial guest-on requires the mods runtime".into())
                }
            }
            Some("read") if words.len() == 4 => {
                let scope = AuthorityScope::new(words[2].parse()?, self.dim.id())?;
                let record = self.world.read_counter_trial(&scope, words[3].parse()?)?;
                Ok(json!({ "record": record_json(record), "status": self.counter_trial_status() }))
            }
            Some("off") if words.len() == 2 => {
                let retired = self.world.take_counter_trial();
                Ok(
                    json!({ "retired": retired.as_ref().map(snapshot_json), "status": self.counter_trial_status() }),
                )
            }
            _ => Err(
                "counter-trial on TYPE X Y Z | guest-on TYPE X Y Z | status | local-status | read SCOPE INSTANCE | off"
                    .into(),
            ),
        }
    }
}

#[cfg(all(test, feature = "mods"))]
mod tests {
    use super::*;

    #[test]
    fn guest_receipt_bounds_and_scalar_proposal_conversion_preserve_values() {
        let wire = CounterProposal {
            world_instance: u64::MAX,
            instance: 23,
            game_time: i64::MIN,
            expected_counter: 9,
            next_counter: 10,
        };
        let proposal = host_proposal(wire);
        assert_eq!(
            proposal,
            Proposal {
                world_instance: u64::MAX,
                instance: 23,
                game_time: i64::MIN,
                expected_counter: 9,
                next_counter: 10
            }
        );
        let request = CounterRequest {
            world_instance: u64::MAX,
            dimension: format!("a:{}", "b".repeat(254)),
            type_id: format!("a:{}", "c".repeat(254)),
            schema: u32::MAX,
            x: i32::MIN,
            y: i32::MAX,
            z: -1,
            instance: 23,
            counter: 9,
            game_time: i64::MIN,
        };
        let receipt = guest_receipt(&[0xff; 32], &request, Some(proposal), "committed", None);
        assert_eq!(
            receipt["proposal"],
            json!({ "world_instance": u64::MAX, "instance": 23,
            "game_time": i64::MIN, "expected_counter": 9, "next_counter": 10 })
        );
        assert_eq!(receipt["package_identity"], "ff".repeat(32));
        assert!(receipt["error"].is_null());
        let failed = guest_receipt(&[0; 32], &request, None, "fault", Some(&"\0".repeat(600)));
        assert!(failed["proposal"].is_null());
        assert_eq!(failed["error"].as_str().unwrap().chars().count(), 512);
        assert!(failed.to_string().len() + "HORIZON_COUNTER_GUEST ".len() < 8 * 1024);
    }

    #[test]
    #[ignore = "requires HORIZON_COUNTER_PAIR_FIXTURE"]
    fn captured_counter_token_projection_and_native_snapshot_shape() {
        use horizon_client::chunk::Chunk;
        use horizon_mods::paired::PreparedBundle;

        let path = std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
            .expect("HORIZON_COUNTER_PAIR_FIXTURE must name the owned prepared Rust pair");
        let prepared = PreparedBundle::read_folder(std::path::Path::new(&path))
            .expect("captured counter fixture");
        let block = prepared
            .content()
            .blocks
            .iter()
            .find(|b| b.machine_type().is_some())
            .expect("bound fixture cube");
        let machine = prepared
            .content()
            .machine_types()
            .get(block.machine_type().unwrap())
            .expect("captured type");
        let position = [2, 70, 2];
        let config = Configuration::new(
            AuthorityScope::new(17, "minecraft:overworld").unwrap(),
            machine,
            BlockState {
                name: block.id.clone(),
                properties: Default::default(),
            },
            BlockStateId(1),
            position,
        )
        .unwrap();
        let mut world = World::new(BlockStateId(0), 0);
        let mut chunk = Chunk::empty(0, 0);
        chunk.set(position[0], position[1], position[2], 1);
        world.chunks.insert((0, 0), Arc::new(chunk));
        world.game_time = -22;
        let native = world.enable_counter_trial(config.clone()).unwrap();
        let native_instance = native.record().unwrap().instance();
        assert_eq!(
            snapshot_json(&native),
            json!({
                "scope": { "world_instance": 17, "dimension": "minecraft:overworld" },
                "type": machine.id(), "schema": 1, "position": position,
                "state": { "name": block.id, "properties": {} },
                "record": { "instance": native_instance, "counter": 0, "last_advanced": null, "resident": true },
                "last_retirement": null, "fault": null, "persistence": "disabled-transient-trial",
            })
        );
        let _ = world.take_counter_trial();
        let guest = world
            .enable_counter_trial(config.with_guest_package(&prepared).unwrap())
            .unwrap();
        assert_eq!(
            snapshot_json(&guest)["backend"],
            json!({ "kind": "guest",
            "owner": prepared.bundle().content_owner().id, "package_identity": package_hex(prepared.identity()) })
        );
        let token = world
            .prepare_guest_counter_phase(true, true)
            .unwrap()
            .unwrap();
        let request = counter_request(&token);
        assert_eq!(
            request,
            CounterRequest {
                world_instance: 17,
                dimension: "minecraft:overworld".into(),
                type_id: machine.id().to_owned(),
                schema: 1,
                x: 2,
                y: 70,
                z: 2,
                instance: guest.record().unwrap().instance(),
                counter: 0,
                game_time: -22
            }
        );
        let receipt = guest_receipt(prepared.identity(), &request, None, "stale", None);
        assert_eq!(
            receipt["scope"],
            json!({ "world_instance": 17, "dimension": "minecraft:overworld" })
        );
        assert_eq!(
            receipt["request"],
            json!({ "world_instance": 17, "dimension": "minecraft:overworld",
            "type_id": machine.id(), "schema": 1, "x": 2, "y": 70, "z": 2,
            "instance": guest.record().unwrap().instance(), "counter": 0, "game_time": -22 })
        );
    }
}
