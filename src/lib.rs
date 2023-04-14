/// Provides the backing implementation for peer connections and channels.
mod backend;

use arc_swap::*;
use serde::*;
use std::error::*;
use std::future::*;
use std::num::*;
use std::pin::*;
use std::sync::*;
use std::sync::mpsc::*;
use std::task::*;
use thiserror::*;


/// A channel across which messages may be exchanged with a remote peer.
pub struct RtcDataChannel {
    handle: backend::RtcDataChannelBackendImpl,
    label: String,
    id: u16,
    previous_result: Result<(), RtcDataChannelError>,
    received_messages: Receiver<Result<Box<[u8]>, RtcDataChannelError>>,
}

impl RtcDataChannel {
    /// Creates a new raw datachannel with the provided backing implementation, identifiers, and received message queue.
    fn new(handle: backend::RtcDataChannelBackendImpl, label: String, id: u16, received_messages: Receiver<Result<Box<[u8]>, RtcDataChannelError>>) -> Self {
        Self {
            handle,
            label,
            id,
            previous_result: Ok(()),
            received_messages
        }
    }

    /// Connects to a remote peer with the provided configuration and negotiator. If successful,
    /// returns the set of channels corresponding to the provided channel configurations.
    pub async fn connect(config: &IceConfiguration<'_>, negotiator: impl RtcNegotiationHandler, channels: &[RtcDataChannelConfiguration<'_>]) -> Result<Box<[Self]>, RtcPeerConnectionError> {
        backend::RtcDataChannelBackendImpl::connect(config, negotiator, channels).await
    }

    /// The unique identifier of this channel.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// The human-readable label of this channel.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The current connection state of the channel.
    pub fn ready_state(&self) -> RtcDataChannelReadyState {
        if self.previous_result.is_ok() {
            self.handle.ready_state()
        }
        else {
            RtcDataChannelReadyState::Closed
        }
    }

    /// Sends the provided message to the remote peer.
    pub fn send(&mut self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        self.previous_result.clone()?;
        if let Err(err) = self.handle.send(message).map_err(|x| RtcDataChannelError::Send(x.to_string())) {
            self.previous_result = Err(err.clone());
            Err(err)
        }
        else {
            Ok(())
        }
    }

    /// Attempts to read the next message from the remote peer,
    /// returning none if no new message has yet been received.
    pub fn receive(&mut self) -> Result<Option<Box<[u8]>>, RtcDataChannelError> {
        self.previous_result.clone()?;
        match self.received_messages.try_recv() {
            Ok(Ok(m)) => Ok(Some(m)),
            Ok(Err(m)) => {
                self.previous_result = Err(m.clone());
                Err(m)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(_) => unreachable!("Receiver should not be dropped before data channel.")
        }
    }

    /// Reads the next message from the remote peer, waiting until a message is received.
    pub fn receive_async(&mut self) -> impl '_ + Future<Output = Result<Box<[u8]>, RtcDataChannelError>> {
        RtcDataChannelReceiveFuture(self)
    }
}

/// Negotiates a connection with the remote peer during the initial connection process,
/// exchanging messages with the signaling server. 
pub trait RtcNegotiationHandler {
    /// Sends a negotiation message to the remote peer through a signaling implementation.
    fn send(&mut self, message: RtcNegotiationMessage) -> Pin<Box<dyn '_ + Future<Output = Result<(), RtcPeerConnectionError>>>>;
    /// Checks the signaling server for new negotiation messages from the remote peer.
    fn receive(&mut self) -> Pin<Box<dyn '_ + Future<Output = Result<Vec<RtcNegotiationMessage>, RtcPeerConnectionError>>>>;
}

/// Describes an error that occurred while attempting to establish a peer connection.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RtcPeerConnectionError {
    /// The connection could not be created.
    #[error("An error occurred during connection creation: {0}")]
    Creation(String),
    /// An issue happened during negotiation.
    #[error("An error occurred during channel negotiation: {0}")]
    Negotiation(String),
    /// The peers did not create the same data channels with the same configurations.
    #[error("The data channel configuration was not the same for both peers: {0}")]
    DataChannelMismatch(String),
    /// The peers could not negotiate over ICE.
    #[error("ICE negotiation failed: {0}")]
    IceNegotiationFailure(String)
}

