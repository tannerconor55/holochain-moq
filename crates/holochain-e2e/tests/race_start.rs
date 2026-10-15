//! E6.1: real validation/control plane without websocket setup overhead.
//! Three real agent cells on one conductor; independent desktop gossip is
//! covered separately by scripts/e61-test.mjs, never substituted here.
mod common;
use common::*;
use holochain::sweettest::{SweetConductor, SweetDnaFile};
use holo_hash::ActionHash;
use holochain_moq_turbo_kart::{CreateLobbyInput, JoinLobbyInput, LobbySnapshot, LobbyState, LobbyStatus};
use holochain_moq_turbo_kart::race::{StartRaceInput, RaceRecord, RaceParticipant, RaceSetup};
use holochain_moq_turbo_kart_client::RaceAdmission;
use holochain_moq_protocol::{Endpoint, EndpointKind, TransportIdentity, IrohHints, BindingSignature, iroh_binding_payload, SignedPresence, EndpointAdvertisement, PeerEndpoint};
use holochain_moq_hdk_bridge::unix_now_ms;
#[derive(serde::Serialize, Debug)]
enum Forge { Race(RaceSetup) }
fn create(name:&str)->CreateLobbyInput {
    CreateLobbyInput { name:name.into(),max_players:2,track_id:"palm-cove".into(),class:"150cc".into(),laps:4,relay_urls:vec![],character_id:"rex".into(),invite_code:None }
}
fn presence(agent:&str,key:&holochain_moq_mesh_iroh::SecretKey)->SignedPresence {
    SignedPresence { advertisement:EndpointAdvertisement::new(PeerEndpoint { agent:agent.into(), endpoint:Endpoint {
        kind:EndpointKind::Iroh,address:IrohHints { direct_addrs:vec!["127.0.0.1:40001".into()],relay_urls:vec![] }.to_address().unwrap(),
        transport_identity:TransportIdentity { fingerprint:*key.public().as_bytes() },
    } },unix_now_ms(),Some(120_000)).unwrap(), binding_signature:BindingSignature::from_bytes(&key.sign(&iroh_binding_payload(key.public().as_bytes(),agent)).to_bytes()) }
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn immutable_start_authority_readiness_concurrency_and_lifecycle() {
    let directory=SweetDnaFile::from_bundle(&dna_bundle()).await.unwrap();
    let kart=SweetDnaFile::from_bundle(&kart_forge_dna_bundle()).await.unwrap();
    let mut conductor=SweetConductor::standard().await;
    let mut dir_cells=Vec::new();let mut kart_cells=Vec::new();
    for i in 0..3 {
        let app=conductor.setup_app(&format!("directory-{i}"),&[directory.clone()]).await.unwrap();
        let dc=app.into_cells().remove(0);
        let kc=conductor.setup_app_for_agent(&format!("kart-{i}"),dc.agent_pubkey().clone(),&[kart.clone()]).await.unwrap().into_cells().remove(0);
        dir_cells.push(dc);kart_cells.push(kc);
    }
    eprintln!("E61 three real agents initialized");
    let zomes:Vec<_>=kart_cells.iter().map(|c|c.zome("turbo_kart")).collect();
    let agents:Vec<_>=kart_cells.iter().map(|c|c.agent_pubkey().to_string()).collect();
    let lobby:ActionHash=conductor.call(&zomes[0],"create_lobby",create("Frozen")).await;
    let id=lobby.to_string();
    for (i,ch) in [(1,"zippy"),(2,"bella")] {
        let _:ActionHash=conductor.call(&zomes[i],"join_lobby",JoinLobbyInput { lobby:id.clone(),character_id:ch.into(),invite_code:None }).await;
    }
    let snap:Option<LobbySnapshot>=conductor.call(&zomes[0],"get_lobby",id.clone()).await;
    let snap=snap.unwrap(); assert_eq!(snap.members.len(),2);assert_eq!(snap.waitlist.len(),1);
    let keys:Vec<_>=(0..3).map(|_|holochain_moq_mesh_iroh::SecretKey::generate()).collect();
    let mut ads=Vec::new();let mut references=Vec::new();
    let mut invalid=presence(&agents[0],&keys[0]);invalid.binding_signature=BindingSignature::from_bytes(&[0;64]);
    let bad=conductor.call_fallible::<_,ActionHash>(&dir_cells[0].zome("moq_directory"),"publish_signed_presence",invalid).await;
    assert!(bad.is_err(),"invalid endpoint proof fails real directory validation");
    let absent:Vec<(String,SignedPresence)>=conductor.call(&dir_cells[0].zome("moq_directory"),"get_recent_endpoint_bindings",dir_cells[0].agent_pubkey().clone()).await;
    assert!(absent.is_empty(),"invalid proof cannot establish readiness");
    for i in 0..2 {
        let p=presence(&agents[i],&keys[i]);
        let h:ActionHash=conductor.call(&dir_cells[i].zome("moq_directory"),"publish_signed_presence",p.clone()).await;
        references.push(h.to_string());ads.push(p);
    }
    let input=StartRaceInput { lobby:id.clone(),participants:snap.members.iter().enumerate().map(|(i,m)|RaceParticipant {
        member:m.clone(),binding:references[i].clone(),endpoint_id:*keys[i].public().as_bytes(),
        binding_published_at_ms:ads[i].advertisement.published_at,binding_expires_at_ms:ads[i].advertisement.expires_at.unwrap(),
    }).collect() };
    assert!(conductor.call_fallible::<_,RaceRecord>(&zomes[1],"start_race",input.clone()).await.is_err(),"nonhost start fails");
    let mut expired=input.clone();expired.participants[0].binding_published_at_ms=1;expired.participants[0].binding_expires_at_ms=2;
    assert!(conductor.call_fallible::<_,RaceRecord>(&zomes[0],"start_race",expired).await.is_err(),"expiry at commit blocks start");
    eprintln!("E61 real readiness/authority checks complete");
    let (first,second)=tokio::join!(conductor.call_fallible::<_,RaceRecord>(&zomes[0],"start_race",input.clone()),conductor.call_fallible::<_,RaceRecord>(&zomes[0],"start_race",input.clone()));
    assert!(first.is_ok()||second.is_ok(),"one simultaneous request commits");
    let a:Option<RaceRecord>=conductor.call(&zomes[0],"get_race",id.clone()).await;
    let a=a.unwrap();
    if let Ok(r)=first {assert_eq!(r,a);}if let Ok(r)=second {assert_eq!(r,a);}
    let duplicate:RaceRecord=conductor.call(&zomes[0],"start_race",input).await;assert_eq!(a,duplicate);
    let b:Option<RaceRecord>=conductor.call(&zomes[1],"get_race",id.clone()).await;assert_eq!(b,Some(a.clone()));
    assert_eq!(a.setup.participants.iter().map(|p|p.member.clone()).collect::<Vec<_>>(),snap.members);
    assert_eq!(a.setup.config.class,"150cc");assert_eq!(a.setup.config.laps,4);assert_eq!(a.setup.config.track_id,"palm-cove");assert_eq!(a.setup.host_epoch,1);
    let admission=RaceAdmission::from_locked(&a,&agents[1],ads.clone()).unwrap();
    assert_eq!(admission.members.iter().find(|m|m.agent==agents[1]).unwrap().character_id,"zippy");
    assert_eq!(admission.config,a.setup.config);assert_eq!(admission.host_epoch,1);
    assert!(RaceAdmission::from_locked(&a,&agents[2],ads.clone()).is_err(),"waitlisted cannot obtain admission");
    let mut forged_proof=ads.clone();forged_proof[0].binding_signature=BindingSignature::from_bytes(&[0;64]);
    assert!(RaceAdmission::from_locked(&a,&agents[0],forged_proof).is_err());
    let mut mismatch=ads;mismatch[1].advertisement.peer_endpoint.agent=agents[2].clone();
    assert!(RaceAdmission::from_locked(&a,&agents[1],mismatch).is_err());
    let _:ActionHash=conductor.call(&zomes[1],"leave_lobby",id.clone()).await;
    let after:Option<RaceRecord>=conductor.call(&zomes[1],"get_race",id).await;assert_eq!(after,Some(a.clone()));
    let mut conflicting=a.setup.clone();conflicting.participants.pop();
    assert!(conductor.call_fallible::<_,ActionHash>(&kart_cells[0].zome("turbo_kart_forge"),"forge_entry",Forge::Race(conflicting)).await.is_err(),"raw duplicate writes fail integrity");
    eprintln!("E61 frozen roster/admission and duplicates checked");
    let left:ActionHash=conductor.call(&zomes[0],"create_lobby",create("Left")).await;
    let lid=left.to_string();let snap:Option<LobbySnapshot>=conductor.call(&zomes[0],"get_lobby",lid.clone()).await;
    let mut pending=a.setup.participants.clone();pending.truncate(1);pending[0].member=snap.unwrap().members[0].clone();
    let _:ActionHash=conductor.call(&zomes[0],"leave_lobby",lid.clone()).await;
    assert!(conductor.call_fallible::<_,RaceRecord>(&zomes[0],"start_race",StartRaceInput { lobby:lid,participants:pending }).await.is_err(),"host leaving before start fails");
    let closed:ActionHash=conductor.call(&zomes[0],"create_lobby",create("Closed")).await;
    let cid=closed.to_string();let _:ActionHash=conductor.call(&zomes[0],"set_lobby_status",LobbyStatus { lobby:cid.clone(),state:LobbyState::Abandoned,race_id:None }).await;
    assert!(conductor.call_fallible::<_,RaceRecord>(&zomes[0],"start_race",StartRaceInput { lobby:cid,participants:vec![] }).await.is_err(),"closed lobby fails");
    let changed:ActionHash=conductor.call(&zomes[0],"create_lobby",create("Changed")).await;
    let chid=changed.to_string();let snap:Option<LobbySnapshot>=conductor.call(&zomes[0],"get_lobby",chid.clone()).await;
    let mut stale=a.setup.participants.clone();stale.truncate(1);stale[0].member=snap.unwrap().members[0].clone();
    let _:ActionHash=conductor.call(&zomes[1],"join_lobby",JoinLobbyInput { lobby:chid.clone(),character_id:"zippy".into(),invite_code:None }).await;
    assert!(conductor.call_fallible::<_,RaceRecord>(&zomes[0],"start_race",StartRaceInput { lobby:chid,participants:stale }).await.is_err(),"observed roster change fails optimistic guard");
    conductor.shutdown().await;
}
