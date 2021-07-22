use crate::crypt;
use crate::{protocol, runtime, Backhaul, Session, SessionBack, SessionConfig, StatsGatherer};
use anyhow::Context;
use bytes::Bytes;
use rand::prelude::*;
use smol::prelude::*;
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Configures the client.
#[derive(Clone)]
pub(crate) struct LowlevelClientConfig {
    pub server_addr: SocketAddr,
    pub server_pubkey: x25519_dalek::PublicKey,
    pub backhaul_gen: Arc<dyn Fn() -> Arc<dyn Backhaul> + 'static + Send + Sync>,
    pub num_shards: usize,
    pub reset_interval: Option<Duration>,
    pub gather: Arc<StatsGatherer>,
}

/// Connects to a remote server, given a closure that generates socket addresses.
pub(crate) async fn connect_custom(cfg: LowlevelClientConfig) -> std::io::Result<Session> {
    let backhaul = (cfg.backhaul_gen)();
    let my_long_sk = x25519_dalek::StaticSecret::new(&mut rand::thread_rng());
    let my_eph_sk = x25519_dalek::StaticSecret::new(&mut rand::thread_rng());
    // do the handshake
    let cookie = crypt::Cookie::new(cfg.server_pubkey);
    let init_hello = protocol::HandshakeFrame::ClientHello {
        long_pk: (&my_long_sk).into(),
        eph_pk: (&my_eph_sk).into(),
        version: VERSION,
    };
    for timeout_factor in (0u32..).map(|x| 2u64.pow(x)) {
        // send hello
        let init_hello = crypt::LegacyAead::new(&cookie.generate_c2s().next().unwrap())
            .pad_encrypt_v1(&std::slice::from_ref(&init_hello), 1000);
        backhaul.send_to(init_hello, cfg.server_addr).await?;
        tracing::trace!("sent client hello");
        // wait for response
        let res = backhaul
            .recv_from()
            .or(async {
                smol::Timer::after(Duration::from_secs(timeout_factor)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out",
                ))
            })
            .await;
        match res {
            Ok((buf, _)) => {
                for possible_key in cookie.generate_s2c() {
                    let decrypter = crypt::LegacyAead::new(&possible_key);
                    let response = decrypter.pad_decrypt_v1(&buf);
                    for response in response.unwrap_or_default() {
                        if let protocol::HandshakeFrame::ServerHello {
                            long_pk,
                            eph_pk,
                            resume_token,
                        } = response
                        {
                            tracing::trace!("obtained response from server");
                            if long_pk.as_bytes() != cfg.server_pubkey.as_bytes() {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionRefused,
                                    "bad pubkey",
                                ));
                            }
                            let shared_sec =
                                crypt::triple_ecdh(&my_long_sk, &my_eph_sk, &long_pk, &eph_pk);
                            return Ok(init_session(cookie, resume_token, shared_sec, cfg.clone()));
                        }
                    }
                }
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::TimedOut {
                    tracing::trace!(
                        "timed out to {} with {}s timeout; trying again",
                        cfg.server_addr,
                        timeout_factor
                    );
                    continue;
                }
                return Err(err);
            }
        }
    }
    unimplemented!()
}
const VERSION: u64 = 3;

fn init_session(
    cookie: crypt::Cookie,
    resume_token: Bytes,
    shared_sec: blake3::Hash,
    cfg: LowlevelClientConfig,
) -> Session {
    let (mut session, back) = Session::new(SessionConfig {
        version: VERSION,
        gather: cfg.gather.clone(),
        session_key: shared_sec.as_bytes().to_vec(),
        role: crate::Role::Client,
    });
    let back = Arc::new(back);
    let backhaul_tasks: Vec<_> = (0..cfg.num_shards)
        .map(|i| {
            runtime::spawn(client_backhaul_once(
                cookie.clone(),
                resume_token.clone(),
                back.clone(),
                i as u8,
                cfg.clone(),
            ))
        })
        .collect();
    session.on_drop(move || {
        drop(backhaul_tasks);
    });
    session
}

#[allow(clippy::all)]
async fn client_backhaul_once(
    cookie: crypt::Cookie,
    resume_token: Bytes,
    session_back: Arc<SessionBack>,
    shard_id: u8,
    cfg: LowlevelClientConfig,
) -> Option<()> {
    let mut last_reset = Instant::now();
    let mut updated = false;
    let mut socket: Arc<dyn Backhaul> = (cfg.backhaul_gen)();
    // let mut _old_cleanup: Option<smol::Task<Option<()>>> = None;

    #[derive(Debug)]
    enum Evt {
        Incoming(Vec<Bytes>),
        Outgoing(Bytes),
    }

    let mut my_reset_millis = cfg.reset_interval.map(|interval| {
        rand::thread_rng().gen_range(interval.as_millis() / 2, interval.as_millis())
    });

    // last remind time
    let mut last_remind: Option<Instant> = None;

    loop {
        let down = {
            let socket = &socket;
            async move {
                let packets = socket
                    .recv_from_many()
                    .await
                    .context("cannot receive from socket")?;
                Ok::<_, anyhow::Error>(Evt::Incoming(packets.into_iter().map(|v| v.0).collect()))
            }
        };
        let up = async {
            let raw_upload = session_back
                .next_outgoing()
                .await
                .context("cannot read out of session_back")?;
            Ok::<_, anyhow::Error>(Evt::Outgoing(raw_upload))
        };

        match smol::future::race(down, up).await {
            Ok(Evt::Incoming(bts)) => {
                for bts in bts {
                    let _ = session_back.inject_incoming(&bts);
                }
            }
            Ok(Evt::Outgoing(bts)) => {
                let bts: Bytes = bts;
                let now = Instant::now();
                if last_remind
                    .replace(Instant::now())
                    .map(|f| f.elapsed() > Duration::from_secs(1))
                    .unwrap_or_default()
                    || !updated
                {
                    updated = true;
                    let g_encrypt = crypt::LegacyAead::new(&cookie.generate_c2s().next().unwrap());
                    if let Some(reset_millis) = my_reset_millis {
                        if now.saturating_duration_since(last_reset).as_millis() > reset_millis {
                            my_reset_millis = cfg.reset_interval.map(|interval| {
                                rand::thread_rng()
                                    .gen_range(interval.as_millis() / 2, interval.as_millis())
                            });
                            last_reset = now;
                            // also replace the UDP socket!
                            let old_socket = socket.clone();
                            let session_back = session_back.clone();
                            // spawn a task to clean up the UDP socket
                            let tata: smol::Task<Option<()>> = runtime::spawn(
                                async move {
                                    loop {
                                        let bufs = old_socket.recv_from_many().await.ok()?;
                                        for (buf, _) in bufs {
                                            session_back.inject_incoming(&buf).ok()?
                                        }
                                    }
                                }
                                .or(async {
                                    smol::Timer::after(Duration::from_secs(60)).await;
                                    None
                                }),
                            );
                            tata.detach();
                            socket = (cfg.backhaul_gen)()
                        }
                    }
                    drop(
                        socket
                            .send_to(
                                g_encrypt.pad_encrypt_v1(
                                    &[protocol::HandshakeFrame::ClientResume {
                                        resume_token: resume_token.clone(),
                                        shard_id,
                                    }],
                                    1000,
                                ),
                                cfg.server_addr,
                            )
                            .await,
                    );
                }
                if let Err(err) = socket.send_to(bts, cfg.server_addr).await {
                    tracing::error!("error sending packet: {:?}", err)
                }
            }
            Err(err) => {
                tracing::error!("FATAL error in down/up: {:?}", err);
                return None;
            }
        }
    }
}
