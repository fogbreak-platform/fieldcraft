use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use base64::Engine as _;
use eframe::egui;
use fieldcraft_engine::{CommandSpec, commands, find_command};
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;

use crate::theme::ThemeMode;

pub const ENGINE_ID: &str = "fogbreak-field";
pub const HOST_RPC_KEY: &str = "fogbreakFieldHost";

pub type ActionQueue = Rc<RefCell<VecDeque<QueuedAction>>>;

pub struct HostContext {
    pub app_title: String,
    pub theme: ThemeMode,
    pub dirty: bool,
}

impl Default for HostContext {
    fn default() -> Self {
        Self {
            app_title: "Fogbreak Field".to_owned(),
            theme: ThemeMode::Dark,
            dirty: false,
        }
    }
}

pub struct QueuedAction {
    pub resolve: Function,
    pub reject: Function,
    pub kind: ActionKind,
}

pub enum ActionKind {
    ExecuteCommand { cmd: String, params: Value },
    ExecuteRaw { method: String, params: Value },
    OpenDocument { bytes: Vec<u8>, name: Option<String> },
    ExportDocument { format: String, params: Value },
    Screenshot { view: Option<String> },
    SetTheme { mode: ThemeMode },
    InspectDoc,
    InspectUi,
}

fn json_to_js(value: &Value) -> JsValue {
    match js_sys::JSON::parse(&value.to_string()) {
        Ok(parsed) => parsed,
        Err(_) => JsValue::NULL,
    }
}

fn js_to_json(val: &JsValue) -> Value {
    if val.is_null() || val.is_undefined() {
        return json!({});
    }
    match js_sys::JSON::stringify(val) {
        Ok(js_str) => {
            if let Some(s) = js_str.as_string() {
                serde_json::from_str(&s).unwrap_or_else(|_| json!({}))
            } else {
                json!({})
            }
        }
        Err(_) => json!({}),
    }
}

fn command_to_json(spec: &CommandSpec) -> Value {
    let mut v = spec.listing();
    if let Some(obj) = v.as_object_mut() {
        obj.entry("name".to_owned()).or_insert_with(|| Value::String(spec.id.to_owned()));
        obj.entry("title".to_owned()).or_insert_with(|| Value::String(spec.label.to_owned()));
        obj.entry("parameters".to_owned()).or_insert_with(|| Value::String(spec.params.to_owned()));
    }
    v
}

pub fn dispatch_host_event(suffix: &str, detail: &Value) {
    let Some(win) = web_sys::window() else { return };
    let event_name = format!("{ENGINE_ID}:{suffix}");
    let init = web_sys::CustomEventInit::new();
    init.set_detail(&json_to_js(detail));
    if let Ok(event) = web_sys::CustomEvent::new_with_event_init_dict(&event_name, &init) {
        let target: &web_sys::EventTarget = win.as_ref();
        let _ = target.dispatch_event(&event);
    }
}

pub fn resolve_ok(resolve: &Function, value: &Value) {
    let _ = resolve.call1(&JsValue::NULL, &json_to_js(value));
}

pub fn reject_err(reject: &Function, message: &str) {
    let err = js_sys::Error::new(message);
    let _ = reject.call1(&JsValue::NULL, &err);
}

fn enqueue_promise(queue: &ActionQueue, ctx: &egui::Context, kind: ActionKind) -> Promise {
    let queue = Rc::clone(queue);
    let ctx = ctx.clone();
    Promise::new(&mut |resolve, reject| {
        queue.borrow_mut().push_back(QueuedAction { resolve, reject, kind });
        ctx.request_repaint();
    })
}

fn decode_open_payload(payload: &JsValue) -> Result<(Vec<u8>, Option<String>), String> {
    if let Some(text) = payload.as_string() {
        return Ok((text.into_bytes(), None));
    }
    if payload.is_instance_of::<Uint8Array>() {
        let arr = Uint8Array::new(payload);
        return Ok((arr.to_vec(), None));
    }
    if payload.is_object() && !payload.is_null() {
        let name = Reflect::get(payload, &JsValue::from_str("name"))
            .ok()
            .and_then(|v| v.as_string())
            .or_else(|| Reflect::get(payload, &JsValue::from_str("filename")).ok().and_then(|v| v.as_string()));

        for key in ["bytes", "data", "buffer"] {
            if let Ok(val) = Reflect::get(payload, &JsValue::from_str(key)) {
                if val.is_instance_of::<Uint8Array>() {
                    return Ok((Uint8Array::new(&val).to_vec(), name));
                }
                if let Some(b64) = val.as_string() {
                    let clean = b64.split(',').next_back().unwrap_or(&b64).trim();
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(clean)
                        .map_err(|e| format!("invalid base64 payload: {e}"))?;
                    return Ok((decoded, name));
                }
            }
        }
        if let Ok(val) = Reflect::get(payload, &JsValue::from_str("base64"))
            && let Some(b64) = val.as_string()
        {
            let clean = b64.split(',').next_back().unwrap_or(&b64).trim();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(clean)
                .map_err(|e| format!("invalid base64 payload: {e}"))?;
            return Ok((decoded, name));
        }
        for key in ["text", "content"] {
            if let Ok(val) = Reflect::get(payload, &JsValue::from_str(key))
                && let Some(text) = val.as_string()
            {
                return Ok((text.into_bytes(), name));
            }
        }
    }
    Err("expected string, Uint8Array, or { data | bytes | base64 | text, name? }".to_owned())
}

