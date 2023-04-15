use arc_swap::*;
use crate::*;
use std::pin::*;
use std::sync::*;
use std::sync::atomic::*;
use std::sync::mpsc::*;
use std::task::*;

/// Provides a native implementation of a data channel based upon the `datachannel` crate.
pub struct RtcDataChannelBackendImpl {
    handle: Pin<Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>>,
    _peer_connection: Arc<Pin<Box<datachannel::RtcPeerConnection<RtcPeerConnectionEventHandlers>>>>,
    ready_state: Arc<AtomicU8>,
    receive_waker: Arc<ArcSwapOption<Waker>>
}

impl RtcDataChannelBackendImpl {
    /// Creates a new native data channel from the provided handle, peer connection reference, and state.
    fn new(handle: Pin<Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>>, peer_connection: Arc<Pin<Box<datachannel::RtcPeerConnection<RtcPeerConnectionEventHandlers>>>>, state: &RtcDataChannelHandlerState) -> Self {
        Self {
            handle,
            _peer_connection: peer_connection,
            ready_state: state.ready_state.clone(),
            receive_waker: state.receive_waker.clone()
        }
    }
}

impl RtcDataChannelBackend for RtcDataChannelBackendImpl {
    fn connect<'a>(config: &'a IceConfiguration<'a>, negotiator: impl 'a + RtcNegotiationHandler, channels: &'a [RtcDataChannelConfiguration<'a>]) -> RtcDataChannelConnectionFuture<'a> {
        Box::pin(RtcPeerConnector::connect(config, negotiator, channels))
    }

    fn ready_state(&self) -> RtcDataChannelReadyState {
        self.ready_state.load(Ordering::Acquire).try_into().expect("Invalid channel state.")
    }

    fn send(&mut self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        self.handle.send(message).map_err(|x| RtcDataChannelError::Send(x.to_string()))
    }

    fn receive_waker(&mut self) -> Arc<ArcSwapOption<Waker>> {
        self.receive_waker.clone()
    }
}

/// Responds to events that occur on a data channel.
struct RtcDataChannelEventHandlers {
    open_count: Arc<AtomicUsize>,
    ready_state: Arc<AtomicU8>,
    receive_waker: Arc<ArcSwapOption<Waker>>,
    sender: Sender<Result<Box<[u8]>, RtcDataChannelError>>
}

impl RtcDataChannelEventHandlers {
    /// Creates a new set of event handlers that updates the given count on open.
    pub fn new(open_count: Arc<AtomicUsize>) -> (Self, RtcDataChannelHandlerState) {
        let ready_state = Arc::new(AtomicU8::new(RtcDataChannelReadyState::Open as u8));
        let receive_waker = Arc::<ArcSwapOption<Waker>>::default();
        let (sender, receiver) = channel();

        (Self { open_count, ready_state: ready_state.clone(), receive_waker: receive_waker.clone(), sender }, RtcDataChannelHandlerState { ready_state, receive_waker, receiver })
    }

    /// Wakes the future currently waiting on this set of event handlers.
    fn wake_listener(&mut self) {
        if let Some(waker) = &*self.receive_waker.load() {
            waker.wake_by_ref();
        }
    }
}

impl datachannel::DataChannelHandler for RtcDataChannelEventHandlers {
    fn on_open(&mut self) {
        self.open_count.fetch_add(1, Ordering::AcqRel);
    }

    fn on_closed(&mut self) {
        drop(self.sender.send(Err(RtcDataChannelError::Receive("The channel was closed.".to_string()))));
        self.ready_state.store(RtcDataChannelReadyState::Closed as u8, Ordering::Release);
        self.wake_listener();
    }

    fn on_error(&mut self, err: &str) {
        drop(self.sender.send(Err(RtcDataChannelError::Receive(format!("The channel encountered an error: {err:?}")))));
        self.ready_state.store(RtcDataChannelReadyState::Closed as u8, Ordering::Release);
        self.wake_listener();
    }

    fn on_message(&mut self, msg: &[u8]) {
        drop(self.sender.send(Ok(msg.to_vec().into_boxed_slice())));
        self.wake_listener();
    }

    fn on_buffered_amount_low(&mut self) {}

