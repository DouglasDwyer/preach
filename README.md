# preach
### Platform independent data channels

[![Crates.io](https://img.shields.io/crates/v/preach.svg)](https://crates.io/crates/preach)
[![Docs.rs](https://docs.rs/preach/badge.svg)](https://docs.rs/preach)

Preach provides an abstraction for WebRTC data channels that runs on both native and web platforms. Preach manages the exchange of session descriptions and ICE candidates, making channel creation straightforward. Further, Preach offers a simple synchronous and asynchronous interface for exchanging messages, making it well-suited for a variety of purposes.

To create a WebRTC connection, one must first exchange information with the remote peer through a signaling server. Because signaling servers vary with use case, Preach does not provide a signaling server implementation. However, Preach does automatically dictate the information that should be sent to the signaling server.

## Usage

For a complete example, see [the tests](src/tests.rs).

To use Preach, a signaling server implementation must be supplied. This is accomplished by implementing the `RtcNegotiationHandler` trait:

```rust
/// Negotiates a connection with the remote peer during the initial connection process,
/// exchanging messages with the signaling server. 
pub trait RtcNegotiationHandler {
    /// Sends a negotiation message to the remote peer through a signaling implementation.
    fn send(&mut self, message: RtcNegotiationMessage) -> Pin<Box<dyn '_ + Future<Output = Result<(), RtcPeerConnectionError>>>>;
    /// Checks the signaling server for new negotiation messages from the remote peer.
    fn receive(&mut self) -> Pin<Box<dyn '_ + Future<Output = Result<Vec<RtcNegotiationMessage>, RtcPeerConnectionError>>>>;
}
```

This trait should be implemented so that any messages sent from one peer are received by the other, through the use of a signaling server. The server may otherwise function however the end user desires - for example, it could be implemented as a REST API.

Once a signaling mechanism is provided, creating a new set of channels is easy:

```rust
let ice_configuation = IceConfiguration {
    ice_servers: vec!(RtcIceServer { urls: vec!("stun:stun.l.google.com:19302".to_string()), ..Default::default() }),
    ice_transport_policy: RtcIceTransportPolicy::All
};

let negotiation_handler = ...; // Implementation of RtcNegotiationHandler

// Boths peers must use the same set of RtcDataChannelConfigurations for a connection to be created.
let channel = RtcDataChannel::connect(&ice_configuation, negotiation_handler,
    &[RtcDataChannelConfiguration { label: "chan".to_string(), ..Default::default() }]
).await.expect("An error occured during channel creation.")[0];

let msg = b"test msg";

// Send messages.
channel.send(&msg[..]).expect("Could not send message.");

// Receive messages.
assert_eq!(&msg[..], &channel.receive_async().await.expect("An error occurred on the channel.")[..]);
```

## Optional features

**vendored** - Builds `libdatachannel` and `OpenSSL` statically on desktop platforms, bundling them into the build.

**wasm_main_executor** - Allows for creating data channels within WASM web workers, and sharing them between threads.
By default, restrictions on workers prevent them from instantiating and working with peer channels. This feature sidesteps
said restrictions by deferring work to the main browser thread. When using this feature, `wasm_main_executor::initialize()`
must be called before creating any data channels.
