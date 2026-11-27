#![forbid(unsafe_code)]

#[cfg(target_arch = "wasm32")]
mod host_bridge;
#[cfg(target_arch = "wasm32")]
mod theme;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("fogbreak-field only runs on wasm32-unknown-unknown");
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web_app::start();
}

#[cfg(target_arch = "wasm32")]
mod web_app {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::mpsc::{Receiver, Sender};

    use base64::Engine as _;
    use eframe::egui;
    use fieldcraft_engine::Session;
    use fieldcraft_ui_egui::files::{Kind, PrefsStore, Services};
    use fieldcraft_ui_egui::{ControlRequest, ModelApp, control};
    use js_sys::Function;
    use serde_json::{Value, json};
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;

    use crate::host_bridge::{
        ActionKind, ActionQueue, HostContext, build_ready_detail, dispatch_host_event, encode_b64, export_slug,
        install_host_rpc, mime_for_format, reject_err, resolve_ok,
    };
    use crate::theme::{self, ThemeMode};

    const CANVAS_ID: &str = "fogbreak_canvas";
    const DEPTH_BITS: u8 = 24;
    const PREFS_STORAGE_KEY: &str = "fogbreak:field:prefs";

    struct PendingControlReply {
        rx: Receiver<Value>,
        resolve: Function,
        reject: Function,
    }

    struct FogbreakFieldOverlay {
        app: ModelApp,
        queue: ActionQueue,
        control_tx: Sender<ControlRequest>,
        pending_replies: Vec<PendingControlReply>,
        host_ctx: HostContext,
        applied_style: Option<(ThemeMode, bool)>,
        last_ui_dark: bool,
        ready_emitted: bool,
        last_dirty: bool,
    }

    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    fn js_err(val: wasm_bindgen::JsValue) -> String {
        val.as_string().unwrap_or_else(|| format!("{val:?}"))
    }

    fn browser_services() -> Services {
        let prefs = PrefsStore {
            load: Box::new(|| local_storage()?.get_item(PREFS_STORAGE_KEY).ok().flatten().map(String::into_bytes)),
            save: Box::new(|bytes| {
                let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
                local_storage()
                    .ok_or_else(|| "browser storage is unavailable".to_owned())?
                    .set_item(PREFS_STORAGE_KEY, text)
                    .map_err(js_err)
            }),
        };
        Services {
            prefs: Some(prefs),
            ..Default::default()
        }
    }

    fn sanitize_filename(raw: Option<&str>, fallback: &str) -> String {
        let tail = raw.unwrap_or(fallback).rsplit(['/', '\\']).next().unwrap_or(fallback);
        let clean: String = tail
            .chars()
            .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
            .take(160)
            .collect();
        if clean.is_empty() || clean == "." || clean == ".." {
            fallback.to_owned()
        } else {
            clean
        }
    }

    impl FogbreakFieldOverlay {
        fn new(cc: &eframe::CreationContext<'_>) -> Self {
            fieldcraft_io::recovery::set_clock(|| js_sys::Date::now().max(0.0) as u64);

            let mode = theme::initial_theme_mode();
            theme::sync_dom_theme(mode);

            let mut app = ModelApp::new(Session::default()).with_services(browser_services());
            app.ui.dark = mode.is_dark();

            if let Some(rs) = &cc.wgpu_render_state {
                app.set_wgpu(rs, DEPTH_BITS, rs.msaa_samples, &cc.egui_ctx);
            }

            let (control_tx, control_rx) = std::sync::mpsc::channel();
            app = app.with_control(control_rx);

            if let Some(sample) = sample_in_query(&query()) {
                app.open_sample(&sample);
            }

            let queue: ActionQueue = Rc::new(RefCell::new(VecDeque::new()));
            install_host_rpc(Rc::clone(&queue), cc.egui_ctx.clone());

            let dirty = app.session.is_dirty();
            let ui_dark = app.ui.dark;
            let host_ctx = HostContext {
                app_title: "Fogbreak Field".to_owned(),
                theme: mode,
                dirty,
            };

            Self {
                app,
                queue,
                control_tx,
                pending_replies: Vec::new(),
                host_ctx,
                applied_style: None,
                last_ui_dark: ui_dark,
                ready_emitted: false,
                last_dirty: dirty,
            }
        }

