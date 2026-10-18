use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
};

use bytes::Bytes;
use chrono::{Datelike, Timelike};
use lazy_static::lazy_static;
use lofty::{file::AudioFile, probe::Probe};
use regex::Regex;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{
        Mutex,
        broadcast::{self, Receiver, Sender},
    },
};

lazy_static! {
    static ref SONG_SUFFIX: Regex =
        Regex::new(r"(?:\s*\[[A-Za-z0-9_-]{11}\]|-[A-Za-z0-9_-]{11})?\.[A-Za-z0-9]+$").unwrap();
    static ref SPACES: Regex = Regex::new(r"\s+").unwrap();
}

const BACKLOG_BYTES: usize = 48 * 1024;

#[derive(Default)]
struct Backlog {
    chunks: VecDeque<Bytes>,
    len: usize,
}

impl Backlog {
    fn push(&mut self, chunk: Bytes) {
        self.len += chunk.len();
        self.chunks.push_back(chunk);
        while self.len > BACKLOG_BYTES {
            match self.chunks.pop_front() {
                Some(old) => self.len -= old.len(),
                None => break,
            }
        }
    }
}

fn song_title(file_name: &str) -> String {
    let title = SONG_SUFFIX.replace(file_name, "");
    let title = title.replace('\u{FF1A}', ":").replace('\u{29F8}', "/");
    SPACES.replace_all(title.trim(), " ").into_owned()
}

pub struct StationInfo {
    path: PathBuf,
    data_tx: Sender<Bytes>,
    backlog: Arc<StdMutex<Backlog>>,
    playing_now: Arc<Mutex<String>>,
}

impl StationInfo {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|i| i.to_str())
            .flatten()
            .map(|i| i.to_string())
            .unwrap_or_default()
    }

    pub async fn playing_now(&self) -> String {
        song_title(&self.playing_now.lock().await)
    }

    pub fn data_rx(&self) -> (Vec<Bytes>, Receiver<Bytes>) {
        let backlog = self.backlog.lock().unwrap();
        (
            backlog.chunks.iter().cloned().collect(),
            self.data_tx.subscribe(),
        )
    }

    pub fn image_path(&self) -> PathBuf {
        self.path.join("image.png")
    }

    pub async fn purge_bad_songs(&self) {
        for song in std::fs::read_dir(&self.path)
            .unwrap()
            .filter_map(|i| i.ok())
        {
            if let Ok(tagged_file) = Probe::open(&song.path()).map(|i| i.read()).flatten() {
                let properties = tagged_file.properties();
                let duration = chrono::Duration::from_std(properties.duration()).unwrap();
                if duration < chrono::Duration::minutes(1) {
                    log::info!("Removing {}", song.path().display());
                    let _ = std::fs::remove_file(song.path());
                }
            }
        }
    }
}

pub struct StationRunner {
    path: PathBuf,
    tx: Sender<Bytes>,
    backlog: Arc<StdMutex<Backlog>>,
    playing_now: Arc<Mutex<String>>,
}

impl StationRunner {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let (tx, _) = broadcast::channel::<Bytes>(1024);

