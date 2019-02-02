#![allow(unused)]

extern crate bytecodec;
#[macro_use]
extern crate stun_codec;
#[macro_use]
extern crate trackable;
extern crate rand;

extern crate futures;
extern crate tokio_udp;
extern crate tokio_timer;

use stun_codec::{MessageDecoder, MessageEncoder};

use bytecodec::{DecodeExt, EncodeExt};
use std::net::{SocketAddr};
use stun_codec::rfc5389::attributes::{
    Software,
    Realm,
    Nonce,
    ErrorCode,
    MessageIntegrity,
    Username,
    XorMappedAddress,
    AlternateServer,
};
use stun_codec::rfc5766::attributes::{
    RequestedTransport,
    XorRelayAddress,
    Lifetime,
};
//use stun_codec::rfc5389::{Attribute as StunAttribute};
//use stun_codec::rfc5766::{Attribute as TurnAttribute};
use stun_codec::rfc5766::methods::{ALLOCATE};
use stun_codec::{Message, MessageClass, TransactionId};
use std::time::{Instant,Duration};
use self::attrs::Attribute;

use futures::{Stream, Sink, Future, Poll, Async};

mod attrs { 
    extern crate stun_codec;
    // Taken from rusturn/src/attribute.rs
    use stun_codec::rfc5389::attributes::*;
    use stun_codec::rfc5766::attributes::*;

    define_attribute_enums!(
        Attribute,
        AttributeDecoder,
        AttributeEncoder,
        [
            // RFC 5389
            MappedAddress,
            Username,
            MessageIntegrity,
            ErrorCode,
            UnknownAttributes,
            Realm,
            Nonce,
            XorMappedAddress,
            Software,
            AlternateServer,
            Fingerprint,
            // RFC 5766
            ChannelNumber,
            Lifetime,
            XorPeerAddress,
            Data,
            XorRelayAddress,
            EvenPort,
            RequestedTransport,
            DontFragment,
            ReservationToken
        ]
    );

}

/// Primitive error handling used in this library.
/// File an issue if you don't like it.
pub type Error = Box<dyn std::error::Error>;

use tokio_udp::UdpSocket;

use tokio_timer::Interval;



/// Options for connecting to TURN server
pub struct TurnClientBuilder {
    /// Address of the TURN server
    pub turn_server: SocketAddr,
    /// Username for TURN authentication
    pub username: String,
    /// Password for TURN authentication
    pub password: String,

    /// "End-to-end" timeout for the initial allocation operation.
    pub alloc_timeout: Duration,
    /// How often to repeat varions requests
    pub retry_interval: Duration,
    /// How often to renew the allocation
    pub refresh_interval: Duration,
    /// `SOFTWARE` attribute value in requests
    /// None means no attribute
    pub software: Option<&'static str>,
}

impl TurnClientBuilder {
    /// A constructor with obligatory parameters
    pub fn new(turn_server: SocketAddr, username: String, password: String) -> Self {
        TurnClientBuilder {
            turn_server,
            username,
            password,
            alloc_timeout: Duration::from_secs(15),
            retry_interval: Duration::from_secs(1),
            refresh_interval: Duration::from_secs(60),
            software: Some("SimpleRustTurnClient"),
        }
    }

    // too lazy to bring in builder pattern methods now

    pub fn build_and_send_request(self, udp: UdpSocket) -> impl Future<Item=TurnClient, Error=Error> {
        let retry_timer = Interval::new_interval(self.retry_interval);
        let tc = TurnClient{
            opts: self,
            udp,
            retry_timer,
            trans_id: gen_transaction_id(),
            data_to_be_sent_while_polling: None,
            allocation_lifetime: None,
            realm: None,
            nonce: None,
        };
        futures::future::ok(tc).and_then(|mut tc| {
            let _ = tc.send_allocate_request();
            futures::future::ok(tc)
        })
    }
}

#[derive(Debug)]
pub enum MessageFromTurnServer {
    /// This variant can be safely ignored
    APacketIsReceivedAndAutomaticallyHandled,
    
    AllocationGranted {
        relay_address: SocketAddr,
        mapped_address: SocketAddr,
        server_software: Option<String>,
    },

    RedirectedToAlternateServer(SocketAddr),

    /// A packet from wrong address or an unexpected STUN/TURN message
    ForeignPacket(SocketAddr, Vec<u8>),
}

pub struct TurnClient {
    opts: TurnClientBuilder,
    udp: UdpSocket,
    retry_timer: Interval,
    trans_id: TransactionId,
    data_to_be_sent_while_polling: Option<Vec<u8>>,
    /// None means not yet allocated
    allocation_lifetime: Option<Instant>,
    realm: Option<Realm>,
    nonce: Option<Nonce>,
}

impl TurnClient {
    /// Consume this TURN client, returning back control of used UDP socket
    pub fn into_udp_socket(self) -> UdpSocket {
        self.udp
    }
}

fn gen_transaction_id() -> TransactionId {
    use rand::Rng;
    let random_bytes = rand::thread_rng().gen::<[u8; 12]>();
    TransactionId::new(random_bytes)
}

