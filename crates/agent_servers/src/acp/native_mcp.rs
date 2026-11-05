use super::{AcpSession, ClientContext, ForegroundWork, ForegroundWorkItem};
use acp_thread::{AcpThread, AgentConnection};
use action_log::ActionLog;
use agent_client_protocol::{
    Agent, Error,
    mcp_server::{McpRequestContext, McpServer},
    schema::v2 as acp,
};
use collections::HashMap;
use futures::{
    FutureExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, WeakEntity};
use project::Project;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};
use util::path_list::PathList;

mod service;
pub(super) use service::registration_guard;
use service::{ReadError, ReadTextFile};

#[derive(Clone)]
enum Owner {
    Pending(WeakEntity<AcpThread>),
    Bound {
        session_id: acp::SessionId,
        thread: WeakEntity<AcpThread>,
    },
    Closed,
}

#[derive(Clone)]
struct SessionBinding {
    project: EntityId,
    owner: Arc<Mutex<Owner>>,
}

impl SessionBinding {
    fn thread(
        &self,
        sessions: &RefCell<HashMap<acp::SessionId, AcpSession>>,
        cx: &App,
    ) -> Result<(Entity<AcpThread>, bool), Error> {
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let (thread, pending) = match owner {
            Owner::Pending(thread) => (thread, true),
            Owner::Bound { session_id, thread } => {
                if !sessions
                    .borrow()
                    .get(&session_id)
                    .is_some_and(|session| session.thread.entity_id() == thread.entity_id())
                {
                    return Err(
                        Error::internal_error().data("Zed session owner was released or replaced")
                    );
                }
                (thread, false)
            }
            Owner::Closed => return Err(Error::request_cancelled()),
        };
        let thread = thread.upgrade().ok_or_else(Error::request_cancelled)?;
        if thread.read(cx).project().entity_id() != self.project
            || thread.read(cx).is_observation_closed()
        {
            return Err(Error::request_cancelled());
        }
        Ok((thread, pending))
    }
}

pub(super) struct PendingSession {
    thread: Entity<AcpThread>,
    binding: SessionBinding,
}

impl PendingSession {
    pub(super) fn action_log(&self, cx: &App) -> Entity<ActionLog> {
        self.thread.read(cx).action_log().clone()
    }

    pub(super) fn bind(&self, thread: &Entity<AcpThread>, cx: &App) -> Result<(), Error> {
        if thread.read(cx).project().entity_id() != self.binding.project {
            return Err(
                Error::internal_error().data("Native MCP registration belongs to another project")
            );
        }
        let mut owner = self
            .binding
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !matches!(&*owner, Owner::Pending(thread) if thread.entity_id() == self.thread.entity_id())
        {
            return Err(
                Error::internal_error().data("Native MCP registration is no longer pending")
            );
        }
        *owner = Owner::Bound {
            session_id: thread.read(cx).session_id().clone(),
            thread: thread.downgrade(),
        };
        Ok(())
    }
}

impl Drop for PendingSession {
    fn drop(&mut self) {
        let mut owner = self
            .binding
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if matches!(&*owner, Owner::Pending(_)) {
            *owner = Owner::Closed;
        }
    }
}

pub(super) fn pending_server(
    dispatch: mpsc::UnboundedSender<ForegroundWork>,
    connection: Rc<dyn AgentConnection>,
    project: Entity<Project>,
    work_dirs: PathList,
    cx: &mut App,
) -> (McpServer<Agent>, PendingSession) {
    let action_log = cx.new(|_| ActionLog::new(project.clone()));
    // This private reader is never registered as an ACP session. It gives setup
    // calls the established buffer reader before the agent chooses a session ID.
    let thread = cx.new(|cx| {
        AcpThread::new(
            None,
            None,
            Some(work_dirs),
            connection,
            project.clone(),
            action_log,
            acp::SessionId::new(format!("zed-mcp-setup:{}", uuid::Uuid::new_v4())),
            watch::Receiver::constant(Default::default()),
            cx,
        )
    });
    let binding = SessionBinding {
        project: project.entity_id(),
        owner: Arc::new(Mutex::new(Owner::Pending(thread.downgrade()))),
    };
    (
        service_server(dispatch, binding.clone()),
        PendingSession { thread, binding },
    )
}

pub(super) fn server(
    dispatch: mpsc::UnboundedSender<ForegroundWork>,
    thread: &Entity<AcpThread>,
    cx: &App,
) -> McpServer<Agent> {
    service_server(
        dispatch,
        SessionBinding {
            project: thread.read(cx).project().entity_id(),
            owner: Arc::new(Mutex::new(Owner::Bound {
                session_id: thread.read(cx).session_id().clone(),
                thread: thread.downgrade(),
            })),
        },
    )
}

fn service_server(
    dispatch: mpsc::UnboundedSender<ForegroundWork>,
    binding: SessionBinding,
) -> McpServer<Agent> {
    service::server(move |arguments, context| {
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
                ReadError::from(
                    Error::internal_error().data("Zed file reader stopped before cleanup"),
                )
            })?
        }
        .boxed()
    })
}

