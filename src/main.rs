use std::{collections::{HashMap, HashSet}, net::SocketAddr, ops::{ControlFlow, Div, Mul}, path::Path, str::FromStr, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};
use colored::Colorize;

use axum::{extract::{ws::{Message, WebSocket}, ConnectInfo, Query, State, WebSocketUpgrade}, http::StatusCode, response::IntoResponse, routing::get, Extension, Router};
use axum_extra::{headers::authorization::Basic, TypedHeader};
use clap::Parser;
use drillx::Solution;
use futures::{stream::SplitSink, SinkExt, StreamExt};
use ore_api::{consts::BUS_COUNT, state::Proof};
use ore_utils::{get_auth_ix, get_cutoff, get_mine_ix, get_proof, get_register_ix, ORE_TOKEN_DECIMALS};
use rand::Rng;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use rand::seq::SliceRandom;
use solana_client::client_error::reqwest;
use solana_client::client_error::reqwest::header::{CONTENT_TYPE, HeaderMap};
use solana_sdk::{commitment_config::CommitmentConfig, compute_budget::ComputeBudgetInstruction, native_token::LAMPORTS_PER_SOL, pubkey, pubkey::Pubkey, signature::{read_keypair_file, Signature}, signer::Signer, transaction::Transaction};
use tokio::{io::AsyncReadExt, sync::{mpsc::{UnboundedReceiver, UnboundedSender}, Mutex, RwLock}, time};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use solana_transaction_status::{Encodable, EncodedTransaction, TransactionConfirmationStatus, UiTransactionEncoding};

use solana_program::{
    instruction::Instruction,
    native_token::{lamports_to_sol, sol_to_lamports},
    system_instruction::transfer,
};

use serde_json::{json, Value};
use eyre::{Report, Result};

const MIN_DIFF: u32 = 8;
const MIN_HASHPOWER: u64 = 5;


struct AppState {
    sockets: HashMap<SocketAddr, Mutex<SplitSink<WebSocket, Message>>>,
}

pub struct MessageInternalMineSuccess {
    difficulty: u32,
    total_balance: f64,
    rewards: f64,
}

#[derive(Debug)]
pub enum ClientMessage {
    Ready(SocketAddr),
    Mining(SocketAddr),
    BestSolution(SocketAddr, Solution),
}

pub struct BestHash {
    solution: Option<Solution>,
    difficulty: u32,
}

pub struct Config {
    password: String,
}

mod ore_utils;

