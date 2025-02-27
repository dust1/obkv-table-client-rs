/*-
 * #%L
 * OBKV Table Client Framework
 * %%
 * Copyright (C) 2021 OceanBase
 * %%
 * OBKV Table Client Framework is licensed under Mulan PSL v2.
 * You can use this software according to the terms and conditions of the Mulan PSL v2.
 * You may obtain a copy of Mulan PSL v2 at:
 *          http://license.coscl.org.cn/MulanPSL2
 * THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY KIND,
 * EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO NON-INFRINGEMENT,
 * MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
 * See the Mulan PSL v2 for more details.
 * #L%
 */

use std::{
    borrow::Borrow,
    collections::HashMap,
    isize,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::Duration,
};

use futures::{future, Future};
use futures_cpupool::{Builder as CpuPoolBuilder, CpuPool};
use prometheus::*;
use rand::{seq::SliceRandom, thread_rng};
use scheduled_thread_pool::ScheduledThreadPool;

use super::{
    metrics::OBKV_CLIENT_RETRY_COUNTER_VEC,
    ocp::{ObOcpModelManager, OcpModel},
    query::{QueryResultSet, QueryStreamResult, StreamQuerier, TableQuery},
    table::{self, ObTable},
    ClientConfig, Table, TableOpResult,
};
use crate::{
    error::{self, CommonErrCode, Error::Common as CommonErr, Result},
    location::{
        ob_part_constants::{MASK, PART_ID_SHIFT},
        ObPartitionLevel, ObServerAddr, ObTableLocation, ReplicaLocation, TableEntry,
        TableEntryKey,
    },
    rpc::{
        conn_pool::{Builder as ConnPoolBuilder, ConnPool},
        protocol::{
            payloads::{
                ObTableBatchOperation, ObTableEntityType, ObTableOperationRequest,
                ObTableOperationResult, ObTableOperationType,
            },
            query::{
                ObHTableFilter, ObNewRange, ObScanOrder, ObTableQuery, ObTableQueryRequest,
                ObTableQueryResult, ObTableStreamRequest,
            },
        },
        proxy::Proxy,
        Builder as ConnBuilder,
    },
    serde_obkv::value::Value,
    util::{
        assert_not_empty, current_time_millis, duration_to_millis, millis_to_secs,
        permit::{PermitGuard, Permits},
        HandyRwLock,
    },
    ResultCodes,
};

lazy_static! {
    pub static ref OBKV_CLIENT_HISTOGRAM_VEC: HistogramVec = register_histogram_vec!(
        "obkv_client_duration_seconds",
        "Bucketed histogram of client operations.",
        &["type"],
        exponential_buckets(0.0005, 2.0, 18).unwrap()
    )
    .unwrap();
    pub static ref OBKV_CLIENT_HISTOGRAM_NUM_VEC: HistogramVec = register_histogram_vec!(
        "obkv_client_metric_distribution",
        "Bucketed histogram of metric distribution",
        &["type"],
        linear_buckets(5.0, 20.0, 20).unwrap()
    )
    .unwrap();
}

const MAX_PRIORITY: isize = 50;

pub struct ServerRoster {
    max_priority: AtomicIsize,
    roster: RwLock<Arc<Vec<ObServerAddr>>>,
}

impl ServerRoster {
    fn new() -> Self {
        ServerRoster {
            max_priority: AtomicIsize::new(0),
            roster: RwLock::new(Arc::new(vec![])),
        }
    }

    fn peek_random_server(&self) -> Option<ObServerAddr> {
        let roster = self.roster.rl();
        if roster.is_empty() {
            None
        } else {
            let mut rng = thread_rng();
            roster.choose(&mut rng).cloned()
        }
    }

    pub fn get_members(&self) -> Arc<Vec<ObServerAddr>> {
        self.roster.rl().clone()
    }

    fn reset(&self, members: Vec<ObServerAddr>) {
        self.max_priority.store(0, Ordering::Release);
        (*self.roster.wl()) = Arc::new(members);
    }

    pub fn upgrade_max_priority(&self, priority: isize) {
        if self.max_priority.load(Ordering::Acquire) >= priority {
            return;
        }

        if priority == 0 {
            self.max_priority.store(0, Ordering::Release);
            return;
        }
        self.reset_max_priority();
    }

    pub fn downgrade_max_priority(&self, priority: isize) {
        if self.max_priority.load(Ordering::Acquire) <= priority {
            return;
        }
        self.reset_max_priority();
    }

    fn reset_max_priority(&self) {
        if self.roster.rl().is_empty() {
            self.max_priority.store(0, Ordering::Release);
            return;
        }

        let mut priority: isize = isize::min_value();

        for addr in self.roster.rl().iter() {
            if addr.priority() > priority {
                priority = addr.priority();
            }
        }

        self.max_priority.store(priority, Ordering::Release);
    }

    pub fn max_priority(&self) -> isize {
        let p = self.max_priority.load(Ordering::Acquire);

        if p < -MAX_PRIORITY {
            -MAX_PRIORITY
        } else if p > MAX_PRIORITY {
            MAX_PRIORITY
        } else {
            p
        }
    }
}

/// ObTable Client running mode
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningMode {
    // Table mode
    Normal,
    // HBase mode, K/Q/T
    HBase,
}

type Lock = Mutex<u8>;

// ObTableClient inner implemetation.
struct ObTableClientInner {
    location: ObTableLocation,
    ocp_manager: ObOcpModelManager,
    config: ClientConfig,
    table_entry_refresh_continuous_failure_count: AtomicUsize,
    datasource_name: String,
    param_url: String,
    full_user_name: String,
    user_name: String,
    tenant_name: String,
    cluster_name: String,
    password: String,
    database: String,
    ocp_model: RwLock<OcpModel>,
    initialized: AtomicBool,
    closed: AtomicBool,
    status_mutex: Lock,

    //ServerAddr(all) -> ObTableConnection
    table_roster: RwLock<HashMap<ObServerAddr, Arc<ObTable>>>,
    server_roster: ServerRoster,
    running_mode: RunningMode,
    //TableName -> TableEntry
    table_locations: RwLock<HashMap<String, Arc<TableEntry>>>,
    table_mutexs: RwLock<HashMap<String, Arc<Lock>>>,
    //TableName -> rowKey element
    table_row_key_element: RwLock<HashMap<String, HashMap<String, i32>>>,
    connection_pools: RwLock<HashMap<ObServerAddr, Arc<ConnPool>>>,

    _retry_on_change_master: bool,
    //TableName -> failure counter
    table_continuous_failures: RwLock<HashMap<String, Arc<AtomicUsize>>>,

    refresh_metadata_mutex: Lock,
    last_refresh_metadata_ts: AtomicUsize,

    conn_init_thread_pool: Arc<ScheduledThreadPool>,

    // table_name => thread pool
    table_batch_op_thread_pools: Arc<RwLock<HashMap<String, Arc<CpuPool>>>>,
    // query concurrency control
    query_permits: Option<Permits>,
}

impl ObTableClientInner {
    fn internal_new(
        param_url: String,
        full_user_name: String,
        password: String,
        user_name: String,
        tenant_name: String,
        cluster_name: String,
        database: String,
        running_mode: RunningMode,
        config: ClientConfig,
    ) -> Result<Self> {
        let conn_init_thread_num = config.conn_init_thread_num;
        let ocp_manager =
            ObOcpModelManager::new(config.rslist_acquire_timeout, &config.ocp_model_cache_file)?;

        let query_permits = if let Some(max) = config.query_concurrency_limit {
            Some(Permits::new(max))
        } else {
            None
        };

        Ok(Self {
            ocp_manager,
            full_user_name,
            param_url,
            password,
            user_name,
            tenant_name,
            cluster_name,
            database,
            datasource_name: "".to_owned(),
            running_mode,
            config: config.clone(),

            location: ObTableLocation::new(config),
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            status_mutex: Mutex::new(0),
            table_entry_refresh_continuous_failure_count: AtomicUsize::new(0),
            ocp_model: RwLock::new(OcpModel::new()),
            table_roster: RwLock::new(HashMap::new()),
            server_roster: ServerRoster::new(),
            table_locations: RwLock::new(HashMap::new()),
            connection_pools: RwLock::new(HashMap::new()),
            table_mutexs: RwLock::new(HashMap::new()),
            table_row_key_element: RwLock::new(HashMap::new()),
            table_continuous_failures: RwLock::new(HashMap::new()),
            _retry_on_change_master: true, //TODO it's useless right now.
            refresh_metadata_mutex: Mutex::new(0),
            last_refresh_metadata_ts: AtomicUsize::new(0),

            conn_init_thread_pool: Arc::new(ScheduledThreadPool::with_name(
                "conn_init_{}",
                conn_init_thread_num,
            )),
            table_batch_op_thread_pools: Arc::new(RwLock::new(HashMap::new())),
            query_permits,
        })
    }

