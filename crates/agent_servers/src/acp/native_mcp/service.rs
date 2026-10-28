use agent_client_protocol::{
    Agent, ConnectionTo, Dispatch, DynamicHandlerGuard, Error, HandleDispatchFrom, Handled,
    NullRun, V2ConnectionTo,
    mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
    schema::v1::McpError,
};
use futures::{
    FutureExt as _,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

const NAME: &str = "zed-project-files";

pub(crate) struct RegistrationGuard {
    _registered: oneshot::Sender<()>,
    _handler: DynamicHandlerGuard<Agent>,
}

struct RegistrationBarrier(Shared<BoxFuture<'static, ()>>);

impl HandleDispatchFrom<Agent> for RegistrationBarrier {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        _connection: ConnectionTo<Agent>,
    ) -> Result<Handled<Dispatch>, Error> {
        self.0.clone().await;
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "Zed native MCP setup registration"
    }
}

pub(crate) fn registration_guard(
    connection: &V2ConnectionTo<Agent>,
) -> Result<RegistrationGuard, Error> {
    let (registered, registration) = oneshot::channel();
    // Session builders publish eagerly. Their SDK readiness barrier also installs
    // this handler before publication, so even a concurrent response must wait
    // until the caller has selected ordered result consumption and dropped the guard.
    let handler = connection.add_dynamic_handler(RegistrationBarrier(
        registration.map(|_| ()).boxed().shared(),
    ))?;
    Ok(RegistrationGuard {
        _registered: registered,
        _handler: handler,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadTextFile {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub limit: Option<u32>,
}

pub(super) fn server<Read>(read: Read) -> McpServer<Agent>
where
    Read: Fn(ReadTextFile, McpRequestContext<Agent>) -> BoxFuture<'static, Result<String, Error>>
        + Clone
        + Send
        + Sync
        + 'static,
{
    McpServer::new_service(NAME, ProjectFiles(read), NullRun)
}

struct ProjectFiles<Read>(Read);

fn outcome(error: Error) -> McpOutcome {
    let mut error_outcome = McpError::new(error.code.into(), error.message);
    if let Some(data) = error.data {
        error_outcome = error_outcome.data(data);
    }
    McpOutcome::Error(error_outcome)
}

impl<Read> McpService<Agent> for ProjectFiles<Read>
where
    Read: Fn(ReadTextFile, McpRequestContext<Agent>) -> BoxFuture<'static, Result<String, Error>>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn execute(
        &self,
        request: McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let read = self.0.clone();
        async move {
            match request.method.as_str() {
                "server/discover" => Ok(McpOutcome::Result(json!({
                    "supportedVersions": ["2026-07-28"],
                    "serverInfo": {"name": NAME, "version": "1"},
                    "capabilities": {"tools": {}}
                }))),
                "tools/list" => Ok(McpOutcome::Result(json!({
                    "resultType": "complete",
                    "ttlMs": 0,
                    "cacheScope": "private",
                    "tools": [{
                        "name": "read_text_file",
                        "description": "Read text from a file in this Zed session's project, including unsaved edits. Paths must be absolute and belong to an existing project worktree. Optional line and limit select a 1-based line range.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "line": {"type": "integer", "minimum": 1, "maximum": u32::MAX},
                                "limit": {"type": "integer", "minimum": 0, "maximum": u32::MAX}
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        },
                        "annotations": {
                            "readOnlyHint": true,
                            "destructiveHint": false,
                            "idempotentHint": true,
                            "openWorldHint": false
                        }
                    }]
                }))),
                "tools/call" => {
                    let params = request.params.unwrap_or_default();
                    if params.get("name").and_then(Value::as_str) != Some("read_text_file") {
                        return Ok(outcome(Error::invalid_params().data("Unknown Zed file tool")));
                    }
                    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                    let arguments: ReadTextFile = match serde_json::from_value(arguments) {
                        Ok(arguments) => arguments,
                        Err(error) => return Ok(outcome(Error::invalid_params().data(error.to_string()))),
                    };
                    if arguments.line == Some(0) {
                        return Ok(outcome(Error::invalid_params().data("line must be 1-based")));
                    }
                    Ok(match read(arguments, context).await {
                        Ok(text) => McpOutcome::Result(json!({
                            "resultType": "complete",
                            "content": [{"type": "text", "text": text}],
                            "isError": false
                        })),
                        Err(error) => outcome(error),
                    })
                }
                _ => Ok(outcome(Error::method_not_found())),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{
        Channel, Client, Responder, V2ConnectionTo,
        schema::{ProtocolVersion, v2 as acp},
    };
    use futures::{
        StreamExt as _,
        channel::{mpsc, oneshot},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn params(arguments: Value) -> serde_json::Map<String, Value> {
        let mut params = arguments.as_object().expect("parameters").clone();
        params.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "test/request-local": true
            }),
        );
        params
    }

    async fn request(
        connection: &V2ConnectionTo<Client>,
        server: &acp::McpServerAcpId,
        id: &str,
        method: &str,
        params: serde_json::Map<String, Value>,
    ) -> Result<Value, Error> {
        let response = connection
            .send_request(
                acp::MessageMcpRequest::new(server.clone(), id.to_owned(), method).params(params),
            )
            .block_task()
            .await?;
        serde_json::to_value(response).map_err(Error::from)
    }

    async fn yield_transport() {
        for _ in 0..8 {
            let mut yielded = false;
            futures::future::poll_fn(|cx| {
                if std::mem::replace(&mut yielded, true) {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
        }
    }

    async fn ordered_setup<Response: Send + 'static>(
        connection: &V2ConnectionTo<Agent>,
        setup: impl FnOnce() -> Result<agent_client_protocol::SentRequest<Response>, Error>,
    ) -> Result<Response, Error> {
        let registration = registration_guard(connection)?;
        let request = setup()?;
        // Let the transport run before selecting response consumption, unlike
        // the production caller, to exercise the publication/registration race.
        yield_transport().await;
        let (sender, received) = oneshot::channel();
        request.on_receiving_result(async move |result| {
            sender.send(result).map_err(|_| Error::internal_error())
        })?;
        drop(registration);
        received.await.map_err(Error::into_internal_error)?
    }

    #[test]
    fn native_mcp_sdk_registration_preserves_response_order() {
        let result = futures::executor::block_on(async {
            let applied = Arc::new(AtomicBool::new(false));
            let (noticed, mut notices) = mpsc::unbounded();
            let (finished, finish) = oneshot::channel();
            let (client_transport, agent_transport) = Channel::duplex();
            let client = Client
                .v2()
                .on_receive_notification(
                    {
                        let applied = applied.clone();
                        async move |_: acp::UpdateSessionNotification, _| {
                            assert!(
                                applied.load(Ordering::SeqCst),
                                "response must precede later updates"
                            );
                            noticed
                                .unbounded_send(())
                                .map_err(Error::into_internal_error)
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(client_transport, async move |connection| {
                    connection
                        .send_request(acp::InitializeRequest::new(
                            ProtocolVersion::V2,
                            acp::Implementation::new("client", "1"),
                        ))
                        .block_task()
                        .await?;
                    let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                    let registration = registration_guard(&connection)?;
                    let request = connection
                        .build_session(cwd)
                        .with_mcp_server(server(|_, _| {
                            futures::future::ready(Ok::<_, Error>(String::new())).boxed()
                        }))?
                        .start_session();
                    yield_transport().await;
                    let (response, received) = oneshot::channel();
                    request.on_receiving_result(async move |result| {
                        applied.store(true, Ordering::SeqCst);
                        response.send(result).map_err(|_| Error::internal_error())
                    })?;
                    drop(registration);
                    received.await.map_err(Error::into_internal_error)??;
                    notices.next().await.expect("post-response update");
                    finished.send(()).expect("agent waits for completion");
                    Ok(())
                });
            let agent = Agent
                .v2()
                .on_receive_request(
                    async |_: acp::InitializeRequest,
                           responder: Responder<acp::InitializeResponse>,
                           _| {
                        responder.respond(
                            acp::InitializeResponse::new(
                                ProtocolVersion::V2,
                                acp::Implementation::new("agent", "1"),
                            )
                            .capabilities(
                                acp::AgentCapabilities::new().session(
                                    acp::SessionCapabilities::new().mcp(
                                        acp::McpCapabilities::new()
                                            .acp(acp::McpAcpCapabilities::new()),
                                    ),
                                ),
                            ),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |_: acp::NewSessionRequest,
                           responder: Responder<acp::NewSessionResponse>,
                           connection: V2ConnectionTo<Client>| {
                        responder.respond(acp::NewSessionResponse::new("ordered"))?;
                        connection.send_notification(acp::UpdateSessionNotification::new(
                            "ordered",
                            acp::SessionUpdate::SessionInfoUpdate(
                                acp::SessionInfoUpdate::new().title("After response".to_owned()),
                            ),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(agent_transport, async move |_| {
                    finish.await.map_err(Error::into_internal_error)
                });
            futures::future::try_join(client, agent).await
        });
        result.expect("native MCP response ordering");
    }

    #[test]
    fn native_mcp_sdk_stateless_contract_and_cancellation() {
        let result = futures::executor::block_on(async {
            let (published, mut declarations) = mpsc::unbounded();
            let (started, mut reads) = mpsc::unbounded();
            let (finished, finish) = oneshot::channel();
            let cleaned = Arc::new(AtomicBool::new(false));
            let read = {
                let cleaned = cleaned.clone();
                move |arguments: ReadTextFile, context: McpRequestContext<Agent>| {
                    let started = started.clone();
                    let cleaned = cleaned.clone();
                    async move {
                        assert_eq!(context.metadata()["test/request-local"], true);
                        if arguments.path == PathBuf::from("blocked") {
                            started
                                .unbounded_send(context.request_id().to_string())
                                .map_err(Error::into_internal_error)?;
                            context.operation_cancellation().cancelled().await;
                            cleaned.store(true, Ordering::SeqCst);
                            return Err(Error::request_cancelled());
                        }
                        if arguments.path == PathBuf::from("missing") {
                            return Err(Error::resource_not_found(Some("missing".into())));
                        }
                        Ok(format!(
                            "{}:{:?}:{:?}",
                            arguments.path.display(),
                            arguments.line,
                            arguments.limit
                        ))
                    }
                    .boxed()
                }
            };
            let (client_transport, agent_transport) = Channel::duplex();
            let client = Client
                .v2()
                .connect_with(client_transport, async move |connection| {
                    connection
                        .send_request(acp::InitializeRequest::new(
                            ProtocolVersion::V2,
                            acp::Implementation::new("test-client", "1"),
                        ))
                        .block_task()
                        .await?;
                    let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                    let opened = ordered_setup(&connection, || {
                        Ok(connection
                            .build_session(&cwd)
                            .with_mcp_server(server(read.clone()))?
                            .start_session())
                    })
                    .await?;
                    ordered_setup(&connection, || {
                        Ok(connection
                            .resume_session(opened.session().session_id().clone(), &cwd)
                            .with_mcp_server(server(read))?
                            .start_session())
                    })
                    .await?;
                    finish.await.map_err(Error::into_internal_error)
                });
            let agent = Agent.v2().on_receive_request(
                async |_: acp::InitializeRequest,
                       responder: Responder<acp::InitializeResponse>,
                       _| {
                    responder.respond(
                        acp::InitializeResponse::new(
                            ProtocolVersion::V2,
                            acp::Implementation::new("test-agent", "1"),
                        )
                        .capabilities(
                            acp::AgentCapabilities::new().session(
                                acp::SessionCapabilities::new().mcp(
                                    acp::McpCapabilities::new().acp(acp::McpAcpCapabilities::new()),
                                ),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            );
            let agent = agent.on_receive_request(
                {
                    let published = published.clone();
                    async move |setup: acp::NewSessionRequest,
                                responder: Responder<acp::NewSessionResponse>,
                                connection: V2ConnectionTo<Client>| {
                        let [acp::McpServer::Acp(server)] = setup.mcp_servers.as_slice() else {
                            return Err(Error::internal_error().data("missing native declaration"));
                        };
                        let server_id = server.server_id.clone();
                        let peer = connection.clone();
                        let published = published.clone();
                        connection.spawn(async move {
                            let discovery = request(
                                &peer,
                                &server_id,
                                "early-discover",
                                "server/discover",
                                params(json!({})),
                            )
                            .await?;
                            assert_eq!(
                                discovery["result"]["supportedVersions"],
                                json!(["2026-07-28"]),
                            );
                            published
                                .unbounded_send(server_id)
                                .map_err(Error::into_internal_error)?;
                            responder.respond(acp::NewSessionResponse::new("native"))
                        })
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );
            let agent = agent.on_receive_request(
                async move |setup: acp::ResumeSessionRequest,
                            responder: Responder<acp::ResumeSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let [acp::McpServer::Acp(server)] = setup.mcp_servers.as_slice() else {
                        return Err(
                            Error::internal_error().data("missing resumed native declaration")
                        );
                    };
                    let server_id = server.server_id.clone();
                    let peer = connection.clone();
                    let published = published.clone();
                    connection.spawn(async move {
                        let list = request(
                            &peer,
                            &server_id,
                            "early-list",
                            "tools/list",
                            params(json!({})),
                        )
                        .await?;
                        assert_eq!(list["result"]["tools"][0]["name"], "read_text_file");
                        published
                            .unbounded_send(server_id)
                            .map_err(Error::into_internal_error)?;
                        responder.respond(acp::ResumeSessionResponse::new())
                    })
                },
                agent_client_protocol::on_receive_request!(),
            );
            let agent = agent.connect_with(agent_transport, async move |connection| {
                let server = declarations.next().await.expect("new declaration");
                let missing = request(
                    &connection, &server, "metadata", "tools/list", serde_json::Map::new(),
                ).await?;
                assert_eq!(missing["error"]["code"], -32602);
                let mut missing_version = params(json!({}));
                missing_version["_meta"].as_object_mut().expect("metadata")
                    .remove("io.modelcontextprotocol/protocolVersion");
                let missing_version = request(
                    &connection, &server, "metadata", "tools/list", missing_version,
                ).await?;
                assert_eq!(missing_version["error"]["code"], -32602);
                let mut unsupported = params(json!({}));
                unsupported["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2025-11-25");
                let unsupported = request(
                    &connection, &server, "metadata", "tools/list", unsupported,
                ).await?;
                assert_eq!(unsupported["error"]["code"], -32022);
                assert_eq!(unsupported["error"]["data"], json!({
                    "requested": "2025-11-25", "supported": ["2026-07-28"]
                }));
                let initialize = request(
                    &connection, &server, "init", "initialize", params(json!({})),
                ).await?;
                assert_eq!(initialize["error"]["code"], -32601);
                let list = request(
                    &connection, &server, "metadata", "tools/list", params(json!({})),
                ).await?;
                assert_eq!(list["result"]["tools"][0]["name"], "read_text_file");
                let missing_file = request(
                    &connection, &server, "read-missing", "tools/call",
                    params(json!({"name": "read_text_file", "arguments": {"path": "missing"}})),
                ).await?;
                assert_eq!(
                    missing_file["error"]["code"], i32::from(Error::resource_not_found(None).code),
                );
                for arguments in [
                    json!({"name": "unknown", "arguments": {"path": "other"}}),
                    json!({"name": "read_text_file", "arguments": {}}),
                    json!({"name": "read_text_file", "arguments": {"path": "other", "line": 0}}),
                    json!({"name": "read_text_file", "arguments": {"path": "other", "extra": true}}),
                ] {
                    let invalid = request(
                        &connection, &server, "invalid", "tools/call", params(arguments),
                    ).await?;
                    assert_eq!(invalid["error"]["code"], -32602);
                }
                let blocked = connection.send_request(
                    acp::MessageMcpRequest::new(server.clone(), "blocked-read", "tools/call")
                        .params(params(json!({
                            "name": "read_text_file", "arguments": {"path": "blocked"}
                        }))),
                );
                assert_eq!(reads.next().await.as_deref(), Some("blocked-read"));
                let independent = connection.send_request(
                    acp::MessageMcpRequest::new(server.clone(), "other-read", "tools/call")
                        .params(params(json!({
                            "name": "read_text_file", "arguments": {"path": "other", "line": 2, "limit": 1}
                        }))),
                );
                assert_ne!(blocked.id(), independent.id());
                blocked.cancel()?;
                let (blocked, independent) = futures::future::join(
                    blocked.block_task(), independent.block_task(),
                ).await;
                assert!(blocked.is_err());
                assert!(cleaned.load(Ordering::SeqCst), "backend cancellation must join cleanup");
                let independent = serde_json::to_value(independent?)?;
                assert_eq!(independent["result"]["content"][0]["text"], "other:Some(2):Some(1)");
                let reused = request(
                    &connection, &server, "blocked-read", "tools/list", params(json!({})),
                ).await?;
                assert_eq!(reused["result"]["tools"][0]["name"], "read_text_file");
                let missing_again = request(
                    &connection, &server, "metadata", "tools/list", serde_json::Map::new(),
                ).await?;
                assert_eq!(missing_again["error"]["code"], -32602);
                let resumed = declarations.next().await.expect("resume declaration");
                assert_ne!(resumed, server);
                let read = request(
                    &connection, &resumed, "resumed-read", "tools/call",
                    params(json!({"name": "read_text_file", "arguments": {"path": "resumed"}})),
                ).await?;
                assert_eq!(read["result"]["content"][0]["text"], "resumed:None:None");
                finished.send(()).expect("client awaits completion");
                Ok(())
            });
            futures::future::try_join(client, agent).await
        });
        result.expect("native MCP contract");
    }
}
