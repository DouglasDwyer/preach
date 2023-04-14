use crate::*;
use js_sys::*;
use std::sync::atomic::*;
use wasm_bindgen::*;
use wasm_bindgen::closure::*;
use wasm_bindgen_futures::*;

pub struct RtcDataChannelBackendImpl {
    _event_handlers: RtcDataChannelEventHandlers,
    handle: web_sys::RtcDataChannel,
    ready_state: Arc<AtomicU8>,
    receive_waker: Arc<ArcSwapOption<Waker>>
}

impl RtcDataChannelBackendImpl {
    fn new(handle: web_sys::RtcDataChannel, open_count: Arc<AtomicUsize>) -> (Self, Receiver<Result<Box<[u8]>, RtcDataChannelError>>) {
        let (event_handlers, state) = RtcDataChannelEventHandlers::new(handle.clone(), open_count);

        (Self {
            _event_handlers: event_handlers,
            handle,
            ready_state: state.ready_state,
            receive_waker: state.receive_waker
        }, state.receiver)
    }
}

impl RtcDataChannelBackend for RtcDataChannelBackendImpl {
    fn connect<'a>(config: &'a IceConfiguration<'a>, negotiator: impl 'a + RtcNegotiationHandler, channels: &'a [RtcDataChannelConfiguration<'a>]) -> Pin<Box<dyn 'a + Future<Output = Result<Box<[RtcDataChannel]>, RtcPeerConnectionError>>>> {
        Box::pin(RtcPeerConnector::connect(config, negotiator, channels))
    }

    fn ready_state(&self) -> RtcDataChannelReadyState {
        self.ready_state.load(Ordering::Acquire).try_into().expect("Invalid channel state.")
    }

    fn send(&mut self, message: &[u8]) -> Result<(), RtcDataChannelError> {
        #[cfg(target_feature = "atomics")]
        {
            let buffer = ArrayBuffer::new(message.len().try_into().expect("Message was too large to be sent."));
            Uint8Array::new(&buffer).copy_from(message);
            self.handle.send_with_array_buffer(&buffer).map_err(|x| RtcDataChannelError::Send(format!("{:?}", x)))
        }
        #[cfg(not(target_feature = "atomics"))]
        {
            self.handle.send_with_u8_array(message).map_err(|x| RtcDataChannelError::Send(format!("{:?}", x)))
        }
    }

    fn receive_waker(&mut self) -> Arc<ArcSwapOption<Waker>> {
        self.receive_waker.clone()
    }
}

#[derive(Debug)]
struct RtcDataChannelHandlerState {
    pub ready_state: Arc<AtomicU8>,
    pub receive_waker: Arc<ArcSwapOption<Waker>>,
    pub receiver: Receiver<Result<Box<[u8]>, RtcDataChannelError>>
}

struct RtcDataChannelEventHandlers {
    handle: web_sys::RtcDataChannel,
    _on_close: Closure<dyn FnMut(JsValue)>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_open: Closure<dyn FnMut(JsValue)>,
}

impl RtcDataChannelEventHandlers {
    pub fn new(handle: web_sys::RtcDataChannel, open_count: Arc<AtomicUsize>) -> (Self, RtcDataChannelHandlerState) {
        let ready_state = Arc::<AtomicU8>::default();
        let receive_waker = Arc::<ArcSwapOption<Waker>>::default();
        let (sender, receiver) = channel();

        let sender_ref = sender.clone();
        let waker_ref = receive_waker.clone();
        let on_close = Closure::wrap(Box::new(move |ev: JsValue| { 
            drop(sender_ref.send(Err(RtcDataChannelError::Receive(format!("{:?}", ev)))));
            Self::wake_listener(&waker_ref);
        }) as Box<dyn FnMut(JsValue)>);
        
        let sender_ref = sender.clone();
        let waker_ref = receive_waker.clone();
        let on_message = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
            drop(sender_ref.send(match ev.data().dyn_into::<ArrayBuffer>() {
                Ok(data) => Ok(Uint8Array::new(&data).to_vec().into_boxed_slice()),
                Err(data) => Err(RtcDataChannelError::Receive(format!("{:?}", data))),
            }));
            Self::wake_listener(&waker_ref);
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);

        let on_open = Closure::wrap(Box::new(move |_: JsValue| { open_count.fetch_add(1, Ordering::AcqRel); }) as Box<dyn FnMut(JsValue)>);

        handle.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        handle.set_onerror(Some(on_close.as_ref().unchecked_ref()));
        handle.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        handle.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        
        (Self { handle, _on_close: on_close, _on_message: on_message, _on_open: on_open }, RtcDataChannelHandlerState { ready_state, receive_waker, receiver })
    }

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

struct RtcDataChannelList {
    pub channels: Vec<RtcDataChannel>,
    pub open_count: Arc<AtomicUsize>
}

enum RtcNegotiationNotification {
    SendMessage(RtcNegotiationMessage),
    Failed(RtcPeerConnectionError)
}

struct RtcPeerConnector<'a, N: RtcNegotiationHandler> {
    channel_configurations: &'a [RtcDataChannelConfiguration<'a>],
    _event_handlers: RtcPeerConnectionEventHandlers,
    handle: web_sys::RtcPeerConnection,
    negotiator: N,
    negotiation_receive: Receiver<RtcNegotiationNotification>
}