    #[inline]
    fn acquire_query_permit(&self) -> Result<Option<PermitGuard>> {
        if let Some(permits) = &self.query_permits {
            let guard = permits.acquire()?;
            OBKV_CLIENT_HISTOGRAM_NUM_VEC
                .with_label_values(&["query_concurrency"])
                .observe(guard.permit() as f64);

            Ok(Some(guard))
        } else {
            Ok(None)
        }
    }

    #[inline]
    fn get_table_entry_from_cache(&self, table_name: &str) -> Option<Arc<TableEntry>> {
        match self.table_locations.rl().get(table_name) {
            Some(v) => Some(v.clone()),
            None => None,
        }
    }

    fn on_table_op_failure(&self, table_name: &str, error: &error::Error) -> Result<()> {
        if error.need_refresh_table() {
            debug!(
                "ObTableClientInner::on_table_op_failure: found error requiring refresh, \
                 table_name:{}, err:{}",
                table_name, error
            );
            self.get_or_refresh_table_entry_non_blocking(table_name, true)?;
        }

        let counter = {
            let table_continuous_failures = self.table_continuous_failures.rl();
            match table_continuous_failures.get(table_name) {
                Some(c) => c.clone(),
                None => {
                    drop(table_continuous_failures); //release read lock
                    let mut table_continuous_failures = self.table_continuous_failures.wl();
                    if let Some(c) = table_continuous_failures.get(table_name) {
                        c.clone()
                    } else {
                        let counter = Arc::new(AtomicUsize::new(0));
                        table_continuous_failures.insert(table_name.to_owned(), counter.clone());
                        counter
                    }
                }
            }
        };

        if counter.fetch_add(1, Ordering::SeqCst) >= self.config.runtime_continuous_failure_ceiling
        {
            warn!("ObTableClientInner::on_table_op_failure refresh table entry {} while execute failed times exceeded {}, err: {}.",
                  table_name, self.config.runtime_continuous_failure_ceiling, error);
            self.get_or_refresh_table_entry(table_name, true)?;
            counter.store(0, Ordering::SeqCst);
        }

        Ok(())
    }

    #[inline]
    fn reset_table_failure(&self, table_name: &str) {
        if let Some(counter) = self.table_continuous_failures.rl().get(table_name) {
            counter.store(0, Ordering::SeqCst);
        }
    }

    fn refresh_table_entry(
        &self,
        table_name: &str,
        table_entry: Option<&Arc<TableEntry>>,
    ) -> Result<Arc<TableEntry>> {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["refresh_table"])
            .start_timer();

        let table_entry_key = TableEntryKey::new(
            &self.cluster_name,
            &self.tenant_name,
            &self.database,
            table_name,
        );

        let server_roster = &self.server_roster;
        let connect_timeout = self.config.table_entry_acquire_connect_timeout;
        let read_timeout = self.config.table_entry_acquire_read_timeout;
        let priority_timeout = self.config.server_address_priority_timeout;

        let result = if let Some(table_entry) = table_entry {
            self.location.load_table_location_with_priority(
                server_roster,
                &table_entry_key,
                table_entry.borrow(),
                connect_timeout,
                read_timeout,
                priority_timeout,
            )
        } else {
            let mut table_entry: TableEntry = self.location.load_table_entry_with_priority(
                server_roster,
                &table_entry_key,
                connect_timeout,
                read_timeout,
                priority_timeout,
            )?;

            if table_entry.is_partition_table() {
                match self.running_mode {
                    RunningMode::Normal => match self.table_row_key_element.rl().get(table_name) {
                        Some(v) => table_entry.set_row_key_element(v.clone()),
                        None => {
                            return Err(CommonErr(
                                CommonErrCode::NotFound,
                                format!(
                                    "Partition table must has row key element, table_key={:?}",
                                    table_entry_key
                                ),
                            ));
                        }
                    },
                    RunningMode::HBase => {
                        let mut hbase_row_key_element: HashMap<String, i32> = HashMap::new();
                        hbase_row_key_element.insert("K".to_owned(), 0);
                        hbase_row_key_element.insert("Q".to_owned(), 1);
                        hbase_row_key_element.insert("T".to_owned(), 2);
                        table_entry.set_row_key_element(hbase_row_key_element);
                    }
                }
                table_entry.prepare()?;
            }
            Ok(table_entry)
        }?;

