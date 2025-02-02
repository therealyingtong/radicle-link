// Copyright © 2019-2020 The Radicle Foundation <hello@radicle.foundation>
//
// This file is part of radicle-link, distributed under the GPLv3 with Radicle
// Linking Exception. For full terms see the included LICENSE file.

use std::{net::SocketAddr, ops::Deref, sync::Arc};

use governor::{Quota, RateLimiter};
use nonzero_ext::nonzero;
use rand_pcg::Pcg64Mcg;
use tracing::Instrument as _;

use super::{broadcast, cache, config, gossip, io, membership, nonce, ProtocolStorage, TinCans};
use crate::{
    git::{
        p2p::{
            server::GitServer,
            transport::{GitStream, GitStreamFactory},
        },
        replication,
        storage::{self, PoolError, PooledRef},
    },
    net::{quic, upgrade},
    PeerId,
};
use futures::future::TryFutureExt as _;

#[derive(Clone, Copy)]
pub(super) struct StateConfig {
    pub replication: replication::Config,
    pub fetch: config::Fetch,
}

/// Runtime state of a protocol instance.
///
/// You know, like `ReaderT (State s) IO`.
#[derive(Clone)]
pub(super) struct State<S> {
    pub local_id: PeerId,
    pub endpoint: quic::Endpoint,
    pub git: GitServer,
    pub membership: membership::Hpv<Pcg64Mcg, SocketAddr>,
    pub storage: Storage<S>,
    pub phone: TinCans,
    pub config: StateConfig,
    pub nonces: nonce::NonceBag,
    pub caches: cache::Caches,
}

#[async_trait]
impl<S> GitStreamFactory for State<S>
where
    S: ProtocolStorage<SocketAddr, Update = gossip::Payload> + Clone + 'static,
{
    async fn open_stream(
        &self,
        to: &PeerId,
        addr_hints: &[SocketAddr],
    ) -> Option<Box<dyn GitStream>> {
        let span = tracing::info_span!("open-git-stream", remote_id = %to);

        let may_conn = match self.endpoint.get_connection(*to) {
            Some(conn) => Some(conn),
            None => {
                let addr_hints = addr_hints.iter().copied().collect::<Vec<_>>();
                io::connect(&self.endpoint, *to, addr_hints)
                    .instrument(span.clone())
                    .await
                    .map(|(conn, ingress)| {
                        tokio::spawn(
                            io::streams::incoming(self.clone(), ingress).instrument(span.clone()),
                        );
                        conn
                    })
            },
        };

        match may_conn {
            None => {
                span.in_scope(|| tracing::error!("unable to obtain connection"));
                None
            },

            Some(conn) => {
                let stream = conn
                    .open_bidi()
                    .inspect_err(|e| tracing::error!(err = ?e, "unable to open stream"))
                    .instrument(span.clone())
                    .await
                    .ok()?;
                let upgraded = upgrade::upgrade(stream, upgrade::Git)
                    .inspect_err(|e| tracing::error!(err = ?e, "unable to upgrade stream"))
                    .instrument(span)
                    .await
                    .ok()?;

                Some(Box::new(upgraded))
            },
        }
    }
}

type Limiter = governor::RateLimiter<
    governor::state::direct::NotKeyed,
    governor::state::InMemoryState,
    governor::clock::DefaultClock,
>;

#[derive(Clone)]
pub(super) struct Storage<S> {
    inner: S,
    limiter: Arc<Limiter>,
}

impl<S> From<S> for Storage<S> {
    fn from(inner: S) -> Self {
        Self {
            inner,
            limiter: Arc::new(RateLimiter::direct(Quota::per_second(
                // TODO: make this an "advanced" config
                nonzero!(5u32),
            ))),
        }
    }
}

impl<S> Deref for Storage<S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[async_trait]
impl<A, S> broadcast::LocalStorage<A> for Storage<S>
where
    A: 'static,
    S: broadcast::LocalStorage<A>,
    S::Update: Send,
{
    type Update = S::Update;

    async fn put<P>(&self, provider: P, has: Self::Update) -> broadcast::PutResult<Self::Update>
    where
        P: Into<(PeerId, Vec<A>)> + Send,
    {
        self.inner.put(provider, has).await
    }

    async fn ask(&self, want: Self::Update) -> bool {
        self.inner.ask(want).await
    }
}

impl<S> broadcast::ErrorRateLimited for Storage<S> {
    fn is_error_rate_limit_breached(&self) -> bool {
        self.limiter.check().is_err()
    }
}

#[async_trait]
impl<S> storage::Pooled for Storage<S>
where
    S: storage::Pooled + Send + Sync,
{
    async fn get(&self) -> Result<PooledRef, PoolError> {
        self.inner.get().await
    }
}
