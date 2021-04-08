use std::iter::FromIterator;
use std::{collections::HashMap, sync::Arc};

use futures::future::join_all;
use graph::prelude::{o, MetricsRegistry, NodeId};
use graph::{
    prelude::{info, CheapClone, EthereumNetworkIdentifier, Logger},
    util::security::SafeDisplay,
};
use graph_store_postgres::connection_pool::ConnectionPool;
use graph_store_postgres::{
    BlockStore as DieselBlockStore, ChainHeadUpdateListener as PostgresChainHeadUpdateListener,
    Shard as ShardName, Store as DieselStore, SubgraphStore, SubscriptionManager, PRIMARY_SHARD,
};

use crate::config::{Config, Shard};

pub struct StoreBuilder {
    logger: Logger,
    subgraph_store: Arc<SubgraphStore>,
    pools: HashMap<ShardName, ConnectionPool>,
    primary_shard: Shard,
    subscription_manager: Arc<SubscriptionManager>,
    registry: Arc<dyn MetricsRegistry>,
    /// Map network names to the shards where they are/should be stored
    chains: HashMap<String, ShardName>,
}

impl StoreBuilder {
    /// Set up all stores, and run migrations. This does a complete store
    /// setup whereas other methods here only get connections for an already
    /// initialized store
    pub async fn new(
        logger: &Logger,
        node: &NodeId,
        config: &Config,
        registry: Arc<dyn MetricsRegistry>,
    ) -> Self {
        let primary_shard = config.primary_store().clone();

        let subscription_manager = Arc::new(SubscriptionManager::new(
            logger.cheap_clone(),
            primary_shard.connection.to_owned(),
        ));

        let (store, pools) =
            Self::make_sharded_store_and_primary_pool(logger, node, config, registry.cheap_clone());

        // Perform setup for all the pools
        let details = pools
            .values()
            .map(|pool| pool.connection_detail())
            .collect::<Result<Vec<_>, _>>()
            .expect("connection url's contain enough detail");
        let details = Arc::new(details);

        join_all(pools.iter().map(|(_, pool)| pool.setup(details.clone()))).await;

        let chains = HashMap::from_iter(config.chains.chains.iter().map(|(name, chain)| {
            let shard = ShardName::new(chain.shard.to_string())
                .expect("config validation catches invalid names");
            (name.to_string(), shard)
        }));

        Self {
            logger: logger.cheap_clone(),
            subgraph_store: store,
            pools,
            subscription_manager,
            primary_shard,
            registry,
            chains,
        }
    }

    /// Make a `ShardedStore` across all configured shards, and also return
    /// the main connection pools for each shard, but not any pools for
    /// replicas
    fn make_sharded_store_and_primary_pool(
        logger: &Logger,
        node: &NodeId,
        config: &Config,
        registry: Arc<dyn MetricsRegistry>,
    ) -> (Arc<SubgraphStore>, HashMap<ShardName, ConnectionPool>) {
        let shards: Vec<_> = config
            .stores
            .iter()
            .map(|(name, shard)| {
                let logger = logger.new(o!("shard" => name.to_string()));
                let conn_pool = Self::main_pool(&logger, node, name, shard, registry.cheap_clone());

                let (read_only_conn_pools, weights) =
                    Self::replica_pools(&logger, node, name, shard, registry.cheap_clone());

                let name =
                    ShardName::new(name.to_string()).expect("shard names have been validated");
                (name, conn_pool, read_only_conn_pools, weights)
            })
            .collect();

        let pools: HashMap<_, _> = HashMap::from_iter(
            shards
                .iter()
                .map(|(name, pool, _, _)| (name.clone(), pool.clone())),
        );

        let store = Arc::new(SubgraphStore::new(
            logger,
            shards,
            Arc::new(config.deployment.clone()),
        ));

        (store, pools)
    }