        self.table_entry_refresh_continuous_failure_count
            .store(0, Ordering::SeqCst);
        Ok(Arc::new(result))
    }

    fn get_table(
        &self,
        table_name: &str,
        row_key: &[Value],
        refresh: bool,
    ) -> Result<(i64, Arc<ObTable>)> {
        let table_entry = self.get_or_refresh_table_entry(table_name, refresh)?;
        let part_id = self.get_partition(&table_entry, row_key)?;
        self.get_or_create_table(table_name, &table_entry, part_id)
    }

    fn get_tables(
        &self,
        table_name: &str,
        start: &[Value],
        start_inclusive: bool,
        end: &[Value],
        end_inclusive: bool,
        refresh: bool,
    ) -> Result<Vec<(i64, Arc<ObTable>)>> {
        //1. get table entry info
        let table_entry = self.get_or_refresh_table_entry(table_name, refresh)?;

        //2. get replica locaton
        let part_id_with_replicas: Vec<(i64, ReplicaLocation)> =
            self.get_partition_leaders(&table_entry, start, start_inclusive, end, end_inclusive)?;

        let mut result: Vec<(i64, Arc<ObTable>)> = vec![];

        for (part_id, replica_location) in part_id_with_replicas {
            if let Some(table) = self.table_roster.rl().get(replica_location.addr()) {
                result.push((part_id, table.clone()));
                continue;
            }
            //Table not found, try to refresh it and retry get it again.
            warn!("ObTableClientInner::get_tables can not get ob table by address {:?} so that will sync refresh metadata.",
                  replica_location.addr());
            self.sync_refresh_metadata()?;
            let table_entry = self.get_or_refresh_table_entry(table_name, true)?;
            match table_entry.get_partition_location_with_part_id(part_id) {
                Some(location) => match location.leader() {
                    Some(leader) => {
                        //Found leader of replication ,try to get table from table roster
                        if let Some(ob_table) = self.table_roster.rl().get(leader.addr()) {
                            result.push((part_id, ob_table.clone()));
                        } else {
                            return Err(CommonErr(
                                CommonErrCode::NotFound,
                                format!("ObTable to {:?} not found in table_roster.", leader),
                            ));
                        }
                    }
                    None => {
                        //Leader not found
                        return Err(CommonErr(
                            CommonErrCode::NotFound,
                            format!(
                                "Leader not found part_id={} for table {:?}",
                                part_id,
                                table_entry.to_owned(),
                            ),
                        ));
                    }
                },
                None => {
                    //Replica not found.
                    return Err(CommonErr(
                        CommonErrCode::NotFound,
                        format!(
                            "Replica not found part_id={} for table {:?}",
                            part_id,
                            table_entry.to_owned(),
                        ),
                    ));
                }
            }
        }

        Ok(result)
    }

    fn fill_partition_location_with_part_id(
        &self,
        result: &mut Vec<(i64, ReplicaLocation)>,
        table_entry: &TableEntry,
        part_id: i64,
    ) -> Result<()> {
        match table_entry.get_partition_location_with_part_id(part_id) {
            Some(location) => match location.leader() {
                Some(leader) => {
                    result.push((part_id, leader.clone()));
                }
                None => {
                    return Err(CommonErr(
                        CommonErrCode::NotFound,
                        format!(
                            "Leader not found part_id={} for table {:?}",
                            part_id,
                            table_entry.to_owned(),
                        ),
                    ));
                }
            },
            None => {
                return Err(CommonErr(
                    CommonErrCode::NotFound,
                    format!(
                        "Replica not found part_id={} for table {:?}",
                        part_id,
                        table_entry.to_owned(),
                    ),
                ));
            }
        }

        Ok(())
    }

    fn get_partition_leaders(
        &self,
        table_entry: &TableEntry,
        start: &[Value],
        start_inclusive: bool,
        end: &[Value],
        end_inclusive: bool,
    ) -> Result<Vec<(i64, ReplicaLocation)>> {
        let mut result: Vec<(i64, ReplicaLocation)> = vec![];

        if !table_entry.is_partition_table()
            || table_entry.is_partition_level(ObPartitionLevel::Zero)
        {
            //Level zero or not partitioned.
            self.fill_partition_location_with_part_id(&mut result, table_entry, 0)?;
            Ok(result)
        } else if table_entry.is_partition_level(ObPartitionLevel::One) {
            //Level one
            match table_entry.partition_info() {
                Some(info) => match info.first_part_desc() {
                    Some(part_desc) => {
                        let part_ids =
                            part_desc.get_part_ids(start, start_inclusive, end, end_inclusive)?;
                        for part_id in part_ids {
                            self.fill_partition_location_with_part_id(
                                &mut result,
                                table_entry,
                                part_id,
                            )?;
                        }
                        Ok(result)
                    }
                    None => Err(CommonErr(
                        CommonErrCode::NotFound,
                        format!(
                            "First part desc not found for table {:?}",
                            table_entry.to_owned(),
                        ),
                    )),
                },
                None => Err(CommonErr(
                    CommonErrCode::NotFound,
                    format!(
                        "Partition info not found for table {:?}",
                        table_entry.to_owned(),
                    ),
                )),
            }
        } else {
            //Level two
            Err(CommonErr(
                CommonErrCode::PartitionError,
                format!(
                    "Unsupported partition level two right now, table={:?}",
                    table_entry
                ),
            ))
        }
    }

    fn get_or_create_conn_pool(&self, addr: &ObServerAddr) -> Result<Arc<ConnPool>> {
        if let Some(pool) = self.connection_pools.rl().get(addr) {
            return Ok(pool.clone());
        }
        let mut pools = self.connection_pools.wl();
        if let Some(pool) = pools.get(addr) {
            Ok(pool.clone())
        } else {
            let conn_builder = ConnBuilder::new()
                .connect_timeout(self.config.rpc_connect_timeout)
                .read_timeout(self.config.rpc_read_timeout)
                .login_timeout(self.config.rpc_login_timeout)
                .operation_timeout(self.config.rpc_operation_timeout)
                .ip(addr.ip())
                .port(addr.svr_port() as u16)
                .tenant_name(&self.tenant_name)
                .user_name(&self.user_name)
                .database_name(&self.database)
                .password(&self.password);

            let pool = Arc::new(
                ConnPoolBuilder::new()
                    .max_conn_num(self.config.max_conns_per_server)
                    .min_conn_num(self.config.min_idle_conns_per_server)
                    .conn_init_thread_pool(self.conn_init_thread_pool.clone())
                    .conn_builder(conn_builder)
                    .build()?,
            );

            pools.insert(addr.to_owned(), pool.clone());

            Ok(pool)
        }
    }

    fn add_ob_table(&self, addr: &ObServerAddr) -> Result<Arc<ObTable>> {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["add_table"])
            .start_timer();
        let mut table_roster = self.table_roster.wl();
        self.add_ob_table_to_roster(addr, &mut table_roster)
    }

    fn add_ob_table_to_roster(
        &self,
        addr: &ObServerAddr,
        table_roster: &mut HashMap<ObServerAddr, Arc<ObTable>>,
    ) -> Result<Arc<ObTable>> {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["add_ob_table_to_roster"])
            .start_timer();

        // check whether exists
        if let Some(table) = table_roster.get(&addr) {
            return Ok(table.clone());
        }

        let rpc_proxy = Proxy::new(self.get_or_create_conn_pool(addr)?);

        let ob_table = Arc::new(
            table::Builder::new(addr.ip(), addr.svr_port())
                .config(&self.config)
                .tenant_name(&self.tenant_name)
                .user_name(&self.user_name)
                .password(&self.password)
                .database(&self.database)
                .rpc_proxy(rpc_proxy)
                .build(),
        );
        table_roster.insert(addr.clone(), ob_table.clone());
        Ok(ob_table)
    }

    fn get_or_create_table(
        &self,
        table_name: &str,
        table_entry: &Arc<TableEntry>,
        part_id: i64,
    ) -> Result<(i64, Arc<ObTable>)> {
        match self.get_partition_leader(table_entry, part_id) {
            Some((part_id, replica)) => match replica {
                Some(r) => {
                    let addr = r.addr();
                    if let Some(table) = self.table_roster.rl().get(addr) {
                        return Ok((part_id, table.clone()));
                    }

                    let ob_table = self.add_ob_table(addr)?;
                    Ok((part_id, ob_table))
                }

                None => Err(CommonErr(
                    CommonErrCode::NotFound,
                    format!("Replica not found for table {}-{}", table_name, part_id),
                )),
            },
            None => Err(CommonErr(
                CommonErrCode::NotFound,
                format!(
                    "Partition leader not found for table {}-{}",
                    table_name, part_id
                ),
            )),
        }
    }

    fn get_partition_leader(
        &self,
        table_entry: &Arc<TableEntry>,
        part_id: i64,
    ) -> Option<(i64, Option<ReplicaLocation>)> {
        Some((
            part_id,
            match table_entry.partition_entry() {
                Some(entry) => match entry.get_partition_location_with_part_id(part_id) {
                    Some(v) => v.leader().to_owned(),
                    None => None,
                },
                None => None,
            },
        ))
    }

    fn get_partition(&self, table_entry: &Arc<TableEntry>, row_key: &[Value]) -> Result<i64> {
        if !table_entry.is_partition_table() {
            return Ok(0);
        }
        if let Some(partition_info) = table_entry.partition_info() {
            match partition_info.level() {
                ObPartitionLevel::Zero => return Ok(0),
                ObPartitionLevel::One => {
                    if partition_info.first_part_desc().is_none() {
                        error!(
                            "get_partition: partition_info level is one, first_part_desc is none"
                        );
                        return Err(CommonErr(
                            CommonErrCode::PartitionError,
                            "get_partition: partition_info level is one, first_part_desc is none"
                                .to_owned(),
                        ));
                    }
                    if let Some(first_part_desc) = partition_info.first_part_desc() {
                        return first_part_desc.get_part_id(row_key);
                    }
                }
                ObPartitionLevel::Two => {
                    match (
                        partition_info.first_part_desc(),
                        partition_info.sub_part_desc(),
                    ) {
                        (None, None) => {
                            error!("get_partition: partition_info level is two, first_part_desc is none, sub_part_desc is none");
                            return Err(CommonErr(
                                CommonErrCode::PartitionError,
                                "get_partition: partition_info level is two, first_part_desc is none, sub_part_desc is none".to_owned(),
                            ));
                        }
                        (Some(_), None) => {
                            error!(
                                "get_partition: partition_info level is two, sub_part_desc is none"
                            );
                            return Err(CommonErr(
                                CommonErrCode::PartitionError,
                                "get_partition: partition_info level is two, sub_part_desc is none"
                                    .to_owned(),
                            ));
                        }
                        (None, Some(_)) => {
                            error!("get_partition: partition_info level is two, first_part_desc is none");
                            return Err(CommonErr(
                                CommonErrCode::PartitionError,
                                "get_partition: partition_info level is two, first_part_desc is none".to_owned(),
                            ));
                        }
                        (Some(first_part_desc), Some(sub_part_desc)) => {
                            let part_id1 = first_part_desc.get_part_id(row_key);

                            let part_id2 = sub_part_desc.get_part_id(row_key);
                            match (part_id1, part_id2) {
                                (Err(e1), Err(e2)) => {
                                    error!("first_part_desc get_part_id err:{:?}, sub_part_desc get_part_id err:{:?}", e1, e2);
                                    return Err(CommonErr(
                                        CommonErrCode::PartitionError,
                                        "first_part_desc get_part_id err and sub_part_desc get_part_id err".to_owned(),
                                    ));
                                }
                                (Err(e1), Ok(_)) => {
                                    error!("first_part_desc get_part_id err:{:?}", e1);
                                    return Err(CommonErr(
                                        CommonErrCode::PartitionError,
                                        "first_part_desc get_part_id err ".to_owned(),
                                    ));
                                }
                                (Ok(_), Err(e2)) => {
                                    error!("sub_part_desc get_part_id err:{:?}", e2);
                                    return Err(CommonErr(
                                        CommonErrCode::PartitionError,
                                        "sub_part_desc get_part_id err".to_owned(),
                                    ));
                                }
                                (Ok(id1), Ok(id2)) => {
                                    return Ok((id1 << PART_ID_SHIFT) | id2 | MASK);
                                }
                            }
                        }
                    }
                }
                ObPartitionLevel::Unknown => {
                    error!("get_partition error:ObPartitionLevel is Unknown");
                    return Err(CommonErr(
                        CommonErrCode::PartitionError,
                        "get_partition error:ObPartitionLevel is Unknown".to_owned(),
                    ));
                }
            }
        }
        Err(CommonErr(
            CommonErrCode::PartitionError,
            "get_partition error:partition_info is None".to_owned(),
        ))
    }

    fn need_refresh_table_entry(&self, table_entry: &Arc<TableEntry>) -> bool {
        let ratio = 2_f64.powi(self.server_roster.max_priority() as i32);

        let interval_ms =
            duration_to_millis(&self.config.table_entry_refresh_interval_base) as f64 / ratio;

        let ceiling_ms =
            duration_to_millis(&self.config.table_entry_refresh_interval_ceiling) as f64;

        let interval_ms = if interval_ms <= ceiling_ms {
            interval_ms
        } else {
            ceiling_ms
        };
        let passed_ms = (current_time_millis() - table_entry.refresh_time_mills()) as f64;

        trace!(
            "ObTableClientInner::need_refresh_table_entry: ratio:{}, interval_ms:{}, \
             ceiling_ms:{}, passed_ms:{}",
            ratio,
            interval_ms,
            ceiling_ms,
            passed_ms,
        );

        passed_ms >= interval_ms
    }

    fn get_or_refresh_table_entry(
        &self,
        table_name: &str,
        refresh: bool,
    ) -> Result<Arc<TableEntry>> {
        self.get_or_refresh_table_entry_with_blocking(table_name, refresh, true)
    }

    fn get_or_refresh_table_entry_non_blocking(
        &self,
        table_name: &str,
        refresh: bool,
    ) -> Result<Arc<TableEntry>> {
        self.get_or_refresh_table_entry_with_blocking(table_name, refresh, false)
    }

    fn get_or_refresh_table_entry_with_blocking(
        &self,
        table_name: &str,
        refresh: bool,
        blocking: bool,
    ) -> Result<Arc<TableEntry>> {
        //Attempt to retrieve it from cache, avoid locking.
        if let Some(table_entry) = self.get_table_entry_from_cache(table_name) {
            //If the refresh is false indicates that user tolerate not the latest data
            if !refresh || !self.need_refresh_table_entry(&table_entry) {
                return Ok(table_entry);
            }
        }

        //Table entry is none or not refresh
        let table_mutex = {
            let table_mutexs = self.table_mutexs.rl();
            match table_mutexs.get(table_name) {
                Some(mutex) => mutex.clone(),
                None => {
                    drop(table_mutexs); //release read lock
                    let mut table_mutexs = self.table_mutexs.wl();
                    // let table_mutex = table_mutexs.get(table_name);

                    //double check
                    match table_mutexs.get(table_name) {
                        Some(mutex) => mutex.clone(),
                        None => {
                            let table_mutex = Arc::new(Mutex::new(0));

                            table_mutexs.insert(table_name.to_owned(), table_mutex.clone());
                            table_mutex
                        }
                    }

                    // if table_mutex.is_some() {
                    //     table_mutex.unwrap().clone()
                    // } else {
                    //     let table_mutex = Arc::new(Mutex::new(0));

                    //     table_mutexs.insert(table_name.to_owned(),
                    // table_mutex.clone());     table_mutex
                    // }
                    //out of write lock of table_mutexs
                }
            }
            //out of read lock of table_mutexs
        };

        //Lock with table mutex
        let _lock = if blocking {
            table_mutex.lock().unwrap()
        } else {
            match table_mutex.try_lock() {
                Ok(lock) => lock,
                Err(_e) =>
                    return Err(CommonErr(CommonErrCode::Lock,
                                         format!(
                                             "ObTableClientInner::get_or_refresh_table_entry: fail to acquire table lock because \
                        some other thread is refreshing with the lock, table_name:{}", table_name)
                    ))
            }
        };

        trace!(
            "ObTableClientInner::get_or_refresh_table_entry: lock got for table:{}",
            table_name
        );
        //double-check whether need to do refreshing
        if let Some(table_entry) = self.get_table_entry_from_cache(table_name) {
            //If the refresh is false indicates that user tolerate not the latest data
            if !refresh || !self.need_refresh_table_entry(&table_entry) {
                debug!(
                    "ObTableClientInner::get_or_refresh_table_entry: double check found no need \
                     to refresh, table_name:{}",
                    table_name
                );
                return Ok(table_entry);
            }
        }

        let server_size = self.server_roster.get_members().len();
        let retry_times = if self.config.table_entry_refresh_try_times > server_size {
            server_size
        } else {
            self.config.table_entry_refresh_try_times
        };

        let retry_interval = self.config.table_entry_refresh_try_interval;

        trace!(
            "ObTableClientInner::get_or_refresh_table_entry: starting refreshing, \
             table_name:{}, retry_times:{}",
            table_name,
            retry_times
        );
        for retry_num in 0..retry_times {
            let table_locations = self.table_locations.rl();
            let table_entry = table_locations.get(table_name);

            match self.refresh_table_entry(table_name, table_entry) {
                Ok(table_entry) => {
                    drop(table_locations); //release read lock
                    let mut table_locations = self.table_locations.wl();
                    table_locations.insert(table_name.to_owned(), table_entry.clone());
                    trace!(
                        "ObTableClientInner::get_or_refresh_table_entry succeed in refreshing table \
                        entry for table:{}, retry_num:{}",
                        table_name, retry_num
                    );
                    return Ok(table_entry);
                }
                e => {
                    drop(table_locations); //release read lock
                    error!(
                        "ObTableClientInner::get_or_refresh_table_entry fail to refresh table \
                         entry for table {}, error is {:?}",
                        table_name, e
                    );
                    if self
                        .table_entry_refresh_continuous_failure_count
                        .fetch_add(1, Ordering::SeqCst)
                        >= self.config.table_entry_refresh_continuous_failure_ceiling
                    {
                        self.sync_refresh_metadata()?;
                        self.table_entry_refresh_continuous_failure_count
                            .store(0, Ordering::SeqCst);
                    }
                    let interval = retry_interval * (retry_num as u32 + 1);
                    thread::sleep(interval);
                }
            }
        }

        info!("ObTableClientInner::get_or_refresh_table_entry refresh table entry has tried {}-times failure and will sync refresh metadata", retry_times);

        self.sync_refresh_metadata()?;
        self.refresh_table_entry(table_name, self.table_locations.rl().get(table_name))
    }

    fn add_row_key_element(&self, table_name: &str, columns: Vec<String>) {
        {
            let table_row_key_element = self.table_row_key_element.rl();
            if table_row_key_element.contains_key(table_name) {
                return;
            }
        }
        {
            let mut row_key_element = HashMap::with_capacity(columns.len());
            for (i, column) in columns.into_iter().enumerate() {
                row_key_element.insert(column, i as i32);
            }
            self.table_row_key_element
                .wl()
                .insert(table_name.to_owned(), row_key_element);
        }
    }

    fn get_or_create_batch_op_thread_pool(&self, table_name: &str) -> Arc<CpuPool> {
        let pools = self.table_batch_op_thread_pools.rl();
        if let Some(pool) = pools.get(table_name) {
            pool.clone()
        } else {
            drop(pools);
            let mut pools = self.table_batch_op_thread_pools.wl();
            if let Some(pool) = pools.get(table_name) {
                pool.clone()
            } else {
                let pool = Arc::new(
                    CpuPoolBuilder::new()
                        .name_prefix(format!("batch-ops-for-{}", table_name))
                        .pool_size(self.config.table_batch_op_thread_num)
                        .create(),
                );

                pools.insert(table_name.to_owned(), pool.clone());
                pool
            }
        }
    }

    fn invalidate_table(&self, table_name: &str) {
        let mutex = {
            let table_mutexs = self.table_mutexs.rl();
            match table_mutexs.get(table_name) {
                Some(mutex) => mutex.clone(),
                None => return,
            }
        };
        {
            let _lock = mutex.lock();
            self.table_locations.wl().remove(table_name);
        }
        self.table_row_key_element.wl().remove(table_name);
        self.table_continuous_failures.wl().remove(table_name);
        self.table_mutexs.wl().remove(table_name);
        self.table_batch_op_thread_pools.wl().remove(table_name);
    }

    fn execute_sql(&self, sql: &str) -> Result<()> {
        if let Some(server_addr) = self.server_roster.peek_random_server() {
            self.location.execute_sql(
                sql,
                &server_addr,
                &self.tenant_name,
                &self.user_name,
                &self.password,
                &self.database,
                self.config.rpc_operation_timeout,
            )
        } else {
            Err(CommonErr(
                CommonErrCode::NotFound,
                "active server not found".to_owned(),
            ))
        }
    }

    fn check_table_exists(&self, table_name: &str) -> Result<bool> {
        let select_sql = format!("SELECT 1 FROM {} LIMIT 1;", table_name);
        let exists = match self.execute_sql(&select_sql) {
            Ok(_) => true,
            Err(e) => {
                debug!(
                    "ObTableClientInner::check_table_exists execute sql results:{:?}",
                    e
                );
                false
            }
        };

        Ok(exists)
    }

    fn truncate_table(&self, table_name: &str) -> Result<()> {
        let truncate_table_sql = format!("truncate table {}; purge recyclebin;", table_name);
        self.execute_sql(&truncate_table_sql)
    }

    fn running_mode(&self) -> RunningMode {
        self.running_mode.clone()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    fn refresh_all_table_entries(&self) {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["refresh_all_tables"])
            .start_timer();

        let tables: Vec<String> = self
            .table_locations
            .rl()
            .keys()
            .map(|e| e.to_owned())
            .collect();

        for table_name in tables {
            if let Err(e) = self.get_or_refresh_table_entry(&table_name, true) {
                error!("ObTableClientInner::refresh_all_table_entries fail to refresh table entry for table: {}, err: {}.",
                                 table_name, e);
            }
        }
    }

    fn init(&self) -> Result<()> {
        if self.is_initialized() {
            warn!("ObTableClientInner::init already initialzied.");
            return Ok(());
        }

        let _lock = self.status_mutex.lock();
        if self.is_initialized() {
            warn!("ObTableClientInner::init already initialzied.");
            return Ok(());
        }
        self.initialized.store(true, Ordering::Release);
        self.init_metadata()
    }

    fn close(&mut self) -> Result<()> {
        if self.is_closed() {
            warn!("ObTableClientInner::close already closed.");
            return Ok(());
        }

        let _lock = self.status_mutex.lock();
        if self.is_closed() {
            warn!("ObTableClientInner::close already closed.");
            return Ok(());
        }
        self.closed.store(true, Ordering::Release);

        for (_addr, table) in self.table_roster.wl().drain() {
            drop(table);
        }

        Ok(())
    }

    fn is_already_refreshed(&self) -> bool {
        current_time_millis() - (self.last_refresh_metadata_ts.load(Ordering::Acquire) as i64)
            < duration_to_millis(&self.config.metadata_refresh_interval)
    }

    fn sync_refresh_metadata(&self) -> Result<()> {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["refresh_metadata"])
            .start_timer();

        if self.is_already_refreshed() {
            warn!("ObTableClientInner::sync_refresh_metadata try to lock metadata refreshing, it has refresh  at: {}, dataSourceName: {}, url: {}",
                  self.last_refresh_metadata_ts.load(Ordering::Acquire), self.datasource_name, self.param_url);
            return Ok(());
        }

        let _lock = self.refresh_metadata_mutex.lock();

        if self.is_already_refreshed() {
            warn!("ObTableClientInner::sync_refresh_metadata already refresh metadata at : {}, dataSourceName: {}, url: {}",
                  self.last_refresh_metadata_ts.load(Ordering::Acquire), self.datasource_name, self.param_url);
            return Ok(());
        }

        let new_ocp_model = self.ocp_manager.load_ocp_model(
            &self.param_url,
            &self.datasource_name,
            self.config.rslist_acquire_try_times,
            self.config.rslist_acquire_retry_interval,
            false,
        )?;

        self.location
            .invalidate_mysql_pools(&new_ocp_model.observer_addrs);

        *self.ocp_model.wl() = new_ocp_model;

        let root_server_key =
            TableEntryKey::new_root_server_key(&self.cluster_name, &self.tenant_name);

        let table_entry = {
            self.location.load_table_entry_randomly(
                &self.ocp_model.rl().observer_addrs,
                &root_server_key,
                self.config.table_entry_acquire_connect_timeout,
                self.config.table_entry_acquire_read_timeout,
            )?
        };

        let mut servers: Vec<ObServerAddr> = vec![];

        {
            // update table roster
            let mut table_roster = self.table_roster.wl();

            for replica_location in table_entry.table_location().replica_locations() {
                let info = replica_location.info();

                if !info.is_active() {
                    warn!("ObTableClientInner::sync_refresh_metadata will not refresh location {:?}, because it's status is {:?} and stop time is {}",
                          replica_location.addr(), info.status(), info.stop_time());
                    continue;
                }

                let addr = replica_location.addr();

                servers.push(addr.clone());

                if table_roster.contains_key(&addr) {
                    continue;
                }

                self.add_ob_table_to_roster(&addr, &mut table_roster)?;
            }

            table_roster.retain(|addr, _| {
                let valid = servers.contains(addr);
                if !valid {
                    info!(
                        "ObTableClientInner::sync_refresh_metadata clean useless obtable addr: {:?}",
                        addr
                    );
                }
                valid
            });
        }

        self.server_roster.reset(servers);
        self.last_refresh_metadata_ts
            .store(current_time_millis() as usize, Ordering::Release);

        Ok(())
    }

    fn init_metadata(&self) -> Result<()> {
        let _lock = self.refresh_metadata_mutex.lock();
        *self.ocp_model.wl() = self.ocp_manager.load_ocp_model(
            &self.param_url,
            &self.datasource_name,
            self.config.rslist_acquire_try_times,
            self.config.rslist_acquire_retry_interval,
            true,
        )?;

        let root_server_key =
            TableEntryKey::new_root_server_key(&self.cluster_name, &self.tenant_name);

        let table_entry = {
            self.location.load_table_entry_randomly(
                &self.ocp_model.rl().observer_addrs,
                &root_server_key,
                self.config.table_entry_acquire_connect_timeout,
                self.config.table_entry_acquire_read_timeout,
            )?
        };

        let mut servers: Vec<ObServerAddr> = vec![];

        for replica_location in table_entry.table_location().replica_locations().iter() {
            let info = replica_location.info();

            if !info.is_active() {
                warn!("ObTableClientInner::init_metadata will not refresh location {:?}, because it's status is {:?} and stop time is {}",
                      replica_location.addr(), info.status(), info.stop_time());
                continue;
            }

            let addr = replica_location.addr();

            servers.push(addr.clone());

            self.add_ob_table(&addr)?;
        }

        self.server_roster.reset(servers);
        self.last_refresh_metadata_ts
            .store(current_time_millis() as usize, Ordering::Release);

        Ok(())
    }

    fn check_status(&self) -> Result<()> {
        if !self.is_initialized() {
            return Err(CommonErr(
                CommonErrCode::NotInitialized,
                format!("ObTableClientInner::check_status is not initialized, param url is {}, full username is {}",
                        self.param_url, self.full_user_name),
            ));
        }

        if self.is_closed() {
            return Err(CommonErr(
                CommonErrCode::AlreadyClosed,
                format!(
                    "ObTableClientInner::check_status is closed, param url is {}, full username is {}",
                    self.param_url, self.full_user_name
                ),
            ));
        }
        Ok(())
    }

    fn execute_once(
        &self,
        table_name: &str,
        operation_type: ObTableOperationType,
        row_keys: Vec<Value>,
        columns: Option<Vec<String>>,
        properties: Option<Vec<Value>>,
    ) -> Result<ObTableOperationResult> {
        self.check_status()?;

        let (part_id, table) = self.get_table(table_name, &row_keys, false)?;

        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&[operation_type.as_str()])
            .start_timer();

        let mut payload = ObTableOperationRequest::new(
            table_name,
            operation_type,
            row_keys,
            columns,
            properties,
            self.config.rpc_operation_timeout,
            self.config.log_level_flag,
        );
        payload.set_partition_id(part_id);
        let mut result = ObTableOperationResult::new();
        table.execute_payload(&mut payload, &mut result)?;
        Ok(result)
    }

    fn execute(
        &self,
        table_name: &str,
        operation_type: ObTableOperationType,
        row_keys: Vec<Value>,
        columns: Option<Vec<String>>,
        properties: Option<Vec<Value>>,
    ) -> Result<ObTableOperationResult> {
        let mut retry_num = 0;
        loop {
            retry_num += 1;
            match self.execute_once(
                table_name,
                operation_type,
                row_keys.clone(),
                columns.clone(),
                properties.clone(),
            ) {
                Ok(result) => {
                    let error_no = result.header().errorno();
                    let result_code = ResultCodes::from_i32(error_no);
                    let result = if result_code == ResultCodes::OB_SUCCESS {
                        self.reset_table_failure(table_name);
                        Ok(result)
                    } else {
                        Err(CommonErr(
                            CommonErrCode::ObException(result_code),
                            format!(
                                "OBKV server return exception, the msg is: {}.",
                                result.header().message()
                            ),
                        ))
                    };
                    return result;
                }
                Err(e) => {
                    debug!(
                        "ObTableClientInner::execute fail to execute once, table_name:{}, \
                         op_type:{:?}, retry_num:{}, err:{}",
                        table_name, operation_type, retry_num, e
                    );
                    if let Err(fail_err) = self.on_table_op_failure(table_name, &e) {
                        error!(
                            "ObTableClientInner::execute on_table_op_failure, table_name:{}, \
                             op_type:{:?}, retry_num:{}, err:{}",
                            table_name, operation_type, retry_num, fail_err
                        );
                        return Err(e);
                    }
                    if retry_num < self.config.rpc_retry_limit && e.need_retry() {
                        OBKV_CLIENT_RETRY_COUNTER_VEC
                            .with_label_values(&["execute"])
                            .inc();

                        if self.config.rpc_retry_interval.as_secs() > 0 {
                            thread::sleep(self.config.rpc_retry_interval);
                        }
                        continue;
                    }
                    error!(
                        "ObTableClientInner::execute execute, retrying too many times, \
                         table_name:{}, op_type:{:?}, retry_num:{}, err:{}",
                        table_name, operation_type, retry_num, e
                    );
                    return Err(e);
                }
            }
        }
    }
}

