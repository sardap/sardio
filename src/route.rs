use actix_files::NamedFile;
use actix_web::{
    HttpResponse, Responder, get,
    web::{self, resource},
};
use askama::Template;
use askama_web::WebTemplate;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::{ListenerGuard, ListeningState, RadioState};

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

    let guard = ListenerGuard::new(listen_states.get_ref().clone(), &name, id);

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

pub(crate) fn scope() -> actix_web::Scope {
    web::scope("")
        .service(resource("/").to(get_index))
        .service(resource("/front/player").to(get_player))
        .service(get_station_play)
        .service(station_now)
        .service(get_station_image)
}
