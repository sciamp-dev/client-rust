// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::{future, Future};
use grpc::{CallOption, EnvBuilder};
use kvproto::metapb;
use kvproto::pdpb::{GetRegionByIDRequest, GetRegionRequest, GetStoreRequest, GetAllStoresRequest, Member};

use super::leader::{check_resp_header, validate_endpoints, LeaderClient, Request};
use super::{Error, PdClient, RegionInfo, Result, PD_REQUEST_HISTOGRAM_VEC, REQUEST_TIMEOUT};
use futures::sync::oneshot;
use pd::{PdFuture, PdTimestamp};
use util::security::SecurityManager;
use util::time::duration_to_sec;
use util::HandyRwLock;

const CQ_COUNT: usize = 1;
const CLIENT_PREFIX: &str = "pd";
const LEADER_CHANGE_RETRY: usize = 10;

macro_rules! thd_name {
    ($name:expr) => {{
        $crate::util::get_tag_from_thread_name()
            .map(|tag| format!("{}::{}", $name, tag))
            .unwrap_or_else(|| $name.to_owned())
    }};
}

pub struct PdRpcClient {
    cluster_id: u64,
    leader: Arc<RwLock<LeaderClient>>,
}

impl PdRpcClient {
    pub fn new(endpoints: &[&str], security_mgr: Arc<SecurityManager>) -> Result<PdRpcClient> {
        let env = Arc::new(
            EnvBuilder::new()
                .cq_count(CQ_COUNT)
                .name_prefix(thd_name!(CLIENT_PREFIX))
                .build(),
        );
        let (client, members) = validate_endpoints(&env, endpoints, &security_mgr)?;

        Ok(PdRpcClient {
            cluster_id: members.get_header().get_cluster_id(),
            leader: LeaderClient::new(env, security_mgr, client, members),
        })
    }

    pub fn get_cluster_id(&self) -> Result<u64> {
        Ok(self.cluster_id)
    }

    pub fn on_reconnect(&self, f: Box<Fn() + Sync + Send + 'static>) {
        let mut leader = self.leader.wl();
        leader.on_reconnect = Some(f);
    }

    pub fn get_cluster_config_async(&self) -> impl Future<Item = metapb::Cluster, Error = Error> {
        future::ok(metapb::Cluster::new())
    }

    pub fn get_all_stores_async(&self) -> impl Future<Item = Vec<metapb::Store>, Error = Error> {
        let timer = Instant::now();

        let executor = move |client: &RwLock<LeaderClient>, req: GetAllStoresRequest| {
            let receiver = client
                .rl()
                .client
                .get_all_stores_async_opt(&req, Self::call_option())
                .unwrap();
            Box::new(receiver.map_err(Error::Grpc).and_then(move |mut resp| {
                PD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["get_all_stores"])
                    .observe(duration_to_sec(timer.elapsed()));
                check_resp_header(resp.get_header())?;
                Ok(resp.take_stores().into_vec())
            })) as PdFuture<_>
        };

