//! TermPlayer — پلیر موزیک ترمینالی
//! اجرا: cargo run --release -- "C:\Users\You\Music"

use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufReader, Stdout};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lofty::file::TaggedFileExt;
use lofty::prelude::*;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use walkdir::WalkDir;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// فرمت‌هایی که rodio به‌صورت پیش‌فرض پخش می‌کند
const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "oga"];

const HELP_TEXT: &str = "↑/↓ select | enter play | space pause | ←/→ seek ±5s | n next | p prev | +/- volume | m mute | r repeat | s shuffle | x stop | q quit";

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

// ---------------- مدل داده ----------------

struct Track {
    path: PathBuf,
    title: String,
    artist: String,
    duration: Option<Duration>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlayStatus {
    Stopped,
    Playing,
    Paused,
}

struct App {
    tracks: Vec<Track>,
    list_state: ListState,
    selected: usize,        // آیتم انتخاب‌شده در لیست
    current: Option<usize>, // ترک در حال پخش
    status: PlayStatus,
    volume: f32,
    muted: bool,
    repeat: RepeatMode,
    shuffle: bool,
    quit: bool,
    message: Option<(String, Instant)>, // پیام موقت در نوار پایین
    started_at: Instant,                // برای جلوگیری از شرط رقابتی بعد از append
    rng: u64,
}

/// جریان صوتی باید زنده نگه داشته شود؛ اگر drop شود پخش قطع می‌شود
struct Audio {
    _stream: OutputStream,
    _handle: OutputStreamHandle,
    sink: Sink,
}

fn init_audio() -> Result<Audio> {
    let (stream, handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&handle)?;
    sink.set_volume(0.8);
    Ok(Audio {
        _stream: stream,
        _handle: handle,
        sink,
    })
}

impl App {
    fn new(tracks: Vec<Track>) -> Self {
        // بذر برای شافل (xorshift ساده)
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xDEAD_BEEF);
        App {
            tracks,
            list_state: ListState::default(),
            selected: 0,
            current: None,
            status: PlayStatus::Stopped,
            volume: 0.8,
            muted: false,
            repeat: RepeatMode::Off,
            shuffle: false,
            quit: false,
            message: None,
            started_at: Instant::now(),
            rng: seed | 1,
        }
    }

    fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn rand_u64(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn move_selection(&mut self, delta: isize) {
        if self.tracks.is_empty() {
            return;
        }
        let len = self.tracks.len() as isize;
        let target = self.selected as isize + delta;
        self.selected = target.clamp(0, len - 1) as usize;
    }

    fn play_index(&mut self, audio: &Audio, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        let path = self.tracks[index].path.clone();
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                self.set_message(format!("Cannot open {}: {e}", path.display()));
                return;
            }
        };
        match Decoder::new(BufReader::new(file)) {
            Ok(source) => {
                audio.sink.clear();
                audio.sink.append(source);
                audio.sink.set_volume(if self.muted { 0.0 } else { self.volume });
                self.current = Some(index);
                self.selected = index;
                self.status = PlayStatus::Playing;
                self.started_at = Instant::now();
            }
            Err(e) => {
                self.set_message(format!("Unsupported format {}: {e}", path.display()));
            }
        }
    }

    fn toggle_pause(&mut self, audio: &Audio) {
        match self.status {
            PlayStatus::Playing => {
                audio.sink.pause();
                self.status = PlayStatus::Paused;
            }
            PlayStatus::Paused => {
                audio.sink.play();
                self.status = PlayStatus::Playing;
            }
            PlayStatus::Stopped => {
                let index = self.selected;
                self.play_index(audio, index);
            }
        }
    }

    /// ایندکس ترک بعدی را تعیین می‌کند (با درنظر گرفتن repeat و shuffle)
    fn next_index(&mut self, natural_end: bool) -> Option<usize> {
        let len = self.tracks.len();
        if len == 0 {
            return None;
        }

        // تکرار یک ترک فقط وقتی خودش تمام شده باشد
        if natural_end && self.repeat == RepeatMode::One {
            return self.current.or(Some(0));
        }

        if self.shuffle && len > 1 {
            loop {
                let candidate = (self.rand_u64() as usize) % len;
                if Some(candidate) != self.current {
                    return Some(candidate);
                }
            }
        }

        let next = match self.current {
            Some(c) => c + 1,
            None => 0,
        };
        if next < len {
            Some(next)
        } else if !natural_end || self.repeat == RepeatMode::All {
            Some(0) // با کلید دستی یا repeat=all از اول لیست
        } else {
            None // پایان لیست
        }
    }

    fn skip_next(&mut self, audio: &Audio) {
        if self.tracks.is_empty() {
            return;
        }
        if let Some(index) = self.next_index(false) {
            self.play_index(audio, index);
        }
    }

    fn skip_previous(&mut self, audio: &Audio) {
        if self.tracks.is_empty() {
            return;
        }
        // اگر بیش از ۳ ثانیه پخش شده، به ابتدای همین ترک برگرد
        if matches!(self.status, PlayStatus::Playing | PlayStatus::Paused)
            && audio.sink.get_pos() > Duration::from_secs(3)
        {
            let _ = audio.sink.try_seek(Duration::ZERO);
            return;
        }
        let index = match self.current {
            Some(c) if c > 0 => c - 1,
            Some(_) if self.repeat == RepeatMode::All => self.tracks.len() - 1,
            _ => 0,
        };
        self.play_index(audio, index);
    }

    fn stop_playback(&mut self, audio: &Audio) {
        audio.sink.clear();
        self.status = PlayStatus::Stopped;
        self.current = None;
        self.set_message("Stopped");
    }

    fn change_volume(&mut self, audio: &Audio, delta: f32) {
        self.muted = false;
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        audio.sink.set_volume(self.volume);
        self.set_message(format!(
            "Volume: {}%",
            (self.volume * 100.0).round() as u32
        ));
    }

    fn toggle_mute(&mut self, audio: &Audio) {
        self.muted = !self.muted;
        if self.muted {
            audio.sink.set_volume(0.0);
            self.set_message("Muted");
        } else {
            audio.sink.set_volume(self.volume);
            self.set_message(format!(
                "Volume: {}%",
                (self.volume * 100.0).round() as u32
            ));
        }
    }

    fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        let label = match self.repeat {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        };
        self.set_message(format!("Repeat: {label}"));
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.set_message(format!(
            "Shuffle: {}",
            if self.shuffle { "on" } else { "off" }
        ));
    }

    fn seek(&mut self, audio: &Audio, seconds: i64) {
        if !matches!(self.status, PlayStatus::Playing | PlayStatus::Paused) {
            return;
        }
        let current = audio.sink.get_pos();
        let target = if seconds >= 0 {
            current + Duration::from_secs(seconds as u64)
        } else {
            current.saturating_sub(Duration::from_secs(seconds.unsigned_abs()))
        };
        // اگر مدت ترک را می‌دانیم، از آخر رد نشویم
        let target = match self.current.and_then(|i| self.tracks[i].duration) {
            Some(total) if total > Duration::ZERO && target > total => {
                total - Duration::from_millis(250)
            }
            _ => target,
        };
        if audio.sink.try_seek(target).is_err() {
            self.set_message("Seeking is not supported for this file");
        }
    }

    /// اگر ترک تمام شده باشد، به ترک بعدی می‌رود
    fn check_track_end(&mut self, audio: &Audio) {
        if self.status != PlayStatus::Playing {
            return;
        }
        // کمی صبر می‌کنیم تا append اولیه روی ترد صدا پردازش شود
        if self.started_at.elapsed() < Duration::from_millis(400) {
            return;
        }
        if audio.sink.empty() {
            match self.next_index(true) {
                Some(index) => self.play_index(audio, index),
                None => {
                    self.status = PlayStatus::Stopped;
                    self.current = None;
                    self.set_message("End of playlist");
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, audio: &Audio) {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.tracks.len().saturating_sub(1),
            KeyCode::Enter => {
                let index = self.selected;
                self.play_index(audio, index);
            }
            KeyCode::Char(' ') => self.toggle_pause(audio),
            KeyCode::Char('n') => self.skip_next(audio),
            KeyCode::Char('p') => self.skip_previous(audio),
            KeyCode::Right => self.seek(audio, 5),
            KeyCode::Left => self.seek(audio, -5),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(audio, 0.05),
            KeyCode::Char('-') | KeyCode::Char('_') => self.change_volume(audio, -0.05),
            KeyCode::Char('m') => self.toggle_mute(audio),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('x') => self.stop_playback(audio),
            _ => {}
        }
    }
}

// ---------------- اسکن و متادیتا ----------------

fn scan_directory(root: &Path) -> Vec<Track> {
    let mut tracks: Vec<Track> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| is_audio_file(e.path()))
        .map(|e| load_track(e.path()))
        .collect();
    tracks.sort_by(|a, b| a.path.cmp(&b.path));
    tracks
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// تگ‌ها را می‌خواند؛ اگر نشد از نام فایل به‌عنوان عنوان استفاده می‌کند
fn load_track(path: &Path) -> Track {
    let fallback_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    let mut track = Track {
        path: path.to_path_buf(),
        title: fallback_title,
        artist: String::new(),
        duration: None,
    };

    let read = lofty::probe::Probe::open(path).and_then(|probe| probe.read());
    if let Ok(tagged) = read {
        track.duration = Some(tagged.properties().duration());
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(title) = tag.title() {
                if !title.trim().is_empty() {
                    track.title = title.trim().to_string();
                }
            }
            if let Some(artist) = tag.artist() {
                if !artist.trim().is_empty() {
                    track.artist = artist.trim().to_string();
                }
            }
        }
    }
    track
}