impl TurnClient {
    /// Send allocate or refresh request
    fn send_allocate_request(&mut self) -> Result<(), Error> {
        assert!(self.data_to_be_sent_while_polling == None);
        let mut message : Message<Attribute> = Message::new(MessageClass::Request, ALLOCATE, self.trans_id);
              
        if let Some(s) = self.opts.software {
            message.add_attribute(Attribute::Software(Software::new(
                s.to_owned(),
            )?));
        }
        
        message.add_attribute(Attribute::RequestedTransport(
            RequestedTransport::new(17 /* UDP */)
        ));
        

        let username = Username::new(self.opts.username.clone())?;
        message.add_attribute(Attribute::Username(
            username.clone()
        ));

        if let (Some(re), Some(no)) = (self.realm.clone(), self.nonce.clone()) {
            message.add_attribute(Attribute::Realm(re.clone()));
            message.add_attribute(Attribute::Nonce(no));
        
            message.add_attribute(Attribute::MessageIntegrity(
                MessageIntegrity::new_long_term_credential(
                        &message, 
                        &username,
                        &re,
                        self.opts.password.as_str())?
            ));
        }
        

        // Encodes the message
        let mut encoder = MessageEncoder::new();
        let bytes = encoder.encode_into_bytes(message.clone())?;

        match self.udp.poll_send_to(&bytes[..], &self.opts.turn_server) {
            Ok(Async::NotReady) => self.data_to_be_sent_while_polling = Some(bytes),
            Err(e) => Err(e)?,
            Ok(Async::Ready(len)) => assert_eq!(len, bytes.len()),
        }

        Ok(())
    }

    /// Handle incoming packet from TURN server
    fn handle_incoming_packet(&mut self, buf:&[u8]) -> Result<MessageFromTurnServer, Error> {
        use self::MessageFromTurnServer::*;

        if self.allocation_lifetime.is_none() {
            let mut decoder = MessageDecoder::<Attribute>::new();

            let decoded = decoder
                .decode_from_bytes(buf)?
                .map_err(|_| format!("Broken TURN reply"))?;
            if decoded.transaction_id() != self.trans_id {
                return Ok(ForeignPacket(self.opts.turn_server, buf.to_vec()));
            }
            
            use stun_codec::MessageClass::{SuccessResponse, ErrorResponse, Indication, Request};
            match decoded.class() {
                SuccessResponse => {
                    let ra = decoded.get_attribute::<XorRelayAddress>().ok_or("No XorRelayAddress in reply")?;
                    let ma = decoded.get_attribute::<XorMappedAddress>().ok_or("No XorMappedAddress in reply")?;
                    let sw = decoded.get_attribute::<Software>().as_ref().map(|x|x.description());
                    let lt = decoded.get_attribute::<Lifetime>().ok_or("No Lifetime in reply")?;

                    /* Big mode change */
                    self.allocation_lifetime = Some(Instant::now() + lt.lifetime());
                    /* Big mode echange */

                    let ret = AllocationGranted {
                        relay_address: ra.address(),
                        mapped_address: ma.address(),
                        server_software: sw.map(|x|x.to_owned()),
                    };
                    return Ok(ret)
                },
                ErrorResponse => {
                    let ec = decoded.get_attribute::<ErrorCode>()
                            .ok_or("ErrorResponse without ErrorCode?")?.code();

                    match ec {
                        401 => {
                            if self.nonce.is_some() {
                                Err("Authentication failed")?;
                            }

                            let re = decoded.get_attribute::<Realm>()
                                    .ok_or("Missing Realm in NotAuthorized response")?;
                            let no = decoded.get_attribute::<Nonce>()
                                    .ok_or("Missing Nonce in NotAuthorized response")?;
                            
                            self.realm = Some(re.clone());
                            self.nonce = Some(no.clone());

                            self.trans_id = gen_transaction_id();
                            self.send_allocate_request();
                        },
                        300 => {
                            let ta = decoded.get_attribute::<AlternateServer>()
                                    .ok_or("Redirect without AlternateServer")?;
                            return Ok(RedirectedToAlternateServer(ta.address()));
                        },
                        _ => {
                            Err(format!("Unknown error code from TURN: {}", ec))?;
                        }
                    }
                },
                Indication => {
                    Err("Indications are not expected in this state")?
                },
                Request => {
                    Err("Received a Request instead of Response from server")?
                },
            }
        } else {
            Err("Not implemented: life after allocation")?
            // TODO
        }
        
        Ok(MessageFromTurnServer::APacketIsReceivedAndAutomaticallyHandled)
    }
}

impl Stream for TurnClient {
    type Error = Error;
    type Item = MessageFromTurnServer;

    fn poll(&mut self) -> Poll<Option<MessageFromTurnServer>, Error> {
        // Receiving things is the first priority
        loop {
            let mut buf = [0; 512];
            match self.udp.poll_recv_from(&mut buf[..]) {
                Err(e) => Err(e)?,
                Ok(Async::NotReady) => break,
                Ok(Async::Ready((len, addr))) => {
                    let buf = &buf[0..len];
                    if addr != self.opts.turn_server {
                        return Ok(Async::Ready(Some(MessageFromTurnServer::ForeignPacket(addr,buf.to_vec()))));
                    }
                    let ret = self.handle_incoming_packet(buf)?;
                    return Ok(Async::Ready(Some(ret)));
                },
            }
        }

        // Then comes sending debt
        if self.data_to_be_sent_while_polling.is_some() {
            let d = self.data_to_be_sent_while_polling.as_ref().unwrap();
            match self.udp.poll_send_to(&d[..], &self.opts.turn_server) {
                Ok(Async::NotReady) => {
                    // No continuing unless UDP socket gets unblocked
                    return Ok(Async::NotReady);
                },
                Err(e) => Err(e)?,
                Ok(Async::Ready(len)) => {
                    assert_eq!(len, d.len());
                    self.data_to_be_sent_while_polling = None;
                },
            }
        }
        
        // Then handling association retry interval
        loop {
            match self.retry_timer.poll() {
                Err(e) => Err(e)?,
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) => Err(format!("Interval stream ended?"))?,
                Ok(Async::Ready(Some(_t))) => {
                    self.send_allocate_request();
                    continue;
                },
            }
        }

        Ok(Async::NotReady) // don't care which one in particular is not ready
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
