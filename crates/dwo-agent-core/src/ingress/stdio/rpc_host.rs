//! Stdio host for the profile JSON-RPC router.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agent_client_protocol::schema::{
    CancelNotification, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Responder, on_receive_notification,
    on_receive_request,
};
use anyhow::Result;
use futures::io::AsyncRead;
use serde_json::Value;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::{acp_handlers, dwo_handlers};
use crate::agent::service::AgentService;
use crate::protocol::dwo::{
    DwoAutomationRecordDeliveryRequest, DwoAutomationRunJobRequest, DwoIngressHandleEventRequest,
    DwoIngressNotifyEventNotification, DwoSessionContextRequest, DwoSessionLoadRequest,
    DwoSessionSetConfigOptionNotification, DwoWorkerPingRequest, DwoWorkerProfileRequest,
    DwoWorkerShutdownRequest,
};

/// Run the profile RPC host over stdio.
pub async fn run_rpc_stdio(agent: Arc<AgentService>) -> Result<()> {
    let stdin = EofAsError::new(tokio::io::stdin().compat());
    let stdout = tokio::io::stdout().compat_write();
    let transport = ByteStreams::new(stdout, stdin);

    match run_rpc_transport(agent, transport).await {
        Ok(()) => Ok(()),
        Err(err) if is_stdio_eof_error(&err) => {
            tracing::info!("profile RPC stdio input closed, shutting down");
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("profile RPC connection error: {err}")),
    }
}

/// Run the profile JSON-RPC host over any line-delimited transport.
pub async fn run_rpc_transport<T>(
    agent: Arc<AgentService>,
    transport: T,
) -> std::result::Result<(), agent_client_protocol::Error>
where
    T: agent_client_protocol::ConnectTo<Agent> + 'static,
{
    let agent_for_init = agent.clone();
    let agent_for_new = agent.clone();
    let agent_for_prompt = agent.clone();
    let agent_for_cancel = agent.clone();
    let agent_for_list = agent.clone();
    let agent_for_load = agent.clone();
    let agent_for_mode = agent.clone();
    let agent_for_config = agent.clone();
    let agent_for_worker_profile = agent.clone();
    let agent_for_session_context = agent.clone();
    let agent_for_session_load = agent.clone();
    let agent_for_ingress = agent.clone();
    let agent_for_ingress_notify = agent.clone();
    let agent_for_session_config_notify = agent.clone();
    let agent_for_automation = agent.clone();
    let agent_for_automation_delivery = agent.clone();

    Agent
        .builder()
        .name(&agent.meta().name)
        .on_receive_request(
            async move |req: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::initialize(agent_for_init.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::new_session(agent_for_new.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::prompt(agent_for_prompt.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: CancelNotification, cx: ConnectionTo<Client>| {
                acp_handlers::cancel(agent_for_cancel.clone(), notif, cx).await
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::list_sessions(agent_for_list.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::load_session(agent_for_load.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionModeRequest,
                        responder: Responder<SetSessionModeResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::set_session_mode(agent_for_mode.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                acp_handlers::set_session_config_option(
                    agent_for_config.clone(),
                    req,
                    responder,
                    cx,
                )
                .await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoWorkerPingRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::worker_ping(req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoWorkerProfileRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::worker_profile(agent_for_worker_profile.clone(), req, responder, cx)
                    .await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoSessionContextRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::session_context(agent_for_session_context.clone(), req, responder, cx)
                    .await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoSessionLoadRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::session_load(agent_for_session_load.clone(), req, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoIngressHandleEventRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::ingress_handle_event(agent_for_ingress.clone(), req, responder, cx)
                    .await
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: DwoIngressNotifyEventNotification, cx: ConnectionTo<Client>| {
                dwo_handlers::ingress_notify_event(agent_for_ingress_notify.clone(), notif, cx)
                    .await
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notif: DwoSessionSetConfigOptionNotification, cx: ConnectionTo<Client>| {
                dwo_handlers::session_set_config_option(
                    agent_for_session_config_notify.clone(),
                    notif,
                    cx,
                )
                .await
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: DwoAutomationRunJobRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::automation_run_job(agent_for_automation.clone(), req, responder, cx)
                    .await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoAutomationRecordDeliveryRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::automation_record_delivery(
                    agent_for_automation_delivery.clone(),
                    req,
                    responder,
                    cx,
                )
                .await
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: DwoWorkerShutdownRequest,
                        responder: Responder<Value>,
                        cx: ConnectionTo<Client>| {
                dwo_handlers::worker_shutdown(req, responder, cx).await
            },
            on_receive_request!(),
        )
        .connect_to(transport)
        .await
}

struct EofAsError<R> {
    inner: R,
}

impl<R> EofAsError<R> {
    fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R> AsyncRead for EofAsError<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(0)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "profile RPC stdio input closed",
            ))),
            other => other,
        }
    }
}

fn is_stdio_eof_error(err: &agent_client_protocol::Error) -> bool {
    format!("{err:?}").contains("profile RPC stdio input closed")
}
