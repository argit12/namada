use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;

use borsh_ext::BorshSerializeExt;
use namada::types::chain::ChainId;
use namada::types::key::*;
use namada::types::storage::BlockHeight;
use namada::types::time::DateTimeUtc;
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::cli::namada_version;
use crate::config;
use crate::facade::tendermint::node::Id as TendermintNodeId;
use crate::facade::tendermint::{block, Genesis, Moniker};
use crate::facade::tendermint_config::{
    Error as TendermintError, TendermintConfig,
};

/// Env. var to output Tendermint log to stdout
pub const ENV_VAR_TM_STDOUT: &str = "NAMADA_CMT_STDOUT";

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to initialize CometBFT: {0}")]
    Init(std::io::Error),
    #[error("Failed to load CometBFT config file: {0}")]
    LoadConfig(TendermintError),
    #[error("Failed to open CometBFT config for writing: {0}")]
    OpenWriteConfig(std::io::Error),
    #[error("Failed to serialize CometBFT config TOML to string: {0}")]
    ConfigSerializeToml(toml::ser::Error),
    #[error("Failed to write CometBFT config: {0}")]
    WriteConfig(std::io::Error),
    #[error("Failed to start up CometBFT node: {0}")]
    StartUp(std::io::Error),
    #[error("{0}")]
    Runtime(String),
    #[error("Failed to rollback CometBFT state: {0}")]
    RollBack(String),
    #[error("Failed to convert to String: {0:?}")]
    TendermintPath(std::ffi::OsString),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Check if the COMET env var has been set and use that as the
/// location of the COMET binary. Otherwise, assume it is on path
///
/// Returns an error if the env var is defined but not a valid Unicode.
fn from_env_or_default() -> Result<String> {
    match std::env::var("COMETBFT") {
        Ok(path) => {
            tracing::info!("Using CometBFT path from env variable: {}", path);
            Ok(path)
        }
        Err(std::env::VarError::NotPresent) => Ok(String::from("cometbft")),
        Err(std::env::VarError::NotUnicode(msg)) => {
            Err(Error::TendermintPath(msg))
        }
    }
}

/// Run the tendermint node.
pub async fn run(
    home_dir: PathBuf,
    chain_id: ChainId,
    genesis_time: DateTimeUtc,
    proxy_app_address: String,
    config: config::Ledger,
    abort_recv: tokio::sync::oneshot::Receiver<
        tokio::sync::oneshot::Sender<()>,
    >,
) -> Result<()> {
    let home_dir_string = home_dir.to_string_lossy().to_string();
    let tendermint_path = from_env_or_default()?;
    let mode = config.shell.tendermint_mode.to_str().to_owned();

    // init and run a tendermint node child process
    let output = Command::new(&tendermint_path)
        .args(["init", &mode, "--home", &home_dir_string])
        .output()
        .await
        .map_err(Error::Init)?;
    if !output.status.success() {
        panic!("Tendermint failed to initialize with {:#?}", output);
    }

    write_tm_genesis(&home_dir, chain_id, genesis_time).await;

    update_tendermint_config(&home_dir, config.cometbft).await?;

    let mut tendermint_node = Command::new(&tendermint_path);
    tendermint_node.args([
        "start",
        "--proxy_app",
        &proxy_app_address,
        "--home",
        &home_dir_string,
    ]);

    let log_stdout = match env::var(ENV_VAR_TM_STDOUT) {
        Ok(val) => val.to_ascii_lowercase().trim() == "true",
        _ => false,
    };
    if !log_stdout {
        tendermint_node.stdout(Stdio::null());
    }

    let mut tendermint_node = tendermint_node
        .kill_on_drop(true)
        .spawn()
        .map_err(Error::StartUp)?;
    tracing::info!("CometBFT node started");

    tokio::select! {
        status = tendermint_node.wait() => {
            match status {
                Ok(status) => {
                    if status.success() {
                        Ok(())
                    } else {
                        Err(Error::Runtime(status.to_string()))
                    }
                },
                Err(err) => {
                    Err(Error::Runtime(err.to_string()))
                }
            }
        },
        resp_sender = abort_recv => {
            match resp_sender {
                Ok(resp_sender) => {
                    tracing::info!("Shutting down Tendermint node...");
                    tendermint_node.kill().await.unwrap();
                    resp_sender.send(()).unwrap();
                },
                Err(err) => {
                    tracing::error!("The Tendermint abort sender has unexpectedly dropped: {}", err);
                    tracing::info!("Shutting down Tendermint node...");
                    tendermint_node.kill().await.unwrap();
                }
            }
            Ok(())
        }
    }
}

pub fn reset(tendermint_dir: impl AsRef<Path>) -> Result<()> {
    let tendermint_path = from_env_or_default()?;
    let tendermint_dir = tendermint_dir.as_ref().to_string_lossy();
    // reset all the Tendermint state, if any
    std::process::Command::new(tendermint_path)
        .args([
            "reset-state",
            "unsafe-all",
            // NOTE: log config: https://docs.tendermint.com/master/nodes/logging.html#configuring-log-levels
            // "--log-level=\"*debug\"",
            "--home",
            &tendermint_dir,
        ])
        .output()
        .expect("Failed to reset tendermint node's data");
    std::fs::remove_dir_all(format!("{}/config", tendermint_dir,))
        .expect("Failed to reset tendermint node's config");
    Ok(())
}

