//! The complaint handling

use crate::consensus::{
    metrics::{timed_call, EcdsaComplaintMetrics},
    utils::RoundRobin,
    ConsensusCrypto,
};
use crate::ecdsa::utils::EcdsaBlockReaderImpl;

use ic_interfaces::consensus_pool::ConsensusBlockCache;
use ic_interfaces::crypto::{ErrorReproducibility, IDkgProtocol};
use ic_interfaces::ecdsa::{EcdsaChangeAction, EcdsaChangeSet, EcdsaPool};
use ic_logger::{debug, warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_types::artifact::EcdsaMessageId;
use ic_types::consensus::ecdsa::{
    complaint_prefix, opening_prefix, EcdsaBlockReader, EcdsaComplaint, EcdsaComplaintContent,
    EcdsaMessage, EcdsaOpening, EcdsaOpeningContent, TranscriptRef,
};
use ic_types::crypto::canister_threshold_sig::error::IDkgLoadTranscriptError;
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgComplaint, IDkgOpening, IDkgTranscript, IDkgTranscriptId,
};
use ic_types::{Height, NodeId, RegistryVersion};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) trait EcdsaComplaintHandler: Send {
    /// The on_state_change() called from the main ECDSA path.
    fn on_state_change(&self, ecdsa_pool: &dyn EcdsaPool) -> EcdsaChangeSet;

    /// Get a reference to the transcript loader.
    fn as_transcript_loader(&self) -> &dyn EcdsaTranscriptLoader;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ComplaintKey {
    transcript_id: IDkgTranscriptId,
    dealer_id: NodeId,
    complainer_id: NodeId,
}

impl From<&EcdsaComplaint> for ComplaintKey {
    fn from(ecdsa_complaint: &EcdsaComplaint) -> Self {
        Self {
            transcript_id: ecdsa_complaint.content.idkg_complaint.transcript_id,
            dealer_id: ecdsa_complaint.content.idkg_complaint.dealer_id,
            complainer_id: ecdsa_complaint.signature.signer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct OpeningKey {
    transcript_id: IDkgTranscriptId,
    dealer_id: NodeId,
    opener_id: NodeId,
}

impl From<&EcdsaOpening> for OpeningKey {
    fn from(ecdsa_opening: &EcdsaOpening) -> Self {
        Self {
            transcript_id: ecdsa_opening.content.idkg_opening.transcript_id,
            dealer_id: ecdsa_opening.content.idkg_opening.dealer_id,
            opener_id: ecdsa_opening.signature.signer,
        }
    }
}

pub(crate) struct EcdsaComplaintHandlerImpl {
    node_id: NodeId,
    consensus_block_cache: Arc<dyn ConsensusBlockCache>,
    crypto: Arc<dyn ConsensusCrypto>,
    schedule: RoundRobin,
    metrics: EcdsaComplaintMetrics,
    log: ReplicaLogger,
}

impl EcdsaComplaintHandlerImpl {
    pub(crate) fn new(
        node_id: NodeId,
        consensus_block_cache: Arc<dyn ConsensusBlockCache>,
        crypto: Arc<dyn ConsensusCrypto>,
        metrics_registry: MetricsRegistry,
        log: ReplicaLogger,
    ) -> Self {
        Self {
            node_id,
            consensus_block_cache,
            crypto,
            schedule: RoundRobin::default(),
            metrics: EcdsaComplaintMetrics::new(metrics_registry),
            log,
        }
    }

    /// Processes the received complaints
    fn validate_complaints(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let active_transcripts = self.active_transcripts(block_reader);

        // Collection of validated complaints <complainer Id, transcript Id, dealer Id>
        let mut validated_complaints = BTreeSet::new();

        let mut ret = Vec::new();
        for (id, signed_complaint) in ecdsa_pool.unvalidated().complaints() {
            let complaint = signed_complaint.get();
            // Remove the duplicate entries
            let key = ComplaintKey::from(&signed_complaint);
            if validated_complaints.contains(&key) {
                self.metrics
                    .complaint_errors_inc("duplicate_complaints_in_batch");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!(
                        "Duplicate complaint in unvalidated batch: {}",
                        signed_complaint
                    ),
                ));
                continue;
            }

            match Action::action(
                block_reader,
                &active_transcripts,
                complaint.idkg_complaint.transcript_id.source_height(),
                &complaint.idkg_complaint.transcript_id,
            ) {
                Action::Process(transcript_ref) => {
                    if self.has_complainer_issued_complaint(
                        ecdsa_pool,
                        &complaint.idkg_complaint,
                        &signed_complaint.signature.signer,
                    ) {
                        self.metrics.complaint_errors_inc("duplicate_complaint");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!("Duplicate complaint: {}", signed_complaint),
                        ));
                    } else {
                        match self.resolve_ref(transcript_ref, block_reader, "validate_complaints")
                        {
                            Some(transcript) => {
                                let action = self.crypto_verify_complaint(
                                    &id,
                                    &transcript,
                                    &signed_complaint,
                                );
                                if let Some(EcdsaChangeAction::MoveToValidated(_)) = action {
                                    validated_complaints.insert(key);
                                }
                                ret.append(&mut action.into_iter().collect());
                            }
                            None => {
                                ret.push(EcdsaChangeAction::HandleInvalid(
                                    id,
                                    format!(
                                        "validate_complaints(): failed to resolve: {}",
                                        signed_complaint
                                    ),
                                ));
                            }
                        }
                    }
                }
                Action::Drop => ret.push(EcdsaChangeAction::RemoveUnvalidated(id)),
                Action::Defer => {}
            }
        }

        ret
    }

    /// Sends openings for complaints from peers
    fn send_openings(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let active_transcripts = self.active_transcripts(block_reader);

        ecdsa_pool
            .validated()
            .complaints()
            .filter(|(_, signed_complaint)| {
                let complaint = signed_complaint.get();
                !self.has_node_issued_opening(
                    ecdsa_pool,
                    &complaint.idkg_complaint.transcript_id,
                    &complaint.idkg_complaint.dealer_id,
                    &self.node_id,
                )
            })
            .filter_map(|(_, signed_complaint)| {
                // Look up the transcript for the complained transcript Id.
                let complaint = signed_complaint.get();
                match active_transcripts.get(&complaint.idkg_complaint.transcript_id) {
                    Some(transcript_ref) => self
                        .resolve_ref(transcript_ref, block_reader, "send_openings")
                        .map(|transcript| (signed_complaint, transcript)),
                    None => {
                        self.metrics
                            .complaint_errors_inc("complaint_inactive_transcript");
                        None
                    }
                }
            })
            .flat_map(|(signed_complaint, transcript)| {
                self.crypto_create_opening(&signed_complaint, &transcript)
            })
            .collect()
    }

    /// Processes the received openings
    fn validate_openings(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let active_transcripts = self.active_transcripts(block_reader);

        // Collection of validated openings <opener Id, transcript Id, dealer Id>
        let mut validated_openings = BTreeSet::new();

        let mut ret = Vec::new();
        for (id, signed_opening) in ecdsa_pool.unvalidated().openings() {
            let opening = signed_opening.get();

            // Remove duplicate entries
            let key = OpeningKey::from(&signed_opening);
            if validated_openings.contains(&key) {
                self.metrics
                    .complaint_errors_inc("duplicate_openings_in_batch");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!("Duplicate opening in unvalidated batch: {}", signed_opening),
                ));
                continue;
            }

            match Action::action(
                block_reader,
                &active_transcripts,
                opening.idkg_opening.transcript_id.source_height(),
                &opening.idkg_opening.transcript_id,
            ) {
                Action::Process(transcript_ref) => {
                    if self.has_node_issued_opening(
                        ecdsa_pool,
                        &opening.idkg_opening.transcript_id,
                        &opening.idkg_opening.dealer_id,
                        &signed_opening.signature.signer,
                    ) {
                        self.metrics.complaint_errors_inc("duplicate_opening");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!("Duplicate opening: {}", signed_opening),
                        ));
                    } else if let Some(signed_complaint) =
                        self.get_complaint_for_opening(ecdsa_pool, &signed_opening)
                    {
                        match self.resolve_ref(transcript_ref, block_reader, "validate_openings") {
                            Some(transcript) => {
                                let action = self.crypto_verify_opening(
                                    &id,
                                    &transcript,
                                    &signed_opening,
                                    &signed_complaint,
                                );
                                if let Some(EcdsaChangeAction::MoveToValidated(_)) = action {
                                    validated_openings.insert(key);
                                }
                                ret.append(&mut action.into_iter().collect());
                            }
                            None => {
                                ret.push(EcdsaChangeAction::HandleInvalid(
                                    id,
                                    format!(
                                        "validate_openings(): failed to resolve: {}",
                                        signed_opening
                                    ),
                                ));
                            }
                        }
                    } else {
                        // Defer handling the opening in case it was received
                        // before the complaint.
                        self.metrics
                            .complaint_errors_inc("opening_missing_complaint");
                    }
                }
                Action::Drop => ret.push(EcdsaChangeAction::RemoveUnvalidated(id)),
                Action::Defer => {}
            }
        }

        ret
    }

    /// Purges the entries no longer needed from the artifact pool
    fn purge_artifacts(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let active_transcripts = block_reader
            .active_transcripts()
            .iter()
            .map(|transcript_ref| transcript_ref.transcript_id)
            .collect::<BTreeSet<_>>();

        let mut ret = Vec::new();
        let current_height = block_reader.tip_height();

        // Unvalidated complaints
        let mut action = ecdsa_pool
            .unvalidated()
            .complaints()
            .filter(|(_, signed_complaint)| {
                let complaint = signed_complaint.get();
                self.should_purge(
                    &complaint.idkg_complaint.transcript_id,
                    complaint.idkg_complaint.transcript_id.source_height(),
                    current_height,
                    &active_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveUnvalidated(id))
            .collect();
        ret.append(&mut action);

        // Validated complaints
        let mut action = ecdsa_pool
            .validated()
            .complaints()
            .filter(|(_, signed_complaint)| {
                let complaint = signed_complaint.get();
                self.should_purge(
                    &complaint.idkg_complaint.transcript_id,
                    complaint.idkg_complaint.transcript_id.source_height(),
                    current_height,
                    &active_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveValidated(id))
            .collect();
        ret.append(&mut action);

        // Unvalidated openings
        let mut action = ecdsa_pool
            .unvalidated()
            .openings()
            .filter(|(_, signed_opening)| {
                let opening = signed_opening.get();
                self.should_purge(
                    &opening.idkg_opening.transcript_id,
                    opening.idkg_opening.transcript_id.source_height(),
                    current_height,
                    &active_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveUnvalidated(id))
            .collect();
        ret.append(&mut action);

        // Validated openings
        let mut action = ecdsa_pool
            .validated()
            .openings()
            .filter(|(_, signed_opening)| {
                let opening = signed_opening.get();
                self.should_purge(
                    &opening.idkg_opening.transcript_id,
                    opening.idkg_opening.transcript_id.source_height(),
                    current_height,
                    &active_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveValidated(id))
            .collect();
        ret.append(&mut action);

        ret
    }

    /// Helper to create a signed complaint
    fn crypto_create_complaint(
        &self,
        idkg_complaint: IDkgComplaint,
        registry_version: RegistryVersion,
    ) -> Option<EcdsaComplaint> {
        let content = EcdsaComplaintContent { idkg_complaint };
        match self.crypto.sign(&content, self.node_id, registry_version) {
            Ok(signature) => {
                let signed_complaint = EcdsaComplaint { content, signature };
                self.metrics.complaint_metrics_inc("complaints_sent");
                Some(signed_complaint)
            }
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to sign complaint: transcript_id: {:?}, dealer_id: {:?}, error = {:?}",
                    content.idkg_complaint.transcript_id,
                    content.idkg_complaint.dealer_id,
                    err
                );
                self.metrics.complaint_errors_inc("sign_complaint");
                None
            }
        }
    }

    /// Helper to verify the complaint
    fn crypto_verify_complaint(
        &self,
        id: &EcdsaMessageId,
        transcript: &IDkgTranscript,
        signed_complaint: &EcdsaComplaint,
    ) -> Option<EcdsaChangeAction> {
        let complaint = signed_complaint.get();

        // Verify the signature
        if let Err(error) = self
            .crypto
            .verify(signed_complaint, transcript.registry_version)
        {
            if error.is_reproducible() {
                self.metrics
                    .complaint_errors_inc("verify_complaint_signature_permanent");
                return Some(EcdsaChangeAction::HandleInvalid(
                    id.clone(),
                    format!(
                        "Complaint signature validation(permanent error): {}, error = {:?}",
                        signed_complaint, error
                    ),
                ));
            } else {
                // Defer in case of transient errors
                debug!(
                    self.log,
                    "Complaint signature validation(transient error): {}, error = {:?}",
                    signed_complaint,
                    error
                );
                self.metrics
                    .complaint_errors_inc("verify_complaint_signature_transient");
                return None;
            }
        }

        self.crypto
            .verify_complaint(
                transcript,
                signed_complaint.signature.signer,
                &complaint.idkg_complaint,
            )
            .map_or_else(
                |error| {
                    if error.is_reproducible() {
                        self.metrics
                            .complaint_errors_inc("verify_complaint_permanent");
                        Some(EcdsaChangeAction::HandleInvalid(
                            id.clone(),
                            format!(
                                "Complaint validation(permanent error): {}, error = {:?}",
                                signed_complaint, error
                            ),
                        ))
                    } else {
                        debug!(
                            self.log,
                            "Complaint validation(transient error): {}, error = {:?}",
                            signed_complaint,
                            error
                        );
                        self.metrics
                            .complaint_errors_inc("verify_complaint_transient");
                        None
                    }
                },
                |()| {
                    self.metrics.complaint_metrics_inc("complaint_received");
                    Some(EcdsaChangeAction::MoveToValidated(id.clone()))
                },
            )
    }

    /// Helper to create a signed opening
    fn crypto_create_opening(
        &self,
        signed_complaint: &EcdsaComplaint,
        transcript: &IDkgTranscript,
    ) -> EcdsaChangeSet {
        let complaint = signed_complaint.get();

        // Create the opening
        let idkg_opening = match self.crypto.open_transcript(
            transcript,
            signed_complaint.signature.signer,
            &complaint.idkg_complaint,
        ) {
            Ok(opening) => opening,
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to create opening for complaint {}, error = {:?}",
                    signed_complaint,
                    err
                );
                self.metrics.complaint_errors_inc("open_transcript");
                return Default::default();
            }
        };

        // Sign the opening
        let content = EcdsaOpeningContent { idkg_opening };
        match self
            .crypto
            .sign(&content, self.node_id, transcript.registry_version)
        {
            Ok(signature) => {
                let ecdsa_opening = EcdsaOpening { content, signature };
                self.metrics.complaint_metrics_inc("openings_sent");
                vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaOpening(ecdsa_opening),
                )]
            }
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to sign opening for complaint {}, error = {:?}", signed_complaint, err
                );
                self.metrics.complaint_errors_inc("sign_opening");
                Default::default()
            }
        }
    }

    /// Helper to verify the opening
    fn crypto_verify_opening(
        &self,
        id: &EcdsaMessageId,
        transcript: &IDkgTranscript,
        signed_opening: &EcdsaOpening,
        signed_complaint: &EcdsaComplaint,
    ) -> Option<EcdsaChangeAction> {
        let opening = signed_opening.get();
        let complaint = signed_complaint.get();

        // Verify the signature
        if let Err(error) = self
            .crypto
            .verify(signed_opening, transcript.registry_version)
        {
            if error.is_reproducible() {
                self.metrics
                    .complaint_errors_inc("verify_opening_signature_permanent");
                return Some(EcdsaChangeAction::HandleInvalid(
                    id.clone(),
                    format!(
                        "Opening signature validation(permanent error): {}, error = {:?}",
                        signed_opening, error
                    ),
                ));
            } else {
                debug!(
                    self.log,
                    "Opening signature validation(transient error): {}, error = {:?}",
                    signed_opening,
                    error
                );
                self.metrics
                    .complaint_errors_inc("verify_opening_signature_transient");
                return None;
            }
        }

        // Verify the opening
        self.crypto
            .verify_opening(
                transcript,
                signed_opening.signature.signer,
                &opening.idkg_opening,
                &complaint.idkg_complaint,
            )
            .map_or_else(
                |error| {
                    if error.is_reproducible() {
                        self.metrics
                            .complaint_errors_inc("verify_opening_permanent");
                        Some(EcdsaChangeAction::HandleInvalid(
                            id.clone(),
                            format!(
                                "Opening validation(permanent error): {}, error = {:?}",
                                signed_opening, error
                            ),
                        ))
                    } else {
                        debug!(
                            self.log,
                            "Opening validation(transient error): {}, error = {:?}",
                            signed_opening,
                            error
                        );
                        self.metrics
                            .complaint_errors_inc("verify_opening_transient");
                        None
                    }
                },
                |()| {
                    self.metrics.complaint_metrics_inc("opening_received");
                    Some(EcdsaChangeAction::MoveToValidated(id.clone()))
                },
            )
    }

    /// Checks if the complainer already issued a complaint for the given
    /// IDkgComplaint
    fn has_complainer_issued_complaint(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        idkg_complaint: &IDkgComplaint,
        complainer_id: &NodeId,
    ) -> bool {
        let prefix = complaint_prefix(
            &idkg_complaint.transcript_id,
            &idkg_complaint.dealer_id,
            complainer_id,
        );
        ecdsa_pool
            .validated()
            .complaints_by_prefix(prefix)
            .any(|(_, signed_complaint)| {
                let complaint = signed_complaint.get();
                signed_complaint.signature.signer == *complainer_id
                    && complaint.idkg_complaint.transcript_id == idkg_complaint.transcript_id
                    && complaint.idkg_complaint.dealer_id == idkg_complaint.dealer_id
            })
    }

    /// Looks up the complaint for the given opening
    fn get_complaint_for_opening(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        signed_opening: &EcdsaOpening,
    ) -> Option<EcdsaComplaint> {
        let opening = signed_opening.get();
        ecdsa_pool
            .validated()
            .complaints()
            .find(|(_, signed_complaint)| {
                let complaint = signed_complaint.get();
                complaint.idkg_complaint.transcript_id == opening.idkg_opening.transcript_id
                    && complaint.idkg_complaint.dealer_id == opening.idkg_opening.dealer_id
            })
            .map(|(_, signed_complaint)| signed_complaint)
    }

    /// Checks if the node has issued an opening for the complaint
    /// <transcript Id, dealer Id, opener Id>
    fn has_node_issued_opening(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_id: &IDkgTranscriptId,
        dealer_id: &NodeId,
        opener_id: &NodeId,
    ) -> bool {
        let prefix = opening_prefix(transcript_id, dealer_id, opener_id);
        ecdsa_pool
            .validated()
            .openings_by_prefix(prefix)
            .any(|(_, signed_opening)| {
                let opening = signed_opening.get();
                opening.idkg_opening.transcript_id == *transcript_id
                    && opening.idkg_opening.dealer_id == *dealer_id
                    && signed_opening.signature.signer == *opener_id
            })
    }

    /// Looks up the valid openings for the given complaint (if any)
    fn get_openings_for_complaint(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        complaint: &IDkgComplaint,
    ) -> BTreeMap<NodeId, IDkgOpening> {
        let mut openings = BTreeMap::new();
        for (_, signed_opening) in ecdsa_pool.validated().openings() {
            let opening = signed_opening.get();
            if opening.idkg_opening.transcript_id == complaint.transcript_id
                && opening.idkg_opening.dealer_id == complaint.dealer_id
                && signed_opening.signature.signer != self.node_id
            {
                openings.insert(
                    signed_opening.signature.signer,
                    opening.idkg_opening.clone(),
                );
            }
        }
        openings
    }

    /// Checks if the artifact should be purged
    fn should_purge(
        &self,
        transcript_id: &IDkgTranscriptId,
        requested_height: Height,
        current_height: Height,
        active_transcripts: &BTreeSet<IDkgTranscriptId>,
    ) -> bool {
        requested_height <= current_height && !active_transcripts.contains(transcript_id)
    }

    /// Resolves the active ref -> transcripts
    fn resolve_ref(
        &self,
        transcript_ref: &TranscriptRef,
        block_reader: &dyn EcdsaBlockReader,
        reason: &str,
    ) -> Option<IDkgTranscript> {
        let _timer = self
            .metrics
            .on_state_change_duration
            .with_label_values(&["resolve_transcript_refs"])
            .start_timer();
        match block_reader.transcript(transcript_ref) {
            Ok(transcript) => {
                self.metrics
                    .complaint_metrics_inc("resolve_transcript_refs");
                Some(transcript)
            }
            Err(error) => {
                warn!(
                    self.log,
                    "Failed to resolve complaint ref: reason = {}, \
                     transcript_ref = {:?}, error = {:?}",
                    reason,
                    transcript_ref,
                    error
                );
                self.metrics.complaint_errors_inc("resolve_transcript_refs");
                None
            }
        }
    }

    /// Returns the active transcript map.
    fn active_transcripts(
        &self,
        block_reader: &dyn EcdsaBlockReader,
    ) -> BTreeMap<IDkgTranscriptId, TranscriptRef> {
        block_reader
            .active_transcripts()
            .iter()
            .map(|transcript_ref| (transcript_ref.transcript_id, *transcript_ref))
            .collect::<BTreeMap<_, _>>()
    }
}