    fn on_available(&mut self) {}
}

/// Manages the establishment of a connection with a remote peer.
struct RtcPeerConnector<'a, N: RtcNegotiationHandler> {
    channel_configurations: &'a [RtcDataChannelConfiguration<'a>],
    handle: Pin<Box<datachannel::RtcPeerConnection<RtcPeerConnectionEventHandlers>>>,
    negotiator: N,
    negotiation_receive: Receiver<RtcNegotiationNotification>
}

impl<'a, N: RtcNegotiationHandler> RtcPeerConnector<'a, N> {
    /// Creates a new connector for the given configuration, negotiator, and channels.
    pub async fn connect(configuration: &'a IceConfiguration<'a>, negotiator: N, channel_configurations: &'a [RtcDataChannelConfiguration<'a>]) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let (negotiation_send, negotiation_receive) = channel();

        let handle = Box::into_pin(datachannel::RtcPeerConnection::new(&configuration.into(), RtcPeerConnectionEventHandlers::new(negotiation_send))
            .map_err(|x| RtcPeerConnectionError::Creation(x.to_string()))?);

        Self { channel_configurations, handle, negotiator, negotiation_receive }.accept_connections().await
    }

    /// Performs ICE with the remote peer and attempts to create a set of data channels.
    async fn accept_connections(mut self) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let channels = self.create_channels()?;
        self.negotiate_connection(&channels).await?;
        Ok(self.collate_connected_channels(channels))
    }

    /// Exchanges messages with the remote peer until an error occurs or a connection is established.
    async fn negotiate_connection(&mut self, channels: &RtcDataChannelList) -> Result<(), RtcPeerConnectionError> {
        let mut candidate_buffer = Vec::new();

        let mut local_description = None;
        while channels.open_count.load(Ordering::Acquire) < channels.handle_states.len() {
            if let Ok(n) = self.negotiation_receive.try_recv() {
                match n {
                    RtcNegotiationNotification::SendMessage(m) => self.send_message_update_description(m, &mut local_description).await?,
                    RtcNegotiationNotification::Failed(m) => return Err(m)
                }
            }
            else {
                for m in self.negotiator.receive().await? {
                    self.receive_negotiation_message(m, &mut candidate_buffer, &local_description).await?;
                }
            }

            PollFuture::once().await;
        }

        Ok(())
    }

    /// Gathers all newly-created channels into a list of output channels.
    fn collate_connected_channels(self, channels: RtcDataChannelList) -> Box<[RtcDataChannel]> {
        let peer_connection = Arc::new(self.handle);

        channels.handle_states.into_iter().enumerate().map(|(id, (handle, state))| {
            RtcDataChannel::new(RtcDataChannelBackendImpl::new(handle, peer_connection.clone(), &state), self.channel_configurations[id].label.to_string(), id as u16, state.receiver)
        }).collect::<Vec<_>>().into_boxed_slice()
    }

    /// Creates the set of data channels described by the current configuration.
    fn create_channels(&mut self) -> Result<RtcDataChannelList, RtcPeerConnectionError> {
        let mut handle_states = Vec::new();
        let open_count = Arc::<AtomicUsize>::default();

        for config in self.channel_configurations {
            let (handler, state) = RtcDataChannelEventHandlers::new(open_count.clone());

            handle_states.push((Box::into_pin(self.handle.create_data_channel_ex(config.label, handler,
                &datachannel::DataChannelInit::from(config).stream(handle_states.len() as u16))
                .map_err(|x| RtcPeerConnectionError::Creation(x.to_string()))?), state));
        }

        Ok(RtcDataChannelList { handle_states, open_count })
    }

    /// Sends a message to the remote peer, and updates the current local description if it is a session description message.
    async fn send_message_update_description(&mut self, message: RtcNegotiationMessage, local_description: &mut Option<RtcSessionDescription>) -> Result<(), RtcPeerConnectionError> {
        if let RtcNegotiationMessage::RemoteSessionDescription(desc) = &message {
            if desc.sdp_type == "offer" && self.handle.remote_description().map(|x| x.sdp_type == datachannel::SdpType::Offer).unwrap_or(false) {
                return Ok(());
            }
            if local_description.is_none() {
                *local_description = Some(desc.clone());
            }
        }

        self.negotiator.send(message).await
    }

    /// Interprets a negotiation message from the remote peer and handles the result.
    async fn receive_negotiation_message(&mut self, message: RtcNegotiationMessage, candidate_buffer: &mut Vec<RtcIceCandidate>, local_description: &Option<RtcSessionDescription>) -> Result<(), RtcPeerConnectionError> {
        match message {
            RtcNegotiationMessage::RemoteCandidate(c) => {
                if self.has_remote_description() {
                    self.add_remote_candidate(c).await
                }
                else {
                    candidate_buffer.push(c);
                    Ok(())
                }
            },
            RtcNegotiationMessage::RemoteSessionDescription(c) => {
                if !self.has_remote_description() {
                    let ci = datachannel::SessionDescription {
                        sdp: webrtc_sdp::parse_sdp(c.sdp.as_str(), false).map_err(|x| RtcPeerConnectionError::Negotiation(x.to_string()))?,
                        sdp_type: match c.sdp_type.as_str() {
                            "answer" => datachannel::SdpType::Answer,
                            "offer" => datachannel::SdpType::Offer,
                            "pranswer" => datachannel::SdpType::Pranswer,
                            "rollback" => datachannel::SdpType::Rollback,
                            _ => Err(RtcPeerConnectionError::Negotiation("Unexpected SDP type".to_string()))?
                        }
                    };
                    
                    if ci.sdp_type != datachannel::SdpType::Offer || local_description.as_ref().map(|x| x.sdp < c.sdp).unwrap_or(true) {
                        self.handle.set_remote_description(&ci).map_err(|x| RtcPeerConnectionError::Negotiation(x.to_string()))?;

                        for cand in candidate_buffer.drain(..) {
                            self.add_remote_candidate(cand).await?;
                        }
                    }
                }
                
                Ok(())
            },
        }
    }

    /// Determines whether this connector has a remote description yet.
    fn has_remote_description(&self) -> bool {
        self.handle.remote_description().is_some()
    }

    /// Adds a remote ICE candidate to the peer connection.
    async fn add_remote_candidate(&mut self, c: RtcIceCandidate) -> Result<(), RtcPeerConnectionError> {
        self.handle.add_remote_candidate(&c.into()).map_err(|x| RtcPeerConnectionError::Negotiation(x.to_string()))
    }
}

