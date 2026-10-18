mod station;

use dotenvy::dotenv;
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf, str::FromStr};

use actix_files::{Files, NamedFile};
use actix_web::{
    App, HttpResponse, HttpServer, Responder, get,
    web::{self, resource},
};
use askama::Template;
use askama_web::WebTemplate;
use lazy_static::lazy_static;
use tokio::sync::broadcast;

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
async fn song(
    name: web::Path<String>,
    radio_states: web::Data<RadioState>,
) -> actix_web::Result<impl Responder> {
    let info = match radio_states.get(&name.into_inner()) {
        Some(station) => station,
        None => return Ok(HttpResponse::NotFound().body("not found")),
    };

    let (backlog, mut rx) = info.data_rx();

    let ogg_stream = async_stream::stream! {
        for chunk in backlog {
            yield Ok::<_, actix_web::Error>(chunk);
        }
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    yield Ok::<_, actix_web::Error>(chunk);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!("Client lagged behind by {} chunks", skipped);
                }
                Err(broadcast::error::RecvError::Closed) => {
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

#[get("/station/{name}/now")]
async fn station_now(
    name: web::Path<String>,
    radio_states: web::Data<RadioState>,
) -> impl Responder {
    match radio_states.get(&name.into_inner()) {
        Some(station) => HttpResponse::Ok().body(station.playing_now().await),
        None => HttpResponse::NotFound().body("not found"),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
struct IndexTemplate {
    stations: Vec<String>,
}

async fn get_index(radio_states: web::Data<RadioState>) -> IndexTemplate {
    IndexTemplate {
        stations: radio_states.keys(),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "player.html")]
struct PlayerTemplate {
    station: String,
}

#[derive(Debug, Deserialize)]
struct PlayerQuery {
    station: String,
}

async fn get_player(query: web::Query<PlayerQuery>) -> PlayerTemplate {
    let station_name = query.0.station;

    PlayerTemplate {
        station: station_name,
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
        info.purge_bad_songs().await;
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

    log::info!("Radio station broadcasting on http://127.0.0.1:8080/live.ogg");

    HttpServer::new(move || {
        App::new()
            .app_data(radio_state.clone())
            .service(resource("/").to(get_index))
            .service(resource("/front/player").to(get_player))
            .service(song)
            .service(station_now)
            .service(get_station_image)
            .service(Files::new("/static", &*STATIC_PATH))
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
