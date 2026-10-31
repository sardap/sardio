use std::path::Path;

use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;

#[derive(Serialize)]
pub struct AnalyticsEntry {
    pub start_time: i64,
    pub end_time: i64,
}

pub async fn save_analytics<P: AsRef<Path>>(
    analytics_path: P,
    id: u128,
    station_name: &str,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) {
    let uuid = uuid::Uuid::from_u128(id);
    let mut path = analytics_path.as_ref().join(&station_name).join(format!(
        "{}_{}",
        start_time.year(),
        start_time.month0()
    ));
    let _ = tokio::fs::create_dir_all(&path).await;
    path.push(uuid.to_string() + ".ron");

    if let Ok(analytics) = ron::ser::to_string(&AnalyticsEntry {
        start_time: start_time.timestamp(),
        end_time: end_time.timestamp(),
    }) {
        log::info!("Writing to analytics {}", path.display());
        let _ = tokio::fs::write(path, analytics).await;
    }
}

pub async fn save_history<HP: AsRef<Path>, MP: AsRef<Path>, FP: AsRef<Path>>(
    history_path: HP,
    music_path: MP,
    file_name: FP,
    station_name: &str,
    start: DateTime<Utc>,
) {
    let mut target = history_path
        .as_ref()
        .join(&station_name)
        .join(start.year().to_string())
        .join(start.month0().to_string())
        .join(start.day0().to_string());
    let _ = tokio::fs::create_dir_all(&target).await;

    let file_name = file_name.as_ref().trim_prefix(music_path.as_ref());
    target.push(format!("{}_{}.txt", station_name, start.timestamp()));
    let _ = tokio::fs::write(target, file_name.display().to_string()).await;
}
