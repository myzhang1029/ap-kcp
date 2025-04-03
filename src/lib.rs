mod async_kcp;
mod core;

#[cfg(feature = "ring")]
pub mod crypto;

pub mod error;
mod segment;
pub mod udp;

pub use crate::async_kcp::KcpHandle;
pub use crate::async_kcp::KcpStream;
pub use crate::core::Congestion;
pub use crate::core::KcpConfig;
pub use crate::core::KcpIo;

pub use async_trait::async_trait;

#[cfg(test)]
pub mod test {
    use std::{sync::Arc, time::Duration};

    use crate::core::KcpConfig;

    use super::*;
    use async_channel::{bounded, Receiver, Sender};
    use log::LevelFilter;
    use rand::prelude::*;
    use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::UdpSocket, task::JoinSet, time::sleep};

    pub async fn get_udp_pair() -> (UdpSocket, UdpSocket) {
        let io1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let io2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        io1.connect(io2.local_addr().unwrap()).await.unwrap();
        io2.connect(io1.local_addr().unwrap()).await.unwrap();
        (io1, io2)
    }

    pub fn init() {
        std::env::set_var("SMOL_THREADS", "8");
        let _ = env_logger::builder()
            .filter_module("ap_kcp", LevelFilter::Debug)
            .try_init();
    }

    async fn send_recv<T: KcpIo + Send + Sync + 'static>(io1: T, io2: T) {
        let kcp1 = KcpHandle::new(io1, KcpConfig::default()).unwrap();
        let kcp2 = KcpHandle::new(io2, KcpConfig::default()).unwrap();

        tokio::spawn(async move {
            let mut stream1 = kcp1.connect().await.unwrap();
            for i in 0..255 {
                let payload = [i as u8; 0x1000];
                stream1.write_all(&payload).await.unwrap();
            }
            stream1.flush().await.unwrap();
            log::debug!("stream1 flushed");
            let mut buf = vec![0; 0x1000];
            for i in 0..255 {
                stream1.read_exact(&mut buf).await.unwrap();
                assert_eq!(i as u8, buf[99]);
            }
            log::debug!("stream1 read");
            stream1.shutdown().await.unwrap();
        });

        let mut stream2 = kcp2.accept().await.unwrap();
        let mut buf = vec![0; 0x1000];
        for i in 0..255 {
            stream2.read_exact(&mut buf).await.unwrap();
            assert_eq!(i as u8, buf[99]);
        }
        log::debug!("stream2 read");
        for i in 0..255 {
            let payload = [i as u8; 0x1000];
            stream2.write_all(&payload).await.unwrap();
        }
        stream2.shutdown().await.unwrap();
    }

    fn random_data() -> Arc<Vec<u8>> {
        let mut buf = vec![0; 0x500];
        rand::rng().fill_bytes(&mut buf);
        Arc::new(buf)
    }

    async fn concurrent_send_recv<T: KcpIo + Send + Sync + 'static>(io1: T, io2: T) {
        let data = random_data();

        let data1 = data.clone();
        let t1 = tokio::spawn(async move {
            let kcp1 = KcpHandle::new(io1, KcpConfig::default()).unwrap();
            let mut tasks = JoinSet::new();
            for _ in 0..10 {
                let mut stream1 = kcp1.connect().await.unwrap();
                let data = data1.clone();
                tasks.spawn(async move {
                    let mut buf = vec![0; data.len()];
                    stream1.write_all(&data).await.unwrap();
                    stream1.read_exact(&mut buf).await.unwrap();
                    assert_eq!(&buf[..], &data[..]);
                    stream1.shutdown().await.unwrap();
                });
            }
            while let Some(t) = tasks.join_next().await {
                t.unwrap();
            }
        });

        let data2 = data.clone();
        let t2 = tokio::spawn(async move {
            let kcp2 = KcpHandle::new(io2, KcpConfig::default()).unwrap();
            let mut tasks = JoinSet::new();
            for _ in 0..10 {
                let mut stream2 = kcp2.accept().await.unwrap();
                let data = data2.clone();
                tasks.spawn(async move {
                    let mut buf = vec![0; data.len()];
                    stream2.read_exact(&mut buf).await.unwrap();
                    assert_eq!(&buf[..], &data[..]);
                    stream2.write_all(&data).await.unwrap();
                    stream2.shutdown().await.unwrap();
                });
            }
            while let Some(t) = tasks.join_next().await {
                t.unwrap();
            }
        });
        tokio::select! {
            r = t1 => r.unwrap(),
            r = t2 => r.unwrap(),
        }
    }

    pub struct NetworkIoSimulator {
        packet_loss: f64,
        delay: u64,
        tx: Sender<Vec<u8>>,
        rx: Receiver<Vec<u8>>,
    }

    impl NetworkIoSimulator {
        fn new(packet_loss: f64, delay: u64) -> (Self, Self) {
            let (tx1, rx1) = bounded(1);
            let (tx2, rx2) = bounded(1);
            let io1 = Self {
                packet_loss,
                delay,
                tx: tx1,
                rx: rx2,
            };
            let io2 = Self {
                packet_loss,
                delay,
                tx: tx2,
                rx: rx1,
            };
            (io1, io2)
        }
    }

    #[async_trait::async_trait]
    impl KcpIo for NetworkIoSimulator {
        async fn send_packet(&self, buf: &mut Vec<u8>) -> std::io::Result<()> {
            let tx = self.tx.clone();
            let delay = self.delay;
            let loss = self.packet_loss;
            let mut packet = Vec::new();
            packet.extend_from_slice(buf);
            tokio::spawn(async move {
                sleep(Duration::from_millis(delay)).await;
                if rand::random_bool(loss) {
                    log::debug!("packet lost XD");
                } else {
                    let _ = tx.send(packet).await;
                }
            });
            Ok(())
        }

        async fn recv_packet(&self) -> std::io::Result<Vec<u8>> {
            let packet = self
                .rx
                .recv()
                .await
                .map_err(|_| std::io::ErrorKind::ConnectionReset)?;
            Ok(packet)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp() {
        init();
        let (io1, io2) = get_udp_pair().await;
        send_recv(io1, io2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn normal() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(0.005, 20);
        send_recv(io1, io2).await;
        let (io1, io2) = NetworkIoSimulator::new(0.005, 20);
        concurrent_send_recv(io1, io2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn laggy() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(0.005, 300);
        send_recv(io1, io2).await;
        let (io1, io2) = NetworkIoSimulator::new(0.005, 300);
        concurrent_send_recv(io1, io2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn packet_lost() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(0.3, 100);
        send_recv(io1, io2).await;
        let (io1, io2) = NetworkIoSimulator::new(0.3, 100);
        concurrent_send_recv(io1, io2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn horrible() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(0.3, 500);
        send_recv(io1, io2).await;
        let (io1, io2) = NetworkIoSimulator::new(0.3, 500);
        concurrent_send_recv(io1, io2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drop_handle() {
        init();
        let (io1, _io2) = NetworkIoSimulator::new(0.0, 10);
        let kcp1 = KcpHandle::new(io1, KcpConfig::default()).unwrap();
        let mut stream1 = kcp1.connect().await.unwrap();
        drop(kcp1);
        let mut buf = vec![0; 100];
        assert!(stream1.read_exact(&mut buf).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drop_stream() {
        init();
        let (io1, _io2) = NetworkIoSimulator::new(0.0, 10);
        let kcp1 = KcpHandle::new(io1, KcpConfig::default()).unwrap();
        let stream1 = kcp1.connect().await.unwrap();
        assert_eq!(kcp1.get_stream_count().await, 1);
        drop(stream1);
        sleep(Duration::from_millis(
            1000 + u64::from(KcpConfig::default().timeout),
        ))
        .await;
        assert_eq!(kcp1.get_stream_count().await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(1.0, 500);
        let config = KcpConfig::default();
        let kcp1 = KcpHandle::new(io1, config.clone()).unwrap();
        let _kcp2 = KcpHandle::new(io2, config.clone()).unwrap();
        let mut stream1 = kcp1.connect().await.unwrap();
        let mut buf = vec![0; 100];
        assert!(stream1.read_exact(&mut buf).await.is_err());
    }

    #[cfg(not(test))] //Disable this test
    #[tokio::test(flavor = "multi_thread")]
    async fn keep_alive() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(0.0, 10);
        let mut config = KcpConfig::default();
        config.timeout = 1000;
        config.keep_alive_interval = 300;
        let kcp1 = KcpHandle::new(io1, config.clone()).unwrap();
        let kcp2 = KcpHandle::new(io2, config.clone()).unwrap();
        let mut stream1 = kcp1.connect().await.unwrap();
        let mut stream2 = kcp2.accept().await.unwrap();
        sleep(Duration::from_secs(5)).await;
        let mut buf = Vec::new();
        buf.resize(100, 0u8);
        stream1.write_all(b"hello1").await.unwrap();
        let len = stream2.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"hello1");
        sleep(Duration::from_secs(5)).await;

        stream1.write_all(b"hello2").await.unwrap();
        let len = stream2.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"hello2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rexmit() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(1.0, 10);
        let mut config = KcpConfig::default();
        config.max_rexmit_time = 8;
        let kcp1 = KcpHandle::new(io1, config.clone()).unwrap();
        let _kcp2 = KcpHandle::new(io2, config.clone()).unwrap();
        let mut stream1 = kcp1.connect().await.unwrap();
        stream1.write(b"test").await.unwrap();
        let mut buf = Vec::new();
        assert!(stream1.read(&mut buf).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close() {
        init();
        let (io1, io2) = NetworkIoSimulator::new(0.0, 100);

        let t = tokio::spawn(async move {
            let config = KcpConfig::default();
            let kcp1 = KcpHandle::new(io1, config).unwrap();
            let mut stream1 = kcp1.connect().await.unwrap();
            stream1.write(b"test").await.unwrap();
            stream1.shutdown().await.unwrap();
        });
        let config = KcpConfig::default();
        let kcp2 = KcpHandle::new(io2, config).unwrap();
        let mut buf = Vec::new();
        let mut stream2 = kcp2.accept().await.unwrap();
        stream2.read(&mut buf).await.unwrap();
        stream2.shutdown().await.unwrap();
        t.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session() {
        init();
        let data = random_data();
        udp_session(data).await;
    }

    pub async fn udp_session(data: Arc<Vec<u8>>) {
        let (io1, io2) = get_udp_pair().await;
        let handle1 = KcpHandle::new(io1, KcpConfig::default()).unwrap();
        let data1 = data.clone();
        let t = tokio::spawn(async move {
            let io2 = udp::UdpListener::new(io2);
            let session = io2.accept().await;
            let handle2 = KcpHandle::new(session, KcpConfig::default()).unwrap();
            let mut stream2 = handle2.accept().await.unwrap();
            let mut buf = vec![0; data1.len()];
            stream2.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf[..], &data1[..]);
            stream2.shutdown().await.unwrap();
        });
        let mut stream1 = handle1.connect().await.unwrap();
        stream1.write_all(&data).await.unwrap();
        stream1.shutdown().await.unwrap();
        t.await.unwrap();
    }
}
