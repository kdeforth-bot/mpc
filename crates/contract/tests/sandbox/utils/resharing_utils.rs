use crate::sandbox::utils::{
    consts::GAS_FOR_VOTE_RESHARED,
    mpc_contract::get_state,
    transactions::{execute_async_handle_calls, execute_async_transactions},
    views::SandboxViewer,
};
use dtos::{AttemptId, EpochId, KeyEventId};
use mpc_contract::primitives::thresholds::{
    GovernanceThresholdParameters, ProposedGovernanceThresholdParameters,
};
use near_mpc_contract_interface::client::MpcContractHandle;
use near_mpc_contract_interface::method_names;
use near_mpc_contract_interface::types::{self as dtos, ProtocolContractState};
use near_workspaces::{Account, AccountId};
use serde_json::json;
use std::collections::BTreeMap;

pub async fn conclude_resharing(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
    all_participants: &[Account],
    prospective_epoch_id: EpochId,
) -> anyhow::Result<()> {
    let ProtocolContractState::Resharing(resharing_state) = get_state(mpc_contract).await else {
        anyhow::bail!("expected resharing state");
    };
    if resharing_state.resharing_key.epoch_id != prospective_epoch_id {
        anyhow::bail!("epoch id mismatch");
    }
    let domain_configs = resharing_state
        .previous_running_state
        .domains
        .domains
        .clone();
    for domain_config in &domain_configs {
        let key_event_id = KeyEventId {
            epoch_id: prospective_epoch_id,
            domain_id: domain_config.id,
            attempt_id: AttemptId(0),
        };
        let state = get_state(mpc_contract).await;
        if !matches!(state, ProtocolContractState::Resharing(_)) {
            anyhow::bail!("expected resharing state");
        }
        start_reshare_instance(mpc_contract, all_participants, key_event_id).await?;

        vote_reshared(mpc_contract.contract_id(), all_participants, key_event_id).await?;
    }
    Ok(())
}

pub async fn vote_cancel_reshaing(
    contract_id: &AccountId,
    accounts: &[Account],
) -> anyhow::Result<()> {
    execute_async_handle_calls(accounts, contract_id, |handle| async move {
        handle.vote_cancel_resharing().await
    })
    .await
}

pub async fn vote_new_parameters(
    contract_id: &AccountId,
    prospective_epoch_id: u64,
    proposal: &GovernanceThresholdParameters,
    persistent_participants: &[Account],
    new_participants: &[Account],
) -> anyhow::Result<()> {
    let proposal: dtos::ProposedGovernanceThresholdParameters =
        ProposedGovernanceThresholdParameters::new(proposal.clone(), BTreeMap::new()).into();
    // At least threshold old participants need to vote first,
    // here we are just using all of them
    execute_async_handle_calls(persistent_participants, contract_id, |handle| {
        let proposal = proposal.clone();
        async move {
            handle
                .vote_new_parameters(EpochId::new(prospective_epoch_id), proposal)
                .await
        }
    })
    .await?;

    // then new participant can vote
    execute_async_handle_calls(new_participants, contract_id, |handle| {
        let proposal = proposal.clone();
        async move {
            handle
                .vote_new_parameters(EpochId::new(prospective_epoch_id), proposal)
                .await
        }
    })
    .await?;
    Ok(())
}

pub async fn start_reshare_instance(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
    accounts: &[Account],
    key_event_id: KeyEventId,
) -> anyhow::Result<()> {
    let state = get_state(mpc_contract).await;
    let active = match &state {
        ProtocolContractState::Initializing(s) => {
            &s.generating_key.parameters.participants.participants
        }
        ProtocolContractState::Running(s) => &s.parameters.participants.participants,
        ProtocolContractState::Resharing(s) => {
            &s.resharing_key.parameters.participants.participants
        }
        ProtocolContractState::NotInitialized => {
            panic!("protocol state must be initialized")
        }
    };
    let leader = accounts
        .iter()
        .min_by_key(|a| {
            active
                .iter()
                .find(|(account_id, _, _)| account_id == a.id())
                .map(|(_, pid, _)| *pid)
                .unwrap()
        })
        .unwrap();
    let result = leader
        .call(
            mpc_contract.contract_id(),
            method_names::START_RESHARE_INSTANCE,
        )
        .args_json(json!({"key_event_id": key_event_id}))
        .transact()
        .await?;
    if !result.is_success() {
        anyhow::bail!("{result:#?}");
    }
    Ok(())
}

pub async fn vote_reshared(
    contract_id: &AccountId,
    accounts: &[Account],
    key_event_id: KeyEventId,
) -> anyhow::Result<()> {
    execute_async_transactions(
        accounts,
        contract_id,
        method_names::VOTE_RESHARED,
        &json!({"key_event_id": key_event_id}),
        GAS_FOR_VOTE_RESHARED,
    )
    .await
}

/// Performs a complete resharing operation with the given parameters.
/// This includes voting for new parameters, starting reshare instances for each domain,
/// and voting reshared to complete the transition.
pub async fn do_resharing(
    remaining_accounts: &[Account],
    mpc_contract: &MpcContractHandle<SandboxViewer>,
    new_threshold_parameters: GovernanceThresholdParameters,
    prospective_epoch_id: EpochId,
) -> anyhow::Result<()> {
    vote_new_parameters(
        mpc_contract.contract_id(),
        prospective_epoch_id.0,
        &new_threshold_parameters,
        remaining_accounts,
        &[],
    )
    .await?;
    conclude_resharing(mpc_contract, remaining_accounts, prospective_epoch_id).await?;
    Ok(())
}
