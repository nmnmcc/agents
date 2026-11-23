use super::request::{GetAttachment, ListMail, MailRef, Request, SearchMail, SendMail, SetRead};
use anyhow::{Context, Result};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Write,
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

#[derive(Deserialize, Serialize)]
enum Response {
    Ok(Value),
    Error(String),
}

#[derive(Clone)]
struct Server {
    socket: PathBuf,
}

#[tool_router]
impl Server {
    #[tool(
        description = "Queue email to agents by ID with To, Cc and Bcc, or reply in-thread using an inbox mail reference."
    )]
    async fn send_mail(&self, Parameters(request): Parameters<SendMail>) -> CallToolResult {
        self.call(Request::Send(request)).await
    }

    #[tool(
        description = "List this agent's inbox, newest first. Optional unread filter and UID cursor."
    )]
    async fn list_mail(&self, Parameters(request): Parameters<ListMail>) -> CallToolResult {
        self.call(Request::List(request)).await
    }

    #[tool(
        description = "Full-text search of this agent's mail headers and bodies, including already-read mail."
    )]
    async fn search_mail(&self, Parameters(request): Parameters<SearchMail>) -> CallToolResult {
        self.call(Request::Search(request)).await
    }

    #[tool(
        description = "Read the full decoded message and attachment metadata without marking it read."
    )]
    async fn get_mail(&self, Parameters(request): Parameters<MailRef>) -> CallToolResult {
        self.call(Request::Get(request)).await
    }

    #[tool(description = "Read messages in the same thread that are visible to this agent.")]
    async fn get_thread(&self, Parameters(request): Parameters<MailRef>) -> CallToolResult {
        self.call(Request::Thread(request)).await
    }

    #[tool(description = "Get one attachment by its zero-based index; content is base64.")]
    async fn get_attachment(
        &self,
        Parameters(request): Parameters<GetAttachment>,
    ) -> CallToolResult {
        self.call(Request::Attachment(request)).await
    }

    #[tool(description = "Explicitly mark a message read or unread in this agent's inbox.")]
    async fn set_mail_read(&self, Parameters(request): Parameters<SetRead>) -> CallToolResult {
        self.call(Request::SetRead(request)).await
    }
}

impl Server {
    async fn call(&self, request: Request) -> CallToolResult {
        let socket = self.socket.clone();
        let answer = tokio::task::spawn_blocking(move || forward(&socket, &request)).await;
        match answer {
            Ok(Ok(Response::Ok(value))) => {
                CallToolResult::success(vec![ContentBlock::text(value.to_string())])
            }
            Ok(Ok(Response::Error(error))) => {
                CallToolResult::error(vec![ContentBlock::text(error)])
            }
            _ => CallToolResult::error(vec![ContentBlock::text(
                "The active mail run is unavailable.",
            )]),
        }
    }
}

#[tool_handler]
impl ServerHandler for Server {}

pub fn serve(socket: PathBuf) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let service = Server { socket }.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
            Ok(())
        })
}

fn forward(path: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(path)?;
    serde_json::to_writer(&mut stream, request)?;
    stream.shutdown(Shutdown::Write)?;
    Ok(serde_json::from_reader(stream)?)
}

pub fn with_bridge<T>(
    socket: &Path,
    mut handle: impl FnMut(Request) -> Result<Value> + Send,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let listener = UnixListener::bind(socket).context("bind private mail MCP socket")?;
    let stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        let bridge = scope.spawn(|| -> Result<()> {
            loop {
                let (mut stream, _) = listener.accept().context("accept mail MCP request")?;
                if stopped.load(Ordering::Acquire) {
                    return Ok(());
                }
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                let response = serde_json::from_reader(&stream)
                    .context("invalid mail request")
                    .and_then(&mut handle);
                let _ = serde_json::to_writer(
                    &mut stream,
                    &match response {
                        Ok(value) => Response::Ok(value),
                        Err(error) => Response::Error(format!("{error:#}")),
                    },
                );
                let _ = stream.flush();
            }
        });
        let result = run();
        stopped.store(true, Ordering::Release);
        // Wake the blocking accept so the bridge sees the stop request.
        let woken = UnixStream::connect(socket);
        bridge
            .join()
            .map_err(|_| anyhow::anyhow!("mail MCP bridge stopped unexpectedly"))??;
        drop(woken);
        result
    })
}
