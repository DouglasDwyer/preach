use crate::*;

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
        ice_servers: vec!(RtcIceServer { urls: vec!("stun:stun.l.google.com:19302".to_string()), ..Default::default() }),
        ice_transport_policy: RtcIceTransportPolicy::All
    };

    RtcDataChannel::connect(&ice_configuation, handler,
        &[RtcDataChannelConfiguration { label: "chan".to_string(), ..Default::default() }]
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
        assert!(&res[..] == &msg[..], "Expected {res:?} but got {msg:?}");
    }).await;
}