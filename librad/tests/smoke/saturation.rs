// Copyright © 2019-2020 The Radicle Foundation <hello@radicle.foundation>
//
// This file is part of radicle-link, distributed under the GPLv3 with Radicle
// Linking Exception. For full terms see the included LICENSE file.

use librad::identities::payload;
use librad_test::{
    logging,
    rad::{identities::TestProject, testnet},
};

#[tokio::test]
async fn saturate_a_peer_with_projects() {
    logging::init();

    const NUM_PEERS: usize = 2;
    const NUM_PROJECTS: usize = 11;

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

        for proj in projs.iter() {
            proj.pull(&peer1, &peer2).await.ok().unwrap();
        }
    })
    .await;
}
