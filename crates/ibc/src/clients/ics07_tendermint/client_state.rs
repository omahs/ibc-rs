use crate::prelude::*;

use core::convert::{TryFrom, TryInto};
use core::time::Duration;

use ibc_proto::google::protobuf::Any;
use ibc_proto::ibc::core::client::v1::Height as RawHeight;
use ibc_proto::ibc::core::commitment::v1::MerkleProof as RawMerkleProof;
use ibc_proto::ibc::lightclients::tendermint::v1::ClientState as RawTmClientState;
use ibc_proto::protobuf::Protobuf;
use prost::Message;
use tendermint::chain::id::MAX_LENGTH as MaxChainIdLen;
use tendermint::trust_threshold::TrustThresholdFraction as TendermintTrustThresholdFraction;
use tendermint_light_client_verifier::options::Options;
use tendermint_light_client_verifier::types::{TrustedBlockState, UntrustedBlockState};
use tendermint_light_client_verifier::{ProdVerifier, Verifier};

use crate::clients::ics07_tendermint::consensus_state::ConsensusState as TmConsensusState;
use crate::clients::ics07_tendermint::error::{Error, IntoResult};
use crate::clients::ics07_tendermint::header::{Header as TmHeader, Header};
use crate::clients::ics07_tendermint::misbehaviour::Misbehaviour as TmMisbehaviour;
use crate::core::ics02_client::client_state::{
    ClientState as Ics2ClientState, UpdatedState, UpgradeOptions as CoreUpgradeOptions,
};
use crate::core::ics02_client::client_type::ClientType;
use crate::core::ics02_client::consensus_state::ConsensusState;
use crate::core::ics02_client::context::ClientReader;
use crate::core::ics02_client::error::ClientError;
use crate::core::ics02_client::trust_threshold::TrustThreshold;
use crate::core::ics03_connection::connection::ConnectionEnd;
use crate::core::ics04_channel::commitment::{AcknowledgementCommitment, PacketCommitment};
use crate::core::ics04_channel::context::ChannelReader;
use crate::core::ics04_channel::packet::Sequence;
use crate::core::ics23_commitment::commitment::{
    CommitmentPrefix, CommitmentProofBytes, CommitmentRoot,
};
use crate::core::ics23_commitment::merkle::{apply_prefix, MerkleProof};
use crate::core::ics23_commitment::specs::ProofSpecs;
use crate::core::ics24_host::identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId};
use crate::core::ics24_host::path::{
    AcksPath, ChannelEndsPath, ClientConsensusStatePath, ClientStatePath, CommitmentsPath,
    ConnectionsPath, ReceiptsPath, SeqRecvsPath,
};
use crate::core::ics24_host::Path;
use crate::timestamp::{Timestamp, ZERO_DURATION};
use crate::Height;

use super::client_type as tm_client_type;

#[cfg(feature = "val_exec_ctx")]
use crate::core::context::ContextError;
#[cfg(feature = "val_exec_ctx")]
use crate::core::ValidationContext;