struct ReadWork {
    binding: SessionBinding,
    arguments: ReadTextFile,
    context: McpRequestContext<Agent>,
    result: oneshot::Sender<Result<String, ReadError>>,
}

#[cfg(test)]
#[derive(Default)]
struct BlockedReads(HashMap<std::path::PathBuf, BlockedRead>);

#[cfg(test)]
struct BlockedRead {
    started: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

#[cfg(test)]
impl gpui::Global for BlockedReads {}

#[cfg(test)]
pub(super) fn block_next_read(
    path: std::path::PathBuf,
    cx: &mut App,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (started, observed) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    assert!(
        cx.default_global::<BlockedReads>()
            .0
            .insert(
                path,
                BlockedRead {
                    started,
                    resume: resumed
                }
            )
            .is_none()
    );
    (observed, resume)
}

impl ForegroundWorkItem for ReadWork {
    fn run(self: Box<Self>, cx: &mut AsyncApp, context: &ClientContext) {
        let Self {
            binding,
            arguments,
            context: request,
            result,
        } = *self;
        let sessions = context.sessions.clone();
        let read = cx.update(|cx| {
            if request.operation_cancellation().is_cancelled() {
                return Err(Error::request_cancelled().into());
            }
            let (thread, pending) = binding.thread(&sessions, cx)?;
            if !thread
                .read(cx)
                .project()
                .read(cx)
                .path_style(cx)
                .is_absolute(&arguments.path)
            {
                return Err(ReadError::InvalidArguments("path must be absolute".into()));
            }
            let project = thread.read(cx).project().clone();
            let project_path = project
                .read(cx)
                .project_path_for_absolute_path(&arguments.path, cx)
                .ok_or_else(|| {
                    Error::resource_not_found(Some(arguments.path.display().to_string()))
                })?;
            let preflight = if project
                .read(cx)
                .get_open_buffer(&project_path, cx)
                .is_some()
            {
                None
            } else {
                let path = arguments.path.to_str().ok_or_else(|| {
                    ReadError::InvalidArguments("path must be valid UTF-8".into())
                })?;
                Some(project.update(cx, |project, cx| {
                    project.resolve_abs_text_file_path(path, cx)
                }))
            };
            #[cfg(test)]
            let blocked = cx
                .default_global::<BlockedReads>()
                .0
                .remove(&arguments.path);
            let read_binding = binding.clone();
            let read_sessions = sessions.clone();
            let read = cx.spawn(async move |cx| {
                // Opening a missing project path creates an empty buffer. Only an
                // already-open buffer may bypass the existing text-file preflight.
                if let Some(preflight) = preflight {
                    if preflight.await.map_err(Error::from)?.is_none() {
                        return Err(Error::resource_not_found(Some(
                            arguments.path.display().to_string(),
                        )));
                    }
                }
                let read = cx.update(|cx| {
                    let (thread, _) = read_binding.thread(&read_sessions, cx)?;
                    Ok::<_, Error>(thread.update(cx, |thread, cx| {
                        thread.read_text_file(
                            arguments.path,
                            arguments.line,
                            arguments.limit,
                            false,
                            cx,
                        )
                    }))
                })?;
                read.await
            });
            #[cfg(test)]
            let read = match blocked {
                Some(blocked) => cx.spawn(async move |_| {
                    blocked
                        .started
                        .send(())
                        .map_err(|_| Error::internal_error())?;
                    blocked.resume.await.map_err(Error::into_internal_error)?;
                    read.await
                }),
                None => read,
            };
            // Only private setup readers may be retained. Keeping a real owner
            // alive here would suppress its release callback during the read.
            Ok((pending.then_some(thread), read))
        });
        cx.spawn(async move |cx| {
            let output = match read {
                Ok((pending_reader, read)) => {
                    let cancellation = request.operation_cancellation().cancelled();
                    let output = {
                        futures::pin_mut!(read, cancellation);
                        match futures::future::select(read, cancellation).await {
                            futures::future::Either::Left((result, _)) => {
                                result.map_err(ReadError::from)
                            }
                            futures::future::Either::Right(_) => {
                                Err(Error::request_cancelled().into())
                            }
                        }
                    };
                    // A read may span setup binding, view release or owner replacement.
                    // Retaining its reader must not retain the registration's authority.
                    let authority = cx.update(|cx| binding.thread(&sessions, cx).map(|_| ()));
                    drop(pending_reader);
                    authority.map_err(ReadError::from).and(output)
                }
                Err(error) => Err(error),
            };
            if result.send(output).is_err() {
                log::debug!("Native MCP file reader lost its result observer");
            }
        })
        .detach();
    }

    fn reject(self: Box<Self>) {
        if self
            .result
            .send(Err(Error::internal_error()
                .data("ACP foreground dispatch queue closed")
                .into()))
            .is_err()
        {
            log::debug!("Native MCP file reader closed without an observer");
        }
    }
}
