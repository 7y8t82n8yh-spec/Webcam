//! Status window: QR code to scan, connection status and a live preview.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions};

use crate::app::App;
use crate::hub::Stats;

const PREVIEW_WIDTH: usize = 384;

struct Window {
    app: App,
    selected_url: usize,
    qr: Option<(usize, TextureHandle)>,
    preview: Option<TextureHandle>,
    last_preview: Instant,
    fps: (Instant, u64, f64),
}

fn qr_image(url: &str) -> ColorImage {
    let code = qrcode::QrCode::new(url.as_bytes()).expect("URL fits in a QR code");
    let width = code.width();
    let border = 2;
    let size = width + 2 * border;
    let mut image = ColorImage::new([size, size], vec![Color32::WHITE; size * size]);
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == qrcode::Color::Dark {
                image[(x + border, y + border)] = Color32::BLACK;
            }
        }
    }
    image
}

/// Downscale the camera frame for the preview (nearest neighbour; it's small).
fn preview_image(app: &App) -> ColorImage {
    let frame = app.hub.latest();
    let (w, h) = (frame.width as usize, frame.height as usize);
    let pw = PREVIEW_WIDTH;
    let ph = pw * h / w.max(1);
    let mut rgb = Vec::with_capacity(pw * ph * 3);
    for y in 0..ph {
        let row = (y * h / ph) * w;
        for x in 0..pw {
            let i = (row + x * w / pw) * 3;
            rgb.extend_from_slice(&frame.rgb[i..i + 3]);
        }
    }
    ColorImage::from_rgb([pw, ph], &rgb)
}

impl Window {
    /// Re-render the QR code when another URL is picked, and the preview every 100 ms.
    fn refresh_textures(&mut self, ctx: &egui::Context) {
        if self.qr.as_ref().is_none_or(|(i, _)| *i != self.selected_url) {
            let image = qr_image(&self.app.urls[self.selected_url]);
            let texture = ctx.load_texture("qr", image, TextureOptions::NEAREST);
            self.qr = Some((self.selected_url, texture));
        }
        if self.preview.is_none() || self.last_preview.elapsed() > Duration::from_millis(100) {
            let image = preview_image(&self.app);
            match &mut self.preview {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => self.preview = Some(ctx.load_texture("preview", image, TextureOptions::LINEAR)),
            }
            self.last_preview = Instant::now();
        }
    }

    fn update_fps(&mut self, stats: &Stats) {
        if self.fps.0.elapsed() >= Duration::from_secs(1) {
            let dt = self.fps.0.elapsed().as_secs_f64();
            self.fps = (Instant::now(), stats.frames_in, (stats.frames_in - self.fps.1) as f64 / dt);
        }
    }

    fn columns(&mut self, ui: &mut egui::Ui, stats: &Stats) {
        ui.vertical(|ui| self.qr_column(ui));
        ui.add_space(16.0);
        ui.vertical(|ui| self.preview_column(ui, stats));
    }

    /// The QR code to scan and the URL it holds.
    fn qr_column(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Scan with the iPhone camera").strong());
        if let Some((_, qr)) = &self.qr {
            let image = egui::Image::new(qr).fit_to_exact_size(egui::vec2(220.0, 220.0));
            ui.add(image);
        }
        egui::ComboBox::from_id_salt("url")
            .width(300.0)
            .selected_text(&self.app.urls[self.selected_url])
            .show_ui(ui, |ui| url_choices(ui, &self.app.urls, &mut self.selected_url));
        if ui.button("Copy URL").clicked() {
            ui.ctx().copy_text(self.app.urls[self.selected_url].clone());
        }
        let hint = RichText::new(
            "Same Wi-Fi as this PC. The first time, Safari warns about\n\
             the certificate: tap Show Details > visit this website.",
        )
        .weak();
        ui.label(hint);
    }

    /// The live preview and what the phone and virtual camera are doing.
    fn preview_column(&self, ui: &mut egui::Ui, stats: &Stats) {
        ui.label(RichText::new("Preview").strong());
        if let Some(preview) = &self.preview {
            ui.image(preview);
        }
        let status = if self.app.hub.streaming() {
            let (w, h) = stats.source_size;
            format!("● Streaming from iPhone ({}) — {w}×{h}, {:.0} fps", stats.transport, self.fps.2)
        } else {
            "○ Waiting for iPhone…".to_string()
        };
        ui.label(status);
        let camera = format!(
            "Virtual camera: {}, {}×{} @ {} fps",
            self.app.camera_status.lock().unwrap(),
            self.app.hub.width,
            self.app.hub.height,
            self.app.settings.fps
        );
        ui.label(RichText::new(camera).weak());
    }
}

fn url_choices(ui: &mut egui::Ui, urls: &[String], selected: &mut usize) {
    for (i, url) in urls.iter().enumerate() {
        ui.selectable_value(selected, i, url);
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh_textures(&ctx);
        let stats = self.app.hub.stats();
        self.update_fps(&stats);

        let panel = |ui: &mut egui::Ui| {
            ui.horizontal_top(|ui| self.columns(ui, &stats));
        };
        egui::CentralPanel::default().show(ui, panel);
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// Run the window until it's closed (on this, the main, thread).
pub fn run(app: App) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 400.0]).with_title("iPhone Webcam"),
        ..Default::default()
    };
    let window = Window {
        app,
        selected_url: 0,
        qr: None,
        preview: None,
        last_preview: Instant::now() - Duration::from_secs(1),
        fps: (Instant::now(), 0, 0.0),
    };
    eframe::run_native("iPhone Webcam", options, Box::new(|_| Ok(Box::new(window))))
        .map_err(|e| anyhow::anyhow!("window: {e}"))
}