/// Describes an error that occurred with an active data channel.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RtcDataChannelError {
    /// A message could not be sent.
    #[error("An error occurred during message sending: {0}")]
    Send(String),
    /// Messages could not be received.
    #[error("An error occurred during message receiving: {0}")]
    Receive(String),
    //#[error("An error occurred while writing to std::io::Write: {0}")]
    //WriteError(std::io::Error)
}

/// Provides data about a potential connection route to a remote peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RtcNegotiationMessage {
    /// Adds a remote candidate for consideration.
    RemoteCandidate(RtcIceCandidate),
    /// Describes the state of a remote session.
    RemoteSessionDescription(RtcSessionDescription)
}

/// Describes a possible way to connect to a remote peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcIceCandidate {
    /// The candidate name.
    pub candidate: String,
    /// The SDP description string describing the candidate.
    #[serde(rename = "sdpMid")]
    pub sdp_mid: String,
}

/// Describes a remote peer's session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcSessionDescription {
    /// The SDP description string describing the session.
    pub sdp: String,
    /// The SDP type.
    #[serde(rename = "type")]
    pub sdp_type: String,
}

/// Describes the set of ICE servers and protocols that should be employed
/// during connection establishment.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct IceConfiguration<'a> {
    /// The set of ICE servers that should be used to gather connection candidates.
    pub ice_servers: &'a [RtcIceServer<'a>],
    /// The subset of ICE transports that should be considered.
    pub ice_transport_policy: RtcIceTransportPolicy,
}

/// Describes a server that may be utilized for peer-to-peer route discovery or communication.
#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub struct RtcIceServer<'a> {
    /// The set of URLs that may be used to connect to the server.
    pub urls: &'a [&'a str],
    /// The optional username that should be employed when connecting to the server.
    pub username: Option<&'a str>,
    /// The optional credential that should be employed when connecting to the server.
    pub credential: Option<&'a str>
}

/// Determines the subset of ICE functionality that should be considered.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
#[repr(u32)]
pub enum RtcIceTransportPolicy {
    /// Both direct peer-to-peer connections and relay-based methods should be used.
    #[default]
    All,
    /// Only relay-based transport methods should be used.
    Relay,
}

/// Describes the guarantees that a data channel should make regarding
/// how data is forwarded to the other side.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RtcReliabilityMode {
    /// Data will always reach the other side, as long as the connection is maintained.
    Reliable(),
    /// The receiver should limit the maximum number of times a message may be retransmitted.
    /// Setting this to `0` creates an unreliable channel.
    MaxRetransmits(u16),
    /// The receiver should limit the maximum number of milliseconds in which a message
    /// may be transmitted.
    MaxPacketLifetime(NonZeroU16)
}

/// Describes how a channel should be created.
#[derive(Clone)]
pub struct RtcDataChannelConfiguration<'a> {
    pub label: &'a str,
    /// Whether messages on the channel should be ordered or unordered.
    pub ordered: bool,
    /// How the data channel should deal with dropped messages and retransmissions.
    pub reliability: RtcReliabilityMode,
    /// The subprotocol that should be employed during data transmission.
    pub protocol: Option<&'a str>,
}

impl<'a> Default for RtcDataChannelConfiguration<'a> {
    fn default() -> Self {
        Self { label: "", ordered: true, reliability: RtcReliabilityMode::Reliable(), protocol: None }
    }
}

/// Describes the current connection state of a channel.
#[derive(Copy, Clone, PartialEq, Debug)]
#[repr(u8)]
pub enum RtcDataChannelReadyState {
    /// The channel's connection is being established.
    Connecting,
    /// The channel is open for sending and receiving data.
    Open,
    /// The procedure for closing the channel has begun.
    Closing,
    /// The channel is closed, and will not receive any more data.
    Closed
}

impl TryFrom<u8> for RtcDataChannelReadyState {
    type Error = Box<dyn Error>;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Connecting),
            1 => Ok(Self::Open),
            2 => Ok(Self::Closing),
            3 => Ok(Self::Closed),
            _ => Err("Value was out-of-range".into())
        }
    }
}

