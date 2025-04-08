use std::net::SocketAddr;
use std::{fs, sync::Arc};

use clap::{Arg, ArgAction, Command};
use log::LevelFilter;
use ring::aead;
use tokio::io::AsyncWriteExt;
use tokio::net::{lookup_host, TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

use crate::udp::{UdpListener, UdpSession};

mod async_kcp;
mod core;
mod crypto;
mod error;
mod segment;
mod udp;

use crate::{
    async_kcp::KcpHandle,
    core::{KcpConfig, KcpIo},
    crypto::{AeadCrypto, Crypto, CryptoLayer},
    error::KcpResult,
};

async fn client<C: Crypto + 'static>(
    local: String,
    crypto: C,
    udp: UdpSocket,
    config: KcpConfig,
) -> std::io::Result<()> {
    let udp = CryptoLayer::wrap(udp, crypto);
    let kcp_handle = KcpHandle::new(udp, config)?;
    let listener = TcpListener::bind(local).await?;
    loop {
        let (mut tcp_stream, addr) = listener.accept().await?;
        log::info!("tcp socket accepted: {addr}");
        let mut kcp_stream = kcp_handle.connect().await?;
        log::info!("kcp tunnel established");
        tokio::spawn(async move {
            tokio::io::copy_bidirectional(&mut tcp_stream, &mut kcp_stream).await?;
            kcp_stream.shutdown().await?;
            tcp_stream.shutdown().await?;
            log::info!("client-side tunnel closed");
            std::io::Result::Ok(())
        });
    }
}

async fn server<C: Crypto + 'static>(
    addr: String,
    udp: UdpSocket,
    crypto: C,
    config: KcpConfig,
) -> std::io::Result<()> {
    config.check()?;
    let listener = UdpListener::new(udp);
    let crypto = Arc::new(crypto);
    let mut sessions: Vec<(
        Arc<KcpHandle<CryptoLayer<UdpSession, Arc<C>>>>,
        JoinHandle<KcpResult<()>>,
    )> = Vec::new();

    loop {
        let udp_session = listener.accept().await;
        log::trace!("udp session accepted: {}", udp_session.get_remote());
        let udp_session = CryptoLayer::wrap(udp_session, crypto.clone());
        let kcp_handle = Arc::new(KcpHandle::new(udp_session, config.clone()).unwrap());
        let t: JoinHandle<KcpResult<()>> = {
            let addr = addr.clone();
            let kcp_handle = kcp_handle.clone();
            tokio::spawn(async move {
                loop {
                    let mut kcp_stream = kcp_handle.accept().await?;
                    log::info!("kcp tunnel established");
                    let mut tcp_stream = TcpStream::connect(addr.clone()).await?;
                    log::info!("tunneling to {addr}");
                    tokio::spawn(async move {
                        tokio::io::copy_bidirectional(&mut kcp_stream, &mut tcp_stream).await?;
                        tcp_stream.shutdown().await?;
                        kcp_stream.shutdown().await?;
                        log::info!("server-side tunnel closed");
                        std::io::Result::Ok(())
                    });
                }
            })
        };
        let mut to_remove = Vec::with_capacity(sessions.len());
        for (idx, (handle, t)) in sessions.iter().enumerate() {
            let count = handle.get_stream_count().await;
            log::debug!("count = {count}");
            if count == 0 {
                log::info!("removing kcp handle");
                to_remove.push(idx);
                t.abort();
            }
        }
        for idx in to_remove.iter().rev() {
            sessions.swap_remove(*idx);
        }
        sessions.push((kcp_handle, t));
    }
}

fn get_algorithm(name: &str) -> &'static aead::Algorithm {
    match name {
        "aes-128-gcm" => &aead::AES_128_GCM,
        "aes-256-gcm" => &aead::AES_256_GCM,
        "chacha20-poly1305" => &aead::CHACHA20_POLY1305,
        _ => {
            panic!("no such algorithm {name}")
        }
    }
}

fn get_level(name: &str) -> LevelFilter {
    match name {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => {
            panic!("no such level {name}")
        }
    }
}