pub const JITO_RECIPIENTS: [Pubkey; 8] = [
    // mainnet
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

pub const JITO_ENDPOINTS: [&str; 4] = [
    "https://mainnet.block-engine.jito.wtf",
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://tokyo.mainnet.block-engine.jito.wtf",
];


// jito submit
#[derive(Deserialize, Serialize, Debug)]
struct BundleSendResponse {
    id: u64,
    jsonrpc: String,
    result: String,
}

// jito query

#[derive(Serialize, Deserialize, Debug)]
struct BundleStatusResponse {
    jsonrpc: String,
    result: BundleStatusResult,
    id: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct BundleStatusResult {
    context: Context,
    value: Vec<BundleStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Context {
    slot: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct BundleStatus {
    bundle_id: String,
    transactions: Vec<String>,
    slot: u64,
    confirmation_status: String,
    err: Option<serde_json::Value>,
}


async fn get_bundle_statuses(params: Value) -> Result<BundleStatusResponse> {
    // 在 async 块外生成随机数
    let endpoint = JITO_ENDPOINTS[rand::thread_rng().gen_range(0..JITO_ENDPOINTS.len())].to_string() + "/api/v1/bundles";

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))  // 设置超时为3秒
        .build()?;

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());

    let response = client
        .post(endpoint)  // 使用预先生成的随机 endpoint
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": params
        }))
        .send()
        .await
        .map_err(|err| eyre::eyre!("Failed to send request: {err}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| eyre::eyre!("Failed to read response content: {err:#}"))?;

    if !status.is_success() {
        eyre::bail!("Status code: {status}, response: {text}");
    }

    serde_json::from_str(&text)
        .map_err(|err| eyre::eyre!("Failed to deserialize response: {err:#}, response: {text}, status: {status}"))
}

pub fn build_bribe_ix(pubkey: &Pubkey, value: u64) -> solana_sdk::instruction::Instruction {
    solana_sdk::system_instruction::transfer(pubkey, &JITO_RECIPIENTS[rand::thread_rng().gen_range(0..JITO_RECIPIENTS.len())], value)
}

async fn send_jito_bundle(params: Value) -> Result<BundleSendResponse> {
    // 在 async 块外生成随机数
    let endpoint = JITO_ENDPOINTS[rand::thread_rng().gen_range(0..JITO_ENDPOINTS.len())].to_string() + "/api/v1/bundles";
    
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))  // 设置超时为5秒
        .build()?;

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());

    let response = client
        .post(endpoint)  // 使用预先生成的随机 endpoint
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": params
        }))
        .send()
        .await
        .map_err(|err| eyre::eyre!("Failed to send request: {err}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| eyre::eyre!("Failed to read response content: {err:#}"))?;

    if !status.is_success() {
        eyre::bail!("Status code: {status}, response: {text}");
    }

    serde_json::from_str(&text)
        .map_err(|err| eyre::eyre!("Failed to deserialize response: {err:#}, response: {text}, status: {status}"))
}

//高难度动态增加tip
pub fn adjust_fee(difficulty: u32, jito_tip_lamports: u64) -> u64 {
    let mut extra_fee = jito_tip_lamports.clone();

    if difficulty > 25 {
        // 对于 difficulty > 25 的特殊处理
        extra_fee += ((extra_fee as f64 * (difficulty as f64 - 20f64) / 20f64) as u64) * 6;
        // 约束下上限
        if extra_fee > 5 * jito_tip_lamports {
            extra_fee = 5 * jito_tip_lamports;
        }
        info!("TIP增加到 {}", extra_fee);
    }
        else if difficulty > 20 {
            extra_fee += (extra_fee as f64 * (difficulty as f64 - 20f64) / 20f64) as u64 * 4;
            // 约束下上限
            if extra_fee > 3 * jito_tip_lamports {
                extra_fee = 3 * jito_tip_lamports
            }
            info!("TIP增加到 {}", extra_fee);
        }
    return extra_fee;
}

#[derive(Parser, Debug)]
#[command(version, author, about, long_about = None)]
struct Args {
    #[arg(
        long,
        value_name = "priority fee",
        help = "Number of microlamports to pay as priority fee per transaction",
        default_value = "0",
        global = true
    )]
    priority_fee: u64,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();
    let args = Args::parse();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ore_hq_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // load envs
    let wallet_path_str = std::env::var("WALLET_PATH").expect("WALLET_PATH must be set.");
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set.");
    let password = std::env::var("PASSWORD").expect("PASSWORD must be set.");

    let priority_fee = Arc::new(Mutex::new(args.priority_fee));
    // load wallet
    let wallet_path = Path::new(&wallet_path_str);

    if !wallet_path.exists() {
        tracing::error!("Failed to load wallet at: {}", wallet_path_str);
        return Err("Failed to find wallet path.".into());
    }

    let wallet = read_keypair_file(wallet_path).expect("Failed to load keypair from file: {wallet_path_str}");
    println!("loaded wallet {}", wallet.pubkey().to_string());

    println!("establishing rpc connection...");
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    println!("loading sol balance...");
    let balance = if let Ok(balance) = rpc_client.get_balance(&wallet.pubkey()).await {
        balance
    } else {
        return Err("Failed to load balance".into());
    };

    println!("Balance: {:.2}", balance as f64 / LAMPORTS_PER_SOL as f64);

    if balance < 1_000_000 {
        return Err("Sol balance is too low!".into());
    }

    let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
        loaded_proof
    } else {
        println!("Failed to load proof.");
        println!("Creating proof account...");

        let ix = get_register_ix(wallet.pubkey());

        if let Ok((hash, _slot)) = rpc_client
            .get_latest_blockhash_with_commitment(rpc_client.commitment()).await {
            let mut tx = Transaction::new_with_payer(&[ix], Some(&wallet.pubkey()));

            tx.sign(&[&wallet], hash);

            let result = rpc_client
                .send_and_confirm_transaction_with_spinner_and_commitment(
                    &tx, rpc_client.commitment(),
                ).await;

            if let Ok(sig) = result {
                println!("Sig: {}", sig.to_string());
            } else {
                return Err("Failed to create proof account".into());
            }
        }
        let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
            loaded_proof
        } else {
            return Err("Failed to get newly created proof".into());
        };
        proof
    };

    let config = Arc::new(Mutex::new(Config {
        password,
    }));

    let best_hash = Arc::new(Mutex::new(BestHash {
        solution: None,
        difficulty: 0,
    }));

    let wallet_extension = Arc::new(wallet);
    let proof_ext = Arc::new(Mutex::new(proof));
    let nonce_ext = Arc::new(Mutex::new(0u64));

    let shared_state = Arc::new(RwLock::new(AppState {
        sockets: HashMap::new(),
    }));
    let ready_clients = Arc::new(Mutex::new(HashSet::new()));

    let (client_message_sender, client_message_receiver) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    // Handle client messages
    let app_shared_state = shared_state.clone();
    let app_ready_clients = ready_clients.clone();
    let app_proof = proof_ext.clone();
    let app_best_hash = best_hash.clone();
    tokio::spawn(async move {
        client_message_handler_system(client_message_receiver, &app_shared_state, app_ready_clients, app_proof, app_best_hash).await;
    });

    // Handle ready clients
    let app_shared_state = shared_state.clone();
    let app_proof = proof_ext.clone();
    let app_best_hash = best_hash.clone();
    let app_nonce = nonce_ext.clone();
    tokio::spawn(async move {
        loop {
            let mut clients = Vec::new();
            {
                let ready_clients_lock = ready_clients.lock().await;
                for ready_client in ready_clients_lock.iter() {
                    clients.push(ready_client.clone());
                }
            };

            let proof = {
                app_proof.lock().await.clone()
            };

            let cutoff = get_cutoff(proof, 5);
            let mut should_mine = true;
            let cutoff = if cutoff <= 0 {
                let solution = {
                    app_best_hash.lock().await.solution
                };
                if solution.is_some() {
                    should_mine = false;
                }
                0
            } else {
                cutoff
            };

            if should_mine {
                let challenge = proof.challenge;

                for client in clients {
                    let nonce_range = {
                        let mut nonce = app_nonce.lock().await;
                        let start = *nonce;
                        // max hashes possible in 60s for a single client
                        *nonce += 2_000_000;
                        let end = *nonce;
                        start..end
                    };
                    {
                        let shared_state = app_shared_state.read().await;
                        // message type is 8 bytes = 1 u8
                        // challenge is 256 bytes = 32 u8
                        // cutoff is 64 bytes = 8 u8
                        // nonce_range is 128 bytes, start is 64 bytes, end is 64 bytes = 16 u8
                        let mut bin_data = [0; 57];
                        bin_data[00..1].copy_from_slice(&0u8.to_le_bytes());
                        bin_data[01..33].copy_from_slice(&challenge);
                        bin_data[33..41].copy_from_slice(&cutoff.to_le_bytes());
                        bin_data[41..49].copy_from_slice(&nonce_range.start.to_le_bytes());
                        bin_data[49..57].copy_from_slice(&nonce_range.end.to_le_bytes());


                        if let Some(sender) = shared_state.sockets.get(&client) {
                            let _ = sender.lock().await.send(Message::Binary(bin_data.to_vec())).await;
                            let _ = ready_clients.lock().await.remove(&client);
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let (mine_success_sender, mut mine_success_receiver) = tokio::sync::mpsc::unbounded_channel::<MessageInternalMineSuccess>();

    let rpc_client = Arc::new(rpc_client);
    let app_proof = proof_ext.clone();
    let app_best_hash = best_hash.clone();
    let app_wallet = wallet_extension.clone();
    let app_nonce = nonce_ext.clone();
    let app_prio_fee = Arc::new(Mutex::new(0)); 
    let prio_fee = *app_prio_fee.lock().await; 
    let jito_tip_lamports = *priority_fee.lock().await;
    let jito_tip_sol = (jito_tip_lamports as f64) / 1_000_000_000.0; 


    tokio::spawn(async move {
        loop {
            let proof = {
                app_proof.lock().await.clone()
            };

            let cutoff = get_cutoff(proof, 0);
            if cutoff <= 0 {
                // process solutions
                let solution = {
                    app_best_hash.lock().await.solution.clone()
                };
                if let Some(solution) = solution {
                    let signer = app_wallet.clone();
                    let mut ixs = vec![];
                    // TODO: set cu's
                    let prio_fee = {
                        app_prio_fee.lock().await.clone()
                    };

                    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(480000);
                    ixs.push(cu_limit_ix);

                    let prio_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(prio_fee);
                    ixs.push(prio_fee_ix);

                    let noop_ix = get_auth_ix(signer.pubkey());
                    ixs.push(noop_ix);

                    // TODO: choose the highest balance bus
                    let bus = rand::thread_rng().gen_range(0..BUS_COUNT);
                    let difficulty = solution.to_hash().difficulty();

                    // 添加Jito tip转账指令,难度>20增加小费
                    ixs.push(build_bribe_ix(&app_wallet.pubkey(), adjust_fee(difficulty, jito_tip_lamports)));
                    if difficulty < 2 {
                        println!("Jito tip: {} SOL", jito_tip_sol);
                    }
                    

                    let ix_mine = get_mine_ix(signer.pubkey(), solution, bus);
                    ixs.push(ix_mine);
                    info!("Starting mine submission attempts with difficulty {}.", difficulty);
                    if let Ok((hash, _slot)) = rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment()).await {
                        let mut tx = Transaction::new_with_payer(&ixs, Some(&signer.pubkey()));

                        tx.sign(&[&signer], hash);

                        let mut bundle_confirm = false;
                        for i in 0..3 {
                            info!("Sending signed tx...");
                            info!("attempt: {}", i + 1);

                            let mut bundle = Vec::with_capacity(5);
                            bundle.push(tx.clone());

                            let _ = *bundle
                                .first()
                                .expect("empty bundle")
                                .signatures
                                .first()
                                .expect("empty transaction");

                            let bundle = bundle
                                .into_iter()
                                .map(|tx| match tx.encode(UiTransactionEncoding::Binary) {
                                    EncodedTransaction::LegacyBinary(b) => b,
                                    _ => panic!("encode-jito-tx-err"),
                                })
                                .collect::<Vec<_>>();

                            println!("Submitting jito transaction... (attempt {})", i);
                            let mut bundle_id = String::new();
                            match send_jito_bundle(json!([bundle])).await {
                                Ok(resp) => {
                                    bundle_id = resp.result.clone();
                                    println!("Sent bundle resp: {:?}", bundle_id);
                                }
                                // Handle submit errors
                                Err(err) => {
                                    println!("Sent bundle err: {:?}", err);
                                    time::sleep(Duration::from_secs(3)).await;
                                    continue;
                                }
                            }

                            let mut attempts = 0;
                            loop {
                                println!("query bundle bundle_id: {}, attempts: {}", bundle_id, attempts);
                                // Retry
                                let param = bundle_id.clone();
                                match get_bundle_statuses(json!([[param]])).await {
                                    Ok(resp) => {
                                        if resp.result.value.len() > 0 && resp.result.value[0].confirmation_status == "confirmed" {
                                            println!(" Bundle landed successfully");
                                            println!(" https://solscan.io/tx/{}?cluster={}", resp.result.value[0].transactions[0], "mainnet");
                                            bundle_confirm = true;
                                            break;
                                        }
                                    }
                                    // Handle submit errors
                                    Err(err) => {
                                        println!("{}", err);
                                    }
                                }
                                attempts += 1;
                                
                                if attempts > 20 {
                                    println!("{}: Max retries", "ERROR".bold().red());
                                    if i == 2 {
                                        // 达到最大后重新分配避免卡死
                                        if let Ok(loaded_proof) = get_proof(&rpc_client, signer.pubkey()).await {
                                            let mut mut_proof = app_proof.lock().await;
                                            *mut_proof = loaded_proof;
                                            let mut nonce = app_nonce.lock().await;
                                            *nonce = 0;
                                            let mut mut_best_hash = app_best_hash.lock().await;
                                            mut_best_hash.solution = None;
                                            mut_best_hash.difficulty = 0;
                                        
                                        }
                                        break;
                                    }
                                    break;
                                }

                                time::sleep(Duration::from_secs(1)).await;
                            }
                            
                            if bundle_confirm {
                                // 成功上链，执行状态更新和任务分发
                                loop {
                                    if let Ok(loaded_proof) = get_proof(&rpc_client, signer.pubkey()).await {
                                        if proof != loaded_proof {
                                            info!("Got new proof.");
                                            let balance = (loaded_proof.balance as f64) / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                            info!("New balance: {}", balance);
                                            let rewards = loaded_proof.balance - proof.balance;
                                            let rewards = (rewards as f64) / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                            info!("Earned: {} ORE", rewards);

                                            let _ = mine_success_sender.send(MessageInternalMineSuccess {
                                                difficulty,
                                                total_balance: balance,
                                                rewards,
                                            });
    
                                            {
                                                let mut mut_proof = app_proof.lock().await;
                                                *mut_proof = loaded_proof;
                                                let mut nonce = app_nonce.lock().await;
                                                *nonce = 0;
                                                let mut mut_best_hash = app_best_hash.lock().await;
                                                mut_best_hash.solution = None;
                                                mut_best_hash.difficulty = 0;
                                            }
                                            break;
                                        }
                                    } else {
                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                    }
                                }
                                break;
                            }

                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        
                    
                    } else {
                        error!("Failed to get latest blockhash. retrying...");
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_secs(cutoff as u64)).await;
            };
        }
    });


    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        loop {
            while let Some(msg) = mine_success_receiver.recv().await {
                let message = format!(
                    "Submitted Difficulty: {}\nEarned: {} ORE.\nTotal Balance: {}\n",
                    msg.difficulty,
                    msg.rewards,
                    msg.total_balance
                );
                {
                    let shared_state = app_shared_state.read().await;
                    for (_socket_addr, socket_sender) in shared_state.sockets.iter() {
                        if let Ok(_) = socket_sender.lock().await.send(Message::Text(message.clone())).await {} else {
                            println!("Failed to send client text");
                        }
                    }
                }
            }
        }
    });

    let client_channel = client_message_sender.clone();
    let app_shared_state = shared_state.clone();
    let app = Router::new()
        .route("/", get(ws_handler))
        .with_state(app_shared_state)
        .layer(Extension(config))
        .layer(Extension(wallet_extension))
        .layer(Extension(client_channel))
        // Logging
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true))
        );


    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .unwrap();

    tracing::debug!("listening on {}", listener.local_addr().unwrap());

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        ping_check_system(&app_shared_state).await;
    });

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    ).await
        .unwrap();

    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    TypedHeader(auth_header): TypedHeader<axum_extra::headers::Authorization<Basic>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<RwLock<AppState>>>,
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>,
    Extension(config): Extension<Arc<Mutex<Config>>>,
) -> impl IntoResponse {
    let password = auth_header.password();
    if config.lock().await.password.ne(password) {
        error!("Auth failed..");
        return Err((StatusCode::UNAUTHORIZED, "Invalid credentials"));
    }

    println!("Client: {addr} connected.");


    Ok(ws.on_upgrade(move |socket| handle_socket(socket, addr, app_state, client_channel)))
}