/// Implements a future that waits for a new message to be received on a data channel.
struct RtcDataChannelReceiveFuture<'a>(&'a mut RtcDataChannel);

impl<'a> Future for RtcDataChannelReceiveFuture<'a> {
    type Output = Result<Box<[u8]>, RtcDataChannelError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.receive() {
            Ok(Some(res)) => Poll::Ready(Ok(res)),
            Ok(None) => { 
                self.0.handle.receive_waker().store(Some(Arc::new(cx.waker().clone())));
                Poll::Pending
            },
            Err(err) => Poll::Ready(Err(err))
        }
    }
}

/// Provides the backing functionality for a data channel.
trait RtcDataChannelBackend {
    /// Attempts to connect to a remote peer with the provided configuration, returning the newly-created set of channels for the peer.
    fn connect<'a>(config: &'a IceConfiguration<'a>, negotiator: impl 'a + RtcNegotiationHandler, channels: &'a [RtcDataChannelConfiguration<'a>]) -> Pin<Box<dyn 'a + Future<Output = Result<Box<[RtcDataChannel]>, RtcPeerConnectionError>>>>;
    /// Determines the channel's current connection state.
    fn ready_state(&self) -> RtcDataChannelReadyState;
    /// Sends a message to the remote host.
    fn send(&mut self, message: &[u8]) -> Result<(), RtcDataChannelError>;
    /// Obtains a reference to an optional waker, which is woken whenever a new message is received.
    fn receive_waker(&mut self) -> Arc<ArcSwapOption<Waker>>;
}

struct PollFuture {
    count: u32
}

impl PollFuture {
    pub fn new(count: u32) -> PollFuture {
        PollFuture { count }
    }

    pub fn once() -> PollFuture {
        PollFuture::new(1)
    }
}

impl Future for PollFuture {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.count > 0 {
            self.get_mut().count -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        else {
            Poll::Ready(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    struct ChannelNegotiationHandler {
        sender: Sender<RtcNegotiationMessage>,
        receiver: Receiver<RtcNegotiationMessage>
    }

    impl ChannelNegotiationHandler {
        pub fn new_pair() -> (Self, Self) {
            let (send_a, recv_b) = channel();
            let (send_b, recv_a) = channel();

            (Self { sender: send_a, receiver: recv_a }, Self { sender: send_b, receiver: recv_b })
        }
    }

    impl RtcNegotiationHandler for ChannelNegotiationHandler {
        fn send(&mut self, message: RtcNegotiationMessage) -> Pin<Box<dyn '_ + Future<Output = Result<(), RtcPeerConnectionError>>>> {
            drop(self.sender.send(message));
            Box::pin(async { Ok(()) })
        }

        fn receive(&mut self) -> Pin<Box<dyn '_ + Future<Output = Result<Vec<RtcNegotiationMessage>, RtcPeerConnectionError>>>> {
            Box::pin(async { Ok(self.receiver.try_recv().ok().into_iter().collect::<Vec<_>>()) })
        }
    }

    async fn create_channels(handler: ChannelNegotiationHandler) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let ice_configuation = IceConfiguration {
            ice_servers: &[RtcIceServer { urls: &[ "stun:stun.l.google.com:19302" ], ..Default::default() }],
            ice_transport_policy: RtcIceTransportPolicy::All
        };

        RtcDataChannel::connect(&ice_configuation, handler,
            &[RtcDataChannelConfiguration { label: "chan", ..Default::default() }]
        ).await
    }
    
    #[tokio::test]
    async fn test_connect() {
        let local = tokio::task::LocalSet::new();

        let (a, b) = ChannelNegotiationHandler::new_pair();

        let msg = b"test msg";

        local.spawn_local(async move {
            let chans = create_channels(a).await;
            chans.expect("An error occurred during channel creation.")[0].send(&msg[..]).expect("An error occurred during message sending.");
        });

        local.run_until(async move {
            let chans = create_channels(b).await;
            let res = chans.expect("An error occurred during channel creation.")[0].receive_async().await.expect("An error occurred during message sending.");
            assert!(&res[..] == &msg[..]);
        }).await;
    }
}