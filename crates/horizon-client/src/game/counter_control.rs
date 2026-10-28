//! One opt-in transient native counter. No inventory, guest ABI or persistence.
use super::Game;
use horizon_client::{
    Result,
    counter_trial::{Configuration, Record, Snapshot},
    mutation_observer::AuthorityScope,
    world::World,
};
use horizon_mods::content::machines::MachineType;
use horizon_world::{chunk::BlockState, registry::BlockStateId};
use serde_json::{Value, json};
use std::io::Write;

fn record_json(record: Record) -> Value {
    json!({ "instance": record.instance(), "counter": record.counter(),
        "last_advanced": record.last_advanced(), "resident": record.resident() })
}

fn snapshot_json(snapshot: &Snapshot) -> Value {
    let config = snapshot.configuration();
    json!({
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
    })
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
        let result = self.world.advance_counter_trial(running, ticket && storage);
        if before.fault().is_none()
            && let Some(Err(fault)) = result
        {
            log::error!("Horizon: {fault}");
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

    fn counter_binding(&self, type_id: &str) -> Result<(MachineType, BlockState, BlockStateId)> {
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
                found = Some((machine.clone(), state, state_id));
            }
        }
        found.ok_or_else(|| "counter type needs one selected captured block binding".into())
    }

    pub(super) fn counter_trial_control(&mut self, words: &[&str]) -> Result<Value> {
        match words.get(1).copied() {
            Some("status") if words.len() == 2 => Ok(self.counter_trial_status()),
            Some("on") if words.len() == 6 => {
                let position = [words[3].parse()?, words[4].parse()?, words[5].parse()?];
                let (machine, state, state_id) = self.counter_binding(words[2])?;
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
                "counter-trial on TYPE X Y Z | status | local-status | read SCOPE INSTANCE | off"
                    .into(),
            ),
        }
    }
}
