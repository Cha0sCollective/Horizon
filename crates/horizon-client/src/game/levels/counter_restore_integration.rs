//! Linux, test-only nonzero native counter install/resume cell. No runtime admission.
use super::*;
use horizon_client::world::counter_restore::{
    CounterFrameV2, CounterTarget, NativeBinding, Placement, RuntimeCounterKey, WorldId,
};
pub(super) const FRAME_PATH: &str = "world/data/horizon/counter_checkpoint.json";
const FRAME_LIMIT: usize = 16 << 10;
const POSITION: [i32; 3] = [2, 70, 2];
fn native_error(phase: Phase, error: impl fmt::Display) -> CaptureError {
    invalid(phase, error)
}
fn target(dimension: Dim) -> CResult<CounterTarget> {
    CounterTarget::new(dimension.id(), POSITION).map_err(|e| native_error(Phase::Validate, e))
}
fn digest_identity(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn fixture_binding(game: &Game<'_>) -> CResult<NativeBinding> {
    let folder = game
        .world_root
        .parent()
        .ok_or_else(|| invalid(Phase::Validate, "fixture parent"))?
        .join("counter-content");
    let bundle = horizon_mods::paired::PreparedBundle::read_folder(&folder)
        .map_err(|e| native_error(Phase::Validate, e))?;
    NativeBinding::from_prepared(&bundle, "capture_counter:type", game.pack.assets.registry)
        .map_err(|e| native_error(Phase::Validate, e))
}
/// A separate V2 codec and envelope correlation; v1 verification remains unchanged.
fn correlate(
    frame: &CounterFrameV2,
    counter: &CounterArchive,
    identity: &Identity,
    binding: &NativeBinding,
    expected_world: WorldId,
    host_target: &CounterTarget,
    saved_time: i64,
) -> CResult<()> {
    frame
        .validate()
        .map_err(|e| native_error(Phase::Recover, e))?;
    let Placement::Active {
        counter: value,
        last_advanced,
        ..
    } = frame.placement
    else {
        return Err(invalid(Phase::Recover, "active-only epoch"));
    };
    if frame.world_uuid != expected_world
        || frame.dimension != host_target.dimension
        || frame.position != host_target.position
        || frame.saved_game_time != saved_time
        || frame.last_retired.is_some()
        || frame.package_identity != identity.counter_content
        || frame.package_identity != digest_identity(binding.package_identity())
        || frame.type_id != binding.type_id()
        || frame.binding.block_id != binding.block_id()
        || frame.type_id != counter.r#type
        || frame.save_schema != counter.save_schema
        || frame.dimension != counter.scope.dimension
        || frame.position != counter.position
        || counter.state.name != frame.binding.block_id
        || !counter.state.properties.is_empty()
        || counter.counter != value
        || counter.last_advanced != last_advanced
        || frame.observed_state.name != "minecraft:iron_block"
        || !frame.observed_state.properties.is_empty()
        || !counter.resident
        || counter.live_generation_restore
    {
        return Err(invalid(
            Phase::Recover,
            "native frame/envelope/host disagreement",
        ));
    }
    Ok(())
}
fn read_machine_image(
    root: &Path,
    manifest: &Manifest,
    binding: &NativeBinding,
    expected_world: WorldId,
    host_target: &CounterTarget,
) -> CResult<CounterFrameV2> {
    if manifest.dimensions != [Dim::Overworld.id()]
        || host_target.dimension != Dim::Overworld.id()
        || !manifest
            .files
            .iter()
            .any(|member| member.path == FRAME_PATH)
    {
        return Err(invalid(
            Phase::Recover,
            "native frame member/profile missing",
        ));
    }
    let frame = CounterFrameV2::decode(&bounded_read(
        &root.join(FRAME_PATH),
        FRAME_LIMIT,
        Phase::Recover,
    )?)
    .map_err(|e| native_error(Phase::Recover, e))?;
    let counter: CounterArchive = parse_json(
        &bounded_read(
            &root.join("research/counter.json"),
            JSON_LIMIT,
            Phase::Recover,
        )?,
        JSON_LIMIT,
    )?;
    // The same gzip/NBT codec as level.dat, bounded before native Level interpretation.
    let level = strict_player(&bounded_read(
        &root.join("world/level.dat"),
        2 << 20,
        Phase::Recover,
    )?)?;
    let time = match level.get("Data").and_then(|data| data.get("Time")) {
        Some(Tag::Long(time)) => *time,
        _ => return Err(invalid(Phase::Recover, "native Level.Time missing/type")),
    };
    correlate(
        &frame,
        &counter,
        &manifest.identity,
        binding,
        expected_world,
        host_target,
        time,
    )?;
    Ok(frame)
}
#[derive(Debug)]
struct MachineRecovery {
    receipt: RecoveryReceipt,
    frame: CounterFrameV2,
}
fn recover_machine_fixture_epoch(
    archive_root: &Path,
    identity: &Identity,
    destination: &Path,
    binding: &NativeBinding,
    expected_world: WorldId,
    host_target: &CounterTarget,
) -> CResult<MachineRecovery> {
    let selected = select_fixture_epoch_for(
        archive_root,
        identity,
        destination,
        RecoveryProfile::CounterOnly,
    )?;
    // Refuse the selected envelope here; never borrow V2 from an older candidate.
    let frame = read_machine_image(
        &selected.root,
        &selected.manifest,
        binding,
        expected_world,
        host_target,
    )?;
    let receipt = copy_selected_epoch(selected, destination)?;
    Ok(MachineRecovery { receipt, frame })
}
impl Game<'_> {
    pub(in crate::game) fn capture_machine_fixture_epoch(
        &mut self,
        request: CaptureRequest,
    ) -> CaptureCompletion {
        self.capture_profile_checks(CaptureProfile::NativeCounterPatch)?;
        let binding = fixture_binding(self)?;
        if request.identity.counter_content != digest_identity(binding.package_identity()) {
            return Err(invalid(Phase::Validate, "host package identity mismatch"));
        }
        let host_target = target(self.dim)?;
        let frame = self
            .world
            .counter_checkpoint_frame(self.world.game_time)
            .map_err(|e| native_error(Phase::Project, e))?;
        let material = self.profile_material(request, CaptureProfile::NativeCounterPatch)?;
        correlate(
            &frame,
            &material.counter,
            &material.request.identity,
            &binding,
            frame.world_uuid,
            &host_target,
            self.world.game_time,
        )?;
        let mut image = material.copy()?;
        image.overlay()?;
        let bytes = frame
            .encode()
            .map_err(|e| native_error(Phase::Overlay, e))?;
        let path = make_parents(&image.root, FRAME_PATH, Phase::Overlay)?;
        write_bytes(&path, &bytes, true, Phase::Overlay)?;
        image.sync_members()?;
        image.write_manifest_temp()?;
        image.finish_manifest()?;
        // Recheck the complete physical envelope including the native saved clock before rename.
        let manifest = verify_image(&image.root, image.request.epoch, &image.request.identity)?;
        read_machine_image(
            &image.root,
            &manifest,
            &binding,
            frame.world_uuid,
            &host_target,
        )?;
        image.rename_published()?.acknowledge()
    }
    fn install_machine_checkpoint(
        &mut self,
        binding: &NativeBinding,
        expected_world: WorldId,
        frame: &CounterFrameV2,
    ) -> CResult<RuntimeCounterKey> {
        if !matches!(frame.placement, Placement::Active { .. }) {
            return Err(invalid(Phase::Recover, "active-only install adapter"));
        }
        let host_target = target(self.dim)?;
        self.world
            .restore_counter_frame(binding, expected_world, &host_target, frame)
            .map_err(|e| native_error(Phase::Recover, e))?
            .ok_or_else(|| invalid(Phase::Recover, "active placement required"))
    }
}
#[path = "slot_epoch_integration.rs"]
pub(super) mod slot_epoch_integration;
#[path = "counter_restore_integration_tests.rs"]
mod tests;