impl Drop for ObTableClientInner {
    fn drop(&mut self) {
        match self.close() {
            Ok(()) => (),
            Err(e) => error!("ObTableClientInner::drop fail to close, error={:?}", e),
        }
    }
}

/// OBKV Table client
#[derive(Clone)]
pub struct ObTableClient {
    inner: Arc<ObTableClientInner>,
    refresh_thread_pool: Arc<ScheduledThreadPool>,
}

impl ObTableClient {
    /// Add row key element for table.
    pub fn add_row_key_element(&self, table_name: &str, columns: Vec<String>) {
        self.inner.add_row_key_element(table_name, columns);
    }

    /// Returns client's current running mode.
    pub fn running_mode(&self) -> RunningMode {
        self.inner.running_mode()
    }

    /// Create a TableQuery instance for table.
    pub fn query(&self, table_name: &str) -> impl TableQuery {
        ObTableClientQueryImpl::new(table_name, self.inner.clone())
    }

    pub fn truncate_table(&self, table_name: &str) -> Result<()> {
        self.inner.truncate_table(table_name)
    }

    pub fn execute_sql(&self, sql: &str) -> Result<()> {
        self.inner.execute_sql(sql)
    }

    pub fn check_table_exists(&self, table_name: &str) -> Result<bool> {
        self.inner.check_table_exists(table_name)
    }

