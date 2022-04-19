use libp2p::kad::record::store::MemoryStore;
use std::fs;
use std::path::Path;
use libp2p::kad::{Kademlia, record::Key};
use libp2p::{
    development_transport, 
    identity,
    mdns::{Mdns, MdnsConfig},
    PeerId, 
    Swarm,
};
use async_std::task;
use std::error::Error;
use std::{str, env};
use std::str::FromStr;
use secp256k1::rand::rngs::OsRng;
use secp256k1::{PublicKey, Secp256k1, SecretKey, Message};
use secp256k1::hashes::sha256;
use secp256k1::ecdsa::Signature;
use tonic::{transport::Server, Request, Response, Status, Code};
use tokio::sync::{mpsc, broadcast};
use futures::stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tokio::sync::Mutex;
use std::sync::Arc;

use api::api_server::{Api, ApiServer};
use api::{GetRequest, GetResponse, PutResponse, PutRequest, Entry, FileUploadRequest, FileUploadResponse, MetaData, File, file_upload_request::UploadRequest};

mod api {
	tonic::include_proto!("api");
}

mod handler;
mod entry;
mod behaviour;
mod dht;

use behaviour::MyBehaviour;
use dht::Dht;
use handler::{MyApi, DhtResponseType, DhtGetRecordResponse, DhtRequestType, DhtPutRecordResponse};

async fn create_swarm() -> Swarm<MyBehaviour> {
	let local_key = identity::Keypair::generate_ed25519();
	let local_peer_id = PeerId::from(local_key.public());

	let transport = development_transport(local_key).await.unwrap();

	let store = MemoryStore::new(local_peer_id);
        let kademlia = Kademlia::new(local_peer_id, store);
        let mdns = task::block_on(Mdns::new(MdnsConfig::default())).unwrap();
        let behaviour = MyBehaviour { 
		kademlia, 
		mdns, 
	};
        Swarm::new(transport, behaviour, local_peer_id)
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
	let args: Vec<String> = env::args().collect();

	if args.len() > 1 && &args[1] == "gen-keypair" {
		let secp = Secp256k1::new();
		let mut rng = OsRng::new().unwrap();
		let (secret_key, public_key) = secp.generate_keypair(&mut rng);

		println!("Public key: {}\nPrivate Key: {}", public_key.to_string(), secret_key.display_secret());

		return Ok(());
	}

	let mut swarm = create_swarm().await;
	swarm.listen_on("/ip4/192.168.0.164/tcp/0".parse()?)?;
	let mut dht_swarm = Dht::new(swarm);

	let (mpsc_sender, mpsc_receiver) = mpsc::channel::<DhtRequestType>(32);
	let (broadcast_sender, broadcast_receiver) = broadcast::channel::<DhtResponseType>(32);

	tokio::spawn(async move {
		let mut mpsc_receiver_stream = ReceiverStream::new(mpsc_receiver);

		while let Some(data) = mpsc_receiver_stream.next().await {
			match data {
				DhtRequestType::GetRecord(dht_get_record) => {
					let key = handler::get_location_key(dht_get_record.location);

					match dht_swarm.get(&key).await {
						Ok(record) => {
							let entry: Entry = serde_json::from_str(&str::from_utf8(&record.value).unwrap()).unwrap();

							broadcast_sender.send(DhtResponseType::GetRecord(DhtGetRecordResponse {
								entry: Some(entry),
								error: None
							})).unwrap();
                                                }
						Err(error) => {
							broadcast_sender.send(DhtResponseType::GetRecord(DhtGetRecordResponse {
								entry: None,
								error: Some(error.to_string())
							})).unwrap();
						}
					};
				}
				DhtRequestType::PutRecord(dht_put_record) => {
					let value = serde_json::to_vec(&dht_put_record.entry).unwrap();
					let pub_key = dht_put_record.public_key.clone();
					let key: String = format!("e_{}", dht_put_record.signature);

					let secp = Secp256k1::new();
					let sig = Signature::from_str(&dht_put_record.signature.clone()).unwrap();
					let message = Message::from_hashed_data::<sha256::Hash>(
						format!("{}/{}", pub_key, dht_put_record.entry.name).as_bytes()
					);

					match secp.verify_ecdsa(&message, &sig, &pub_key) {
						Err(error) => {
							broadcast_sender.send(DhtResponseType::PutRecord(DhtPutRecordResponse {
								signature: Some(key),
								error: Some((Code::Unauthenticated, "Invalid signature".to_string()))
							})).unwrap();
							continue;
						}
						_ => {}
					}

					let res = match dht_swarm.put(Key::new(&key.clone()), value).await {
						Ok(_) => DhtResponseType::PutRecord(DhtPutRecordResponse { 
                                                        signature: Some(key),
                                                        error: None
                                                }),
						Err(error) => DhtResponseType::PutRecord(DhtPutRecordResponse { 
                                                        // signature: None,
                                                        // error: Some((Code::Unknown, error.to_string()))
                                                        error: None,
							signature: Some(key)
                                                })
					};

                                        broadcast_sender.send(res);

				}
			};

		}
	});

	let say = MyApi {
		mpsc_sender,
		broadcast_receiver: Arc::new(Mutex::new(broadcast_receiver))
	};
	let server = Server::builder().add_service(ApiServer::new(say));

	let addr = "192.168.0.164:50051".parse().unwrap();
	println!("Server listening on {}", addr);
	server.serve(addr).await;

	Ok(())
}