fn export_formats_json() -> Value {
    json!({
        "import": ["fieldcraft", "sc", "solidcraft", "solvecraft", "step", "stp", "iges", "igs", "3mf", "stl", "obj", "ply", "gltf", "glb", "msh", "inp", "json"],
        "export": ["fieldcraft", "abaqus", "inp", "nastran", "bdf", "lsdyna", "k", "stl", "vtu", "csv", "png"],
    })
}

fn all_commands_json() -> Value {
    let items: Vec<Value> = commands().iter().map(command_to_json).collect();
    Value::Array(items)
}

fn describe_command_json(id: &str) -> Result<Value, String> {
    find_command(id)
        .map(command_to_json)
        .ok_or_else(|| format!("unknown command `{id}`"))
}

pub fn install_host_rpc(queue: ActionQueue, ctx: egui::Context) {
    let Some(win) = web_sys::window() else { return };
    let host_obj = Object::new();

    // Sync listCommands / commands (complete, uncapped catalog)
    let list_fn = Closure::<dyn Fn() -> JsValue>::new(|| json_to_js(&all_commands_json()));
    let _ = Reflect::set(&host_obj, &JsValue::from_str("commands"), list_fn.as_ref());
    let _ = Reflect::set(&host_obj, &JsValue::from_str("listCommands"), list_fn.as_ref());
    list_fn.forget();

    // Sync describeCommand(id)
    let describe_fn = Closure::<dyn Fn(String) -> JsValue>::new(|id: String| match describe_command_json(&id) {
        Ok(v) => json_to_js(&v),
        Err(_) => JsValue::NULL,
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("describeCommand"), describe_fn.as_ref());
    describe_fn.forget();

    // Async execute / executeCommand
    let q_exec = Rc::clone(&queue);
    let c_exec = ctx.clone();
    let exec_fn = Closure::<dyn Fn(String, JsValue) -> Promise>::new(move |cmd: String, params: JsValue| {
        enqueue_promise(
            &q_exec,
            &c_exec,
            ActionKind::ExecuteCommand {
                cmd,
                params: js_to_json(&params),
            },
        )
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("execute"), exec_fn.as_ref());
    let _ = Reflect::set(&host_obj, &JsValue::from_str("executeCommand"), exec_fn.as_ref());
    exec_fn.forget();

    // Async request(method, params)
    let q_req = Rc::clone(&queue);
    let c_req = ctx.clone();
    let req_fn = Closure::<dyn Fn(String, JsValue) -> Promise>::new(move |method: String, params: JsValue| {
        let p = js_to_json(&params);
        match method.as_str() {
            "commands" | "command.list" | "engine.commands" => Promise::resolve(&json_to_js(&all_commands_json())),
            "command.describe" => {
                let id = p.get("id").and_then(Value::as_str).unwrap_or_default();
                match describe_command_json(id) {
                    Ok(v) => Promise::resolve(&json_to_js(&v)),
                    Err(e) => Promise::reject(&js_sys::Error::new(&e)),
                }
            }
            "io.formats" => Promise::resolve(&json_to_js(&export_formats_json())),
            "handoff" | "emitHandoff" => {
                dispatch_host_event("handoff", &p);
                Promise::resolve(&json_to_js(&json!({"ok": true, "emitted": true})))
            }
            _ => enqueue_promise(
                &q_req,
                &c_req,
                ActionKind::ExecuteRaw { method, params: p },
            ),
        }
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("request"), req_fn.as_ref());
    req_fn.forget();

    // Async docInspect / inspect
    let q_doc = Rc::clone(&queue);
    let c_doc = ctx.clone();
    let doc_fn = Closure::<dyn Fn() -> Promise>::new(move || enqueue_promise(&q_doc, &c_doc, ActionKind::InspectDoc));
    let _ = Reflect::set(&host_obj, &JsValue::from_str("docInspect"), doc_fn.as_ref());
    let _ = Reflect::set(&host_obj, &JsValue::from_str("inspect"), doc_fn.as_ref());
    doc_fn.forget();

    // Async uiInspect
    let q_ui = Rc::clone(&queue);
    let c_ui = ctx.clone();
    let ui_fn = Closure::<dyn Fn() -> Promise>::new(move || enqueue_promise(&q_ui, &c_ui, ActionKind::InspectUi));
    let _ = Reflect::set(&host_obj, &JsValue::from_str("uiInspect"), ui_fn.as_ref());
    ui_fn.forget();

    // Async screenshot
    let q_shot = Rc::clone(&queue);
    let c_shot = ctx.clone();
    let shot_fn = Closure::<dyn Fn(JsValue) -> Promise>::new(move |params: JsValue| {
        let p = js_to_json(&params);
        let view = p.get("view").and_then(Value::as_str).map(str::to_owned);
        enqueue_promise(&q_shot, &c_shot, ActionKind::Screenshot { view })
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("screenshot"), shot_fn.as_ref());
    shot_fn.forget();

    // Async open
    let q_open = Rc::clone(&queue);
    let c_open = ctx.clone();
    let open_fn = Closure::<dyn Fn(JsValue) -> Promise>::new(move |payload: JsValue| match decode_open_payload(&payload) {
        Ok((bytes, name)) => enqueue_promise(&q_open, &c_open, ActionKind::OpenDocument { bytes, name }),
        Err(e) => Promise::reject(&js_sys::Error::new(&e)),
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("open"), open_fn.as_ref());
    open_fn.forget();

    // emitHandoff
    let handoff_fn = Closure::<dyn Fn(JsValue) -> Promise>::new(|payload: JsValue| {
        let detail = js_to_json(&payload);
        dispatch_host_event("handoff", &detail);
        Promise::resolve(&json_to_js(&json!({"ok": true, "emitted": true})))
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("emitHandoff"), handoff_fn.as_ref());
    handoff_fn.forget();

    // setTheme
    let q_theme = Rc::clone(&queue);
    let c_theme = ctx.clone();
    let theme_fn = Closure::<dyn Fn(String) -> Promise>::new(move |mode_str: String| {
        match ThemeMode::from_str_opt(&mode_str) {
            Some(mode) => enqueue_promise(&q_theme, &c_theme, ActionKind::SetTheme { mode }),
            None => Promise::reject(&js_sys::Error::new("theme must be 'light' or 'dark'")),
        }
    });
    let _ = Reflect::set(&host_obj, &JsValue::from_str("setTheme"), theme_fn.as_ref());
    theme_fn.forget();

    // io: { formats, import, export }
    let io_obj = Object::new();
    let formats_fn = Closure::<dyn Fn() -> JsValue>::new(|| json_to_js(&export_formats_json()));
    let _ = Reflect::set(&io_obj, &JsValue::from_str("formats"), formats_fn.as_ref());
    formats_fn.forget();

    let q_imp = Rc::clone(&queue);
    let c_imp = ctx.clone();
    let import_fn = Closure::<dyn Fn(JsValue) -> Promise>::new(move |payload: JsValue| match decode_open_payload(&payload) {
        Ok((bytes, name)) => enqueue_promise(&q_imp, &c_imp, ActionKind::OpenDocument { bytes, name }),
        Err(e) => Promise::reject(&js_sys::Error::new(&e)),
    });
    let _ = Reflect::set(&io_obj, &JsValue::from_str("import"), import_fn.as_ref());
    import_fn.forget();

    let q_exp = queue;
    let c_exp = ctx;
    let export_fn = Closure::<dyn Fn(JsValue) -> Promise>::new(move |opts: JsValue| {
        let params = js_to_json(&opts);
        let format = params
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("fieldcraft")
            .trim_start_matches('.')
            .to_ascii_lowercase();
        enqueue_promise(&q_exp, &c_exp, ActionKind::ExportDocument { format, params })
    });
    let _ = Reflect::set(&io_obj, &JsValue::from_str("export"), export_fn.as_ref());
    export_fn.forget();

    let _ = Reflect::set(&host_obj, &JsValue::from_str("io"), &io_obj);
    let _ = Reflect::set(&win, &JsValue::from_str(HOST_RPC_KEY), &host_obj);
}

pub fn build_ready_detail(ctx: &HostContext) -> Value {
    json!({
        "engine": ENGINE_ID,
        "rpcKey": HOST_RPC_KEY,
        "title": ctx.app_title,
        "theme": ctx.theme.as_str(),
        "dirty": ctx.dirty,
    })
}

pub fn mime_for_format(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "csv" => "text/csv",
        "vtu" => "application/xml",
        "inp" | "bdf" | "k" | "abaqus" | "nastran" | "lsdyna" => "text/plain",
        "stl" | "fieldcraft" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

pub fn export_slug(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "model".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub fn encode_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[allow(dead_code)]
fn _touch_array(_: &Array) {}