        fn send_control(&mut self, method: &str, params: Value, resolve: Function, reject: Function, ctx: &egui::Context) {
            let (tx, rx) = std::sync::mpsc::channel();
            let req = ControlRequest::new(method.to_owned(), params, tx);
            if self.control_tx.send(req).is_err() {
                reject_err(&reject, "control channel closed");
                return;
            }
            self.pending_replies.push(PendingControlReply { rx, resolve, reject });
            ctx.request_repaint();
        }

        fn drain_actions(&mut self, ctx: &egui::Context) {
            let actions: Vec<_> = self.queue.borrow_mut().drain(..).collect();
            for action in actions {
                match action.kind {
                    ActionKind::ExecuteCommand { cmd, params } => {
                        self.send_control(
                            "engine.execute",
                            json!({ "command": cmd, "params": params }),
                            action.resolve,
                            action.reject,
                            ctx,
                        );
                    }
                    ActionKind::ExecuteRaw { method, params } => match method.as_str() {
                        "doc.inspect" | "session.inspect" => {
                            self.handle_doc_inspect(&action.resolve, &action.reject);
                        }
                        "theme.set" => {
                            let mode_str = params
                                .get("theme")
                                .or_else(|| params.get("mode"))
                                .and_then(Value::as_str)
                                .unwrap_or("dark");
                            match ThemeMode::from_str_opt(mode_str) {
                                Some(mode) => {
                                    self.apply_theme_mode(ctx, mode);
                                    resolve_ok(&action.resolve, &json!({ "theme": mode.as_str() }));
                                }
                                None => reject_err(&action.reject, "theme must be 'light' or 'dark'"),
                            }
                        }
                        "io.import" => {
                            if let Some(b64) = params.get("base64").or_else(|| params.get("data")).and_then(Value::as_str) {
                                let clean = b64.split(',').next_back().unwrap_or(b64).trim();
                                match base64::engine::general_purpose::STANDARD.decode(clean) {
                                    Ok(bytes) => {
                                        let name = params.get("name").and_then(Value::as_str).map(str::to_owned);
                                        self.handle_open(bytes, name, &action.resolve, &action.reject);
                                    }
                                    Err(e) => reject_err(&action.reject, &format!("invalid base64: {e}")),
                                }
                            } else {
                                reject_err(&action.reject, "missing `data` or `base64`");
                            }
                        }
                        "io.export" => {
                            let format = params
                                .get("format")
                                .and_then(Value::as_str)
                                .unwrap_or("fieldcraft")
                                .trim_start_matches('.')
                                .to_ascii_lowercase();
                            self.handle_export(&format, &params, &action.resolve, &action.reject);
                        }
                        "screenshot" => {
                            let view = params.get("view").and_then(Value::as_str);
                            self.handle_screenshot(ctx, view, &action.resolve, &action.reject);
                        }
                        _ => {
                            self.send_control(&method, params, action.resolve, action.reject, ctx);
                        }
                    },
                    ActionKind::OpenDocument { bytes, name } => {
                        self.handle_open(bytes, name, &action.resolve, &action.reject);
                    }
                    ActionKind::ExportDocument { format, params } => {
                        self.handle_export(&format, &params, &action.resolve, &action.reject);
                    }
                    ActionKind::Screenshot { view } => {
                        self.handle_screenshot(ctx, view.as_deref(), &action.resolve, &action.reject);
                    }
                    ActionKind::SetTheme { mode } => {
                        self.apply_theme_mode(ctx, mode);
                        resolve_ok(&action.resolve, &json!({ "theme": mode.as_str() }));
                    }
                    ActionKind::InspectDoc => {
                        self.handle_doc_inspect(&action.resolve, &action.reject);
                    }
                    ActionKind::InspectUi => {
                        self.send_control("ui.inspect", json!({}), action.resolve, action.reject, ctx);
                    }
                }
            }
        }

        fn apply_theme_mode(&mut self, ctx: &egui::Context, mode: ThemeMode) {
            self.host_ctx.theme = mode;
            self.applied_style = None;
            theme::sync_dom_theme(mode);
            theme::apply_theme(ctx, mode, &mut self.app.ui.dark);
            self.last_ui_dark = self.app.ui.dark;
            dispatch_host_event("theme", &json!({ "theme": mode.as_str() }));
            ctx.request_repaint();
        }