pub const TENDERMINT_CLIENT_STATE_TYPE_URL: &str = "/ibc.lightclients.tendermint.v1.ClientState";

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientState {
    pub chain_id: ChainId,
    pub trust_level: TrustThreshold,
    pub trusting_period: Duration,
    pub unbonding_period: Duration,
    max_clock_drift: Duration,
    latest_height: Height,
    pub proof_specs: ProofSpecs,
    pub upgrade_path: Vec<String>,
    allow_update: AllowUpdate,
    frozen_height: Option<Height>,
    #[cfg_attr(feature = "serde", serde(skip))]
    verifier: ProdVerifier,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AllowUpdate {
    pub after_expiry: bool,
    pub after_misbehaviour: bool,
}

impl ClientState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_id: ChainId,
        trust_level: TrustThreshold,
        trusting_period: Duration,
        unbonding_period: Duration,
        max_clock_drift: Duration,
        latest_height: Height,
        proof_specs: ProofSpecs,
        upgrade_path: Vec<String>,
        allow_update: AllowUpdate,
        frozen_height: Option<Height>,
    ) -> Result<ClientState, Error> {
        if chain_id.as_str().len() > MaxChainIdLen {
            return Err(Error::ChainIdTooLong {
                chain_id: chain_id.clone(),
                len: chain_id.as_str().len(),
                max_len: MaxChainIdLen,
            });
        }

        // `TrustThreshold` is guaranteed to be in the range `[0, 1)`, but a `TrustThreshold::ZERO`
        // value is invalid in this context
        if trust_level == TrustThreshold::ZERO {
            return Err(Error::InvalidTrustThreshold {
                reason: "ClientState trust-level cannot be zero".to_string(),
            });
        }

        let _ = TendermintTrustThresholdFraction::new(
            trust_level.numerator(),
            trust_level.denominator(),
        )
        .map_err(Error::InvalidTendermintTrustThreshold)?;

        // Basic validation of trusting period and unbonding period: each should be non-zero.
        if trusting_period <= Duration::new(0, 0) {
            return Err(Error::InvalidTrustThreshold {
                reason: format!(
                    "ClientState trusting period ({trusting_period:?}) must be greater than zero"
                ),
            });
        }

        if unbonding_period <= Duration::new(0, 0) {
            return Err(Error::InvalidTrustThreshold {
                reason: format!(
                    "ClientState unbonding period ({unbonding_period:?}) must be greater than zero"
                ),
            });
        }

        if trusting_period >= unbonding_period {
            return Err(Error::InvalidTrustThreshold {
                reason: format!(
                "ClientState trusting period ({trusting_period:?}) must be smaller than unbonding period ({unbonding_period:?})"
            ),
            });
        }

        if max_clock_drift <= Duration::new(0, 0) {
            return Err(Error::InvalidMaxClockDrift {
                reason: "ClientState max-clock-drift must be greater than zero".to_string(),
            });
        }

        if latest_height.revision_number() != chain_id.version() {
            return Err(Error::InvalidLatestHeight {
                reason: "ClientState latest-height revision number must match chain-id version"
                    .to_string(),
            });
        }

        // Disallow empty proof-specs
        if proof_specs.is_empty() {
            return Err(Error::Validation {
                reason: "ClientState proof-specs cannot be empty".to_string(),
            });
        }

        // `upgrade_path` itself may be empty, but if not then each key must be non-empty
        for (idx, key) in upgrade_path.iter().enumerate() {
            if key.trim().is_empty() {
                return Err(Error::Validation {
                    reason: format!(
                        "ClientState upgrade-path key at index {idx:?} cannot be empty"
                    ),
                });
            }
        }

        Ok(Self {
            chain_id,
            trust_level,
            trusting_period,
            unbonding_period,
            max_clock_drift,
            latest_height,
            proof_specs,
            upgrade_path,
            allow_update,
            frozen_height,
            verifier: ProdVerifier::default(),
        })
    }

    pub fn latest_height(&self) -> Height {
        self.latest_height
    }

    pub fn with_header(self, h: TmHeader) -> Result<Self, Error> {
        Ok(ClientState {
            latest_height: Height::new(
                self.latest_height.revision_number(),
                h.signed_header.header.height.into(),
            )
            .map_err(|_| Error::InvalidHeaderHeight {
                height: h.signed_header.header.height.value(),
            })?,
            ..self
        })
    }

    pub fn with_frozen_height(self, h: Height) -> Self {
        Self {
            frozen_height: Some(h),
            ..self
        }
    }

    /// Get the refresh time to ensure the state does not expire
    pub fn refresh_time(&self) -> Option<Duration> {
        Some(2 * self.trusting_period / 3)
    }

    /// Helper method to produce a [`Options`] struct for use in
    /// Tendermint-specific light client verification.
    pub fn as_light_client_options(&self) -> Result<Options, Error> {
        Ok(Options {
            trust_threshold: self.trust_level.try_into().map_err(|e: ClientError| {
                Error::InvalidTrustThreshold {
                    reason: e.to_string(),
                }
            })?,
            trusting_period: self.trusting_period,
            clock_drift: self.max_clock_drift,
        })
    }

    /// Verify the time and height delays
    pub fn verify_delay_passed(
        current_time: Timestamp,
        current_height: Height,
        processed_time: Timestamp,
        processed_height: Height,
        delay_period_time: Duration,
        delay_period_blocks: u64,
    ) -> Result<(), Error> {
        let earliest_time =
            (processed_time + delay_period_time).map_err(Error::TimestampOverflow)?;
        if !(current_time == earliest_time || current_time.after(&earliest_time)) {
            return Err(Error::NotEnoughTimeElapsed {
                current_time,
                earliest_time,
            });
        }

        let earliest_height = processed_height.add(delay_period_blocks);
        if current_height < earliest_height {
            return Err(Error::NotEnoughBlocksElapsed {
                current_height,
                earliest_height,
            });
        }

        Ok(())
    }

    /// Verify that the client is at a sufficient height and unfrozen at the given height
    pub fn verify_height(&self, height: Height) -> Result<(), Error> {
        if self.latest_height < height {
            return Err(Error::InsufficientHeight {
                latest_height: self.latest_height(),
                target_height: height,
            });
        }

        match self.frozen_height {
            Some(frozen_height) if frozen_height <= height => Err(Error::ClientFrozen {
                frozen_height,
                target_height: height,
            }),
            _ => Ok(()),
        }
    }

    fn check_header_validator_set(
        trusted_consensus_state: &TmConsensusState,
        header: &Header,
    ) -> Result<(), ClientError> {
        let trusted_val_hash = header.trusted_validator_set.hash();

        if trusted_consensus_state.next_validators_hash != trusted_val_hash {
            return Err(Error::MisbehaviourTrustedValidatorHashMismatch {
                trusted_validator_set: header.trusted_validator_set.validators().clone(),
                next_validators_hash: trusted_consensus_state.next_validators_hash,
                trusted_val_hash,
            }
            .into());
        }

        Ok(())
    }

    fn check_header_and_validator_set(
        &self,
        header: &Header,
        consensus_state: &TmConsensusState,
        current_timestamp: Timestamp,
    ) -> Result<(), ClientError> {
        Self::check_header_validator_set(consensus_state, header)?;

        let duration_since_consensus_state = current_timestamp
            .duration_since(&consensus_state.timestamp())
            .ok_or_else(|| ClientError::InvalidConsensusStateTimestamp {
                time1: consensus_state.timestamp(),
                time2: current_timestamp,
            })?;

        if duration_since_consensus_state >= self.trusting_period {
            return Err(Error::ConsensusStateTimestampGteTrustingPeriod {
                duration_since_consensus_state,
                trusting_period: self.trusting_period,
            }
            .into());
        }

        let untrusted_state = header.as_untrusted_block_state();
        let chain_id = self.chain_id.clone().into();
        let trusted_state = header.as_trusted_block_state(consensus_state, &chain_id)?;
        let options = self.as_light_client_options()?;

        self.verifier
            .validate_against_trusted(
                &untrusted_state,
                &trusted_state,
                &options,
                current_timestamp.into_tm_time().unwrap(),
            )
            .into_result()?;

        Ok(())
    }

    fn verify_header_commit_against_trusted(
        &self,
        header: &Header,
        consensus_state: &TmConsensusState,
    ) -> Result<(), ClientError> {
        let untrusted_state = header.as_untrusted_block_state();
        let chain_id = self.chain_id.clone().into();
        let trusted_state = Header::as_trusted_block_state(header, consensus_state, &chain_id)?;
        let options = self.as_light_client_options()?;

        self.verifier
            .verify_commit_against_trusted(&untrusted_state, &trusted_state, &options)
            .into_result()?;

        Ok(())
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeOptions {
    pub unbonding_period: Duration,
}

impl CoreUpgradeOptions for UpgradeOptions {}

impl Ics2ClientState for ClientState {
    fn chain_id(&self) -> ChainId {
        self.chain_id.clone()
    }

    fn client_type(&self) -> ClientType {
        tm_client_type()
    }

    fn latest_height(&self) -> Height {
        self.latest_height
    }

    fn frozen_height(&self) -> Option<Height> {
        self.frozen_height
    }

    fn upgrade(
        &mut self,
        upgrade_height: Height,
        upgrade_options: &dyn CoreUpgradeOptions,
        chain_id: ChainId,
    ) {
        let upgrade_options = upgrade_options
            .as_any()
            .downcast_ref::<UpgradeOptions>()
            .expect("UpgradeOptions not of type Tendermint");

        // Reset custom fields to zero values
        self.trusting_period = ZERO_DURATION;
        self.trust_level = TrustThreshold::ZERO;
        self.allow_update.after_expiry = false;
        self.allow_update.after_misbehaviour = false;
        self.frozen_height = None;
        self.max_clock_drift = ZERO_DURATION;

        // Upgrade the client state
        self.latest_height = upgrade_height;
        self.unbonding_period = upgrade_options.unbonding_period;
        self.chain_id = chain_id;
    }

    fn expired(&self, elapsed: Duration) -> bool {
        elapsed > self.trusting_period
    }

    fn initialise(&self, consensus_state: Any) -> Result<Box<dyn ConsensusState>, ClientError> {
        TmConsensusState::try_from(consensus_state).map(TmConsensusState::into_box)
    }

    fn check_header_and_update_state(
        &self,
        ctx: &dyn ClientReader,
        client_id: ClientId,
        header: Any,
    ) -> Result<UpdatedState, ClientError> {
        fn maybe_consensus_state(
            ctx: &dyn ClientReader,
            client_id: &ClientId,
            height: &Height,
        ) -> Result<Option<Box<dyn ConsensusState>>, ClientError> {
            match ctx.consensus_state(client_id, height) {
                Ok(cs) => Ok(Some(cs)),
                Err(e) => match e {
                    ClientError::ConsensusStateNotFound {
                        client_id: _,
                        height: _,
                    } => Ok(None),
                    _ => Err(e),
                },
            }
        }

        let client_state = downcast_tm_client_state(self)?.clone();
        let header = TmHeader::try_from(header)?;

        if header.height().revision_number() != client_state.chain_id().version() {
            return Err(ClientError::ClientSpecific {
                description: Error::MismatchedRevisions {
                    current_revision: client_state.chain_id().version(),
                    update_revision: header.height().revision_number(),
                }
                .to_string(),
            });
        }

        // Check if a consensus state is already installed; if so it should
        // match the untrusted header.
        let header_consensus_state = TmConsensusState::from(header.clone());
        let existing_consensus_state =
            match maybe_consensus_state(ctx, &client_id, &header.height())? {
                Some(cs) => {
                    let cs = downcast_tm_consensus_state(cs.as_ref())?;
                    // If this consensus state matches, skip verification
                    // (optimization)
                    if cs == header_consensus_state {
                        // Header is already installed and matches the incoming
                        // header (already verified)
                        return Ok(UpdatedState {
                            client_state: client_state.into_box(),
                            consensus_state: cs.into_box(),
                        });
                    }
                    Some(cs)
                }
                None => None,
            };

        let trusted_consensus_state = downcast_tm_consensus_state(
            ctx.consensus_state(&client_id, &header.trusted_height)?
                .as_ref(),
        )?;

        let trusted_state = TrustedBlockState {
            chain_id: &self.chain_id.clone().into(),
            header_time: trusted_consensus_state.timestamp,
            height: header
                .trusted_height
                .revision_height()
                .try_into()
                .map_err(|_| ClientError::ClientSpecific {
                    description: Error::InvalidHeaderHeight {
                        height: header.trusted_height.revision_height(),
                    }
                    .to_string(),
                })?,
            next_validators: &header.trusted_validator_set,
            next_validators_hash: trusted_consensus_state.next_validators_hash,
        };

        let untrusted_state = UntrustedBlockState {
            signed_header: &header.signed_header,
            validators: &header.validator_set,
            // NB: This will skip the
            // VerificationPredicates::next_validators_match check for the
            // untrusted state.
            next_validators: None,
        };

        let options = client_state.as_light_client_options()?;

        self.verifier
            .verify(
                untrusted_state,
                trusted_state,
                &options,
                ctx.host_timestamp()?.into_tm_time().unwrap(),
            )
            .into_result()?;

        // If the header has verified, but its corresponding consensus state
        // differs from the existing consensus state for that height, freeze the
        // client and return the installed consensus state.
        if let Some(cs) = existing_consensus_state {
            if cs != header_consensus_state {
                return Ok(UpdatedState {
                    client_state: client_state.with_frozen_height(header.height()).into_box(),
                    consensus_state: cs.into_box(),
                });
            }
        }

        // Monotonicity checks for timestamps for in-the-middle updates
        // (cs-new, cs-next, cs-latest)
        if header.height() < client_state.latest_height() {
            let maybe_next_cs = ctx
                .next_consensus_state(&client_id, &header.height())?
                .as_ref()
                .map(|cs| downcast_tm_consensus_state(cs.as_ref()))
                .transpose()?;

            if let Some(next_cs) = maybe_next_cs {
                // New (untrusted) header timestamp cannot occur after next
                // consensus state's height
                if header.signed_header.header().time > next_cs.timestamp {
                    return Err(ClientError::ClientSpecific {
                        description: Error::HeaderTimestampTooHigh {
                            actual: header.signed_header.header().time.to_string(),
                            max: next_cs.timestamp.to_string(),
                        }
                        .to_string(),
                    });
                }
            }
        }

        // (cs-trusted, cs-prev, cs-new)
        if header.trusted_height < header.height() {
            let maybe_prev_cs = ctx
                .prev_consensus_state(&client_id, &header.height())?
                .as_ref()
                .map(|cs| downcast_tm_consensus_state(cs.as_ref()))
                .transpose()?;

            if let Some(prev_cs) = maybe_prev_cs {
                // New (untrusted) header timestamp cannot occur before the
                // previous consensus state's height
                if header.signed_header.header().time < prev_cs.timestamp {
                    return Err(ClientError::ClientSpecific {
                        description: Error::HeaderTimestampTooLow {
                            actual: header.signed_header.header().time.to_string(),
                            min: prev_cs.timestamp.to_string(),
                        }
                        .to_string(),
                    });
                }
            }
        }

        Ok(UpdatedState {
            client_state: client_state.with_header(header.clone())?.into_box(),
            consensus_state: TmConsensusState::from(header).into_box(),
        })
    }

    fn check_misbehaviour_and_update_state(
        &self,
        ctx: &dyn ClientReader,
        client_id: ClientId,
        misbehaviour: Any,
    ) -> Result<Box<dyn Ics2ClientState>, ClientError> {
        let misbehaviour = TmMisbehaviour::try_from(misbehaviour)?;
        let header_1 = misbehaviour.header1();
        let header_2 = misbehaviour.header2();

        if header_1.height() == header_2.height() {
            // Fork
            if header_1.signed_header.commit.block_id.hash
                == header_2.signed_header.commit.block_id.hash
            {
                return Err(Error::MisbehaviourHeadersBlockHashesEqual.into());
            }
        } else {
            // BFT time violation
            if header_1.signed_header.header.time > header_2.signed_header.header.time {
                return Err(Error::MisbehaviourHeadersNotAtSameHeight.into());
            }
        }

        let consensus_state_1 = {
            let cs = ctx.consensus_state(&client_id, &header_1.trusted_height)?;
            downcast_tm_consensus_state(cs.as_ref())
        }?;
        let consensus_state_2 = {
            let cs = ctx.consensus_state(&client_id, &header_2.trusted_height)?;
            downcast_tm_consensus_state(cs.as_ref())
        }?;

        let chain_id = self
            .chain_id
            .clone()
            .with_version(header_1.height().revision_number());
        if !misbehaviour.chain_id_matches(&chain_id) {
            return Err(Error::MisbehaviourHeadersChainIdMismatch {
                header_chain_id: header_1.signed_header.header.chain_id.to_string(),
                chain_id: self.chain_id.to_string(),
            }
            .into());
        }

        let current_timestamp = ctx.host_timestamp()?;

        self.check_header_and_validator_set(header_1, &consensus_state_1, current_timestamp)?;
        self.check_header_and_validator_set(header_2, &consensus_state_2, current_timestamp)?;

        self.verify_header_commit_against_trusted(header_1, &consensus_state_1)?;
        self.verify_header_commit_against_trusted(header_2, &consensus_state_2)?;

        let client_state = downcast_tm_client_state(self)?.clone();
        Ok(client_state
            .with_frozen_height(Height::new(0, 1).unwrap())
            .into_box())
    }

    #[cfg(feature = "val_exec_ctx")]
    fn new_check_misbehaviour_and_update_state(
        &self,
        ctx: &dyn ValidationContext,
        client_id: ClientId,
        misbehaviour: Any,
    ) -> Result<Box<dyn Ics2ClientState>, ContextError> {
        let misbehaviour = TmMisbehaviour::try_from(misbehaviour)?;
        let header_1 = misbehaviour.header1();
        let header_2 = misbehaviour.header2();

        if header_1.height() == header_2.height() {
            // Fork
            if header_1.signed_header.commit.block_id.hash
                == header_2.signed_header.commit.block_id.hash
            {
                return Err(ContextError::ClientError(
                    Error::MisbehaviourHeadersBlockHashesEqual.into(),
                ));
            }
        } else {
            // BFT time violation
            if header_1.signed_header.header.time > header_2.signed_header.header.time {
                return Err(ContextError::ClientError(
                    Error::MisbehaviourHeadersNotAtSameHeight.into(),
                ));
            }
        }

        let consensus_state_1 = {
            let cs = ctx.consensus_state(&client_id, &header_1.trusted_height)?;
            downcast_tm_consensus_state(cs.as_ref())
        }?;
        let consensus_state_2 = {
            let cs = ctx.consensus_state(&client_id, &header_2.trusted_height)?;
            downcast_tm_consensus_state(cs.as_ref())
        }?;

        let chain_id = self
            .chain_id
            .clone()
            .with_version(header_1.height().revision_number());
        if !misbehaviour.chain_id_matches(&chain_id) {
            return Err(ContextError::ClientError(
                Error::MisbehaviourHeadersChainIdMismatch {
                    header_chain_id: header_1.signed_header.header.chain_id.to_string(),
                    chain_id: self.chain_id.to_string(),
                }
                .into(),
            ));
        }

        let current_timestamp = ctx.host_timestamp()?;

        self.check_header_and_validator_set(header_1, &consensus_state_1, current_timestamp)?;
        self.check_header_and_validator_set(header_2, &consensus_state_2, current_timestamp)?;

        self.verify_header_commit_against_trusted(header_1, &consensus_state_1)?;
        self.verify_header_commit_against_trusted(header_2, &consensus_state_2)?;

        let client_state = downcast_tm_client_state(self)?.clone();
        Ok(client_state
            .with_frozen_height(Height::new(0, 1).unwrap())
            .into_box())
    }

    #[cfg(feature = "val_exec_ctx")]
    fn new_check_header_and_update_state(
        &self,
        ctx: &dyn ValidationContext,
        client_id: ClientId,
        header: Any,
    ) -> Result<UpdatedState, ClientError> {
        fn maybe_consensus_state(
            ctx: &dyn ValidationContext,
            client_id: &ClientId,
            height: Height,
        ) -> Result<Option<Box<dyn ConsensusState>>, ClientError> {
            match ctx.consensus_state(client_id, &height) {
                Ok(cs) => Ok(Some(cs)),
                Err(e) => match e {
                    ContextError::ClientError(e) => Err(e),
                    _ => Ok(None),
                },
            }
        }

        let client_state = downcast_tm_client_state(self)?.clone();
        let header = TmHeader::try_from(header)?;

        if header.height().revision_number() != client_state.chain_id().version() {
            return Err(ClientError::ClientSpecific {
                description: Error::MismatchedRevisions {
                    current_revision: client_state.chain_id().version(),
                    update_revision: header.height().revision_number(),
                }
                .to_string(),
            });
        }

        // Check if a consensus state is already installed; if so it should
        // match the untrusted header.
        let header_consensus_state = TmConsensusState::from(header.clone());
        let existing_consensus_state =
            match maybe_consensus_state(ctx, &client_id, header.height())? {
                Some(cs) => {
                    let cs = downcast_tm_consensus_state(cs.as_ref())?;
                    // If this consensus state matches, skip verification
                    // (optimization)
                    if cs == header_consensus_state {
                        // Header is already installed and matches the incoming
                        // header (already verified)
                        return Ok(UpdatedState {
                            client_state: client_state.into_box(),
                            consensus_state: cs.into_box(),
                        });
                    }
                    Some(cs)
                }
                None => None,
            };

        let trusted_consensus_state = downcast_tm_consensus_state(
            ctx.consensus_state(&client_id, &header.trusted_height)
                .map_err(|e| match e {
                    ContextError::ClientError(e) => e,
                    _ => todo!(),
                })?
                .as_ref(),
        )?;

        let trusted_state = TrustedBlockState {
            chain_id: &self.chain_id.clone().into(),
            header_time: trusted_consensus_state.timestamp,
            height: header
                .trusted_height
                .revision_height()
                .try_into()
                .map_err(|_| ClientError::ClientSpecific {
                    description: Error::InvalidHeaderHeight {
                        height: header.trusted_height.revision_height(),
                    }
                    .to_string(),
                })?,
            next_validators: &header.trusted_validator_set,
            next_validators_hash: trusted_consensus_state.next_validators_hash,
        };

        let untrusted_state = UntrustedBlockState {
            signed_header: &header.signed_header,
            validators: &header.validator_set,
            // NB: This will skip the
            // VerificationPredicates::next_validators_match check for the
            // untrusted state.
            next_validators: None,
        };

        let options = client_state.as_light_client_options()?;
        let now = ctx
            .host_timestamp()
            .map_err(|e| ClientError::Other {
                description: e.to_string(),
            })?
            .into_tm_time()
            .unwrap();

        self.verifier
            .verify(untrusted_state, trusted_state, &options, now)
            .into_result()?;

        // If the header has verified, but its corresponding consensus state
        // differs from the existing consensus state for that height, freeze the
        // client and return the installed consensus state.
        if let Some(cs) = existing_consensus_state {
            if cs != header_consensus_state {
                return Ok(UpdatedState {
                    client_state: client_state.with_frozen_height(header.height()).into_box(),
                    consensus_state: cs.into_box(),
                });
            }
        }

        // Monotonicity checks for timestamps for in-the-middle updates
        // (cs-new, cs-next, cs-latest)
        if header.height() < client_state.latest_height() {
            let maybe_next_cs = ctx
                .next_consensus_state(&client_id, &header.height())
                .map_err(|e| match e {
                    ContextError::ClientError(e) => e,
                    _ => todo!(),
                })?
                .as_ref()
                .map(|cs| downcast_tm_consensus_state(cs.as_ref()))
                .transpose()?;

            if let Some(next_cs) = maybe_next_cs {
                // New (untrusted) header timestamp cannot occur after next
                // consensus state's height
                if header.signed_header.header().time > next_cs.timestamp {
                    return Err(ClientError::ClientSpecific {
                        description: Error::HeaderTimestampTooHigh {
                            actual: header.signed_header.header().time.to_string(),
                            max: next_cs.timestamp.to_string(),
                        }
                        .to_string(),
                    });
                }
            }
        }

        // (cs-trusted, cs-prev, cs-new)
        if header.trusted_height < header.height() {
            let maybe_prev_cs = ctx
                .prev_consensus_state(&client_id, &header.height())
                .map_err(|e| match e {
                    ContextError::ClientError(e) => e,
                    _ => todo!(),
                })?
                .as_ref()
                .map(|cs| downcast_tm_consensus_state(cs.as_ref()))
                .transpose()?;

            if let Some(prev_cs) = maybe_prev_cs {
                // New (untrusted) header timestamp cannot occur before the
                // previous consensus state's height
                if header.signed_header.header().time < prev_cs.timestamp {
                    return Err(ClientError::ClientSpecific {
                        description: Error::HeaderTimestampTooLow {
                            actual: header.signed_header.header().time.to_string(),
                            min: prev_cs.timestamp.to_string(),
                        }
                        .to_string(),
                    });
                }
            }
        }

        Ok(UpdatedState {
            client_state: client_state.with_header(header.clone())?.into_box(),
            consensus_state: TmConsensusState::from(header).into_box(),
        })
    }

    fn verify_upgrade_and_update_state(
        &self,
        _consensus_state: Any,
        _proof_upgrade_client: RawMerkleProof,
        _proof_upgrade_consensus_state: RawMerkleProof,
    ) -> Result<UpdatedState, ClientError> {
        unimplemented!()
    }

    fn verify_client_consensus_state(
        &self,
        height: Height,
        prefix: &CommitmentPrefix,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        client_id: &ClientId,
        consensus_height: Height,
        expected_consensus_state: &dyn ConsensusState,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;

        let path = ClientConsensusStatePath {
            client_id: client_id.clone(),
            epoch: consensus_height.revision_number(),
            height: consensus_height.revision_height(),
        };
        let value = expected_consensus_state
            .encode_vec()
            .map_err(ClientError::InvalidAnyConsensusState)?;

        verify_membership(client_state, prefix, proof, root, path, value)
    }

    fn verify_connection_state(
        &self,
        height: Height,
        prefix: &CommitmentPrefix,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        connection_id: &ConnectionId,
        expected_connection_end: &ConnectionEnd,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;

        let path = ConnectionsPath(connection_id.clone());
        let value = expected_connection_end
            .encode_vec()
            .map_err(ClientError::InvalidConnectionEnd)?;
        verify_membership(client_state, prefix, proof, root, path, value)
    }

    fn verify_channel_state(
        &self,
        height: Height,
        prefix: &CommitmentPrefix,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        port_id: &PortId,
        channel_id: &ChannelId,
        expected_channel_end: &crate::core::ics04_channel::channel::ChannelEnd,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;

        let path = ChannelEndsPath(port_id.clone(), channel_id.clone());
        let value = expected_channel_end
            .encode_vec()
            .map_err(ClientError::InvalidChannelEnd)?;
        verify_membership(client_state, prefix, proof, root, path, value)
    }

    fn verify_client_full_state(
        &self,
        height: Height,
        prefix: &CommitmentPrefix,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        client_id: &ClientId,
        expected_client_state: Any,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;

        let path = ClientStatePath(client_id.clone());
        let value = expected_client_state.encode_to_vec();
        verify_membership(client_state, prefix, proof, root, path, value)
    }

    fn verify_packet_data(
        &self,
        ctx: &dyn ChannelReader,
        height: Height,
        connection_end: &ConnectionEnd,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        port_id: &PortId,
        channel_id: &ChannelId,
        sequence: Sequence,
        commitment: PacketCommitment,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;
        verify_delay_passed(ctx, height, connection_end)?;

        let commitment_path = CommitmentsPath {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
            sequence,
        };

        verify_membership(
            client_state,
            connection_end.counterparty().prefix(),
            proof,
            root,
            commitment_path,
            commitment.into_vec(),
        )
    }

    fn verify_packet_acknowledgement(
        &self,
        ctx: &dyn ChannelReader,
        height: Height,
        connection_end: &ConnectionEnd,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        port_id: &PortId,
        channel_id: &ChannelId,
        sequence: Sequence,
        ack_commitment: AcknowledgementCommitment,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;
        verify_delay_passed(ctx, height, connection_end)?;

        let ack_path = AcksPath {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
            sequence,
        };
        verify_membership(
            client_state,
            connection_end.counterparty().prefix(),
            proof,
            root,
            ack_path,
            ack_commitment.into_vec(),
        )
    }

    fn verify_next_sequence_recv(
        &self,
        ctx: &dyn ChannelReader,
        height: Height,
        connection_end: &ConnectionEnd,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        port_id: &PortId,
        channel_id: &ChannelId,
        sequence: Sequence,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;
        verify_delay_passed(ctx, height, connection_end)?;

        let mut seq_bytes = Vec::new();
        u64::from(sequence)
            .encode(&mut seq_bytes)
            .expect("buffer size too small");

        let seq_path = SeqRecvsPath(port_id.clone(), channel_id.clone());

        verify_membership(
            client_state,
            connection_end.counterparty().prefix(),
            proof,
            root,
            seq_path,
            seq_bytes,
        )
    }

    fn verify_packet_receipt_absence(
        &self,
        ctx: &dyn ChannelReader,
        height: Height,
        connection_end: &ConnectionEnd,
        proof: &CommitmentProofBytes,
        root: &CommitmentRoot,
        port_id: &PortId,
        channel_id: &ChannelId,
        sequence: Sequence,
    ) -> Result<(), ClientError> {
        let client_state = downcast_tm_client_state(self)?;
        client_state.verify_height(height)?;
        verify_delay_passed(ctx, height, connection_end)?;

        let receipt_path = ReceiptsPath {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
            sequence,
        };
        verify_non_membership(
            client_state,
            connection_end.counterparty().prefix(),
            proof,
            root,
            receipt_path,
        )
    }
}

fn verify_membership(
    client_state: &ClientState,
    prefix: &CommitmentPrefix,
    proof: &CommitmentProofBytes,
    root: &CommitmentRoot,
    path: impl Into<Path>,
    value: Vec<u8>,
) -> Result<(), ClientError> {
    let merkle_path = apply_prefix(prefix, vec![path.into().to_string()]);
    let merkle_proof: MerkleProof = RawMerkleProof::try_from(proof.clone())
        .map_err(ClientError::InvalidCommitmentProof)?
        .into();

    merkle_proof
        .verify_membership(
            &client_state.proof_specs,
            root.clone().into(),
            merkle_path,
            value,
            0,
        )
        .map_err(ClientError::Ics23Verification)
}

fn verify_non_membership(
    client_state: &ClientState,
    prefix: &CommitmentPrefix,
    proof: &CommitmentProofBytes,
    root: &CommitmentRoot,
    path: impl Into<Path>,
) -> Result<(), ClientError> {
    let merkle_path = apply_prefix(prefix, vec![path.into().to_string()]);
    let merkle_proof: MerkleProof = RawMerkleProof::try_from(proof.clone())
        .map_err(ClientError::InvalidCommitmentProof)?
        .into();

    merkle_proof
        .verify_non_membership(&client_state.proof_specs, root.clone().into(), merkle_path)
        .map_err(ClientError::Ics23Verification)
}

fn verify_delay_passed(
    ctx: &dyn ChannelReader,
    height: Height,
    connection_end: &ConnectionEnd,
) -> Result<(), ClientError> {
    let current_timestamp = ctx.host_timestamp().map_err(|e| ClientError::Other {
        description: e.to_string(),
    })?;
    let current_height = ctx.host_height().map_err(|e| ClientError::Other {
        description: e.to_string(),
    })?;

    let client_id = connection_end.client_id();
    let processed_time =
        ctx.client_update_time(client_id, &height)
            .map_err(|_| Error::ProcessedTimeNotFound {
                client_id: client_id.clone(),
                height,
            })?;
    let processed_height = ctx.client_update_height(client_id, &height).map_err(|_| {
        Error::ProcessedHeightNotFound {
            client_id: client_id.clone(),
            height,
        }
    })?;

    let delay_period_time = connection_end.delay_period();
    let delay_period_height = ctx.block_delay(&delay_period_time);

    ClientState::verify_delay_passed(
        current_timestamp,
        current_height,
        processed_time,
        processed_height,
        delay_period_time,
        delay_period_height,
    )
    .map_err(|e| e.into())
}

fn downcast_tm_client_state(cs: &dyn Ics2ClientState) -> Result<&ClientState, ClientError> {
    cs.as_any()
        .downcast_ref::<ClientState>()
        .ok_or_else(|| ClientError::ClientArgsTypeMismatch {
            client_type: tm_client_type(),
        })
}

fn downcast_tm_consensus_state(cs: &dyn ConsensusState) -> Result<TmConsensusState, ClientError> {
    cs.as_any()
        .downcast_ref::<TmConsensusState>()
        .ok_or_else(|| ClientError::ClientArgsTypeMismatch {
            client_type: tm_client_type(),
        })
        .map(Clone::clone)
}

impl Protobuf<RawTmClientState> for ClientState {}

impl TryFrom<RawTmClientState> for ClientState {
    type Error = Error;

    fn try_from(raw: RawTmClientState) -> Result<Self, Self::Error> {
        let chain_id = ChainId::from_string(raw.chain_id.as_str());

        let trust_level = {
            let trust_level = raw
                .trust_level
                .clone()
                .ok_or(Error::MissingTrustingPeriod)?;
            trust_level
                .try_into()
                .map_err(|e| Error::InvalidTrustThreshold {
                    reason: format!("{e}"),
                })?
        };

        let trusting_period = raw
            .trusting_period
            .ok_or(Error::MissingTrustingPeriod)?
            .try_into()
            .map_err(|_| Error::MissingTrustingPeriod)?;

        let unbonding_period = raw
            .unbonding_period
            .ok_or(Error::MissingUnbondingPeriod)?
            .try_into()
            .map_err(|_| Error::MissingUnbondingPeriod)?;

        let max_clock_drift = raw
            .max_clock_drift
            .ok_or(Error::NegativeMaxClockDrift)?
            .try_into()
            .map_err(|_| Error::NegativeMaxClockDrift)?;

        let latest_height = raw
            .latest_height
            .ok_or(Error::MissingLatestHeight)?
            .try_into()
            .map_err(|_| Error::MissingLatestHeight)?;

        // In `RawClientState`, a `frozen_height` of `0` means "not frozen".
        // See:
        // https://github.com/cosmos/ibc-go/blob/8422d0c4c35ef970539466c5bdec1cd27369bab3/modules/light-clients/07-tendermint/types/client_state.go#L74
        let frozen_height = raw
            .frozen_height
            .and_then(|raw_height| raw_height.try_into().ok());

        // We use set this deprecated field just so that we can properly convert
        // it back in its raw form
        #[allow(deprecated)]
        let allow_update = AllowUpdate {
            after_expiry: raw.allow_update_after_expiry,
            after_misbehaviour: raw.allow_update_after_misbehaviour,
        };

        let client_state = ClientState::new(
            chain_id,
            trust_level,
            trusting_period,
            unbonding_period,
            max_clock_drift,
            latest_height,
            raw.proof_specs.into(),
            raw.upgrade_path,
            allow_update,
            frozen_height,
        )?;

        Ok(client_state)
    }
}

impl From<ClientState> for RawTmClientState {
    fn from(value: ClientState) -> Self {
        #[allow(deprecated)]
        Self {
            chain_id: value.chain_id.to_string(),
            trust_level: Some(value.trust_level.into()),
            trusting_period: Some(value.trusting_period.into()),
            unbonding_period: Some(value.unbonding_period.into()),
            max_clock_drift: Some(value.max_clock_drift.into()),
            frozen_height: Some(value.frozen_height.map(|height| height.into()).unwrap_or(
                RawHeight {
                    revision_number: 0,
                    revision_height: 0,
                },
            )),
            latest_height: Some(value.latest_height.into()),
            proof_specs: value.proof_specs.into(),
            upgrade_path: value.upgrade_path,
            allow_update_after_expiry: value.allow_update.after_expiry,
            allow_update_after_misbehaviour: value.allow_update.after_misbehaviour,
        }
    }
}

impl Protobuf<Any> for ClientState {}

impl TryFrom<Any> for ClientState {
    type Error = ClientError;

    fn try_from(raw: Any) -> Result<Self, Self::Error> {
        use bytes::Buf;
        use core::ops::Deref;

        fn decode_client_state<B: Buf>(buf: B) -> Result<ClientState, Error> {
            RawTmClientState::decode(buf)
                .map_err(Error::Decode)?
                .try_into()
        }

        match raw.type_url.as_str() {
            TENDERMINT_CLIENT_STATE_TYPE_URL => {
                decode_client_state(raw.value.deref()).map_err(Into::into)
            }
            _ => Err(ClientError::UnknownClientStateType {
                client_state_type: raw.type_url,
            }),
        }
    }
}

impl From<ClientState> for Any {
    fn from(client_state: ClientState) -> Self {
        Any {
            type_url: TENDERMINT_CLIENT_STATE_TYPE_URL.to_string(),
            value: Protobuf::<RawTmClientState>::encode_vec(&client_state)
                .expect("encoding to `Any` from `TmClientState`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use crate::Height;
    use core::time::Duration;
    use test_log::test;

    use ibc_proto::ics23::ProofSpec as Ics23ProofSpec;

    use crate::clients::ics07_tendermint::client_state::{AllowUpdate, ClientState};
    use crate::core::ics02_client::trust_threshold::TrustThreshold;
    use crate::core::ics23_commitment::specs::ProofSpecs;
    use crate::core::ics24_host::identifier::ChainId;
    use crate::timestamp::{Timestamp, ZERO_DURATION};

    #[derive(Clone, Debug, PartialEq)]
    struct ClientStateParams {
        id: ChainId,
        trust_level: TrustThreshold,
        trusting_period: Duration,
        unbonding_period: Duration,
        max_clock_drift: Duration,
        latest_height: Height,
        proof_specs: ProofSpecs,
        upgrade_path: Vec<String>,
        allow_update: AllowUpdate,
    }

    #[test]
    fn client_state_new() {
        // Define a "default" set of parameters to reuse throughout these tests.
        let default_params: ClientStateParams = ClientStateParams {
            id: ChainId::default(),
            trust_level: TrustThreshold::ONE_THIRD,
            trusting_period: Duration::new(64000, 0),
            unbonding_period: Duration::new(128000, 0),
            max_clock_drift: Duration::new(3, 0),
            latest_height: Height::new(0, 10).unwrap(),
            proof_specs: ProofSpecs::default(),
            upgrade_path: Default::default(),
            allow_update: AllowUpdate {
                after_expiry: false,
                after_misbehaviour: false,
            },
        };

        struct Test {
            name: String,
            params: ClientStateParams,
            want_pass: bool,
        }

        let tests: Vec<Test> = vec![
            Test {
                name: "Valid parameters".to_string(),
                params: default_params.clone(),
                want_pass: true,
            },
            Test {
                name: "Valid (empty) upgrade-path".to_string(),
                params: ClientStateParams {
                    upgrade_path: vec![],
                    ..default_params.clone()
                },
                want_pass: true,
            },
            Test {
                name: "Valid upgrade-path".to_string(),
                params: ClientStateParams {
                    upgrade_path: vec!["upgrade".to_owned(), "upgradedIBCState".to_owned()],
                    ..default_params.clone()
                },
                want_pass: true,
            },
            Test {
                name: "Valid long (50 chars) chain-id".to_string(),
                params: ClientStateParams {
                    id: ChainId::new("a".repeat(48), 0),
                    ..default_params.clone()
                },
                want_pass: true,
            },
            Test {
                name: "Invalid too-long (51 chars) chain-id".to_string(),
                params: ClientStateParams {
                    id: ChainId::new("a".repeat(49), 0),
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (zero) max-clock-drift period".to_string(),
                params: ClientStateParams {
                    max_clock_drift: ZERO_DURATION,
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid unbonding period".to_string(),
                params: ClientStateParams {
                    unbonding_period: ZERO_DURATION,
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (too small) trusting period".to_string(),
                params: ClientStateParams {
                    trusting_period: ZERO_DURATION,
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (too large) trusting period w.r.t. unbonding period".to_string(),
                params: ClientStateParams {
                    trusting_period: Duration::new(11, 0),
                    unbonding_period: Duration::new(10, 0),
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (equal) trusting period w.r.t. unbonding period".to_string(),
                params: ClientStateParams {
                    trusting_period: Duration::new(10, 0),
                    unbonding_period: Duration::new(10, 0),
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (zero) trusting trust threshold".to_string(),
                params: ClientStateParams {
                    trust_level: TrustThreshold::ZERO,
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (too small) trusting trust threshold".to_string(),
                params: ClientStateParams {
                    trust_level: TrustThreshold::new(1, 4).unwrap(),
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid latest height revision number (doesn't match chain)".to_string(),
                params: ClientStateParams {
                    latest_height: Height::new(1, 1).unwrap(),
                    ..default_params.clone()
                },
                want_pass: false,
            },
            Test {
                name: "Invalid (empty) proof specs".to_string(),
                params: ClientStateParams {
                    proof_specs: ProofSpecs::from(Vec::<Ics23ProofSpec>::new()),
                    ..default_params
                },
                want_pass: false,
            },
        ]
        .into_iter()
        .collect();

        for test in tests {
            let p = test.params.clone();

            let cs_result = ClientState::new(
                p.id,
                p.trust_level,
                p.trusting_period,
                p.unbonding_period,
                p.max_clock_drift,
                p.latest_height,
                p.proof_specs,
                p.upgrade_path,
                p.allow_update,
                None,
            );

            assert_eq!(
                test.want_pass,
                cs_result.is_ok(),
                "ClientState::new() failed for test {}, \nmsg{:?} with error {:?}",
                test.name,
                test.params.clone(),
                cs_result.err(),
            );
        }
    }

    #[test]
    fn client_state_verify_delay_passed() {
        #[derive(Debug, Clone)]
        struct Params {
            current_time: Timestamp,
            current_height: Height,
            processed_time: Timestamp,
            processed_height: Height,
            delay_period_time: Duration,
            delay_period_blocks: u64,
        }
        struct Test {
            name: String,
            params: Params,
            want_pass: bool,
        }
        let now = Timestamp::now();

        let tests: Vec<Test> = vec![
            Test {
                name: "Successful delay verification".to_string(),
                params: Params {
                    current_time: (now + Duration::from_nanos(2000)).unwrap(),
                    current_height: Height::new(0, 5).unwrap(),
                    processed_time: (now + Duration::from_nanos(1000)).unwrap(),
                    processed_height: Height::new(0, 3).unwrap(),
                    delay_period_time: Duration::from_nanos(500),
                    delay_period_blocks: 2,
                },
                want_pass: true,
            },
            Test {
                name: "Delay period(time) has not elapsed".to_string(),
                params: Params {
                    current_time: (now + Duration::from_nanos(1200)).unwrap(),
                    current_height: Height::new(0, 5).unwrap(),
                    processed_time: (now + Duration::from_nanos(1000)).unwrap(),
                    processed_height: Height::new(0, 3).unwrap(),
                    delay_period_time: Duration::from_nanos(500),
                    delay_period_blocks: 2,
                },
                want_pass: false,
            },
            Test {
                name: "Delay period(blocks) has not elapsed".to_string(),
                params: Params {
                    current_time: (now + Duration::from_nanos(2000)).unwrap(),
                    current_height: Height::new(0, 5).unwrap(),
                    processed_time: (now + Duration::from_nanos(1000)).unwrap(),
                    processed_height: Height::new(0, 4).unwrap(),
                    delay_period_time: Duration::from_nanos(500),
                    delay_period_blocks: 2,
                },
                want_pass: false,
            },
        ];

        for test in tests {
            let res = ClientState::verify_delay_passed(
                test.params.current_time,
                test.params.current_height,
                test.params.processed_time,
                test.params.processed_height,
                test.params.delay_period_time,
                test.params.delay_period_blocks,
            );

            assert_eq!(
                test.want_pass,
                res.is_ok(),
                "ClientState::verify_delay_passed() failed for test {}, \nmsg{:?} with error {:?}",
                test.name,
                test.params.clone(),
                res.err(),
            );
        }
    }

    #[test]
    fn client_state_verify_height() {
        // Define a "default" set of parameters to reuse throughout these tests.
        let default_params: ClientStateParams = ClientStateParams {
            id: ChainId::new("ibc".to_string(), 1),
            trust_level: TrustThreshold::ONE_THIRD,
            trusting_period: Duration::new(64000, 0),
            unbonding_period: Duration::new(128000, 0),
            max_clock_drift: Duration::new(3, 0),
            latest_height: Height::new(1, 10).unwrap(),
            proof_specs: ProofSpecs::default(),
            upgrade_path: Default::default(),
            allow_update: AllowUpdate {
                after_expiry: false,
                after_misbehaviour: false,
            },
        };

        struct Test {
            name: String,
            height: Height,
            setup: Option<Box<dyn FnOnce(ClientState) -> ClientState>>,
            want_pass: bool,
        }

        let tests = vec![
            Test {
                name: "Successful height verification".to_string(),
                height: Height::new(1, 8).unwrap(),
                setup: None,
                want_pass: true,
            },
            Test {
                name: "Invalid (too large)  client height".to_string(),
                height: Height::new(1, 12).unwrap(),
                setup: None,
                want_pass: false,
            },
            Test {
                name: "Invalid, client is frozen below current height".to_string(),
                height: Height::new(1, 6).unwrap(),
                setup: Some(Box::new(|client_state| {
                    client_state.with_frozen_height(Height::new(1, 5).unwrap())
                })),
                want_pass: false,
            },
        ];

        for test in tests {
            let p = default_params.clone();
            let client_state = ClientState::new(
                p.id,
                p.trust_level,
                p.trusting_period,
                p.unbonding_period,
                p.max_clock_drift,
                p.latest_height,
                p.proof_specs,
                p.upgrade_path,
                p.allow_update,
                None,
            )
            .unwrap();
            let client_state = match test.setup {
                Some(setup) => (setup)(client_state),
                _ => client_state,
            };
            let res = client_state.verify_height(test.height);

            assert_eq!(
                test.want_pass,
                res.is_ok(),
                "ClientState::verify_delay_height() failed for test {}, \nmsg{:?} with error {:?}",
                test.name,
                test.height,
                res.err(),
            );
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use tendermint_rpc::endpoint::abci_query::AbciQuery;

    use crate::test::test_serialization_roundtrip;

    #[test]
    fn serialization_roundtrip_no_proof() {
        let json_data =
            include_str!("../../../tests/support/query/serialization/client_state.json");
        test_serialization_roundtrip::<AbciQuery>(json_data);
    }

    #[test]
    fn serialization_roundtrip_with_proof() {
        let json_data =
            include_str!("../../../tests/support/query/serialization/client_state_proof.json");
        test_serialization_roundtrip::<AbciQuery>(json_data);
    }
}

#[cfg(any(test, feature = "mocks"))]
pub mod test_util {
    use crate::prelude::*;
    use core::time::Duration;

    use tendermint::block::Header;

    use crate::clients::ics07_tendermint::client_state::{AllowUpdate, ClientState};
    use crate::core::ics02_client::height::Height;
    use crate::core::ics24_host::identifier::ChainId;

    pub fn get_dummy_tendermint_client_state(tm_header: Header) -> ClientState {
        ClientState::new(
            ChainId::from(tm_header.chain_id.clone()),
            Default::default(),
            Duration::from_secs(64000),
            Duration::from_secs(128000),
            Duration::from_millis(3000),
            Height::new(
                ChainId::chain_version(tm_header.chain_id.as_str()),
                u64::from(tm_header.height),
            )
            .unwrap(),
            Default::default(),
            Default::default(),
            AllowUpdate {
                after_expiry: false,
                after_misbehaviour: false,
            },
            None,
        )
        .unwrap()
    }
}