impl EcdsaComplaintHandler for EcdsaComplaintHandlerImpl {
    fn on_state_change(&self, ecdsa_pool: &dyn EcdsaPool) -> EcdsaChangeSet {
        let block_reader = EcdsaBlockReaderImpl::new(self.consensus_block_cache.finalized_chain());
        let metrics = self.metrics.clone();

        let validate_complaints = || {
            timed_call(
                "validate_complaints",
                || self.validate_complaints(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let send_openings = || {
            timed_call(
                "send_openings",
                || self.send_openings(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let validate_openings = || {
            timed_call(
                "validate_openings",
                || self.validate_openings(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let purge_artifacts = || {
            timed_call(
                "purge_artifacts",
                || self.purge_artifacts(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };

        let calls: [&'_ dyn Fn() -> EcdsaChangeSet; 4] = [
            &validate_complaints,
            &send_openings,
            &validate_openings,
            &purge_artifacts,
        ];
        self.schedule.call_next(&calls)
    }

    fn as_transcript_loader(&self) -> &dyn EcdsaTranscriptLoader {
        self
    }
}

pub(crate) trait EcdsaTranscriptLoader: Send {
    /// Loads the given transcript
    fn load_transcript(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript: &IDkgTranscript,
    ) -> TranscriptLoadStatus;
}

pub(crate) enum TranscriptLoadStatus {
    /// Transcript was loaded successfully
    Success,

    /// Failed to load the transcript
    Failure,

    /// Resulted in new complaints
    Complaints(Vec<EcdsaComplaint>),
}

impl EcdsaTranscriptLoader for EcdsaComplaintHandlerImpl {
    fn load_transcript(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript: &IDkgTranscript,
    ) -> TranscriptLoadStatus {
        // 1. Try loading the transcripts without openings
        let complaints = match IDkgProtocol::load_transcript(&*self.crypto, transcript) {
            Ok(complaints) => {
                if complaints.is_empty() {
                    self.metrics.complaint_metrics_inc("transcripts_loaded");
                    return TranscriptLoadStatus::Success;
                }
                complaints
            }
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to load transcript: transcript_id: {:?}, error = {:?}",
                    transcript.transcript_id,
                    err
                );
                self.metrics.complaint_errors_inc("load_transcript");
                return TranscriptLoadStatus::Failure;
            }
        };

        // 2. Add any new complaints to the pool
        let mut new_complaints = Vec::new();
        let mut old_complaints = Vec::new();
        for complaint in complaints {
            if !self.has_complainer_issued_complaint(ecdsa_pool, &complaint, &self.node_id) {
                if let Some(ecdsa_complaint) =
                    self.crypto_create_complaint(complaint, transcript.registry_version)
                {
                    new_complaints.push(ecdsa_complaint);
                } else {
                    return TranscriptLoadStatus::Failure;
                }
            } else {
                old_complaints.push(complaint);
            }
        }
        if !new_complaints.is_empty() {
            return TranscriptLoadStatus::Complaints(new_complaints);
        }

        // 3. No new complaints. Collect the validated openings for the old complaints
        // and retry loading the transcript
        let mut openings = BTreeMap::new();
        for complaint in old_complaints {
            let complaint_openings = self.get_openings_for_complaint(ecdsa_pool, &complaint);
            openings.insert(complaint, complaint_openings);
        }
        // TODO: check num openings satisfies the threshold
        match IDkgProtocol::load_transcript_with_openings(&*self.crypto, transcript, &openings) {
            Ok(()) => {
                self.metrics
                    .complaint_metrics_inc("transcripts_loaded_with_openings");
                TranscriptLoadStatus::Success
            }
            Err(IDkgLoadTranscriptError::InsufficientOpenings { .. }) => {
                self.metrics
                    .complaint_errors_inc("load_transcript_with_openings_threshold");
                TranscriptLoadStatus::Failure
            }
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to load transcript with openings: transcript_id: {:?}, error = {:?}",
                    transcript.transcript_id,
                    err
                );
                self.metrics
                    .complaint_errors_inc("load_transcript_with_openings");
                TranscriptLoadStatus::Failure
            }
        }
    }
}

/// Specifies how to handle a received message
#[derive(Eq, PartialEq, Debug)]
#[allow(clippy::large_enum_variant)]
enum Action<'a> {
    /// The message is relevant to our current state, process it
    /// immediately.
    Process(&'a TranscriptRef),

    /// Keep it to be processed later (e.g) this is from a node
    /// ahead of us
    Defer,

    /// Don't need it
    Drop,
}

impl<'a> Action<'a> {
    /// Decides the action to take on a received message with the given
    /// height/transcriptId
    #[allow(clippy::self_named_constructors)]
    fn action(
        block_reader: &'a dyn EcdsaBlockReader,
        active_transcripts: &'a BTreeMap<IDkgTranscriptId, TranscriptRef>,
        msg_height: Height,
        msg_transcript_id: &IDkgTranscriptId,
    ) -> Action<'a> {
        if msg_height > block_reader.tip_height() {
            // Message is from a node ahead of us, keep it to be
            // processed later
            return Action::Defer;
        }

        match active_transcripts.get(msg_transcript_id) {
            Some(transcript_ref) => Action::Process(transcript_ref),
            None => Action::Drop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecdsa::utils::test_utils::*;
    use ic_crypto_test_utils_canister_threshold_sigs::CanisterThresholdSigTestEnvironment;
    use ic_interfaces::artifact_pool::UnvalidatedArtifact;
    use ic_interfaces::ecdsa::MutableEcdsaPool;
    use ic_interfaces::time_source::TimeSource;
    use ic_test_utilities::types::ids::{NODE_1, NODE_2, NODE_3, NODE_4};
    use ic_test_utilities::FastForwardTimeSource;
    use ic_test_utilities_logger::with_test_replica_logger;
    use ic_types::consensus::ecdsa::{EcdsaObject, TranscriptRef};
    use ic_types::Height;

    // Tests the Action logic
    #[test]
    fn test_ecdsa_complaint_action() {
        let (id_1, id_2, id_3, id_4) = (
            create_transcript_id(1),
            create_transcript_id(2),
            create_transcript_id(3),
            create_transcript_id(4),
        );

        let ref_1 = TranscriptRef::new(Height::new(10), id_1);
        let ref_2 = TranscriptRef::new(Height::new(20), id_2);
        let block_reader =
            TestEcdsaBlockReader::for_complainer_test(Height::new(100), vec![ref_1, ref_2]);
        let mut active_transcripts = BTreeMap::new();
        active_transcripts.insert(id_1, ref_1);
        active_transcripts.insert(id_2, ref_2);

        // Message from a node ahead of us
        assert_eq!(
            Action::action(&block_reader, &active_transcripts, Height::from(200), &id_3),
            Action::Defer
        );

        // Messages for transcripts not currently active
        assert_eq!(
            Action::action(&block_reader, &active_transcripts, Height::from(10), &id_3),
            Action::Drop
        );
        assert_eq!(
            Action::action(&block_reader, &active_transcripts, Height::from(20), &id_4),
            Action::Drop
        );

        // Messages for transcripts currently active
        assert!(matches!(
            Action::action(&block_reader, &active_transcripts, Height::from(10), &id_1),
            Action::Process(_)
        ));
        assert!(matches!(
            Action::action(&block_reader, &active_transcripts, Height::from(20), &id_2),
            Action::Process(_)
        ));
    }

    #[test]
    fn test_crypto_verify_complaint() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let env = CanisterThresholdSigTestEnvironment::new(1);
                let crypto = env.crypto_components.into_values().next().unwrap();
                let (_, complaint_handler) = create_complaint_dependencies_with_crypto(
                    pool_config,
                    logger,
                    Some(Arc::new(crypto)),
                );
                let id = create_transcript_id_with_height(2, Height::from(20));
                let transcript = create_transcript(id, &[NODE_2]);
                let complaint = create_complaint(id, NODE_2, NODE_3);
                let changeset: Vec<_> = complaint_handler
                    .crypto_verify_complaint(&complaint.message_id(), &transcript, &complaint)
                    .into_iter()
                    .collect();
                // assert that the mock complaint does not pass real crypto check
                assert!(is_handle_invalid(&changeset, &complaint.message_id()));
            })
        })
    }

    // Tests validation of the received complaints
    #[test]
    fn test_ecdsa_validate_complaints() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(200)),
                    create_transcript_id_with_height(2, Height::from(20)),
                    create_transcript_id_with_height(3, Height::from(30)),
                );

                // Set up the ECDSA pool
                // Complaint from a node ahead of us (deferred)
                let complaint = create_complaint(id_1, NODE_2, NODE_3);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Complaint for a transcript not currently active (dropped)
                let complaint = create_complaint(id_2, NODE_2, NODE_3);
                let msg_id_2 = complaint.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Complaint for a transcript currently active (accepted)
                let complaint = create_complaint(id_3, NODE_2, NODE_3);
                let msg_id_3 = complaint.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Only id_3 is active
                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_3)],
                );
                let change_set = complaint_handler.validate_complaints(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_2));
                assert!(is_moved_to_validated(&change_set, &msg_id_3));
            })
        })
    }

    // Tests that duplicate complaint from the same complainer is dropped
    #[test]
    fn test_ecdsa_duplicate_complaints() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_1 = create_transcript_id_with_height(1, Height::from(30));

                // Set up the ECDSA pool
                // Complaint from NODE_3 for transcript id_1, dealer NODE_2
                let complaint = create_complaint(id_1, NODE_2, NODE_3);
                let msg_id = complaint.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint.clone()),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Validated pool already has complaint from NODE_3 for
                // transcript id_1, dealer NODE_2
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(30),
                    vec![TranscriptRef::new(Height::new(30), id_1)],
                );
                let change_set = complaint_handler.validate_complaints(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id));
            })
        })
    }

    // Tests that duplicate complaint from the same complainer in the unvalidated
    // pool  is dropped
    #[test]
    fn test_ecdsa_duplicate_complaints_in_batch() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_1 = create_transcript_id_with_height(1, Height::from(30));

                // Set up the ECDSA pool
                // Complaint from NODE_3 for transcript id_1, dealer NODE_2
                let complaint = create_complaint_with_nonce(id_1, NODE_2, NODE_3, 0);
                let msg_id_1 = complaint.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Complaint from NODE_3 for transcript id_1, dealer NODE_2
                let complaint = create_complaint_with_nonce(id_1, NODE_2, NODE_3, 1);
                let msg_id_2 = complaint.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(30), id_1)],
                );
                let change_set = complaint_handler.validate_complaints(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                // One is considered duplicate
                assert!(is_handle_invalid(&change_set, &msg_id_1));
                // One is considered valid
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that openings are sent for eligible complaints
    #[test]
    fn test_ecdsa_send_openings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    create_transcript_id(1),
                    create_transcript_id(2),
                    create_transcript_id(3),
                );

                // Complaint for which we haven't issued an opening. This should
                // result in opening sent out.
                let complaint = create_complaint(id_1, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Complaint for which we already issued an opening. This should
                // not result in an opening.
                let complaint = create_complaint(id_2, NODE_2, NODE_3);
                let opening = create_opening(id_2, NODE_2, NODE_3, NODE_1);
                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaComplaint(complaint)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaOpening(opening)),
                ];
                ecdsa_pool.apply_changes(change_set);

                // Complaint for transcript not in the active list
                let complaint = create_complaint(id_3, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![
                        TranscriptRef::new(Height::new(10), id_1),
                        TranscriptRef::new(Height::new(20), id_2),
                    ],
                );
                let change_set = complaint_handler.send_openings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_opening_added_to_validated(
                    &change_set,
                    &id_1,
                    &NODE_2,
                    &NODE_1
                ));
            })
        })
    }

    #[test]
    fn test_crypto_verify_opening() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let env = CanisterThresholdSigTestEnvironment::new(1);
                let crypto = env.crypto_components.into_values().next().unwrap();
                let (_, complaint_handler) = create_complaint_dependencies_with_crypto(
                    pool_config,
                    logger,
                    Some(Arc::new(crypto)),
                );
                let id = create_transcript_id_with_height(2, Height::from(20));
                let transcript = create_transcript(id, &[NODE_2]);
                let complaint = create_complaint(id, NODE_2, NODE_3);
                let opening = create_opening(id, NODE_2, NODE_3, NODE_4);
                let changeset: Vec<_> = complaint_handler
                    .crypto_verify_opening(&opening.message_id(), &transcript, &opening, &complaint)
                    .into_iter()
                    .collect();
                // assert that the mock opening does not pass real crypto check
                assert!(is_handle_invalid(&changeset, &opening.message_id()));
            })
        })
    }

    // Tests the validation of received openings
    #[test]
    fn test_ecdsa_validate_openings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3, id_4) = (
                    create_transcript_id_with_height(1, Height::from(400)),
                    create_transcript_id_with_height(2, Height::from(20)),
                    create_transcript_id_with_height(3, Height::from(30)),
                    create_transcript_id_with_height(4, Height::from(40)),
                );

                // Set up the ECDSA pool
                // Opening from a node ahead of us (deferred)
                let opening = create_opening(id_1, NODE_2, NODE_3, NODE_4);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Opening for a transcript not currently active(dropped)
                let opening = create_opening(id_2, NODE_2, NODE_3, NODE_4);
                let msg_id_1 = opening.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Opening for a transcript currently active,
                // with a matching complaint (accepted)
                let opening = create_opening(id_3, NODE_2, NODE_3, NODE_4);
                let msg_id_2 = opening.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                let complaint = create_complaint(id_3, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Opening for a transcript currently active,
                // without a matching complaint (deferred)
                let opening = create_opening(id_4, NODE_2, NODE_3, NODE_4);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![
                        TranscriptRef::new(Height::new(10), id_3),
                        TranscriptRef::new(Height::new(20), id_4),
                    ],
                );
                let change_set = complaint_handler.validate_openings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_1));
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that duplicate openings are dropped
    #[test]
    fn test_ecdsa_duplicate_openings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_1 = create_transcript_id_with_height(1, Height::from(20));

                // Set up the ECDSA pool
                // Opening from NODE_4 for transcript id_1, dealer NODE_2, complainer NODE_3
                let opening = create_opening(id_1, NODE_2, NODE_3, NODE_4);
                let msg_id = opening.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening.clone()),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Validated pool already has it
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaOpening(opening),
                )];
                ecdsa_pool.apply_changes(change_set);

                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_1)],
                );
                let change_set = complaint_handler.validate_openings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id));
            })
        })
    }

    // Tests that duplicate openings from the same opener in the unvalidated
    // pool is dropped
    #[test]
    fn test_ecdsa_duplicate_openings_in_batch() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_1 = create_transcript_id_with_height(1, Height::from(20));

                // Set up the ECDSA pool
                // Opening from NODE_4 for transcript id_1, dealer NODE_2, complainer NODE_3
                let opening = create_opening_with_nonce(id_1, NODE_2, NODE_3, NODE_4, 1);
                let msg_id_1 = opening.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Opening from NODE_4 for transcript id_1, dealer NODE_2, complainer NODE_3
                let opening = create_opening_with_nonce(id_1, NODE_2, NODE_3, NODE_4, 2);
                let msg_id_2 = opening.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Make sure we also have matching complaints
                let complaint = create_complaint(id_1, NODE_2, NODE_3);
                let message = EcdsaMessage::EcdsaComplaint(complaint);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: message.clone(),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });
                let change_set = vec![EcdsaChangeAction::AddToValidated(message)];
                ecdsa_pool.apply_changes(change_set);

                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_1)],
                );
                let change_set = complaint_handler.validate_openings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                // One is considered duplicate
                assert!(is_handle_invalid(&change_set, &msg_id_2));
                // One is considered valid
                assert!(is_moved_to_validated(&change_set, &msg_id_1));
            })
        })
    }

    // Tests purging of complaints from unvalidated pool
    #[test]
    fn test_ecdsa_purge_unvalidated_complaints() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(30)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Complaint 1: height <= current_height, active transcripts (not purged)
                let complaint = create_complaint(id_1, NODE_2, NODE_3);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Complaint 2: height <= current_height, non-active transcripts (purged)
                let complaint = create_complaint(id_2, NODE_2, NODE_3);
                let msg_id = complaint.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Complaint 3: height > current_height (not purged)
                let complaint = create_complaint(id_3, NODE_2, NODE_3);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaComplaint(complaint),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Only id_1 is active
                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_1)],
                );
                let change_set = complaint_handler.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id));
            })
        })
    }

    // Tests purging of complaints from validated pool
    #[test]
    fn test_ecdsa_purge_validated_complaints() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(30)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Complaint 1: height <= current_height, active transcripts (not purged)
                let complaint = create_complaint(id_1, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Complaint 2: height <= current_height, non-active transcripts (purged)
                let complaint = create_complaint(id_2, NODE_2, NODE_3);
                let msg_id = complaint.message_id();
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Complaint 3: height > current_height (not purged)
                let complaint = create_complaint(id_3, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaComplaint(complaint),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Only id_1 is active
                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_1)],
                );
                let change_set = complaint_handler.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_validated(&change_set, &msg_id));
            })
        })
    }

    // Tests purging of openings from unvalidated pool
    #[test]
    fn test_ecdsa_purge_unvalidated_openings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(30)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Opening 1: height <= current_height, active transcripts (not purged)
                let opening = create_opening(id_1, NODE_2, NODE_3, NODE_4);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Opening 2: height <= current_height, non-active transcripts (purged)
                let opening = create_opening(id_2, NODE_2, NODE_3, NODE_4);
                let msg_id = opening.message_id();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Complaint 3: height > current_height (not purged)
                let opening = create_opening(id_3, NODE_2, NODE_3, NODE_4);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaOpening(opening),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                // Only id_1 is active
                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_1)],
                );
                let change_set = complaint_handler.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id));
            })
        })
    }

    // Tests purging of openings from validated pool
    #[test]
    fn test_ecdsa_purge_validated_openings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, complaint_handler) =
                    create_complaint_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(30)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Opening 1: height <= current_height, active transcripts (not purged)
                let opening = create_opening(id_1, NODE_2, NODE_3, NODE_4);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaOpening(opening),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Opening 2: height <= current_height, non-active transcripts (purged)
                let opening = create_opening(id_2, NODE_2, NODE_3, NODE_4);
                let msg_id = opening.message_id();
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaOpening(opening),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Complaint 3: height > current_height (not purged)
                let opening = create_opening(id_3, NODE_2, NODE_3, NODE_4);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaOpening(opening),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Only id_1 is active
                let block_reader = TestEcdsaBlockReader::for_complainer_test(
                    Height::new(100),
                    vec![TranscriptRef::new(Height::new(10), id_1)],
                );
                let change_set = complaint_handler.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_validated(&change_set, &msg_id));
            })
        })
    }
}