impl<'a, N: RtcNegotiationHandler> RtcPeerConnector<'a, N> {
    pub async fn connect(configuration: &'a IceConfiguration<'a>, negotiator: N, channel_configurations: &'a [RtcDataChannelConfiguration<'a>]) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let (negotiation_send, negotiation_receive) = channel();
        let handle = web_sys::RtcPeerConnection::new_with_configuration(&configuration.into()).map_err(|x| RtcPeerConnectionError::Creation(format!("{:?}", x)))?;
        let event_handlers = RtcPeerConnectionEventHandlers::new(handle.clone(), negotiation_send);

        Self { channel_configurations, _event_handlers: event_handlers, handle, negotiator, negotiation_receive }.accept_connections().await
    }

    async fn accept_connections(&mut self) -> Result<Box<[RtcDataChannel]>, RtcPeerConnectionError> {
        let channels = self.create_channels()?;
        let local_description = self.create_offer().await?;
        self.negotiator.send(RtcNegotiationMessage::RemoteSessionDescription(local_description.clone())).await?;
        self.negotiate(&channels, &local_description).await?;        
        Ok(channels.channels.into_boxed_slice())
    }

    fn create_channels(&mut self) -> Result<RtcDataChannelList, RtcPeerConnectionError> {
        let mut channels = Vec::new();
        let open_count = Arc::<AtomicUsize>::default();

        for config in self.channel_configurations {
            let (handler, receiver) = RtcDataChannelBackendImpl::new(
                self.handle.create_data_channel_with_data_channel_dict(config.label, &web_sys::RtcDataChannelInit::from(config).id(channels.len() as u16)), open_count.clone());

            channels.push(RtcDataChannel::new(handler, config.label.to_string(), channels.len() as u16, receiver));
        }

        Ok(RtcDataChannelList { channels, open_count })
    }

    async fn create_offer(&mut self) -> Result<RtcSessionDescription, RtcPeerConnectionError> {
        let offer = JsFuture::from(self.handle.create_offer_with_rtc_offer_options(&web_sys::RtcOfferOptions::new())).await
            .map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?.unchecked_into::<web_sys::RtcSessionDescription>();
        JsFuture::from(self.handle.set_local_description(&offer.clone().unchecked_into())).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

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
                    self.receive_negotiation_message(m, &mut candidate_buffer, &local_description).await?;
                }
            }

            PollFuture::once().await;
        }

        Ok(())
    }

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
                        self.handle_remote_answer(c, candidate_buffer).await?;
                    }
                }

                Ok(())
            },
        }
    }

    async fn handle_remote_offer(&mut self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
        description.sdp(&c.sdp);
        JsFuture::from(self.handle.set_remote_description(&description)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        let answer = JsFuture::from(self.handle.create_answer()).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;
        let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp")).unwrap().as_string().unwrap();
        let mut answer_obj = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
        answer_obj.sdp(&answer_sdp);
        JsFuture::from(self.handle.set_local_description(&answer_obj)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;
        self.negotiator.send(RtcNegotiationMessage::RemoteSessionDescription(RtcSessionDescription {
            sdp_type: "answer".into(),
            sdp: answer_sdp,
        })).await?;

        Ok(())
    }

    async fn handle_remote_answer(&mut self, c: RtcSessionDescription, candidate_buffer: &mut Vec<RtcIceCandidate>) -> Result<(), RtcPeerConnectionError> {
        let mut description = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::from_js_value(&JsValue::from_str(&c.sdp_type)).unwrap());
        description.sdp(&c.sdp);
        JsFuture::from(self.handle.set_remote_description(&description)).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        for cand in candidate_buffer.drain(..) {
            self.add_remote_candidate(cand).await?;
        }

        Ok(())
    }

    fn has_remote_description(&self) -> bool {
        self.handle.remote_description().is_some()
    }

    async fn add_remote_candidate(&mut self, c: RtcIceCandidate) -> Result<(), RtcPeerConnectionError> {
        let mut cand = web_sys::RtcIceCandidateInit::new(&c.candidate);
        cand.sdp_mid(Some(&c.sdp_mid));
        JsFuture::from(self.handle.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand))).await.map_err(|x| RtcPeerConnectionError::Negotiation(format!("{:?}", x)))?;

        Ok(())
    }
}

struct RtcPeerConnectionEventHandlers {
    handle: web_sys::RtcPeerConnection,
    _on_ice_candidate: Closure<dyn FnMut(web_sys::RtcPeerConnectionIceEvent)>,
    _on_ice_connection_state_change: Closure<dyn FnMut(JsValue)>,
}

impl RtcPeerConnectionEventHandlers {
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
        let ns = negotiation_send.clone();
        let _on_ice_connection_state_change = Closure::wrap(Box::new(move |_: JsValue| {
            let evs = Reflect::get(&han, &JsString::from("connectionState")).map(|x| x.as_string().unwrap_or("".to_string())).unwrap_or("".to_string());
            if evs.as_str() == "failed" {
                drop(ns.send(RtcNegotiationNotification::Failed(RtcPeerConnectionError::IceNegotiationFailure("ICE protocol could not find a valid candidate pair.".to_string()))));
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

impl From<&IceConfiguration<'_>> for web_sys::RtcConfiguration {
    fn from(config: &IceConfiguration<'_>) -> Self {
        let ice_servers = Array::new();
        for s in config.ice_servers {
            let urls = Array::new();

            for &u in s.urls {
                urls.push(&JsString::from(u));
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

impl From<&RtcDataChannelConfiguration<'_>> for web_sys::RtcDataChannelInit {
    fn from(x: &RtcDataChannelConfiguration<'_>) -> Self {
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