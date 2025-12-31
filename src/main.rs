#![windows_subsystem = "windows"]

use eframe::egui;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Clone)]
struct Requirements {
    python: bool,
    ytdlp: bool,
    ffmpeg: bool,
}

impl Requirements {
    fn check() -> Self {
        let python = check_command("python", &["--version"]);
        let ytdlp = check_command("python", &["-c", "import yt_dlp"]) || check_command("yt-dlp", &["--version"]);
        let ffmpeg = check_command("ffmpeg", &["-version"]);

        Self { python, ytdlp, ffmpeg }
    }

    fn all_ok(&self) -> bool {
        self.python && self.ytdlp && self.ffmpeg
    }
}

fn check_command(cmd: &str, args: &[&str]) -> bool {
    let mut command = Command::new(cmd);
    command.args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    command.status().map(|s| s.success()).unwrap_or(false)
}

fn parse_progress(line: &str) -> Option<f32> {
    if !line.contains("[download]") {
        return None;
    }

    // Try to find percentage - look for number followed by %
    for word in line.split_whitespace() {
        if word.ends_with('%') {
            let num_str = word.trim_end_matches('%');
            if let Ok(pct) = num_str.parse::<f32>() {
                return Some(pct);
            }
        }
    }

    // Also try parsing "frag X/Y" format for fragmented downloads
    if line.contains("frag") {
        if let Some(frag_part) = line.split("frag").nth(1) {
            let parts: Vec<&str> = frag_part.trim().trim_matches(')').split('/').collect();
            if parts.len() == 2 {
                if let (Ok(current), Ok(total)) = (
                    parts[0].trim().parse::<f32>(),
                    parts[1].trim().parse::<f32>()
                ) {
                    if total > 0.0 {
                        return Some((current / total) * 100.0);
                    }
                }
            }
        }
    }

    None
}