    // Remove table entry metadata and config from client.
    pub fn invalidate_table(&self, table_name: &str) {
        self.inner.invalidate_table(table_name);
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Returns true when the client is initialized.
    pub fn is_initialized(&self) -> bool {
        self.inner.is_initialized()
    }

    /// Intialize the ob table client instance.
    pub fn init(&self) -> Result<()> {
        self.inner.init()?;
        let inner = self.inner.clone();
        self.refresh_thread_pool.execute_with_fixed_delay(
            inner.config.table_entry_refresh_interval_base,
            inner.config.table_entry_refresh_interval_ceiling,
            move || {
                inner.refresh_all_table_entries();
            },
        );

        Ok(())
    }

    pub fn get_table(
        &self,
        table_name: &str,
        row_key: &[Value],
        refresh: bool,
    ) -> Result<(i64, Arc<ObTable>)> {
        self.inner.get_table(table_name, row_key, refresh)
    }

    fn execute_batch_once(
        &self,
        table_name: &str,
        batch_op: ObTableBatchOperation,
    ) -> Result<Vec<TableOpResult>> {
        self.inner.check_status()?;

        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["execute_batch"])
            .start_timer();

        assert!(batch_op.is_raw());
        let mut batch_op = batch_op;

        OBKV_CLIENT_HISTOGRAM_NUM_VEC
            .with_label_values(&["batch_ops"])
            .observe(batch_op.get_raw_ops().len() as f64);

        let table_entry = self.inner.get_or_refresh_table_entry(table_name, false)?;

        let mut part_batch_ops = HashMap::with_capacity(1);
        for op in batch_op.take_raw_ops() {
            let partition_id = self.inner.get_partition(&table_entry, &op.1)?;
            part_batch_ops
                .entry(partition_id)
                .or_insert_with(ObTableBatchOperation::new)
                .add_op(op);
        }
        if part_batch_ops.is_empty() {
            return Ok(Vec::new());
        }

        OBKV_CLIENT_HISTOGRAM_NUM_VEC
            .with_label_values(&["partitioned_batch_ops"])
            .observe(part_batch_ops.len() as f64);

        // fast path: to process batch operations involving only one partition
        if part_batch_ops.len() == 1 {
            let (part_id, mut part_batch_op) = part_batch_ops.into_iter().next().unwrap();
            part_batch_op.set_partition_id(part_id);
            part_batch_op.set_table_name(table_name.to_owned());
            part_batch_op.set_atomic_op(batch_op.is_atomic_op());
            let (_, table) = self
                .inner
                .get_or_create_table(table_name, &table_entry, part_id)?;
            return table.execute_batch(table_name, part_batch_op);
        }

        // atomic now only support single partition
        if (part_batch_ops.len() as u32) != 1 && batch_op.is_atomic_op() {
            return Err(CommonErr(
                CommonErrCode::ObException(ResultCodes::OB_INVALID_PARTITION),
                format!(
                    "batch operation is atomic, but involves multiple partitions: {:?}",
                    batch_op
                ),
            ));
        }

        // slow path: have to process operations involving multiple partitions
        // concurrent send the batch ops by partition
        let pool = self.inner.get_or_create_batch_op_thread_pool(table_name);

        // prepare all the runners
        let mut runners = Vec::with_capacity(part_batch_ops.len());
        for (part_id, mut batch_op) in part_batch_ops {
            let (_, table) = self
                .inner
                .get_or_create_table(table_name, &table_entry, part_id)?;
            let table_name = table_name.to_owned();
            runners.push(move || {
                batch_op.set_partition_id(part_id);
                batch_op.set_table_name(table_name.clone());
                table.execute_batch(&table_name, batch_op)
            });
        }

        // join all runners into one future
        let put_all = future::join_all(runners.into_iter().map(|runner| pool.spawn_fn(runner)));

        // wait for all futures done
        let results = put_all.wait()?;
        Ok(results.into_iter().flatten().collect())
    }
}

