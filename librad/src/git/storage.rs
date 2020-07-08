// This file is part of radicle-link
// <https://github.com/radicle-dev/radicle-link>
//
// Copyright (C) 2019-2020 The Radicle Team <dev@radicle.xyz>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License version 3 or
// later as published by the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    io,
    iter,
    marker::PhantomData,
    ops::Range,
    path::Path,
};

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use crate::{
    git::{
        ext::{
            blob::{self, Blob},
            is_exists_err,
            is_not_found_err,
            Oid,
            References,
        },
        p2p::url::{GitUrl, GitUrlRef},
        refs::{self, Refs},
        repo::Repo,
        types::Reference,
    },
    hash::Hash,
    internal::{
        borrow::TryToOwned,
        canonical::{Cjson, CjsonError},
        result::ResultExt,
    },
    keys::SecretKey,
    meta::{
        entity::{
            self,
            data::EntityInfoExt,
            Draft,
            Entity,
            GenericDraftEntity,
            Signatory,
            Verified,
        },
        user::User,
    },
    paths::Paths,
    peer::{self, PeerId},
    uri::{self, RadUrl, RadUrn},
};

mod config;
mod fetch;

#[cfg(test)]
mod test;

use config::Config;
use fetch::Fetcher;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Already exists: {0}")]
    AlreadyExists(RadUrn),

    #[error("Not found: {0}")]
    NoSuchUrn(RadUrn),

    #[error(
        "Identity root hash doesn't match resolved URL. Expected {expected}, actual: {actual}"
    )]
    RootHashMismatch { expected: Hash, actual: Hash },

    #[error("Metadata is not signed")]
    UnsignedMetadata,

    #[error("Signer key does not match key used at initialisation")]
    SignerKeyMismatch,

    #[error("Can't refer to the local key for this operation")]
    SelfReferential,

    #[error("Metadata must be signed by local key")]
    NotSignedBySelf,

    #[error("Local key certifier not found: {0}")]
    NoSelf(Reference),

    #[error("Missing certifier {certifier} of {urn}")]
    MissingCertifier { certifier: RadUrn, urn: RadUrn },

    #[error(transparent)]
    PeerId(#[from] peer::conversion::Error),

    #[error(transparent)]
    Urn(#[from] uri::rad_urn::ParseError),

    #[error(transparent)]
    Entity(#[from] entity::Error),

    #[error(transparent)]
    Fetch(#[from] fetch::Error),

    #[error(transparent)]
    Refsig(#[from] refs::signed::Error),

    #[error(transparent)]
    Cjson(#[from] CjsonError),

    #[error(transparent)]
    Blob(#[from] blob::Error),

    #[error(transparent)]
    Config(#[from] config::Error),

    #[error(transparent)]
    Git(#[from] git2::Error),

    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub enum RadSelfSpec {
    Default,
    Urn(RadUrn),
}

pub type NoSigner = PhantomData<!>;
pub type WithSigner = SecretKey;

pub struct Storage<S> {
    pub(super) backend: git2::Repository,
    peer_id: PeerId,
    signer: S,
}

impl<S: Clone> Storage<S> {
    /// Obtain a new, owned handle to the backing store.
    pub fn reopen(&self) -> Result<Self, Error> {
        self.try_to_owned()
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn open_repo(&self, urn: RadUrn) -> Result<Repo<S>, Error> {
        let urn = RadUrn {
            path: uri::Path::empty(),
            ..urn
        };

        if !self.has_urn(&urn)? {
            return Err(Error::NoSuchUrn(urn));
        }

        Ok(Repo {
            urn,
            storage: self.into(),
        })
    }

    /// Get the [`Entity`] metadata found at the provided [`RadUrn`].
    pub fn metadata<T>(&self, urn: &RadUrn) -> Result<Entity<T, Draft>, Error>
    where
        T: Clone + Serialize + DeserializeOwned + EntityInfoExt,
    {
        self.metadata_of(urn, None)
    }

    /// Get the [`Entity`] metadata of the tracked `peer` at the provided
    /// [`RadUrn`].
    ///
    /// Note that "tracked" here refers to the transitive tracking graph. That
    /// is, the metadata will resolve if, and only if, it has been fetched from
    /// the network acc. to the replication rules prior to calling this method.
    pub fn metadata_of<T, P>(&self, urn: &RadUrn, peer: P) -> Result<Entity<T, Draft>, Error>
    where
        T: Clone + Serialize + DeserializeOwned + EntityInfoExt,
        P: Into<Option<PeerId>>,
    {
        self.metadata_from_reference(
            Reference::rad_id(urn.id.clone())
                .set_remote(peer.into())
                .borrow(),
        )
    }

    /// Like [`Storage::metadata`], but for situations where the type is not
    /// statically known.
    pub fn some_metadata(&self, urn: &RadUrn) -> Result<GenericDraftEntity, Error> {
        self.some_metadata_of(urn, None)
    }

    /// Like [`Storage::metadata_of`], but for situations where the type is not
    /// statically known.
    pub fn some_metadata_of<P>(&self, urn: &RadUrn, peer: P) -> Result<GenericDraftEntity, Error>
    where
        P: Into<Option<PeerId>>,
    {
        self.metadata_from_reference(
            Reference::rad_id(urn.id.clone())
                .set_remote(peer.into())
                .borrow(),
        )
    }

    /// Get all the [`Entity`] data in this `Storage`.
    ///
    /// The caller has the choice to filter on the [`EntityInfo`], which is
    /// useful when the you want a list of a specific kind of `Entity`.
    pub fn all_metadata<'a>(
        &'a self,
    ) -> Result<impl Iterator<Item = Result<GenericDraftEntity, Error>> + 'a, Error> {
        let iter = References::from_globs(&self.backend, &["refs/namespaces/*/rad/id"])?;

        Ok(iter.map(move |reference| self.metadata_from_reference(reference?)))
    }

    /// Retrieve the `rad/self` identity configured via
    /// [`Storage::set_default_rad_self`].
    pub fn default_rad_self(&self) -> Result<User<Draft>, Error> {
        let urn = Config::try_from(&self.backend)?.user()?;
        self.metadata(&urn)
    }

    /// Get the `rad/self` identity for `urn`.
    pub fn get_rad_self(&self, urn: &RadUrn) -> Result<User<Draft>, Error> {
        self.get_rad_self_of(urn, None)
    }

    /// Get the `rad/self` identity for the remote `peer` under the `urn`.
    pub fn get_rad_self_of<P>(&self, urn: &RadUrn, peer: P) -> Result<User<Draft>, Error>
    where
        P: Into<Option<PeerId>>,
    {
        self.metadata_from_reference(Reference::rad_self(urn.id.clone(), peer).borrow())
    }

    pub fn certifiers_of(&self, urn: &RadUrn, peer: &PeerId) -> Result<HashSet<RadUrn>, Error> {
        let mut refs = References::from_globs(
            &self.backend,
            &[format!(
                "refs/namespaces/{}/refs/remotes/{}/rad/ids/*",
                &urn.id, peer
            )],
        )?;
        let refnames = refs.names();
        Ok(urns_from_refs(refnames).collect())
    }

    pub fn has_commit(&self, urn: &RadUrn, oid: git2::Oid) -> Result<bool, Error> {
        let span = tracing::warn_span!("Storage::has_commit", urn = %urn, oid = %oid);
        let _guard = span.enter();

        if oid.is_zero() {
            return Ok(false);
        }

        let commit = self.backend.find_commit(oid);
        match commit {
            Err(e) if is_not_found_err(&e) => {
                tracing::warn!("commit not found");
                Ok(false)
            },
            Ok(commit) => {
                let namespace = &urn.id;
                let branch = urn.path.deref_or_default();
                let branch = branch.strip_prefix("refs/").unwrap_or(branch);

                let refs = References::from_globs(
                    &self.backend,
                    &[format!("refs/namespaces/{}/refs/{}", namespace, branch)],
                )?;

                for (_, oid) in refs.peeled() {
                    if oid == commit.id() || self.backend.graph_descendant_of(oid, commit.id())? {
                        return Ok(true);
                    }
                }

                Ok(false)
            },
            Err(e) => Err(e.into()),
        }
    }

    pub fn has_ref(&self, reference: &Reference) -> Result<bool, Error> {
        self.backend
            .find_reference(&reference.to_string())
            .map(|_| true)
            .or_matches(is_not_found_err, || Ok(false))
    }

    pub fn has_urn(&self, urn: &RadUrn) -> Result<bool, Error> {
        let namespace = &urn.id;
        let branch = urn.path.deref_or_default();
        let branch = branch.strip_prefix("refs/").unwrap_or(branch);
        self.backend
            .find_reference(&format!("refs/namespaces/{}/refs/{}", namespace, branch))
            .map(|_| true)
            .or_matches(is_not_found_err, || Ok(false))
    }

    pub fn untrack(&self, urn: &RadUrn, peer: &PeerId) -> Result<(), Error> {
        let remote_name = tracking_remote_name(urn, peer);
        // TODO: This removes all remote tracking branches matching the
        // fetchspec (I suppose). Not sure this is what we want.
        self.backend
            .remote_delete(&remote_name)
            .map_err(|e| e.into())
    }

    pub fn tracked(&self, urn: &RadUrn) -> Result<Tracked, Error> {
        Tracked::collect(&self.backend, urn).map_err(|e| e.into())
    }

    /// Read the current [`Refs`] from the repo state
    pub fn rad_refs(&self, urn: &RadUrn) -> Result<Refs, Error> {
        let span = tracing::debug_span!("Storage::rad_refs", urn = %urn);
        let _guard = span.enter();

        // Collect refs/heads (our branches) at their current state
        let heads = self.references_glob(urn, Some("refs/heads/*"))?;
        let heads: BTreeMap<String, Oid> = heads.map(|(name, oid)| (name, Oid(oid))).collect();

        tracing::debug!(heads = ?heads);

        // Get 1st degree tracked peers from the remotes configured in .git/config
        let tracked = self.tracked(urn)?;
        let mut remotes: HashMap<PeerId, HashMap<PeerId, HashSet<PeerId>>> =
            tracked.map(|peer| (peer, HashMap::new())).collect();

        tracing::debug!(remotes.bare = ?remotes);

        // For each of the 1st degree tracked peers, lookup their rad/refs (if any),
        // verify the signature, and add their [`Remotes`] to ours (minus the 3rd
        // degree)
        for (peer, tracked) in remotes.iter_mut() {
            match self.rad_refs_of(urn, peer.clone()) {
                Ok(refs) => *tracked = refs.remotes.cutoff(),
                Err(Error::Blob(blob::Error::NotFound(_))) => {},
                Err(e) => return Err(e),
            }
        }

        tracing::debug!(remotes.verified = ?remotes);

        Ok(Refs {
            heads,
            remotes: remotes.into(),
        })
    }

    pub fn rad_refs_of(&self, urn: &RadUrn, peer: PeerId) -> Result<Refs, Error> {
        let signed = {
            let refs = Reference::rad_refs(urn.id.clone(), peer.clone());
            let blob = Blob::Tip {
                branch: refs.borrow().into(),
                path: Path::new("refs"),
            }
            .get(&self.backend)?;
            refs::Signed::from_json(blob.content(), &peer)
        }?;

        Ok(Refs::from(signed))
    }

    /// The set of all certifiers of the given identity, transitively
    pub fn certifiers(&self, urn: &RadUrn) -> Result<HashSet<RadUrn>, Error> {
        let mut refs = References::from_globs(
            &self.backend,
            &[
                format!("refs/namespaces/{}/refs/rad/ids/*", &urn.id),
                format!("refs/namespaces/{}/refs/remotes/**/rad/ids/*", &urn.id),
            ],
        )?;
        let refnames = refs.names();
        Ok(urns_from_refs(refnames).collect())
    }

    pub(crate) fn references_glob<'a>(
        &'a self,
        urn: &RadUrn,
        globs: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<impl Iterator<Item = (String, git2::Oid)> + 'a, Error> {
        let namespace_prefix = format!("refs/namespaces/{}/", &urn.id);

        let refs = References::from_globs(
            &self.backend,
            globs
                .into_iter()
                .map(|glob| format!("{}{}", namespace_prefix, glob.as_ref())),
        )?;

        Ok(refs.peeled().filter_map(move |(name, target)| {
            name.strip_prefix(&namespace_prefix)
                .map(|name| (name.to_owned(), target))
        }))
    }

    fn metadata_from_reference<'a, R, T>(&self, reference: R) -> Result<Entity<T, Draft>, Error>
    where
        R: Into<blob::Branch<'a>>,
        T: Clone + Serialize + DeserializeOwned + EntityInfoExt,
    {
        let blob = Blob::Tip {
            branch: reference.into(),
            path: Path::new("id"),
        }
        .get(&self.backend)?;

        Entity::<T, Draft>::from_json_slice(blob.content()).map_err(Error::from)
    }

    pub(crate) fn path(&self) -> &Path {
        self.backend.path()
    }
}

impl Storage<NoSigner> {
    /// Open the `Storage` found at the given [`Path`]'s `git_dir.
    ///
    /// The `Storage` must have been initialised with [`Storage::init`] prior to
    /// calling this method.
    pub fn open(paths: &Paths) -> Result<Self, Error> {
        let backend = git2::Repository::open_bare(paths.git_dir())?;
        let peer_id = Config::try_from(&backend)?.peer_id()?;
        Ok(Self {
            backend,
            peer_id,
            signer: PhantomData,
        })
    }

    pub fn with_signer(self, signer: SecretKey) -> Result<Storage<WithSigner>, Error> {
        if self.peer_id != PeerId::from(&signer) {
            return Err(Error::SignerKeyMismatch);
        }

        Ok(Storage {
            backend: self.backend,
            peer_id: self.peer_id,
            signer,
        })
    }
}

impl Storage<WithSigner> {
    /// Open the `Storage` found at the given [`Paths`]'s `git_dir`, or
    /// initialise it if it isn't yet.
    pub fn open_or_init(paths: &Paths, signer: SecretKey) -> Result<Self, Error> {
        match Storage::open(paths) {
            Ok(this) => {
                if this.peer_id != PeerId::from(&signer) {
                    Err(Error::SignerKeyMismatch)
                } else {
                    this.with_signer(signer)
                }
            },
            Err(Error::Git(e)) if is_not_found_err(&e) => Self::init(paths, signer),
            Err(e) => Err(e),
        }
    }

    /// Initialise the `Storage` at the given [`Paths`]'s `git_dir`.
    pub fn init(paths: &Paths, signer: SecretKey) -> Result<Self, Error> {
        let mut backend = git2::Repository::init_opts(
            paths.git_dir(),
            git2::RepositoryInitOptions::new()
                .bare(true)
                .no_reinit(true)
                .external_template(false),
        )?;
        Config::init(&mut backend, &signer, None)?;

        Ok(Self {
            backend,
            peer_id: PeerId::from(&signer),
            signer,
        })
    }

    pub fn downcast(self) -> Storage<NoSigner> {
        Storage {
            backend: self.backend,
            peer_id: self.peer_id,
            signer: PhantomData,
        }
    }

    pub fn create_repo<T>(&self, meta: &Entity<T, Draft>) -> Result<Repo<WithSigner>, Error>
    where
        T: Serialize + DeserializeOwned + Clone + EntityInfoExt,
    {
        let span = tracing::info_span!("Storage::create_repo");
        let _guard = span.enter();

        // FIXME: properly verify meta

        if meta.signatures().is_empty() {
            return Err(Error::UnsignedMetadata);
        }

        let urn = RadUrn::new(
            meta.root_hash().to_owned(),
            uri::Protocol::Git,
            uri::Path::empty(),
        );

        let self_sig = meta
            .signatures()
            .get(&self.signer.public())
            .ok_or(Error::NotSignedBySelf)?;

        let rad_id = Reference::rad_id(meta.urn().id);
        let rad_self = Reference::rad_self(meta.urn().id, None);
        let rad_self_target = match &self_sig.by {
            Signatory::OwnedKey => rad_id.clone(),
            Signatory::User(urn) => Reference::rad_id(urn.id.clone()),
        };

        // Invariants
        {
            // Check if `rad/self` has a valid target
            if rad_id != rad_self_target && !self.has_ref(&rad_self_target)? {
                return Err(Error::NoSelf(rad_self_target));
            }

            // Check if `rad/ids/*` have valid targets
            for certifier in meta.certifiers() {
                if !self.has_urn(certifier)? {
                    let certifier = certifier.clone();
                    return Err(Error::MissingCertifier { certifier, urn });
                }
            }
        }

        self.commit_initial_meta(&meta)?;

        // self and certifier symrefs
        {
            let res = iter::once((rad_self, rad_self_target))
                .chain(meta.certifiers().iter().map(|certifier| {
                    (
                        Reference::rad_certifier(meta.urn().id, certifier),
                        Reference::rad_id(certifier.id.clone()),
                    )
                }))
                .try_for_each(|(src, target)| {
                    self.backend
                        .reference_symbolic(
                            &src.to_string(),
                            &target.to_string(),
                            true,
                            &format!("{} -> {}", src, target),
                        )
                        .and(Ok(()))
                });

            if let Err(err) = res {
                self.delete_repo(&urn)?;
                return Err(err.into());
            }
        }

        self.track_signers(&meta)?;
        self.update_refs(&urn)?;

        Ok(Repo {
            urn,
            storage: self.into(),
        })
    }

    /// Attempt to clone the designated repo from the network.
    ///
    /// Note that this method **must** be spawned on a `async` runtime, where
    /// currently the only supported method is [`tokio::task::spawn_blocking`].
    pub fn clone_repo<T>(&self, url: RadUrl) -> Result<Repo<WithSigner>, Error>
    where
        T: Serialize + DeserializeOwned + Clone + EntityInfoExt,
    {
        let span = tracing::info_span!("Storage::clone_repo", url = %url);
        let _guard = span.enter();

        let remote_peer = url.authority.clone();

        let urn = RadUrn {
            path: uri::Path::empty(),
            ..url.urn.clone()
        };

        if self.has_urn(&urn)? {
            return Err(Error::AlreadyExists(urn));
        }

        // Fetch the identity first
        let git_url = GitUrl::from_rad_url(url, self.peer_id.clone());
        let mut fetcher = Fetcher::new(&self.backend, git_url)?;
        fetcher.prefetch()?;

        let meta = self.some_metadata_of(&urn, remote_peer.clone())?;

        // TODO: properly verify
        let valid: Result<(), Error> = {
            if meta.signatures().is_empty() {
                Err(Error::UnsignedMetadata)
            } else if meta.root_hash() != &urn.id {
                Err(Error::RootHashMismatch {
                    expected: urn.id.clone(),
                    actual: meta.root_hash().to_owned(),
                })
            } else {
                Ok(())
            }
        };

        if let Err(invalid) = valid {
            self.delete_repo(&urn)?;
            return Err(invalid);
        }

        // We determined that `remote_peer`'s view of the identity is valid, so
        // we can adopt it as our own (ie. make `refs/rad/id` point to what
        // `remote_peer` said)
        {
            let local_id = Reference::rad_id(urn.id.clone());
            let remote_id = local_id.with_remote(remote_peer);
            let remote_id_head = remote_id.find(&self.backend).and_then(|reference| {
                reference.target().ok_or_else(|| {
                    git2::Error::from_str(&format!(
                        "We just read `{}`, but now it's gone",
                        remote_id
                    ))
                })
            })?;
            self.backend.reference(
                &local_id.to_string(),
                remote_id_head,
                /* force */ false,
                &format!("Adopted `{}` as ours", remote_id),
            )?;
        }

        self.track_signers(&meta)?;
        self.update_refs(&urn)?;
        self.fetch_internal(fetcher)?;

        Ok(Repo {
            urn,
            storage: self.into(),
        })
    }

    pub fn fetch_repo(&self, url: RadUrl) -> Result<(), Error> {
        let span = tracing::info_span!("Storage::fetch", fetch.url = %url);
        let _guard = span.enter();

        let git_url = GitUrl::from_rad_url(url, self.peer_id.clone());
        let fetcher = Fetcher::new(&self.backend, git_url)?;
        self.fetch_internal(fetcher)
    }

    fn fetch_internal(&self, mut fetcher: Fetcher<'_>) -> Result<(), Error> {
        let url = fetcher.url();
        let urn = url.clone().into_rad_url().urn;

        let remote_peer = url.remote_peer.clone();

        let rad_refs = self.rad_refs(&urn)?;
        let transitively_tracked = rad_refs.remotes.flatten().collect::<HashSet<&PeerId>>();

        fetcher.fetch(
            transitively_tracked,
            |peer| self.rad_refs_of(&urn, peer),
            |peer| self.certifiers_of(&urn, peer),
        )?;

        // Symref any certifiers from `remote_peer`, ie. for all valid refs in
        // the remotes's `rad/ids/*`, create a symref in the _local_ `rad/ids/*`
        // pointing to the `rad/id` in the respective top-level namespace.
        {
            References::from_globs(
                &self.backend,
                &[format!(
                    "refs/namespaces/{}/refs/remotes/{}/rad/ids/*",
                    &urn.id, &remote_peer
                )],
            )?
            .names()
            .try_for_each(|certifier_ref| {
                let certifier_ref = certifier_ref?;
                match certifier_ref.parse::<Hash>() {
                    Err(_) => Ok(()),
                    Ok(certifier_hash) => {
                        let certifier_here = Reference::rad_certifier(
                            urn.id.clone(),
                            &RadUrn::new(
                                certifier_hash.clone(),
                                uri::Protocol::Git,
                                uri::Path::empty(),
                            ),
                        );
                        let certifier_id = Reference::rad_id(certifier_hash);
                        self.backend
                            .reference_symbolic(
                                &certifier_here.to_string(),
                                &certifier_id.to_string(),
                                /* force */ false,
                                &format!(
                                    "Symref certifier: `{}` -> `{}`",
                                    certifier_here, certifier_id
                                ),
                            )
                            .and(Ok(()))
                    },
                }
            })?;
        }

        // At this point, the transitive tracking graph may have changed. Let's
        // update the refs, but don't recurse here for now (we could, if
        // we reload `self.rad_refs()` and compare to the value we had
        // before fetching).
        self.update_refs(&urn)
    }

    // DO NOT MAKE THIS PUBLIC YET
    fn delete_repo(&self, urn: &RadUrn) -> Result<(), Error> {
        References::from_globs(&self.backend, &[format!("refs/namespaces/{}/*", urn.id)])?
            .try_for_each(|reference| reference?.delete())
            .map_err(Error::from)
    }

    /// Persist [`User`] `id` as the default `rad/self` identity
    pub fn set_default_rad_self(&self, id: User<Verified>) -> Result<(), Error> {
        let urn = id.urn();
        if !self.has_urn(&urn)? {
            return Err(Error::NoSuchUrn(urn));
        }

        Config::try_from(&self.backend)?
            .set_user(Some(id))
            .map_err(Error::from)
    }

    /// Set the `rad/self` identity for `urn`
    ///
    /// [`None`] removes `rad/self`, if present.
    pub fn set_rad_self<S>(&self, urn: &RadUrn, spec: S) -> Result<(), Error>
    where
        S: Into<Option<RadSelfSpec>>,
    {
        match spec.into() {
            None => {
                let have = Reference::rad_self(urn.id.clone(), None).find(&self.backend);
                match have {
                    Err(_) => Ok(()),
                    Ok(mut reference) => reference.delete().map_err(Error::from),
                }
            },

            Some(spec) => {
                let src = Reference::rad_self(urn.id.clone(), None);
                let target = match spec {
                    RadSelfSpec::Default => {
                        let id = self.default_rad_self()?;
                        Ok::<_, Error>(Reference::rad_id(id.urn().id))
                    },

                    RadSelfSpec::Urn(self_urn) => {
                        let meta: User<Draft> = self.metadata(&self_urn)?;
                        Config::try_from(&self.backend)?.guard_user_valid(&meta)?;
                        Ok(Reference::rad_id(self_urn.id))
                    },
                }?;

                let sym_log_msg = &format!("{} -> {}", src, target);
                tracing::info!("creating symbolic link: {}", sym_log_msg);

                self.backend
                    .reference_symbolic(
                        &src.to_string(),
                        &target.to_string(),
                        /* force */ true,
                        sym_log_msg,
                    )
                    .and(Ok(()))
                    .map_err(Error::from)
            },
        }
    }

    pub fn track(&self, urn: &RadUrn, peer: &PeerId) -> Result<(), Error> {
        if peer == &self.peer_id {
            return Err(Error::SelfReferential);
        }

        let remote_name = tracking_remote_name(urn, peer);
        let url = GitUrlRef::from_rad_urn(&urn, &self.peer_id, peer).to_string();

        tracing::debug!(
            urn = %urn,
            peer = %peer,
            "Storage::track"
        );

        self.backend
            .remote(&remote_name, &url)
            .map(|_| ())
            .or_matches(is_exists_err, || Ok(()))
    }

    // Helpers

    fn commit_initial_meta<T>(&self, meta: &Entity<T, Draft>) -> Result<git2::Oid, Error>
    where
        T: Serialize + DeserializeOwned + Clone + EntityInfoExt,
    {
        let canonical_data = Cjson(meta).canonical_form()?;
        let blob = self.backend.blob(&canonical_data)?;
        let tree = {
            let mut builder = self.backend.treebuilder(None)?;
            builder.insert("id", blob, 0o100_644)?;
            let oid = builder.write()?;
            self.backend.find_tree(oid)
        }?;
        let author = self.backend.signature()?;

        let branch_name = Reference::rad_id(meta.urn().id);

        let oid = self.backend.commit(
            Some(&branch_name.to_string()),
            &author,
            &author,
            &format!("Initialised with identity {}", meta.root_hash()),
            &tree,
            &[],
        )?;

        tracing::debug!(
            repo.urn = %meta.urn(),
            repo.id.branch = %branch_name,
            repo.id.oid = %oid,
            "Initial metadata committed"
        );

        Ok(oid)
    }

    // FIXME: decide if we want to require verified entities
    // FIXME: yes, we want this
    fn track_signers<T>(&self, meta: &Entity<T, Draft>) -> Result<(), Error>
    where
        T: Serialize + DeserializeOwned + Clone + EntityInfoExt,
    {
        let span = tracing::debug_span!("Storage::track_signers", meta.urn = %meta.urn());
        let _guard = span.enter();

        let meta_urn = meta.urn();
        meta.signatures()
            .iter()
            .map(|(pk, sig)| {
                let peer_id = PeerId::from(pk.clone());
                match &sig.by {
                    Signatory::User(urn) => (peer_id, Some(urn)),
                    Signatory::OwnedKey => (peer_id, None),
                }
            })
            .filter(|(peer, _)| peer != self.peer_id())
            .try_for_each(|(peer, urn)| {
                tracing::debug!(
                    tracked.peer = %peer,
                    tracked.urn =
                        %urn.map(|urn| urn.to_string()).unwrap_or_else(|| "None".to_owned()),
                    "Tracking signer of {}",
                    meta.urn()
                );

                // Track the signer's version of this repo (if any)
                self.track(&meta_urn, &peer)?;
                // Track the signer's version of the identity she used for
                // signing (if any)
                if let Some(urn) = urn {
                    self.track(urn, &peer)?;
                }

                Ok(())
            })
    }

    pub(crate) fn update_refs(&self, urn: &RadUrn) -> Result<(), Error> {
        let span = tracing::debug_span!("Storage::update_refs");
        let _guard = span.enter();

        let refsig_canonical = self
            .rad_refs(urn)?
            .sign(&self.signer)
            .and_then(|signed| Cjson(signed).canonical_form())?;

        let rad_refs_ref = Reference::rad_refs(urn.id.clone(), None).to_string();

        let parent: Option<git2::Commit> = self
            .backend
            .find_reference(&rad_refs_ref)
            .and_then(|refs| refs.peel_to_commit().map(Some))
            .or_matches::<Error, _, _>(is_not_found_err, || Ok(None))?;
        let tree = {
            let blob = self.backend.blob(&refsig_canonical)?;
            let mut builder = self.backend.treebuilder(None)?;

            builder.insert("refs", blob, 0o100_644)?;
            let oid = builder.write()?;

            self.backend.find_tree(oid)
        }?;

        // Don't create a new commit if it would be the same tree as the parent
        if let Some(ref parent) = parent {
            if parent.tree()?.id() == tree.id() {
                return Ok(());
            }
        }

        let author = self.backend.signature()?;
        self.backend.commit(
            Some(&rad_refs_ref),
            &author,
            &author,
            "",
            &tree,
            &parent.iter().collect::<Vec<&git2::Commit>>(),
        )?;

        Ok(())
    }
}

impl<S: Clone> TryToOwned for Storage<S> {
    type Owned = Self;
    type Error = Error;

    fn try_to_owned(&self) -> Result<Self::Owned, Self::Error> {
        let backend = self.backend.try_to_owned()?;
        let peer_id = self.peer_id.clone();
        let signer = self.signer.clone();
        Ok(Self {
            backend,
            peer_id,
            signer,
        })
    }
}

/// Iterator over the 1st degree tracked peers of a repo.
///
/// Created by the [`Storage::tracked`] method.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Tracked {
    remotes: git2::string_array::StringArray,
    range: Range<usize>,
    prefix: String,
}

impl Tracked {
    pub(super) fn collect(repo: &git2::Repository, context: &RadUrn) -> Result<Self, git2::Error> {
        let remotes = repo.remotes()?;
        let range = 0..remotes.len();
        let prefix = format!("{}/", context.id);
        Ok(Self {
            remotes,
            range,
            prefix,
        })
    }
}

impl Iterator for Tracked {
    type Item = PeerId;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.range.next().and_then(|i| self.remotes.get(i));
        match next {
            None => None,
            Some(name) => {
                let peer = name
                    .strip_prefix(&self.prefix)
                    .and_then(|peer| peer.parse().ok());
                peer.or_else(|| self.next())
            },
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }
}

fn tracking_remote_name(urn: &RadUrn, peer: &PeerId) -> String {
    format!("{}/{}", urn.id, peer)
}

fn urn_from_ref(refname: &str) -> Option<RadUrn> {
    refname
        .split('/')
        .next_back()
        .and_then(|urn| urn.parse().ok())
}

fn urns_from_refs<'a, E>(
    refs: impl Iterator<Item = Result<&'a str, E>> + 'a,
) -> impl Iterator<Item = RadUrn> + 'a {
    refs.filter_map(|refname| refname.ok().and_then(urn_from_ref))
}