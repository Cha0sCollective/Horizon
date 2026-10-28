//! Experimental pure counter computation. No host state or ordinary mod imports.
use super::{CheckedPair, FUEL_TICK, MEMORY};
use crate::Side;
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder,
    component::{
        Component, ComponentExportIndex, ComponentType, InstancePre, Lift, Linker, Lower,
        types::ComponentItem,
    },
};

// Diagnostic limits, not general identifier or stable SDK policies.
const IDENTIFIER_BYTES: usize = 256;

fn diagnostic(stage: &str, error: wasmtime::Error) -> String {
    super::clip_to(format!("counter-trial {stage}: {error:#}"), 512)
}

/// Invocation-local host projection; these IDs are not durable generations.
#[derive(Clone, Debug, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(record)]
pub struct CounterRequest {
    #[component(name = "world-instance")]
    pub world_instance: u64,
    pub dimension: String,
    #[component(name = "type-id")]
    pub type_id: String,
    pub schema: u32,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub instance: u64,
    pub counter: u64,
    #[component(name = "game-time")]
    pub game_time: i64,
}

/// A checked scalar proposal, never an authoritative host mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(record)]
pub struct CounterProposal {
    #[component(name = "world-instance")]
    pub world_instance: u64,
    pub instance: u64,
    #[component(name = "game-time")]
    pub game_time: i64,
    #[component(name = "expected-counter")]
    pub expected_counter: u64,
    #[component(name = "next-counter")]
    pub next_counter: u64,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[expect(dead_code, reason = "Constructed through canonical result lifting.")]
enum Rejection {
    #[component(name = "rejected")]
    Rejected,
}

/// Compiled captured server code; each evaluation owns and discards a fresh Store.
/// Construction proves only this diagnostic ABI, not ordinary activation.
pub struct CounterEvaluator {
    pre: InstancePre<StoreLimits>,
    step: ComponentExportIndex,
    identity: [u8; 32],
}

impl CounterEvaluator {
    /// Selects only the captured server half after both ordinary side preflights.
    pub fn from_pair(pair: &CheckedPair<'_>) -> Result<Self, String> {
        Self::checked(pair.component(Side::Server)?, *pair.prepared().identity())
    }

    fn checked(component: &Component, identity: [u8; 32]) -> Result<Self, String> {
        let engine = component.engine();
        if component.component_type().imports(engine).next().is_some() {
            return Err("counter-trial requires zero component imports".into());
        }
        let Some((ComponentItem::ComponentInstance(_), interface)) =
            component.get_export(None, "counter-trial")
        else {
            return Err("missing counter-trial interface".into());
        };
        let Some((ComponentItem::ComponentFunc(function), step)) =
            component.get_export(Some(&interface), "step")
        else {
            return Err("missing counter-trial.step function".into());
        };
        if function.async_() {
            return Err("counter-trial.step must be synchronous".into());
        }
        let pre = Linker::<StoreLimits>::new(engine)
            .instantiate_pre(component)
            .map_err(|e| diagnostic("preflight", e))?;
        function
            .typecheck::<(CounterRequest,), (Result<CounterProposal, Rejection>,)>(
                &pre.instance_type(),
            )
            .map_err(|e| diagnostic("step type", e))?;
        Ok(Self {
            pre,
            step,
            identity,
        })
    }

    pub fn identity(&self) -> &[u8; 32] {
        &self.identity
    }

    pub fn evaluate(&self, request: &CounterRequest) -> Result<CounterProposal, String> {
        self.evaluate_with(request, FUEL_TICK, limits())
    }

    fn evaluate_with(
        &self,
        request: &CounterRequest,
        fuel: u64,
        limits: StoreLimits,
    ) -> Result<CounterProposal, String> {
        if request.dimension.len() > IDENTIFIER_BYTES || request.type_id.len() > IDENTIFIER_BYTES {
            return Err("counter-trial request identifier exceeds 256 bytes".into());
        }
        let next = request
            .counter
            .checked_add(1)
            .ok_or("counter-trial counter exhausted")?;
        let mut store = Store::new(self.pre.engine(), limits);
        store.limiter(|limits| limits);
        // One budget covers core/component start, argument realloc, body and post-return.
        store.set_fuel(fuel).map_err(|e| diagnostic("fuel", e))?;
        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| diagnostic("instantiate", e))?;
        let step = instance
            .get_typed_func::<(CounterRequest,), (Result<CounterProposal, Rejection>,)>(
                &mut store, self.step,
            )
            .map_err(|e| diagnostic("step lookup", e))?;
        // Wasmtime 48 call completes lifting AND canonical post-return before success.
        let (reply,) = step
            .call(&mut store, (request.clone(),))
            .map_err(|e| diagnostic("step call", e))?;
        drop(store);
        let proposal = reply.map_err(|_| "counter-trial guest rejected".to_owned())?;
        if proposal.world_instance != request.world_instance
            || proposal.instance != request.instance
            || proposal.game_time != request.game_time
            || proposal.expected_counter != request.counter
            || proposal.next_counter != next
        {
            return Err("counter-trial invalid scalar proposal".into());
        }
        Ok(proposal)
    }
}

fn limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(MEMORY)
        .instances(16)
        .memories(1)
        .tables(2)
        .table_elements(16_384)
        .trap_on_grow_failure(true)
        .build()
}

#[cfg(test)]
mod tests;
