//! One active slot epoch fixture. No ordinary admission, tombstone or history service.
use super::*;
use horizon_client::{
    items::{ItemStack, Items},
    world::slot_restore::{
        DecodedSlotFrame, SlotAllocation, SlotFrameAllocation, SlotStatus, decode_slot_frame,
    },
    world_slot_trial::SlotKey,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::game::levels::capture_integration) struct SlotManifest {
    schema: u32,
    kind: String,
    slot_path: String,
    epoch: u64,
    identity: Identity,
    players: Vec<PlayerDescriptor>,
    dimensions: Vec<String>,
    counter: String,
    directories: Vec<String>,
    files: Vec<Member>,
}
impl SlotManifest {
    pub(in crate::game::levels::capture_integration) fn from_manifest(manifest: Manifest) -> Self {
        Self {
            schema: 2,
            kind: SLOT_KIND.into(),
            slot_path: SLOT_MEMBER.into(),
            epoch: manifest.epoch,
            identity: manifest.identity,
            players: manifest.players,
            dimensions: manifest.dimensions,
            counter: manifest.counter,
            directories: manifest.directories,
            files: manifest.files,
        }
    }
    pub(in crate::game::levels::capture_integration) fn validate(
        &self,
        identity: &Identity,
        epoch: u64,
    ) -> CResult<()> {
        if self.schema != 2
            || self.kind != SLOT_KIND
            || self.slot_path != SLOT_MEMBER
            || !self.files.iter().any(|member| member.path == SLOT_MEMBER)
            || !self.files.iter().any(|member| member.path == FRAME_PATH)
        {
            return Err(profile_error(
                "strict slot manifest profile/member mismatch",
            ));
        }
        // Shared physical descriptor view is internal only; schema1 decoder stays exact.
        let physical = Manifest {
            schema: 1,
            kind: "fresh-native-capture-cell".into(),
            epoch: self.epoch,
            identity: self.identity.clone(),
            players: self.players.clone(),
            dimensions: self.dimensions.clone(),
            counter: self.counter.clone(),
            directories: self.directories.clone(),
            files: self.files.clone(),
        };
        validate_manifest(&physical, epoch, identity)
    }
}
fn plain_stone(stack: Option<&ItemStack>, count: u32) -> bool {
    stack.is_some_and(|stack| {
        &*stack.item == "minecraft:stone"
            && stack.count == count
            && stack.damage == 0
            && stack.extra.is_none()
    })
}
fn correlate_slot(
    slot: &DecodedSlotFrame,
    counter: &CounterFrameV2,
    world: WorldId,
    host: &CounterTarget,
) -> CResult<()> {
    counter
        .validate()
        .map_err(|e| native_error(Phase::Recover, e))?;
    let Placement::Active {
        generation,
        counter: value,
        last_advanced,
    } = counter.placement
    else {
        return Err(profile_error("active counter required"));
    };
    let view = slot.view();
    let SlotFrameAllocation::Active {
        slot_generation,
        machine_generation,
        revision,
        stack,
    } = view.allocation
    else {
        return Err(profile_error("active slot required"));
    };
    if value != 3
        || last_advanced != Some(counter.saved_game_time)
        || counter.last_retired.is_some()
        || counter.world_uuid != world
        || counter.dimension != host.dimension
        || counter.position != host.position
        || view.world != world
        || view.dimension != host.dimension
        || view.position != host.position
        || view.package_identity != counter.package_identity
        || view.type_id != counter.type_id
        || view.block_id != counter.binding.block_id
        || view.save_schema != counter.save_schema
        || view.observed_state != "minecraft:iron_block"
        || view.saved_game_time != counter.saved_game_time
        || view.machine_high_water != counter.allocation_high_water
        || machine_generation != generation
        || slot_generation != 1
        || view.slot_high_water != 1
        || revision != 1
        || !plain_stone(stack, 7)
    {
        return Err(profile_error("slot/counter/host/plain-seven mismatch"));
    }
    Ok(())
}
fn validate_player_share(root: &Path, manifest: &Manifest) -> CResult<()> {
    let owner = manifest
        .players
        .iter()
        .find(|row| row.integrated_owner)
        .ok_or_else(|| profile_error("canonical owner absent"))?;
    let tag = strict_player(&bounded_read(
        &root.join(player_target(owner, "data")),
        2 << 20,
        Phase::Recover,
    )?)?;
    let Some(Tag::List(inventory)) = tag.get("Inventory") else {
        return Err(profile_error("owner Inventory absent"));
    };
    let stone: Vec<_> = inventory
        .iter()
        .filter(|item| item.get("id") == Some(&Tag::String("minecraft:stone".into())))
        .collect();
    if stone.len() != 1
        || stone[0].get("Slot") != Some(&Tag::Byte(0))
        || stone[0].get("count") != Some(&Tag::Int(3))
        || stone[0].get("components").is_some()
    {
        return Err(profile_error(
            "canonical PLAYER3 share disagrees with slot7",
        ));
    }
    for row in &manifest.players {
        let player = strict_player(&bounded_read(
            &root.join(player_target(row, "data")),
            2 << 20,
            Phase::Recover,
        )?)?;
        for key in ["Inventory", "EnderItems"] {
            if let Some(Tag::List(items)) = player.get(key) {
                for item in items {
                    if item.get("id") == Some(&Tag::String("minecraft:stone".into()))
                        && !(row.integrated_owner
                            && key == "Inventory"
                            && item.get("Slot") == Some(&Tag::Byte(0)))
                    {
                        return Err(profile_error(
                            "additional PLAYER stone outside qualified three-item share",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}
fn read_slot_image(
    root: &Path,
    manifest: &Manifest,
    binding: &NativeBinding,
    items: &Items,
    world: WorldId,
    host: &CounterTarget,
) -> CResult<(CounterFrameV2, DecodedSlotFrame)> {
    let counter = read_machine_image(root, manifest, binding, world, host)?;
    let slot = decode_slot_frame(
        &bounded_read(&root.join(SLOT_MEMBER), 1 << 20, Phase::Recover)?,
        items,
    )
    .map_err(|e| native_error(Phase::Recover, e))?;
    correlate_slot(&slot, &counter, world, host)?;
    validate_player_share(root, manifest)?;
    Ok((counter, slot))
}
#[derive(Debug)]
struct SlotRecovery {
    receipt: RecoveryReceipt,
    counter: CounterFrameV2,
    slot: DecodedSlotFrame,
}
fn recover_slot_fixture_epoch(
    archive: &Path,
    identity: &Identity,
    destination: &Path,
    binding: &NativeBinding,
    items: &Items,
    world: WorldId,
    host: &CounterTarget,
) -> CResult<SlotRecovery> {
    let selected =
        select_fixture_epoch_for(archive, identity, destination, RecoveryProfile::ActiveSlot)?;
    let (counter, slot) = read_slot_image(
        &selected.root,
        &selected.manifest,
        binding,
        items,
        world,
        host,
    )?;
    let receipt = copy_selected_epoch(selected, destination)?;
    Ok(SlotRecovery {
        receipt,
        counter,
        slot,
    })
}
#[derive(Debug)]
struct SlotInstallFailure {
    error: CaptureError,
    slot: DecodedSlotFrame,
}
/// Refusal-only diagnostics over already bounded detached projections. Never
/// reads mutable owners, changes a projection, or weakens the equality guard.
fn projection_change_diagnostics(
    counter_equal: bool,
    slot_equal: bool,
    before: &[PlayerImage],
    after: &[PlayerImage],
) {
    fn brief(tag: Option<&Tag>, depth: usize) -> String {
        let Some(tag) = tag else {
            return "absent".into();
        };
        match tag {
            Tag::End => "End".into(),
            Tag::Byte(value) => format!("Byte({value})"),
            Tag::Short(value) => format!("Short({value})"),
            Tag::Int(value) => format!("Int({value})"),
            Tag::Long(value) => format!("Long({value})"),
            Tag::Float(value) => format!("FloatBits({value:08x})"),
            Tag::Double(value) => format!("DoubleBits({value:016x})"),
            Tag::String(value) => format!(
                "String(len={},prefix={:?})",
                value.0.len(),
                value.to_string_lossy().chars().take(32).collect::<String>()
            ),
            Tag::ByteArray(value) => format!(
                "ByteArray(len={},prefix={:?})",
                value.len(),
                &value[..value.len().min(4)]
            ),
            Tag::IntArray(value) => format!(
                "IntArray(len={},prefix={:?})",
                value.len(),
                &value[..value.len().min(4)]
            ),
            Tag::LongArray(value) => format!(
                "LongArray(len={},prefix={:?})",
                value.len(),
                &value[..value.len().min(4)]
            ),
            Tag::List(values) if depth < 2 => format!(
                "List(len={},prefix=[{}])",
                values.len(),
                values
                    .iter()
                    .take(4)
                    .map(|value| brief(Some(value), depth + 1))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Tag::Compound(values) if depth < 2 => format!(
                "Compound(len={},prefix=[{}])",
                values.len(),
                values
                    .iter()
                    .take(4)
                    .map(|(key, value)| format!(
                        "{:?}:{}",
                        key.to_string_lossy().chars().take(32).collect::<String>(),
                        brief(Some(value), depth + 1)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Tag::List(values) => format!("List(len={})", values.len()),
            Tag::Compound(values) => format!("Compound(len={})", values.len()),
        }
    }
    eprintln!(
        "SLOT_EPOCH_SAVE_DIFF counter_equal={counter_equal} slot_equal={slot_equal} players_before={} players_after={}",
        before.len(),
        after.len()
    );
    for (index, (before, after)) in before.iter().zip(after).take(2).enumerate() {
        eprintln!(
            "SLOT_EPOCH_PLAYER_DIFF index={index} descriptor_equal={} nbt_equal={} advancements_equal={} stats_equal={} nbt_bytes_before={} nbt_bytes_after={}",
            before.descriptor == after.descriptor,
            before.nbt == after.nbt,
            before.advancements == after.advancements,
            before.stats == after.stats,
            before.nbt.len(),
            after.nbt.len()
        );
        if before.nbt == after.nbt {
            continue;
        }
        match (strict_player(&before.nbt), strict_player(&after.nbt)) {
            (Ok(Tag::Compound(before)), Ok(Tag::Compound(after))) => {
                let changed = before
                    .keys()
                    .chain(after.keys().filter(|key| !before.contains_key(*key)))
                    .filter(|key| before.get(*key) != after.get(*key));
                for key in changed.take(16) {
                    eprintln!(
                        "SLOT_EPOCH_NBT_DIFF index={index} key={:?} before={} after={}",
                        key.to_string_lossy().chars().take(64).collect::<String>(),
                        brief(before.get(key), 0)
                            .chars()
                            .take(160)
                            .collect::<String>(),
                        brief(after.get(key), 0)
                            .chars()
                            .take(160)
                            .collect::<String>()
                    );
                }
            }
            _ => eprintln!(
                "SLOT_EPOCH_NBT_DIFF index={index} bounded_decode_or_compound_failure=true"
            ),
        }
    }
}
impl Game<'_> {
    /// Full native projection validation for the fixed E1 fixture path; never takes an owner.
    pub(in crate::game) fn slot_epoch_projection(&self) -> CResult<(CounterFrameV2, Vec<u8>)> {
        if self.dim != Dim::Overworld {
            return Err(profile_error("slot fixture requires actual OW"));
        }
        let host = target(self.dim)?;
        let domain = self
            .world
            .counter_domain_snapshot()
            .ok_or_else(|| profile_error("native counter domain absent"))?;
        let metadata = self
            .world
            .persistent_world_slot_metadata()
            .ok_or_else(|| profile_error("persistent slot metadata absent"))?;
        if metadata.status != SlotStatus::Healthy
            || !matches!(metadata.allocation, SlotAllocation::Active { .. })
        {
            return Err(profile_error("healthy active slot owner required"));
        }
        let counter = self
            .world
            .counter_checkpoint_frame(self.world.game_time)
            .map_err(|e| native_error(Phase::Project, e))?;
        let bytes = self
            .world
            .slot_checkpoint_bytes(self.res.items(), self.world.game_time)
            .map_err(|e| native_error(Phase::Project, e))?;
        let slot = decode_slot_frame(&bytes, self.res.items())
            .map_err(|e| native_error(Phase::Project, e))?;
        correlate_slot(&slot, &counter, domain.world, &host)?;
        let Some(host) = &self.lan else {
            return Err(profile_error("actual player host absent"));
        };
        let owner = host
            .hub
            .guests
            .iter()
            .find(|guest| guest.uuid == 11)
            .ok_or_else(|| profile_error("owner player absent"))?;
        if !plain_stone(owner.inventory.first().and_then(Option::as_ref), 3)
            || host.hub.guests.iter().any(|guest| {
                guest.inventory.iter().enumerate().any(|(index, stack)| {
                    !(guest.uuid == 11 && index == 0)
                        && stack
                            .as_ref()
                            .is_some_and(|stack| &*stack.item == "minecraft:stone")
                }) || guest
                    .ender_items
                    .iter()
                    .flatten()
                    .any(|stack| &*stack.item == "minecraft:stone")
            })
        {
            return Err(profile_error("live PLAYER3 share disagrees with slot7"));
        }
        Ok((counter, bytes))
    }
    fn slot_player_images(&self) -> CResult<Vec<PlayerImage>> {
        let roster = self
            .observe_checkpoint_participants()
            .map_err(|e| invalid(Phase::Project, format!("{e:?}")))?;
        self.project_checkpoint_participants(&roster)
            .map_err(|e| invalid(Phase::Project, format!("{e:?}")))?
            .into_capture_rows()
            .into_iter()
            .map(PlayerImage::from_row)
            .collect()
    }
    pub(in crate::game) fn capture_slot_fixture_epoch(
        &mut self,
        request: CaptureRequest,
    ) -> CaptureCompletion {
        self.capture_profile_checks(CaptureProfile::NativeSlotPatch)?;
        let binding = fixture_binding(self)?;
        if request.identity.counter_content != digest_identity(binding.package_identity()) {
            return Err(profile_error("actual package identity mismatch"));
        }
        let (counter, slot_bytes) = self.slot_epoch_projection()?;
        let material = self.profile_material(request, CaptureProfile::NativeSlotPatch)?;
        let host = target(self.dim)?;
        correlate(
            &counter,
            &material.counter,
            &material.request.identity,
            &binding,
            counter.world_uuid,
            &host,
            self.world.game_time,
        )?;
        let (after_counter, after_slot) = self.slot_epoch_projection()?;
        let after_players = self.slot_player_images()?;
        if after_counter != counter
            || after_slot != slot_bytes
            || after_players.len() != material.players.len()
            || after_players
                .iter()
                .zip(&material.players)
                .any(|(after, before)| {
                    after.descriptor != before.descriptor
                        || after.nbt != before.nbt
                        || after.advancements != before.advancements
                        || after.stats != before.stats
                })
        {
            projection_change_diagnostics(
                after_counter == counter,
                after_slot == slot_bytes,
                &material.players,
                &after_players,
            );
            return Err(invalid(
                Phase::Save,
                "counter/slot/PLAYER projections changed during native save",
            ));
        }
        let mut image = material.copy()?;
        image.overlay()?;
        write_bytes(
            &make_parents(&image.root, FRAME_PATH, Phase::Overlay)?,
            &counter
                .encode()
                .map_err(|e| native_error(Phase::Overlay, e))?,
            true,
            Phase::Overlay,
        )?;
        write_bytes(
            &make_parents(&image.root, SLOT_MEMBER, Phase::Overlay)?,
            &slot_bytes,
            true,
            Phase::Overlay,
        )?;
        image.sync_members()?;
        image.write_manifest_temp_for(RecoveryProfile::ActiveSlot)?;
        image.finish_manifest()?;
        let verified =
            verify_complete_envelope(&image.root, image.request.epoch, &image.request.identity)?;
        check_recovery_profile(
            &verified.manifest,
            verified.slot_manifest.as_ref(),
            RecoveryProfile::ActiveSlot,
        )?;
        read_slot_image(
            &image.root,
            &verified.manifest,
            &binding,
            self.res.items(),
            counter.world_uuid,
            &host,
        )?;
        image
            .rename_published_for(RecoveryProfile::ActiveSlot)?
            .acknowledge()
    }
    fn install_slot_epoch(
        &mut self,
        binding: &NativeBinding,
        world: WorldId,
        counter: &CounterFrameV2,
        slot: DecodedSlotFrame,
    ) -> std::result::Result<(RuntimeCounterKey, SlotKey), SlotInstallFailure> {
        let host = match target(self.dim) {
            Ok(host) => host,
            Err(error) => return Err(SlotInstallFailure { error, slot }),
        };
        if let Err(error) = correlate_slot(&slot, counter, world, &host) {
            return Err(SlotInstallFailure { error, slot });
        }
        let counter_key = match self.install_machine_checkpoint(binding, world, counter) {
            Ok(key) => key,
            Err(error) => return Err(SlotInstallFailure { error, slot }),
        };
        match self.world.restore_persistent_world_slot(
            binding,
            self.res.items(),
            world,
            &host,
            slot,
        ) {
            Ok(Some(slot_key)) => Ok((counter_key, slot_key)),
            Ok(None) => unreachable!("checked active input cannot install dormant"),
            Err(rejected) => Err(SlotInstallFailure {
                error: native_error(Phase::Recover, rejected.error),
                slot: rejected.frame,
            }),
        }
    }
}
#[path = "slot_epoch_integration_tests.rs"]
mod tests;