pub fn rollback(tendermint_dir: impl AsRef<Path>) -> Result<BlockHeight> {
    let tendermint_path = from_env_or_default()?;
    let tendermint_dir = tendermint_dir.as_ref().to_string_lossy();

    // Rollback tendermint state, see https://github.com/tendermint/tendermint/blob/main/cmd/tendermint/commands/rollback.go for details
    // on how the tendermint rollback behaves
    let output = std::process::Command::new(tendermint_path)
        .args([
            "rollback",
            "unsafe-all",
            // NOTE: log config: https://docs.tendermint.com/master/nodes/logging.html#configuring-log-levels
            // "--log-level=\"*debug\"",
            "--home",
            &tendermint_dir,
        ])
        .output()
        .map_err(|e| Error::RollBack(e.to_string()))?;

    // Capture the block height from the output of tendermint rollback
    // Tendermint stdout message: "Rolled
    // back state to height %d and hash %v"
    let output_msg = String::from_utf8(output.stdout)
        .map_err(|e| Error::RollBack(e.to_string()))?;
    let (_, right) = output_msg
        .split_once("Rolled back state to height")
        .ok_or(Error::RollBack(
            "Missing expected block height in tendermint stdout message"
                .to_string(),
        ))?;

    let mut sub = right.split_ascii_whitespace();
    let height = sub.next().ok_or(Error::RollBack(
        "Missing expected block height in tendermint stdout message"
            .to_string(),
    ))?;

    Ok(height
        .parse::<u64>()
        .map_err(|e| Error::RollBack(e.to_string()))?
        .into())
}

/// Convert a common signing scheme validator key into JSON for
/// Tendermint
fn validator_key_to_json(
    sk: &common::SecretKey,
) -> std::result::Result<serde_json::Value, ParseSecretKeyError> {
    let raw_hash = tm_consensus_key_raw_hash(&sk.ref_to());
    let (id_str, pk_arr, kp_arr) = match sk {
        common::SecretKey::Ed25519(_) => {
            let sk_ed: ed25519::SecretKey = sk.try_to_sk().unwrap();
            let keypair =
                [sk_ed.serialize_to_vec(), sk_ed.ref_to().serialize_to_vec()]
                    .concat();
            ("Ed25519", sk_ed.ref_to().serialize_to_vec(), keypair)
        }
        common::SecretKey::Secp256k1(_) => {
            let sk_sec: secp256k1::SecretKey = sk.try_to_sk().unwrap();
            (
                "Secp256k1",
                sk_sec.ref_to().serialize_to_vec(),
                sk_sec.serialize_to_vec(),
            )
        }
    };

    Ok(json!({
        "address": raw_hash,
        "pub_key": {
            "type": format!("tendermint/PubKey{}",id_str),
            "value": base64::encode(pk_arr),
        },
        "priv_key": {
            "type": format!("tendermint/PrivKey{}",id_str),
            "value": base64::encode(kp_arr),
        }
    }))
}

/// Initialize validator private key for Tendermint
pub async fn write_validator_key_async(
    home_dir: impl AsRef<Path>,
    consensus_key: &common::SecretKey,
) {
    let home_dir = home_dir.as_ref();
    let path = home_dir.join("config").join("priv_validator_key.json");
    // Make sure the dir exists
    let wallet_dir = path.parent().unwrap();
    fs::create_dir_all(wallet_dir)
        .await
        .expect("Couldn't create private validator key directory");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .expect("Couldn't create private validator key file");
    let key = validator_key_to_json(consensus_key).unwrap();
    let data = serde_json::to_vec_pretty(&key)
        .expect("Couldn't encode private validator key file");
    file.write_all(&data[..])
        .await
        .expect("Couldn't write private validator key file");
}

/// Initialize validator private key for Tendermint
pub fn write_validator_key(
    home_dir: impl AsRef<Path>,
    consensus_key: &common::SecretKey,
) {
    let home_dir = home_dir.as_ref();
    let path = home_dir.join("config").join("priv_validator_key.json");
    // Make sure the dir exists
    let wallet_dir = path.parent().unwrap();
    std::fs::create_dir_all(wallet_dir)
        .expect("Couldn't create private validator key directory");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("Couldn't create private validator key file");
    let key = validator_key_to_json(consensus_key).unwrap();
    serde_json::to_writer_pretty(file, &key)
        .expect("Couldn't write private validator key file");
}

/// Initialize validator private state for Tendermint
pub fn write_validator_state(home_dir: impl AsRef<Path>) {
    let home_dir = home_dir.as_ref();
    let path = home_dir.join("data").join("priv_validator_state.json");
    // Make sure the dir exists
    let wallet_dir = path.parent().unwrap();
    std::fs::create_dir_all(wallet_dir)
        .expect("Couldn't create private validator state directory");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("Couldn't create private validator state file");
    let state = json!({
       "height": "0",
       "round": 0,
       "step": 0
    });
    serde_json::to_writer_pretty(file, &state)
        .expect("Couldn't write private validator state file");
}

