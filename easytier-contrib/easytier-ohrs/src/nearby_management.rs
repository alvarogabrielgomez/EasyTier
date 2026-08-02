//! HarmonyOS nearby transport for the canonical EasyTier management RPC surface.
//!
//! Cross-device payloads are native EasyTier `ZCPacket`s. The host-facing side
//! connects to a Unix-domain standalone RPC server owned by the process that
//! actually runs the EasyTier instance, so a VPN Extension never accidentally
//! exposes the empty `InstanceManager` loaded by `EntryAbility`.

use std::{
    collections::HashMap,
    fmt, io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use async_trait::async_trait;
use bytes::BytesMut;
use easytier::{
    common::config::{ConfigLoader as _, NetworkConfigExt as _, TomlConfigLoader},
    instance::factory::NativeInstanceFactory,
    rpc_service::logger::NativeLoggerControl,
};
use easytier_core::{
    config::{api::network_config_from_toml, toml::TomlConfig},
    management::{
        ConfigFileControl, ConfigFilePermission, ConfigFileStorage, InstanceMutationHooks,
        LoggerManagementRpc, ProcessManagementRpc, register_instance_management_rpc,
    },
    packet::{PacketType, ZCPacket, ZCPacketType},
    rpc::{bidirect::BidirectRpcManager, client::Client, standalone::StandAloneServer},
    socket::{SocketListener, tcp::VirtualTcpSocket},
    tunnel::{SplitTunnel, Tunnel, tcp::TcpTunnelUpgrader},
};
use easytier_proto::{
    api::{
        config::{ConfigRpc, ConfigRpcClientFactory},
        instance::{
            AclManageRpc, AclManageRpcClientFactory, ConnectorManageRpc,
            ConnectorManageRpcClientFactory, CredentialManageRpc, CredentialManageRpcClientFactory,
            MappedListenerManageRpc, MappedListenerManageRpcClientFactory, PeerManageRpc,
            PeerManageRpcClientFactory, PortForwardManageRpc, PortForwardManageRpcClientFactory,
            StatsRpc, StatsRpcClientFactory, TcpProxyRpc, TcpProxyRpcClientFactory, VpnPortalRpc,
            VpnPortalRpcClientFactory,
        },
        logger::{LoggerRpc, LoggerRpcClientFactory, LoggerRpcServer},
        manage::{
            CollectNetworkInfoRequest, CollectNetworkInfoResponse, DeleteNetworkInstanceRequest,
            DeleteNetworkInstanceResponse, GetNetworkInstanceConfigRequest,
            GetNetworkInstanceConfigResponse, ListNetworkInstanceMetaRequest,
            ListNetworkInstanceMetaResponse, ListNetworkInstanceRequest,
            ListNetworkInstanceResponse, RetainNetworkInstanceRequest,
            RetainNetworkInstanceResponse, RunNetworkInstanceRequest, RunNetworkInstanceResponse,
            ValidateConfigRequest, ValidateConfigResponse, WebClientService,
            WebClientServiceClientFactory, WebClientServiceServer,
        },
    },
    common::TunnelInfo,
    peer_rpc::{PeerCenterRpc, PeerCenterRpcClientFactory},
    rpc_types::{controller::BaseController, error::Error as RpcError},
};
use futures::{SinkExt, StreamExt};
use napi_ohos::bindgen_prelude::Uint8Array;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{UnixListener, UnixStream},
    sync::mpsc::{Receiver, Sender, channel},
    task::JoinHandle,
};
use url::Url;
use uuid::Uuid;

use crate::{
    ASYNC_RUNTIME, INSTANCE_MANAGER,
    config::{
        repository::{
            cache_runtime_config_snapshot, config_root_dir, get_runtime_config_snapshot,
            load_config_json, save_config_record,
        },
        storage::config_meta::get_config_display_name,
    },
};

const MAX_NEARBY_SESSIONS: usize = 8;
const MAX_SESSION_KEY_LENGTH: usize = 128;
const MAX_PACKET_BYTES: usize = 64 * 1024;
const MAX_JSON_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_DRAIN_PACKETS: usize = 256;
const PACKET_QUEUE_CAPACITY: usize = 256;
const RPC_PEER_ID: u32 = 1;
const MANAGEMENT_SOCKET_FILE_NAME: &str = "easytier-nearby-management.sock";
const MANAGEMENT_CONFIG_DIR_NAME: &str = ".easytier-nearby-management";