impl Table for ObTableClient {
    #[inline]
    fn insert(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) -> Result<i64> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::Insert,
                row_keys,
                Some(columns),
                Some(properties),
            )?
            .affected_rows())
    }

    #[inline]
    fn update(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) -> Result<i64> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::Update,
                row_keys,
                Some(columns),
                Some(properties),
            )?
            .affected_rows())
    }

    #[inline]
    fn insert_or_update(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) -> Result<i64> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::InsertOrUpdate,
                row_keys,
                Some(columns),
                Some(properties),
            )?
            .affected_rows())
    }

    #[inline]
    fn replace(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) -> Result<i64> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::Replace,
                row_keys,
                Some(columns),
                Some(properties),
            )?
            .affected_rows())
    }

    #[inline]
    fn append(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) -> Result<i64> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::Append,
                row_keys,
                Some(columns),
                Some(properties),
            )?
            .affected_rows())
    }

    #[inline]
    fn increment(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
        properties: Vec<Value>,
    ) -> Result<i64> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::Increment,
                row_keys,
                Some(columns),
                Some(properties),
            )?
            .affected_rows())
    }

    #[inline]
    fn delete(&self, table_name: &str, row_keys: Vec<Value>) -> Result<i64> {
        Ok(self
            .inner
            .execute(table_name, ObTableOperationType::Del, row_keys, None, None)?
            .affected_rows())
    }

    #[inline]
    fn get(
        &self,
        table_name: &str,
        row_keys: Vec<Value>,
        columns: Vec<String>,
    ) -> Result<HashMap<String, Value>> {
        Ok(self
            .inner
            .execute(
                table_name,
                ObTableOperationType::Get,
                row_keys,
                Some(columns),
                None,
            )?
            .take_entity()
            .take_properties())
    }

    #[inline]
    fn batch_operation(&self, ops_num_hint: usize) -> ObTableBatchOperation {
        ObTableBatchOperation::with_ops_num_raw(ops_num_hint)
    }

    fn execute_batch(
        &self,
        table_name: &str,
        batch_op: ObTableBatchOperation,
    ) -> Result<Vec<TableOpResult>> {
        let mut retry_num = 0;
        loop {
            retry_num += 1;
            match self.execute_batch_once(table_name, batch_op.clone()) {
                Ok(res) => {
                    self.inner.reset_table_failure(table_name);
                    return Ok(res);
                }
                Err(e) => {
                    debug!(
                        "ObTableClientInner::execute_batch fail to execute batch once, \
                         table_name:{}, retry_num:{}, err:{}",
                        table_name, retry_num, e
                    );
                    if let Err(fail_err) = self.inner.on_table_op_failure(table_name, &e) {
                        error!(
                            "ObTableClient::execute_batch fail to call on_table_op_failure, \
                             table_name:{}, err:{}",
                            table_name, fail_err
                        );
                        return Err(e);
                    };
                    if retry_num < self.inner.config.rpc_retry_limit && e.need_retry() {
                        // TODO: add error type as label
                        OBKV_CLIENT_RETRY_COUNTER_VEC
                            .with_label_values(&["execute_batch"])
                            .inc();

                        if self.inner.config.rpc_retry_interval.as_secs() > 0 {
                            thread::sleep(self.inner.config.rpc_retry_interval);
                        }
                        continue;
                    }
                    error!(
                        "ObTableClientInner::execute_batch execute batch, retrying too many times, \
                        table_name:{}, retried_num:{}, err:{}",
                        table_name, retry_num, e
                    );
                    return Err(e);
                }
            }
        }
    }
}

