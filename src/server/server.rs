// Copyright 2016 PingCAP, Inc.
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

use std::i32;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::Stream;
use grpc::{ChannelBuilder, EnvBuilder, Environment, Server as GrpcServer, ServerBuilder};
use kvproto::debugpb_grpc::create_debug;
use kvproto::import_sstpb_grpc::create_import_sst;
use kvproto::tikvpb_grpc::*;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};
use tokio::timer::Interval;

use coprocessor::Endpoint;
use import::ImportSSTService;
use raftstore::store::{Engines, SnapManager};
use storage::{Engine, Storage};
use util::security::SecurityManager;
use util::worker::Worker;

use super::load_statistics::*;
use super::raft_client::RaftClient;
use super::resolve::StoreAddrResolver;
use super::service::*;
use super::snap::{Runner as SnapHandler, Task as SnapTask};
use super::transport::{RaftStoreRouter, ServerTransport};
use super::{Config, Result};

const LOAD_STATISTICS_SLOTS: usize = 4;
const LOAD_STATISTICS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_GRPC_RECV_MSG_LEN: i32 = 10 * 1024 * 1024;
pub const GRPC_THREAD_PREFIX: &str = "grpc-server";
pub const STATS_THREAD_PREFIX: &str = "transport-stats";

pub struct Server<T: RaftStoreRouter + 'static, S: StoreAddrResolver + 'static> {
    env: Arc<Environment>,
    // Grpc server.
    grpc_server: GrpcServer,
    local_addr: SocketAddr,
    // Transport.
    trans: ServerTransport<T, S>,
    raft_router: T,
    // For sending/receiving snapshots.
    snap_mgr: SnapManager,
    snap_worker: Worker<SnapTask>,

    // Currently load statistics is done in the thread.
    stats_runtime: Arc<Runtime>,
    thread_load: Arc<ThreadLoad>,
}