/// Length of a Tendermint Node ID in bytes
const TENDERMINT_NODE_ID_LENGTH: usize = 20;

/// Derive Tendermint node ID from public key
pub fn id_from_pk(pk: &common::PublicKey) -> TendermintNodeId {
    let mut bytes = [0u8; TENDERMINT_NODE_ID_LENGTH];

    match pk {
        common::PublicKey::Ed25519(_) => {
            let _pk: ed25519::PublicKey = pk.try_to_pk().unwrap();
            let digest = Sha256::digest(_pk.serialize_to_vec().as_slice());
            bytes.copy_from_slice(&digest[..TENDERMINT_NODE_ID_LENGTH]);
        }
        common::PublicKey::Secp256k1(_) => {
            let _pk: secp256k1::PublicKey = pk.try_to_pk().unwrap();
            let digest = Sha256::digest(_pk.serialize_to_vec().as_slice());
            bytes.copy_from_slice(&digest[..TENDERMINT_NODE_ID_LENGTH]);
        }
    }
    TendermintNodeId::new(bytes)
}

async fn update_tendermint_config(
    home_dir: impl AsRef<Path>,
    mut config: TendermintConfig,
) -> Result<()> {
    let home_dir = home_dir.as_ref();
    let path = home_dir.join("config").join("config.toml");

    config.moniker =
        Moniker::from_str(&format!("{}-{}", config.moniker, namada_version()))
            .expect("Invalid moniker");

    config.consensus.create_empty_blocks = true;

    // mempool config
    // https://forum.cosmos.network/t/our-understanding-of-the-cosmos-hub-mempool-issues/12040
    {
        // We set this to true as we don't want any invalid tx be re-applied.
        // This also implies that it's not possible for an invalid tx to
        // become valid again in the future.
        config.mempool.keep_invalid_txs_in_cache = false;

        // Drop txs from the mempool that are larger than 1 MiB
        //
        // The application (Namada) can assign arbitrary max tx sizes,
        // which are subject to consensus. Either way, nodes are able to
        // configure their local mempool however they please.
        //
        // 1 MiB is a reasonable value that allows governance proposal txs
        // containing wasm code to be proposed by a leading validator
        // during some round's start
        config.mempool.max_tx_bytes = 1024 * 1024;

        // Hold 50x the max amount of txs in a block
        //
        // 6 MiB is the default Namada max proposal size governance
        // parameter -> 50 * 6 MiB
        config.mempool.max_txs_bytes = 50 * 6 * 1024 * 1024;

        // Hold up to 4k txs in the mempool
        config.mempool.size = 4000;
    }

    // Bumped from the default `1_000_000`, because some WASMs can be
    // quite large
    config.rpc.max_body_bytes = 2_000_000;

    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(Error::OpenWriteConfig)?;
    let config_str =
        toml::to_string(&config).map_err(Error::ConfigSerializeToml)?;
    file.write_all(config_str.as_bytes())
        .await
        .map_err(Error::WriteConfig)
}

async fn write_tm_genesis(
    home_dir: impl AsRef<Path>,
    chain_id: ChainId,
    genesis_time: DateTimeUtc,
) {
    let home_dir = home_dir.as_ref();
    let path = home_dir.join("config").join("genesis.json");
    let mut file = File::open(&path).await.unwrap_or_else(|err| {
        panic!(
            "Couldn't open the genesis file at {:?}, error: {}",
            path, err
        )
    });
    let mut file_contents = vec![];
    file.read_to_end(&mut file_contents)
        .await
        .expect("Couldn't read Tendermint genesis file");
    // Set `Option<String>` for the omitted `app_state`
    let mut genesis: Genesis<Option<String>> =
        serde_json::from_slice(&file_contents[..])
            .expect("Couldn't deserialize the genesis file");
    genesis.chain_id =
        FromStr::from_str(chain_id.as_str()).expect("Invalid chain ID");
    genesis.genesis_time = genesis_time
        .try_into()
        .expect("Couldn't convert DateTimeUtc to Tendermint Time");
    let size = block::Size {
        // maximum size of a serialized Tendermint block.
        // on Namada, we have a hard-cap of 16 MiB (6 MiB max
        // txs in a block + 10 MiB reserved for evidence data,
        // block headers and protobuf serialization overhead)
        max_bytes: 16 * 1024 * 1024,
        // gas is metered app-side, so we disable it
        // at the Tendermint level
        max_gas: -1,
        /// This parameter has no value anymore in Tendermint-core
        time_iota_ms: block::Size::default_time_iota_ms(),
    };
    genesis.consensus_params.block = size;

    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .unwrap_or_else(|err| {
            panic!(
                "Couldn't open the genesis file at {:?} for writing, error: {}",
                path, err
            )
        });
    let data = serde_json::to_vec_pretty(&genesis)
        .expect("Couldn't encode the CometBFT genesis file");
    file.write_all(&data[..])
        .await
        .expect("Couldn't write the CometBFT genesis file");
}