        fn handle_doc_inspect(&mut self, resolve: &Function, reject: &Function) {
            match self.app.session.execute("model.info", &json!({})) {
                Ok(info) => resolve_ok(
                    resolve,
                    &json!({
                        "ok": true,
                        "path": self.app.session.path,
                        "dirty": self.app.session.is_dirty(),
                        "nodes": self.app.session.model.nodes().len(),
                        "elements": self.app.session.model.elements().len(),
                        "info": info,
                    }),
                ),
                Err(e) => reject_err(reject, &e.to_string()),
            }
        }

        fn handle_screenshot(&mut self, ctx: &egui::Context, view: Option<&str>, resolve: &Function, reject: &Function) {
            if matches!(view, Some("3d" | "viewport")) {
                match self.app.render_png(1280, 800, 1.0) {
                    Ok((bytes, renderer)) => {
                        let b64 = encode_b64(&bytes);
                        resolve_ok(
                            resolve,
                            &json!({
                                "ok": true,
                                "width": 1280,
                                "height": 800,
                                "mimeType": "image/png",
                                "byteLength": bytes.len(),
                                "renderer": format!("{renderer:?}"),
                                "data": b64,
                                "pngBase64": b64,
                            }),
                        );
                    }
                    Err(e) => reject_err(reject, &e),
                }
                return;
            }

            let vpath = "/fogbreak/capture.png";
            let (tx, _rx) = std::sync::mpsc::channel();
            let req = ControlRequest::new(
                "ui.capture",
                json!({ "path": vpath, "width": 1280, "height": 800, "scale": 1 }),
                tx,
            );
            if let control::Outcome::Done(reply) = control::handle(&mut self.app, ctx, &req)
                && reply.get("ok").and_then(Value::as_bool) == Some(true)
                && let Ok(bytes) = fieldcraft_io::fs::read_capped(Path::new(vpath), 64 << 20)
            {
                let _ = fieldcraft_io::fs::remove(Path::new(vpath));
                let _ = fieldcraft_io::fs::take_writes();
                let b64 = encode_b64(&bytes);
                let width = reply.pointer("/result/width").cloned().unwrap_or_else(|| json!(1280));
                let height = reply.pointer("/result/height").cloned().unwrap_or_else(|| json!(800));
                resolve_ok(
                    resolve,
                    &json!({
                        "ok": true,
                        "width": width,
                        "height": height,
                        "mimeType": "image/png",
                        "byteLength": bytes.len(),
                        "data": b64,
                        "pngBase64": b64,
                    }),
                );
                return;
            }

            match self.app.render_png(1280, 800, 1.0) {
                Ok((bytes, renderer)) => {
                    let b64 = encode_b64(&bytes);
                    resolve_ok(
                        resolve,
                        &json!({
                            "ok": true,
                            "width": 1280,
                            "height": 800,
                            "mimeType": "image/png",
                            "byteLength": bytes.len(),
                            "renderer": format!("{renderer:?}"),
                            "data": b64,
                            "pngBase64": b64,
                        }),
                    );
                }
                Err(e) => reject_err(reject, &e),
            }
        }

        fn handle_open(&mut self, bytes: Vec<u8>, name: Option<String>, resolve: &Function, reject: &Function) {
            let safe = sanitize_filename(name.as_deref(), "model.fieldcraft");
            let ext = safe.rsplit('.').next().unwrap_or("fieldcraft").to_ascii_lowercase();
            let vpath = format!("/fogbreak/import/{safe}");

            if ext == "json" {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(script) => match self.app.run_script(&script) {
                        Ok(()) => {
                            self.app.prompt = None;
                            let detail = json!({
                                "ok": true,
                                "name": safe,
                                "nodes": self.app.session.model.nodes().len(),
                                "elements": self.app.session.model.elements().len(),
                            });
                            dispatch_host_event("open", &detail);
                            resolve_ok(resolve, &detail);
                        }
                        Err(e) => reject_err(reject, &e),
                    },
                    Err(e) => reject_err(reject, &format!("invalid JSON script: {e}")),
                }
                return;
            }

            if let Err(e) = fieldcraft_io::fs::insert(Path::new(&vpath), bytes) {
                reject_err(reject, &e.to_string());
                return;
            }

            let res = match Kind::of(&safe) {
                Some(Kind::Model) => self.app.run("file.open", json!({ "path": vpath })),
                Some(Kind::Design) => self.app.run("geometry.open", json!({ "path": vpath, "model": "new" })),
                Some(Kind::Mesh) => self.app.run("geometry.import.mesh", json!({ "path": vpath })),
                Some(Kind::Orphan) => self.app.run("mesh.import.orphan", json!({ "path": vpath })),
                None if ext == "inp" => self.app.run("deck.read", json!({ "format": "abaqus", "path": vpath })),
                None => self.app.run("file.open", json!({ "path": vpath })),
            };

