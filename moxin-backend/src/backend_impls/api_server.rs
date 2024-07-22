use std::{collections::HashMap, net::SocketAddr};

use anyhow::anyhow;
use futures_util::StreamExt;
use moxin_protocol::{
    open_ai::{
        ChatResponse, ChatResponseChunkData, ChatResponseData, ChunkChoiceData, MessageData, Role,
        StopReason,
    },
    protocol::LoadModelOptions,
};
use wasmedge_sdk::{wasi::WasiModule, Module, Store, Vm};

use crate::store::download_files::DownloadedFile;

use super::BackendModel;

// From https://github.com/L-jasmine/LlamaEdge/tree/feat/support_unload_and_exit
// A repo that fork from LlamaEdge/LlamaEdge for support unload model and exit
static WASM: &[u8] = include_bytes!("../../wasm/llama-api-server.wasm");

/// Use server which is OpenAI compatible
pub struct LLamaEdgeApiServer {
    id: String,
    listen_addr: SocketAddr,
    wasm_module: Module,
    running_controller: tokio::sync::broadcast::Sender<()>,
    #[allow(dead_code)]
    model_thread: std::thread::JoinHandle<()>,
}

fn create_wasi(
    listen_addr: SocketAddr,
    file: &DownloadedFile,
    load_model: &LoadModelOptions,
) -> wasmedge_sdk::WasmEdgeResult<WasiModule> {
    // use model metadata context size
    let ctx_size = Some(format!("{}", file.context_size));

    let n_gpu_layers = match load_model.gpu_layers {
        moxin_protocol::protocol::GPULayers::Specific(n) => Some(n.to_string()),
        moxin_protocol::protocol::GPULayers::Max => None,
    };

    // Set n_batch to a fixed value of 128.
    let batch_size = Some(format!("128"));

    let mut prompt_template = load_model.prompt_template.clone();
    if prompt_template.is_none() && !file.prompt_template.is_empty() {
        prompt_template = Some(file.prompt_template.clone());
    }

    let reverse_prompt = if file.reverse_prompt.is_empty() {
        None
    } else {
        Some(file.reverse_prompt.clone())
    };

    let listen_addr = Some(format!("{listen_addr}"));

    let module_alias = file.name.as_ref();

    let mut args = vec!["llama-api-server", "-a", module_alias, "-m", module_alias];

    macro_rules! add_args {
        ($flag:expr, $value:expr) => {
            if let Some(ref value) = $value {
                args.push($flag);
                args.push(value.as_str());
            }
        };
    }

    add_args!("-c", ctx_size);
    add_args!("-g", n_gpu_layers);
    add_args!("-b", batch_size);
    add_args!("-p", prompt_template);
    add_args!("-r", reverse_prompt);
    add_args!("--socket-addr", listen_addr);

    WasiModule::create(Some(args), None, None)
}

pub fn run_wasm_by_downloaded_file(
    listen_addr: SocketAddr,
    wasm_module: Module,
    file: DownloadedFile,
    load_model: LoadModelOptions,
) {
    use wasmedge_sdk::AsInstance;

    let mut instances = HashMap::new();

    let mut wasi = create_wasi(listen_addr, &file, &load_model).unwrap();
    instances.insert(wasi.name().to_string(), wasi.as_mut());

    let mut wasi_nn = wasmedge_sdk::plugin::PluginManager::load_plugin_wasi_nn().unwrap();
    instances.insert(wasi_nn.name().unwrap(), &mut wasi_nn);

    let mut wasi_logger = wasmedge_sdk::plugin::PluginManager::create_plugin_instance(
        "wasi_logging",
        "wasi:logging/logging",
    );

    if let Ok(wasi_logger) = &mut wasi_logger {
        instances.insert(wasi_logger.name().unwrap(), wasi_logger);
    }

    let store = Store::new(None, instances).unwrap();
    let mut vm = Vm::new(store);
    vm.register_module(None, wasm_module.clone()).unwrap();

    let _ = vm.run_func(None, "_start", []);

    log::debug!("wasm exit");
}

fn stop_chunk(reason: StopReason) -> ChatResponseChunkData {
    ChatResponseChunkData {
        id: String::new(),
        choices: vec![ChunkChoiceData {
            finish_reason: Some(reason),
            index: 0,
            delta: MessageData {
                content: String::new(),
                role: Role::Assistant,
            },
            logprobs: None,
        }],
        created: 0,
        model: String::new(),
        system_fingerprint: String::new(),
        object: "chat.completion.chunk".to_string(),
    }
}