#[derive(Clone, Copy, PartialEq, Eq)]
enum NearbyRole {
    Client,
    HostProxy,
}

struct NearbyUnixSocket {
    stream: UnixStream,
}

impl NearbyUnixSocket {
    fn new(stream: UnixStream) -> Self {
        Self { stream }
    }
}

impl AsyncRead for NearbyUnixSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for NearbyUnixSocket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

impl VirtualTcpSocket for NearbyUnixSocket {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1)))
    }

    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 2)))
    }

    fn transport_label(&self) -> Option<&str> {
        Some("unix")
    }
}

struct NearbyUnixListener {
    socket_path: PathBuf,
    listener: Option<UnixListener>,
}

impl NearbyUnixListener {
    fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            listener: None,
        }
    }
}

impl fmt::Debug for NearbyUnixListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NearbyUnixListener")
            .field("socket_path", &self.socket_path)
            .field("listening", &self.listener.is_some())
            .finish()
    }
}

#[async_trait]
impl SocketListener for NearbyUnixListener {
    type Accepted = Box<dyn Tunnel>;

    async fn listen(&mut self) -> anyhow::Result<()> {
        if self.listener.is_some() {
            return Ok(());
        }
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        }
        self.listener = Some(listener);
        Ok(())
    }

    async fn accept(&mut self) -> anyhow::Result<Self::Accepted> {
        let listener = self
            .listener
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nearby management listener is not started"))?;
        let (stream, _) = listener.accept().await?;
        let tunnel_info = TunnelInfo {
            tunnel_type: "unix".to_owned(),
            local_addr: Some(self.local_url().into()),
            remote_addr: None,
            resolved_remote_addr: None,
        };
        Ok(TcpTunnelUpgrader::new(tunnel_info).upgrade(NearbyUnixSocket::new(stream))?)
    }

    fn local_url(&self) -> Url {
        management_socket_url(&self.socket_path)
    }
}

impl Drop for NearbyUnixListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

struct NearbyConfigStorage;

#[async_trait]
impl ConfigFileStorage for NearbyConfigStorage {
    async fn inspect(&self, path: &Path) -> ConfigFileControl {
        match config_id_from_management_path(path) {
            Ok(_) => ConfigFileControl::new(
                Some(path.to_owned()),
                ConfigFilePermission::from(ConfigFilePermission::NO_DELETE),
            ),
            Err(_) => ConfigFileControl::new(
                Some(path.to_owned()),
                ConfigFilePermission::from(
                    ConfigFilePermission::READ_ONLY | ConfigFilePermission::NO_DELETE,
                ),
            ),
        }
    }

    async fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        let config_id = config_id_from_management_path(path)?;
        let Some(raw) = load_config_json(&config_id.to_string()) else {
            return Ok(None);
        };
        let config = serde_json::from_str::<easytier_proto::api::manage::NetworkConfig>(&raw)?;
        Ok(Some(config.gen_config()?.dump().into_bytes()))
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        let config_id = config_id_from_management_path(path)?;
        let toml = std::str::from_utf8(contents)?;
        let config = TomlConfigLoader::new_from_str(toml)?;
        if config.get_id() != config_id {
            anyhow::bail!("nearby management cannot change the running instance ID");
        }
        let network_config = network_config_from_toml(&config);
        let display_name = get_config_display_name(&config_id.to_string())
            .or_else(|| {
                get_runtime_config_snapshot(&config_id.to_string())
                    .map(|snapshot| snapshot.display_name)
            })
            .unwrap_or_else(|| config.get_inst_name());
        let raw = serde_json::to_string(&network_config)?;
        save_config_record(config_id.to_string(), display_name, raw)
            .ok_or_else(|| anyhow::anyhow!("failed to persist nearby management config"))?;
        Ok(())
    }

    async fn remove(&self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("nearby management never deletes a saved HarmonyOS config")
    }
}

struct NearbyMutationHooks;

#[async_trait]
impl InstanceMutationHooks for NearbyMutationHooks {
    async fn pre_run_network_instance(&self, config: &TomlConfig) -> Result<(), String> {
        let instance_ids = INSTANCE_MANAGER.instance_ids();
        if instance_ids.len() != 1 || instance_ids[0] != config.get_id() {
            return Err(
                "nearby management may only overwrite the single running HarmonyOS instance"
                    .to_owned(),
            );
        }
        Ok(())
    }