async fn handle_socket(mut socket: WebSocket, who: SocketAddr, app_state: Arc<RwLock<AppState>>, client_channel: UnboundedSender<ClientMessage>) {
    if socket.send(axum::extract::ws::Message::Ping(vec![1, 2, 3])).await.is_ok() {
        println!("Pinged {who}...");
    } else {
        println!("could not ping {who}");

        // if we can't ping we can't do anything, return to close the connection
        return;
    }

    let (sender, mut receiver) = socket.split();
    let mut app_state = app_state.write().await;
    if app_state.sockets.contains_key(&who) {
        println!("Socket addr: {who} already has an active connection");
        // TODO: Close Connection here?
    } else {
        app_state.sockets.insert(who, Mutex::new(sender));
    }
    drop(app_state);

    let _ = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if process_message(msg, who, client_channel.clone()).is_break() {
                break;
            }
        }
    }).await;

    println!("Client: {who} disconnected!");
}

fn process_message(msg: Message, who: SocketAddr, client_channel: UnboundedSender<ClientMessage>) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!(">>> {who} sent str: {t:?}");
        }
        Message::Binary(d) => {
            // first 8 bytes are message type
            let message_type = d[0];
            match message_type {
                0 => {
                    let msg = ClientMessage::Ready(who);
                    let _ = client_channel.send(msg);
                }
                1 => {
                    let msg = ClientMessage::Mining(who);
                    let _ = client_channel.send(msg);
                }
                2 => {
                    // parse solution from message data
                    let mut solution_bytes = [0u8; 16];
                    // extract (16 u8's) from data for hash digest
                    let mut b_index = 1;
                    for i in 0..16 {
                        solution_bytes[i] = d[i + b_index];
                    }
                    b_index += 16;

                    // extract 64 bytes (8 u8's)
                    let mut nonce = [0u8; 8];
                    for i in 0..8 {
                        nonce[i] = d[i + b_index];
                    }

                    let solution = Solution::new(solution_bytes, nonce);

                    let msg = ClientMessage::BestSolution(who, solution);
                    let _ = client_channel.send(msg);
                }
                _ => {
                    println!(">>> {} sent an invalid message", who);
                }
            }
        }
        Message::Close(c) => {
            if let Some(cf) = c {
                println!(
                    ">>> {} sent close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                println!(">>> {who} somehow sent close message without CloseFrame");
            }
            return ControlFlow::Break(());
        }
        Message::Pong(v) => {
            //println!(">>> {who} sent pong with {v:?}");
        }
        Message::Ping(v) => {
            //println!(">>> {who} sent ping with {v:?}");
        }
    }

    ControlFlow::Continue(())
}

