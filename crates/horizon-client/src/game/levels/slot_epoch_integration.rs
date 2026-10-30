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
fn native_empty_owner_lists(tag: &Tag) -> CResult<()> {
    // encode_with ALWAYS writes block_entities. Its optional SaveTables branch
    // writes BOTH tick lists; without tables, neither generated key is emitted.
    if !matches!(tag.get("block_entities"),Some(Tag::List(values)) if values.is_empty()) {
        return Err(profile_error(
            "native block_entities must be a present empty List",
        ));
    }
    match (tag.get("block_ticks"), tag.get("fluid_ticks")) {
        (None, None) => Ok(()),
        (Some(Tag::List(blocks)), Some(Tag::List(fluids)))
            if blocks.is_empty() && fluids.is_empty() =>
        {
            Ok(())
        }
        _ => Err(profile_error(
            "native tick lists must be both absent or both exact empty Lists",
        )),
    }
}
fn correlate_slot(
    slot: &DecodedSlotFrame,
    counter: &CounterFrameV2,
    world: WorldId,
    host: &CounterTarget,
) -> CResult<()> {
    correlate_slot_for(slot, counter, world, host, SlotPhase::Three)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotPhase {
    Three,
    Four,
}
fn correlate_slot_for(
    slot: &DecodedSlotFrame,
    counter: &CounterFrameV2,
    world: WorldId,
    host: &CounterTarget,
    phase: SlotPhase,
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
    if value
        != match phase {
            SlotPhase::Three => 3,
            SlotPhase::Four => 4,
        }
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
    read_slot_image_for(
        root,
        manifest,
        binding,
        items,
        world,
        host,
        SlotPhase::Three,
    )
}
fn read_slot_image_for(
    root: &Path,
    manifest: &Manifest,
    binding: &NativeBinding,
    items: &Items,
    world: WorldId,
    host: &CounterTarget,
    phase: SlotPhase,
) -> CResult<(CounterFrameV2, DecodedSlotFrame)> {
    let counter = read_machine_image(root, manifest, binding, world, host)?;
    let slot = decode_slot_frame(
        &bounded_read(&root.join(SLOT_MEMBER), 1 << 20, Phase::Recover)?,
        items,
    )
    .map_err(|e| native_error(Phase::Recover, e))?;
    correlate_slot_for(&slot, &counter, world, host, phase)?;
    validate_player_share(root, manifest)?;
    Ok((counter, slot))
}
#[derive(Debug)]
struct SlotRecovery {
    receipt: RecoveryReceipt,
    counter: CounterFrameV2,
    slot: DecodedSlotFrame,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdvanceDisposition {
    Current4,
    Baseline3Fallback { selected_epoch: u64 },
}
#[derive(Debug)]
struct AdvanceRecovery {
    receipt: RecoveryReceipt,
    counter: CounterFrameV2,
    slot: DecodedSlotFrame,
    disposition: AdvanceDisposition,
}
fn recover_advanced_slot_fixture_epoch(
    game: &Game<'_>,
    archive: &Path,
    identity: &Identity,
    destination: &Path,
    binding: &NativeBinding,
    world: WorldId,
    host: &CounterTarget,
) -> CResult<AdvanceRecovery> {
    let selected = select_fixture_epoch_for(
        archive,
        identity,
        destination,
        RecoveryProfile::AdvancedSlot,
    )?;
    if selected.rejected.iter().any(|(epoch, _)| *epoch > 3) {
        return Err(profile_error("advanced fixed archive exceeds epoch3"));
    }
    let phase = match selected.manifest.epoch {
        3 => SlotPhase::Four,
        2 if selected.rejected.iter().any(|(epoch, _)| *epoch == 3) => SlotPhase::Three,
        _ => {
            return Err(profile_error(
                "advanced recovery requires epoch3 or explicit physical baseline2 fallback",
            ));
        }
    };
    let (counter, slot) = read_slot_image_for(
        &selected.root,
        &selected.manifest,
        binding,
        game.res.items(),
        world,
        host,
        phase,
    )?;
    game.checked_slot_columns(
        &selected.root.join("world"),
        binding,
        counter.saved_game_time,
        phase,
    )?;
    let disposition = match phase {
        SlotPhase::Four => AdvanceDisposition::Current4,
        SlotPhase::Three => AdvanceDisposition::Baseline3Fallback {
            selected_epoch: selected.manifest.epoch,
        },
    };
    let receipt = copy_selected_epoch(selected, destination)?;
    Ok(AdvanceRecovery {
        receipt,
        counter,
        slot,
        disposition,
    })
}
fn slot_region_fingerprint(root: &Path) -> CResult<Vec<(String, u64, String)>> {
    let directory = horizon_client::save::dimension_dir(root, Dim::Overworld.id()).join("region");
    let mut rows = Vec::new();
    for x in -1..=0 {
        for z in -1..=0 {
            let name = format!("r.{x}.{z}.mca");
            let (len, hash) = file_hash(&directory.join(&name), Phase::Validate)?;
            rows.push((name, len, hash));
        }
    }
    Ok(rows)
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

#[derive(Clone, Copy)]
enum SlotCaptureOrigin {
    Initial,
    Restored,
    Advanced,
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
    /// Read-only native region decoding, with typed checks before native readers
    /// can coerce or omit malformed timestamps/owner lists. No SaveDir migration.
    fn checked_slot_columns(
        &self,
        root: &Path,
        binding: &NativeBinding,
        time: i64,
        phase: SlotPhase,
    ) -> CResult<Vec<std::sync::Arc<horizon_client::chunk::Chunk>>> {
        use horizon_core::position::ChunkPos;
        use horizon_world::region::RegionFile;
        // RegionFile::read may follow a native external c.X.Z.mcc record. Check
        // EVERY source member's type/link/length before the first native read.
        let _ = tree(root, &["session.lock"], Phase::Validate)?;
        let baseline = match phase {
            SlotPhase::Three => Some(time),
            SlotPhase::Four => time.checked_sub(1),
        }
        .filter(|time| *time >= 0)
        .ok_or_else(|| profile_error("native baseline clock overflow"))?;
        let level = strict_player(&bounded_read(
            &root.join("level.dat"),
            2 << 20,
            Phase::Recover,
        )?)?;
        if level.get("Data").and_then(|value| value.get("Time")) != Some(&Tag::Long(time)) {
            return Err(profile_error("typed native Level.Time mismatch"));
        }
        let dimension = horizon_client::save::dimension_dir(root, Dim::Overworld.id());
        let mut world = horizon_client::world::World::new(self.res.facts().air, 0);
        world.game_time = time;
        let mut columns = Vec::new();
        for z in -1i32..=1 {
            for x in -1i32..=1 {
                let path = dimension.join(format!(
                    "region/r.{}.{}.mca",
                    x.div_euclid(32),
                    z.div_euclid(32)
                ));
                let _ = file_hash(&path, Phase::Recover)?;
                let mut region = RegionFile::open(&path)
                    .map_err(|error| io_error(Phase::Recover, &path, error))?;
                let record = region
                    .read(ChunkPos::new(x, z))
                    .map_err(|error| io_error(Phase::Recover, &path, error))?
                    .ok_or_else(|| profile_error("native nine-column record absent"))?;
                let named = record
                    .nbt(Limits {
                        max_bytes: 2 << 20,
                        max_nodes: NODE_LIMIT,
                        max_depth: 32,
                    })
                    .map_err(profile_error)?;
                let tag = &named.tag;
                if tag.get("xPos") != Some(&Tag::Int(x))
                    || tag.get("zPos") != Some(&Tag::Int(z))
                    || tag.get("yPos") != Some(&Tag::Int(horizon_client::chunk::MIN_Y >> 4))
                    || tag.get("Status") != Some(&Tag::String("minecraft:full".into()))
                    || tag.get("DataVersion") != Some(&Tag::Int(horizon_world::DATA_VERSION))
                    || tag.get("LastUpdate") != Some(&Tag::Long(baseline))
                    || tag.get("isLightOn") != Some(&Tag::Byte(1))
                    || tag.get("InhabitedTime") != Some(&Tag::Long(0))
                {
                    return Err(profile_error("typed native column metadata mismatch"));
                }
                native_empty_owner_lists(tag)?;
                if tag
                    .get("entities")
                    .is_some_and(|value| !matches!(value,Tag::List(values) if values.is_empty()))
                {
                    return Err(profile_error("native entity owner not empty"));
                }
                let mut chunk = horizon_client::save::decode(
                    &named,
                    self.pack.assets.registry,
                    self.res.biomes(),
                )
                .ok_or_else(|| profile_error("native column decode failed"))?;
                horizon_client::save::load_light(tag, &mut chunk, true);
                let chunk = std::sync::Arc::new(chunk);
                world.chunks.insert((x, z), chunk.clone());
                columns.push(chunk);
            }
        }
        let owner = match phase {
            SlotPhase::Three => world.checkpoint_world_owner_for_native_empty_roots(),
            SlotPhase::Four => world.checkpoint_world_owner_for_native_previous_tick_roots(),
        };
        if owner.is_some() {
            return Err(profile_error("native selected column owner/root mismatch"));
        }
        let host = target(Dim::Overworld)?;
        if world.block(host.position[0], host.position[1], host.position[2]) != binding.state_id() {
            return Err(profile_error("native independent target state mismatch"));
        }
        Ok(columns)
    }
    fn checked_advance_baseline(
        &self,
        binding: &NativeBinding,
        live: &CounterFrameV2,
        live_slot: &[u8],
    ) -> CResult<InheritedSlotFiles> {
        let files = InheritedSlotFiles::read(&self.world_root, Phase::Validate)?;
        let baseline = CounterFrameV2::decode(&files.counter)
            .map_err(|error| native_error(Phase::Validate, error))?;
        let slot = decode_slot_frame(&files.slot, self.res.items())
            .map_err(|error| native_error(Phase::Validate, error))?;
        let current = decode_slot_frame(live_slot, self.res.items())
            .map_err(|error| native_error(Phase::Validate, error))?;
        let world = self
            .world
            .counter_domain_snapshot()
            .ok_or_else(|| profile_error("advanced domain absent"))?
            .world;
        let host = target(self.dim)?;
        correlate_slot_for(&slot, &baseline, world, &host, SlotPhase::Three)?;
        correlate_slot_for(&current, live, world, &host, SlotPhase::Four)?;
        let previous = self
            .world
            .game_time
            .checked_sub(1)
            .filter(|time| *time >= 0)
            .ok_or_else(|| profile_error("advanced baseline clock overflow"))?;
        let generation = |frame: &CounterFrameV2| match frame.placement {
            Placement::Active { generation, .. } => Some(generation),
            _ => None,
        };
        if baseline.saved_game_time != previous
            || live.saved_game_time != self.world.game_time
            || baseline.format != live.format
            || baseline.kind != live.kind
            || baseline.world_uuid != live.world_uuid
            || baseline.dimension != live.dimension
            || baseline.position != live.position
            || baseline.allocation_high_water != live.allocation_high_water
            || baseline.package_identity != live.package_identity
            || baseline.type_id != live.type_id
            || baseline.save_schema != live.save_schema
            || baseline.binding != live.binding
            || baseline.observed_state != live.observed_state
            || baseline.last_retired != live.last_retired
            || generation(&baseline) != generation(live)
            || baseline
                .encode()
                .map_err(|error| native_error(Phase::Validate, error))?
                != files.counter
            || baseline.package_identity != digest_identity(binding.package_identity())
            || baseline.type_id != binding.type_id()
            || baseline.binding.block_id != binding.block_id()
            || self
                .world
                .block(host.position[0], host.position[1], host.position[2])
                != binding.state_id()
        {
            return Err(profile_error("baseline3/current4 native context mismatch"));
        }
        let a = slot.view();
        let b = current.view();
        if a.world != b.world
            || a.dimension != b.dimension
            || a.position != b.position
            || a.package_identity != b.package_identity
            || a.type_id != b.type_id
            || a.block_id != b.block_id
            || a.save_schema != b.save_schema
            || a.observed_state != b.observed_state
            || a.machine_high_water != b.machine_high_water
            || a.slot_high_water != b.slot_high_water
        {
            return Err(profile_error("baseline/current slot headers mismatch"));
        }
        // Both strict frames have already proved the COMPLETE supported payload:
        // plain stone7, damage0, no components, revision1, slot incarnation/head1,
        // and identical placement generation through the native counter frames.
        self.checked_slot_columns(&self.world_root, binding, previous, SlotPhase::Three)?;
        Ok(files)
    }
    /// Full native projection validation for the fixed E1 fixture path; never takes an owner.
    pub(in crate::game) fn slot_epoch_projection(&self) -> CResult<(CounterFrameV2, Vec<u8>)> {
        self.slot_projection_for(SlotPhase::Three)
    }
    pub(in crate::game) fn slot_advance_projection(&self) -> CResult<(CounterFrameV2, Vec<u8>)> {
        self.slot_projection_for(SlotPhase::Four)
    }
    fn slot_projection_for(&self, phase: SlotPhase) -> CResult<(CounterFrameV2, Vec<u8>)> {
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
        correlate_slot_for(&slot, &counter, domain.world, &host, phase)?;
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
        self.capture_slot_from_origin(request, SlotCaptureOrigin::Initial)
    }
    pub(in crate::game) fn recapture_slot_fixture_epoch(
        &mut self,
        request: CaptureRequest,
    ) -> CaptureCompletion {
        self.capture_slot_from_origin(request, SlotCaptureOrigin::Restored)
    }
    pub(in crate::game) fn capture_advanced_slot_fixture_epoch(
        &mut self,
        request: CaptureRequest,
    ) -> CaptureCompletion {
        self.capture_slot_from_origin(request, SlotCaptureOrigin::Advanced)
    }
    fn checked_inherited_slot_files(
        &self,
        binding: &NativeBinding,
        live: &CounterFrameV2,
        live_slot: &[u8],
    ) -> CResult<InheritedSlotFiles> {
        let inherited = InheritedSlotFiles::read(&self.world_root, Phase::Validate)?;
        let counter = CounterFrameV2::decode(&inherited.counter)
            .map_err(|error| native_error(Phase::Validate, error))?;
        let slot = decode_slot_frame(&inherited.slot, self.res.items())
            .map_err(|error| native_error(Phase::Validate, error))?;
        let expected_world = self
            .world
            .counter_domain_snapshot()
            .ok_or_else(|| profile_error("restored native domain absent"))?
            .world;
        let host = target(self.dim)?;
        correlate_slot(&slot, &counter, expected_world, &host)?;
        if counter != *live
            || counter
                .encode()
                .map_err(|error| native_error(Phase::Validate, error))?
                != inherited.counter
            || inherited.slot != live_slot
            || counter.package_identity != digest_identity(binding.package_identity())
            || counter.type_id != binding.type_id()
            || counter.binding.block_id != binding.block_id()
            || counter.save_schema != 1
            || self
                .world
                .block(host.position[0], host.position[1], host.position[2])
                != binding.state_id()
        {
            return Err(invalid(
                Phase::Validate,
                "inherited sidecars/live native context mismatch",
            ));
        }
        Ok(inherited)
    }
    fn capture_slot_from_origin(
        &mut self,
        request: CaptureRequest,
        origin: SlotCaptureOrigin,
    ) -> CaptureCompletion {
        let profile = match origin {
            SlotCaptureOrigin::Initial => CaptureProfile::NativeSlotPatch,
            SlotCaptureOrigin::Restored => CaptureProfile::NativeSlotRecapture,
            SlotCaptureOrigin::Advanced => CaptureProfile::NativeSlotAdvance,
        };
        let phase = match origin {
            SlotCaptureOrigin::Advanced => SlotPhase::Four,
            _ => SlotPhase::Three,
        };
        let recovery_profile = match origin {
            SlotCaptureOrigin::Advanced => RecoveryProfile::AdvancedSlot,
            _ => RecoveryProfile::ActiveSlot,
        };
        if matches!(origin, SlotCaptureOrigin::Advanced) && request.epoch != 3 {
            return Err(profile_error("fixed advanced epoch3 required"));
        }
        self.capture_profile_checks(profile)?;
        let binding = fixture_binding(self)?;
        if request.identity.counter_content != digest_identity(binding.package_identity()) {
            return Err(profile_error("actual package identity mismatch"));
        }
        let (counter, slot_bytes) = self.slot_projection_for(phase)?;
        let inherited = match origin {
            SlotCaptureOrigin::Initial => None,
            SlotCaptureOrigin::Restored => {
                Some(self.checked_inherited_slot_files(&binding, &counter, &slot_bytes)?)
            }
            SlotCaptureOrigin::Advanced => {
                Some(self.checked_advance_baseline(&binding, &counter, &slot_bytes)?)
            }
        };
        let regions = if matches!(origin, SlotCaptureOrigin::Advanced) {
            Some(slot_region_fingerprint(&self.world_root)?)
        } else {
            None
        };
        let material = self.profile_material(request, profile)?;
        if let Some(files) = &inherited {
            files.require_unchanged(&material.source, Phase::Save)?;
        }
        if matches!(origin, SlotCaptureOrigin::Advanced) {
            self.checked_slot_columns(&material.source, &binding, counter.saved_game_time, phase)?;
        }
        if let Some(regions) = regions
            && slot_region_fingerprint(&material.source)? != regions
        {
            return Err(invalid(Phase::Save, "clean native region bytes changed"));
        }
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
        let (after_counter, after_slot) = self.slot_projection_for(phase)?;
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
        let source = material.source.clone();
        let mut image = material.copy()?;
        image.overlay()?;
        let counter_bytes = counter
            .encode()
            .map_err(|error| native_error(Phase::Overlay, error))?;
        if let Some(files) = &inherited {
            image.replace_inherited_slot_files(&source, files, &counter_bytes, &slot_bytes)?;
        } else {
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
        }
        image.sync_members()?;
        image.write_manifest_temp_for(recovery_profile)?;
        image.finish_manifest()?;
        let verified =
            verify_complete_envelope(&image.root, image.request.epoch, &image.request.identity)?;
        check_recovery_profile(
            &verified.manifest,
            verified.slot_manifest.as_ref(),
            recovery_profile,
        )?;
        read_slot_image_for(
            &image.root,
            &verified.manifest,
            &binding,
            self.res.items(),
            counter.world_uuid,
            &host,
            phase,
        )?;
        if matches!(origin, SlotCaptureOrigin::Advanced) {
            self.checked_slot_columns(
                &image.root.join("world"),
                &binding,
                counter.saved_game_time,
                phase,
            )?;
        }
        image.rename_published_for(recovery_profile)?.acknowledge()
    }
    fn install_slot_epoch(
        &mut self,
        binding: &NativeBinding,
        world: WorldId,
        counter: &CounterFrameV2,
        slot: DecodedSlotFrame,
    ) -> std::result::Result<(RuntimeCounterKey, SlotKey), SlotInstallFailure> {
        self.install_slot_for(binding, world, counter, slot, SlotPhase::Three)
    }
    fn install_slot_for(
        &mut self,
        binding: &NativeBinding,
        world: WorldId,
        counter: &CounterFrameV2,
        slot: DecodedSlotFrame,
        phase: SlotPhase,
    ) -> std::result::Result<(RuntimeCounterKey, SlotKey), SlotInstallFailure> {
        let host = match target(self.dim) {
            Ok(host) => host,
            Err(error) => return Err(SlotInstallFailure { error, slot }),
        };
        if let Err(error) = correlate_slot_for(&slot, counter, world, &host, phase) {
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
#[path = "slot_advance_capture_tests.rs"]
mod advance_tests;
#[path = "native_write_fence_tests.rs"]
mod native_write_fence_tests;
#[path = "slot_recapture_integration_tests.rs"]
mod recapture_tests;
#[path = "slot_epoch_integration_tests.rs"]
mod tests;
#[path = "used_streamer_tick_tests.rs"]
mod used_streamer_tests;
