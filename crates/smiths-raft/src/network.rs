//! TCP-based Raft network transport (slice 6.3b).
//!
//! Each Raft RPC (`AppendEntries`, `InstallSnapshot`, `Vote`) is sent as a
//! JSON-lines message over a fresh TCP connection. This is intentionally
//! simple and mirrors the 6.2 replication service pattern.

use std::net::SocketAddr;

use openraft::BasicNode;
use openraft::error::{InstallSnapshotError, RPCError, RaftError};
use openraft::network::{RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::warn;

use crate::types::{NodeId, SmithsTypeConfig};

// ---------------------------------------------------------------------------
// RPC envelope — wraps each request/response type for multiplexing
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "rpc", rename_all = "snake_case")]
enum RaftRpc {
    AppendEntries(AppendEntriesRequest<SmithsTypeConfig>),
    InstallSnapshot(InstallSnapshotRequest<SmithsTypeConfig>),
    Vote(VoteRequest<NodeId>),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "rpc", rename_all = "snake_case")]
enum RaftRpcResponse {
    AppendEntries(AppendEntriesResponse<NodeId>),
    InstallSnapshot(InstallSnapshotResponse<NodeId>),
    Vote(VoteResponse<NodeId>),
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Network factory
// ---------------------------------------------------------------------------

/// Produces `SmithsNetwork` connections to peer nodes.
#[derive(Clone)]
pub struct SmithsNetworkFactory;

impl RaftNetworkFactory<SmithsTypeConfig> for SmithsNetworkFactory {
    type Network = SmithsNetwork;

    async fn new_client(&mut self, _target: NodeId, node: &BasicNode) -> Self::Network {
        SmithsNetwork {
            addr: node.addr.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Network client (one per peer)
// ---------------------------------------------------------------------------

/// A Raft network client that sends RPCs over TCP.
pub struct SmithsNetwork {
    addr: String,
}

impl SmithsNetwork {
    /// Send a request envelope and read the response.
    async fn rpc(&self, req: RaftRpc) -> Result<RaftRpcResponse, std::io::Error> {
        let mut stream = TcpStream::connect(&self.addr).await?;
        let payload = serde_json::to_vec(&req).unwrap();
        stream.write_all(&payload).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let resp: RaftRpcResponse = serde_json::from_str(line.trim())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(resp)
    }
}

type AppendError = RPCError<NodeId, BasicNode, RaftError<NodeId>>;
type InstallError = RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>;
type VoteError = RPCError<NodeId, BasicNode, RaftError<NodeId>>;

impl RaftNetwork<SmithsTypeConfig> for SmithsNetwork {
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<SmithsTypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, AppendError> {
        let resp = self
            .rpc(RaftRpc::AppendEntries(req))
            .await
            .map_err(|e| RPCError::Unreachable(openraft::error::Unreachable::new(&e)))?;
        match resp {
            RaftRpcResponse::AppendEntries(r) => Ok(r),
            RaftRpcResponse::Error { message } => Err(RPCError::Unreachable(
                openraft::error::Unreachable::new(&std::io::Error::other(message)),
            )),
            _ => Err(RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected response type"),
            ))),
        }
    }

    async fn install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<SmithsTypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, InstallError> {
        let resp = self
            .rpc(RaftRpc::InstallSnapshot(req))
            .await
            .map_err(|e| RPCError::Unreachable(openraft::error::Unreachable::new(&e)))?;
        match resp {
            RaftRpcResponse::InstallSnapshot(r) => Ok(r),
            RaftRpcResponse::Error { message } => Err(RPCError::Unreachable(
                openraft::error::Unreachable::new(&std::io::Error::other(message)),
            )),
            _ => Err(RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected response type"),
            ))),
        }
    }

    async fn vote(
        &mut self,
        req: VoteRequest<NodeId>,
        _option: openraft::network::RPCOption,
    ) -> Result<VoteResponse<NodeId>, VoteError> {
        let resp = self
            .rpc(RaftRpc::Vote(req))
            .await
            .map_err(|e| RPCError::Unreachable(openraft::error::Unreachable::new(&e)))?;
        match resp {
            RaftRpcResponse::Vote(r) => Ok(r),
            RaftRpcResponse::Error { message } => Err(RPCError::Unreachable(
                openraft::error::Unreachable::new(&std::io::Error::other(message)),
            )),
            _ => Err(RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected response type"),
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Raft RPC server — listens on a TCP port, dispatches to the local Raft node
// ---------------------------------------------------------------------------

/// Run the Raft RPC listener on `addr`.
///
/// Incoming connections carry one JSON-lines `RaftRpc` request. The handler
/// dispatches to the local `Raft` instance and writes back the response.
pub async fn run_raft_server(
    addr: SocketAddr,
    raft: crate::types::Raft,
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "Raft RPC server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let raft = raft.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_raft_conn(stream, &raft).await {
                warn!(%peer, ?e, "Raft RPC handler error");
            }
        });
    }
}

async fn handle_raft_conn(
    stream: tokio::net::TcpStream,
    raft: &crate::types::Raft,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let request: RaftRpc = serde_json::from_str(line.trim())?;

    let response = match request {
        RaftRpc::AppendEntries(req) => match raft.append_entries(req).await {
            Ok(r) => RaftRpcResponse::AppendEntries(r),
            Err(e) => RaftRpcResponse::Error {
                message: e.to_string(),
            },
        },
        RaftRpc::InstallSnapshot(req) => match raft.install_snapshot(req).await {
            Ok(r) => RaftRpcResponse::InstallSnapshot(r),
            Err(e) => RaftRpcResponse::Error {
                message: e.to_string(),
            },
        },
        RaftRpc::Vote(req) => match raft.vote(req).await {
            Ok(r) => RaftRpcResponse::Vote(r),
            Err(e) => RaftRpcResponse::Error {
                message: e.to_string(),
            },
        },
    };

    let payload = serde_json::to_vec(&response)?;
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    Ok(())
}
