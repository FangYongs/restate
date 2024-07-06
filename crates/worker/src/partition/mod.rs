// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::fmt::Debug;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use assert2::let_assert;
use futures::TryStreamExt as _;
use metrics::{counter, histogram};
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_stream::StreamExt;
use tracing::{debug, instrument, trace, Span};

use restate_bifrost::{Bifrost, FindTailAttributes, LogRecord, Record};
use restate_core::cancellation_watcher;
use restate_core::metadata;
use restate_core::network::Networking;
use restate_partition_store::{PartitionStore, RocksDBTransaction};
use restate_storage_api::deduplication_table::{
    DedupInformation, DedupSequenceNumber, EpochSequenceNumber, ProducerId,
};
use restate_storage_api::StorageError;
use restate_types::cluster::cluster_state::{PartitionProcessorStatus, ReplayStatus, RunMode};
use restate_types::identifiers::{PartitionId, PartitionKey};
use restate_types::journal::raw::RawEntryCodec;
use restate_types::logs::{LogId, Lsn, SequenceNumber};
use restate_types::time::MillisSinceEpoch;
use restate_wal_protocol::control::AnnounceLeader;
use restate_wal_protocol::{Command, Destination, Envelope, Header};

use self::storage::invoker::InvokerStorageReader;
use crate::metric_definitions::{
    PARTITION_ACTUATOR_HANDLED, PARTITION_LABEL, PARTITION_LEADER_HANDLE_ACTION_BATCH_DURATION,
    PP_APPLY_RECORD_DURATION,
};
use crate::partition::leadership::LeadershipState;
use crate::partition::state_machine::{ActionCollector, Effects, StateMachine};
use crate::partition::storage::{DedupSequenceNumberResolver, PartitionStorage, Transaction};

mod action_effect_handler;
mod leadership;
pub mod shuffle;
mod state_machine;
pub mod storage;
pub mod types;

/// Control messages from Manager to individual partition processor instances.
pub enum PartitionProcessorControlCommand {}

#[derive(Debug)]
pub(super) struct PartitionProcessorBuilder<InvokerInputSender> {
    pub partition_id: PartitionId,
    pub partition_key_range: RangeInclusive<PartitionKey>,

    num_timers_in_memory_limit: Option<usize>,
    channel_size: usize,

    status: PartitionProcessorStatus,
    invoker_tx: InvokerInputSender,
    control_rx: mpsc::Receiver<PartitionProcessorControlCommand>,
    status_watch_tx: watch::Sender<PartitionProcessorStatus>,
}

impl<InvokerInputSender> PartitionProcessorBuilder<InvokerInputSender>
where
    InvokerInputSender:
        restate_invoker_api::ServiceHandle<InvokerStorageReader<PartitionStore>> + Clone,
{
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        partition_id: PartitionId,
        partition_key_range: RangeInclusive<PartitionKey>,
        status: PartitionProcessorStatus,
        num_timers_in_memory_limit: Option<usize>,
        channel_size: usize,
        control_rx: mpsc::Receiver<PartitionProcessorControlCommand>,
        status_watch_tx: watch::Sender<PartitionProcessorStatus>,
        invoker_tx: InvokerInputSender,
    ) -> Self {
        Self {
            partition_id,
            partition_key_range,
            status,
            num_timers_in_memory_limit,
            channel_size,
            invoker_tx,
            control_rx,
            status_watch_tx,
        }
    }

    pub async fn build<Codec: RawEntryCodec + Default + Debug>(
        self,
        networking: Networking,
        bifrost: Bifrost,
        partition_store: PartitionStore,
    ) -> Result<PartitionProcessor<Codec, InvokerInputSender>, StorageError> {
        let PartitionProcessorBuilder {
            partition_id,
            partition_key_range,
            num_timers_in_memory_limit,
            channel_size,
            invoker_tx,
            control_rx,
            status_watch_tx,
            status,
            ..
        } = self;

        let mut partition_storage =
            PartitionStorage::new(partition_id, partition_key_range.clone(), partition_store);

        let state_machine = Self::create_state_machine::<Codec>(
            &mut partition_storage,
            partition_key_range.clone(),
        )
        .await?;

        let leadership_state = LeadershipState::follower(
            partition_id,
            partition_key_range.clone(),
            num_timers_in_memory_limit,
            channel_size,
            invoker_tx,
            bifrost.clone(),
            networking,
        );

        Ok(PartitionProcessor {
            partition_id,
            partition_key_range,
            leadership_state,
            state_machine,
            partition_storage: Some(partition_storage),
            bifrost,
            control_rx,
            status_watch_tx,
            status,
        })
    }

    async fn create_state_machine<Codec>(
        partition_storage: &mut PartitionStorage<PartitionStore>,
        partition_key_range: RangeInclusive<PartitionKey>,
    ) -> Result<StateMachine<Codec>, StorageError>
    where
        Codec: RawEntryCodec + Default + Debug,
    {
        let inbox_seq_number = partition_storage.load_inbox_seq_number().await?;
        let outbox_seq_number = partition_storage.load_outbox_seq_number().await?;

        let state_machine =
            StateMachine::new(inbox_seq_number, outbox_seq_number, partition_key_range);

        Ok(state_machine)
    }
}

