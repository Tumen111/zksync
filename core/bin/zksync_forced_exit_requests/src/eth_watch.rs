use chrono::{DateTime, Utc};
use ethabi::Hash;
use std::{
    convert::TryFrom,
    time::{Duration, Instant},
};
use std::{convert::TryInto, fmt::Debug};
use tokio::task::JoinHandle;
use tokio::time;
use web3::{
    contract::Contract,
    transports::Http,
    types::{BlockNumber, Log},
    Web3,
};
use zksync_config::ZkSyncConfig;
use zksync_storage::ConnectionPool;

use zksync_contracts::forced_exit_contract;
use zksync_types::H160;

use zksync_api::core_api_client::CoreApiClient;
use zksync_core::eth_watch::{get_contract_events, get_web3_block_number, WatcherMode};
use zksync_types::forced_exit_requests::FundsReceivedEvent;

/// As `infura` may limit the requests, upon error we need to wait for a while
/// before repeating the request.
const RATE_LIMIT_DELAY: Duration = Duration::from_secs(30);

use crate::prepare_forced_exit_sender;

use super::ForcedExitSender;

struct ContractTopics {
    pub funds_received: Hash,
}

impl ContractTopics {
    fn new(contract: &ethabi::Contract) -> Self {
        Self {
            funds_received: contract
                .event("FundsReceived")
                .expect("forced_exit contract abi error")
                .signature(),
        }
    }
}
pub struct EthClient {
    web3: Web3<Http>,
    forced_exit_contract: Contract<Http>,
    topics: ContractTopics,
}

impl EthClient {
    pub fn new(web3: Web3<Http>, zksync_contract_addr: H160) -> Self {
        let forced_exit_contract =
            Contract::new(web3.eth(), zksync_contract_addr, forced_exit_contract());

        let topics = ContractTopics::new(forced_exit_contract.abi());
        Self {
            forced_exit_contract,
            web3,
            topics,
        }
    }

    async fn get_events<T>(&self, from: u64, to: u64, topics: Vec<Hash>) -> anyhow::Result<Vec<T>>
    where
        T: TryFrom<Log>,
        T::Error: Debug,
    {
        let from = BlockNumber::from(from);
        let to = BlockNumber::from(to);
        get_contract_events(
            &self.web3,
            self.forced_exit_contract.address(),
            from,
            to,
            topics,
        )
        .await
    }

    async fn get_funds_received_events(
        &self,
        from: u64,
        to: u64,
    ) -> anyhow::Result<Vec<FundsReceivedEvent>> {
        let start = Instant::now();
        let result = self
            .get_events(from, to, vec![self.topics.funds_received])
            .await;

        metrics::histogram!(
            "forced_exit_requests.get_funds_received_events",
            start.elapsed()
        );
        result
    }

    async fn block_number(&self) -> anyhow::Result<u64> {
        get_web3_block_number(&self.web3).await
    }
}

struct ForcedExitContractWatcher {
    connection_pool: ConnectionPool,
    config: ZkSyncConfig,
    eth_client: EthClient,
    last_viewed_block: u64,
    forced_exit_sender: ForcedExitSender,

    mode: WatcherMode,
}

// Usually blocks are created much slower (at rate 1 block per 10-20s),
// but the block time falls through time, so just to double-check
const MILLIS_PER_BLOCK: i64 = 7000;

// Returns number of blocks that should have been created during the time
fn time_range_to_block_diff(from: DateTime<Utc>, to: DateTime<Utc>) -> u64 {
    let millis_from = from.timestamp_millis();
    let millis_to = to.timestamp_millis();

    if millis_to >= millis_from {
        ((millis_to - millis_from) / MILLIS_PER_BLOCK)
            .try_into()
            .unwrap()
    } else {
        0u64
    }
}

impl ForcedExitContractWatcher {
    async fn restore_state_from_eth(&mut self, block: u64) -> anyhow::Result<()> {
        //let last_block = self.eth_client.get_block_number().await.expect("Failed to restore ");

        let mut storage = self.connection_pool.access_storage().await?;
        let mut fe_schema = storage.forced_exit_requests_schema();

        let oldest_request = fe_schema.get_oldest_unfulfilled_request().await?;

        let wait_confirmations = self.config.forced_exit_requests.wait_confirmations;

        // No oldest request means that there are no requests that were possibly ignored
        let oldest_request = match oldest_request {
            Some(r) => r,
            None => {
                self.last_viewed_block = block - wait_confirmations;
                return Ok(());
            }
        };

        let block_diff = time_range_to_block_diff(oldest_request.created_at, Utc::now());
        let max_possible_viewed_block = block - wait_confirmations;

        // If the last block is too young, then we will use max_possible_viewed_block,
        // otherwise we will use block - block_diff
        self.last_viewed_block = std::cmp::min(block - block_diff, max_possible_viewed_block);

        Ok(())
    }

