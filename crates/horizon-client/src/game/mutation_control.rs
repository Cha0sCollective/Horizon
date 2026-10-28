//! Explicit local diagnostic fixture for passive resident mutation observations.
//! No observer is enabled by normal startup, or by a remote player/guest.
use super::{Dim, Game};
use horizon_client::{Result, mutation_observer::AuthorityScope};
use horizon_world::registry::BlockStateId;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

// Keep the crate's declared Rust minimum; the renamed try_update is newer.
fn next_instance(counter: &AtomicU64) -> Result<u64> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current
            .checked_add(1)
            .ok_or("mutation observer instance IDs exhausted")?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(instance) => return Ok(instance),
            Err(actual) => current = actual,
        }
    }
}

impl Game<'_> {
    pub(super) fn observation_control(&mut self, words: &[&str]) -> Result<Value> {
        if words.get(1) == Some(&"local-status") && words.len() == 2 {
            return Ok(if words.first() == Some(&"mutations") {
                self.mutation_status()
            } else {
                self.residency_status()
            });
        }
        if let Some(server) = &self.integrated {
            let owned: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            return server
                .call(move |game| {
                    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
                    game.observation_owner(&words).map_err(|e| e.to_string())
                })
                .ok_or("the integrated server stopped")?
                .map_err(Into::into);
        }
        if self.remote.is_some() {
            return Err("observation requires a local authoritative world".into());
        }
        self.observation_local(words)
    }

    fn observation_owner(&mut self, words: &[&str]) -> Result<Value> {
        let owner = self
            .serving
            .as_ref()
            .ok_or("not an integrated server")?
            .owner;
        let guest = self
            .lan
            .as_ref()
            .and_then(|host| host.hub.guests.iter().find(|g| g.uuid == owner && g.placed))
            .ok_or("the integrated owner is not placed")?;
        let dimension = Dim::from_id(&guest.dimension);
        if dimension.id() != guest.dimension {
            return Err("the owner's dimension is unsupported".into());
        }
        let back = self.dim;
        self.enter_command_level(dimension);
        if self.dim != dimension {
            return Err("the owner's authoritative level is unavailable".into());
        }
        let result = self.observation_local(words);
        self.enter_command_level(back);
        if self.dim != back {
            return Err("could not restore the server's level after observation".into());
        }
        result
    }

    fn observation_local(&mut self, words: &[&str]) -> Result<Value> {
        match words.first() {
            Some(&"mutations") => self.mutation_local(words),
            Some(&"residency") => self.residency_local(words),
            _ => Err("unknown observation command".into()),
        }
    }

    pub(super) fn observation_scope(&self) -> Result<AuthorityScope> {
        Ok(AuthorityScope::new(
            next_instance(&NEXT_INSTANCE)?,
            self.dim.id(),
        )?)
    }

    fn mutation_status(&self) -> Value {
        let observer = self.world.mutation_observer();
        json!({
            "enabled": observer.is_some(),
            "execution_role": format!("{:?}", self.options.execution_role),
            "remote": self.remote.is_some(),
            "level": self.dim.id(),
            "scope": observer.map(|o| json!({
                "world_instance": o.scope().world_instance(),
                "dimension": o.scope().dimension(),
            })),
            "pending": observer.map(|o| o.pending_len()),
            "fault": observer.and_then(|o| o.fault()).map(|e| e.to_string()),
        })
    }

    fn mutation_local(&mut self, words: &[&str]) -> Result<Value> {
        match words.get(1).copied() {
            Some("status") if words.len() == 2 => Ok(self.mutation_status()),
            Some("off") if words.len() == 2 => {
                self.world.take_mutation_observer();
                Ok(self.mutation_status())
            }
            Some("on") if words.len() == 3 || words.len() == 4 => {
                if self.world.mutation_observer().is_some() {
                    return Err(
                        "mutation observer already enabled; explicitly turn it off first".into(),
                    );
                }
                let name = words[2];
                let capacity = words
                    .get(3)
                    .map_or(Ok(4096), |text| text.parse::<usize>())?;
                let registry = &self.pack.assets.registry;
                if registry.default_state(name).is_none() {
                    return Err("mutations on needs one exact registered block name".into());
                }
                let states = (0..registry.len())
                    .map(|id| BlockStateId(id as u32))
                    .filter(|id| registry.state(*id).is_some_and(|state| state.name == name));
                let scope = self.observation_scope()?;
                self.world
                    .enable_mutation_observer(scope, states, capacity)?;
                Ok(self.mutation_status())
            }
            Some("drain") if words.len() == 2 => {
                let batch = self
                    .world
                    .drain_mutations()
                    .ok_or("mutation observer disabled")??;
                let state = |id: BlockStateId| -> Result<Value> {
                    let state = self
                        .pack
                        .assets
                        .registry
                        .state(id)
                        .ok_or("observed state is absent from the frozen registry")?;
                    Ok(json!({ "name": state.name, "properties": state.properties }))
                };
                let events = batch
                    .iter()
                    .map(|event| -> Result<Value> {
                        Ok(json!({
                            "scope": { "world_instance": event.scope().world_instance(),
                                "dimension": event.scope().dimension() },
                            "position": event.position(),
                            "previous": state(event.previous())?,
                            "current": state(event.current())?,
                            "game_time": event.game_time(),
                            "sequence": event.sequence(),
                            "cause": "unclassified",
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({ "events": events, "status": self.mutation_status() }))
            }
            _ => Err("mutations on BLOCK [CAPACITY] | status | local-status | drain | off".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_instance_ids_never_wrap_or_become_reusable() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_instance(&counter).unwrap(), u64::MAX - 1);
        assert!(next_instance(&counter).is_err());
        assert!(next_instance(&counter).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
}
