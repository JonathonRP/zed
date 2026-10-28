use super::{ClientContext, ForegroundWork, ForegroundWorkItem, session_thread};
use acp_thread::AcpThread;
mod service;

use agent_client_protocol::{
    Agent, Error,
    mcp_server::{McpRequestContext, McpServer},
    schema::v2 as acp,
};
use futures::{
    FutureExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AsyncApp, Entity, EntityId};
use service::ReadTextFile;
use std::sync::{Arc, Mutex};

pub(super) use service::registration_guard;

#[derive(Clone, Default)]
pub(super) struct SessionBinding(Arc<Mutex<Option<(acp::SessionId, EntityId)>>>);

impl SessionBinding {
    pub(super) fn bind(&self, thread: &Entity<AcpThread>, cx: &App) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((thread.read(cx).session_id().clone(), thread.entity_id()));
    }
}

pub(super) fn server(
    dispatch: mpsc::UnboundedSender<ForegroundWork>,
) -> (McpServer<Agent>, SessionBinding) {
    let binding = SessionBinding::default();
    (
        service::server({
            let binding = binding.clone();
            move |arguments, context| {
                let binding = binding.clone();
                let dispatch = dispatch.clone();
                async move {
                    let (result, received) = oneshot::channel();
                    let work: ForegroundWork = Box::new(ReadWork {
                        binding,
                        arguments,
                        context,
                        result,
                    });
                    if let Err(error) = dispatch.unbounded_send(work) {
                        error.into_inner().reject();
                    }
                    received.await.map_err(|_| {
                        Error::internal_error().data("Zed file reader stopped before cleanup")
                    })?
                }
                .boxed()
            }
        }),
        binding,
    )
}

struct ReadWork {
    binding: SessionBinding,
    arguments: ReadTextFile,
    context: McpRequestContext<Agent>,
    result: oneshot::Sender<Result<String, Error>>,
}

impl ForegroundWorkItem for ReadWork {
    fn run(self: Box<Self>, cx: &mut AsyncApp, context: &ClientContext) {
        let Self {
            binding,
            arguments,
            context: request,
            result,
        } = *self;
        let read = (|| {
            if request.operation_cancellation().is_cancelled() {
                return Err(Error::request_cancelled());
            }
            let (session_id, owner) = binding
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
                .ok_or_else(|| Error::internal_error().data("Zed session is not initialized"))?;
            let thread = session_thread(context, &session_id)?;
            if thread.entity_id() != owner {
                return Err(Error::internal_error().data("Zed session owner was replaced"));
            }
            thread
                .update(cx, |thread, cx| {
                    if thread.is_observation_closed() {
                        return Err(Error::request_cancelled());
                    }
                    if !thread
                        .project()
                        .read(cx)
                        .path_style(cx)
                        .is_absolute(&arguments.path)
                    {
                        return Err(Error::invalid_params().data("path must be absolute"));
                    }
                    Ok(thread.read_text_file(
                        arguments.path,
                        arguments.line,
                        arguments.limit,
                        false,
                        cx,
                    ))
                })
                .map_err(Error::from)?
        })();
        cx.spawn(async move |_cx| {
            let output = match read {
                Ok(read) => {
                    let cancellation = request.operation_cancellation().cancelled();
                    futures::pin_mut!(read, cancellation);
                    match futures::future::select(read, cancellation).await {
                        futures::future::Either::Left((result, _)) => result,
                        futures::future::Either::Right(_) => Err(Error::request_cancelled()),
                    }
                }
                Err(error) => Err(error),
            };
            // A cancellation response is not complete until the foreground read task is dropped.
            if result.send(output).is_err() {
                log::debug!("Native MCP file reader lost its result observer");
            }
        })
        .detach();
    }

    fn reject(self: Box<Self>) {
        if self
            .result
            .send(Err(
                Error::internal_error().data("ACP foreground dispatch queue closed")
            ))
            .is_err()
        {
            log::debug!("Native MCP file reader closed without an observer");
        }
    }
}