struct ObTableClientStreamQuerier {
    client: Arc<ObTableClientInner>,
    table_name: String,
    start_execute_ts: AtomicI64,
}

impl ObTableClientStreamQuerier {
    fn new(table_name: &str, client: Arc<ObTableClientInner>) -> Self {
        Self {
            client,
            table_name: table_name.to_owned(),
            start_execute_ts: AtomicI64::new(0),
        }
    }
}

impl Drop for ObTableClientStreamQuerier {
    fn drop(&mut self) {
        let start_ts = self.start_execute_ts.load(Ordering::Relaxed);

        if start_ts > 0 {
            let cost_secs = millis_to_secs(current_time_millis() - start_ts);
            OBKV_CLIENT_HISTOGRAM_VEC
                .with_label_values(&["stream_querier_total_time"])
                .observe(cost_secs as f64);
        }
    }
}

impl StreamQuerier for ObTableClientStreamQuerier {
    fn execute_query(
        &self,
        stream_result: &mut QueryStreamResult,
        (part_id, ob_table): (i64, Arc<ObTable>),
        payload: &mut ObTableQueryRequest,
    ) -> Result<i64> {
        self.client.acquire_query_permit()?;

        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["execute_query"])
            .start_timer();

        self.start_execute_ts
            .store(current_time_millis(), Ordering::Relaxed);

        let mut result = ObTableQueryResult::new();
        match ob_table.execute_payload(payload, &mut result) {
            Ok(()) => self.client.reset_table_failure(&self.table_name),
            Err(e) => {
                if let Err(e) = self.client.on_table_op_failure(&self.table_name, &e) {
                    error!(
                        "ObTableClientStreamQuerier::execute_query on_table_op_failure err: {}.",
                        e
                    );
                }
                return Err(e);
            }
        }
        let row_count = result.row_count();
        OBKV_CLIENT_HISTOGRAM_NUM_VEC
            .with_label_values(&["query_rows"])
            .observe(row_count as f64);

        stream_result.cache_stream_next((part_id, ob_table), result);
        Ok(row_count)
    }

    fn execute_stream(
        &self,
        stream_result: &mut QueryStreamResult,
        (part_id, ob_table): (i64, Arc<ObTable>),
        payload: &mut ObTableStreamRequest,
    ) -> Result<i64> {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["execute_stream"])
            .start_timer();

        let is_stream_next = payload.is_stream_next();

        let mut result = ObTableQueryResult::new();
        match ob_table.execute_payload(payload, &mut result) {
            Ok(()) => self.client.reset_table_failure(&self.table_name),
            Err(e) => {
                if let Err(e) = self.client.on_table_op_failure(&self.table_name, &e) {
                    error!(
                        "ObTableClientStreamQuerier::execute_query on_table_op_failure err: {}.",
                        e
                    );
                }
                return Err(e);
            }
        }
        let row_count = result.row_count();
        OBKV_CLIENT_HISTOGRAM_NUM_VEC
            .with_label_values(&["query_rows"])
            .observe(row_count as f64);

        if is_stream_next {
            stream_result.cache_stream_next((part_id, ob_table), result);
        }
        Ok(row_count)
    }
}

/// TODO refactor with ObTableQueryImpl
struct ObTableClientQueryImpl {
    operation_timeout: Option<Duration>,
    entity_type: ObTableEntityType,
    table_name: String,
    client: Arc<ObTableClientInner>,
    table_query: ObTableQuery,
}

impl ObTableClientQueryImpl {
    fn new(table_name: &str, client: Arc<ObTableClientInner>) -> Self {
        Self {
            operation_timeout: None,
            entity_type: ObTableEntityType::Dynamic,
            table_name: table_name.to_owned(),
            client,
            table_query: ObTableQuery::new(),
        }
    }

    fn reset(&mut self) {
        self.table_query = ObTableQuery::new();
    }
}

impl TableQuery for ObTableClientQueryImpl {
    fn execute(&self) -> Result<QueryResultSet> {
        let _timer = OBKV_CLIENT_HISTOGRAM_VEC
            .with_label_values(&["query_execute"])
            .start_timer();

        let mut partition_table: HashMap<i64, (i64, Arc<ObTable>)> = HashMap::new();

        self.table_query.verify()?;

        for range in self.table_query.get_key_ranges() {
            let border_flag = range.get_border_flag();
            let pairs = self.client.get_tables(
                &self.table_name,
                range.get_start_key().keys(),
                border_flag.is_inclusive_start(),
                range.get_end_key().keys(),
                border_flag.is_inclusive_end(),
                false,
            )?;

            for (part_id, ob_table) in pairs {
                partition_table.insert(part_id, (part_id, ob_table));
            }
        }

        let mut stream_result = QueryStreamResult::new(
            Arc::new(ObTableClientStreamQuerier::new(
                &self.table_name,
                self.client.clone(),
            )),
            self.table_query.clone(),
        );

        stream_result.set_entity_type(self.entity_type());
        stream_result.set_table_name(&self.table_name);
        stream_result.set_expectant(partition_table);
        stream_result.set_operation_timeout(self.operation_timeout);
        stream_result.set_flag(self.client.config.log_level_flag);
        stream_result.init()?;

        Ok(QueryResultSet::from_stream_result(stream_result))
    }

    #[inline]
    fn get_table_name(&self) -> String {
        self.table_name.to_owned()
    }