        self.request(request!(self.cluster_id, GetAllStoresRequest), executor, LEADER_CHANGE_RETRY)
    }

    pub fn get_store_async(
        &self,
        store_id: u64,
    ) -> impl Future<Item = metapb::Store, Error = Error> {
        let timer = Instant::now();

        let mut req = request!(self.cluster_id, GetStoreRequest);
        req.set_store_id(store_id);

        let executor = move |client: &RwLock<LeaderClient>, req: GetStoreRequest| {
            let receiver = client
                .rl()
                .client
                .get_store_async_opt(&req, Self::call_option())
                .unwrap();
            Box::new(receiver.map_err(Error::Grpc).and_then(move |mut resp| {
                PD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["get_store"])
                    .observe(duration_to_sec(timer.elapsed()));
                check_resp_header(resp.get_header())?;
                Ok(resp.take_store())
            })) as PdFuture<_>
        };

        self.request(req, executor, LEADER_CHANGE_RETRY)
    }

    pub fn get_region_async(
        &self,
        key: &[u8],
    ) -> impl Future<Item = metapb::Region, Error = Error> {
        self.get_region_and_leader_async(key).and_then(|x| Ok(x.0))
    }

    pub fn get_region_info_async(
        &self,
        key: &[u8],
    ) -> impl Future<Item = RegionInfo, Error = Error> {
        self.get_region_and_leader_async(key)
            .and_then(|x| Ok(RegionInfo::new(x.0, x.1)))
    }

    pub fn get_region_by_id_async(
        &self,
        region_id: u64,
    ) -> impl Future<Item = Option<metapb::Region>, Error = Error> {
        let timer = Instant::now();

        let mut req = request!(self.cluster_id, GetRegionByIDRequest);
        req.set_region_id(region_id);

        let executor = move |client: &RwLock<LeaderClient>, req: GetRegionByIDRequest| {
            let handler = client
                .rl()
                .client
                .get_region_by_id_async_opt(&req, Self::call_option())
                .unwrap();
            Box::new(handler.map_err(Error::Grpc).and_then(move |mut resp| {
                PD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["get_region_by_id"])
                    .observe(duration_to_sec(timer.elapsed()));
                check_resp_header(resp.get_header())?;
                if resp.has_region() {
                    Ok(Some(resp.take_region()))
                } else {
                    Ok(None)
                }
            })) as PdFuture<_>
        };

        self.request(req, executor, LEADER_CHANGE_RETRY)
    }

    pub fn get_ts_async(&self) -> impl Future<Item = PdTimestamp, Error = Error> {
        self.leader.wl().get_ts()
    }

    fn get_leader(&self) -> Member {
        self.leader.rl().members.get_leader().clone()
    }

    #[inline]
    fn call_option() -> CallOption {
        CallOption::default().timeout(Duration::from_secs(REQUEST_TIMEOUT))
    }

    fn get_region_and_leader_async(
        &self,
        key: &[u8],
    ) -> impl Future<Item = (metapb::Region, Option<metapb::Peer>), Error = Error> {
        let timer = Instant::now();

        let mut req = request!(self.cluster_id, GetRegionRequest);
        req.set_region_key(key.to_owned());

        let executor = move |client: &RwLock<LeaderClient>, req: GetRegionRequest| {
            let receiver = client
                .rl()
                .client
                .get_region_async_opt(&req, Self::call_option())
                .unwrap();
            let key = req.get_region_key().to_owned();
            Box::new(receiver.map_err(Error::Grpc).and_then(move |mut resp| {
                PD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["get_region"])
                    .observe(duration_to_sec(timer.elapsed()));
                check_resp_header(resp.get_header())?;
                let region = if resp.has_region() {
                    resp.take_region()
                } else {
                    return Err(Error::RegionNotFound(key));
                };
                let leader = if resp.has_leader() {
                    Some(resp.take_leader())
                } else {
                    None
                };
                Ok((region, leader))
            })) as PdFuture<_>
        };
        self.request(req, executor, LEADER_CHANGE_RETRY)
    }

    fn request<Req, Resp, F>(
        &self,
        req: Req,
        func: F,
        retry: usize,
    ) -> impl Future<Item = Resp, Error = Error>
    where
        Req: Clone + Send + 'static,
        Resp: Send + fmt::Debug + 'static,
        F: FnMut(&RwLock<LeaderClient>, Req) -> PdFuture<Resp> + Send + 'static,
    {
        let future = Request::new(req, func, Arc::clone(&self.leader), retry).execute();
        let (tx, rx) = oneshot::channel();
        let future = Box::new(
            future
                .and_then(move |resp| {
                    tx.send(resp).unwrap();
                    Ok(())
                }).map_err(|e| panic!("{}", e)),
        );
        self.leader.wl().schedule(future);
        rx.map_err(Error::Canceled).and_then(Ok)
    }
}

impl PdClient for PdRpcClient {
    fn get_cluster_id(&self) -> Result<u64> {
        Ok(self.cluster_id)
    }

    fn get_store(&self, store_id: u64) -> Result<metapb::Store> {
        self.get_store_async(store_id).wait()
    }

    fn get_all_stores(&self) -> Result<Vec<metapb::Store>> {
        self.get_all_stores_async().wait()
    }

    fn get_cluster_config(&self) -> Result<metapb::Cluster> {
        self.get_cluster_config_async().wait()
    }

    fn get_region(&self, key: &[u8]) -> Result<metapb::Region> {
        self.get_region_async(key).wait()
    }

    fn get_region_info(&self, key: &[u8]) -> Result<RegionInfo> {
        self.get_region_info_async(key).wait()
    }

    fn get_region_by_id(&self, region_id: u64) -> Result<Option<metapb::Region>> {
        self.get_region_by_id_async(region_id).wait()
    }

    fn get_ts(&self) -> Result<PdTimestamp> {
        self.get_ts_async().wait()
    }
}

impl fmt::Debug for PdRpcClient {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("PdRpcClient")
            .field("cluster_id", &self.cluster_id)
            .field("leader", &self.get_leader())
            .finish()
    }
}