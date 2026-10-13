use std::collections::BTreeSet;

use crate::sandbox::utils::transactions::CallMpcContract;
use crate::sandbox::utils::views::SandboxViewer;

use super::transactions::{all_receipts_successful, execute_async_handle_calls};
use mpc_contract::tee::tee_state::NodeId;
use mpc_primitives::hash::{LauncherImageHash, NodeImageHash, TeeVerifierCodeHash};
use near_mpc_contract_interface::{
    client::MpcContractHandle,
    method_names,
    types::{
        Attestation, Config, Ed25519PublicKey, GetAttestationResponse, GovernanceThreshold,
        Participants, ProtocolContractState, StoredAttestation,
    },
};
use near_workspaces::{
    Account, AccountId, Contract, result::ExecutionFinalResult, types::NearToken,
};

pub fn total_gas_fee(result: &ExecutionFinalResult) -> NearToken {
    result
        .outcomes()
        .iter()
        .map(|outcome| outcome.tokens_burnt)
        .fold(NearToken::ZERO, NearToken::saturating_add)
}

pub async fn get_config(mpc_contract: &MpcContractHandle<SandboxViewer>) -> anyhow::Result<Config> {
    Ok(mpc_contract.config().await?.value)
}

pub async fn get_state(mpc_contract: &MpcContractHandle<SandboxViewer>) -> ProtocolContractState {
    mpc_contract.state().await.unwrap().value
}

pub async fn get_participants(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
) -> anyhow::Result<Participants> {
    let state = get_state(mpc_contract).await;
    let ProtocolContractState::Running(running) = state else {
        panic!("Expected running state")
    };

    Ok(running.parameters.participants)
}

pub async fn get_tee_accounts(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
) -> anyhow::Result<BTreeSet<NodeId>> {
    Ok(mpc_contract
        .get_tee_accounts()
        .await?
        .value
        .into_iter()
        .collect())
}

pub async fn available_attestation_grants(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
    account_id: &AccountId,
) -> anyhow::Result<u32> {
    Ok(mpc_contract
        .available_attestation_grants(account_id.clone())
        .await?
        .value)
}

pub async fn prepay_attestation_grants(
    payer: &Account,
    contract: &Contract,
    beneficiary: &AccountId,
    grants: u32,
) -> anyhow::Result<ExecutionFinalResult> {
    let fee = attestation_storage_fee(contract).await?;
    let total = NearToken::from_yoctonear(fee.as_yoctonear() * u128::from(grants));
    payer
        .call_mpc(contract.id())
        .prepay_attestation_storage(beneficiary.clone(), grants, total)
        .await
        .map_err(Into::into)
}

/// The one `config` field the fee is computed from. Read on its own rather than through the
/// current `Config` DTO, which a released binary's config no longer deserializes into.
#[derive(serde::Deserialize)]
struct AttestationStorageFee {
    attestation_storage_fee_millinear: u64,
}

async fn attestation_storage_fee(contract: &Contract) -> anyhow::Result<NearToken> {
    let fee: AttestationStorageFee = contract.view(method_names::CONFIG).await?.json()?;
    Ok(NearToken::from_millinear(u128::from(
        fee.attestation_storage_fee_millinear,
    )))
}

/// Prepays one grant, then submits. For a first submission; a re-attestation of a key the
/// account already owns consumes no grant and should use [`submit_participant_info`].
pub async fn prepay_and_submit_participant_info(
    account: &Account,
    contract: &Contract,
    attestation: &Attestation,
    tls_key: &Ed25519PublicKey,
) -> anyhow::Result<ExecutionFinalResult> {
    let prepayment = prepay_attestation_grants(account, contract, account.id(), 1).await?;
    anyhow::ensure!(prepayment.is_success(), "prepayment failed: {prepayment:?}");
    submit_participant_info(account, contract, attestation, tls_key).await
}

/// Submits without prepaying anything.
pub async fn submit_participant_info(
    account: &Account,
    contract: &Contract,
    attestation: &Attestation,
    tls_key: &Ed25519PublicKey,
) -> anyhow::Result<ExecutionFinalResult> {
    // TODO(#3906): check if inlining is nicer once we ported the entire contract interface.
    let contract_handle = account.call_mpc(contract.id());
    contract_handle
        .submit_participant_info(attestation.clone(), tls_key.clone())
        .await
        .map_err(Into::into)
}

pub async fn tee_verifier_account_id(mpc_contract: &MpcContractHandle<SandboxViewer>) -> AccountId {
    mpc_contract.tee_verifier_account_id().await.unwrap().value
}

pub async fn get_participant_attestation(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
    tls_key: &Ed25519PublicKey,
) -> anyhow::Result<Option<StoredAttestation>> {
    let stored =
        mpc_contract
            .get_attestation(tls_key)
            .await?
            .value
            .map(|response| match response {
                GetAttestationResponse::Current(stored) => stored,
                GetAttestationResponse::Legacy(attestation) => StoredAttestation {
                    attestation,
                    accepted_at_seconds: None,
                },
            });
    Ok(stored)
}

pub async fn assert_running_return_participants(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
) -> anyhow::Result<Participants> {
    // Verify contract is back to running state with new threshold
    let final_state: ProtocolContractState = mpc_contract.state().await?.value;
    let ProtocolContractState::Running(running_state) = final_state else {
        panic!(
            "Expected contract to be in Running state after resharing, but got: {:?}",
            final_state
        );
    };
    Ok(running_state.parameters.participants)
}

pub async fn assert_running_return_threshold(
    mpc_contract: &MpcContractHandle<SandboxViewer>,
) -> GovernanceThreshold {
    let final_state: ProtocolContractState = get_state(mpc_contract).await;
    let ProtocolContractState::Running(running_state) = final_state else {
        panic!(
            "Expected contract to be in Running state: {:?}",
            final_state
        );
    };
    running_state.parameters.threshold
}

pub async fn vote_for_hash(
    account: &Account,
    contract: &Contract,
    image_hash: &NodeImageHash,
) -> anyhow::Result<()> {
    let result = account
        .call_mpc(contract.id())
        .vote_mpc_node_manifest_digest(*image_hash)
        .await?;
    all_receipts_successful(result)?;
    Ok(())
}

pub async fn vote_add_launcher_hash(
    account: &Account,
    contract: &Contract,
    launcher_hash: &LauncherImageHash,
) -> anyhow::Result<()> {
    let result = account
        .call(contract.id(), method_names::VOTE_ADD_LAUNCHER_HASH)
        .args_json(serde_json::json!({"launcher_hash": launcher_hash}))
        .transact()
        .await?;
    all_receipts_successful(result)?;
    Ok(())
}

pub async fn vote_tee_verifier_change(
    accounts: &[Account],
    contract_id: &AccountId,
    verifier: &AccountId,
) -> anyhow::Result<()> {
    // Arbitrary: the hash only buckets votes (voters must commit to the same
    // value), the contract never compares it to the deployed verifier code.
    let expected_code_hash = TeeVerifierCodeHash::new([7u8; 32]);
    execute_async_handle_calls(accounts, contract_id, |handle| {
        let verifier = verifier.clone();
        async move {
            handle
                .vote_tee_verifier_change(verifier, expected_code_hash)
                .await
        }
    })
    .await
}