/// Stores a list of data channels that are being opened.
struct RtcDataChannelList {
    /// A list of channel handles and their handler state, ordered by ID.
    pub handle_states: Vec<(Pin<Box<datachannel::RtcDataChannel<RtcDataChannelEventHandlers>>>, RtcDataChannelHandlerState)>,
    /// The total number of channels that have successfully opened so far.
    pub open_count: Arc<AtomicUsize>
}

/// Stores the state necessary to interact with a data channel handle.
#[derive(Debug)]
struct RtcDataChannelHandlerState {
    /// The current ready state of the channel.
    pub ready_state: Arc<AtomicU8>,
    /// A waker that receives a notification whenever something new happens on the channel.
    pub receive_waker: Arc<ArcSwapOption<Waker>>,
    /// The receiver to read in order to obtain new messages from the remote peer.
    pub receiver: RtcDataChannelMessageReceiver
}

/// Notifies the local connector about an event during connection establishment.
enum RtcNegotiationNotification {
    /// A message should be sent to the remote peer.
    SendMessage(RtcNegotiationMessage),
    /// The connection failed with an error.
    Failed(RtcPeerConnectionError)
}

/// Responds to events that occur on a new peer connection.
struct RtcPeerConnectionEventHandlers {
    negotiation_send: Sender<RtcNegotiationNotification>
}

impl RtcPeerConnectionEventHandlers {
    /// Creates a new set of event handlers that send messages to the provided sink.
    pub fn new(negotiation_send: Sender<RtcNegotiationNotification>) -> Self {
        Self { negotiation_send }
    }
}

impl datachannel::PeerConnectionHandler for RtcPeerConnectionEventHandlers {
    type DCH = RtcDataChannelEventHandlers;

