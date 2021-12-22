use anyhow::{anyhow, Context, Result};
use client::{Client, EthersClient, SolanaClient};
use futures::stream::{FuturesUnordered, StreamExt};
use log::{debug, error, info, warn};
use realtps_common::{all_chains, Block, Chain, Db, JsonDb};
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::task;
use tokio::task::JoinHandle;

mod client;
mod delay;
mod import;

#[derive(StructOpt, Debug)]
struct Opts {
    #[structopt(subcommand)]
    cmd: Option<Command>,
}

#[derive(StructOpt, Debug)]
enum Command {
    Run,
    Import,
    Calculate,
}

enum Job {
    Import(Chain),
    Calculate,
}

static RPC_CONFIG_PATH: &str = "rpc_config.toml";

#[derive(Deserialize, Serialize)]
struct RpcConfig {
    chains: HashMap<Chain, String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let opts = Opts::from_args();
    let cmd = opts.cmd.unwrap_or(Command::Run);

    let rpc_config = load_rpc_config(RPC_CONFIG_PATH)?;

    Ok(run(cmd, rpc_config).await?)
}

async fn run(cmd: Command, rpc_config: RpcConfig) -> Result<()> {
    let importer = make_importer(&rpc_config).await?;

    let mut jobs = FuturesUnordered::new();

    for job in init_jobs(cmd).into_iter() {
        jobs.push(importer.do_job(job));
    }

    loop {
        let job_result = jobs.next().await;
        if let Some(new_jobs) = job_result {
            for new_job in new_jobs {
                jobs.push(importer.do_job(new_job));
            }
        } else {
            error!("no more jobs?!");
            break;
        }
    }

    Ok(())
}

fn load_rpc_config<P: AsRef<Path>>(path: P) -> Result<RpcConfig> {
    let rpc_config_file = fs::read_to_string(path).context("unable to load RPC configuration")?;
    let rpc_config = toml::from_str::<RpcConfig>(&rpc_config_file)
        .context("unable to parse RPC configuration")?;

    Ok(rpc_config)
}

fn print_error(e: &anyhow::Error) {
    error!("error: {}", e);
    let mut source = e.source();
    while let Some(source_) = source {
        error!("source: {}", source_);
        source = source_.source();
    }
}

fn init_jobs(cmd: Command) -> Vec<Job> {
    match cmd {
        Command::Run => {
            let import_jobs = init_jobs(Command::Import);
            let calculate_jobs = init_jobs(Command::Calculate);
            import_jobs
                .into_iter()
                .chain(calculate_jobs.into_iter())
                .collect()
        }
        Command::Import => all_chains().into_iter().map(Job::Import).collect(),
        Command::Calculate => {
            vec![Job::Calculate]
        }
    }
}

async fn make_importer(rpc_config: &RpcConfig) -> Result<Importer> {
    let clients = make_all_clients(rpc_config).await?;

    Ok(Importer {
        db: Arc::new(JsonDb),
        clients,
    })
}

async fn make_all_clients(rpc_config: &RpcConfig) -> Result<HashMap<Chain, Box<dyn Client>>> {
    let mut client_futures = vec![];
    for chain in all_chains() {
        let rpc_url = get_rpc_url(&chain, rpc_config).to_string();
        let client_future = task::spawn(make_client(chain, rpc_url));
        client_futures.push((chain, client_future));
    }

    let mut clients = HashMap::new();

    for (chain, client_future) in client_futures {
        let client = client_future.await??;
        clients.insert(chain, client);
    }

    Ok(clients)
}

async fn make_client(chain: Chain, rpc_url: String) -> Result<Box<dyn Client>> {
    info!("creating client for {} at {}", chain, rpc_url);

    match chain {
        Chain::Arbitrum
        | Chain::Avalanche
        | Chain::Binance
        | Chain::Celo
        | Chain::Cronos
        | Chain::Ethereum
        | Chain::Fuse
        | Chain::Fantom
        | Chain::Harmony
        | Chain::Heco
        | Chain::KuCoin
        | Chain::Moonriver
        | Chain::OKEx
        | Chain::Polygon
        | Chain::Rootstock
        | Chain::Telos
        | Chain::XDai => {
            let client = EthersClient::new(chain, &rpc_url)?;
            let version = client.client_version().await?;
            info!("node version for {}: {}", chain, version);

            Ok(Box::new(client))
        }
        Chain::Solana => {
            let client = SolanaClient::new(&rpc_url)?;
            let version = client.client_version().await?;
            info!("node version for Solana: {}", version);

            Ok(Box::new(client))
        }
    }
}

fn get_rpc_url<'a>(chain: &Chain, rpc_config: &'a RpcConfig) -> &'a str {
    if let Some(url) = rpc_config.chains.get(chain) {
        return url;
    } else {
        todo!()
    }
}

struct Importer {
    db: Arc<dyn Db>,
    clients: HashMap<Chain, Box<dyn Client>>,
}