fn load_icon() -> Option<egui::IconData> {
    let icon_bytes = include_bytes!("../icon.png");
    let img = image::load_from_memory(icon_bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    let icon = load_icon();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([500.0, 420.0])
        .with_resizable(false);

    if let Some(icon_data) = icon {
        viewport = viewport.with_icon(Arc::new(icon_data));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "YouTube MP3 Downloader",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

enum Phase {
    Idle,
    Fetching,
    Downloading,
    Extracting,
    Converting,
    Done,
}

struct App {
    url: String,
    status: Arc<Mutex<String>>,
    progress: Arc<Mutex<f32>>,
    phase: Arc<Mutex<Phase>>,
    is_downloading: Arc<Mutex<bool>>,
    download_folder: Arc<Mutex<String>>,
    requirements: Requirements,
    show_setup: bool,
}

impl App {
    fn new() -> Self {
        let default_folder = if let Some(home) = dirs_next::home_dir() {
            home.join("Downloads").join("YouTube").to_string_lossy().to_string()
        } else {
            String::from("Downloads/YouTube")
        };

        let requirements = Requirements::check();
        let show_setup = !requirements.all_ok();

        Self {
            url: String::new(),
            status: Arc::new(Mutex::new(String::new())),
            progress: Arc::new(Mutex::new(0.0)),
            phase: Arc::new(Mutex::new(Phase::Idle)),
            is_downloading: Arc::new(Mutex::new(false)),
            download_folder: Arc::new(Mutex::new(default_folder)),
            requirements,
            show_setup,
        }
    }

    fn recheck_requirements(&mut self) {
        self.requirements = Requirements::check();
        if self.requirements.all_ok() {
            self.show_setup = false;
        }
    }

    fn choose_folder(&self) {
        let download_folder = Arc::clone(&self.download_folder);
        let current = download_folder.lock().unwrap().clone();

        thread::spawn(move || {
            if let Some(folder) = rfd::FileDialog::new()
                .set_directory(&current)
                .pick_folder()
            {
                *download_folder.lock().unwrap() = folder.to_string_lossy().to_string();
            }
        });
    }

    fn download(&mut self) {
        if *self.is_downloading.lock().unwrap() {
            return;
        }

        let url = self.url.trim().to_string();
        if url.is_empty() {
            *self.status.lock().unwrap() = "Please enter a YouTube URL".to_string();
            return;
        }

        *self.is_downloading.lock().unwrap() = true;
        *self.status.lock().unwrap() = "Starting download...".to_string();
        *self.progress.lock().unwrap() = 0.0;
        *self.phase.lock().unwrap() = Phase::Fetching;

        let status = Arc::clone(&self.status);
        let progress = Arc::clone(&self.progress);
        let phase = Arc::clone(&self.phase);
        let is_downloading = Arc::clone(&self.is_downloading);
        let download_folder = self.download_folder.lock().unwrap().clone();

        thread::spawn(move || {
            // Create download folder
            let _ = std::fs::create_dir_all(&download_folder);

            *status.lock().unwrap() = "Starting...".to_string();

            let output_template = format!("{}/%(title)s.%(ext)s", download_folder);

            // Use python to call yt-dlp with merged stdout/stderr
            let python_script = format!(
                r#"
import subprocess
import sys
proc = subprocess.Popen(
    ['yt-dlp', '--newline', '-f', 'ba/b', '-x', '--audio-format', 'mp3', '--audio-quality', '0',
     '-o', r'{}', r'{}'],
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    text=True
)
for line in proc.stdout:
    print(line, end='', flush=True)
sys.exit(proc.wait())
"#,
                output_template.replace('\\', "\\\\").replace('\'', "\\'"),
                url.replace('\\', "\\\\").replace('\'', "\\'")
            );

            let mut cmd = Command::new("python");
            cmd.args(["-c", &python_script])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            #[cfg(windows)]
            cmd.creation_flags(CREATE_NO_WINDOW);

            let result = cmd.spawn();

            match result {
                Ok(mut child) => {
                    if let Some(stdout) = child.stdout.take() {
                        let reader = BufReader::new(stdout);
                        let mut in_extract = false;
                        for line in reader.lines().map_while(Result::ok) {
                            if let Some(pct) = parse_progress(&line) {
                                if !in_extract {
                                    *phase.lock().unwrap() = Phase::Downloading;
                                    *progress.lock().unwrap() = pct / 100.0;
                                    *status.lock().unwrap() = format!("Downloading... {:.0}%", pct);
                                }
                            } else if line.contains("[ExtractAudio]") {
                                in_extract = true;
                                *phase.lock().unwrap() = Phase::Converting;
                                *status.lock().unwrap() = "Converting to MP3...".to_string();
                                *progress.lock().unwrap() = 0.5;
                            } else if line.contains("[youtube]") && !in_extract {
                                *phase.lock().unwrap() = Phase::Fetching;
                                *status.lock().unwrap() = "Fetching video info...".to_string();
                            } else if (line.contains("[hlsnative]") || line.contains("[info]")) && !in_extract {
                                *status.lock().unwrap() = "Preparing download...".to_string();
                            } else if line.contains("Deleting original") {
                                *status.lock().unwrap() = "Finishing up...".to_string();
                                *progress.lock().unwrap() = 0.9;
                            }
                        }
                    }

                    match child.wait() {
                        Ok(exit_status) => {
                            if exit_status.success() {
                                *phase.lock().unwrap() = Phase::Done;
                                *progress.lock().unwrap() = 1.0;
                                *status.lock().unwrap() = format!("Download complete!\nSaved to: {}", download_folder);
                            } else {
                                *phase.lock().unwrap() = Phase::Idle;
                                *status.lock().unwrap() = "Download failed!".to_string();
                            }
                        }
                        Err(e) => {
                            *phase.lock().unwrap() = Phase::Idle;
                            *status.lock().unwrap() = format!("Error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        *status.lock().unwrap() = "Python not found!".to_string();
                    } else {
                        *status.lock().unwrap() = format!("Error: {}", e);
                    }
                }
            }

            *is_downloading.lock().unwrap() = false;
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_downloading = *self.is_downloading.lock().unwrap();

        // Request repaint while downloading
        if is_downloading {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.heading("YouTube MP3 Downloader");
                ui.add_space(20.0);
            });

            // Show setup instructions if requirements are missing
            if self.show_setup {
                self.show_setup_ui(ui);
                return;
            }

            ui.add_space(10.0);
            ui.label("Paste YouTube URL:");
            ui.add_space(5.0);

            let text_edit = egui::TextEdit::singleline(&mut self.url)
                .hint_text("https://www.youtube.com/watch?v=...")
                .desired_width(f32::INFINITY);
            ui.add(text_edit);

            ui.add_space(20.0);

            ui.vertical_centered(|ui| {
                let button = if is_downloading {
                    egui::Button::new("Downloading...")
                } else {
                    egui::Button::new("Download MP3")
                };

                if ui.add_sized([150.0, 40.0], button).clicked() && !is_downloading {
                    self.download();
                }
            });

            ui.add_space(20.0);

            // Progress bars based on phase
            let progress_val = *self.progress.lock().unwrap();
            let phase = &*self.phase.lock().unwrap();

            if is_downloading {
                match phase {
                    Phase::Fetching => {
                        ui.horizontal(|ui| {
                            ui.label("Fetching:");
                            ui.spinner();
                        });
                    }
                    Phase::Downloading => {
                        ui.horizontal(|ui| {
                            ui.label("Download:");
                            ui.add(egui::ProgressBar::new(progress_val).show_percentage());
                        });
                    }
                    Phase::Extracting | Phase::Converting => {
                        // Show download as complete
                        ui.horizontal(|ui| {
                            ui.label("Download:");
                            ui.add(egui::ProgressBar::new(1.0).show_percentage());
                        });
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label("Convert: ");
                            ui.add(egui::ProgressBar::new(progress_val).show_percentage());
                        });
                    }
                    Phase::Done => {
                        ui.horizontal(|ui| {
                            ui.label("Download:");
                            ui.add(egui::ProgressBar::new(1.0).show_percentage());
                        });
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label("Convert: ");
                            ui.add(egui::ProgressBar::new(1.0).show_percentage());
                        });
                    }
                    Phase::Idle => {}
                }
                ui.add_space(10.0);
            }

            // Status area
            let status = self.status.lock().unwrap().clone();
            if !status.is_empty() {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        ui.label(&status);
                    });
                });
            }

            ui.add_space(20.0);

            // Save location with change button
            ui.horizontal(|ui| {
                let folder = self.download_folder.lock().unwrap().clone();
                ui.label("Save to:");
                ui.add_space(5.0);

                // Truncate path if too long
                let display_path = if folder.len() > 40 {
                    format!("...{}", &folder[folder.len()-37..])
                } else {
                    folder
                };
                ui.label(display_path);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Change").clicked() && !is_downloading {
                        self.choose_folder();
                    }
                });
            });
        });
    }
}

