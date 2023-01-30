use ed25519_dalek::Verifier;
use locutus_aft_interface::{
    AllocationError, TokenAllocationRecord, TokenAllocationSummary, TokenAssignment,
    TokenParameters,
};
use locutus_stdlib::prelude::*;

struct TokenAllocContract;

impl ContractInterface for TokenAllocContract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let assigned_tokens = TokenAllocationRecord::try_from(state)?;
        let params = TokenParameters::try_from(parameters)?;
        for (_tier, assignments) in (&assigned_tokens).into_iter() {
            for assignment in assignments {
                if !assignment.is_valid(&params) {
                    return Ok(ValidateResult::Invalid);
                }
            }
        }
        Ok(ValidateResult::Valid)
    }

    /// The contract verifies that the release times for a tier matches the tier.
    ///
    /// For example, a 15:30 UTC release time isn't permitted for hour_1 tier, but 15:00 UTC is permitted.
    fn validate_delta(
        parameters: Parameters<'static>,
        delta: StateDelta<'static>,
    ) -> Result<bool, ContractError> {
        let assigned_token = TokenAssignment::try_from(delta)?;
        let params = TokenParameters::try_from(parameters)?;
        Ok(assigned_token.is_valid(&params))
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let mut assigned_tokens = TokenAllocationRecord::try_from(state)?;
        let params = TokenParameters::try_from(parameters)?;
        for update in data {
            match update {
                UpdateData::State(s) => {
                    let new_assigned_tokens = TokenAllocationRecord::try_from(s)?;
                    assigned_tokens
                        .merge(new_assigned_tokens, &params)
                        .map_err(|err| {
                            tracing::error!("{err}");
                            ContractError::InvalidUpdate
                        })?;
                }
                UpdateData::Delta(d) => {
                    let new_assigned_token = TokenAssignment::try_from(d)?;
                    assigned_tokens
                        .append(new_assigned_token, &params)
                        .map_err(|err| {
                            tracing::error!("{err}");
                            ContractError::InvalidUpdate
                        })?;
                }
                UpdateData::StateAndDelta { state, delta } => {
                    let new_assigned_tokens = TokenAllocationRecord::try_from(state)?;
                    assigned_tokens
                        .merge(new_assigned_tokens, &params)
                        .map_err(|err| {
                            tracing::error!("{err}");
                            ContractError::InvalidUpdate
                        })?;
                    let new_assigned_token = TokenAssignment::try_from(delta)?;
                    assigned_tokens
                        .append(new_assigned_token, &params)
                        .map_err(|err| {
                            tracing::error!("{err}");
                            ContractError::InvalidUpdate
                        })?;
                }
                _ => unreachable!(),
            }
        }
        let update = assigned_tokens.try_into()?;
        Ok(UpdateModification::valid(update))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let assigned_tokens = TokenAllocationRecord::try_from(state)?;
        let summary = assigned_tokens.summarize();
        summary.try_into()
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let assigned_tokens = TokenAllocationRecord::try_from(state)?;
        let summary = TokenAllocationSummary::try_from(summary)?;
        let delta = assigned_tokens.delta(&summary);
        delta.try_into()
    }
}

trait TokenAssignmentExt {
    fn is_valid(&self, params: &TokenParameters) -> bool;
}

impl TokenAssignmentExt for TokenAssignment {
    fn is_valid(&self, params: &TokenParameters) -> bool {
        if !self.tier.is_valid_slot(self.time_slot) {
            return false;
        }
        let msg = TokenAssignment::to_be_signed(&self.time_slot, &self.assignee, self.tier);
        if params
            .generator_public_key
            .verify(&msg, &self.signature)
            .is_err()
        {
            // not signed by the private key of this generator
            return false;
        }
        true
    }
}

trait TokenAllocationRecordExt {
    fn merge(&mut self, other: Self, params: &TokenParameters) -> Result<(), AllocationError>;
    fn append(
        &mut self,
        assignment: TokenAssignment,
        params: &TokenParameters,
    ) -> Result<(), AllocationError>;
}

impl TokenAllocationRecordExt for TokenAllocationRecord {
    fn merge(&mut self, other: Self, params: &TokenParameters) -> Result<(), AllocationError> {
        for (_, assignments) in other.into_iter() {
            for assignment in assignments {
                self.append(assignment, params)?;
            }
        }
        Ok(())
    }

    fn append(
        &mut self,
        assignment: TokenAssignment,
        params: &TokenParameters,
    ) -> Result<(), AllocationError> {
        match self.get_mut_tier(&assignment.tier) {
            Some(list) => {
                if list.binary_search(&assignment).is_err() {
                    if assignment.is_valid(params) {
                        list.push(assignment);
                        list.sort_unstable();
                        Ok(())
                    } else {
                        Err(AllocationError::invalid_assignment(assignment))
                    }
                } else {
                    Err(AllocationError::allocated_slot(&assignment))
                }
            }
            None => {
                self.insert(assignment.tier, vec![assignment]);
                Ok(())
            }
        }
    }
}
