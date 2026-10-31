use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    thread::JoinHandle,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use lazy_static::lazy_static;
use lofty::{
    file::{AudioFile, FileType, TaggedFileExt},
    probe::Probe,
};
use regex::Regex;
use serde::Deserialize;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{
        Mutex,
        broadcast::{self, Receiver, Sender},
    },
    time::Instant,
};

use crate::{HISTORY_PATH, MUSIC_PATH};

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

fn copy_file_non_blocking<S: AsRef<Path>, T: AsRef<Path>>(source: S, target: T) -> JoinHandle<()> {
    let source: PathBuf = PathBuf::new().join(source);
    let target: PathBuf = PathBuf::new().join(target);
    std::thread::spawn(move || {
        log::info!("Copy started {} {}", source.display(), target.display());
        let _ = std::process::Command::new("cp")
            .arg(source.clone())
            .arg(target.clone())
            .status();
        log::info!("Copy done {} {}", source.display(), target.display());
    })
}

fn update_history<T: AsRef<Path>>(station: &str, start: DateTime<Utc>, file_name: T) {
    let file_name: PathBuf = PathBuf::new().join(file_name);
    let station = station.to_owned();
    tokio::spawn(async move {
        sardio_core::save_history(&*HISTORY_PATH, &*MUSIC_PATH, &file_name, &station, start).await;
    });
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SplitWeight {
    dir: String,
    odds: f32,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub enum SourceMethod {
    #[default]
    Direct,
    Split(Vec<String>),
    SplitWght(Vec<SplitWeight>),
}

fn default_background() -> String {
    "#555555".to_string()
}

fn default_border() -> String {
    "#dc3545".to_string()
}

fn default_hover() -> String {
    "#dc3545".to_string()
}

fn default_text() -> String {
    "white".to_string()
}

fn default_should_loop() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_should_loop")]
    pub should_loop: bool,
    #[serde(default = "default_background")]
    pub background: String,
    #[serde(default = "default_border")]
    pub border: String,
    #[serde(default = "default_hover")]
    pub border_hover: String,
    #[serde(default = "default_hover")]
    pub text_colour: String,
    #[serde(default = "SourceMethod::default")]
    pub source_method: SourceMethod,
    #[serde(default)]
    pub hide_playing: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            should_loop: true,
            background: default_background(),
            border: default_border(),
            border_hover: default_hover(),
            text_colour: default_text(),
            source_method: Default::default(),
            hide_playing: false,
        }
    }
}

pub struct StationInfo {
    path: PathBuf,
    data_tx: Sender<Bytes>,
    backlog: Arc<StdMutex<Backlog>>,
    playing_now: Arc<Mutex<String>>,
    config: Config,
}

