use crate::*;
use js_sys::*;
use send_wrapper::*;
use std::mem::*;
use std::sync::atomic::*;
use wasm_bindgen::*;
use wasm_bindgen::closure::*;
use wasm_bindgen_futures::*;

/// Provides a browser-backed implementation of a datachannel based upon the `web_sys` crate.
pub struct RtcDataChannelBackendImpl {
    handle: Option<Arc<SendWrapper<RtcDataChannelHandle>>>,
    ready_state: Arc<AtomicU8>,
    receive_waker: Arc<ArcSwapOption<Waker>>,
    #[allow(dead_code)]
    main_message_send: Sender<Vec<u8>>,
    #[allow(dead_code)]
    main_message_recv: Arc<Mutex<Receiver<Vec<u8>>>>
}

impl RtcDataChannelBackendImpl {
    /// Creates a new data channel and receiver from the given handle. Increments the provided open count when the channel opens.
    fn new(handle: web_sys::RtcDataChannel, open_count: Arc<AtomicUsize>) -> (Self, RtcDataChannelMessageReceiver) {
        handle.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        let (event_handlers, state) = RtcDataChannelEventHandlers::new(handle.clone(), open_count);
        let (main_message_send, recv) = channel();

        (Self {
            handle: Some(Arc::new(SendWrapper::new(RtcDataChannelHandle { event_handlers, handle }))),
            ready_state: state.ready_state,
            receive_waker: state.receive_waker,
            main_message_send,
            main_message_recv: Arc::new(Mutex::new(recv))
        }, state.receiver)
    }

    /// Gets a reference to the underlying data channel handle.
    fn handle(&self) -> &Arc<SendWrapper<RtcDataChannelHandle>> {
        self.handle.as_ref().expect("Handle was already disposed.")
    }

    /// Copies the provided message to a separate Javascript array, and then sends it on the given channel.
    #[allow(dead_code)]
    fn copy_send_with_handle(handle: &web_sys::RtcDataChannel, message: &[u8]) -> Result<(), RtcDataChannelError> {
        let buffer = ArrayBuffer::new(message.len().try_into().expect("Message was too large to be sent."));
        Uint8Array::new(&buffer).copy_from(message);
        handle.send_with_array_buffer(&buffer).map_err(|x| RtcDataChannelError::Send(format!("{x:?}")))
    }

    /// Determines whether this is the main browser thread.
    fn is_main_thread() -> bool {
        web_sys::window().is_some()
    }
}

impl RtcDataChannelBackend for RtcDataChannelBackendImpl {
    fn connect<'a>(config: &'a IceConfiguration, negotiator: impl RtcNegotiationHandler, channels: &'a [RtcDataChannelConfiguration]) -> RtcDataChannelConnectionFuture<'a> {
        let config_ref = config.clone();
        let channel_ref = channels.to_vec();
        if Self::is_main_thread() {
            Box::pin(RtcPeerConnector::connect(config_ref, negotiator, channel_ref))
        }
        else {
            Box::pin(spawn(FnFuture(move || RtcPeerConnector::connect(config_ref, negotiator, channel_ref))))
        }
    }

    fn ready_state(&self) -> RtcDataChannelReadyState {
        self.ready_state.load(Ordering::Acquire).try_into().expect("Invalid channel state.")
    }

    fn send(&mut self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        #[cfg(target_feature = "atomics")]
        {
            let handle = self.handle();
            if handle.valid() {
                Self::copy_send_with_handle(&handle.handle, message)
            }
            else {
                drop(self.main_message_send.send(message.to_vec()));
                let handle_ref = handle.clone();
                let recv_ref = self.main_message_recv.clone();
                drop(spawn(async move {
                    if let Some((err, f)) = Self::copy_send_with_handle(&handle_ref.handle,
                        &recv_ref.lock().expect("Could not acquire sending mutex.").try_recv().expect("Could not get next message to send")[..]).err()
                        .and_then(|t| handle_ref.handle.onerror().map(|f| (t, f))) {
                        drop(f.call1(&JsValue::NULL, &JsValue::from_str(&err.to_string())));
                    }
                }));

                Ok(())
            }
        }
        #[cfg(not(target_feature = "atomics"))]
        {
            self.handle().handle.send_with_u8_array(message).map_err(|x| RtcDataChannelError::Send(format!("{x:?}")))
        }
    }