pub struct PartitionProcessor<Codec, InvokerSender> {
    partition_id: PartitionId,
    partition_key_range: RangeInclusive<PartitionKey>,
    leadership_state: LeadershipState<InvokerSender>,
    state_machine: StateMachine<Codec>,
    bifrost: Bifrost,
    control_rx: mpsc::Receiver<PartitionProcessorControlCommand>,
    status_watch_tx: watch::Sender<PartitionProcessorStatus>,
    status: PartitionProcessorStatus,

    // will be taken by the `run` method to decouple transactions from self
    partition_storage: Option<PartitionStorage<PartitionStore>>,
}

impl<Codec, InvokerSender> PartitionProcessor<Codec, InvokerSender>
where
    Codec: RawEntryCodec + Default + Debug,
    InvokerSender: restate_invoker_api::ServiceHandle<InvokerStorageReader<PartitionStore>> + Clone,
{
    #[instrument(level = "info", skip_all, fields(partition_id = %self.partition_id, is_leader = tracing::field::Empty))]
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut partition_storage = self
            .partition_storage
            .take()
            .expect("partition storage must be configured");
        let last_applied_lsn = partition_storage.load_applied_lsn().await?;
        let last_applied_lsn = last_applied_lsn.unwrap_or(Lsn::INVALID);

        self.status.last_applied_log_lsn = Some(last_applied_lsn);
        let current_tail = self
            .bifrost
            .find_tail(
                LogId::from(self.partition_id),
                FindTailAttributes::default(),
            )
            .await?;
        debug!(
            last_applied_lsn = %last_applied_lsn,
            current_log_tail = ?current_tail,
            "PartitionProcessor creating log reader",
        );
        if current_tail.offset() == last_applied_lsn.next() {
            self.status.replay_status = ReplayStatus::Active;
        } else {
            // catching up.
            self.status.target_tail_lsn = Some(current_tail.offset());
            self.status.replay_status = ReplayStatus::CatchingUp;
        }

        // Start reading after the last applied lsn
        let mut log_reader = self
            .bifrost
            .create_reader(
                LogId::from(self.partition_id),
                last_applied_lsn.next(),
                Lsn::MAX,
            )
            .await?
            .map_ok(|record| {
                let LogRecord { record, offset } = record;
                match record {
                    Record::Data(payload) => {
                        let envelope = Envelope::from_bytes(payload.into_body())?;
                        anyhow::Ok((offset, envelope))
                    }
                    Record::TrimGap(_) => {
                        unimplemented!("Currently not supported")
                    }
                    Record::Seal(_) => {
                        unimplemented!("Currently not supported")
                    }
                }
            });

        // avoid synchronized timers. We pick a randomised timer between 500 and 1023 millis.
        let mut status_update_timer =
            tokio::time::interval(Duration::from_millis(500 + rand::random::<u64>() % 524));
        status_update_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut cancellation = std::pin::pin!(cancellation_watcher());
        let partition_id_str: &'static str = Box::leak(Box::new(self.partition_id.to_string()));
        // Telemetry setup
        let apply_record_latency =
            histogram!(PP_APPLY_RECORD_DURATION, PARTITION_LABEL => partition_id_str);
        let record_actions_latency = histogram!(PARTITION_LEADER_HANDLE_ACTION_BATCH_DURATION);
        let actuator_effects_handled = counter!(PARTITION_ACTUATOR_HANDLED);

        let mut action_collector = ActionCollector::default();
        let mut effects = Effects::default();

        loop {
            tokio::select! {
                _ = &mut cancellation => break,
                _command = self.control_rx.recv() => {
                    // todo: handle leadership change requests here
                }
                _ = status_update_timer.tick() => {
                    self.status_watch_tx.send_modify(|old| {
                        old.clone_from(&self.status);
                        old.updated_at = MillisSinceEpoch::now();
                    });
                }
                record = log_reader.next() => {
                    let command_start = Instant::now();
                    let Some(record) = record else {
                        // read stream terminated!
                        anyhow::bail!("Read stream terminated for partition processor");
                    };
                    let record = record??;
                    trace!(lsn = %record.0, "Processing bifrost record for '{}': {:?}", record.1.command.name(), record.1.header);

                    let mut transaction = partition_storage.create_transaction();

                    // clear buffers used when applying the next record
                    action_collector.clear();
                    effects.clear();

                    let leadership_change = self.apply_record(
                        record,
                        &mut transaction,
                        &mut effects,
                        &mut action_collector).await?;

                    if let Some(announce_leader) = leadership_change {
                        let new_esn = EpochSequenceNumber::new(announce_leader.leader_epoch);

                        self.status.last_observed_leader_epoch = Some(announce_leader.leader_epoch);
                        self.status.last_observed_leader_node = Some(announce_leader.node_id);
                        // update our own epoch sequence number to filter out messages from previous leaders
                        transaction.store_dedup_sequence_number(ProducerId::self_producer(), DedupSequenceNumber::Esn(new_esn)).await;
                        // commit all changes so far, this is important so that the actuators see all changes
                        // when becoming leader.
                        transaction.commit().await?;

                        // We can ignore all actions collected so far because as a new leader we have to instruct the
                        // actuators afresh.
                        action_collector.clear();

                        if announce_leader.node_id == metadata().my_node_id() {
                            let was_follower = !self.leadership_state.is_leader();
                            self.leadership_state.become_leader(new_esn, &mut partition_storage).await?;
                            self.status.effective_mode = Some(RunMode::Leader);
                            if was_follower {
                                Span::current().record("is_leader", self.leadership_state.is_leader());
                                debug!(leader_epoch = %new_esn.leader_epoch, "Partition leadership acquired");
                            }
                        } else {
                            let was_leader = self.leadership_state.is_leader();
                            self.leadership_state.become_follower().await?;
                            self.status.effective_mode = Some(RunMode::Follower);
                            if was_leader {
                                Span::current().record("is_leader", self.leadership_state.is_leader());
                                debug!(leader_epoch = %new_esn.leader_epoch, "Partition leadership lost to {}", announce_leader.node_id);
                            }
                        }
                        apply_record_latency.record(command_start.elapsed());
                    } else {
                        // Commit our changes and notify actuators about actions if we are the leader
                        transaction.commit().await?;
                        apply_record_latency.record(command_start.elapsed());
                        let actions_start = Instant::now();
                        self.leadership_state.handle_actions(action_collector.drain(..)).await?;
                        record_actions_latency.record(actions_start.elapsed());
                    }
                },
                Some(action_effects) = self.leadership_state.next_action_effects() => {
                    actuator_effects_handled.increment(action_effects.len() as u64);
                    self.leadership_state.handle_action_effect(action_effects).await?;
                },
            }
        }

        debug!(restate.node = %metadata().my_node_id(), %self.partition_id, "Shutting partition processor down.");
        self.leadership_state.become_follower().await?;

        Ok(())
    }

    async fn apply_record(
        &mut self,
        record: (Lsn, Envelope),
        transaction: &mut Transaction<RocksDBTransaction<'_>>,
        effects: &mut Effects,
        action_collector: &mut ActionCollector,
    ) -> Result<Option<AnnounceLeader>, state_machine::Error> {
        let (lsn, envelope) = record;
        transaction.store_applied_lsn(lsn).await?;

        // Update replay status
        self.status.last_applied_log_lsn = Some(record.0);
        self.status.last_record_applied_at = Some(MillisSinceEpoch::now());
        match self.status.replay_status {
            ReplayStatus::CatchingUp
                if self.status.target_tail_lsn.is_some_and(|v| record.0 >= v) =>
            {
                // finished catching up
                self.status.replay_status = ReplayStatus::Active;
            }
            _ => {}
        };

        if let Some(dedup_information) = self.is_targeted_to_me(&envelope.header) {
            // deduplicate if deduplication information has been provided
            if let Some(dedup_information) = dedup_information {
                if Self::is_outdated_or_duplicate(dedup_information, transaction).await? {
                    debug!(
                        "Ignoring outdated or duplicate message: {:?}",
                        envelope.header
                    );
                    return Ok(None);
                }
                transaction
                    .store_dedup_sequence_number(
                        dedup_information.producer_id.clone(),
                        dedup_information.sequence_number,
                    )
                    .await;
            }

            if let Command::AnnounceLeader(announce_leader) = envelope.command {
                let last_known_esn = transaction
                    .get_dedup_sequence_number(&ProducerId::self_producer())
                    .await?
                    .map(|dedup_sn| {
                        let_assert!(
                            DedupSequenceNumber::Esn(esn) = dedup_sn,
                            "self producer must store epoch sequence numbers!"
                        );
                        esn
                    });

                if last_known_esn
                    .map(|last_known_esn| {
                        last_known_esn.leader_epoch < announce_leader.leader_epoch
                    })
                    .unwrap_or(true)
                {
                    // leadership change detected, let's finish our transaction here
                    return Ok(Some(announce_leader));
                }
                debug!(
                    last_known_esn = %last_known_esn.as_ref().unwrap().leader_epoch,
                    announce_esn = %announce_leader.leader_epoch,
                    node_id = %announce_leader.node_id,
                    "Ignoring outdated leadership announcement."
                );
            } else {
                self.state_machine
                    .apply(
                        envelope.command,
                        effects,
                        transaction,
                        action_collector,
                        self.leadership_state.is_leader(),
                    )
                    .await?;
            }
        } else {
            self.status.num_skipped_records += 1;
            trace!(
                "Ignore message which is not targeted to me: {:?}",
                envelope.header
            );
        }

        Ok(None)
    }

    fn is_targeted_to_me<'a>(&self, header: &'a Header) -> Option<&'a Option<DedupInformation>> {
        match &header.dest {
            Destination::Processor {
                partition_key,
                dedup,
            } if self.partition_key_range.contains(partition_key) => Some(dedup),
            _ => None,
        }
    }

    async fn is_outdated_or_duplicate(
        dedup_information: &DedupInformation,
        dedup_resolver: &mut impl DedupSequenceNumberResolver,
    ) -> Result<bool, StorageError> {
        let last_dsn = dedup_resolver
            .get_dedup_sequence_number(&dedup_information.producer_id)
            .await?;

        // Check whether we have seen this message before
        let is_duplicate = if let Some(last_dsn) = last_dsn {
            match (last_dsn, &dedup_information.sequence_number) {
                (DedupSequenceNumber::Esn(last_esn), DedupSequenceNumber::Esn(esn)) => last_esn >= *esn,
                (DedupSequenceNumber::Sn(last_sn), DedupSequenceNumber::Sn(sn)) => last_sn >= *sn,
                (last_dsn, dsn) => panic!("sequence number types do not match: last sequence number '{:?}', received sequence number '{:?}'", last_dsn, dsn),
            }
        } else {
            false
        };

        Ok(is_duplicate)
    }
}