impl StationInfo {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .and_then(|i| i.to_str())
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
        self.path.join("image.webp")
    }

    pub async fn purge_bad_songs(&self) {
        for song in std::fs::read_dir(&self.path)
            .unwrap()
            .filter_map(|i| i.ok())
        {
            if let Ok(tagged_file) = Probe::open(song.path()).and_then(|i| i.read()) {
                let properties = tagged_file.properties();
                let duration = chrono::Duration::from_std(properties.duration()).unwrap();
                if duration < chrono::Duration::minutes(1) {
                    log::info!("Removing {}", song.path().display());
                    let _ = std::fs::remove_file(song.path());
                }
            }
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
}

struct CachedSong {
    original_path: Option<PathBuf>,
    cached_path: PathBuf,
    copy_job: Option<JoinHandle<()>>,
    created: Instant,
}

impl CachedSong {
    pub fn copy<P: AsRef<Path>>(&mut self, target: P) {
        let copy = copy_file_non_blocking(target.as_ref(), &self.cached_path);
        self.original_path = Some(target.as_ref().join(""));
        self.copy_job = Some(copy);
        self.created = Instant::now();
    }

    pub fn wait_copy(&mut self) {
        let copy_job = self.copy_job.take().unwrap();
        let _ = copy_job.join();
        self.original_path.take();
    }
}

pub struct StationRunner {
    path: PathBuf,
    tx: Sender<Bytes>,
    backlog: Arc<StdMutex<Backlog>>,
    playing_now: Arc<Mutex<String>>,
    songs: Vec<PathBuf>,
    cached_songs: Vec<CachedSong>,
}

impl StationRunner {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let (tx, _) = broadcast::channel::<Bytes>(1024);

        let _ = std::fs::create_dir(PathBuf::new().join(std::env::temp_dir()).join("sardio"));

        Self {
            path: PathBuf::new().join(&path),
            tx,
            backlog: Arc::default(),
            playing_now: Arc::new(Mutex::new(String::default())),
            songs: {
                let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&path)
                    .into_iter()
                    .filter_map(|i| i.ok())
                    .filter(|i| match i.path().extension() {
                        None => false,
                        Some(ext) => {
                            [OsStr::new("ogg"), OsStr::new("mp3"), OsStr::new("m4a")].contains(&ext)
                        }
                    })
                    .map(|i| {
                        let p: PathBuf = i.path().into();
                        p.components().collect()
                    })
                    .collect();

                files.sort();

                files
            },
            cached_songs: {
                let name = path
                    .as_ref()
                    .file_name()
                    .and_then(|i| i.to_str())
                    .map(|i| i.to_string())
                    .unwrap_or_default();

                (0..2)
                    .into_iter()
                    .map(|i| CachedSong {
                        original_path: None,
                        cached_path: PathBuf::new()
                            .join(std::env::temp_dir())
                            .join("sardio")
                            .join(format!("{}_{}.mp3", name, i)),
                        copy_job: None,
                        created: Instant::now(),
                    })
                    .collect()
            },
        }
    }

    pub fn config(&self) -> Config {
        std::fs::File::open(self.path.join("config.ron"))
            .map(|r| match ron::de::from_reader(r) {
                Ok(x) => x,
                Err(err) => panic!("{} {}", self.path.display(), err),
            })
            .unwrap_or_default()
    }

    pub async fn station_info(&self) -> StationInfo {
        StationInfo {
            path: self.path.clone(),
            data_tx: self.tx.clone(),
            backlog: self.backlog.clone(),
            playing_now: self.playing_now.clone(),
            config: self.config(),
        }
    }

    fn get_next_cache_file(&mut self) -> &mut CachedSong {
        self.cached_songs
            .iter_mut()
            .find(|i| i.original_path.is_none())
            .unwrap()
    }

    fn get_current_cache_file(&mut self) -> &mut CachedSong {
        self.cached_songs
            .iter_mut()
            .filter(|i| i.original_path.is_some())
            .min_by(|a, b| a.created.cmp(&b.created))
            .unwrap()
    }

    pub async fn run(&mut self) {
        let mut buffer = vec![0u8; 4096];
        let mut boot_loop = true;
        let mut rng = fastrand::Rng::new();

        let next_song = self.pick_song(&mut rng);
        let cached = self.get_next_cache_file();
        cached.copy(next_song);

        let name = &self.station_info().await.name();

        loop {
            {
                let next_song = self.pick_song(&mut rng);
                let cached = self.get_next_cache_file();
                cached.copy(next_song);
            }
            let (original_path, cached_path) = {
                let song = self.get_current_cache_file();
                let original_path = song.original_path.as_ref().unwrap().clone();
                let cached_path = song.cached_path.clone();
                song.wait_copy();
                (original_path, cached_path)
            };
            {
                let mut now = self.playing_now.lock().await;
                *now = original_path
                    .file_name()
                    .and_then(|i| i.to_str())
                    .map(|i| i.to_string())
                    .unwrap_or_default();
            }
            let tagged_file = Probe::open(&cached_path)
                .expect("Failed to open file")
                .read()
                .expect("Failed to read Ogg metadata");

            let properties = tagged_file.properties();
            let duration = chrono::Duration::from_std(properties.duration()).unwrap();

            let max_song_time = chrono::Duration::minutes(rng.i64(3..6))
                + chrono::Duration::seconds(rng.i64(0..60));

            let config = self.config();

            log::info!("loaded {:?}", config);
            let loops = if !config.should_loop || duration > max_song_time {
                0i32
            } else {
                (max_song_time.num_milliseconds() as f32 / duration.num_milliseconds() as f32)
                    .floor()
                    .max(1.) as i32
            };
            log::info!("Looping {} {}", original_path.display(), loops);

            let start_offset = if boot_loop {
                boot_loop = false;
                chrono::Duration::milliseconds(rng.i64(0..max_song_time.num_milliseconds()))
            } else {
                chrono::Duration::zero()
            };

            update_history(name, Utc::now(), &original_path);

            let matches_stream = tagged_file.file_type() == FileType::Mpeg
                && properties.audio_bitrate() == Some(64)
                && properties.channels() == Some(1)
                && properties.sample_rate() == Some(44100);
            let codec_args: &[&str] = if matches_stream {
                &["-c:a", "copy"]
            } else {
                &[
                    "-c:a",
                    "libmp3lame",
                    "-b:a",
                    "64k",
                    "-ac",
                    "1",
                    "-ar",
                    "44100",
                ]
            };

            let mut ffmpeg = Command::new("ffmpeg")
                .arg("-re")
                .arg("-stream_loop")
                .arg(loops.to_string())
                .arg("-ss")
                .arg(start_offset.num_seconds().to_string())
                .arg("-i")
                .arg(&cached_path)
                .args(["-map", "0:a:0"])
                .args(codec_args)
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
                        log::info!("{} is done playing", original_path.display());
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

    pub fn pick_song(&self, rng: &mut fastrand::Rng) -> PathBuf {
        match self.config().source_method {
            SourceMethod::Direct => rng.choice(&self.songs).unwrap().clone(),
            SourceMethod::Split(dir_splits) => {
                let dir = rng.choice(dir_splits).unwrap();
                let dir_os_str = OsStr::new(&dir);

                let possible: Vec<_> = self
                    .songs
                    .iter()
                    .filter(|i| i.components().any(|j| j.as_os_str() == dir_os_str))
                    .collect();

                match rng.choice(&possible).cloned() {
                    Some(i) => i.clone(),
                    None => rng.choice(&self.songs).unwrap().clone(),
                }
            }
            SourceMethod::SplitWght(split_weights) => {
                use weighted_rand::builder::*;

                let options: Vec<_> = split_weights.iter().map(|i| &i.dir).collect();
                let odds: Vec<_> = split_weights.iter().map(|i| i.odds).collect();

                let builder = WalkerTableBuilder::new(&odds);
                let wa_table = builder.build();

                let dir = options[wa_table.next()];

                let dir_os_str = OsStr::new(&dir);

                let possible: Vec<_> = self
                    .songs
                    .iter()
                    .filter(|i| i.components().any(|j| j.as_os_str() == dir_os_str))
                    .collect();

                match rng.choice(&possible).cloned() {
                    Some(i) => i.clone(),
                    None => rng.choice(&self.songs).unwrap().clone(),
                }
            }
        }
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