impl BackendModel for LLamaEdgeApiServer {
    fn new_or_reload(
        async_rt: &tokio::runtime::Runtime,
        old_model: Option<Self>,
        file: crate::store::download_files::DownloadedFile,
        options: moxin_protocol::protocol::LoadModelOptions,
        tx: std::sync::mpsc::Sender<anyhow::Result<moxin_protocol::protocol::LoadModelResponse>>,
    ) -> Self {
        let mut need_reload = true;
        let (wasm_module, listen_addr) = if let Some(old_model) = &old_model {
            if old_model.id == file.id.as_str() {
                need_reload = false;
            }
            (old_model.wasm_module.clone(), old_model.listen_addr)
        } else {
            (
                Module::from_bytes(None, WASM).unwrap(),
                ([0, 0, 0, 0], 8080).into(),
            )
        };

        if !need_reload {
            let _ = tx.send(Ok(moxin_protocol::protocol::LoadModelResponse::Completed(
                moxin_protocol::protocol::LoadedModelInfo {
                    file_id: file.id.to_string(),
                    model_id: file.model_id,
                    information: "".to_string(),
                },
            )));
            return old_model.unwrap();
        }

        if let Some(old_model) = old_model {
            old_model.stop(async_rt);
        }

        let wasm_module_ = wasm_module.clone();

        let file_id = file.id.to_string();

        let url = format!("http://{}/echo", listen_addr);

        let file_ = file.clone();

        let model_thread = std::thread::spawn(move || {
            run_wasm_by_downloaded_file(listen_addr, wasm_module_, file, options)
        });

        async_rt.spawn(async move {
            let mut test_server = false;
            for _i in 0..600 {
                let r = reqwest::get(&url).await;
                if let Ok(resp) = r {
                    if resp.status().is_success() {
                        test_server = true;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            if test_server {
                let _ = tx.send(Ok(moxin_protocol::protocol::LoadModelResponse::Completed(
                    moxin_protocol::protocol::LoadedModelInfo {
                        file_id: file_.id.to_string(),
                        model_id: file_.model_id,
                        information: "".to_string(),
                    },
                )));
            } else {
                let _ = tx.send(Err(anyhow!("Failed to start the model")));
            }
        });

        let running_controller = tokio::sync::broadcast::channel(1).0;

        let new_model = Self {
            id: file_id,
            wasm_module,
            listen_addr,
            running_controller,
            model_thread,
        };

        new_model
    }

    fn chat(
        &self,
        async_rt: &tokio::runtime::Runtime,
        data: moxin_protocol::open_ai::ChatRequestData,
        tx: std::sync::mpsc::Sender<anyhow::Result<ChatResponse>>,
    ) -> bool {
        let is_stream = data.stream.unwrap_or(false);
        let url = format!("http://{}/v1/chat/completions", self.listen_addr);
        let mut cancel = self.running_controller.subscribe();

        async_rt.spawn(async move {
            let request_body = serde_json::to_string(&data).unwrap();
            let resp = reqwest::Client::new()
                .post(url)
                .body(request_body)
                .send()
                .await
                .map_err(|e| anyhow!(e));

            match resp {
                Ok(resp) => {
                    if is_stream {
                        let mut stream = resp.bytes_stream();

                        while let Some(chunk) = tokio::select! {
                            chunk = stream.next() => chunk,
                            _ = cancel.recv() => None,
                        } {
                            match chunk {
                                Ok(chunk) => {
                                    if chunk.starts_with(b"data: [DONE]") {
                                        break;
                                    }
                                    let resp: Result<ChatResponseChunkData, anyhow::Error> =
                                        serde_json::from_slice(&chunk[5..]).map_err(|e| anyhow!(e));
                                    let _ = tx.send(resp.map(ChatResponse::ChatResponseChunk));
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(anyhow!(e)));
                                    return;
                                }
                            }
                        }

                        let _ = tx.send(Ok(ChatResponse::ChatResponseChunk(stop_chunk(
                            StopReason::Stop,
                        ))));
                    } else {
                        let resp: Result<ChatResponseData, anyhow::Error> =
                            resp.json().await.map_err(|e| anyhow!(e));
                        let _ = tx.send(resp.map(ChatResponse::ChatFinalResponseData));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    let _ = tx.send(Ok(ChatResponse::ChatResponseChunk(stop_chunk(
                        StopReason::Stop,
                    ))));
                }
            }
        });

        true
    }

    fn stop_chat(&self, _async_rt: &tokio::runtime::Runtime) {
        let _ = self.running_controller.send(());
    }

    fn stop(self, _async_rt: &tokio::runtime::Runtime) {
        let url = format!("http://{}/admin/exit", self.listen_addr);
        let _ = reqwest::blocking::get(url);
        let _ = self.model_thread.join();
    }
}