// ---------------- حلقهٔ اصلی و رابط کاربری ----------------

fn run() -> Result<()> {
    // مسیر موزیک‌ها از آرگومان خط فرمان (پیش‌فرض: پوشهٔ جاری)
    let root = PathBuf::from(env::args().nth(1).unwrap_or_else(|| ".".to_string()));

    let tracks = if root.is_file() {
        vec![load_track(&root)]
    } else if root.is_dir() {
        scan_directory(&root)
    } else {
        eprintln!("Path not found: {}", root.display());
        process::exit(1);
    };

    if tracks.is_empty() {
        eprintln!(
            "No audio files found in \"{}\" (supported: mp3, wav, flac, ogg)",
            root.display()
        );
        process::exit(1);
    }

    // دستگاه صدا قبل از ورود به TUI راه‌اندازی می‌شود تا خطاها مستقیم چاپ شوند
    let audio = init_audio()?;
    let mut app = App::new(tracks);

    // --- ورود به حالت ترمینال ---
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run_app(&mut terminal, &mut app, audio);

    // --- بازگرداندن ترمینال به حالت عادی (همیشه اجرا می‌شود) ---
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    mut audio: Audio,
) -> Result<()> {
    while !app.quit {
        // روی ویندوز رویداد Release هم می‌آید؛ فقط Press/Repeat را پردازش می‌کنیم
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Release {
                    app.handle_key(key, &audio);
                }
            }
        }

        // تشخیص پایان ترک و رفتن به بعدی
        app.check_track_end(&audio);

        terminal.draw(|frame| ui(frame, app, &audio))?;
    }
    Ok(())
}