    async fn post_run_network_instance(&self, instance_id: &Uuid) -> Result<(), String> {
        let config = INSTANCE_MANAGER
            .config(*instance_id)
            .ok_or_else(|| format!("instance {instance_id} is missing after restart"))?;
        let display_name = get_config_display_name(&instance_id.to_string())
            .or_else(|| {
                get_runtime_config_snapshot(&instance_id.to_string())
                    .map(|snapshot| snapshot.display_name)
            })
            .unwrap_or_else(|| config.get_inst_name());
        cache_runtime_config_snapshot(
            instance_id.to_string(),
            display_name,
            network_config_from_toml(&config),
        );
        Ok(())
    }
}

#[derive(Clone)]
struct NearbyWebClientService {
    inner: ProcessManagementRpc<NativeInstanceFactory>,
}

impl NearbyWebClientService {
    fn new() -> Self {
        Self {
            inner: ProcessManagementRpc::new(
                INSTANCE_MANAGER.clone(),
                Arc::new(NearbyMutationHooks),
                Arc::new(NearbyConfigStorage),
            ),
        }
    }

    fn ensure_safe_overwrite(&self, request: &RunNetworkInstanceRequest) -> Result<(), RpcError> {
        if !request.overwrite {
            return Err(anyhow::anyhow!("nearby management requires an explicit overwrite").into());
        }
        let requested_id = request
            .inst_id
            .clone()
            .map(Uuid::from)
            .ok_or_else(|| anyhow::anyhow!("nearby management requires an instance ID"))?;
        let running = INSTANCE_MANAGER.instance_ids();
        if running.len() != 1 || running[0] != requested_id {
            return Err(anyhow::anyhow!(
                "nearby management may only overwrite the single running HarmonyOS instance"
            )
            .into());
        }
        let expected_path = management_config_path(requested_id)
            .ok_or_else(|| anyhow::anyhow!("HarmonyOS config store is not initialized"))?;
        let control = INSTANCE_MANAGER
            .config_control(requested_id)
            .ok_or_else(|| anyhow::anyhow!("running instance config control is missing"))?;
        if control.path.as_deref() != Some(expected_path.as_path())
            || control.is_read_only()
            || !control.is_no_delete()
        {
            return Err(anyhow::anyhow!(
                "running instance is not owned by the HarmonyOS nearby console"
            )
            .into());
        }
        Ok(())
    }
}

#[async_trait]
impl WebClientService for NearbyWebClientService {
    type Controller = BaseController;

    async fn validate_config(
        &self,
        controller: BaseController,
        request: ValidateConfigRequest,
    ) -> Result<ValidateConfigResponse, RpcError> {
        self.inner.validate_config(controller, request).await
    }

    async fn run_network_instance(
        &self,
        controller: BaseController,
        request: RunNetworkInstanceRequest,
    ) -> Result<RunNetworkInstanceResponse, RpcError> {
        self.ensure_safe_overwrite(&request)?;
        self.inner.run_network_instance(controller, request).await
    }

    async fn retain_network_instance(
        &self,
        _controller: BaseController,
        _request: RetainNetworkInstanceRequest,
    ) -> Result<RetainNetworkInstanceResponse, RpcError> {
        Err(anyhow::anyhow!("nearby management cannot retain or remove HarmonyOS instances").into())
    }

    async fn collect_network_info(
        &self,
        controller: BaseController,
        request: CollectNetworkInfoRequest,
    ) -> Result<CollectNetworkInfoResponse, RpcError> {
        self.inner.collect_network_info(controller, request).await
    }

    async fn list_network_instance(
        &self,
        controller: BaseController,
        request: ListNetworkInstanceRequest,
    ) -> Result<ListNetworkInstanceResponse, RpcError> {
        self.inner.list_network_instance(controller, request).await
    }

    async fn delete_network_instance(
        &self,
        _controller: BaseController,
        _request: DeleteNetworkInstanceRequest,
    ) -> Result<DeleteNetworkInstanceResponse, RpcError> {
        Err(anyhow::anyhow!("nearby management cannot stop or delete a HarmonyOS instance").into())
    }

    async fn get_network_instance_config(
        &self,
        controller: BaseController,
        request: GetNetworkInstanceConfigRequest,
    ) -> Result<GetNetworkInstanceConfigResponse, RpcError> {
        self.inner
            .get_network_instance_config(controller, request)
            .await
    }

    async fn list_network_instance_meta(
        &self,
        controller: BaseController,
        request: ListNetworkInstanceMetaRequest,
    ) -> Result<ListNetworkInstanceMetaResponse, RpcError> {
        self.inner
            .list_network_instance_meta(controller, request)
            .await
    }
}

