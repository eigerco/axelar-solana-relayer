use std::net::SocketAddrV4;

pub mod component;

pub struct Config {
    pub socket_addr: SocketAddrV4,
}