            match res {
                Ok(result) => {
                    self.app.prompt = None;
                    let detail = json!({
                        "ok": true,
                        "name": safe,
                        "path": self.app.session.path,
                        "nodes": self.app.session.model.nodes().len(),
                        "elements": self.app.session.model.elements().len(),
                        "result": result,
                    });
                    dispatch_host_event("open", &detail);
                    resolve_ok(resolve, &detail);
                }
                Err(e) => reject_err(reject, &e),
            }
        }

        fn handle_export(&mut self, format: &str, params: &Value, resolve: &Function, reject: &Function) {
            if format == "png" {
                match self.app.render_png(1280, 800, 1.0) {
                    Ok((bytes, _)) => {
                        let base = self
                            .app
                            .session
                            .path
                            .as_deref()
                            .and_then(|p| p.rsplit(['/', '\\']).next())
                            .unwrap_or("model");
                        let filename = format!("{}.png", export_slug(base));
                        let out = json!({
                            "filename": filename,
                            "mimeType": "image/png",
                            "byteLength": bytes.len(),
                            "data": encode_b64(&bytes),
                        });
                        dispatch_host_event("save", &json!({ "filename": filename, "byteLength": bytes.len() }));
                        resolve_ok(resolve, &out);
                    }
                    Err(e) => reject_err(reject, &e),
                }
                return;
            }

            let raw_name = self
                .app
                .session
                .path
                .as_deref()
                .and_then(|p| p.rsplit(['/', '\\']).next())
                .and_then(|n| n.split('.').next())
                .unwrap_or("model");
            let stem = export_slug(raw_name);

            let (filename, ext, cmd, cmd_params) = match format {
                "fieldcraft" => {
                    let fname = format!("{stem}.fieldcraft");
                    let vpath = format!("/fogbreak/export/{fname}");
                    (fname, "fieldcraft", "file.save_as", json!({ "path": vpath }))
                }
                "abaqus" | "inp" => {
                    let fname = format!("{stem}.inp");
                    let vpath = format!("/fogbreak/export/{fname}");
                    (fname, "inp", "deck.write", json!({ "format": "abaqus", "path": vpath }))
                }
                "nastran" | "bdf" => {
                    let fname = format!("{stem}.bdf");
                    let vpath = format!("/fogbreak/export/{fname}");
                    (fname, "bdf", "deck.write", json!({ "format": "nastran", "path": vpath }))
                }
                "lsdyna" | "k" => {
                    let fname = format!("{stem}.k");
                    let vpath = format!("/fogbreak/export/{fname}");
                    (fname, "k", "deck.write", json!({ "format": "lsdyna", "path": vpath }))
                }
                "stl" => {
                    let fname = format!("{stem}.stl");
                    let vpath = format!("/fogbreak/export/{fname}");
                    (fname, "stl", "mesh.export.stl", json!({ "path": vpath }))
                }
                "vtu" => {
                    let fname = format!("{stem}.vtu");
                    let vpath = format!("/fogbreak/export/{fname}");
                    (fname, "vtu", "results.export.vtu", json!({ "path": vpath }))
                }
                "csv" => {
                    let fname = format!("{stem}.csv");
                    let vpath = format!("/fogbreak/export/{fname}");
                    let what = params.get("what").and_then(Value::as_str).unwrap_or("reactions");
                    (fname, "csv", "results.export.csv", json!({ "what": what, "path": vpath }))
                }
                other => {
                    reject_err(reject, &format!("unsupported export format `{other}`"));
                    return;
                }
            };

            let vpath = format!("/fogbreak/export/{filename}");
            match self.app.session.execute(cmd, &cmd_params) {
                Ok(_) => match fieldcraft_io::fs::read_capped(Path::new(&vpath), 256 << 20) {
                    Ok(bytes) => {
                        let _ = fieldcraft_io::fs::remove(Path::new(&vpath));
                        let _ = fieldcraft_io::fs::take_writes();
                        let out = json!({
                            "filename": filename,
                            "mimeType": mime_for_format(ext),
                            "byteLength": bytes.len(),
                            "data": encode_b64(&bytes),
                        });
                        dispatch_host_event("save", &json!({ "filename": filename, "byteLength": bytes.len() }));
                        resolve_ok(resolve, &out);
                    }
                    Err(e) => reject_err(reject, &e.to_string()),
                },
                Err(e) => reject_err(reject, &e.to_string()),
            }
        }

