//! Explicit local fixture for the bounded authoritative residency observer.
use super::Game;
use horizon_client::{
    Result,
    residency_observer::{Kind, MAX_WATCHED_CHUNKS, Snapshot},
};
use serde_json::{Value, json};

fn snapshot_json(snapshot: &Snapshot) -> Value {
    json!({
        "scope": { "world_instance": snapshot.scope().world_instance(),
            "dimension": snapshot.scope().dimension() },
        "game_time": snapshot.game_time(),
        "entries": snapshot.entries().iter().map(|(coordinate, resident)|
            json!({ "chunk": coordinate, "resident": resident })).collect::<Vec<_>>(),
    })
}

impl Game<'_> {
    pub(super) fn residency_status(&self) -> Value {
        let observer = self.world.residency_observer();
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
            "current": self.world.residency_snapshot().as_ref().map(snapshot_json),
        })
    }

    pub(super) fn residency_local(&mut self, words: &[&str]) -> Result<Value> {
        match words.get(1).copied() {
            Some("status") if words.len() == 2 => Ok(self.residency_status()),
            Some("off") if words.len() == 2 => {
                self.world.take_residency_observer();
                Ok(self.residency_status())
            }
            Some("on") if words.len() >= 5 && (words.len() - 3).is_multiple_of(2) => {
                if (words.len() - 3) / 2 > MAX_WATCHED_CHUNKS {
                    return Err(format!(
                        "residency accepts at most {MAX_WATCHED_CHUNKS} supplied coordinates"
                    )
                    .into());
                }
                let capacity = words[2].parse::<usize>()?;
                let watched = words[3..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| -> Result<(i32, i32)> { Ok((pair[0].parse()?, pair[1].parse()?)) })
                    .collect::<Result<Vec<_>>>()?;
                let scope = self.observation_scope()?;
                let snapshot = self
                    .world
                    .enable_residency_observer(scope, watched, capacity)?;
                Ok(
                    json!({ "snapshot": snapshot_json(&snapshot), "status": self.residency_status() }),
                )
            }
            Some("drain") if words.len() == 2 => {
                let events = self
                    .world
                    .drain_residency()
                    .ok_or("residency observer disabled")??;
                let events = events.iter().map(|event| json!({
                    "scope": { "world_instance": event.scope().world_instance(),
                        "dimension": event.scope().dimension() },
                    "chunk": event.coordinate(),
                    "kind": match event.kind() {
                        Kind::Loaded => "loaded", Kind::Unloaded => "unloaded", Kind::Replaced => "replaced",
                    },
                    "game_time": event.game_time(),
                    "sequence": event.sequence(),
                })).collect::<Vec<_>>();
                Ok(json!({ "events": events, "status": self.residency_status() }))
            }
            _ => Err(
                "residency on CAPACITY CX CZ [CX CZ ...] | status | local-status | drain | off"
                    .into(),
            ),
        }
    }
}