    fn data_channel_handler(&mut self, _: datachannel::DataChannelInfo) -> Self::DCH {
        drop(self.negotiation_send.send(RtcNegotiationNotification::Failed(RtcPeerConnectionError::DataChannelMismatch("The remote peer attempted to open an unsolicited data channel.".to_string()))));
        RtcDataChannelEventHandlers::new(Arc::default()).0
    }

    fn on_candidate(&mut self, cand: datachannel::IceCandidate) {
        drop(self.negotiation_send.send(RtcNegotiationNotification::SendMessage(cand.into())));
    }

    fn on_description(&mut self, sess_desc: datachannel::SessionDescription) {
        drop(self.negotiation_send.send(RtcNegotiationNotification::SendMessage(sess_desc.into())));
    }

    fn on_connection_state_change(&mut self, state: datachannel::ConnectionState) {
        if state == datachannel::ConnectionState::Failed {
            drop(self.negotiation_send.send(RtcNegotiationNotification::Failed(RtcPeerConnectionError::IceNegotiationFailure("ICE protocol could not find a valid candidate pair.".to_string()))));
        }
    }
}

impl From<datachannel::SessionDescription> for RtcNegotiationMessage {
    fn from(x: datachannel::SessionDescription) -> Self {
        Self::RemoteSessionDescription(RtcSessionDescription {
            sdp: x.sdp.to_string(),
            sdp_type: match x.sdp_type {
                datachannel::SdpType::Answer => "answer",
                datachannel::SdpType::Offer => "offer",
                datachannel::SdpType::Pranswer => "pranswer",
                datachannel::SdpType::Rollback => "rollback",
            }.to_string()
        })
    }
}

impl From<datachannel::IceCandidate> for RtcNegotiationMessage {
    fn from(x: datachannel::IceCandidate) -> Self {
        Self::RemoteCandidate(RtcIceCandidate {
            candidate: x.candidate,
            sdp_mid: x.mid
        })
    }
}

impl From<RtcIceCandidate> for datachannel::IceCandidate {
    fn from(x: RtcIceCandidate) -> Self {
        Self {
            candidate: x.candidate,
            mid: x.sdp_mid
        }
    }
}

impl From<&IceConfiguration<'_>> for datachannel::RtcConfig {
    fn from(x: &IceConfiguration<'_>) -> Self {
        let mut is = vec!();

        for s in x.ice_servers {
            for u in s.urls {
                if u.starts_with("turn:") {
                    is.push(format_ice_url(&u.replacen("turn:", "", 1), s.username, s.credential));
                }
                else {
                    is.push(u.to_string());
                }
            }
        }

        let mut ret = Self::new(&is[..]);
        ret.ice_transport_policy = x.ice_transport_policy.into();

        ret
    }
}


impl From<RtcIceTransportPolicy> for datachannel::TransportPolicy {
    fn from(x: RtcIceTransportPolicy) -> Self {
        match x {
            RtcIceTransportPolicy::All => Self::All,
            RtcIceTransportPolicy::Relay => Self::Relay
        }
    }
}

fn format_ice_url(url: &str, username: Option<&str>, credential: Option<&str>) -> String {
    let mut en = url.to_string();

    if let Some(x) = username {
        let qr = if let Some(y) = credential { ":".to_string() + y } else { "".to_string() };
        en = x.to_owned() + &qr + "@" + &en;
    }

    "turn:".to_owned() + &en
}

impl From<&RtcDataChannelConfiguration<'_>> for datachannel::DataChannelInit {
    fn from(x: &RtcDataChannelConfiguration<'_>) -> Self {
        Self::default()
            .protocol(x.protocol.as_ref().copied().unwrap_or_default())
            .reliability(match x.reliability {
                RtcReliabilityMode::Reliable() => datachannel::Reliability { unordered: !x.ordered, unreliable: false, max_packet_life_time: 0, max_retransmits: 0 },
                RtcReliabilityMode::MaxRetransmits(t) => datachannel::Reliability { unordered: !x.ordered, unreliable: true, max_packet_life_time: 0, max_retransmits: t },
                RtcReliabilityMode::MaxPacketLifetime(t) => datachannel::Reliability { unordered: !x.ordered, unreliable: true, max_packet_life_time: t.get(), max_retransmits: 0 }
            }).negotiated().manual_stream()
    }
}