        fn flush_control_replies(&mut self) {
            let mut remaining = Vec::with_capacity(self.pending_replies.len());
            for pending in self.pending_replies.drain(..) {
                match pending.rx.try_recv() {
                    Ok(reply) => {
                        if reply.get("ok").and_then(Value::as_bool) == Some(true) {
                            let result = reply.get("result").cloned().unwrap_or(Value::Null);
                            resolve_ok(&pending.resolve, &result);
                        } else {
                            let msg = reply
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("control request failed");
                            reject_err(&pending.reject, msg);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => remaining.push(pending),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        reject_err(&pending.reject, "control reply channel disconnected");
                    }
                }
            }
            self.pending_replies = remaining;
        }
    }

    impl eframe::App for FogbreakFieldOverlay {
        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            self.app.raw_input_hook(ctx, raw);
        }

        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.drain_actions(ctx);
            self.app.logic(ctx);
            self.flush_control_replies();

            if self.app.ui.dark != self.last_ui_dark {
                self.last_ui_dark = self.app.ui.dark;
                let mode = if self.app.ui.dark { ThemeMode::Dark } else { ThemeMode::Light };
                self.host_ctx.theme = mode;
                self.applied_style = None;
                theme::sync_dom_theme(mode);
                dispatch_host_event("theme", &json!({ "theme": mode.as_str() }));
            }

            let style_key = (self.host_ctx.theme, self.app.ui.dark);
            if self.applied_style != Some(style_key) {
                theme::apply_theme(ctx, self.host_ctx.theme, &mut self.app.ui.dark);
                self.last_ui_dark = self.app.ui.dark;
                self.applied_style = Some((self.host_ctx.theme, self.app.ui.dark));
            }

            let dirty = self.app.session.is_dirty();
            if dirty != self.last_dirty {
                self.last_dirty = dirty;
                self.host_ctx.dirty = dirty;
                dispatch_host_event("dirty", &json!({ "dirty": dirty }));
            }

            if !self.ready_emitted {
                self.ready_emitted = true;
                dispatch_host_event("ready", &build_ready_detail(&self.host_ctx));
            }
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.app.ui(ui);
        }
    }

    pub fn start() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        capture_app_shortcuts();
        wasm_bindgen_futures::spawn_local(async {
            let Some(document) = web_sys::window().and_then(|w| w.document()) else {
                log::error!("no document");
                return;
            };
            let result = match document
                .get_element_by_id(CANVAS_ID)
                .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            {
                Some(canvas) => run(canvas).await,
                None => Err(format!("missing <canvas id=\"{CANVAS_ID}\">")),
            };
            if let Err(message) = result {
                log::error!("Fogbreak Field failed to start: {message}");
                dispatch_host_event("failed", &json!({ "message": message }));
            }
        });
    }

    async fn run(canvas: web_sys::HtmlCanvasElement) -> Result<(), String> {
        let mut options = eframe::WebOptions {
            depth_buffer: DEPTH_BITS,
            ..Default::default()
        };
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(|cc| Ok(Box::new(FogbreakFieldOverlay::new(cc)))),
            )
            .await
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    fn capture_app_shortcuts() {
        let Some(window) = web_sys::window() else { return };
        let on_key = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
            let ctrl = e.ctrl_key() || e.meta_key();
            let k = e.key().to_ascii_lowercase();
            let taken = (ctrl && ["s", "o", "d", "a", "z", "y", "e"].contains(&k.as_str())) || k == "f5";
            if taken {
                e.prevent_default();
            }
        });
        let options = web_sys::AddEventListenerOptions::new();
        options.set_capture(true);
        if let Err(e) = window.add_event_listener_with_callback_and_add_event_listener_options("keydown", on_key.as_ref().unchecked_ref(), &options) {
            log::warn!("couldn't capture shortcuts: {e:?}");
        }
        on_key.forget();
    }

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    fn sample_in_query(query: &str) -> Option<String> {
        let first = fieldcraft_engine::sample::SAMPLES.first().map_or("", |s| s.name);
        query.trim_start_matches('?').split('&').find_map(|kv| {
            let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
            (key == "sample").then(|| if value.is_empty() { first } else { value }.to_string())
        })
    }
}
