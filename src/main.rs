use crate::device::{ConnectionParams, Device};
use futures_util::StreamExt;
use igloo_interface::ipc::{self, IglooMessage};
use rustc_hash::{FxBuildHasher, FxHashMap};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, error::Error, io::SeekFrom, path::PathBuf, sync::Arc};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};

pub mod connection;
pub mod device;
pub mod entity;
pub mod api {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}
pub mod model {
    include!(concat!(env!("OUT_DIR"), "/model.rs"));
}

pub const CONFIG_FILE: &str = "config.toml";

/// Eventually this will be described in the Igloo.toml file
pub const ADD_DEVICE: u16 = 32;

#[derive(Debug)]
pub struct ConfigManager {
    file: File,
    config: Config,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// maps Persisnt Igloo Device ID -> Connection Params
    #[serde(rename = "device")]
    devices: FxHashMap<u64, ConnectionParams>,
}

pub type CommandAndPayload = (u16, Vec<u8>);

#[tokio::main]
async fn main() {
    let mut cm = ConfigManager::load().await.unwrap();

    let (mut writer, mut reader) = ipc::connect()
        .await
        .expect("Failed to initialize Extension");

    // writer task
    let (write_tx, write_rx) = kanal::bounded_async(100);
    tokio::spawn(async move {
        loop {
            let msg = match write_rx.recv().await {
                Ok(msg) => msg,
                Err(e) => {
                    eprintln!("Error reading from write_rx: {e}");
                    break;
                }
            };

            if let Err(e) = writer.write(&msg).await {
                eprintln!("Error writing message to Igloo: {e}");
            }
        }

        println!("Write task shutdown");
    });

    // Device ID -> Device Channel
    let mut device_txs = HashMap::with_capacity_and_hasher(20, FxBuildHasher);

    // connect to devices in config
    for (device_id, params) in cm.config.devices.clone() {
        let (device_tx, deivce_rx) = kanal::bounded_async(50);
        device_txs.insert(device_id, device_tx);
        let mut device = Device::new(device_id, params);
        let write_tx_1 = write_tx.clone();
        tokio::spawn(async move {
            let did = device.id;
            if let Err(e) = device.connect().await {
                eprintln!("Error connecting to device ID={did}: {e}");
                return;
            }
            println!("Device ID={did} connected.");
            if let Err(e) = device.run(write_tx_1, deivce_rx).await {
                eprintln!("Error running device ID={did}: {e}");
            }
        });
    }

    let pending_creation: Arc<Mutex<FxHashMap<String, Device>>> = Arc::new(Mutex::new(
        HashMap::with_capacity_and_hasher(5, FxBuildHasher),
    ));

    while let Some(res) = reader.next().await {
        let msg = match res {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Error reading message, skipping: {e}");
                continue;
            }
        };

        use IglooMessage::*;
        match msg {
            DeviceCreated(name, did) => {
                // pull out pending device
                let mut pc = pending_creation.lock().await;
                let Some(mut device) = pc.remove(&name) else {
                    eprintln!("Igloo sent DeviceCreated for unknown device. Skipping..");
                    continue;
                };
                drop(pc);

                // save to disk
                cm.config.devices.insert(did, device.params.clone());
                cm.save().await.unwrap();

                // give actual ID now
                device.id = did;

                // run
                let (device_tx, device_rx) = kanal::bounded_async(50);
                device_txs.insert(did, device_tx);
                let write_tx_1 = write_tx.clone();
                tokio::spawn(async move {
                    let res = device.run(write_tx_1, device_rx).await;
                    if let Err(e) = res {
                        eprintln!("Device {did} crashed: {e}");
                    }
                });
            }

            WriteComponents {
                device: did,
                entity,
                comps,
            } => {
                let Some(device) = device_txs.get(&did) else {
                    eprintln!("Igloo sent write for unknown device '{did}'. Skipping..");
                    continue;
                };

                if let Err(e) = device.send((entity, comps)).await {
                    eprintln!("Error sending message to device '{did}': {e}");
                }
            }

            Custom { name, params } => {
                if name != "Add Device" {
                    eprintln!("Unknown custom command '{name}'. Skipping..");
                    continue;
                }

                let Some(ip) = params.get("ip") else {
                    eprintln!(
                        "Parameter 'ip' must be specified for custom command 'Add Device'. Skipping.."
                    );
                    continue;
                };

                let params = ConnectionParams {
                    ip: ip.clone(),
                    noise_psk: params.get("noise_psk").cloned(),
                    password: params.get("password").cloned(),
                    name: params.get("name").cloned(),
                };

                let mut device = Device::new(0, params);
                let write_tx_1 = write_tx.clone();
                let pending_creation_1 = pending_creation.clone();
                tokio::spawn(async move {
                    let info = device.connect().await.unwrap();
                    write_tx_1
                        .send(IglooMessage::CreateDevice(info.name.clone()))
                        .await
                        .unwrap();
                    let mut pc = pending_creation_1.lock().await;
                    pc.insert(info.name, device);
                    drop(pc);
                });
            }

            WhatsUpIgloo { .. } | CreateDevice(..) | RegisterEntity { .. } => {
                eprintln!("Igloo unexpectedly sent client message. Skipping..");
            }
        }
    }
}

impl ConfigManager {
    async fn load() -> Result<Self, Box<dyn Error>> {
        let path: PathBuf = [&ipc::get_data_path(), CONFIG_FILE].iter().collect();

        if !fs::try_exists(&path).await? {
            fs::write(&path, "").await?;
        }

        let mut file = File::options().read(true).write(true).open(&path).await?;

        let meta = file.metadata().await?;
        if meta.is_dir() {
            return Err(format!("{} should not be directory", path.to_string_lossy()).into());
        }

        if meta.is_symlink() {
            let sym_meta = fs::symlink_metadata(&path).await?;
            if sym_meta.is_dir() {
                return Err(format!("{} should not be directory", path.to_string_lossy()).into());
            }
        }

        let mut content = String::with_capacity(meta.len() as usize);
        file.read_to_string(&mut content).await?;

        Ok(Self {
            file,
            config: toml::from_str(&content)?,
        })
    }

    async fn save(&mut self) -> Result<(), Box<dyn Error>> {
        let content = toml::to_string_pretty(&self.config)?;
        self.file.seek(SeekFrom::Start(0)).await?;
        self.file.write_all(content.as_bytes()).await?;
        self.file.flush().await?;
        self.file.set_len(content.len() as u64).await?;

        Ok(())
    }
}
