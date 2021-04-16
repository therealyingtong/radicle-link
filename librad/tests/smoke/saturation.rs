// Copyright © 2019-2020 The Radicle Foundation <hello@radicle.foundation>
//
// This file is part of radicle-link, distributed under the GPLv3 with Radicle
// Linking Exception. For full terms see the included LICENSE file.

use std::{convert::TryFrom, time::Duration};

use futures::StreamExt;

use librad::{
    git::{identities, tracking},
    git_ext::RefLike,
    identities::payload,
    net::protocol::{
        event::{self, upstream::predicate::gossip_from},
        gossip,
    },
};
use librad_test::{
    logging,
    rad::{identities::TestProject, testnet},
};

#[tokio::test]
async fn saturate_a_peer_with_projects() {
    logging::init();

    const NUM_PEERS: usize = 2;
    const NUM_PROJECTS: usize = 15;

    let peers = testnet::setup(NUM_PEERS).await.unwrap();
    testnet::run_on_testnet(peers, NUM_PEERS, |mut peers| async move {
        let peer1 = peers.pop().unwrap();
        let peer2 = peers.pop().unwrap();

        let payloads = (1..NUM_PROJECTS).into_iter().map(|n| payload::Project {
            name: format!("radicle-{}", n).into(),
            description: None,
            default_branch: Some(format!("rad-{}", n).into()),
        });
        let projs = peer1
            .using_storage({
                move |storage| {
                    let mut projs = Vec::with_capacity(NUM_PROJECTS + 1);
                    let proj = TestProject::create(&storage)?;
                    let owner = proj.owner.clone();
                    projs.push(proj);
                    for payload in payloads {
                        projs.push(TestProject::from_project_payload(
                            &storage,
                            owner.clone(),
                            payload,
                        )?);
                    }
                    Ok::<_, anyhow::Error>(projs)
                }
            })
            .await
            .unwrap()
            .unwrap();
        peer2
            .using_storage({
                let remote = peer1.peer_id();
                let urns = projs
                    .iter()
                    .map(|proj| proj.project.urn())
                    .collect::<Vec<_>>();
                move |storage| -> Result<(), anyhow::Error> {
                    for urn in urns {
                        tracking::track(storage, &urn, remote)?;
                    }
                    Ok(())
                }
            })
            .await
            .unwrap()
            .unwrap();

        for proj in projs.iter() {
            let branch = RefLike::try_from(
                proj.project
                    .subject()
                    .default_branch
                    .as_ref()
                    .unwrap()
                    .as_str(),
            )
            .unwrap();
            peer1
                .announce(gossip::Payload {
                    origin: None,
                    urn: proj.project.urn().with_path(branch.clone()),
                    rev: None,
                })
                .unwrap();

            let peer2_events = peer2.subscribe();
            event::upstream::expect(
                peer2_events.boxed(),
                gossip_from(peer1.peer_id()),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        }

        let n_projects = peer2
            .using_storage(move |storage| -> Result<usize, anyhow::Error> {
                Ok(identities::any::list(&storage)?
                    .filter_map(|some| some.unwrap().project())
                    .count())
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n_projects, NUM_PROJECTS);
    })
    .await;
}