struct NearbyManagementHostServer {
    socket_path: PathBuf,
    _server: StandAloneServer<NearbyUnixListener>,
}

static NEARBY_HOST_SERVER: once_cell::sync::Lazy<Mutex<Option<NearbyManagementHostServer>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

pub(crate) fn runtime_management_config_control(instance_id: Uuid) -> Option<ConfigFileControl> {
    management_config_path(instance_id).map(|path| {
        ConfigFileControl::new(
            Some(path),
            ConfigFilePermission::from(ConfigFilePermission::NO_DELETE),
        )
    })
}

pub(crate) fn ensure_runtime_management_server_started() -> bool {
    let Some(socket_path) = management_socket_path() else {
        ohrs_log_error!("[Rust] nearby management config store is not initialized");
        return false;
    };
    let Ok(mut state) = NEARBY_HOST_SERVER.lock() else {
        return false;
    };
    if state
        .as_ref()
        .is_some_and(|server| server.socket_path == socket_path && socket_path.exists())
    {
        return true;
    }
    state.take();

    let mut server = StandAloneServer::new(NearbyUnixListener::new(socket_path.clone()));
    register_instance_management_rpc(INSTANCE_MANAGER.clone(), server.registry());
    server.registry().register(
        LoggerRpcServer::new(LoggerManagementRpc::new(Arc::new(NativeLoggerControl))),
        "",
    );
    server.registry().register(
        WebClientServiceServer::new(NearbyWebClientService::new()),
        "",
    );
    if let Err(error) = ASYNC_RUNTIME.block_on(server.serve()) {
        ohrs_log_error!("[Rust] nearby management server failed to start: {}", error);
        return false;
    }
    *state = Some(NearbyManagementHostServer {
        socket_path,
        _server: server,
    });
    true
}

pub(crate) fn stop_runtime_management_server() -> bool {
    NEARBY_HOST_SERVER
        .lock()
        .map(|mut state| state.take().is_some())
        .unwrap_or(false)
}

struct NearbyManagementSession {
    role: NearbyRole,
    rpc: Option<Arc<BidirectRpcManager>>,
    inbound: Mutex<Option<Sender<Vec<u8>>>>,
    outbound: Mutex<Receiver<Vec<u8>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl NearbyManagementSession {
    fn client() -> Self {
        let rpc = Arc::new(BidirectRpcManager::new());
        let tunnel = rpc.run_and_create_tunnel();
        Self::from_split(NearbyRole::Client, Some(rpc), tunnel.split())
    }

    fn host_proxy() -> anyhow::Result<Self> {
        let socket_path = management_socket_path()
            .ok_or_else(|| anyhow::anyhow!("HarmonyOS config store is not initialized"))?;
        let stream = ASYNC_RUNTIME.block_on(UnixStream::connect(&socket_path))?;
        let tunnel_info = TunnelInfo {
            tunnel_type: "unix".to_owned(),
            local_addr: None,
            remote_addr: Some(management_socket_url(&socket_path).into()),
            resolved_remote_addr: None,
        };
        let tunnel = TcpTunnelUpgrader::new(tunnel_info).upgrade(NearbyUnixSocket::new(stream))?;
        Ok(Self::from_split(
            NearbyRole::HostProxy,
            None,
            tunnel.split(),
        ))
    }

    fn from_split(
        role: NearbyRole,
        rpc: Option<Arc<BidirectRpcManager>>,
        split: SplitTunnel,
    ) -> Self {
        let (mut bridge_stream, mut bridge_sink) = split;
        let (inbound_tx, mut inbound_rx) = channel::<Vec<u8>>(PACKET_QUEUE_CAPACITY);
        let (outbound_tx, outbound_rx) = channel::<Vec<u8>>(PACKET_QUEUE_CAPACITY);

        let inbound_task = ASYNC_RUNTIME.spawn(async move {
            while let Some(bytes) = inbound_rx.recv().await {
                let packet =
                    ZCPacket::new_from_buf(BytesMut::from(bytes.as_slice()), ZCPacketType::NIC);
                if bridge_sink.send(packet).await.is_err() {
                    break;
                }
            }
            let _ = bridge_sink.close().await;
        });
        let outbound_task = ASYNC_RUNTIME.spawn(async move {
            while let Some(result) = bridge_stream.next().await {
                let Ok(packet) = result else {
                    break;
                };
                let bytes = packet.convert_type(ZCPacketType::NIC).into_bytes().to_vec();
                if outbound_tx.send(bytes).await.is_err() {
                    break;
                }
            }
        });

        Self {
            role,
            rpc,
            inbound: Mutex::new(Some(inbound_tx)),
            outbound: Mutex::new(outbound_rx),
            tasks: Mutex::new(vec![inbound_task, outbound_task]),
            closed: AtomicBool::new(false),
        }
    }

    fn push_packet(&self, bytes: Vec<u8>) -> bool {
        if self.closed.load(Ordering::Acquire) || !valid_rpc_packet_bytes(&bytes) {
            return false;
        }
        self.inbound
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .is_some_and(|sender| sender.try_send(bytes).is_ok())
    }

    fn drain_packets(&self) -> Vec<Uint8Array> {
        let Ok(mut receiver) = self.outbound.lock() else {
            return Vec::new();
        };
        let mut packets = Vec::new();
        while packets.len() < MAX_DRAIN_PACKETS {
            match receiver.try_recv() {
                Ok(packet) => packets.push(Uint8Array::from(packet)),
                Err(_) => break,
            }
        }
        packets
    }

    async fn call_json(
        &self,
        service_name: &str,
        method_name: &str,
        domain_name: &str,
        payload: Value,
    ) -> Result<Value, RpcError> {
        if self.role != NearbyRole::Client {
            return Err(RpcError::ExecutionError(anyhow::anyhow!(
                "nearby host proxy sessions cannot originate management calls"
            )));
        }
        let rpc = self.rpc.as_ref().ok_or_else(|| {
            RpcError::ExecutionError(anyhow::anyhow!("nearby RPC client is unavailable"))
        })?;
        call_official_management_json_rpc(
            rpc.rpc_client(),
            service_name,
            method_name,
            domain_name,
            payload,
        )
        .await
    }

    fn shutdown(self: Arc<Self>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut inbound) = self.inbound.lock() {
            inbound.take();
        }
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
        if let Some(rpc) = self.rpc.clone() {
            ASYNC_RUNTIME.spawn(async move {
                rpc.stop().await;
            });
        }
    }
}

