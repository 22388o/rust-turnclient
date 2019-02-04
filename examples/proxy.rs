#![allow(unused)]

extern crate tokio;
extern crate turnclient;


use std::net::{SocketAddr};

use tokio::net::udp::UdpSocket;
use tokio::prelude::{Future,Stream};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args : Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("Usage: proxy turn_host:port username password peer_host:port< interactive commands");
        Err(format!("Invalid command-line arguments"))?;
    }

    let turn_server : SocketAddr = args[1].parse()?;
    let username : String = args[2].parse()?;
    let password : String = args[3].parse()?;
    let peer_addr : SocketAddr = args[4].parse()?;

    let local_addr : SocketAddr = "0.0.0.0:0".parse().unwrap();
    let udp = tokio::net::udp::UdpSocket::bind(&local_addr)?;

    let c = turnclient::TurnClientBuilder::new(turn_server, username, password);
    let f = c.build_and_send_request(udp)
    .and_then(move |turncl| {
        let (turnsink, turnstream) = turncl.split();
        turnstream.map(move |x| {
            //println!("{:?}", x);
            print!(".");
            match x {
                turnclient::MessageFromTurnServer::AllocationGranted{..} => {
                    println!("Allocation granted: {:?}", x);
                    turnclient::MessageToTurnServer::AddPermission(peer_addr)
                },
                turnclient::MessageFromTurnServer::RecvFrom(sa,data) => {
                    println!("Incoming {} bytes from {}", data.len(), sa);
                    //turnclient::MessageToTurnServer::SendTo(sa, data)
                    turnclient::MessageToTurnServer::Disconnect
                },
                _ => turnclient::MessageToTurnServer::Noop,
            }
        }).forward(turnsink)
        .and_then(|(_turnstream,_turnsink)|{
            futures::future::ok(())
        })
    })
    .map_err(|e|eprintln!("{}", e))
    ;

    tokio::runtime::current_thread::run(f);

    Ok(())
}