impl Importer {
    async fn do_job(&self, job: Job) -> Vec<Job> {
        let r = match job {
            Job::Import(chain) => self.import(chain).await,
            Job::Calculate => self.calculate().await,
        };

        match r {
            Ok(new_jobs) => new_jobs,
            Err(e) => {
                print_error(&e);
                error!("error running job. repeating");
                delay::job_error_delay().await;
                vec![job]
            }
        }
    }

    async fn import(&self, chain: Chain) -> Result<Vec<Job>> {
        let client = self.clients.get(&chain).expect("client");
        import::import(chain, client.as_ref(), &self.db).await?;
        Ok(vec![Job::Import(chain)])
    }

    async fn calculate(&self) -> Result<Vec<Job>> {
        info!("beginning tps calculation");
        let tasks: Vec<(Chain, JoinHandle<Result<ChainCalcs>>)> = all_chains()
            .into_iter()
            .map(|chain| {
                let calc_future = calculate_for_chain(self.db.clone(), chain);
                (chain, task::spawn(calc_future))
            })
            .collect();

        for (chain, task) in tasks {
            let res = task.await?;
            match res {
                Ok(calcs) => {
                    info!("calculated {} tps for chain {}", calcs.tps, calcs.chain);
                    let db = self.db.clone();
                    task::spawn_blocking(move || db.store_tps(calcs.chain, calcs.tps)).await??;
                }
                Err(e) => {
                    print_error(&anyhow::Error::from(e));
                    error!("error calculating for {}", chain);
                }
            }
        }

        delay::recalculate_delay().await;

        Ok(vec![Job::Calculate])
    }
}

struct ChainCalcs {
    chain: Chain,
    tps: f64,
}

async fn calculate_for_chain(db: Arc<dyn Db>, chain: Chain) -> Result<ChainCalcs> {
    let highest_block_number = {
        let db = db.clone();
        task::spawn_blocking(move || db.load_highest_block_number(chain)).await??
    };
    let highest_block_number =
        highest_block_number.ok_or_else(|| anyhow!("no data for chain {}", chain))?;

    async fn load_block_(
        db: &Arc<dyn Db>,
        chain: Chain,
        number: u64,
    ) -> Result<Option<Block>> {
        let db = db.clone();
        task::spawn_blocking(move || db.load_block(chain, number)).await?
    }

    let load_block = |number| load_block_(&db, chain, number);

    let latest_timestamp = load_block(highest_block_number)
        .await?
        .expect("first block")
        .timestamp;

    let seconds_per_week = 60 * 60 * 24 * 7;
    let min_timestamp = latest_timestamp
        .checked_sub(seconds_per_week)
        .expect("underflow");

    let mut current_block_number = highest_block_number;
    let mut current_block = load_block(current_block_number)
        .await?
        .expect("first_block");

    let mut num_txs: u64 = 0;

    let start = std::time::Instant::now();

    let mut blocks = 0;

    let init_timestamp = loop {
        let now = std::time::Instant::now();
        let duration = now - start;
        let secs = duration.as_secs();
        if secs > 0 {
            debug!("bps for {}: {:.2}", chain, blocks as f64 / secs as f64)
        }
        blocks += 1;

        assert!(current_block_number != 0);

        let prev_block_number = current_block.prev_block_number;
        if let Some(prev_block_number) = prev_block_number {
            let prev_block = load_block(prev_block_number).await?;

            if let Some(prev_block) = prev_block {
                num_txs = num_txs
                    .checked_add(current_block.num_txs)
                    .expect("overflow");

                if prev_block.timestamp > current_block.timestamp {
                    warn!(
                        "non-monotonic timestamp in block {} for chain {}. prev: {}; current: {}",
                        current_block_number, chain, prev_block.timestamp, current_block.timestamp
                    );
                }

                if prev_block.timestamp <= min_timestamp {
                    break prev_block.timestamp;
                }
                if prev_block.block_number == 0 {
                    break prev_block.timestamp;
                }

                current_block_number = prev_block_number;
                current_block = prev_block;
            } else {
                break current_block.timestamp;
            }
        } else {
            break current_block.timestamp;
        }
    };

    assert!(init_timestamp <= latest_timestamp);
    let total_seconds = latest_timestamp - init_timestamp;
    let total_seconds_u32 =
        u32::try_from(total_seconds).map_err(|_| anyhow!("seconds overflows u32"))?;
    let num_txs_u32 = u32::try_from(num_txs).map_err(|_| anyhow!("num txs overflows u32"))?;
    let total_seconds_f64 = f64::from(total_seconds_u32);
    let num_txs_f64 = f64::from(num_txs_u32);
    let mut tps = num_txs_f64 / total_seconds_f64;

    // Special float values will not serialize sensibly
    if tps.is_nan() || tps.is_infinite() {
        tps = 0.0;
    }

    Ok(ChainCalcs { chain, tps })
}