        Self {
            path: PathBuf::new().join(path),
            tx,
            backlog: Arc::default(),
            playing_now: Arc::new(Mutex::new(String::default())),
        }
    }

    pub async fn station_info(&self) -> StationInfo {
        StationInfo {
            path: self.path.clone(),
            data_tx: self.tx.clone(),
            backlog: self.backlog.clone(),
            playing_now: self.playing_now.clone(),
        }
    }

    pub async fn run(&mut self) {
        let mut buffer = vec![0u8; 4096];

        loop {
            let now = chrono::Utc::now();
            let current_seed: u64 = format!(
                "{}{}{}{}{}",
                now.year(),
                now.month0(),
                now.day0(),
                now.hour(),
                now.minute()
            )
            .parse()
            .unwrap();

            let mut rng = fastrand::Rng::with_seed(current_seed);

            let song = rng.choice(self.get_songs()).unwrap();
            {
                let mut now = self.playing_now.lock().await;
                *now = song
                    .file_name()
                    .map(|i| i.to_str())
                    .flatten()
                    .map(|i| i.to_string())
                    .unwrap_or_default();
            }
            let tagged_file = Probe::open(&song)
                .expect("Failed to open file")
                .read()
                .expect("Failed to read Ogg metadata");

            let properties = tagged_file.properties();
            let duration = chrono::Duration::from_std(properties.duration()).unwrap();

            let max_song_time = chrono::Duration::minutes(rng.i64(5..12))
                + chrono::Duration::seconds(rng.i64(0..60));

            let loops = if duration > max_song_time {
                0i32
            } else {
                (max_song_time.num_milliseconds() as f32 / duration.num_milliseconds() as f32)
                    .floor()
                    .max(1.) as i32
            };
            log::info!("Looping {} {}", song.display(), loops);

            let mut ffmpeg = Command::new("ffmpeg")
                .arg("-re")
                .arg("-stream_loop")
                .arg(loops.to_string())
                .arg("-i")
                .arg(&song)
                .arg("-c:a")
                .arg("libmp3lame")
                .arg("-b:a")
                .arg("64k")
                .arg("-ac")
                .arg("1")
                .arg("-f")
                .arg("mp3")
                .arg("pipe:1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("Failed to start FFmpeg. Is it installed on your system?");
            let mut stdout = ffmpeg.stdout.take().expect("Failed to open FFmpeg stdout");

            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) => {
                        log::info!("{} is done playing", song.display());
                        break;
                    }
                    Ok(n) => {
                        let chunk = Bytes::copy_from_slice(&buffer[..n]);
                        let mut backlog = self.backlog.lock().unwrap();
                        backlog.push(chunk.clone());
                        let _ = self.tx.send(chunk);
                    }
                    Err(e) => {
                        log::error!("Error reading from FFmpeg: {}", e);
                        break;
                    }
                }
            }
        }
    }

    pub fn get_songs(&self) -> Vec<PathBuf> {
        let mut files: Vec<_> = std::fs::read_dir(&self.path)
            .unwrap()
            .filter_map(|i| i.ok())
            .filter(|i| match i.path().extension() {
                None => false,
                Some(ext) => {
                    [OsStr::new("ogg"), OsStr::new("mp3"), OsStr::new("m4a")].contains(&ext)
                }
            })
            .map(|i| i.path())
            .collect();

        files.sort();

        files
    }
}

#[cfg(test)]
mod tests {
    use super::song_title;

    #[test]
    fn strips_id_and_extension() {
        let cases = [
            (
                "Horse Grass [Howl] - The Legend of Zelda - Twilight Princess [GTTd2KJPUmk].m4a",
                "Horse Grass [Howl] - The Legend of Zelda - Twilight Princess",
            ),
            (
                "Gorgeous-Go-Round (Casino Area) - Kirby\u{FF1A} Planet Robobot OST [020] [MSM1M2roCjY].ogg",
                "Gorgeous-Go-Round (Casino Area) - Kirby: Planet Robobot OST [020]",
            ),
            (
                "De-De-Deluxe!!! - Kirby Triple Deluxe [EdWTDNMBb-c].ogg",
                "De-De-Deluxe!!! - Kirby Triple Deluxe",
            ),
            (
                "Generic BGM - B - Megami Ibunroku Persona-OuhUX5K-a6Y.mp3",
                "Generic BGM - B - Megami Ibunroku Persona",
            ),
            (
                "35   Casino   Mu Continent-k0Fh1SYxyl4.mp3",
                "35 Casino Mu Continent",
            ),
            (
                "City 2 Accident  - Megami Ibunroku Persona-pLxqqNocoQk.mp3",
                "City 2 Accident - Megami Ibunroku Persona",
            ),
            ("Plain Song.mp3", "Plain Song"),
        ];
        for (file_name, title) in cases {
            assert_eq!(song_title(file_name), title);
        }
    }
}