async fn client_message_handler_system(
    mut receiver_channel: UnboundedReceiver<ClientMessage>,
    shared_state: &Arc<RwLock<AppState>>,
    ready_clients: Arc<Mutex<HashSet<SocketAddr>>>,
    proof: Arc<Mutex<Proof>>,
    best_hash: Arc<Mutex<BestHash>>,
) {
    while let Some(client_message) = receiver_channel.recv().await {
        match client_message {
            ClientMessage::Ready(addr) => {
                println!("Client {} is ready!", addr.to_string());
                {
                    let shared_state = shared_state.read().await;
                    if let Some(sender) = shared_state.sockets.get(&addr) {
                        {
                            let mut ready_clients = ready_clients.lock().await;
                            ready_clients.insert(addr);
                        }

                        if let Ok(_) = sender.lock().await.send(Message::Text(String::from("Client successfully added."))).await {} else {
                            println!("Failed notify client they were readied up!");
                        }
                    }
                }
            }
            ClientMessage::Mining(addr) => {
                println!("Client {} has started mining!", addr.to_string());
            }
            ClientMessage::BestSolution(addr, solution) => {
                println!("Client {} found a solution.", addr);
                let challenge = {
                    let proof = proof.lock().await;
                    proof.challenge
                };

                if solution.is_valid(&challenge) {
                    let diff = solution.to_hash().difficulty();
                    println!("{} found diff: {}", addr, diff);
                    if diff >= MIN_DIFF {
                        {
                            let mut best_hash = best_hash.lock().await;
                            if diff > best_hash.difficulty {
                                best_hash.difficulty = diff;
                                best_hash.solution = Some(solution);
                            }
                        }

                        // calculate rewards
                        let hashpower = MIN_HASHPOWER * 2u64.pow(diff - MIN_DIFF);

                        println!("Client: {}, provided {} Hashpower", addr, hashpower);
                    } else {
                        println!("Diff to low, skipping");
                    }
                } else {
                    println!("{} returned an invalid solution!", addr);
                }
            }
        }
    }
}

async fn ping_check_system(
    shared_state: &Arc<RwLock<AppState>>,
) {
    loop {
        // send ping to all sockets
        let mut failed_sockets = Vec::new();
        let app_state = shared_state.read().await;
        // I don't like doing all this work while holding this lock...
        for (who, socket) in app_state.sockets.iter() {
            if socket.lock().await.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
                //println!("Pinged: {who}...");
            } else {
                failed_sockets.push(who.clone());
            }
        }
        drop(app_state);

        // remove any sockets where ping failed
        let mut app_state = shared_state.write().await;
        for address in failed_sockets {
            app_state.sockets.remove(&address);
        }
        drop(app_state);

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
