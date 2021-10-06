use rust_fsm::{StateMachine, StateMachineImpl};

use crate::{
    conn_manager::ConnectionBridge, contract::ContractKey, message::Transaction,
    node::OpStateStorage,
};

pub(crate) use self::messages::GetMsg;

use super::OpError;

/// This is just a placeholder for now!
pub(crate) struct GetOp(StateMachine<GetOpSM>);

impl GetOp {
    pub fn start_op(key: ContractKey) -> Self {
        let state = StateMachine::from_state(GetState::Requesting { key });
        GetOp(state)
    }
}

struct GetOpSM;

impl StateMachineImpl for GetOpSM {
    type Input = GetMsg;

    type State = GetState;

    type Output = GetMsg;

    const INITIAL_STATE: Self::State = GetState::Initializing;

    fn transition(state: &Self::State, inget: &Self::Input) -> Option<Self::State> {
        match (state, inget) {
            (GetState::Initializing, GetMsg::FetchRouting { key }) => {
                Some(GetState::Requesting { key: *key })
            }
            _ => None,
        }
    }

    fn output(state: &Self::State, inget: &Self::Input) -> Option<Self::Output> {
        match (state, inget) {
            (GetState::Initializing, GetMsg::FetchRouting { key }) => {
                Some(GetMsg::FetchRouting { key: *key })
            }
            _ => None,
        }
    }
}

enum GetState {
    Initializing,
    Requesting { key: ContractKey },
}

pub(crate) async fn handle_get_response<CB>(
    op_storage: &OpStateStorage,
    conn_manager: &mut CB,
    get_op: GetMsg,
) -> Result<(), OpError>
where
    CB: ConnectionBridge,
{
    Ok(())
}

/// Request to get the current value from a contract.
pub(crate) async fn request_get(op_storage: &OpStateStorage, get_op: GetOp) -> Result<(), OpError> {
    // the initial request must provide:
    // - a location in the network where the contract resides
    // - and the value to get
    todo!()
}

mod messages {
    use crate::{conn_manager::PeerKeyLocation, contract::ContractKey};

    use super::*;

    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
    pub(crate) enum GetMsg {
        FetchRouting { key: ContractKey },
    }

    impl GetMsg {
        pub fn id(&self) -> &Transaction {
            todo!()
        }

        pub fn sender(&self) -> Option<&PeerKeyLocation> {
            todo!()
        }
    }
}