impl App {
    fn show_setup_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);

        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.vertical(|ui| {
                ui.colored_label(egui::Color32::YELLOW, "Setup Required");
                ui.add_space(10.0);
                ui.label("This app needs the following to work:");
                ui.add_space(10.0);

                // Python status
                ui.horizontal(|ui| {
                    if self.requirements.python {
                        ui.colored_label(egui::Color32::GREEN, "OK");
                    } else {
                        ui.colored_label(egui::Color32::RED, "X ");
                    }
                    ui.label("Python");
                });

                // yt-dlp status
                ui.horizontal(|ui| {
                    if self.requirements.ytdlp {
                        ui.colored_label(egui::Color32::GREEN, "OK");
                    } else {
                        ui.colored_label(egui::Color32::RED, "X ");
                    }
                    ui.label("yt-dlp");
                });

                // FFmpeg status
                ui.horizontal(|ui| {
                    if self.requirements.ffmpeg {
                        ui.colored_label(egui::Color32::GREEN, "OK");
                    } else {
                        ui.colored_label(egui::Color32::RED, "X ");
                    }
                    ui.label("FFmpeg");
                });
            });
        });

        ui.add_space(15.0);

        // Installation instructions
        egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
            if !self.requirements.python {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong("Install Python:");
                    ui.label("1. Go to python.org/downloads");
                    ui.label("2. Download and run installer");
                    ui.label("3. CHECK 'Add Python to PATH'");
                    ui.label("4. Click Install Now");
                });
                ui.add_space(10.0);
            }

            if !self.requirements.ytdlp {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong("Install yt-dlp:");
                    ui.label("Open Command Prompt and run:");
                    ui.code("pip install yt-dlp");
                });
                ui.add_space(10.0);
            }

            if !self.requirements.ffmpeg {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong("Install FFmpeg:");
                    ui.label("Option 1: Open Command Prompt and run:");
                    ui.code("winget install FFmpeg");
                    ui.add_space(5.0);
                    ui.label("Option 2: Manual install from ffmpeg.org");
                    ui.label("(extract to C:\\ffmpeg and add bin to PATH)");
                });
            }
        });

        ui.add_space(15.0);

        ui.vertical_centered(|ui| {
            if ui.button("I've installed them - Check Again").clicked() {
                self.recheck_requirements();
            }
            ui.add_space(5.0);
            if ui.small_button("Skip (use anyway)").clicked() {
                self.show_setup = false;
            }
        });
    }
}