    fn receive_waker(&mut self) -> Arc<ArcSwapOption<Waker>> {
        self.receive_waker.clone()
    }
}

impl Drop for RtcDataChannelBackendImpl {
    fn drop(&mut self) {
        if !self.handle().valid() {
            let handle = take(&mut self.handle).expect("Handle was already disposed.");
            drop(spawn(async move { drop(handle); }));
        }
    }
}

/// Stores the set of data channel objects that must remain on the main thread.
struct RtcDataChannelHandle {
    /// The set of handlers that respond to WebRTC events.
    #[allow(dead_code)]
    pub event_handlers: RtcDataChannelEventHandlers,
    /// The underlying data channel.
    pub handle: web_sys::RtcDataChannel,
}

impl Drop for RtcDataChannelHandle {
    fn drop(&mut self) {
        self.handle.close();
    }
}

#[allow(dead_code)]
/// Responds to events that occur on a data channel.
struct RtcDataChannelEventHandlers {
    handle: web_sys::RtcDataChannel,
    on_close: Closure<dyn FnMut(JsValue)>,
    on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    on_open: Closure<dyn FnMut(JsValue)>,
}

impl RtcDataChannelEventHandlers {
    /// Creates a new set of event handlers for the given handle. The provided count is updated upon channel open.
    pub fn new(handle: web_sys::RtcDataChannel, open_count: Arc<AtomicUsize>) -> (Self, RtcDataChannelHandlerState) {
        let ready_state = Arc::new(AtomicU8::new(RtcDataChannelReadyState::Open as u8));
        let receive_waker = Arc::<ArcSwapOption<Waker>>::default();
        let (sender, receiver) = channel();

        let sender_ref = sender.clone();
        let waker_ref = receive_waker.clone();
        let ready_state_ref = ready_state.clone();
        let on_close = Closure::wrap(Box::new(move |ev: JsValue| { 
            ready_state_ref.store(RtcDataChannelReadyState::Closed as u8, Ordering::Release);
            drop(sender_ref.send(Err(RtcDataChannelError::Receive(format!("{ev:?}")))));
            Self::wake_listener(&waker_ref);
        }) as Box<dyn FnMut(JsValue)>);
        
        let waker_ref = receive_waker.clone();
        let ready_state_ref = ready_state.clone();
        let on_message = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
            drop(sender.send(match ev.data().dyn_into::<ArrayBuffer>() {
                Ok(data) => Ok(Uint8Array::new(&data).to_vec().into_boxed_slice()),
                Err(data) => {
                    ready_state_ref.store(RtcDataChannelReadyState::Closed as u8, Ordering::Release);
                    Err(RtcDataChannelError::Receive(format!("{data:?}")))
                },
            }));
            Self::wake_listener(&waker_ref);
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);

        let on_open = Closure::wrap(Box::new(move |_: JsValue| { open_count.fetch_add(1, Ordering::AcqRel); }) as Box<dyn FnMut(JsValue)>);
        
        handle.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        handle.set_onerror(Some(on_close.as_ref().unchecked_ref()));
        handle.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        handle.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        
        (Self { handle, on_close, on_message, on_open }, RtcDataChannelHandlerState { ready_state, receive_waker, receiver })
    }

    /// Wakes the provided listener, if any.
    fn wake_listener(waker: &ArcSwapOption<Waker>) {
        if let Some(waker) = &*waker.load() {
            waker.wake_by_ref();
        }
    }
}