fn ui(frame: &mut Frame, app: &mut App, audio: &Audio) {
    let [header_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(4),
    ])
    .areas(frame.area());

    app.list_state.select(Some(app.selected));

    render_header(frame, app, header_area);
    render_playlist(frame, app, list_area);
    render_footer(frame, app, audio, footer_area);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let now_playing = match app.current {
        Some(index) => {
            let track = &app.tracks[index];
            if track.artist.is_empty() {
                track.title.clone()
            } else {
                format!("{} — {}", track.title, track.artist)
            }
        }
        None => "press Enter to start".to_string(),
    };

    let (badge, badge_style) = match app.status {
        PlayStatus::Playing => (
            " PLAYING ",
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        PlayStatus::Paused => (
            " PAUSED ",
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        PlayStatus::Stopped => (
            " STOPPED ",
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
    };

    let volume_label = if app.muted {
        "muted".to_string()
    } else {
        format!("{}%", (app.volume * 100.0).round() as u32)
    };
    let repeat_label = match app.repeat {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    };

    let title_line = Line::from(vec![
        Span::styled(
            " TermPlayer",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  |  "),
        Span::styled(now_playing, Style::default().add_modifier(Modifier::BOLD)),
    ]);

    let info_line = Line::from(vec![
        Span::styled(badge, badge_style),
        Span::raw(format!("  Vol: {volume_label}")),
        Span::raw(format!("  Repeat: {repeat_label}")),
        Span::raw(format!("  Shuffle: {}", if app.shuffle { "on" } else { "off" })),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Now Playing ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Paragraph::new(vec![title_line, info_line]).block(block), area);
}

fn render_playlist(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_width = area.width.saturating_sub(2) as usize;

    let items: Vec<ListItem> = app
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let is_current = app.current == Some(index);
            let marker = if is_current { "▶ " } else { "  " };
            let number = format!("{:02}. ", index + 1);

            let full_label = if track.artist.is_empty() {
                track.title.clone()
            } else {
                format!("{} — {}", track.title, track.artist)
            };

            let duration_text = track
                .duration
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_string());

            // متن بلند را متناسب با عرض ترمینال می‌بُریم و مدت‌زمان را راست‌چین می‌کنیم
            let base = marker.chars().count()
                + number.chars().count()
                + duration_text.chars().count();
            let label = truncate(&full_label, inner_width.saturating_sub(base + 2));
            let padding_len = inner_width
                .saturating_sub(base + label.chars().count())
                .max(1);

            let mut spans = vec![
                Span::styled(
                    marker,
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(number, Style::default().fg(Color::DarkGray)),
            ];
            if is_current {
                spans.push(Span::styled(
                    label.clone(),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(label));
            }
            spans.push(Span::styled(
                format!("{}{}", " ".repeat(padding_len), duration_text),
                Style::default().fg(Color::DarkGray),
            ));

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    format!(" Playlist ({} tracks) ", app.tracks.len()),
                    Style::default().fg(Color::Cyan),
                )),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_footer(frame: &mut Frame, app: &App, audio: &Audio, area: Rect) {
    let elapsed = if matches!(app.status, PlayStatus::Playing | PlayStatus::Paused) {
        audio.sink.get_pos()
    } else {
        Duration::ZERO
    };
    let total = app.current.and_then(|index| app.tracks[index].duration);

    let elapsed_text = format_duration(elapsed);
    let total_text = total
        .map(format_duration)
        .unwrap_or_else(|| "--:--".to_string());

    let ratio = match total {
        Some(t) if t.as_secs_f64() > 0.0 => {
            (elapsed.as_secs_f64() / t.as_secs_f64()).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };

    let inner_width = area.width.saturating_sub(2) as usize;
    let bar_width = inner_width
        .saturating_sub(elapsed_text.len() + total_text.len() + 8)
        .clamp(5, 40);
    let filled = (ratio * bar_width as f64).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));

    let progress_line = Line::from(vec![
        Span::styled(
            format!(" {elapsed_text} "),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(bar, Style::default().fg(Color::Cyan)),
        Span::styled(format!(" {total_text}"), Style::default().fg(Color::Green)),
    ]);

    // پیام موقت یا راهنمای کلیدها
    let info_line = match &app.message {
        Some((message, at)) if at.elapsed() < Duration::from_secs(3) => Line::from(Span::styled(
            format!(" {message}"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        _ => Line::from(Span::styled(
            format!(" {HELP_TEXT}"),
            Style::default().fg(Color::DarkGray),
        )),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    frame.render_widget(
        Paragraph::new(vec![progress_line, info_line]).block(block),
        area,
    );
}

// ---------------- ابزارهای کمکی ----------------

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else if max_chars == 0 {
        String::new()
    } else {
        let head: String = text.chars().take(max_chars - 1).collect();
        format!("{head}…")
    }
}