    // TODO try to move it to eth client
    fn is_backoff_requested(&self, error: &anyhow::Error) -> bool {
        error.to_string().contains("429 Too Many Requests")
    }

    fn enter_backoff_mode(&mut self) {
        let backoff_until = Instant::now() + RATE_LIMIT_DELAY;
        self.mode = WatcherMode::Backoff(backoff_until);
        // This is needed to track how much time is spent in backoff mode
        // and trigger grafana alerts
        metrics::histogram!(
            "forced_exit_requests.eth_watcher.enter_backoff_mode",
            RATE_LIMIT_DELAY
        );
    }

    fn polling_allowed(&mut self) -> bool {
        match self.mode {
            WatcherMode::Working => true,
            WatcherMode::Backoff(delay_until) => {
                if Instant::now() >= delay_until {
                    vlog::info!("Exiting the backoff mode");
                    self.mode = WatcherMode::Working;
                    true
                } else {
                    // We have to wait more until backoff is disabled.
                    false
                }
            }
        }
    }

    fn handle_infura_error(&mut self, error: anyhow::Error) {
        if self.is_backoff_requested(&error) {
            vlog::warn!(
                "Rate limit was reached, as reported by Ethereum node. \
                Entering the backoff mode"
            );
            self.enter_backoff_mode();
        } else {
            // Some unexpected kind of error, we won't shutdown the node because of it,
            // but rather expect node administrators to handle the situation.
            vlog::error!("Failed to process new blocks {}", error);
        }
    }

    pub async fn poll(&mut self) {
        if !self.polling_allowed() {
            // Polling is currently disabled, skip it.
            return;
        }

        let last_block = match self.eth_client.block_number().await {
            Ok(block) => block,
            Err(error) => {
                self.handle_infura_error(error);
                return;
            }
        };

        let wait_confirmations = self.config.forced_exit_requests.wait_confirmations;
        let last_confirmed_block = last_block.saturating_sub(wait_confirmations);
        if last_confirmed_block <= self.last_viewed_block {
            return;
        };

        let events = self
            .eth_client
            .get_funds_received_events(self.last_viewed_block + 1, last_confirmed_block)
            .await;

        let events = match events {
            Ok(e) => e,
            Err(error) => {
                self.handle_infura_error(error);
                return;
            }
        };

        for e in events {
            self.forced_exit_sender
                .process_request(e.amount.clone())
                .await;
        }

        self.last_viewed_block = last_confirmed_block;
    }

    pub async fn run(mut self) {
        // As infura may be not responsive, we want to retry the query until we've actually got the
        // block number.
        // Normally, however, this loop is not expected to last more than one iteration.
        let block = loop {
            let block = self.eth_client.block_number().await;

            match block {
                Ok(block) => {
                    break block;
                }
                Err(error) => {
                    vlog::warn!(
                        "Unable to fetch last block number: '{}'. Retrying again in {} seconds",
                        error,
                        RATE_LIMIT_DELAY.as_secs()
                    );

                    time::delay_for(RATE_LIMIT_DELAY).await;
                }
            }
        };

        // We don't expect rate limiting to happen again
        self.restore_state_from_eth(block)
            .await
            .expect("Failed to restore state for ForcedExit eth_watcher");

        let mut timer = time::interval(Duration::from_secs(1));

        loop {
            timer.tick().await;
            self.poll().await;
        }
    }
}

pub fn run_forced_exit_contract_watcher(
    core_api_client: CoreApiClient,
    connection_pool: ConnectionPool,
    config: ZkSyncConfig,
) -> JoinHandle<()> {
    let transport = web3::transports::Http::new(&config.eth_client.web3_url[0]).unwrap();
    let web3 = web3::Web3::new(transport);
    let eth_client = EthClient::new(web3, config.contracts.forced_exit_addr);

    tokio::spawn(async move {
        // It is fine to unwrap here, since without it there is not way we
        // can be sure that the forced exit sender will work properly
        prepare_forced_exit_sender(connection_pool.clone(), core_api_client.clone(), &config)
            .await
            .unwrap();

        // It is ok to unwrap here, since if fe_sender is not created, then
        // the watcher is meaningless
        let mut forced_exit_sender = ForcedExitSender::new(
            core_api_client.clone(),
            connection_pool.clone(),
            config.clone(),
        )
        .await
        .unwrap();

        forced_exit_sender
            .await_unconfirmed()
            .await
            .expect("Unexpected error while trying to wait for unconfirmed transactions");

        let contract_watcher = ForcedExitContractWatcher {
            connection_pool,
            config,
            eth_client,
            last_viewed_block: 0,
            forced_exit_sender,
            mode: WatcherMode::Working,
        };

        contract_watcher.run().await;
    })
}
