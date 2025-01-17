// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use anyhow::{anyhow, Error};
use restate_core::metadata;
use restate_ingress_http::{GetOutputResult, InvocationStorageReader};
use restate_partition_store::PartitionStoreManager;
use restate_storage_api::idempotency_table::ReadOnlyIdempotencyTable;
use restate_storage_api::invocation_status_table::{
    InvocationStatus, ReadOnlyInvocationStatusTable,
};
use restate_storage_api::service_status_table::{
    ReadOnlyVirtualObjectStatusTable, VirtualObjectStatus,
};
use restate_types::identifiers::WithPartitionKey;
use restate_types::ingress::{IngressResponseResult, InvocationResponse};
use restate_types::invocation::{
    InvocationQuery, InvocationTarget, InvocationTargetType, ResponseResult, WorkflowHandlerType,
};
use restate_types::partition_table::FindPartition;

#[derive(Debug, Clone)]
pub struct InvocationStorageReaderImpl {
    partition_store_manager: PartitionStoreManager,
}

impl InvocationStorageReaderImpl {
    pub fn new(partition_store_manager: PartitionStoreManager) -> Self {
        Self {
            partition_store_manager,
        }
    }
}

impl InvocationStorageReader for InvocationStorageReaderImpl {
    async fn get_output(&self, query: InvocationQuery) -> Result<GetOutputResult, Error> {
        let partition_id = metadata()
            .partition_table_ref()
            .find_partition_id(query.partition_key())?;
        let mut partition_storage = self
            .partition_store_manager
            .get_partition_store(partition_id)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "Can't find partition store for partition id {}",
                    partition_id
                )
            })?;

        let invocation_id = match query {
            InvocationQuery::Invocation(invocation_id) => invocation_id,
            ref q @ InvocationQuery::Workflow(ref service_id) => {
                match partition_storage
                    .get_virtual_object_status(service_id)
                    .await?
                {
                    VirtualObjectStatus::Locked(iid) => iid,
                    VirtualObjectStatus::Unlocked => {
                        // Try the deterministic id
                        q.to_invocation_id()
                    }
                }
            }
            ref q @ InvocationQuery::IdempotencyId(ref idempotency_id) => {
                match partition_storage
                    .get_idempotency_metadata(idempotency_id)
                    .await?
                {
                    Some(idempotency_metadata) => idempotency_metadata.invocation_id,
                    None => {
                        // Try the deterministic id
                        q.to_invocation_id()
                    }
                }
            }
        };

        let invocation_status = partition_storage
            .get_invocation_status(&invocation_id)
            .await?;

        match invocation_status {
            InvocationStatus::Free => Ok(GetOutputResult::NotFound),
            is if is.idempotency_key().is_none()
                && is
                    .invocation_target()
                    .map(InvocationTarget::invocation_target_ty)
                    != Some(InvocationTargetType::Workflow(
                        WorkflowHandlerType::Workflow,
                    )) =>
            {
                Ok(GetOutputResult::NotSupported)
            }
            InvocationStatus::Completed(completed) => {
                Ok(GetOutputResult::Ready(InvocationResponse {
                    request_id: Default::default(),
                    response: match completed.response_result.clone() {
                        ResponseResult::Success(res) => {
                            IngressResponseResult::Success(completed.invocation_target, res)
                        }
                        ResponseResult::Failure(err) => IngressResponseResult::Failure(err),
                    },
                    invocation_id: Some(invocation_id),
                }))
            }
            _ => Ok(GetOutputResult::NotReady),
        }
    }
}