static NEARBY_SESSIONS: once_cell::sync::Lazy<
    Mutex<HashMap<String, Arc<NearbyManagementSession>>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

fn management_socket_path() -> Option<PathBuf> {
    config_root_dir().map(|root| root.join(MANAGEMENT_SOCKET_FILE_NAME))
}

fn management_socket_url(path: &Path) -> Url {
    format!("unix://{}", path.display())
        .parse()
        .expect("HarmonyOS sandbox path must form a Unix URL")
}

fn management_config_path(instance_id: Uuid) -> Option<PathBuf> {
    config_root_dir().map(|root| {
        root.join(MANAGEMENT_CONFIG_DIR_NAME)
            .join(format!("{instance_id}.toml"))
    })
}

fn config_id_from_management_path(path: &Path) -> anyhow::Result<Uuid> {
    let file_stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("nearby management config path is invalid"))?;
    let instance_id = Uuid::parse_str(file_stem)?;
    let expected = management_config_path(instance_id)
        .ok_or_else(|| anyhow::anyhow!("HarmonyOS config store is not initialized"))?;
    if expected != path {
        anyhow::bail!("nearby management config path is outside the managed store");
    }
    Ok(instance_id)
}

fn valid_session_key(session_key: &str) -> bool {
    !session_key.is_empty()
        && session_key.len() <= MAX_SESSION_KEY_LENGTH
        && session_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}

fn valid_rpc_packet_bytes(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
        return false;
    }
    let packet = ZCPacket::new_from_buf(BytesMut::from(bytes), ZCPacketType::NIC);
    let Some(header) = packet.peer_manager_header() else {
        return false;
    };
    matches!(
        header.packet_type,
        value if value == PacketType::RpcReq as u8 || value == PacketType::RpcResp as u8
    )
}

fn session(session_key: &str) -> Option<Arc<NearbyManagementSession>> {
    NEARBY_SESSIONS
        .lock()
        .ok()
        .and_then(|sessions| sessions.get(session_key).cloned())
}

