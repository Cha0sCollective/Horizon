//! Explicit storage for the current Game stream and the dormant retained owner.
//!
//! Every production Game factory still installs Existing. That includes legacy
//! remote NoTerrain pools: this variant is not native checkpoint provenance.
//! A later retained remote Game recipe must replace its reachable assertions
//! with checked dispatch before it can install the other variant.
#[cfg(feature = "paired-packages")]
use horizon_client::streaming::RetainedRemoteStreaming;
use horizon_client::streaming::Streaming;

pub(super) enum GameStreaming {
    Existing(Box<Streaming>),
    #[cfg(feature = "paired-packages")]
    #[cfg_attr(not(test), allow(dead_code))] // Deliberately no production Game installer.
    RetainedRemote(Box<RetainedRemoteStreaming>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ExistingUnavailable;

impl GameStreaming {
    pub(super) fn from_existing(streaming: Streaming) -> Self {
        Self::Existing(Box::new(streaming))
    }

    pub(super) fn as_existing(&self) -> Result<&Streaming, ExistingUnavailable> {
        match self {
            Self::Existing(streaming) => Ok(streaming),
            #[cfg(feature = "paired-packages")]
            Self::RetainedRemote(_) => Err(ExistingUnavailable),
        }
    }

    pub(super) fn as_existing_mut(&mut self) -> Result<&mut Streaming, ExistingUnavailable> {
        match self {
            Self::Existing(streaming) => Ok(streaming),
            #[cfg(feature = "paired-packages")]
            Self::RetainedRemote(_) => Err(ExistingUnavailable),
        }
    }

    pub(super) fn expect_existing(&self) -> &Streaming {
        self.as_existing()
            .expect("all current Game factories install Existing streaming")
    }

    pub(super) fn expect_existing_mut(&mut self) -> &mut Streaming {
        self.as_existing_mut()
            .expect("all current Game factories install Existing streaming")
    }

    #[cfg_attr(not(all(test, feature = "paired-packages")), allow(dead_code))] // Checked consuming boundary for later adoption.
    pub(super) fn try_into_existing(self) -> Result<Streaming, Self> {
        match self {
            Self::Existing(streaming) => Ok(*streaming),
            #[cfg(feature = "paired-packages")]
            original @ Self::RetainedRemote(_) => Err(original),
        }
    }

    #[cfg_attr(not(all(test, feature = "paired-packages")), allow(dead_code))] // No current factory installs the refused variant.
    #[allow(clippy::result_large_err)] // Refusal returns the original incoming owner intact.
    pub(super) fn try_replace_existing(
        &mut self,
        incoming: Streaming,
    ) -> Result<Streaming, Streaming> {
        match self {
            Self::Existing(current) => Ok(*std::mem::replace(current, Box::new(incoming))),
            #[cfg(feature = "paired-packages")]
            Self::RetainedRemote(_) => Err(incoming),
        }
    }

    pub(super) fn try_swap_existing(
        &mut self,
        parked: &mut Streaming,
    ) -> Result<(), ExistingUnavailable> {
        let current = self.as_existing_mut()?;
        std::mem::swap(current, parked);
        Ok(())
    }
}

#[cfg(all(test, feature = "paired-packages"))]
mod tests {
    use super::*;
    use crate::game::{Game, integrated::transition_ownership_tests::Fixture};
    use horizon_client::{streaming::RemoteCloseStatus, testgen::TestTerrain};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    fn original(game: &Game<'_>) -> Streaming {
        Streaming::new(
            Arc::new(TestTerrain::new(game.pack.assets.registry, game.res.biomes()).unwrap()),
            game.pack.context.clone(),
            1,
        )
    }

    #[test]
    #[ignore = "pinned Game-bin resources: original production stream extraction and replacement"]
    fn existing_owner_extraction_and_replacement_preserve_original_sources() {
        let fixture = Fixture::new("stream-owner-replace");
        let game = fixture.game();
        let first = original(&game);
        let first_source = first.lod_source();
        let first_work = first.observe_native_work().origins;
        let second = original(&game);
        let second_source = second.lod_source();
        let second_work = second.observe_native_work().origins;
        assert!(!Arc::ptr_eq(&first_source, &second_source));
        let mut slot = GameStreaming::from_existing(first);
        assert!(Arc::ptr_eq(
            &first_source,
            &slot.as_existing().unwrap().lod_source()
        ));
        slot.as_existing_mut().unwrap().center = (19, -7);
        let old = match slot.try_replace_existing(second) {
            Ok(old) => old,
            Err(_) => panic!("original Existing replacement refused"),
        };
        assert_eq!(old.center, (19, -7));
        assert!(Arc::ptr_eq(&first_source, &old.lod_source()));
        assert_eq!(old.observe_native_work().origins, first_work);
        assert!(Arc::ptr_eq(
            &second_source,
            &slot.as_existing().unwrap().lod_source()
        ));
        let current = match slot.try_into_existing() {
            Ok(current) => current,
            Err(_) => panic!("original Existing extraction refused"),
        };
        assert!(Arc::ptr_eq(&second_source, &current.lod_source()));
        assert_eq!(current.observe_native_work().origins, second_work);
        // These remain actual legacy production owners; their ordinary disposal
        // owns their original workers. No replacement pool stands in for either.
        drop(old);
        drop(current);
    }

    #[test]
    #[ignore = "pinned Game-bin resources: original active/parked owner swap"]
    fn existing_owner_swap_preserves_original_sources_and_native_observations() {
        let fixture = Fixture::new("stream-owner-swap");
        let game = fixture.game();
        let mut first = original(&game);
        first.center = (8, 13);
        first.forced.insert((2, 3));
        let first_source = first.lod_source();
        let first_work = first.observe_native_work().origins;
        let mut parked = original(&game);
        parked.center = (-5, 9);
        parked.forced.insert((7, 11));
        let parked_source = parked.lod_source();
        let parked_work = parked.observe_native_work().origins;
        let mut slot = GameStreaming::from_existing(first);
        slot.try_swap_existing(&mut parked).unwrap();
        assert!(Arc::ptr_eq(
            &parked_source,
            &slot.as_existing().unwrap().lod_source()
        ));
        assert!(Arc::ptr_eq(&first_source, &parked.lod_source()));
        assert_eq!(slot.as_existing().unwrap().center, (-5, 9));
        assert_eq!(parked.center, (8, 13));
        assert!(slot.as_existing().unwrap().forced.contains(&(7, 11)));
        assert!(parked.forced.contains(&(2, 3)));
        assert_eq!(
            slot.as_existing().unwrap().observe_native_work().origins,
            parked_work
        );
        assert_eq!(parked.observe_native_work().origins, first_work);
        slot.try_swap_existing(&mut parked).unwrap();
        assert!(Arc::ptr_eq(
            &first_source,
            &slot.as_existing().unwrap().lod_source()
        ));
        assert!(Arc::ptr_eq(&parked_source, &parked.lod_source()));
        drop(slot);
        drop(parked);
    }

    #[test]
    #[ignore = "pinned Game-bin resources: dormant original retained owner refuses existing operations"]
    fn dormant_remote_refuses_existing_operations_without_losing_original_owners() {
        let fixture = Fixture::new("stream-owner-refuse");
        let game = fixture.game();
        let remote = match RetainedRemoteStreaming::prepare(game.pack.context.clone(), 1) {
            Ok(remote) => remote,
            Err(_) => panic!("actual inert retained preparation refused"),
        };
        let mut slot = GameStreaming::RetainedRemote(Box::new(remote));
        assert!(matches!(slot.as_existing(), Err(ExistingUnavailable)));
        assert!(matches!(slot.as_existing_mut(), Err(ExistingUnavailable)));
        let incoming = original(&game);
        let source = incoming.lod_source();
        let work = incoming.observe_native_work().origins;
        let mut incoming = match slot.try_replace_existing(incoming) {
            Err(original) => original,
            Ok(_) => panic!("retained slot accepted an Existing replacement"),
        };
        assert!(Arc::ptr_eq(&source, &incoming.lod_source()));
        assert_eq!(incoming.observe_native_work().origins, work);
        assert_eq!(
            slot.try_swap_existing(&mut incoming),
            Err(ExistingUnavailable)
        );
        assert!(Arc::ptr_eq(&source, &incoming.lod_source()));
        assert_eq!(incoming.observe_native_work().origins, work);
        let slot = match slot.try_into_existing() {
            Err(original) => original,
            Ok(_) => panic!("retained slot allowed Existing extraction"),
        };
        let mut remote = match slot {
            GameStreaming::RetainedRemote(original) => *original,
            GameStreaming::Existing(_) => panic!("refusal replaced the original slot variant"),
        };
        let observation = remote
            .try_observe()
            .expect("uncontended original prepared owner");
        assert!(!observation.started && !observation.sealed);
        assert_eq!(observation.owned, 0);
        remote.begin_close(Instant::now() + Duration::from_secs(5));
        assert!(matches!(
            remote.poll_close(),
            RemoteCloseStatus::Quiescent { .. }
        ));
        let closed = match remote.try_closed() {
            Ok(closed) => closed,
            Err(_) => panic!("original inert retained owner failed checked close"),
        };
        let observation = closed
            .try_observe()
            .expect("original closed owner observation");
        assert!(!observation.started && observation.sealed);
        assert_eq!(observation.owned, 0);
        assert_eq!(observation.joined, 0);
        drop(closed);
        drop(incoming);
    }
}