    #[inline]
    fn set_entity_type(&mut self, entity_type: ObTableEntityType) {
        self.entity_type = entity_type;
    }

    #[inline]
    fn entity_type(&self) -> ObTableEntityType {
        self.entity_type
    }

    #[inline]
    fn select(mut self, columns: Vec<String>) -> Self
    where
        Self: Sized,
    {
        self.table_query.select_columns(columns);
        self
    }

    #[inline]
    fn limit(mut self, offset: Option<i32>, limit: i32) -> Self
    where
        Self: Sized,
    {
        if let Some(v) = offset {
            self.table_query.set_offset(v);
        }
        self.table_query.set_limit(limit);
        self
    }

    fn add_scan_range(
        mut self,
        start: Vec<Value>,
        start_equals: bool,
        end: Vec<Value>,
        end_equals: bool,
    ) -> Self
    where
        Self: Sized,
    {
        let mut range = ObNewRange::from_keys(start, end);
        if start_equals {
            range.set_inclusive_start();
        } else {
            range.unset_inclusive_start();
        }

        if end_equals {
            range.set_inclusive_end();
        } else {
            range.unset_inclusive_end();
        }

        self.table_query.add_key_range(range);
        self
    }

    fn add_scan_range_starts_with(mut self, start: Vec<Value>, start_equals: bool) -> Self
    where
        Self: Sized,
    {
        let mut end = Vec::with_capacity(start.len());

        for _ in 0..start.len() {
            end.push(Value::get_max());
        }

        let mut range = ObNewRange::from_keys(start, end);

        if start_equals {
            range.set_inclusive_start();
        } else {
            range.unset_inclusive_start();
        }

        self.table_query.add_key_range(range);
        self
    }

    fn add_scan_range_ends_with(mut self, end: Vec<Value>, end_equals: bool) -> Self
    where
        Self: Sized,
    {
        let mut start = Vec::with_capacity(end.len());

        for _ in 0..end.len() {
            start.push(Value::get_min());
        }

        let mut range = ObNewRange::from_keys(start, end);

        if end_equals {
            range.set_inclusive_end();
        } else {
            range.unset_inclusive_end();
        }

        self.table_query.add_key_range(range);
        self
    }

    #[inline]
    fn scan_order(mut self, forward: bool) -> Self
    where
        Self: Sized,
    {
        self.table_query
            .set_scan_order(ObScanOrder::from_bool(forward));
        self
    }

    #[inline]
    fn index_name(mut self, index_name: &str) -> Self
    where
        Self: Sized,
    {
        self.table_query.set_index_name(index_name.to_owned());
        self
    }

    #[inline]
    fn filter_string(mut self, filter_string: &str) -> Self
    where
        Self: Sized,
    {
        self.table_query.set_filter_string(filter_string.to_owned());
        self
    }

    #[inline]
    fn htable_filter(mut self, filter: ObHTableFilter) -> Self
    where
        Self: Sized,
    {
        self.table_query.set_htable_filter(filter);
        self
    }

    #[inline]
    fn batch_size(mut self, batch_size: i32) -> Self
    where
        Self: Sized,
    {
        self.table_query.set_batch_size(batch_size);
        self
    }

    #[inline]
    fn operation_timeout(mut self, timeout: Duration) -> Self
    where
        Self: Sized,
    {
        self.operation_timeout = Some(timeout);
        self
    }

    #[inline]
    fn clear(&mut self) {
        self.reset();
    }
}

/// ObTableClient builder
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Builder {
    full_user_name: String,
    param_url: String,
    password: String,
    user_name: String,
    tenant_name: String,
    cluster_name: String,
    database: String,
    running_mode: RunningMode,
    config: ClientConfig,
}

const USER_NAME_SEPERATORS: &[&str] = &[":", "-", "."];
const DATABASE_PARAM_KEY: &str = "database";

impl Builder {
    pub fn new() -> Self {
        Self {
            full_user_name: "".to_owned(),
            param_url: "".to_owned(),
            password: "".to_owned(),
            user_name: "".to_owned(),
            tenant_name: "".to_owned(),
            cluster_name: "".to_owned(),
            database: "".to_owned(),
            running_mode: RunningMode::Normal,
            config: ClientConfig::default(),
        }
    }

    fn parse_non_standard_full_user_name(&mut self, username: &str) {
        let mut ut_index = -1;
        let mut tc_index = -1;
        for sep in USER_NAME_SEPERATORS {
            if let Some(pos) = username.find(sep) {
                ut_index = pos as i32;
            }
            if let Some(pos) = username.rfind(sep) {
                tc_index = pos as i32;
            }

            if ut_index != tc_index {
                break;
            }
        }

        assert!(
            ut_index != -1 && tc_index != -1 && ut_index != tc_index,
            "Invalid full username"
        );

        let ut_index: usize = ut_index as usize;
        let tc_index: usize = tc_index as usize;

        let cluster_name = &username[0..ut_index];
        let tenant_name = &username[(ut_index + 1)..tc_index];
        let user = &username[(tc_index + 1)..];

        assert_not_empty(cluster_name, "Blank cluster name");
        assert_not_empty(tenant_name, "Blank tenant name");
        assert_not_empty(user, "Blank user name");

        self.cluster_name = cluster_name.to_owned();
        self.tenant_name = tenant_name.to_owned();
        self.user_name = user.to_owned();
    }

    fn parse_standard_full_user_name(&mut self, username: &str) {
        let ut_index = username.find('@').unwrap();
        let tc_index = username.find('#').unwrap();

        assert!(ut_index < tc_index, "Invalid full user name.");

        let user = &username[0..ut_index];
        let tenant_name = &username[(ut_index + 1)..tc_index];
        let cluster_name = &username[(tc_index + 1)..];

        assert_not_empty(cluster_name, "Blank cluster name");
        assert_not_empty(tenant_name, "Blank tenant name");
        assert_not_empty(user, "Blank user name");

        self.cluster_name = cluster_name.to_owned();
        self.tenant_name = tenant_name.to_owned();
        self.user_name = user.to_owned();
    }

    pub fn running_mode(mut self, mode: RunningMode) -> Self {
        self.running_mode = mode;
        self
    }

    pub fn full_user_name(mut self, name: &str) -> Self {
        assert_not_empty(name, "Blank full user name");

        if name.find('@').is_some() || name.find('#').is_some() {
            self.parse_standard_full_user_name(name);
        } else {
            self.parse_non_standard_full_user_name(name);
        }

        self.full_user_name = name.to_owned();
        self
    }

    pub fn password(mut self, pwd: &str) -> Self {
        self.password = pwd.to_owned();
        self
    }

    pub fn config(mut self, config: ClientConfig) -> Self {
        self.config = config;
        self
    }

    pub fn param_url(mut self, url: &str) -> Self {
        assert_not_empty(url, "Blank param url");

        let params = if let Some(param_index) = url.find('?') {
            if param_index + 1 == url.len() {
                panic!("Missing parameters after '?' in url");
            } else {
                &url[(param_index + 1)..]
            }
        } else {
            panic!("Missing parameters in url.");
        };

        let mut db = "";

        for param in params.split('&') {
            let kv: Vec<&str> = param.split('=').collect();

            assert!(kv.len() == 2, "Invalid param");

            if kv.first().unwrap().to_lowercase() == DATABASE_PARAM_KEY {
                db = &kv[1];
            }
        }

        assert_not_empty(db, "blank database.");

        self.database = db.to_owned();
        self.param_url = url.to_owned();
        self
    }

    pub fn sys_user_name(mut self, name: &str) -> Self {
        self.config.sys_user_name = name.to_owned();
        self
    }

    pub fn sys_password(mut self, pwd: &str) -> Self {
        self.config.sys_password = pwd.to_owned();
        self
    }

    pub fn build(self) -> Result<ObTableClient> {
        assert_not_empty(&self.param_url, "Blank param url");
        assert_not_empty(&self.full_user_name, "Blank full user name");

        Ok(ObTableClient {
            inner: Arc::new(ObTableClientInner::internal_new(
                self.param_url,
                self.full_user_name,
                self.password,
                self.user_name,
                self.tenant_name,
                self.cluster_name,
                self.database,
                self.running_mode,
                self.config,
            )?),
            refresh_thread_pool: Arc::new(ScheduledThreadPool::with_name(
                "ObTableClient-RefreshMetadata-Thread-",
                2,
            )),
        })
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}