#[tokio::main]
async fn main() {
    let matches = Command::new("ap_kcp")
        .arg(Arg::new("local").long("local").short('l').required(true))
        .arg(Arg::new("remote").long("remote").short('r').required(true))
        .arg(
            Arg::new("client")
                .long("client")
                .short('c')
                .action(ArgAction::SetTrue)
                .conflicts_with("server"),
        )
        .arg(
            Arg::new("server")
                .long("server")
                .short('s')
                .action(ArgAction::SetTrue)
                .conflicts_with("client"),
        )
        .arg(
            Arg::new("password")
                .long("password")
                .short('p')
                .required(true),
        )
        .arg(Arg::new("level").long("level").default_value("info"))
        .arg(
            Arg::new("algorithm")
                .long("algorithm")
                .short('a')
                .value_parser(["aes-256-gcm", "aes-128-gcm", "chacha20-poly1305"])
                .default_value("aes-256-gcm"),
        )
        .arg(
            Arg::new("kcp-config")
                .long("kcp-config")
                .short('k')
                .required(false),
        )
        .author("black-binary")
        .version("0.1.0")
        .get_matches();

    let level = matches.get_one::<String>("level").unwrap();
    let _ = env_logger::builder()
        .filter_module("ap_kcp", get_level(level))
        .try_init();

    let config = match matches.get_one::<String>("kcp-config") {
        Some(path) => {
            let content = fs::read_to_string(path).unwrap();
            toml::from_str::<KcpConfig>(&content).unwrap()
        }
        None => KcpConfig::default(),
    };

    let local = matches.get_one::<String>("local").unwrap();
    let remote = matches.get_one::<String>("remote").unwrap();
    let password = matches.get_one::<String>("password").unwrap();
    let algorithm_name = matches.get_one::<String>("algorithm").unwrap();
    let aead = AeadCrypto::new(password.as_bytes(), get_algorithm(algorithm_name));

    if matches.get_flag("client") {
        log::info!("ap-kcp-tun client");
        log::info!("listening on {local}, tunneling via {remote}");
        log::info!("algorithm: {algorithm_name}");
        log::info!("settings: {config:?}");
        let remote_addrs = lookup_host(remote).await.unwrap();
        for remote in remote_addrs {
            match remote {
                SocketAddr::V4(remote) => {
                    let Ok(udp) = UdpSocket::bind("0.0.0.0:0").await else {
                        continue;
                    };
                    if udp.connect(remote).await.is_err() {
                        continue;
                    }
                    client(local.to_string(), aead, udp, config).await.unwrap();
                    break;
                }
                SocketAddr::V6(remote) => {
                    let Ok(udp) = UdpSocket::bind("[::]:0").await else {
                        continue;
                    };
                    if udp.connect(remote).await.is_err() {
                        continue;
                    }
                    client(local.to_string(), aead, udp, config).await.unwrap();
                    break;
                }
            }
        }
    } else if matches.get_flag("server") {
        log::info!("ap-kcp-tun server");
        log::info!("listening on {local}, tunneling to {remote}");
        log::info!("algorithm: {algorithm_name}");
        log::info!("settings: {config:?}");
        let udp = UdpSocket::bind(local).await.unwrap();
        server(remote.to_string(), udp, aead, config).await.unwrap();
    } else {
        log::warn!("neither --server or --client is specified");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn simple_iperf() {
    let _ = env_logger::builder()
        .filter_module("ap_kcp", LevelFilter::Debug)
        .try_init();
    let password = "password";
    let t1 = tokio::spawn(async move {
        let local = "0.0.0.0:5000";
        let remote = "127.0.0.1:6000";
        let udp = UdpSocket::bind(":::0").await.unwrap();
        udp.connect(remote).await.unwrap();
        let aead = AeadCrypto::new(password.as_bytes(), &aead::AES_256_GCM);
        let mut config = KcpConfig::default();
        config.name = String::from("client");
        client(local.to_string(), aead, udp, config).await.unwrap();
    });

    let t2 = tokio::spawn(async move {
        let local = "127.0.0.1:6000";
        let remote = "127.0.0.1:5201";
        let udp = UdpSocket::bind(local).await.unwrap();
        let aead = AeadCrypto::new(password.as_bytes(), &aead::AES_256_GCM);
        let mut config = KcpConfig::default();
        config.name = String::from("server");
        server(remote.to_string(), udp, aead, config).await.unwrap();
    });
    tokio::select! {
        _ = t1 => {}
        _ = t2 => {}
    }
}