fn open_session(session_key: String, role: NearbyRole) -> bool {
    if !valid_session_key(&session_key) {
        return false;
    }
    let session = match role {
        NearbyRole::Client => NearbyManagementSession::client(),
        NearbyRole::HostProxy => match NearbyManagementSession::host_proxy() {
            Ok(session) => session,
            Err(error) => {
                ohrs_log_error!("[Rust] nearby management host proxy failed: {}", error);
                return false;
            }
        },
    };
    let Ok(mut sessions) = NEARBY_SESSIONS.lock() else {
        return false;
    };
    if sessions.contains_key(&session_key) || sessions.len() >= MAX_NEARBY_SESSIONS {
        return false;
    }
    sessions.insert(session_key, Arc::new(session));
    true
}

pub(crate) fn open_nearby_management_session(session_key: String, host: bool) -> bool {
    open_session(
        session_key,
        if host {
            NearbyRole::HostProxy
        } else {
            NearbyRole::Client
        },
    )
}

pub(crate) fn close_nearby_management_session(session_key: String) -> bool {
    let removed = NEARBY_SESSIONS
        .lock()
        .ok()
        .and_then(|mut sessions| sessions.remove(&session_key));
    if let Some(session) = removed {
        session.shutdown();
        true
    } else {
        false
    }
}

pub(crate) fn push_nearby_management_packet(session_key: String, packet: Uint8Array) -> bool {
    session(&session_key).is_some_and(|session| session.push_packet(packet.to_vec()))
}

pub(crate) fn drain_nearby_management_packets(session_key: String) -> Vec<Uint8Array> {
    session(&session_key)
        .map(|session| session.drain_packets())
        .unwrap_or_default()
}

pub(crate) async fn call_nearby_management_json_rpc(
    session_key: String,
    service_name: String,
    method_name: String,
    domain_name: Option<String>,
    payload_json: String,
) -> String {
    if payload_json.len() > MAX_JSON_PAYLOAD_BYTES {
        return json!({ "ok": false, "error": "management payload is too large" }).to_string();
    }
    let Some(session) = session(&session_key) else {
        return json!({ "ok": false, "error": "nearby management session not found" }).to_string();
    };
    let payload = match serde_json::from_str::<Value>(&payload_json) {
        Ok(payload) => payload,
        Err(error) => {
            return json!({ "ok": false, "error": format!("invalid management JSON: {error}") })
                .to_string();
        }
    };
    match session
        .call_json(
            service_name.trim(),
            method_name.trim(),
            domain_name.as_deref().unwrap_or_default().trim(),
            payload,
        )
        .await
    {
        Ok(result) => json!({ "ok": true, "result": result }).to_string(),
        Err(error) => json!({ "ok": false, "error": error.to_string() }).to_string(),
    }
}

async fn call_official_management_json_rpc(
    client: &Client,
    service_name: &str,
    method_name: &str,
    domain_name: &str,
    payload: Value,
) -> Result<Value, RpcError> {
    let domain = domain_name.to_owned();
    let controller = BaseController::default();
    match service_name {
        "api.manage.WebClientService" => {
            client
                .scoped_client::<WebClientServiceClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.logger.LoggerRpcService" => {
            client
                .scoped_client::<LoggerRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.PeerManageRpcService" => {
            client
                .scoped_client::<PeerManageRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.PeerCenterManageRpcService" => {
            client
                .scoped_client::<PeerCenterRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.ConnectorManageRpcService" => {
            client
                .scoped_client::<ConnectorManageRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.MappedListenerManageRpcService" => {
            client
                .scoped_client::<MappedListenerManageRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.VpnPortalRpcService" => {
            client
                .scoped_client::<VpnPortalRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.TcpProxyRpcService" => {
            client
                .scoped_client::<TcpProxyRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.AclManageRpcService" => {
            client
                .scoped_client::<AclManageRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.PortForwardManageRpcService" => {
            client
                .scoped_client::<PortForwardManageRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.StatsRpcService" => {
            client
                .scoped_client::<StatsRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.instance.CredentialManageRpcService" => {
            client
                .scoped_client::<CredentialManageRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        "api.config.ConfigRpcService" => {
            client
                .scoped_client::<ConfigRpcClientFactory<BaseController>>(
                    RPC_PEER_ID,
                    RPC_PEER_ID,
                    domain,
                )
                .json_call_method(controller, method_name, payload)
                .await
        }
        _ => Err(RpcError::InvalidServiceKey(
            service_name.to_owned(),
            service_name.to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_rpc_packets_before_the_harmony_transport() {
        assert!(!valid_rpc_packet_bytes(&[]));
        assert!(!valid_rpc_packet_bytes(&[1, 2, 3]));
        assert!(!valid_session_key("contains space"));
        assert!(valid_session_key("42:client"));
    }
}