impl Drop for RtcDataChannelEventHandlers {
    fn drop(&mut self) {
        self.handle.set_onmessage(None);
        self.handle.set_onclose(None);
        self.handle.set_onerror(None);
        self.handle.set_onopen(None);
    }
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

/// Stores a list of data channels that are being opened
struct RtcDataChannelList {
    /// A list of channels, ordered by ID.
    pub channels: Vec<RtcDataChannel>,
    /// The total number of channels that have successfully opened so far.
    pub open_count: Arc<AtomicUsize>
}

/// Notifies the local connector about an event during connection establishment.
#[derive(Clone, Debug)]
enum RtcNegotiationNotification {
    /// A message should be sent to the remote peer.
    SendMessage(RtcNegotiationMessage),
    /// The connection failed with an error.
    Failed(RtcPeerConnectionError)
}

/// Manages the establishment of a connection with a remote peer.
struct RtcPeerConnector<N: RtcNegotiationHandler> {
    channel_configurations: Vec<RtcDataChannelConfiguration>,
    #[allow(dead_code)]
    event_handlers: RtcPeerConnectionEventHandlers,
    handle: web_sys::RtcPeerConnection,
    negotiator: N,
    negotiation_receive: Receiver<RtcNegotiationNotification>
}

impl<N: RtcNegotiationHandler> RtcPeerConnector<N> {
    /// Creates a new connector for the given configuration, negotiator, and channels.
    pub async fn connect(configuration: IceConfiguration, negotiator: N, channel_configurations: Vec<RtcDataChannelConfiguration>) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let (negotiation_send, negotiation_receive) = channel();
        let handle = web_sys::RtcPeerConnection::new_with_configuration(&configuration.into()).map_err(|x| RtcPeerConnectionError::Creation(format!("{x:?}")))?;
        let event_handlers = RtcPeerConnectionEventHandlers::new(handle.clone(), negotiation_send);

        Self { channel_configurations, event_handlers, handle, negotiator, negotiation_receive }.accept_connections().await
    }

    /// Performs ICE with the remote peer and attempts to create a set of data channels.
    async fn accept_connections(&mut self) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let channels = self.create_channels()?;
        let local_description = self.create_offer().await?;
        self.negotiator.send(RtcNegotiationMessage::RemoteSessionDescription(local_description.clone())).await?;
        self.negotiate(&channels, &local_description).await?;        
        Ok(channels.channels.into_boxed_slice())
    }

    /// Creates the set of data channels described by the current configuration.
    fn create_channels(&mut self) -> Result<RtcDataChannelList, RtcPeerConnectionError> {
        let mut channels = Vec::new();
        let open_count = Arc::<AtomicUsize>::default();

        for config in &self.channel_configurations {
            let (handler, receiver) = RtcDataChannelBackendImpl::new(
                self.handle.create_data_channel_with_data_channel_dict(&config.label, web_sys::RtcDataChannelInit::from(config).id(channels.len() as u16)), open_count.clone());

            channels.push(RtcDataChannel::new(handler, config.label.clone(), channels.len() as u16, receiver));
        }

        Ok(RtcDataChannelList { channels, open_count })
    }

    /// Creates a connection offer to send to the remote host.
    async fn create_offer(&mut self) -> Result<RtcSessionDescription, RtcPeerConnectionError> {
        let offer = JsFuture::from(self.handle.create_offer_with_rtc_offer_options(&web_sys::RtcOfferOptions::new())).await
            .map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?.unchecked_into::<web_sys::RtcSessionDescription>();

        Ok(RtcSessionDescription {
            sdp_type: match offer.type_() {
                web_sys::RtcSdpType::Answer => "answer",
                web_sys::RtcSdpType::Offer => "offer",
                web_sys::RtcSdpType::Pranswer => "pranswer",
                web_sys::RtcSdpType::Rollback => "rollback",
                _ => Err(RtcPeerConnectionError::Negotiation("Unexpected SDP type".to_string()))?
            }.to_string(),
            sdp: offer.sdp(),
        })
    }

    /// Exchanges messages with the remote peer until an error occurs or a connection is established.
    async fn negotiate(&mut self, channels: &RtcDataChannelList, local_description: &RtcSessionDescription) -> Result<(), RtcPeerConnectionError> {
        let mut candidate_buffer = Vec::new();

        while channels.open_count.load(Ordering::Acquire) < channels.channels.len() {
            if let Ok(n) = self.negotiation_receive.try_recv() {
                match n {
                    RtcNegotiationNotification::SendMessage(m) => self.negotiator.send(m).await?,
                    RtcNegotiationNotification::Failed(m) => return Err(m)
                }
            }
            else {
                for m in self.negotiator.receive().await? {
                    self.receive_negotiation_message(m, &mut candidate_buffer, local_description).await?;
                }
            }

            PollFuture::once().await;
        }

        Ok(())
    }

    /// Interprets a negotiation message from the remote peer and handles the result.
    async fn receive_negotiation_message(&mut self, message: RtcNegotiationMessage, candidate_buffer: &mut Vec<RtcIceCandidate>, local_description: &RtcSessionDescription) -> Result<(), RtcPeerConnectionError> {
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
                    if c.sdp_type == "offer" {
                        if local_description.sdp < c.sdp {
                            self.handle_remote_offer(c, candidate_buffer).await?;
                        }
                    }
                    else {
                        self.set_local_description(local_description).await?;
                        self.handle_remote_answer(c, candidate_buffer).await?;
                    }
                }

                Ok(())
            },
        }
    }

    /// Handles the receipt of a remote offer, creating a local answer and adding any known ICE candidates to the connection.
    async fn handle_remote_offer(&mut self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
        description.sdp(&c.sdp);
        JsFuture::from(self.handle.set_remote_description(&description)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        let answer = JsFuture::from(self.handle.create_answer()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?;
        let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp")).unwrap().as_string().unwrap();
        let mut answer_obj = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
        answer_obj.sdp(&answer_sdp);
        JsFuture::from(self.handle.set_local_description(&answer_obj)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?;
        self.negotiator.send(RtcNegotiationMessage::RemoteSessionDescription(RtcSessionDescription {
            sdp_type: "answer".into(),
            sdp: answer_sdp,
        })).await?;

        Ok(())
    }

    /// Handles the receipt of a remote answer, setting the remote description and adding any known ICE candidates to the connection.
    async fn handle_remote_answer(&mut self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::from_js_value(&JsValue::from_str(&c.sdp_type)).unwrap());
        description.sdp(&c.sdp);
        JsFuture::from(self.handle.set_remote_description(&description)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        Ok(())
    }

    /// Sets the local description for an offer.
    async fn set_local_description(&mut self, local_description: &RtcSessionDescription) -> Result<(), RtcPeerConnectionError> {
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
        description.sdp(&local_description.sdp);
        JsFuture::from(self.handle.set_local_description(&description.clone().unchecked_into())).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?;
        Ok(())
    }

    /// Determines whether this connector has a remote description yet.
    fn has_remote_description(&self) -> bool {
        self.handle.remote_description().is_some()
    }

    /// Adds a remote ICE candidate to the peer connection.
    async fn add_remote_candidate(&mut self, c: RtcIceCandidate) -> Result<(), RtcPeerConnectionError> {
        let mut cand = web_sys::RtcIceCandidateInit::new(&c.candidate);
        cand.sdp_mid(Some(&c.sdp_mid));
        JsFuture::from(self.handle.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand))).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{x:?}")))?;

        Ok(())
    }
}

