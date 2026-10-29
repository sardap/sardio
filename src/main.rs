mod route;
mod station;

use chrono::{DateTime, Utc};
use dotenvy::dotenv;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

use actix_files::Files;
use actix_web::{
    App, HttpServer,
    web::{self},
};
use lazy_static::lazy_static;
use tokio::sync::Mutex;

use crate::station::{StationInfo, StationRunner};

lazy_static! {
    static ref STATIC_PATH: PathBuf = PathBuf::from_str(&std::env::var("STATIC_PATH").unwrap())
        .unwrap()
        .canonicalize()
        .unwrap();
    static ref MUSIC_PATH: PathBuf = PathBuf::from_str(&std::env::var("MUSIC_PATH").unwrap())
        .unwrap()
        .canonicalize()
        .unwrap();
    static ref ANALYTICS_PATH: PathBuf =
        PathBuf::from_str(&std::env::var("ANALYTICS_PATH").unwrap())
            .unwrap()
            .canonicalize()
            .unwrap();
}

#[derive(Debug, Clone, Default)]
pub struct ListeningState {
    pub listeners: Arc<Mutex<HashMap<String, HashSet<u128>>>>,
}

impl ListeningState {
    pub async fn get_active(&self, key: &str) -> usize {
        let map = self.listeners.lock().await;
        log::info!("ga {:?}", map);
        map.get(key).map(|i| i.len()).unwrap_or_default()
    }

    pub async fn add_listener(&self, key: &str, id: u128) {
        let mut map = self.listeners.lock().await;
        let set = map.entry(key.to_string()).or_default();
        set.insert(id);
        log::info!("al {:?}", map);
    }

    pub async fn remove_listener(&self, key: &str, id: u128) {
        let mut map = self.listeners.lock().await;
        if let Some(set) = map.get_mut(key) {
            set.remove(&id);
        }
    }
}

#[derive(Serialize)]
struct AnalyticsEntry {
    name: String,
    start_time: i64,
    end_time: i64,
}

struct ListenerGuard {
    listen_states: ListeningState,
    name: String,
    id: u128,
    start_time: DateTime<Utc>,
}

impl ListenerGuard {
    pub fn new(listen_states: ListeningState, name: &str, id: u128) -> Self {
        Self {
            listen_states,
            name: name.to_string(),
            id,
            start_time: Utc::now(),
        }
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let start_time = self.start_time;
        let end_time = Utc::now();
        let listen_states = self.listen_states.clone();
        let name = std::mem::take(&mut self.name);
        let id = self.id;
        actix_web::rt::spawn(async move {
            listen_states.remove_listener(&name, id).await;
            log::info!("Removing listener for {} {}", name, id);

            let uuid = uuid::Uuid::from_u128(id);
            let path = ANALYTICS_PATH.join(uuid.to_string() + ".ron");

            if let Ok(analytics) = ron::ser::to_string(&AnalyticsEntry {
                name,
                start_time: start_time.timestamp(),
                end_time: end_time.timestamp(),
            }) {
                log::info!("Writing to analytics {}", path.display());
                let _ = tokio::fs::write(path, analytics).await;
            }
        });
    }
}

struct RadioState {
    stations: HashMap<String, StationInfo>,
}

impl RadioState {
    pub fn get(&self, name: &str) -> Option<&StationInfo> {
        self.stations.get(name)
    }

    pub fn keys(&self) -> Vec<String> {
        let mut result: Vec<_> = self.stations.keys().map(|i| i.to_string()).collect();

        result.sort();

        result
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();
    env_logger::init();

    let list_dir = std::fs::read_dir(&*MUSIC_PATH).unwrap();

    let mut stations = vec![];

    for entry in list_dir.filter_map(|i| i.ok()) {
        if entry.path().is_dir() {
            stations.push(StationRunner::new(entry.path()));
        }
    }

    let mut station_infos = HashMap::new();
    for station in &stations {
        let info = station.station_info().await;
        if info.name() != "giant_bomb" && false {
            log::info!("Checking bad songs for {}", info.name());
            info.purge_bad_songs().await;
        }
        log::info!("Loaded {}", info.name());
        station_infos.insert(info.name(), info);
    }

    for mut station in stations {
        tokio::spawn(async move {
            station.run().await;
        });
    }

    let radio_state = web::Data::new(RadioState {
        stations: station_infos,
    });

    let listen_state = web::Data::new(ListeningState::default());

    log::info!("Radio station broadcasting");

    HttpServer::new(move || {
        App::new()
            .app_data(radio_state.clone())
            .app_data(listen_state.clone())
            .wrap(actix_web::middleware::Logger::default())
            .service(Files::new("/static", &*STATIC_PATH))
            .service(crate::route::scope())
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
