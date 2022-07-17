//! Serves a new contract so is available for browsing.

use std::{fs::File, io::{Cursor, Read}, path::PathBuf, sync::Arc};
use byteorder::{BigEndian, ReadBytesExt};

use locutus_node::{ErrorKind, libp2p::identity::ed25519::PublicKey, SqlitePool, WrappedState};
use locutus_runtime::{ContractCode, ContractKey, StateStore, WrappedContract};
use serde::Serialize;

const MAX_SIZE: i64 = 10 * 1024 * 1024;
const MAX_MEM_CACHE: u32 = 10_000_000;
const CRATE_DIR: &str = env!("CARGO_MANIFEST_DIR");

struct WebBundle {
    data_contract: WrappedContract<'static>,
    initial_state: WrappedState,
    web_contract: WrappedContract<'static>,
    web_content: WrappedState,
}

struct UnpackedState {
    pub metadata: Vec<u8>,
    pub state: WrappedState,
}

fn test_web(public_key: PublicKey) -> Result<WebBundle, std::io::Error> {

    fn get_data_contract(
        public_key: PublicKey,
    ) -> std::io::Result<(WrappedContract<'static>, WrappedState)> {
        let path = PathBuf::from(CRATE_DIR).join("examples/freenet_microblogging_data.wasm");
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;

        #[derive(Serialize)]
        struct Verification {
            public_key: Vec<u8>,
        }
        let params = serde_json::to_vec(&Verification {
            public_key: vec![],
        })
        .unwrap();
        let contract = WrappedContract::new(Arc::new(ContractCode::from(bytes)), params.into());

        let path = PathBuf::from(CRATE_DIR).join("examples/freenet_microblogging_data");
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;

        Ok((contract, bytes.into()))
    }

    fn get_web_contract() -> std::io::Result<(WrappedContract<'static>, WrappedState)> {
        let path = PathBuf::from(CRATE_DIR).join("examples/freenet_microblogging_web.wasm");
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;

        let contract =
            WrappedContract::new(Arc::new(ContractCode::from(bytes)), [].as_ref().into());

        let path = PathBuf::from(CRATE_DIR).join("examples/freenet_microblogging_web");
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;

        Ok((contract, bytes.into()))
    }

    let (data_contract, initial_state) = get_data_contract(public_key)?;
    let (web_contract, web_content) = get_web_contract()?;

    Ok(WebBundle {
        data_contract,
        initial_state,
        web_contract,
        web_content,
    })
}

#[cfg(feature = "local")]
async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use http_gw::HttpGateway;
    use locutus_dev::ContractStore;
    use locutus_node::libp2p::identity::ed25519::Keypair;

    let keypair = Keypair::generate();

    let bundle = test_web(keypair.public())?;
    log::info!(
        "loading web contract {} in local node",
        bundle.web_contract.key().encode()
    );
    log::info!(
        "loading data contract {} in local node",
        bundle.data_contract.key().encode()
    );

    let tmp_path = std::env::temp_dir().join("locutus");
    let contract_store = ContractStore::new(tmp_path.join("contracts"), MAX_SIZE);
    let state_store = StateStore::new(SqlitePool::new().await?, MAX_MEM_CACHE).unwrap();
    let mut local_node =
        locutus_dev::LocalNode::new(contract_store.clone(), state_store.clone()).await?;
    let id = HttpGateway::next_client_id();
    local_node
        .preload(id, bundle.data_contract, bundle.initial_state)
        .await;
    local_node
        .preload(id, bundle.web_contract, bundle.web_content)
        .await;
    http_gw::local_node::set_local_node(local_node).await
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(not(feature = "local"))]
    {
        panic!("only allowed if local feature is enabled");
    }
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
    // env_logger::Builder::from_default_env()
    //     .format_module_path(true)
    //     .filter_level(log::LevelFilter::Info)
    //     .init();

    #[allow(unused_variables)]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    #[cfg(feature = "local")]
    {
        rt.block_on(run())?;
    }

    Ok(())
}