/// Responds to events that occur on a new peer connection.
struct RtcPeerConnectionEventHandlers {
    handle: web_sys::RtcPeerConnection,
    _on_ice_candidate: Closure<dyn FnMut(web_sys::RtcPeerConnectionIceEvent)>,
    _on_ice_connection_state_change: Closure<dyn FnMut(JsValue)>,
}

impl RtcPeerConnectionEventHandlers {
    /// Creates a new set of event handlers that send messages to the provided sink.
    pub fn new(handle: web_sys::RtcPeerConnection, negotiation_send: Sender<RtcNegotiationNotification>) -> Self {
        let ns = negotiation_send.clone();
        let _on_ice_candidate = Closure::wrap(Box::new(move |ev: web_sys::RtcPeerConnectionIceEvent| {
            if let Some(candidate) = ev.candidate() {
                drop(ns.send(RtcNegotiationNotification::SendMessage(
                    RtcNegotiationMessage::RemoteCandidate(RtcIceCandidate {
                        candidate: candidate.candidate(),
                        sdp_mid: candidate.sdp_mid().unwrap_or("".to_string())
                    })
                )));
            }
            else {
                drop(ns.send(RtcNegotiationNotification::SendMessage(
                    RtcNegotiationMessage::RemoteCandidate(RtcIceCandidate {
                        candidate: "".to_string(),
                        sdp_mid: "".to_string()
                    })
                )));
            }
        }) as Box<dyn FnMut(web_sys::RtcPeerConnectionIceEvent)>);
        handle.set_onicecandidate(Some(_on_ice_candidate.as_ref().unchecked_ref()));

        let han = handle.clone();
        let _on_ice_connection_state_change = Closure::wrap(Box::new(move |_: JsValue| {
            let evs = Reflect::get(&han, &JsString::from("connectionState")).map(|x| x.as_string().unwrap_or("".to_string())).unwrap_or("".to_string());
            if evs.as_str() == "failed" {
                drop(negotiation_send.send(RtcNegotiationNotification::Failed(RtcPeerConnectionError::IceNegotiationFailure("ICE protocol could not find a valid candidate pair.".to_string()))));
            }
        }) as Box<dyn FnMut(JsValue)>);
        Reflect::set(&handle, &JsString::from("onconnectionstatechange"), _on_ice_connection_state_change.as_ref().unchecked_ref()).unwrap();

        Self { handle, _on_ice_candidate, _on_ice_connection_state_change }
    }
}

