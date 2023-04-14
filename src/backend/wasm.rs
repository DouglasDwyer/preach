use crate::*;
use js_sys::*;
use std::sync::atomic::*;
use wasm_bindgen::*;
use wasm_bindgen::closure::*;

pub struct RtcDataChannelBackendImpl {
    _event_handlers: RtcDataChannelEventHandlers,
    handle: web_sys::RtcDataChannel,
    ready_state: Arc<AtomicU8>,
    receive_waker: Arc<ArcSwapOption<Waker>>
}

impl RtcDataChannelBackendImpl {
    fn new(handle: web_sys::RtcDataChannel) -> Self {
        todo!()
    }
}

impl RtcDataChannelBackend for RtcDataChannelBackendImpl {
    fn connect<'a>(config: &'a IceConfiguration<'a>, negotiator: impl 'a + RtcNegotiationHandler, channels: &'a [RtcDataChannelConfiguration<'a>]) -> Pin<Box<dyn 'a + Future<Output = Result<Box<[RtcDataChannel]>, RtcPeerConnectionError>>>> {
        todo!()
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
    _on_close: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_open: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
}

impl RtcDataChannelEventHandlers {
    pub fn new(handle: web_sys::RtcDataChannel, open_count: Arc<AtomicUsize>) -> (Self, RtcDataChannelHandlerState) {
        let ready_state = Arc::<AtomicU8>::default();
        let receive_waker = Arc::<ArcSwapOption<Waker>>::default();
        let (sender, receiver) = channel();

        let sender_ref = sender.clone();
        let waker_ref = receive_waker.clone();
        let on_close = Closure::wrap(Box::new(move |ev: wasm_bindgen::JsValue| { 
            drop(sender_ref.send(Err(RtcDataChannelError::Receive(format!("{:?}", ev)))));
            Self::wake_listener(&waker_ref);
        }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);
        
        let sender_ref = sender.clone();
        let waker_ref = receive_waker.clone();
        let on_message = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
            drop(sender_ref.send(match ev.data().dyn_into::<ArrayBuffer>() {
                Ok(data) => Ok(Uint8Array::new(&data).to_vec().into_boxed_slice()),
                Err(data) => Err(RtcDataChannelError::Receive(format!("{:?}", data))),
            }));
            Self::wake_listener(&waker_ref);
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);

        let on_open = Closure::wrap(Box::new(move |_: wasm_bindgen::JsValue| { open_count.fetch_add(1, Ordering::AcqRel); }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);

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