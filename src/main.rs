mod station;

use dotenvy::dotenv;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

use actix_files::{Files, NamedFile};
use actix_web::{
    App, HttpRequest, HttpResponse, HttpServer, Responder, get,
    web::{self, resource},
};
use askama::Template;
use askama_web::WebTemplate;
use lazy_static::lazy_static;
use tokio::sync::{Mutex, broadcast};

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
}

#[get("/station/{name}/play")]
async fn get_station_play(
    name: web::Path<String>,
    radio_states: web::Data<RadioState>,
    listen_states: web::Data<ListeningState>,
) -> actix_web::Result<impl Responder> {
    let info = match radio_states.get(&name.into_inner()) {
        Some(station) => station,
        None => return Ok(HttpResponse::NotFound().body("not found")),
    };

    let name = info.name();
    let id = uuid::Uuid::new_v4().as_u128();

    listen_states.add_listener(&name, id).await;

    log::info!("added listener for {} {}", name, id);

    let (backlog, mut rx) = info.data_rx();

    let guard = ListenerGuard {
        listen_states: listen_states.get_ref().clone(),
        name,
        id,
    };

    let ogg_stream = async_stream::stream! {
        let _guard = guard;
        for chunk in backlog {
            yield Ok::<_, actix_web::Error>(chunk);
        }
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    log::debug!("Sending chunk {}",  id);
                    yield Ok::<_, actix_web::Error>(chunk);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log::error!("Client lagged behind by {} chunks", skipped);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    log::debug!("closed  {}",  id);
                    break;
                }
            }
        }
    };

    Ok(HttpResponse::Ok()
        .content_type("audio/mpeg")
        .insert_header(("Cache-Control", "no-cache, no-store, must-revalidate"))
        .insert_header(("Connection", "keep-alive"))
        .streaming(ogg_stream))
}

#[derive(Template, WebTemplate)]
#[template(path = "now.html")]
struct NowTemplate {
    current_title: String,
    listeners: usize,
}

#[get("/station/{name}/now")]
async fn station_now(
    name: web::Path<String>,
    radio_states: web::Data<RadioState>,
    listen_states: web::Data<ListeningState>,
) -> NowTemplate {
    let name = name.into_inner();
    match radio_states.get(&name) {
        Some(station) => {
            let playing = if station.config().hide_playing {
                "????".to_string()
            } else {
                station.playing_now().await
            };
            NowTemplate {
                current_title: playing,
                listeners: listen_states.get_active(&name).await,
            }
        }
        None => NowTemplate {
            current_title: "Not found".to_string(),
            listeners: 0,
        },
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
struct IndexTemplate {
    stations: Vec<String>,
    selected: String,
}

#[derive(Debug, Deserialize)]
struct IndexQuery {
    #[serde(default)]
    p: Option<String>,
}

async fn get_index(
    query: web::Query<IndexQuery>,
    radio_states: web::Data<RadioState>,
) -> IndexTemplate {
    let stations = radio_states.keys();
    IndexTemplate {
        selected: query.into_inner().p.unwrap_or(stations[0].clone()),
        stations,
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "player.html")]
struct PlayerTemplate {
    station: String,
    background_colour: String,
    border_colour: String,
    border_hover_colour: String,
    text_colour: String,
}

#[derive(Debug, Deserialize)]
struct PlayerQuery {
    station: String,
}

async fn get_player(
    query: web::Query<PlayerQuery>,
    radio_states: web::Data<RadioState>,
) -> PlayerTemplate {
    let station_name = query.0.station;

    let config = radio_states
        .get(&station_name)
        .map(|i| i.config())
        .cloned()
        .unwrap_or_default();

    PlayerTemplate {
        station: station_name,
        background_colour: config.background,
        border_colour: config.border,
        border_hover_colour: config.border_hover,
        text_colour: config.text_colour,
    }
}

#[get("/station/{name}/image.png")]
async fn get_station_image(
    name: web::Path<String>,
    radio_states: web::Data<RadioState>,
) -> actix_web::Result<impl Responder> {
    match radio_states.get(&name.into_inner()) {
        Some(station) => Ok(NamedFile::open(station.image_path())?),
        None => Err(actix_web::error::ErrorNotFound("Not found")),
    }
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

struct ListenerGuard {
    listen_states: ListeningState,
    name: String,
    id: u128,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let listen_states = self.listen_states.clone();
        let name = std::mem::take(&mut self.name);
        let id = self.id;
        actix_web::rt::spawn(async move {
            listen_states.remove_listener(&name, id).await;
            log::info!("Removing listener for {} {}", name, id);
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

    log::info!("Radio station broadcasting on http://127.0.0.1:8080/live.ogg");

    HttpServer::new(move || {
        App::new()
            .app_data(radio_state.clone())
            .app_data(listen_state.clone())
            .wrap(actix_web::middleware::Logger::default())
            .service(resource("/").to(get_index))
            .service(resource("/front/player").to(get_player))
            .service(get_station_play)
            .service(station_now)
            .service(get_station_image)
            .service(Files::new("/static", &*STATIC_PATH))
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