    // Somehow, rustc gets this wrong; the function is used in
    // `manager::make_store`
    #[allow(dead_code)]
    pub fn make_sharded_store(
        logger: &Logger,
        node: &NodeId,
        config: &Config,
        registry: Arc<dyn MetricsRegistry>,
    ) -> Arc<SubgraphStore> {
        Self::make_sharded_store_and_primary_pool(logger, node, config, registry).0
    }

    /// Create a connection pool for the main database of hte primary shard
    /// without connecting to all the other configured databases
    pub fn main_pool(
        logger: &Logger,
        node: &NodeId,
        name: &str,
        shard: &Shard,
        registry: Arc<dyn MetricsRegistry>,
    ) -> ConnectionPool {
        let logger = logger.new(o!("pool" => "main"));
        let pool_size = shard.pool_size.size_for(node, name).expect(&format!(
            "we can determine the pool size for store {}",
            name
        ));
        info!(
            logger,
            "Connecting to Postgres";
            "url" => SafeDisplay(shard.connection.as_str()),
            "conn_pool_size" => pool_size,
            "weight" => shard.weight
        );
        ConnectionPool::create(
            name,
            "main",
            shard.connection.to_owned(),
            pool_size,
            &logger,
            registry.cheap_clone(),
        )
    }

    /// Create connection pools for each of the replicas
    fn replica_pools(
        logger: &Logger,
        node: &NodeId,
        name: &str,
        shard: &Shard,
        registry: Arc<dyn MetricsRegistry>,
    ) -> (Vec<ConnectionPool>, Vec<usize>) {
        let mut weights: Vec<_> = vec![shard.weight];
        (
            shard
                .replicas
                .values()
                .enumerate()
                .map(|(i, replica)| {
                    let pool = &format!("replica{}", i + 1);
                    let logger = logger.new(o!("pool" => pool.clone()));
                    info!(
                        &logger,
                        "Connecting to Postgres (read replica {})", i+1;
                        "url" => SafeDisplay(replica.connection.as_str()),
                        "weight" => replica.weight
                    );
                    weights.push(replica.weight);
                    let pool_size = replica.pool_size.size_for(node, name).expect(&format!(
                        "we can determine the pool size for replica {}",
                        name
                    ));
                    ConnectionPool::create(
                        name,
                        pool,
                        replica.connection.clone(),
                        pool_size,
                        &logger,
                        registry.cheap_clone(),
                    )
                })
                .collect(),
            weights,
        )
    }

    /// Return a store that combines both a `Store` for subgraph data
    /// and a `BlockStore` for all chain related data
    pub fn network_store(
        self,
        networks: Vec<(String, EthereumNetworkIdentifier)>,
    ) -> Arc<DieselStore> {
        let chain_head_update_listener = Arc::new(PostgresChainHeadUpdateListener::new(
            &self.logger,
            self.registry.cheap_clone(),
            self.primary_shard.connection.to_owned(),
        ));

        let networks = networks
            .into_iter()
            .map(|(name, ident)| {
                let shard = self.chains.get(&name).unwrap_or(&*PRIMARY_SHARD).clone();
                (name, ident, shard)
            })
            .collect();

        let logger = self.logger.new(o!("component" => "BlockStore"));

        let block_store = Arc::new(
            DieselBlockStore::new(
                logger,
                networks,
                self.pools.clone(),
                chain_head_update_listener.clone(),
            )
            .expect("Creating the BlockStore works"),
        );

        Arc::new(DieselStore::new(
            self.subgraph_store.cheap_clone(),
            block_store,
        ))
    }

    pub fn subscription_manager(&self) -> Arc<SubscriptionManager> {
        self.subscription_manager.cheap_clone()
    }

    // This is used in the test-store, but rustc keeps complaining that it
    // is not used
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn primary_pool(&self) -> ConnectionPool {
        self.pools.get(&*PRIMARY_SHARD).unwrap().clone()
    }
}