impl Drop for RtcPeerConnectionEventHandlers {
    fn drop(&mut self) {
        self.handle.set_onicecandidate(None);
        self.handle.set_ondatachannel(None);
        Reflect::set(&self.handle, &JsString::from("onconnectionstatechange"), &JsValue::UNDEFINED).unwrap();
    }
}

struct FnFuture<F: Future, T: FnOnce() -> F>(T);

impl<F: Future, T: FnOnce() -> F> IntoFuture for FnFuture<F, T> {
    type Output = F::Output;
    type IntoFuture = F;

    fn into_future(self) -> Self::IntoFuture {
        (self.0)()
    }
}

#[cfg(feature = "wasm_main_executor")]
/// Spawns a new task for the main thread executor, or panics if the executor
/// feature is not enabled.
pub fn spawn<F: 'static + IntoFuture + Send>(f: F) -> impl Future<Output = F::Output> + Send + Sync where F::Output: Send {
    wasm_main_executor::spawn(f)
}

#[cfg(not(feature = "wasm_main_executor"))]
/// Spawns a new task for the main thread executor, or panics if the executor
/// feature is not enabled.
pub fn spawn<F: 'static + IntoFuture + Send>(_: F) -> F::IntoFuture {
    panic!("Attempted to create data channel on worker thread without enabling the wasm_main_executor feature.")
}

impl From<IceConfiguration> for web_sys::RtcConfiguration {
    fn from(config: IceConfiguration) -> Self {
        let ice_servers = Array::new();
        for s in config.ice_servers {
            let urls = Array::new();

            for u in &s.urls {
                urls.push(&JsString::from(u.as_str()));
            }

            let mut ice_server = web_sys::RtcIceServer::new();
            ice_server.urls(&urls);
            
            if let Some(u) = &s.username {
                ice_server.username(u);
            }

            if let Some(u) = &s.credential {
                ice_server.credential(u);
            }

            ice_servers.push(&ice_server);
        }

        let mut ret = web_sys::RtcConfiguration::new();
        ret.ice_servers(&ice_servers);
        ret.ice_transport_policy(config.ice_transport_policy.into());

        ret
    }
}

impl From<RtcIceTransportPolicy> for web_sys::RtcIceTransportPolicy {
    fn from(x: RtcIceTransportPolicy) -> Self {
        match x {
            RtcIceTransportPolicy::All => web_sys::RtcIceTransportPolicy::All,
            RtcIceTransportPolicy::Relay => web_sys::RtcIceTransportPolicy::Relay
        }
    }
}

impl From<&RtcDataChannelConfiguration> for web_sys::RtcDataChannelInit {
    fn from(x: &RtcDataChannelConfiguration) -> Self {
        let mut ret = web_sys::RtcDataChannelInit::new();
        
        ret.ordered(x.ordered);
        ret.negotiated(true);
        
        if let Some(i) = &x.protocol { ret.protocol(i); }

        match x.reliability {
            RtcReliabilityMode::MaxPacketLifetime(i) => { ret.max_packet_life_time(i.get()); },
            RtcReliabilityMode::MaxRetransmits(i) => { ret.max_retransmits(i); },
            _ => {}
        };

        ret
    }
}