impl<T: RaftStoreRouter, S: StoreAddrResolver + 'static> Server<T, S> {
    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn new<E: Engine>(
        cfg: &Arc<Config>,
        security_mgr: &Arc<SecurityManager>,
        storage: Storage<E>,
        cop: Endpoint<E>,
        raft_router: T,
        resolver: S,
        snap_mgr: SnapManager,
        debug_engines: Option<Engines>,
        import_service: Option<ImportSSTService<T>>,
    ) -> Result<Self> {
        // A helper thread (or pool) for transport layer.
        let stats_runtime = Arc::new(
            RuntimeBuilder::new()
                .core_threads(cfg.as_ref().stats_concurrency)
                .name_prefix(STATS_THREAD_PREFIX)
                .build()
                .unwrap(),
        );
        let thread_load = Arc::new(ThreadLoad::with_threshold(cfg.heavy_load_threshold));

        let env = Arc::new(
            EnvBuilder::new()
                .cq_count(cfg.grpc_concurrency)
                .name_prefix(thd_name!(GRPC_THREAD_PREFIX))
                .build(),
        );

        let snap_worker = Worker::new("snap-handler");

        let kv_service = KvService::new(storage, cop, raft_router.clone(), snap_worker.scheduler());
        let addr = SocketAddr::from_str(&cfg.addr)?;
        info!("listening on {}", addr);
        let ip = format!("{}", addr.ip());
        let channel_args = ChannelBuilder::new(Arc::clone(&env))
            .stream_initial_window_size(cfg.grpc_stream_initial_window_size.0 as i32)
            .max_concurrent_stream(cfg.grpc_concurrent_stream)
            .max_receive_message_len(MAX_GRPC_RECV_MSG_LEN)
            .max_send_message_len(-1)
            .build_args();
        let grpc_server = {
            let mut sb = ServerBuilder::new(Arc::clone(&env))
                .channel_args(channel_args)
                .register_service(create_tikv(kv_service));
            sb = security_mgr.bind(sb, &ip, addr.port());
            if let Some(engines) = debug_engines {
                let debug_service = DebugService::new(engines, raft_router.clone());
                sb = sb.register_service(create_debug(debug_service));
            }
            if let Some(service) = import_service {
                sb = sb.register_service(create_import_sst(service));
            }
            sb.build()?
        };

        let addr = {
            let (ref host, port) = grpc_server.bind_addrs()[0];
            SocketAddr::new(IpAddr::from_str(host)?, port as u16)
        };

        let raft_client = Arc::new(RwLock::new(RaftClient::new(
            Arc::clone(&env),
            Arc::clone(cfg),
            Arc::clone(security_mgr),
        )));

        let trans = ServerTransport::new(
            raft_client,
            snap_worker.scheduler(),
            raft_router.clone(),
            resolver,
        );

        let svr = Server {
            env: Arc::clone(&env),
            grpc_server,
            local_addr: addr,
            trans,
            raft_router,
            snap_mgr,
            snap_worker,
            stats_runtime,
            thread_load,
        };

        Ok(svr)
    }

    pub fn transport(&self) -> ServerTransport<T, S> {
        self.trans.clone()
    }

    pub fn start(&mut self, cfg: Arc<Config>, security_mgr: Arc<SecurityManager>) -> Result<()> {
        let snap_runner = SnapHandler::new(
            Arc::clone(&self.env),
            self.snap_mgr.clone(),
            self.raft_router.clone(),
            security_mgr,
            Arc::clone(&cfg),
        );
        box_try!(self.snap_worker.start(snap_runner));
        self.grpc_server.start();

        let mut load_stats = {
            let tl = Arc::clone(&self.thread_load);
            ThreadLoadStatistics::new(LOAD_STATISTICS_SLOTS, GRPC_THREAD_PREFIX, tl)
        };
        self.stats_runtime.executor().spawn(
            Interval::new(Instant::now(), LOAD_STATISTICS_INTERVAL)
                .map_err(|_| ())
                .for_each(move |i| {
                    load_stats.record(i);
                    Ok(())
                }),
        );

        info!("TiKV is ready to serve");
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.snap_worker.stop();
        self.grpc_server.shutdown();
        Ok(())
    }

    // Return listening address, this may only be used for outer test
    // to get the real address because we may use "127.0.0.1:0"
    // in test to avoid port conflict.
    pub fn listening_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::*;
    use std::sync::mpsc::*;
    use std::sync::*;
    use std::time::Duration;

    use super::*;

    use super::super::resolve::{Callback as ResolveCallback, StoreAddrResolver};
    use super::super::transport::RaftStoreRouter;
    use super::super::{Config, Result};
    use coprocessor;
    use kvproto::raft_serverpb::RaftMessage;
    use raftstore::store::transport::Transport;
    use raftstore::store::Msg as StoreMsg;
    use raftstore::store::*;
    use raftstore::Result as RaftStoreResult;
    use server::readpool::{self, ReadPool};
    use storage::TestStorageBuilder;
    use util::security::SecurityConfig;
    use util::worker::FutureWorker;

    #[derive(Clone)]
    struct MockResolver {
        quick_fail: Arc<AtomicBool>,
        addr: Arc<Mutex<Option<String>>>,
    }

    impl StoreAddrResolver for MockResolver {
        fn resolve(&self, _: u64, cb: ResolveCallback) -> Result<()> {
            if self.quick_fail.load(Ordering::SeqCst) {
                return Err(box_err!("quick fail"));
            }
            let addr = self.addr.lock().unwrap();
            cb(addr
                .as_ref()
                .map(|s| s.to_owned())
                .ok_or(box_err!("not set")));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestRaftStoreRouter {
        tx: Sender<usize>,
        significant_msg_sender: Sender<SignificantMsg>,
    }

    impl RaftStoreRouter for TestRaftStoreRouter {
        fn send(&self, _: StoreMsg) -> RaftStoreResult<()> {
            self.tx.send(1).unwrap();
            Ok(())
        }

        fn try_send(&self, _: StoreMsg) -> RaftStoreResult<()> {
            self.tx.send(1).unwrap();
            Ok(())
        }

        fn significant_send(&self, msg: SignificantMsg) -> RaftStoreResult<()> {
            self.significant_msg_sender.send(msg).unwrap();
            Ok(())
        }
    }

    fn is_unreachable_to(msg: &SignificantMsg, region_id: u64, to_peer_id: u64) -> bool {
        *msg == SignificantMsg::Unreachable {
            region_id,
            to_peer_id,
        }
    }

    #[test]
    // if this failed, unset the environmental variables 'http_proxy' and 'https_proxy', and retry.
    fn test_peer_resolve() {
        let mut cfg = Config::default();
        cfg.addr = "127.0.0.1:0".to_owned();

        let storage = TestStorageBuilder::new().build().unwrap();

        let (tx, rx) = mpsc::channel();
        let (significant_msg_sender, significant_msg_receiver) = mpsc::channel();
        let router = TestRaftStoreRouter {
            tx,
            significant_msg_sender,
        };

        let quick_fail = Arc::new(AtomicBool::new(false));
        let cfg = Arc::new(cfg);
        let security_mgr = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());

        let pd_worker = FutureWorker::new("test-pd-worker");
        let cop_read_pool = ReadPool::new(
            "cop-readpool",
            &readpool::Config::default_for_test(),
            || || coprocessor::ReadPoolContext::new(pd_worker.scheduler()),
        );
        let cop = coprocessor::Endpoint::new(&cfg, storage.get_engine(), cop_read_pool);

        let addr = Arc::new(Mutex::new(None));
        let mut server = Server::new(
            &cfg,
            &security_mgr,
            storage,
            cop,
            router,
            MockResolver {
                quick_fail: Arc::clone(&quick_fail),
                addr: Arc::clone(&addr),
            },
            SnapManager::new("", None),
            None,
            None,
        ).unwrap();

        server.start(cfg, security_mgr).unwrap();

        let mut trans = server.transport();
        trans.report_unreachable(RaftMessage::new());
        let mut resp = significant_msg_receiver.try_recv().unwrap();
        assert!(is_unreachable_to(&resp, 0, 0), "{:?}", resp);

        let mut msg = RaftMessage::new();
        msg.set_region_id(1);
        trans.send(msg.clone()).unwrap();
        trans.flush();
        resp = significant_msg_receiver.try_recv().unwrap();
        assert!(is_unreachable_to(&resp, 1, 0), "{:?}", resp);

        *addr.lock().unwrap() = Some(format!("{}", server.listening_addr()));

        trans.send(msg.clone()).unwrap();
        trans.flush();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());

        msg.mut_to_peer().set_store_id(2);
        msg.set_region_id(2);
        quick_fail.store(true, Ordering::SeqCst);
        trans.send(msg.clone()).unwrap();
        trans.flush();
        resp = significant_msg_receiver.try_recv().unwrap();
        assert!(is_unreachable_to(&resp, 2, 0), "{:?}", resp);
        server.stop().unwrap();
    }
}
